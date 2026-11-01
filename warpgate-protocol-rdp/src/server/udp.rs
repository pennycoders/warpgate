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
//!   (MS-RDPEUDP 2.2.2.9), which names the pending request it answers. One whose hash
//!   names no request is dropped: its sender does not hold a cookie Warpgate issued, so
//!   its Tunnel Create Request (MS-RDPEMT 3.2.5.1) would fail anyway, and routing it by
//!   address instead would let it take another viewer's request;
//! * a SYN whose `SYNDATAEX` offers version 1 or 2 carries no `cookieHash` (2.2.2.9 forbids
//!   one). It goes to a pending request only when exactly one is waiting for its source
//!   address, and only on a guess: the request stays open, and a version 3 SYN whose
//!   `cookieHash` names it takes it over until the connection's UDP accept is over
//!   ([`SidebandDatagrams::replacement`]): while the guessed flow's handshake runs, and
//!   after it failed, until the accept's deadline. A SYN with a forged source address
//!   therefore cannot keep a viewer that sends its cookie off UDP, whether its sender
//!   leaves the handshake hanging or breaks it at once;
//! * a SYN without `SYNDATAEX` offers only version 1 (3.1.5.1.1 step 4), and a version 3
//!   offer without `cookieHash` breaks 2.2.2.9. `ironrdp-rdpeudp`'s accept refuses both (it
//!   needs `SYNDATAEX`, and a version 3 SYN's hash), so they are dropped here rather than
//!   handed to a request whose handshake would fail at once and leave it on TCP.
//!
//! A SYN is only ever routed to a request whose viewer's TCP connection came from the SYN's
//! source IP. Behind SNAT, or a load balancer that rewrites the UDP source but not the
//! PROXY header that `proxy_protocol` reads, the two differ and the viewer stays on TCP;
//! the first such SYN for each request is logged as a warning.
//!
//! Routing is not authentication: whichever flow a request gets, its RDPEMT Tunnel Create
//! Request still has to present that request's `RequestID` and `SecurityCookie` over TLS
//! (MS-RDPEMT 3.2.5.1) before the session uses it.
//!
//! The port is also bounded against being used as a reflector. A client SYN is zero-padded
//! to at least 1132 bytes (3.1.5.1.1 step 6, 2.2.2.5), so a shorter one is dropped before
//! the server answers it with a SYN+ACK of up to 1232, and a flow still in its handshake
//! passes on at most [`SYN_BUDGET`] SYNs, each of which has the server repeat its SYN+ACK.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use ironrdp_pdu::rdp::multitransport::MultitransportRequestPdu;
use ironrdp_rdpeudp::pdu::{FecHeader, UdpVersion, V1Datagram, V1Flags};
use ironrdp_server::{SidebandDatagrams, SidebandFuture, UdpSideband};
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};
use warpgate_common::{ListenEndpoint, WarpgateError};

/// The least a client SYN can measure: it is zero-padded to the smaller of its two MTUs
/// (MS-RDPEUDP 3.1.5.1.1 step 6), each at least 1132 (2.2.2.5).
const MIN_SYN_LEN: usize = 1132;

/// The largest datagram the protocol allows: the negotiated MTU is at most 1232 plus the
/// largest `RDPUDP_ACK_VECTOR_HEADER` (MS-RDPEUDP 3.1.1.3), which is a 2-byte size, an ACK
/// vector of up to 2048 bytes and padding to a DWORD boundary (2.2.2.7). RDP-UDP2 keeps to
/// 1232 (MS-RDPEUDP2 1.3), but versions 1 and 2 may use the rest. 3.1.1.3 has an endpoint
/// ignore anything larger.
const MAX_DATAGRAM: usize = 1232 + 2052;

/// One byte more than [`MAX_DATAGRAM`], so that a receive that fills it was a datagram
/// too large to accept rather than one that fit exactly.
const RECV_BUFFER: usize = MAX_DATAGRAM + 1;

/// Bytes queued for one flow before further datagrams are dropped. RDP-UDP retransmits what
/// is lost (MS-RDPEUDP 3.1.1.5), so a flow whose transport falls behind loses throughput,
/// not data. This is above what Warpgate's receive window lets a client have in flight:
/// 64 datagrams of up to 1232 bytes (`ironrdp-rdpeudp`'s default `log_window_size` of 6).
const FLOW_QUEUE_BYTES: usize = 128 * 1024;

