//! Server-side UDP multitransport bootstrapping.
//!
//! [MS-RDPBCGR] 2.2.15.1/2.2.15.2 (`ironrdp_acceptor::Acceptor`) sends the
//! Initiate Multitransport Request and reports it via
//! [`ironrdp_acceptor::accept_finalize_with_multitransport`]. This module
//! establishes the sideband RDPEUDP2 + TLS + RDPEMT transport that request
//! bootstraps, for [`crate::server`] to migrate DVC channels onto via
//! [`ironrdp_dvc::DrdynvcServer::request_reliable_udp`].
//!
//! [MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/b8e7c588-51cb-455b-bb73-92d480903133

use core::future::Future;
use core::net::{SocketAddr, SocketAddrV6};
use core::pin::Pin;
use core::time::Duration;
use std::io;
use std::sync::Arc;

use ironrdp_pdu::rdp::multitransport::MultitransportRequestPdu;
use ironrdp_rdpemt::TunnelConfig;
use ironrdp_rdpeudp::ConnectionConfig;
use ironrdp_rdpeudp_tokio::{
    DatagramIo, TunnelMessage, UdpAcceptConfig, UdpTransport, UdpTransportSender, accept_udp,
    accept_udp_with_datagram_io,
};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_rustls::rustls;
use tracing::{debug, warn};

/// How long the full UDP accept sequence (initial datagram, RDPEUDP2
/// handshake, TLS, RDPEMT tunnel creation) may take before giving up and
/// continuing TCP-only.
const UDP_ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);

/// A boxed future, as the object-safe sideband traits below return them.
pub type SidebandFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One client's datagrams on a UDP port the embedder listens on for every
/// connection and demultiplexes itself; see [`UdpSideband`].
pub trait SidebandDatagrams: Send + Sync + 'static {
    /// The client's address: where [`Self::send`] sends to and what
    /// [`Self::recv`] receives from.
    fn peer_addr(&self) -> SocketAddr;

    /// The local address the client sent to, if known.
    fn local_addr(&self) -> Option<SocketAddr>;

    /// Sends one datagram to the client.
    fn send<'a>(&'a self, buf: &'a [u8]) -> SidebandFuture<'a, io::Result<usize>>;

    /// Receives the client's next datagram into `buf` and returns its length,
    /// truncating one longer than `buf`.
    ///
    /// Must be cancellation safe: the transport awaits it inside
    /// `tokio::select!`, and a datagram has to be either returned or kept for
    /// the next call.
    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> SidebandFuture<'a, io::Result<usize>>;
}

/// The sideband UDP transport of a server that listens on one UDP port for
/// every connection, as MS-RDPEUDP 2.1 and 3.1.3 describe.
///
/// [`RdpServerBuilder::with_udp_transport`](crate::RdpServerBuilder::with_udp_transport)
/// binds a socket per connection instead, so only one connection at a time
/// can take the port. With a `UdpSideband`
/// ([`RdpServerBuilder::with_udp_sideband`](crate::RdpServerBuilder::with_udp_sideband))
/// the embedder owns the listening socket and hands each connection the
/// datagrams of the client that answers its Initiate Multitransport Request
/// (MS-RDPBCGR 2.2.15.1).
pub trait UdpSideband: Send + Sync {
    /// Called once the server has sent `request`, with its `requestId` and
    /// `securityCookie`.
    ///
    /// Whatever the embedder needs to recognize the client's SYN
    /// (MS-RDPEUDP 3.1.5.1.1) has to be in place by the time this returns, not
    /// when the future is first polled: the client may already be sending. A
    /// version 3 SYN carries the SHA-256 of `securityCookie` as its
    /// `cookieHash` (MS-RDPEUDP 2.2.2.9).
    ///
    /// The future resolves to that client's datagrams, the SYN first, or to
    /// `None` when there are none to give (the server then continues
    /// TCP-only). The server bounds the wait together with the handshake that
    /// follows and drops the future, or the datagrams, when it gives up or the
    /// connection ends; the embedder can release what it set up for the
    /// request then.
    fn open(&self, request: &MultitransportRequestPdu) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>>;
}

/// [`SidebandDatagrams`] as the transport's [`DatagramIo`].
struct BoxedDatagrams(Box<dyn SidebandDatagrams>);

