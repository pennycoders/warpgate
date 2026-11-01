//! The UDP side of the RDP port: one listener that every viewer's reliable UDP transport
//! (MS-RDPEMT over MS-RDPEUDP) shares.
//!
//! MS-RDPEUDP 2.1 and 3.1.3 have the server take all of its UDP connections on one socket
//! bound to the RDP port, and a client sends its SYN (3.1.5.1.1) to the address and port
//! it reached over TCP, from a port of its own per connection. So the port is
//! demultiplexed here:
//!
//! * a datagram from a peer that already has a flow goes to that flow, which covers the
//!   client's retransmitted SYNs (3.1.5.2) as well as everything after them;
//! * a version 3 SYN carries the SHA-256 of the `securityCookie` its connection was sent
//!   in the Initiate Multitransport Request (MS-RDPBCGR 2.2.15.1) as its `cookieHash`
//!   (MS-RDPEUDP 2.2.2.9), which names the pending request it answers;
//! * a version 1 or 2 SYN has no `cookieHash`, and neither, for routing, does a version 3
//!   one whose hash matches nothing (3.1.5.1.1 drops such a connection to version 2 rather
//!   than refusing it). It goes to a pending request only when exactly one is waiting for
//!   its source address.
//!
//! A SYN is only ever routed to a request whose viewer's TCP connection came from the SYN's
//! source IP. Routing is not authentication: whichever flow a request gets, its RDPEMT
//! Tunnel Create Request still has to present that request's `RequestID` and
//! `SecurityCookie` over TLS (MS-RDPEMT 3.2.5.1) before the session uses it.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use ironrdp_pdu::rdp::multitransport::MultitransportRequestPdu;
use ironrdp_rdpeudp::pdu::{V1Datagram, V1Flags};
use ironrdp_server::{SidebandDatagrams, SidebandFuture, UdpSideband};
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

/// Datagrams queued for one flow before further ones are dropped. RDP-UDP retransmits what
/// is lost (MS-RDPEUDP 3.1.1.5), so a flow whose transport falls behind loses throughput,
/// not data.
const FLOW_QUEUE: usize = 256;

/// Multitransport requests that may wait for their SYN at once; past this a viewer stays on
/// TCP. Each connection makes one request, so this bounds connections still setting up.
const MAX_PENDING: usize = 256;

/// Flows that may be open at once; past this a SYN is dropped and its viewer stays on TCP.
const MAX_FLOWS: usize = 1024;

/// Large enough for any datagram, as `ironrdp-rdpeudp-tokio` reads them.
const RECV_BUFFER: usize = 9000;

/// How long the listener waits after a failed receive, so a socket that keeps failing does
/// not spin.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(10);

/// The RDP port's UDP listener, shared by every viewer connection.
#[derive(Clone, Default)]
pub struct UdpListener {
    routes: Arc<Mutex<Routes>>,
}

#[derive(Default)]
struct Routes {
    flows: HashMap<SocketAddr, Flow>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
}

struct Flow {
    id: u64,
    tx: mpsc::Sender<Bytes>,
}

struct Pending {
    /// The IP the viewer's TCP connection came from.
    viewer_ip: IpAddr,
    /// SHA-256 of the request's `securityCookie` (MS-RDPEUDP 2.2.2.9).
    cookie_hash: [u8; 32],
    ready: oneshot::Sender<Box<dyn SidebandDatagrams>>,
}

/// What [`Routes::route`] decided for one datagram.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// To this established flow.
    Flow,
    /// A SYN that opens a flow for this pending request.
    Open {
        id: u64,
        by_cookie: bool,
    },
    Drop(DropReason),
}

#[derive(Debug, PartialEq, Eq)]
enum DropReason {
    /// Not a client SYN, from a peer with no flow.
    NotSyn,
    /// A SYN that no pending request from its source IP matches.
    NoRequest,
    /// A SYN whose `cookieHash` names a request a viewer made from another IP.
    SourceMismatch,
    /// A SYN without a usable `cookieHash` while several requests wait for its source IP.
    Ambiguous,
    /// [`MAX_FLOWS`] reached.
    TooManyFlows,
}