/// Datagrams queued for one flow, however small: acknowledgements are a few dozen bytes.
const FLOW_QUEUE: usize = 1024;

/// SYNs a flow passes on while its handshake runs: the one that opened it and up to five
/// retransmissions, after which a client gives up (MS-RDPEUDP 3.1.5.2 step 4). The server
/// answers each with its SYN+ACK.
const SYN_BUDGET: u8 = 6;

/// Multitransport requests that may wait for their SYN at once; past this a viewer stays on
/// TCP. Each connection makes one request, so this bounds connections still setting up.
const MAX_PENDING: usize = 256;

/// Flows that may be open at once; past this a SYN is dropped and its viewer stays on TCP.
const MAX_FLOWS: usize = 1024;

/// Waiting requests and open flows from one IP address; past this another connection from
/// it stays on TCP.
const MAX_PER_ADDRESS: usize = 8;

/// How long the listener waits after a failed receive, so a socket that keeps failing does
/// not spin.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(10);

/// How often a UDP listener that no RDP listener holds checks whether it is still used. It
/// closes once two checks in a row find it unheld and unused, so a listener restart, which
/// takes it back within moments, never finds the port closed.
const LINGER: Duration = Duration::from_secs(5);

/// Where a request's datagrams are handed over.
type Handover = oneshot::Receiver<Box<dyn SidebandDatagrams>>;

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
    /// The request it was opened for.
    id: u64,
    queue: FlowQueue,
    /// Whether the peer has sent nothing but SYNs so far.
    handshake: bool,
    /// SYNs passed on so far.
    syns: u8,
}

/// The sending side of one flow's queue.
#[derive(Clone)]
struct FlowQueue {
    tx: mpsc::Sender<Bytes>,
    queued: Arc<AtomicUsize>,
}

struct Pending {
    /// The IP the viewer's TCP connection came from.
    viewer_ip: IpAddr,
    /// SHA-256 of the request's `securityCookie` (MS-RDPEUDP 2.2.2.9).
    cookie_hash: [u8; 32],
    state: PendingState,
    /// Whether a SYN naming this request from another IP has been logged.
    mismatch_warned: bool,
}

enum PendingState {
    /// No SYN yet; the connection waits for its datagrams here.
    Waiting(oneshot::Sender<Box<dyn SidebandDatagrams>>),
    /// Handed the flow of `peer` on a guess (a SYN without `cookieHash`, by source
    /// address); a SYN with the request's `cookieHash` replaces it through `replace` until
    /// the connection's UDP accept is over, even once that flow's handshake failed.
    Guessed {
        peer: SocketAddr,
        replace: oneshot::Sender<Box<dyn SidebandDatagrams>>,
    },
}

/// What [`Routes::route`] decided for one datagram.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// To this established flow.
    Flow,
    /// A SYN that opens a flow for this waiting request.
    Open {
        id: u64,
        by_cookie: bool,
    },
    /// A SYN whose `cookieHash` names a request that was handed a flow on a guess.
    Replace {
        id: u64,
    },
    Drop(DropReason),
}

#[derive(Debug, PartialEq, Eq)]
enum DropReason {
    /// Larger than [`MAX_DATAGRAM`].
    Oversized,
    /// Not a client SYN, from a peer with no flow.
    NotSyn,
    /// A client SYN shorter than [`MIN_SYN_LEN`].
    ShortSyn,
    /// A SYN past its flow's [`SYN_BUDGET`].
    SynBudget,
    /// A SYN without `cookieHash` that `ironrdp-rdpeudp` cannot accept: no `SYNDATAEX`, or
    /// a version 3 offer.
    Unroutable,
    /// A SYN whose `cookieHash` names no request.
    UnknownCookie,
    /// A SYN without `cookieHash` that no waiting request from its source IP matches.
    NoRequest,
    /// A SYN whose `cookieHash` names a request a viewer made from another IP.
    SourceMismatch { id: u64 },
    /// A SYN without `cookieHash` while several requests wait for its source IP.
    Ambiguous,
    /// [`MAX_FLOWS`] reached.
    TooManyFlows,
    /// [`MAX_PER_ADDRESS`] reached for the SYN's source IP.
    TooManyFromAddress,
}