impl DatagramIo for BoxedDatagrams {
    fn local_addr(&self) -> Option<SocketAddr> {
        self.0.local_addr()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        Some(self.0.peer_addr())
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> impl Future<Output = io::Result<usize>> + Send + 'a {
        self.0.send(buf)
    }

    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> impl Future<Output = io::Result<usize>> + Send + 'a {
        self.0.recv(buf)
    }
}

/// Shared handle to an established sideband UDP transport.
///
/// `send()` uses `UdpTransportSender`, cloned once at construction from the
/// underlying `UdpTransport`. That handle is entirely independent of the
/// `Mutex` below, so it never contends with (or blocks behind) `recv()`,
/// which holds that lock for the full duration of its idle wait for the
/// next datagram: Sending and receiving already run over separate channels
/// fed by separate background tasks, so the two were never meant to share
/// one lock. `recv()` is intended for a single dedicated caller (the
/// `client_loop` select arm).
#[derive(Clone)]
pub(crate) struct UdpTransportHandle {
    sender: UdpTransportSender,
    transport: Arc<Mutex<UdpTransport>>,
}

impl UdpTransportHandle {
    fn new(transport: UdpTransport) -> Self {
        let sender = transport.sender();
        Self {
            sender,
            transport: Arc::new(Mutex::new(transport)),
        }
    }

    /// Sends one higher-layer (raw DVC) payload over the tunnel.
    ///
    /// A failure is logged here rather than propagated: The RDP session never
    /// fails over an optional sideband transport.
    pub(crate) async fn send(&self, data: Vec<u8>) {
        self.send_message(TunnelMessage::from(data)).await;
    }

    /// Sends one Tunnel Data PDU's content, sub-headers included; failures
    /// are handled as in [`Self::send`].
    pub(crate) async fn send_message(&self, message: TunnelMessage) {
        if let Err(error) = self.sender.send_message(message).await {
            warn!(%error, "Failed to send data over UDP transport");
        }
    }

    pub(crate) async fn recv(&self) -> Option<TunnelMessage> {
        self.transport.lock().await.recv_message().await
    }
}

/// The address to bind the sideband UDP socket to: `bind` itself, unless its
/// IP is unspecified and the local address the client reached over TCP is
/// known, in which case that address (at `bind`'s port). Replies must leave
/// from the address the client sent to, which a socket bound to the
/// unspecified address does not guarantee on a host with several addresses.
///
/// A link-local IPv6 address keeps its scope ID, without which it cannot be
/// bound.
pub(crate) fn sideband_bind_addr(bind: SocketAddr, connection_local: Option<SocketAddr>) -> SocketAddr {
    let Some(local) = connection_local.filter(|_| bind.ip().is_unspecified()) else {
        return bind;
    };
    match local {
        SocketAddr::V6(v6) if v6.ip().to_ipv4_mapped().is_none() => {
            SocketAddr::V6(SocketAddrV6::new(*v6.ip(), bind.port(), 0, v6.scope_id()))
        }
        _ => SocketAddr::new(local.ip().to_canonical(), bind.port()),
    }
}

/// Attempts to establish the sideband UDP transport for one Initiate
/// Multitransport Request.
///
/// Binds a fresh socket to `udp_bind_addr` for this attempt, avoiding any
/// shared, long-lived UDP socket state to manage. This assumes at most one
/// live attempt at a time; under
/// [`RdpServerOptions::preempt_existing_session`](crate::server::RdpServerOptions::preempt_existing_session)
/// a candidate session's negotiation can overlap the still-live session it
/// is preempting, and `udp_bind_addr` is typically the same host and port for
/// both (see [`RdpServerBuilder::with_udp_transport`](crate::RdpServerBuilder::with_udp_transport)),
/// so the second bind fails with `AddrInUse`. That failure is handled below
/// like any other: The overlapping connection degrades to TCP-only rather
/// than failing.
///
/// Any failure (bind, RDPEUDP2 handshake, TLS, RDPEMT tunnel) is logged and
/// reported as `None` rather than propagated: An optional sideband transport
/// failing to come up is never a reason to fail the RDP connection, matching
/// the reference client implementation's posture.
pub(crate) async fn accept(
    udp_bind_addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
    request: &MultitransportRequestPdu,
) -> Option<UdpTransportHandle> {
    let socket = match UdpSocket::bind(udp_bind_addr).await {
        Ok(socket) => socket,
        Err(error) => {
            warn!(%error, %udp_bind_addr, "Failed to bind UDP socket for multitransport, continuing TCP-only");
            return None;
        }
    };

    let config = accept_config(tls_config, request, UDP_ACCEPT_TIMEOUT);
    established(accept_udp(socket, config).await)
}

