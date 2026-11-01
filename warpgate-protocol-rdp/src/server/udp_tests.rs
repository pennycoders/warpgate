use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ironrdp_core::MonotonicInstant;
use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
use ironrdp_pdu::rdp::multitransport::{MultitransportRequestPdu, RequestedProtocol};
use ironrdp_rdpeudp::pdu::UdpVersion;
use ironrdp_rdpeudp::{ConnectionConfig, RdpeudpConnection};
use ironrdp_server::SidebandDatagrams;
use tokio::net::UdpSocket;

use super::{DropReason, Route, UdpListener, cookie_hash};

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
    client.poll_transmit(now).expect("SYN").contents
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
    listener.deliver(&socket, addr(VIEWER, 61002), b"to the second");
    listener.deliver(&socket, addr(VIEWER, 61001), b"to the first");
    assert_eq!(next_datagram(first.as_ref()).await, b"to the first");
    assert_eq!(next_datagram(second.as_ref()).await, b"to the second");
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
async fn a_syn_with_an_unknown_cookie_is_not_routed_while_requests_are_ambiguous() {
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
        Route::Drop(DropReason::Ambiguous)
    );
    listener.deliver(&socket, addr(VIEWER, 61003), &wrong);
    assert!(opened(first).await.is_none());
    assert!(opened(second).await.is_none());
}

#[tokio::test]
async fn a_syn_with_an_unknown_cookie_goes_to_the_only_request_from_its_address() {
    // MS-RDPEUDP 3.1.5.1.1 drops a connection whose cookieHash is invalid to version 2
    // instead of refusing it; the RDPEMT Tunnel Create Request still has to carry the
    // right cookie (MS-RDPEMT 3.2.5.1).
    let listener = UdpListener::new();
    let socket = reply_socket().await;
    let only = listener
        .sideband(addr(VIEWER, 50000))
        .open(&request(1, [0x11; 16]));
    let wrong = syn(UdpVersion::V3, [0x33; 16]);
    assert_eq!(
        listener.routes().route(addr(VIEWER, 61003), &wrong),
        Route::Open {
            id: 0,
            by_cookie: false
        }
    );
    listener.deliver(&socket, addr(VIEWER, 61003), &wrong);
    let only = opened(only).await.expect("flow");
    assert_eq!(next_datagram(only.as_ref()).await, wrong);
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
        Route::Drop(DropReason::SourceMismatch)
    );
    listener.deliver(&socket, addr(OTHER_VIEWER, 61001), &stolen);
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
    let _flow = opened(pending).await.expect("flow");
    for _ in 0..super::FLOW_QUEUE * 2 {
        listener.deliver(&socket, addr(VIEWER, 61001), b"data");
    }
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
