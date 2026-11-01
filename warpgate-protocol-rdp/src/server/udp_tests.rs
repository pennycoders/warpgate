use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ironrdp_core::MonotonicInstant;
use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
use ironrdp_pdu::rdp::multitransport::{MultitransportRequestPdu, RequestedProtocol};
use ironrdp_rdpeudp::pdu::{FecHeader, SynDataPayload, UdpVersion, V1Datagram, V1Flags};
use ironrdp_rdpeudp::{ConnectionConfig, RdpeudpConnection};
use ironrdp_server::SidebandDatagrams;
use tokio::net::UdpSocket;
use warpgate_common::ListenEndpoint;

use super::{
    DropReason, MAX_DATAGRAM, MAX_PER_ADDRESS, MIN_SYN_LEN, PendingState, Route, SYN_BUDGET,
    UdpListener, UdpServices, cookie_hash, is_icmp_report, is_transient_send_error,
};

const VIEWER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const OTHER_VIEWER: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 20);

fn addr(ip: Ipv4Addr, port: u16) -> SocketAddr {
    SocketAddr::from((ip, port))
}

fn request(request_id: u32, security_cookie: [u8; 16]) -> MultitransportRequestPdu {
    MultitransportRequestPdu {
        security_header: BasicSecurityHeader {
            flags: BasicSecurityHeaderFlags::TRANSPORT_REQ,
        },
        request_id,
        requested_protocol: RequestedProtocol::UdpFecR,
        security_cookie,
    }
}

/// The SYN a client sends (MS-RDPEUDP 3.1.5.1.1): version 3 with the cookie's hash, or an
/// older version without one.
fn syn(version: UdpVersion, security_cookie: [u8; 16]) -> Vec<u8> {
    let config = ConnectionConfig {
        offer_version: version,
        cookie_hash: (version == UdpVersion::V3).then(|| cookie_hash(&security_cookie)),
        ..ConnectionConfig::default()
    };
    let now = MonotonicInstant::from_millis(0);
    let mut client = RdpeudpConnection::connect(config, now).expect("client connection");
    padded(client.poll_transmit(now).expect("SYN").contents)
}

/// A client datagram past its SYN: an ACK (MS-RDPEUDP 3.1.5.1.2) followed by `tag`.
fn ack(tag: &[u8]) -> Vec<u8> {
    let mut datagram = vec![0, 0, 0, 1, 0, 64, 0, 0x04];
    datagram.extend_from_slice(tag);
    datagram
}

/// Zero-padded to the default MTU, as a client sends its SYN (3.1.5.1.1 step 6).
fn padded(mut datagram: Vec<u8>) -> Vec<u8> {
    datagram.resize(1232, 0);
    datagram
}

/// A version 1 SYN without `SYNDATAEX` (3.1.5.1.1 step 4).
fn syn_without_synex() -> Vec<u8> {
    padded(
        ironrdp_core::encode_vec(&V1Datagram {
            header: FecHeader {
                sn_source_ack: u32::MAX,
                receive_window_size: 64,
                flags: V1Flags::SYN,
            },
            ack_vector: None,
            ack_of_acks: None,
            syn_data: Some(SynDataPayload {
                initial_sequence_number: 7,
                upstream_mtu: 1232,
                downstream_mtu: 1232,
            }),
            correlation_id: None,
            syn_data_ex: None,
            data: None,
        })
        .expect("encode"),
    )
}

/// Datagrams queued for `datagrams` right now.
async fn drain(datagrams: &dyn SidebandDatagrams) -> Vec<Vec<u8>> {
    let mut received = Vec::new();
    let mut buf = vec![0u8; 4096];
    while let Ok(Ok(len)) =
        tokio::time::timeout(Duration::from_millis(50), datagrams.recv(&mut buf)).await
    {
        received.push(buf[..len].to_vec());
    }
    received
}

async fn reply_socket() -> Arc<UdpSocket> {
    Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"))
}

/// Resolves what `open` returned, without waiting for a SYN that is not coming.
async fn opened(
    future: ironrdp_server::SidebandFuture<'static, Option<Box<dyn SidebandDatagrams>>>,
) -> Option<Box<dyn SidebandDatagrams>> {
    tokio::time::timeout(Duration::from_millis(200), future)
        .await
        .ok()
        .flatten()
}