impl Routes {
    fn route(&self, from: SocketAddr, datagram: &[u8]) -> Route {
        if datagram.len() > MAX_DATAGRAM {
            return Route::Drop(DropReason::Oversized);
        }
        if self.flows.contains_key(&from) {
            return Route::Flow;
        }
        if !is_client_syn(datagram) {
            return Route::Drop(DropReason::NotSyn);
        }
        if datagram.len() < MIN_SYN_LEN {
            return Route::Drop(DropReason::ShortSyn);
        }
        let Some(syn) = client_syn(datagram) else {
            return Route::Drop(DropReason::NotSyn);
        };
        if self.flows.len() >= MAX_FLOWS {
            return Route::Drop(DropReason::TooManyFlows);
        }
        let source = from.ip().to_canonical();

        let opened = match syn {
            ClientSyn::Hashed(hash) => {
                let Some((&id, pending)) = self
                    .pending
                    .iter()
                    .find(|(_, pending)| pending.cookie_hash == hash)
                else {
                    return Route::Drop(DropReason::UnknownCookie);
                };
                if pending.viewer_ip != source {
                    return Route::Drop(DropReason::SourceMismatch { id });
                }
                match pending.state {
                    PendingState::Waiting(_) => Route::Open {
                        id,
                        by_cookie: true,
                    },
                    // Takes the place of a flow from the same address, so it needs no room
                    // of its own.
                    PendingState::Guessed { .. } => return Route::Replace { id },
                }
            }
            ClientSyn::Unhashed => {
                let mut candidates = self
                    .pending
                    .iter()
                    .filter(|(_, pending)| {
                        pending.viewer_ip == source
                            && matches!(pending.state, PendingState::Waiting(_))
                    })
                    .map(|(&id, _)| id);
                match (candidates.next(), candidates.next()) {
                    (Some(id), None) => Route::Open {
                        id,
                        by_cookie: false,
                    },
                    (None, _) => return Route::Drop(DropReason::NoRequest),
                    (Some(_), Some(_)) => return Route::Drop(DropReason::Ambiguous),
                }
            }
            ClientSyn::Unroutable => return Route::Drop(DropReason::Unroutable),
        };
        if self.flows_from(source) >= MAX_PER_ADDRESS {
            return Route::Drop(DropReason::TooManyFromAddress);
        }
        opened
    }

    fn flows_from(&self, ip: IpAddr) -> usize {
        self.flows
            .keys()
            .filter(|peer| peer.ip().to_canonical() == ip)
            .count()
    }

    /// Requests waiting for a SYN from `ip`, and flows open from it.
    fn used_by(&self, ip: IpAddr) -> usize {
        let waiting = self
            .pending
            .values()
            .filter(|pending| {
                pending.viewer_ip == ip && matches!(pending.state, PendingState::Waiting(_))
            })
            .count();
        waiting + self.flows_from(ip)
    }

    fn is_empty(&self) -> bool {
        self.flows.is_empty() && self.pending.is_empty()
    }
}

impl Flow {
    /// Whether to pass on `datagram`, which the flow's peer sent: while the handshake runs, a
    /// SYN only at its full length and within [`SYN_BUDGET`]. Once the peer has sent anything
    /// else (its ACK of the SYN+ACK, MS-RDPEUDP 3.1.5.2 step 4) its handshake is over and
    /// every datagram passes: RDP-UDP2 packets (MS-RDPEUDP2 2.2) are not V1 datagrams, so
    /// their bytes say nothing about V1 flags.
    fn admit(&mut self, datagram: &[u8]) -> Result<(), DropReason> {
        if !self.handshake {
            return Ok(());
        }
        if !is_client_syn(datagram) {
            self.handshake = false;
            return Ok(());
        }
        if datagram.len() < MIN_SYN_LEN {
            return Err(DropReason::ShortSyn);
        }
        if self.syns >= SYN_BUDGET {
            return Err(DropReason::SynBudget);
        }
        self.syns += 1;
        Ok(())
    }
}

impl FlowQueue {
    fn new() -> (Self, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(FLOW_QUEUE);
        let queue = Self {
            tx,
            queued: Arc::new(AtomicUsize::new(0)),
        };
        (queue, rx)
    }