impl Routes {
    fn route(&self, from: SocketAddr, datagram: &[u8]) -> Route {
        if self.flows.contains_key(&from) {
            return Route::Flow;
        }
        let Some(ClientSyn { cookie_hash }) = client_syn(datagram) else {
            return Route::Drop(DropReason::NotSyn);
        };
        if self.flows.len() >= MAX_FLOWS {
            return Route::Drop(DropReason::TooManyFlows);
        }
        let source = from.ip().to_canonical();

        if let Some(hash) = cookie_hash
            && let Some((&id, pending)) = self
                .pending
                .iter()
                .find(|(_, pending)| pending.cookie_hash == hash)
        {
            return if pending.viewer_ip == source {
                Route::Open {
                    id,
                    by_cookie: true,
                }
            } else {
                Route::Drop(DropReason::SourceMismatch)
            };
        }

        let mut candidates = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.viewer_ip == source)
            .map(|(&id, _)| id);
        match (candidates.next(), candidates.next()) {
            (Some(id), None) => Route::Open {
                id,
                by_cookie: false,
            },
            (None, _) => Route::Drop(DropReason::NoRequest),
            (Some(_), Some(_)) => Route::Drop(DropReason::Ambiguous),
        }
    }
}

/// A client SYN (SYN without ACK, MS-RDPEUDP 3.1.5.1.1).
struct ClientSyn {
    /// Present in a version 3 SYN (2.2.2.9).
    cookie_hash: Option<[u8; 32]>,
}

fn client_syn(datagram: &[u8]) -> Option<ClientSyn> {
    let datagram: V1Datagram = ironrdp_core::decode(datagram).ok()?;
    let flags = datagram.header.flags;
    (flags.contains(V1Flags::SYN) && !flags.contains(V1Flags::ACK)).then(|| ClientSyn {
        cookie_hash: datagram.syn_data_ex.and_then(|ex| ex.cookie_hash),
    })
}

fn cookie_hash(security_cookie: &[u8; 16]) -> [u8; 32] {
    Sha256::digest(security_cookie).into()
}

impl UdpListener {
    pub fn new() -> Self {
        Self::default()
    }

    fn routes(&self) -> MutexGuard<'_, Routes> {
        // The map stays consistent whatever panicked while holding it: every change is a
        // single insert or remove.
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Receive on `sockets` for as long as the server runs.
    pub async fn serve(self, sockets: Vec<UdpSocket>) {
        let tasks = sockets.into_iter().map(|socket| {
            let listener = self.clone();
            async move { listener.serve_socket(Arc::new(socket)).await }
        });
        futures::future::join_all(tasks).await;
    }