async fn next_datagram(datagrams: &dyn SidebandDatagrams) -> Vec<u8> {
    let mut buf = vec![0u8; 2048];
    let len = tokio::time::timeout(Duration::from_secs(1), datagrams.recv(&mut buf))
        .await
        .expect("a datagram")
        .expect("receive");
    buf.truncate(len);
    buf
}

#[tokio::test]
async fn concurrent_requests_get_their_own_syn_by_cookie_hash() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    // Two viewers behind the same address, so only the cookie can tell them apart.
    let first = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let second = listener
        .sideband(addr(VIEWER, 50001))
        .open(&request(2, [0x22; 16]));

    let second_syn = syn(UdpVersion::V3, [0x22; 16]);
    let first_syn = syn(UdpVersion::V3, [0x11; 16]);
    listener.deliver(&socket, addr(VIEWER, 61002), &second_syn);
    listener.deliver(&socket, addr(VIEWER, 61001), &first_syn);

    let first = opened(first).await.expect("first flow");
    let second = opened(second).await.expect("second flow");
    assert_eq!(first.peer_addr(), addr(VIEWER, 61001));
    assert_eq!(second.peer_addr(), addr(VIEWER, 61002));
    assert_eq!(next_datagram(first.as_ref()).await, first_syn);
    assert_eq!(next_datagram(second.as_ref()).await, second_syn);

    // Each flow's later traffic follows its peer.
    let (to_first, to_second) = (ack(b"to the first"), ack(b"to the second"));
    listener.deliver(&socket, addr(VIEWER, 61002), &to_second);
    listener.deliver(&socket, addr(VIEWER, 61001), &to_first);
    assert_eq!(next_datagram(first.as_ref()).await, to_first);
    assert_eq!(next_datagram(second.as_ref()).await, to_second);
}

#[tokio::test]
async fn a_retransmitted_syn_stays_with_its_flow() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let first = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let syn = syn(UdpVersion::V3, [0x11; 16]);
    listener.deliver(&socket, addr(VIEWER, 61001), &syn);
    let first = opened(first).await.expect("flow");

    // A second request from the same viewer address is pending meanwhile; the
    // retransmission (MS-RDPEUDP 3.1.5.2) must not be taken for its SYN.
    let second = listener
        .sideband(addr(VIEWER, 50001))
        .open(&request(2, [0x22; 16]));
    listener.deliver(&socket, addr(VIEWER, 61001), &syn);

    assert_eq!(next_datagram(first.as_ref()).await, syn);
    assert_eq!(next_datagram(first.as_ref()).await, syn);
    assert!(opened(second).await.is_none());
}

#[tokio::test]
async fn a_syn_without_a_usable_cookie_is_not_routed_while_requests_are_ambiguous() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let first = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let second = listener
        .sideband(addr(VIEWER, 50001))
        .open(&request(2, [0x22; 16]));

    let wrong = syn(UdpVersion::V3, [0x33; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61003), &wrong),
        Route::Drop(DropReason::UnknownCookie)
    );
    listener.deliver(&socket, addr(VIEWER, 61003), &wrong);
    let v2 = syn(UdpVersion::V2, [0; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61003), &v2),
        Route::Drop(DropReason::Ambiguous)
    );
    listener.deliver(&socket, addr(VIEWER, 61003), &v2);
    assert!(opened(first).await.is_none());
    assert!(opened(second).await.is_none());
}

#[tokio::test]
async fn a_syn_with_an_unknown_cookie_is_dropped() {
    // Even with only one request waiting for its address: a sender that forged that
    // address must not be able to take the request by naming no cookie it holds.
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let only = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let wrong = syn(UdpVersion::V3, [0x33; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61003), &wrong),
        Route::Drop(DropReason::UnknownCookie)
    );
    listener.deliver(&socket, addr(VIEWER, 61003), &wrong);
    assert!(listener.routes().flows.is_empty());
    assert!(opened(only).await.is_none());
}