    /// Queues `datagram`, or drops it when the flow is [`FLOW_QUEUE_BYTES`] or
    /// [`FLOW_QUEUE`] datagrams behind: the transport recovers it.
    fn push(&self, datagram: &[u8]) {
        let len = datagram.len();
        // Counted before it can be taken, so the receiver never subtracts first.
        if self.queued.fetch_add(len, Ordering::Relaxed) + len > FLOW_QUEUE_BYTES
            || self.tx.try_send(Bytes::copy_from_slice(datagram)).is_err()
        {
            self.queued.fetch_sub(len, Ordering::Relaxed);
        }
    }
}

/// What a client SYN (SYN without ACK, MS-RDPEUDP 3.1.5.1.1) offers to route it by.
enum ClientSyn {
    /// The `cookieHash` of a version 3 SYN (2.2.2.9).
    Hashed([u8; 32]),
    /// A `SYNDATAEX` offering version 1 or 2, which carries no `cookieHash`.
    Unhashed,
    /// Neither: no `SYNDATAEX`, or a version 3 offer without `cookieHash`.
    Unroutable,
}

/// Whether `datagram` starts with a client SYN's header (SYN without ACK), read from the
/// fixed eight bytes alone.
fn is_client_syn(datagram: &[u8]) -> bool {
    ironrdp_core::decode::<FecHeader>(datagram).is_ok_and(|header| {
        header.flags.contains(V1Flags::SYN) && !header.flags.contains(V1Flags::ACK)
    })
}