/// [`accept`], over the client's datagrams as `sideband` hands them over
/// rather than over a socket of its own.
///
/// `datagrams` is what [`UdpSideband::open`] returned for `request`. The wait
/// for it and the handshake share one [`UDP_ACCEPT_TIMEOUT`]; failures are
/// handled as in [`accept`].
pub(crate) async fn accept_sideband(
    datagrams: SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>>,
    tls_config: Arc<rustls::ServerConfig>,
    request: &MultitransportRequestPdu,
) -> Option<UdpTransportHandle> {
    let deadline = tokio::time::Instant::now() + UDP_ACCEPT_TIMEOUT;
    let datagrams = match tokio::time::timeout_at(deadline, datagrams).await {
        Ok(Some(datagrams)) => datagrams,
        Ok(None) => {
            debug!("No sideband UDP datagrams for this connection, continuing TCP-only");
            return None;
        }
        Err(_) => {
            debug!(timeout = ?UDP_ACCEPT_TIMEOUT, "Client sent no UDP SYN, continuing TCP-only");
            return None;
        }
    };
    let peer = datagrams.peer_addr();
    debug!(%peer, "Client answered the multitransport request over UDP");
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let config = accept_config(tls_config, request, remaining);
    established(accept_udp_with_datagram_io(BoxedDatagrams(datagrams), config).await)
}

fn accept_config(
    tls_config: Arc<rustls::ServerConfig>,
    request: &MultitransportRequestPdu,
    accept_timeout: Duration,
) -> UdpAcceptConfig {
    UdpAcceptConfig {
        tls_config,
        tunnel_config: TunnelConfig {
            request_id: request.request_id,
            security_cookie: request.security_cookie,
        },
        connection_config: ConnectionConfig::default(),
        accept_timeout,
    }
}

fn established(
    result: Result<UdpTransport, ironrdp_rdpeudp_tokio::UdpTransportError>,
) -> Option<UdpTransportHandle> {
    match result {
        Ok(transport) => {
            debug!("Sideband UDP transport established");
            Some(UdpTransportHandle::new(transport))
        }
        Err(error) => {
            warn!(%error, "Failed to establish sideband UDP transport, continuing TCP-only");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    const V6_LOCAL: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0xc518);

    #[test]
    fn unspecified_bind_takes_the_connection_address() {
        let bind = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3389);
        let local = SocketAddr::new(IpAddr::V6(V6_LOCAL), 3389);
        assert_eq!(
            sideband_bind_addr(bind, Some(local)),
            SocketAddr::new(IpAddr::V6(V6_LOCAL), 3389)
        );
    }

    #[test]
    fn ipv4_client_on_a_dual_stack_listener_binds_ipv4() {
        let bind = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3389);
        let mapped = SocketAddr::new(IpAddr::V6(Ipv4Addr::new(192, 0, 2, 7).to_ipv6_mapped()), 3389);
        assert_eq!(
            sideband_bind_addr(bind, Some(mapped)),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)), 3389)
        );
    }

    #[test]
    fn a_link_local_address_keeps_its_scope() {
        let bind = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3389);
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let local = SocketAddr::V6(SocketAddrV6::new(link_local, 50000, 0, 2));
        assert_eq!(
            sideband_bind_addr(bind, Some(local)),
            SocketAddr::V6(SocketAddrV6::new(link_local, 3389, 0, 2))
        );
    }

    #[test]
    fn an_explicit_bind_address_is_kept() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 3390);
        let local = SocketAddr::new(IpAddr::V6(V6_LOCAL), 3389);
        assert_eq!(sideband_bind_addr(bind, Some(local)), bind);
    }

    #[test]
    fn unknown_connection_address_keeps_the_bind() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3389);
        assert_eq!(sideband_bind_addr(bind, None), bind);
    }
}