#[tokio::test]
async fn a_version_2_syn_needs_exactly_one_request_from_its_address() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let v2 = syn(UdpVersion::V2, [0; 16]);

    // Nothing pending.
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61001), &v2),
        Route::Drop(DropReason::NoRequest)
    );

    // One request from this address, one from another: unambiguous.
    let mine = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let theirs = listener
        .sideband(addr(OTHER_VIEWER, 50000))
        .open(&request(2, [0x22; 16]));
    listener.deliver(&socket, addr(VIEWER, 61001), &v2);
    let mine = opened(mine).await.expect("flow");
    assert_eq!(next_datagram(mine.as_ref()).await, v2);

    // Two requests from one address: no telling which this SYN answers.
    let first = listener
        .sideband(addr(OTHER_VIEWER, 50001))
        .open(&request(3, [0x33; 16]));
    assert_eq!(
        listener.routes().route(addr(OTHER_VIEWER, 61002), &v2),
        Route::Drop(DropReason::Ambiguous)
    );
    listener.deliver(&socket, addr(OTHER_VIEWER, 61002), &v2);
    assert!(opened(theirs).await.is_none());
    assert!(opened(first).await.is_none());
}

#[tokio::test]
async fn a_syn_from_another_address_than_the_viewer_is_dropped() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));

    let stolen = syn(UdpVersion::V3, [0x11; 16]);
    assert_eq!(
        listener.routes().route(addr(OTHER_VIEWER, 61001), &stolen),
        Route::Drop(DropReason::SourceMismatch { id: 0 })
    );
    listener.deliver(&socket, addr(OTHER_VIEWER, 61001), &stolen);
    // Warned about once per request.
    assert!(listener.routes().pending[&0].mismatch_warned);
    listener.deliver(&socket, addr(OTHER_VIEWER, 61002), &stolen);
    let v2 = syn(UdpVersion::V2, [0; 16]);
    assert_eq!(
        listener.routes().route(addr(OTHER_VIEWER, 61001), &v2),
        Route::Drop(DropReason::NoRequest)
    );
    assert!(opened(pending).await.is_none());
}

#[tokio::test]
async fn an_ipv4_mapped_viewer_matches_its_ipv4_syn() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let mapped = SocketAddr::from((VIEWER.to_ipv6_mapped(), 50000));
    let pending = listener.sideband(mapped).open(&request(1, [0x11; 16]));
    listener.deliver(
        &socket,
        addr(VIEWER, 61001),
        &syn(UdpVersion::V3, [0x11; 16]),
    );
    assert!(opened(pending).await.is_some());
}

#[tokio::test]
async fn datagrams_other_than_a_syn_open_nothing() {
    let listener = UdpListener::new();
    let _pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    assert_eq!(
        listener
            .routes()
            .route(addr(VIEWER, 61001), b"\x00\x01garbage"),
        Route::Drop(DropReason::NotSyn)
    );
}

#[tokio::test]
async fn requests_and_flows_are_withdrawn_when_dropped() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;

    // A request whose connection gave up waiting: never polled, then dropped.
    let abandoned = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    assert_eq!(listener.routes().pending.len(), 1);
    drop(abandoned);
    assert!(listener.routes().pending.is_empty());

    // A flow whose transport is gone stops routing its peer.
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(2, [0x22; 16]));
    listener.deliver(
        &socket,
        addr(VIEWER, 61001),
        &syn(UdpVersion::V3, [0x22; 16]),
    );
    let flow = opened(pending).await.expect("flow");
    assert_eq!(listener.routes().flows.len(), 1);
    drop(flow);
    assert!(listener.routes().flows.is_empty());
    assert!(listener.routes().pending.is_empty());
}

#[tokio::test]
async fn a_syn_for_a_connection_that_stopped_waiting_opens_nothing() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let sideband = listener.sideband(addr(VIEWER, 50000));
    let pending = sideband.open(&request(1, [0x11; 16]));
    // Polled once and abandoned, as when the accept times out mid-wait.
    assert!(opened(pending).await.is_none());
    listener.deliver(
        &socket,
        addr(VIEWER, 61001),
        &syn(UdpVersion::V3, [0x11; 16]),
    );
    assert!(listener.routes().flows.is_empty());
}

