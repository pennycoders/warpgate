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

    /// Resolves to another client's datagrams when the embedder learns that
    /// the request these were handed over for belongs to that client instead,
    /// or to `None` (or never) when it has no such client to offer.
    ///
    /// An embedder that can only guess which client answers a request (a SYN
    /// without a `cookieHash`, MS-RDPEUDP 2.2.2.9, matched by its source
    /// address) hands the guess over and keeps the request open; a version 3
    /// SYN whose `cookieHash` names the request then proves its sender the
    /// right client. The server calls this once, before the handshake over
    /// these datagrams starts, and keeps the future for as long as that
    /// handshake runs: when it resolves, the server abandons the handshake,
    /// drops these datagrams and starts over with the replacement, within the
    /// same [`UDP_ACCEPT_TIMEOUT`]. Dropping the future means the handshake is
    /// over, whatever its outcome, and the embedder can close the request.
    ///
    /// The default never resolves.
    fn replacement(&self) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> {
        Box::pin(core::future::pending())
    }
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
    let mut datagrams = datagrams;
    loop {
        let peer = datagrams.peer_addr();
        debug!(%peer, "Client answered the multitransport request over UDP");
        let mut replacement = datagrams.replacement();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let config = accept_config(Arc::clone(&tls_config), request, remaining);
        tokio::select! {
            result = accept_udp_with_datagram_io(BoxedDatagrams(datagrams), config) => {
                return established(result);
            }
            Some(next) = &mut replacement => {
                debug!(
                    abandoned = %peer,
                    peer = %next.peer_addr(),
                    "Another client proved it answers the multitransport request, restarting the UDP handshake"
                );
                datagrams = next;
            }
        }
    }
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
        connection_config: ConnectionConfig {
            initial_sequence_number: initial_sequence_number(),
            ..ConnectionConfig::default()
        },
        accept_timeout,
    }
}