fn client_syn(datagram: &[u8]) -> Option<ClientSyn> {
    let datagram: V1Datagram = ironrdp_core::decode(datagram).ok()?;
    datagram.syn_data.as_ref()?;
    Some(match datagram.syn_data_ex {
        Some(ex) => match ex.cookie_hash {
            Some(hash) => ClientSyn::Hashed(hash),
            None if ex.udp_ver == UdpVersion::V1 || ex.udp_ver == UdpVersion::V2 => {
                ClientSyn::Unhashed
            }
            None => ClientSyn::Unroutable,
        },
        None => ClientSyn::Unroutable,
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
        // The maps stay consistent whatever panicked while holding them: every change is a
        // single insert, update or remove.
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.routes, &other.routes)
    }

    /// Whether no request waits and no flow is open.
    fn is_idle(&self) -> bool {
        self.routes().is_empty()
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
        match routes.route(from, datagram) {
            Route::Flow => {
                let Some(flow) = routes.flows.get_mut(&from) else {
                    return;
                };
                match flow.admit(datagram) {
                    Ok(()) => flow.queue.push(datagram),
                    Err(reason) => {
                        drop(routes);
                        debug!(peer = %from, ?reason, "Dropped an RDP UDP SYN");
                    }
                }
            }
            Route::Open { id, by_cookie } => {
                self.open(routes, socket, from, id, by_cookie, datagram);
            }
            Route::Replace { id } => self.replace(routes, socket, from, id, datagram),
            Route::Drop(DropReason::SourceMismatch { id }) => {
                let first = routes
                    .pending
                    .get_mut(&id)
                    .is_some_and(|pending| !std::mem::replace(&mut pending.mismatch_warned, true));
                drop(routes);
                if first {
                    warn!(
                        peer = %from,
                        "A viewer's RDP UDP SYN came from another IP than its TCP connection, \
                         so it stays on TCP; SNAT or a proxy in front of the RDP port can \
                         change one address but not the other"
                    );
                } else {
                    debug!(peer = %from, "Dropped an RDP UDP SYN from another IP than its viewer's");
                }
            }
            Route::Drop(reason) => {
                drop(routes);
                match reason {
                    DropReason::NotSyn | DropReason::Oversized => {
                        trace!(peer = %from, len = datagram.len(), ?reason, "Dropped a stray RDP UDP datagram");
                    }
                    reason => debug!(peer = %from, ?reason, "Dropped an RDP UDP SYN"),
                }
            }
        }
    }

    /// Opens a flow from `from` for the waiting request `id`, whose SYN `syn` is.
    fn open(
        &self,
        mut routes: MutexGuard<'_, Routes>,
        socket: &Arc<UdpSocket>,
        from: SocketAddr,
        id: u64,
        by_cookie: bool,
        syn: &[u8],
    ) {
        let Some(Pending {
            viewer_ip,
            cookie_hash,
            state,
            mismatch_warned,
        }) = routes.pending.remove(&id)
        else {
            return;
        };
        // `route` found it waiting, under this same lock.
        let PendingState::Waiting(ready) = state else {
            return;
        };
        let replacement = if by_cookie {
            None
        } else {
            // Kept open for a SYN that carries its cookieHash.
            let (replace, rx) = oneshot::channel();
            routes.pending.insert(
                id,
                Pending {
                    viewer_ip,
                    cookie_hash,
                    state: PendingState::Guessed {
                        peer: from,
                        replace,
                    },
                    mismatch_warned,
                },
            );
            Some(Replacement {
                rx,
                registration: GuessRegistration {
                    listener: self.clone(),
                    id,
                    peer: from,
                },
            })
        };
        let (datagrams, queue) = self.flow(&mut routes, socket, from, id, replacement);
        // Released before `ready` is answered: a refused hand-over drops `datagrams`, whose
        // registrations take the lock to remove the flow and the request again.
        drop(routes);
        if ready.send(datagrams).is_err() {
            debug!(peer = %from, "The connection stopped waiting for its UDP SYN");
            return;
        }
        info!(
            peer = %from,
            matched = if by_cookie { "cookieHash" } else { "source address" },
            "Viewer opened an RDP UDP flow"
        );
        queue.push(syn);
    }

    /// Hands the guessed request `id` the flow of `from`, whose SYN `syn` carries its
    /// `cookieHash`.
    fn replace(
        &self,
        mut routes: MutexGuard<'_, Routes>,
        socket: &Arc<UdpSocket>,
        from: SocketAddr,
        id: u64,
        syn: &[u8],
    ) {
        let Some(pending) = routes.pending.remove(&id) else {
            return;
        };
        // `route` found it guessed, under this same lock.
        let PendingState::Guessed { peer, replace } = pending.state else {
            return;
        };
        let (datagrams, queue) = self.flow(&mut routes, socket, from, id, None);
        drop(routes);
        if replace.send(datagrams).is_err() {
            debug!(peer = %from, "The handshake over the guessed RDP UDP flow was already over");
            return;
        }
        info!(
            peer = %from,
            replaced = %peer,
            "Viewer's cookieHash took its RDP UDP request over from a flow matched by address"
        );
        queue.push(syn);
    }

    fn flow(
        &self,
        routes: &mut Routes,
        socket: &Arc<UdpSocket>,
        peer: SocketAddr,
        id: u64,
        replacement: Option<Replacement>,
    ) -> (Box<dyn SidebandDatagrams>, FlowQueue) {
        let (queue, rx) = FlowQueue::new();
        routes.flows.insert(
            peer,
            Flow {
                id,
                queue: queue.clone(),
                handshake: true,
                syns: 1,
            },
        );
        let datagrams = FlowDatagrams {
            socket: Arc::clone(socket),
            peer,
            rx: tokio::sync::Mutex::new(rx),
            queued: Arc::clone(&queue.queued),
            replacement: Mutex::new(replacement),
            _registration: FlowRegistration {
                listener: self.clone(),
                peer,
                id,
            },
        };
        (Box::new(datagrams), queue)
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
    ) -> Result<(u64, Handover), &'static str> {
        let mut routes = self.routes();
        if routes.pending.len() >= MAX_PENDING {
            return Err("Too many viewers waiting for their RDP UDP SYN, continuing over TCP");
        }
        if routes.used_by(viewer_ip) >= MAX_PER_ADDRESS {
            return Err("Too many RDP UDP flows from this viewer's address, continuing over TCP");
        }
        let id = routes.next_id;
        routes.next_id = routes.next_id.wrapping_add(1);
        let (ready, rx) = oneshot::channel();
        routes.pending.insert(
            id,
            Pending {
                viewer_ip,
                cookie_hash,
                state: PendingState::Waiting(ready),
                mismatch_warned: false,
            },
        );
        Ok((id, rx))
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
        let (id, ready) = match self
            .listener
            .register(self.viewer_ip, cookie_hash(&request.security_cookie))
        {
            Ok(registered) => registered,
            Err(why) => {
                warn!("{why}");
                return Box::pin(async { None });
            }
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

/// Withdraws a request that never got its SYN.
struct PendingRegistration {
    listener: UdpListener,
    id: u64,
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let mut routes = self.listener.routes();
        if routes
            .pending
            .get(&self.id)
            .is_some_and(|pending| matches!(pending.state, PendingState::Waiting(_)))
        {
            routes.pending.remove(&self.id);
        }
    }
}

/// Withdraws a request handed a flow on a guess once the connection's UDP accept is over:
/// the transport is up, or the deadline passed, or the connection ended.
struct GuessRegistration {
    listener: UdpListener,
    id: u64,
    peer: SocketAddr,
}

impl Drop for GuessRegistration {
    fn drop(&mut self) {
        let mut routes = self.listener.routes();
        if routes.pending.get(&self.id).is_some_and(|pending| {
            matches!(pending.state, PendingState::Guessed { peer, .. } if peer == self.peer)
        }) {
            routes.pending.remove(&self.id);
        }
    }
}

/// The receiving end of a guessed flow's [`SidebandDatagrams::replacement`]. Dropped with
/// the flow when the server never asks for it.
struct Replacement {
    rx: Handover,
    registration: GuessRegistration,
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
///
/// Replies leave through `sendto` on that socket, so their source address is whichever the
/// kernel picks for the route back. On a socket bound to a specific address that is the
/// address the viewer sent to. On a wildcard bind of a host with several addresses on the
/// viewer's route it can be another one, and the viewer, which only takes datagrams from
/// the address it sent to, never completes the handshake and stays on TCP. Replying from
/// each datagram's destination address takes `IP_PKTINFO`/`IPV6_RECVPKTINFO` control
/// messages, which tokio's `UdpSocket` does not expose and which Warpgate's `unsafe_code`
/// ban rules out doing by hand, so `rdp.udp` documents the specific listen address such a
/// host needs instead.
struct FlowDatagrams {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    queued: Arc<AtomicUsize>,
    replacement: Mutex<Option<Replacement>>,
    _registration: FlowRegistration,
}

/// Whether a failed send to a viewer is the network dropping the datagram rather than the
/// flow being unusable: a route or neighbour that is gone for a moment, a firewall rule, a
/// full interface queue or socket buffer. RDP-UDP retransmits what goes unacknowledged
/// (MS-RDPEUDP 3.1.1.5, MS-RDPEUDP2 3.1.1.2), so these are loss like any other. Any other
/// error ends the transport, and with it a session whose Graphics Pipeline moved onto UDP:
/// the tunnel lasts as long as the connection (MS-RDPEMT 1.3.3) and the Soft-Sync that
/// moved the channel promised no more of its data over TCP (MS-RDPEDYC 3.1.5.3), so the
/// viewer reconnects rather than keep a session it cannot draw.
fn is_transient_send_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::WouldBlock
    ) || error.raw_os_error() == Some(ENOBUFS)
}