#[tokio::test]
async fn a_full_flow_queue_drops_rather_than_blocks() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    listener.deliver(
        &socket,
        addr(VIEWER, 61001),
        &syn(UdpVersion::V3, [0x11; 16]),
    );
    let flow = opened(pending).await.expect("flow");
    for _ in 0..super::FLOW_QUEUE * 2 {
        listener.deliver(&socket, addr(VIEWER, 61001), b"data");
    }
    // Bounded by bytes too: large datagrams stop at FLOW_QUEUE_BYTES.
    let large = vec![0x5a; 1232];
    let flow_two = {
        let pending = listener
            .sideband(addr(VIEWER, 50001))
            .open(&request(2, [0x22; 16]));
        listener.deliver(
            &socket,
            addr(VIEWER, 61002),
            &syn(UdpVersion::V3, [0x22; 16]),
        );
        opened(pending).await.expect("second flow")
    };
    for _ in 0..super::FLOW_QUEUE {
        listener.deliver(&socket, addr(VIEWER, 61002), &large);
    }
    let queued: usize = drain(flow_two.as_ref()).await.iter().map(Vec::len).sum();
    assert!(queued <= super::FLOW_QUEUE_BYTES, "{queued} bytes queued");
    assert!(
        queued > super::FLOW_QUEUE_BYTES - 2 * 1232,
        "{queued} bytes queued"
    );
    // Taking datagrams off the queue makes room again.
    listener.deliver(&socket, addr(VIEWER, 61002), &large);
    assert_eq!(drain(flow_two.as_ref()).await, vec![large]);
    drop(flow);
}

/// The listener's datagrams as the transport's `DatagramIo`, as `ironrdp-server` adapts them.
struct ServerIo(Box<dyn SidebandDatagrams>);
impl ironrdp_rdpeudp_tokio::DatagramIo for ServerIo {
    fn local_addr(&self) -> Option<SocketAddr> {
        self.0.local_addr()
    }
    fn peer_addr(&self) -> Option<SocketAddr> {
        Some(self.0.peer_addr())
    }
    fn send<'a>(
        &'a self,
        buf: &'a [u8],
    ) -> impl Future<Output = std::io::Result<usize>> + Send + 'a {
        self.0.send(buf)
    }
    fn recv<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = std::io::Result<usize>> + Send + 'a {
        self.0.recv(buf)
    }
}

/// End to end over loopback: two clients, each with the full RDPEUDP + TLS + RDPEMT stack
/// of `ironrdp-rdpeudp-tokio`, connect to one listening socket at once and each reaches
/// the accept of its own request.
#[tokio::test]
async fn two_transports_share_one_port() {
    use ironrdp_rdpemt::TunnelConfig;
    use ironrdp_rdpeudp_tokio::{
        UdpAcceptConfig, UdpTransportConfig, accept_udp_with_datagram_io, connect_udp,
    };
    use tokio_rustls::rustls;

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
    let certificate = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::try_from(certified.signing_key.serialize_der())
        .expect("key");
    let tls = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .expect("server TLS config"),
    );

    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let server_addr = socket.local_addr().expect("address");
    let listener = UdpListener::new();
    let serving = tokio::spawn(listener.clone().serve(vec![socket]));

    let viewer = SocketAddr::from((Ipv4Addr::LOCALHOST, 50000));
    let session = |request_id: u32, security_cookie: [u8; 16]| {
        let datagrams = listener
            .sideband(viewer)
            .open(&request(request_id, security_cookie));
        let tls = Arc::clone(&tls);
        async move {
            let datagrams = datagrams.await.expect("this session's SYN");
            accept_udp_with_datagram_io(
                ServerIo(datagrams),
                UdpAcceptConfig {
                    tls_config: tls,
                    tunnel_config: TunnelConfig {
                        request_id,
                        security_cookie,
                    },
                    connection_config: ConnectionConfig::default(),
                    accept_timeout: Duration::from_secs(10),
                },
            )
            .await
            .expect("accept")
        }
    };
    let client = |request_id: u32, security_cookie: [u8; 16]| {
        connect_udp(UdpTransportConfig::new(
            server_addr,
            "localhost".into(),
            TunnelConfig {
                request_id,
                security_cookie,
            },
        ))
    };

    let (mut first_server, mut second_server, first_client, second_client) =
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(
                session(1, [0x11; 16]),
                session(2, [0x22; 16]),
                client(1, [0x11; 16]),
                client(2, [0x22; 16]),
            )
        })
        .await
        .expect("both transports up in time");
    let first_client = first_client.expect("first client");
    let mut second_client = second_client.expect("second client");

    first_client.send(b"first".to_vec()).await.expect("send");
    second_client.send(b"second".to_vec()).await.expect("send");
    assert_eq!(first_server.recv().await.as_deref(), Some(&b"first"[..]));
    assert_eq!(second_server.recv().await.as_deref(), Some(&b"second"[..]));
    second_server.send(b"back".to_vec()).await.expect("send");
    assert_eq!(second_client.recv().await.as_deref(), Some(&b"back"[..]));

    serving.abort();
}