/// The server's `snInitialSequenceNumber` for its SYN+ACK: MS-RDPEUDP
/// 3.1.5.1.1 step 2 (which 3.1.5.1.3 applies to the SYN+ACK) requires "a
/// 32-bit number generated by using a truly random function".
/// `ConnectionConfig::default()` leaves it 0, which makes every connection's
/// sequence space predictable to an off-path sender.
///
/// Taken from the operating system's generator; the thread-local CSPRNG,
/// itself seeded from it, covers the case where it cannot be read.
fn initial_sequence_number() -> u32 {
    use rand::{RngCore as _, TryRngCore as _};

    rand::rngs::OsRng
        .try_next_u32()
        .unwrap_or_else(|_| rand::rng().next_u32())
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

    /// A client's datagrams as an embedder listening on one shared socket hands them over.
    struct SharedSocketDatagrams {
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
        incoming: Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    }

    impl SidebandDatagrams for SharedSocketDatagrams {
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
                let datagram = self
                    .incoming
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted))?;
                let len = datagram.len().min(buf.len());
                buf[..len].copy_from_slice(&datagram[..len]);
                Ok(len)
            })
        }
    }

    /// The datagrams of whichever client sends to `socket` first, as an embedder that learns
    /// a client from its SYN hands them over.
    fn first_sender_datagrams(socket: Arc<UdpSocket>) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        Box::pin(async move {
            let mut buf = vec![0u8; 9000];
            let (len, peer) = socket.recv_from(&mut buf).await.ok()?;
            tx.send(buf[..len].to_vec()).await.ok()?;
            let pump_socket = Arc::clone(&socket);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 9000];
                while let Ok((len, from)) = pump_socket.recv_from(&mut buf).await {
                    if from == peer && tx.send(buf[..len].to_vec()).await.is_err() {
                        break;
                    }
                }
            });
            Some(Box::new(SharedSocketDatagrams {
                socket,
                peer,
                incoming: Mutex::new(rx),
            }) as Box<dyn SidebandDatagrams>)
        })
    }

    fn request(security_cookie: [u8; 16]) -> MultitransportRequestPdu {
        use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
        use ironrdp_pdu::rdp::multitransport::RequestedProtocol;

        MultitransportRequestPdu {
            security_header: BasicSecurityHeader {
                flags: BasicSecurityHeaderFlags::TRANSPORT_REQ,
            },
            request_id: 3,
            requested_protocol: RequestedProtocol::UdpFecR,
            security_cookie,
        }
    }

    fn tls_config() -> Arc<rustls::ServerConfig> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("certificate");
        let certificate = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::try_from(certified.signing_key.serialize_der()).expect("key");
        Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key)
                .expect("server TLS config"),
        )
    }

    #[tokio::test]
    async fn sideband_without_datagrams_continues_tcp_only() {
        let none: SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> = Box::pin(async { None });
        assert!(
            accept_sideband(none, tls_config(), &request([0x44; 16]))
                .await
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sideband_gives_up_on_a_client_that_never_sends() {
        let never: SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> =
            Box::pin(core::future::pending());
        assert!(
            accept_sideband(never, tls_config(), &request([0x44; 16]))
                .await
                .is_none()
        );
    }

    // oracle: spec MS-RDPEUDP 2.1 / 3.1.3, MS-RDPEMT 3.2.5.1
    #[tokio::test]
    async fn sideband_establishes_the_transport_over_the_embedders_socket() {
        use ironrdp_rdpeudp_tokio::{UdpTransportConfig, connect_udp};

        let security_cookie = [0x44; 16];
        let request = request(security_cookie);
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let server_addr = socket.local_addr().expect("address");

        // What the embedder does: learn the client from its SYN, then pass its datagrams on.
        let datagrams = first_sender_datagrams(socket);

        let client = connect_udp(UdpTransportConfig::new(
            server_addr,
            "localhost".into(),
            TunnelConfig {
                request_id: request.request_id,
                security_cookie,
            },
        ));
        let (server, client) = tokio::join!(accept_sideband(datagrams, tls_config(), &request), client);
        let server = server.expect("sideband transport");
        let mut client = client.expect("client transport");

        client.send(b"over udp".to_vec()).await.expect("client sends");
        let message = server.recv().await.expect("server receives");
        assert_eq!(message.data, b"over udp");
        server.send(b"and back".to_vec()).await;
        assert_eq!(client.recv().await.as_deref(), Some(&b"and back"[..]));
    }
    // oracle: spec MS-RDPEUDP 3.1.5.1.1 step 2, 3.1.5.1.3
    #[test]
    fn every_accept_draws_its_own_initial_sequence_number() {
        // Two draws of a random 32-bit value collide once in 2^32 runs.
        let isn = || {
            accept_config(tls_config(), &request([0x44; 16]), UDP_ACCEPT_TIMEOUT)
                .connection_config
                .initial_sequence_number
        };
        assert_ne!(isn(), isn());
    }

    /// Datagrams handed over on a guess: they carry one SYN from a sender that never
    /// answers the SYN+ACK, until the embedder offers the right client's instead.
    struct GuessedDatagrams {
        syn: std::sync::Mutex<Option<Vec<u8>>>,
        replacement: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<Box<dyn SidebandDatagrams>>>>,
    }

    impl SidebandDatagrams for GuessedDatagrams {
        fn peer_addr(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], 9))
        }

        fn local_addr(&self) -> Option<SocketAddr> {
            None
        }

        fn send<'a>(&'a self, buf: &'a [u8]) -> SidebandFuture<'a, io::Result<usize>> {
            Box::pin(async move { Ok(buf.len()) })
        }

        fn recv<'a>(&'a self, buf: &'a mut [u8]) -> SidebandFuture<'a, io::Result<usize>> {
            let syn = self.syn.lock().expect("syn").take();
            Box::pin(async move {
                let Some(syn) = syn else {
                    return core::future::pending().await;
                };
                buf[..syn.len()].copy_from_slice(&syn);
                Ok(syn.len())
            })
        }

        fn replacement(&self) -> SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>> {
            let replacement = self.replacement.lock().expect("replacement").take();
            Box::pin(async move { replacement?.await.ok() })
        }
    }

    // oracle: spec MS-RDPEUDP 2.2.2.9, 3.1.5.1.1, MS-RDPEMT 3.2.5.1
    #[tokio::test]
    async fn sideband_restarts_the_handshake_with_a_replacement() {
        use ironrdp_core::MonotonicInstant;
        use ironrdp_rdpeudp::RdpeudpConnection;
        use ironrdp_rdpeudp::pdu::UdpVersion;
        use ironrdp_rdpeudp_tokio::{UdpTransportConfig, connect_udp};

        let security_cookie = [0x44; 16];
        let request = request(security_cookie);
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let server_addr = socket.local_addr().expect("address");

        // A version 2 SYN carries no cookieHash, so it was routed on a guess.
        let now = MonotonicInstant::from_millis(0);
        let guessed_syn = RdpeudpConnection::connect(
            ConnectionConfig {
                offer_version: UdpVersion::V2,
                ..ConnectionConfig::default()
            },
            now,
        )
        .expect("guessed client")
        .poll_transmit(now)
        .expect("SYN")
        .contents;
        let (replace, replacement) = tokio::sync::oneshot::channel();
        let guessed: Box<dyn SidebandDatagrams> = Box::new(GuessedDatagrams {
            syn: std::sync::Mutex::new(Some(guessed_syn)),
            replacement: std::sync::Mutex::new(Some(replacement)),
        });

        // The client the request was for, recognized once its own SYN arrives.
        let right_client = first_sender_datagrams(socket);
        tokio::spawn(async move {
            if let Some(datagrams) = right_client.await {
                let _ = replace.send(datagrams);
            }
        });

        let client = connect_udp(UdpTransportConfig::new(
            server_addr,
            "localhost".into(),
            TunnelConfig {
                request_id: request.request_id,
                security_cookie,
            },
        ));
        let (server, client) = tokio::join!(
            accept_sideband(Box::pin(async move { Some(guessed) }), tls_config(), &request),
            client
        );
        let server = server.expect("sideband transport over the replacement");
        let mut client = client.expect("client transport");

        client.send(b"over udp".to_vec()).await.expect("client sends");
        assert_eq!(server.recv().await.expect("server receives").data, b"over udp");
        server.send(b"and back".to_vec()).await;
        assert_eq!(client.recv().await.as_deref(), Some(&b"and back"[..]));
    }
}