#[cfg(unix)]
const ENOBUFS: i32 = libc::ENOBUFS;
/// `WSAENOBUFS`.
#[cfg(windows)]
const ENOBUFS: i32 = 10055;

impl SidebandDatagrams for FlowDatagrams {
    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> SidebandFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            match self.socket.send_to(buf, self.peer).await {
                Err(error) if is_transient_send_error(&error) => {
                    trace!(peer = %self.peer, %error, "RDP UDP send failed, leaving it to retransmission");
                    Ok(buf.len())
                }
                result => result,
            }
        })
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
            self.queued.fetch_sub(datagram.len(), Ordering::Relaxed);
            let len = datagram.len().min(buf.len());
            if let (Some(dst), Some(src)) = (buf.get_mut(..len), datagram.get(..len)) {
                dst.copy_from_slice(src);
            }
            Ok(len)
        })
    }

    fn replacement(&self) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> {
        let replacement = self
            .replacement
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        Box::pin(async move {
            let Replacement { rx, registration } = replacement?;
            // Held for as long as the server waits here: until the transport is up or the
            // accept's deadline, past a failed handshake over the guessed flow.
            let datagrams = rx.await.ok();
            drop(registration);
            datagrams
        })
    }
}

/// The UDP listeners of the RDP listen addresses, which outlive the RDP listener.
///
/// The listener supervisor restarts the RDP listener for a renewed certificate or a changed
/// endpoint by aborting its accept loop and binding again. Viewers' flows keep the UDP
/// socket open past that, so a UDP listener bound per accept loop would be bound twice: the
/// new bind fails while the old socket, no longer read, starves the flows. Instead a UDP
/// listener is kept here by address for as long as an accept loop holds it ([`UdpHold`]) or
/// a request or flow uses it, and the restarted accept loop takes the same one back.
pub struct UdpServices {
    listeners: Mutex<HashMap<SocketAddr, Service>>,
    linger: Duration,
}