// oracle: spec MS-RDPEUDP 2.2.2.9, 3.1.5.1.1
#[tokio::test]
async fn a_cookie_takes_over_a_request_matched_by_address() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));

    // A SYN without cookieHash, perhaps with a forged source address, gets the request on
    // a guess; the request stays open for its cookie.
    let guess = syn(UdpVersion::V2, [0; 16]);
    listener.deliver(&socket, addr(VIEWER, 61001), &guess);
    let guessed = opened(pending).await.expect("guessed flow");
    assert_eq!(guessed.peer_addr(), addr(VIEWER, 61001));
    assert_eq!(next_datagram(guessed.as_ref()).await, guess);
    assert!(matches!(
        listener.routes().pending[&0].state,
        PendingState::Guessed { .. }
    ));

    // What the server does: wait for a replacement while the handshake runs.
    let replacement = guessed.replacement();
    let hashed = syn(UdpVersion::V3, [0x11; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61002), &hashed),
        Route::Replace { id: 0 }
    );
    listener.deliver(&socket, addr(VIEWER, 61002), &hashed);
    let right = tokio::time::timeout(Duration::from_secs(1), replacement)
        .await
        .expect("replaced in time")
        .expect("the hashed sender's flow");
    assert_eq!(right.peer_addr(), addr(VIEWER, 61002));
    assert_eq!(next_datagram(right.as_ref()).await, hashed);
    assert!(listener.routes().pending.is_empty());

    // The server drops the guessed flow; the replacement's traffic keeps its flow.
    drop(guessed);
    assert_eq!(listener.routes().flows.len(), 1);
    listener.deliver(&socket, addr(VIEWER, 61002), b"data");
    assert_eq!(next_datagram(right.as_ref()).await, b"data");
}

#[tokio::test]
async fn a_guessed_request_closes_with_its_handshake() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    listener.deliver(&socket, addr(VIEWER, 61001), &syn(UdpVersion::V2, [0; 16]));
    let guessed = opened(pending).await.expect("guessed flow");

    // Another SYN without cookieHash does not take a request that is no longer waiting.
    let another = syn(UdpVersion::V2, [0; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61003), &another),
        Route::Drop(DropReason::NoRequest)
    );

    // The handshake over the guessed flow finished: the server drops the replacement.
    drop(guessed.replacement());
    assert!(listener.routes().pending.is_empty());
    assert_eq!(
        listener
            .routes()
            .route(addr(VIEWER, 61002), &syn(UdpVersion::V3, [0x11; 16])),
        Route::Drop(DropReason::UnknownCookie)
    );

    // A guessed flow whose transport is gone withdraws its request too.
    let pending = listener
        .sideband(addr(VIEWER, 50001))
        .open(&request(2, [0x22; 16]));
    listener.deliver(&socket, addr(VIEWER, 61004), &syn(UdpVersion::V2, [0; 16]));
    let second = opened(pending).await.expect("second guessed flow");
    assert_eq!(listener.routes().pending.len(), 1);
    drop(second);
    drop(guessed);
    assert!(listener.routes().is_empty());
}