    async fn serve_socket(&self, socket: Arc<UdpSocket>) {
        let mut buf = vec![0u8; RECV_BUFFER];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, from)) => {
                    if let Some(datagram) = buf.get(..len) {
                        self.deliver(&socket, from, datagram);
                    }
                }
                // An unconnected socket can see transient errors, e.g. an ICMP port
                // unreachable reported on some platforms; none of them ends the listener.
                Err(error) => {
                    debug!(%error, "RDP UDP receive failed");
                    tokio::time::sleep(RECV_ERROR_PAUSE).await;
                }
            }
        }
    }

    /// Route one datagram that `socket` received from `from`.
    fn deliver(&self, socket: &Arc<UdpSocket>, from: SocketAddr, datagram: &[u8]) {
        let mut routes = self.routes();
        let route = routes.route(from, datagram);
        let tx = match route {
            Route::Flow => routes.flows.get(&from).map(|flow| flow.tx.clone()),
            Route::Open { id, by_cookie } => {
                let Some(pending) = routes.pending.remove(&id) else {
                    return;
                };
                let (tx, rx) = mpsc::channel(FLOW_QUEUE);
                let datagrams = FlowDatagrams {
                    socket: Arc::clone(socket),
                    peer: from,
                    rx: tokio::sync::Mutex::new(rx),
                    _registration: FlowRegistration {
                        listener: self.clone(),
                        peer: from,
                        id,
                    },
                };
                routes.flows.insert(from, Flow { id, tx: tx.clone() });
                // Released before `ready` is answered: a refused hand-over drops
                // `datagrams`, whose registration takes the lock to remove the flow again.
                drop(routes);
                if pending.ready.send(Box::new(datagrams)).is_err() {
                    debug!(peer = %from, "The connection stopped waiting for its UDP SYN");
                    return;
                }
                info!(
                    peer = %from,
                    matched = if by_cookie { "cookieHash" } else { "source address" },
                    "Viewer opened an RDP UDP flow"
                );
                Some(tx)
            }
            Route::Drop(reason) => {
                drop(routes);
                match reason {
                    DropReason::NotSyn => {
                        trace!(peer = %from, len = datagram.len(), "Dropped a stray RDP UDP datagram");
                    }
                    reason => debug!(peer = %from, ?reason, "Dropped an RDP UDP SYN"),
                }
                None
            }
        };
        if let Some(tx) = tx {
            // Full means the transport is behind; it recovers what is dropped here.
            let _ = tx.try_send(Bytes::copy_from_slice(datagram));
        }
    }

    /// The sideband for one viewer connection whose TCP peer is `viewer`.
    pub fn sideband(&self, viewer: SocketAddr) -> Arc<dyn UdpSideband> {
        Arc::new(ConnectionSideband {
            listener: self.clone(),
            viewer_ip: viewer.ip().to_canonical(),
        })
    }

    fn register(
        &self,
        viewer_ip: IpAddr,
        cookie_hash: [u8; 32],
    ) -> Option<(u64, oneshot::Receiver<Box<dyn SidebandDatagrams>>)> {
        let mut routes = self.routes();
        if routes.pending.len() >= MAX_PENDING {
            return None;
        }
        let id = routes.next_id;
        routes.next_id = routes.next_id.wrapping_add(1);
        let (ready, rx) = oneshot::channel();
        routes.pending.insert(
            id,
            Pending {
                viewer_ip,
                cookie_hash,
                ready,
            },
        );
        Some((id, rx))
    }
}

/// One viewer connection's view of the listener.
struct ConnectionSideband {
    listener: UdpListener,
    viewer_ip: IpAddr,
}

impl UdpSideband for ConnectionSideband {
    fn open(
        &self,
        request: &MultitransportRequestPdu,
    ) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> {
        let Some((id, ready)) = self
            .listener
            .register(self.viewer_ip, cookie_hash(&request.security_cookie))
        else {
            warn!("Too many viewers waiting for their RDP UDP SYN, continuing over TCP");
            return Box::pin(async { None });
        };
        debug!(
            request_id = request.request_id,
            "Waiting for the viewer's RDP UDP SYN"
        );
        // Taken into the future so that dropping it, polled or not, withdraws the request.
        let registration = PendingRegistration {
            listener: self.listener.clone(),
            id,
        };
        Box::pin(async move {
            let _registration = registration;
            ready.await.ok()
        })
    }
}

/// Withdraws a pending request that never got its SYN.
struct PendingRegistration {
    listener: UdpListener,
    id: u64,
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        self.listener.routes().pending.remove(&self.id);
    }
}

/// Closes a flow once its transport is gone, so the peer's datagrams are no longer routed.
struct FlowRegistration {
    listener: UdpListener,
    peer: SocketAddr,
    id: u64,
}

impl Drop for FlowRegistration {
    fn drop(&mut self) {
        let mut routes = self.listener.routes();
        if routes
            .flows
            .get(&self.peer)
            .is_some_and(|flow| flow.id == self.id)
        {
            routes.flows.remove(&self.peer);
            debug!(peer = %self.peer, "Closed an RDP UDP flow");
        }
    }
}

/// One viewer's share of the listener: datagrams from its address, replies from the socket
/// they arrived on.
struct FlowDatagrams {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    _registration: FlowRegistration,
}

impl SidebandDatagrams for FlowDatagrams {
    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> SidebandFuture<'a, io::Result<usize>> {
        Box::pin(self.socket.send_to(buf, self.peer))
    }

    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> SidebandFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            // Both awaits are cancellation safe: a datagram is either taken and copied
            // here or left in the queue.
            let datagram = self
                .rx
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted))?;
            let len = datagram.len().min(buf.len());
            if let (Some(dst), Some(src)) = (buf.get_mut(..len), datagram.get(..len)) {
                dst.copy_from_slice(src);
            }
            Ok(len)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[path = "udp_tests.rs"]
mod tests;