struct Service {
    listener: UdpListener,
    holders: usize,
    /// Consecutive checks that found it unheld and unused.
    idle_checks: u8,
}

static SERVICES: LazyLock<Arc<UdpServices>> = LazyLock::new(|| UdpServices::with_linger(LINGER));

impl UdpServices {
    /// The process's UDP listeners.
    pub fn global() -> Arc<Self> {
        Arc::clone(&SERVICES)
    }

    fn with_linger(linger: Duration) -> Arc<Self> {
        Arc::new(Self {
            listeners: Mutex::new(HashMap::new()),
            linger,
        })
    }

    fn listeners(&self) -> MutexGuard<'_, HashMap<SocketAddr, Service>> {
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The UDP listener on `endpoint`'s addresses and port, bound unless it is open
    /// already.
    pub async fn acquire(
        self: &Arc<Self>,
        endpoint: &ListenEndpoint,
    ) -> Result<UdpHold, WarpgateError> {
        let key = endpoint.address();
        if let Some(hold) = self.hold_existing(key) {
            return Ok(hold);
        }
        let sockets = endpoint.udp_sockets().await?;
        let listener = UdpListener::new();
        self.listeners().insert(
            key,
            Service {
                listener: listener.clone(),
                holders: 1,
                idle_checks: 0,
            },
        );
        tokio::spawn({
            let services = Arc::clone(self);
            let listener = listener.clone();
            async move {
                tokio::select! {
                    () = listener.clone().serve(sockets) => {}
                    () = services.linger_until_unused(key, &listener) => {}
                }
                debug!(address = %key, "Closed the RDP UDP listener");
            }
        });
        Ok(UdpHold {
            services: Arc::clone(self),
            key,
            listener,
        })
    }

    fn hold_existing(self: &Arc<Self>, key: SocketAddr) -> Option<UdpHold> {
        let mut listeners = self.listeners();
        let service = listeners.get_mut(&key)?;
        service.holders += 1;
        service.idle_checks = 0;
        Some(UdpHold {
            services: Arc::clone(self),
            key,
            listener: service.listener.clone(),
        })
    }

    async fn linger_until_unused(&self, key: SocketAddr, listener: &UdpListener) {
        let mut checks = tokio::time::interval(self.linger);
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        checks.tick().await;
        loop {
            checks.tick().await;
            if self.release_if_unused(key, listener) {
                return;
            }
        }
    }

    /// Whether `listener` is no longer kept for `key`, taking it out once two checks in a
    /// row have found it unheld and unused.
    fn release_if_unused(&self, key: SocketAddr, listener: &UdpListener) -> bool {
        let mut listeners = self.listeners();
        let Some(service) = listeners
            .get_mut(&key)
            .filter(|service| service.listener.same_as(listener))
        else {
            return true;
        };
        if service.holders > 0 || !listener.is_idle() {
            service.idle_checks = 0;
            return false;
        }
        service.idle_checks += 1;
        if service.idle_checks < 2 {
            return false;
        }
        listeners.remove(&key);
        true
    }
}

/// An RDP listener's hold on its UDP listener, released when the RDP listener stops.
pub struct UdpHold {
    services: Arc<UdpServices>,
    key: SocketAddr,
    listener: UdpListener,
}

impl UdpHold {
    /// The listener held.
    pub fn listener(&self) -> UdpListener {
        self.listener.clone()
    }
}

impl Drop for UdpHold {
    fn drop(&mut self) {
        let mut listeners = self.services.listeners();
        if let Some(service) = listeners
            .get_mut(&self.key)
            .filter(|service| service.listener.same_as(&self.listener))
        {
            service.holders = service.holders.saturating_sub(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[path = "udp_tests.rs"]
mod tests;