// oracle: spec MS-RDPEUDP 2.2.2.9, 3.1.5.1.1 step 5
#[tokio::test]
async fn a_cookie_takes_over_after_the_guessed_handshake_failed() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    listener.deliver(&socket, addr(VIEWER, 61001), &syn(UdpVersion::V2, [0; 16]));
    let guessed = opened(pending).await.expect("guessed flow");

    // The handshake over the guessed flow failed: the server dropped its datagrams but
    // still waits for a replacement.
    let replacement = guessed.replacement();
    drop(guessed);
    assert!(listener.routes().flows.is_empty());
    let hashed = syn(UdpVersion::V3, [0x11; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61002), &hashed),
        Route::Replace { id: 0 }
    );
    listener.deliver(&socket, addr(VIEWER, 61002), &hashed);
    let right = tokio::time::timeout(Duration::from_secs(1), replacement)
        .await
        .expect("replaced in time")
        .expect("the hashed sender's flow");
    assert_eq!(right.peer_addr(), addr(VIEWER, 61002));
    assert_eq!(next_datagram(right.as_ref()).await, hashed);
    assert!(listener.routes().pending.is_empty());
}

// oracle: spec MS-RDPEUDP 3.1.5.1.1 step 4, 2.2.2.9
#[tokio::test]
async fn a_syn_ironrdp_cannot_accept_is_not_routed() {
    let listener = UdpListener::new();
    let _pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    assert_eq!(
        listener
            .routes()
            .route(addr(VIEWER, 61001), &syn_without_synex()),
        Route::Drop(DropReason::Unroutable)
    );
}

// oracle: spec MS-RDPEUDP 3.1.5.1.1 step 6, 2.2.2.5
#[tokio::test]
async fn a_short_syn_is_dropped() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let full = syn(UdpVersion::V3, [0x11; 16]);
    let mut short = full.clone();
    short.truncate(MIN_SYN_LEN - 1);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61001), &short),
        Route::Drop(DropReason::ShortSyn)
    );
    let mut shortest = full.clone();
    shortest.truncate(MIN_SYN_LEN);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61001), &shortest),
        Route::Open {
            id: 0,
            by_cookie: true
        }
    );

    // Nor does a short one reach a flow still in its handshake.
    listener.deliver(&socket, addr(VIEWER, 61001), &full);
    let flow = opened(pending).await.expect("flow");
    listener.deliver(&socket, addr(VIEWER, 61001), &short);
    assert_eq!(drain(flow.as_ref()).await, vec![full]);
}

// oracle: spec MS-RDPEUDP 3.1.5.2 step 4
#[tokio::test]
async fn repeated_syns_are_passed_on_only_within_the_budget() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let syn = syn(UdpVersion::V3, [0x11; 16]);
    for _ in 0..20 {
        listener.deliver(&socket, addr(VIEWER, 61001), &syn);
    }
    let flow = opened(pending).await.expect("flow");
    assert_eq!(drain(flow.as_ref()).await.len(), usize::from(SYN_BUDGET));

    // Past the handshake, datagrams are no longer read for V1 flags.
    listener.deliver(&socket, addr(VIEWER, 61001), &ack(b""));
    listener.deliver(&socket, addr(VIEWER, 61001), &syn);
    assert_eq!(drain(flow.as_ref()).await.len(), 2);
}

// oracle: spec MS-RDPEUDP 3.1.1.3, 2.2.2.7
#[tokio::test]
async fn an_oversized_datagram_is_dropped() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    listener.deliver(
        &socket,
        addr(VIEWER, 61001),
        &syn(UdpVersion::V3, [0x11; 16]),
    );
    let flow = opened(pending).await.expect("flow");
    let _syn = drain(flow.as_ref()).await;

    listener.deliver(&socket, addr(VIEWER, 61001), &ack(b""));
    listener.deliver(&socket, addr(VIEWER, 61001), &vec![1; MAX_DATAGRAM + 1]);
    listener.deliver(&socket, addr(VIEWER, 61001), &vec![2; MAX_DATAGRAM]);
    let received = drain(flow.as_ref()).await;
    assert_eq!(received.len(), 2);
    assert_eq!(received[1].len(), MAX_DATAGRAM);
}

