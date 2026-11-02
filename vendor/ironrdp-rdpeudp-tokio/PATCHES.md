Fork of `ironrdp-rdpeudp-tokio` 0.2.0, as published on crates.io (packaged from IronRDP
`b13befa0db86f48f523a0b2b007e399f9811e0ec`, the same release commit as the
`ironrdp-server` fork). `warpgate.patch` is the source difference from the published crate
(`diff -ru ironrdp-rdpeudp-tokio-0.2.0/src base/src`).

## Accepting over a caller-supplied datagram endpoint

MS-RDPEUDP 2.1 and 3.1.3 have the terminal server bind one UDP socket, on the RDP port
(3389 by default), and take every connection on it; the client sends its SYN
(3.1.5.1.1) to the port the server listens on, and each of its connections comes from a
port of its own. Upstream's server side, `accept_udp`, takes a socket per connection and
`connect()`s it to the first peer that sends to it, so two sessions cannot share the port:
the second session's bind fails.

The driver already reads and writes through a `DatagramIo` trait (a `UdpSocket`, or the
ICE candidate pair), but the trait and `accept_udp_with_datagram_io` were crate-private
and the latter compiled only with the `ice` feature. The fork:

- makes `DatagramIo` public (re-exported at the crate root) and documents its contract,
  including that `recv` must be cancellation safe since the driver awaits it inside
  `tokio::select!`;
- makes `accept_udp_with_datagram_io` public and unconditional, bounded by
  `UdpAcceptConfig::accept_timeout` like `accept_udp`. The ICE path, which applies its
  own timeout, calls the unbounded body (`accept_udp_with_datagram_io_inner`) as before.

A server that listens on one port demultiplexes it by peer address and hands each peer's
datagrams, starting with its SYN, to `accept_udp_with_datagram_io`. The handshake checks
are unchanged: the version 3 `cookieHash` (2.2.2.9, 3.1.5.1.1) and the RDPEMT Tunnel
Create Request's `RequestID` and `SecurityCookie` (MS-RDPEMT 3.2.5.1).

`transport::tests::accepts_over_a_shared_server_socket` connects a client to a socket the
test reads with `recv_from`, as such a listener does, and passes data both ways. It needs
a rustls provider: `cargo test --lib --features rustls-aws-lc-rs`.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The
package's `.cargo_vcs_info.json`, `.cargo-ok` and `Cargo.lock` are left out, and the
license files are IronRDP's.

Drop the fork once IronRDP publishes a public datagram accept entry point.