#[tokio::test]
async fn one_address_gets_a_bounded_share() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let mut waiting = Vec::new();
    for n in 0..MAX_PER_ADDRESS {
        let cookie = [u8::try_from(n).unwrap() + 1; 16];
        let port = 50000 + u16::try_from(n).unwrap();
        waiting.push((
            cookie,
            listener
                .sideband(addr(VIEWER, port))
                .open(&request(u32::try_from(n).unwrap(), cookie)),
        ));
    }
    assert_eq!(listener.routes().pending.len(), MAX_PER_ADDRESS);

    // One more from the same address stays on TCP at once; another address is unaffected.
    let refused = listener
        .sideband(addr(VIEWER, 51000))
        .open(&request(100, [0xee; 16]));
    assert!(opened(refused).await.is_none());
    assert_eq!(listener.routes().pending.len(), MAX_PER_ADDRESS);
    let _other = listener
        .sideband(addr(OTHER_VIEWER, 50000))
        .open(&request(101, [0xef; 16]));
    assert_eq!(listener.routes().pending.len(), MAX_PER_ADDRESS + 1);

    // Requests that open flows keep counting against the address while their handshake
    // runs.
    let (cookie, first) = waiting.remove(0);
    listener.deliver(&socket, addr(VIEWER, 61000), &syn(UdpVersion::V3, cookie));
    let _flow = opened(first).await.expect("flow");
    let refused = listener
        .sideband(addr(VIEWER, 51001))
        .open(&request(102, [0xf0; 16]));
    assert!(opened(refused).await.is_none());

    // Once the peer acknowledges the SYN+ACK (MS-RDPEUDP 3.1.5.2 step 5), the flow no
    // longer does.
    listener.deliver(&socket, addr(VIEWER, 61000), &ack(b"ack"));
    let _admitted = listener
        .sideband(addr(VIEWER, 51002))
        .open(&request(103, [0xf1; 16]));
    assert_eq!(listener.routes().pending.len(), MAX_PER_ADDRESS + 1);
}

// oracle: spec MS-RDPEUDP 3.1.5.2
#[tokio::test]
async fn viewers_behind_one_address_all_get_flows() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let mut flows = Vec::new();
    for n in 0..3 * MAX_PER_ADDRESS {
        let cookie = [u8::try_from(n).unwrap() + 1; 16];
        let port = 50000 + u16::try_from(n).unwrap();
        let peer = addr(VIEWER, 61000 + u16::try_from(n).unwrap());
        let pending = listener
            .sideband(addr(VIEWER, port))
            .open(&request(u32::try_from(n).unwrap(), cookie));
        listener.deliver(&socket, peer, &syn(UdpVersion::V3, cookie));
        let flow = opened(pending)
            .await
            .expect("flow behind the shared address");
        listener.deliver(&socket, peer, &ack(b"ack"));
        flows.push(flow);
    }
    assert_eq!(listener.routes().flows.len(), 3 * MAX_PER_ADDRESS);
    assert_eq!(listener.routes().setting_up(VIEWER.into()), 0);
}

// oracle: spec MS-RDPEUDP 2.2.2.9
#[tokio::test]
async fn a_guessed_request_and_its_flow_count_once() {
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let pending = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    assert_eq!(listener.routes().setting_up(VIEWER.into()), 1);
    listener.deliver(&socket, addr(VIEWER, 61001), &syn(UdpVersion::V2, [0; 16]));
    let guessed = opened(pending).await.expect("guessed flow");
    assert_eq!(listener.routes().setting_up(VIEWER.into()), 1);

    // A failed guessed handshake drops the flow; the request still waits for its cookie.
    let replacement = guessed.replacement();
    drop(guessed);
    assert_eq!(listener.routes().setting_up(VIEWER.into()), 1);
    drop(replacement);
    assert_eq!(listener.routes().setting_up(VIEWER.into()), 0);
}

#[test]
fn transient_send_errors_are_loss() {
    use std::io::{Error, ErrorKind};

    for kind in [
        ErrorKind::HostUnreachable,
        ErrorKind::NetworkUnreachable,
        ErrorKind::PermissionDenied,
        ErrorKind::WouldBlock,
    ] {
        assert!(is_transient_send_error(&Error::from(kind)), "{kind:?}");
    }
    assert!(is_transient_send_error(&Error::from_raw_os_error(
        super::ENOBUFS
    )));
    for kind in [
        ErrorKind::ConnectionAborted,
        ErrorKind::InvalidInput,
        ErrorKind::AddrNotAvailable,
    ] {
        assert!(!is_transient_send_error(&Error::from(kind)), "{kind:?}");
    }
}

#[test]
fn icmp_reports_do_not_pause_the_listener() {
    use std::io::{Error, ErrorKind};

    assert!(is_icmp_report(&Error::from(ErrorKind::ConnectionReset)));
    assert!(is_icmp_report(&Error::from(ErrorKind::ConnectionRefused)));
    for kind in [
        ErrorKind::WouldBlock,
        ErrorKind::Interrupted,
        ErrorKind::OutOfMemory,
        ErrorKind::Other,
    ] {
        assert!(!is_icmp_report(&Error::from(kind)), "{kind:?}");
    }
}

/// A local UDP address nothing listens on.
fn free_endpoint() -> ListenEndpoint {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
    ListenEndpoint::from(probe.local_addr().expect("address"))
}

#[tokio::test]
async fn a_listener_restart_keeps_the_udp_port_and_its_flows() {
    let services = UdpServices::with_linger(Duration::from_millis(20));
    let endpoint = free_endpoint();
    let viewer = UdpSocket::bind("127.0.0.1:0").await.expect("viewer socket");
    let viewer_addr = viewer.local_addr().expect("viewer address");

    let before = services.acquire(&endpoint).await.expect("bind");
    let pending = before
        .listener()
        .sideband(viewer_addr)
        .open(&request(1, [0x11; 16]));
    viewer
        .send_to(&syn(UdpVersion::V3, [0x11; 16]), endpoint.address())
        .await
        .expect("SYN");
    let flow = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .expect("SYN routed in time")
        .expect("flow");
    let _syn = next_datagram(flow.as_ref()).await;

    // The supervisor aborts the accept loop, dropping its hold, and binds again. The flow
    // keeps the listener open meanwhile, however long the restart takes.
    drop(before);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = services
        .acquire(&endpoint)
        .await
        .expect("the same listener");
    assert!(
        after
            .listener()
            .same_as(&flow_listener(&services, &endpoint))
    );

    viewer
        .send_to(b"after the restart", endpoint.address())
        .await
        .expect("send");
    assert_eq!(next_datagram(flow.as_ref()).await, b"after the restart");
    flow.send(b"and back").await.expect("reply");
    let mut buf = [0u8; 64];
    let (len, from) = tokio::time::timeout(Duration::from_secs(1), viewer.recv_from(&mut buf))
        .await
        .expect("reply in time")
        .expect("receive");
    assert_eq!(&buf[..len], b"and back");
    assert_eq!(from, endpoint.address());
}

fn flow_listener(services: &UdpServices, endpoint: &ListenEndpoint) -> UdpListener {
    services.listeners()[&endpoint.address()].listener.clone()
}

#[tokio::test]
async fn an_unused_udp_listener_closes() {
    let services = UdpServices::with_linger(Duration::from_millis(20));
    let endpoint = free_endpoint();
    drop(services.acquire(&endpoint).await.expect("bind"));
    // A restart within the linger takes it back.
    drop(services.acquire(&endpoint).await.expect("held again"));

    let released = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::net::UdpSocket::bind(endpoint.address()).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(released.is_ok(), "the port is free again");
    assert!(services.listeners().is_empty());
}

#[tokio::test]
async fn a_taken_udp_port_is_an_error() {
    let services = UdpServices::with_linger(Duration::from_millis(20));
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let endpoint = ListenEndpoint::from(taken.local_addr().expect("address"));
    assert!(services.acquire(&endpoint).await.is_err());
    assert!(services.listeners().is_empty());
}
