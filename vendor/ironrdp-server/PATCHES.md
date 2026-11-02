Fork of `ironrdp-server` 0.14.0, packaged from IronRDP master at
`b13befa0db86f48f523a0b2b007e399f9811e0ec`.

## Unreleased base

IronRDP no longer publishes `ironrdp-server` (`release = false` in its
`release-plz.toml`); the last release, 0.13.0, depends on `ironrdp-pdu` 0.9 and cannot
share a dependency tree with the `ironrdp-session` 0.13 / `ironrdp-connector` 0.12
generation. `b13befa` is the release commit that published every other IronRDP crate of
that generation (`ironrdp-pdu` 0.11.0, `ironrdp-acceptor` 0.12.0, `ironrdp-egfx` 0.5.0,
...), so the server source taken from it builds against those releases unchanged. The
directory is the output of `cargo package --no-verify --exclude-lockfile -p
ironrdp-server` at that commit; `warpgate.patch` is the source difference from it.

It needs `ironrdp-error` 0.2.1 (`ResultExt`).

## Backported upstream fixes

`warpgate.patch` carries four IronRDP master commits made after `b13befa`, applied in this
order with their `ironrdp-server/src` changes only (the testsuite changes stay upstream).
Together they make `src/` identical to IronRDP master at `904a7da6` except for #2293
(rdpsnd left unstarted for a client that asked for no audio), which needs an
`ironrdp-acceptor` change and is not taken. All four drop out on a re-vendor past
`904a7da6`.

- **#2261, size the display for non-resizing clients before input.** A client that does
  not set `desktopResizeFlag` (Windows App on iOS) keeps the size it asked for, but
  upstream skipped `request_initial_size` for it, so the encoder could be built at another
  size; and input queued during activation was replayed before the display was sized.
- **#2291, keep pointer updates within the client's pointer cache.** A pointer update whose
  `cache_index` is not below the client's `pointerCacheSize` is dropped, since mstsc ends
  the session and FreeRDP drops the connection on one. `DisplayContext` gains
  `pointer_cache_size` and `color_pointer_cache_size`.
- **#2123, cap fast-path updates at the client's MaxRequestSize** (Devolutions/IronRDP#2122).
  The encoder used the server's own `max_request_size` (8 MB) and ignored the client's
  Multifragment Update Capability Set; Jump Desktop sends 65536 and dropped the connection
  on the first full-screen bitmap update ("FastPath: Not enough space in the reassembly
  buffer"). This replaces Warpgate's own fix of the same bug. It also counts the rows of a
  bitmap chunk with `div_ceil`: a sub-rectangle's data ends at its last pixel, short of a
  full stride, so the legacy bitmap path sent every region narrower than the screen one row
  short (horizontal lines in Jump Desktop), which Warpgate had patched separately too.
- **#2274, fix pointer update encoding and reuse the RemoteFX buffer.** It also takes the
  desktop size once, outside the loop that grows the buffer, which is the fix for the RFX
  header blocks a buffer retry used to drop: a first frame whose encoding overran the
  buffer consumed the once-per-connection `TS_RFX_SYNC` / `TS_RFX_CONTEXT` /
  `TS_RFX_CHANNELS` / `TS_RFX_CODEC_VERSIONS` blocks on the failed attempt, and every later
  frame was rejected (FreeRDP: `rfx_process_message: incomplete header blocks processing`),
  leaving the viewer black (warp-tech/warpgate#2606). The buffer is now sized from the tile
  count up front as well. Warpgate's own patch for this is replaced by upstream's.

## Bitmap scan width in bytes

The legacy bitmap path (clients without Surface Commands, such as Jump Desktop) writes the
Compressed Data Header's `cbScanWidth` as the width in pixels. MS-RDPBCGR words it that
way, but the FreeRDP server sends `width * 4` for 32 bpp and clients read it so: Jump
Desktop drew every bitmap at a quarter of its width and rejected widths whose quarter is
not a multiple of 4 ("bad width - must be divisible by 4"). `src/encoder/bitmap.rs` sends
the row length in bytes.

## Bitmap widths that are not a multiple of 4 pixels

Upstream `BitmapEncoder::encode` refuses a rectangle whose width is not a multiple of 4 pixels
(`Width must be a multiple of 4`), and the error ends the connection. A target desktop of such a
width, as after a viewer resizes or reconnects at a new size, ends the session at its first
frame on the bitmap path.

`warpgate.patch` pads each row to the next multiple of 4 pixels with copies of its last pixel
and declares the padded width, while the destination rectangle keeps the real width, so the
client clips the padding. The scan width above is that of the padded row. xrdp sends its bitmap
updates the same way (`libxrdp_send_bitmap` in `libxrdp/libxrdp.c`).

## Disconnect Provider Ultimatum on a server-initiated disconnection

`ServerEvent::Disconnect(ErrorInfo)` sends the client a Set Error Info PDU (when it opted
into them) and ends the client loop, but the server then drops the socket, as it does for
`ServerEvent::Quit` and for the display's update stream ending, so the client reports a
transport error instead of a disconnection. Every disconnection the server side starts now
ends with an MCS Disconnect Provider Ultimatum (`ProviderInitiated`) at the end of
`client_loop` in `src/server.rs`. Warpgate uses this to pass the target's reason for ending
a session on to the viewer.

## A pointer the client cannot take becomes its default pointer

The base already carries upstream's large pointer support (#1786, #1787): the server reads
the client's `pointerCacheSize` and Large Pointer Capability Set, advertises its own, and
the encoder promotes an `RGBAPointer` above the client's New Pointer ceiling to a Large
Pointer Update when the client takes those. Where it cannot send the shape at all (no New
Pointer Update support, larger than the client takes, or a `LargePointer` the client has no
384x384 support for), upstream drops the update and the client keeps showing the previous
shape. `warpgate.patch` sends the default pointer instead (`EncoderIter::next` in
`src/encoder/mod.rs`), so a viewer never keeps a shape the target has left. Warpgate does
not know a viewer's pointer capabilities when it builds the update; the encoder does.

## Audio output on `AUDIO_PLAYBACK_DVC` too

With a sound factory, the server also offers `AUDIO_PLAYBACK_DVC` (`RdpsndDvcServer` from the
vendored `ironrdp-rdpsnd`) next to the `rdpsnd` static channel, and sends each
`ServerEvent::Rdpsnd` on the static channel when the client negotiated a format there,
otherwise on the dynamic one. Jump Desktop takes audio only on the dynamic channel.

## A Windows NT platform in the General capability set

Upstream leaves the server's platform unspecified. Windows and xrdp report Windows NT
(`OSMAJORTYPE_WINDOWS`, `OSMINORTYPE_WINDOWS_NT`), and Windows App on iOS turns a session with
the device only on servers it takes for Windows 8.1 or later. The fork reports Windows NT
(`general_capabilities` in `src/capabilities.rs`).

## A Display Control layout that does not decode is dropped

`DisplayControlServer` returns its decode error, which ends the client loop, so a single
monitor layout outside MS-RDPEDISP's 200..=8192 pixel range ended the session. Jump
Desktop on macOS sends such layouts, and its sessions ended with a protocol error. The
channel is wrapped in `TolerantDisplayControl`, which logs the PDU and ignores it; a
layout is only a resize request, so the session keeps its size.

## `AUDIO_INPUT` opens when recording starts

Upstream registers `AUDIO_INPUT` with the other dynamic channels, so it is created while the
connection is set up. Windows App (macOS and iOS) accepts that channel and answers the
server's Version PDU, but never sends the Incoming Data and Sound Formats PDUs that MS-RDPEAI
3.2.5.1.4 and 3.2.5.1.5 require, so the server never gets to send Open (3.3.5.1.6) and the
microphone stays silent. A Windows server creates the channel only when an application
starts recording and closes it when the last one stops; the same client then completes the
initialization sequence of MS-RDPEAI 1.3.1. `ServerEvent::RdpeaiOpenChannel` creates the
channel (with a fresh backend from the factory) and `ServerEvent::RdpeaiCloseChannel` closes
it with a Close Request (MS-RDPEDYC 2.2.4), which is also the only way MS-RDPEAI has to stop
a client recording. `ServerEvent::Rdpeai` goes to the channel opened that way. The embedder
must not open the channel before the client's DVC Capabilities Response (MS-RDPEDYC 3.3.5.1).

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The same
goes for its `zstd-safe = "8.2"` requirement (upstream pins `7.2`); the streaming API the
code uses is unchanged between the two.

## Forwarded drive requests

`RdpdrServerMessage::Forward` (added by the `ironrdp-rdpdr` fork, see its PATCHES.md) is
dispatched to `RdpdrServer::forward_drive_io_request`, next to the other RDPDR messages.
Drop with the `ironrdp-rdpdr` fork.

## The client's multitransport flags in `ConnectionInfo`

`ConnectionInfo` gains `multitransport_flags`, the flags of the client's GCC Client
Multitransport Channel Data (MS-RDPBCGR 2.2.1.3.8) that `AcceptorResult` already carries,
so an embedder can see which clients offer the UDP transports (MS-RDPBCGR 2.2.15.1) and
Soft-Sync (MS-RDPEDYC 3.1.5.3) without reaching into the acceptor. Warpgate logs it once
per viewer connection.

## UDP multitransport over the embedder's own listener

Upstream's `with_udp_transport` binds a fresh UDP socket per connection for the sideband
transport (`multitransport::accept`), which `ironrdp-rdpeudp-tokio` then `connect()`s to
the first peer that sends to it. MS-RDPEUDP 2.1 and 3.1.3 have the terminal server listen
on one UDP socket, bound to the RDP port, for every connection, and the client sends its
SYN (3.1.5.1.1) to that port; with a socket per connection a second session's bind fails
and it stays on TCP, so only one session at a time can use UDP.

`RdpServerBuilder::with_udp_sideband` (`RdpServerOptions::udp_sideband`) takes an
`Arc<dyn UdpSideband>` instead. When the acceptor has sent the Initiate Multitransport
Request (MS-RDPBCGR 2.2.15.1), the server calls `UdpSideband::open` with it, before
anything else runs, so the embedder can be ready for the client's SYN: a version 3 SYN
carries the SHA-256 of the request's `securityCookie` as its `cookieHash` (MS-RDPEUDP
2.2.2.9). `open` returns a future of the client's datagrams (`SidebandDatagrams`, SYN
first), and `multitransport::accept_sideband` runs the RDPEUDP, TLS and RDPEMT handshakes
over them through `ironrdp_rdpeudp_tokio::accept_udp_with_datagram_io` (see that fork's
PATCHES.md). The wait for the datagrams and the handshakes share `UDP_ACCEPT_TIMEOUT`, and
every failure falls back to TCP-only as on the socket path. The Tunnel Create Request is
still checked against the request's `RequestID` and `SecurityCookie` (MS-RDPEMT 3.2.5.1),
whatever the embedder routed. `with_udp_sideband` takes precedence over
`with_udp_transport`; either one makes `PendingConnection::new` offer the transport
(`RdpServerOptions::offers_udp`).

An embedder that routes by `cookieHash` cannot follow 3.1.5.1.1 step 5 for a version 3 SYN
whose hash is not valid: the server is to accept it and reset the connection to version 2
(which `ironrdp-rdpeudp`'s accept does), but a hash that names no request leaves nothing to
hand the SYN to. Warpgate's listener (`warpgate-protocol-rdp`, `server::udp`) drops such a
SYN, and a client that sends one stays on TCP. A client whose hash is wrong does not hold
the cookie of any waiting request, so its Tunnel Create Request (MS-RDPEMT 3.2.5.1) would
fail on whichever request it were given; Windows validates the hash itself from Windows 10
v2004 on (MS-RDPEUDP 3.1.5.1.1, note 6).

The `multitransport::tests::sideband_*` tests cover a sideband with no datagrams, a client
that never sends, and the full handshake over a socket shared the way an embedder's
listener shares it. They need `rcgen` and tokio's `test-util` as dev-dependencies, which
`Cargo.toml` carries outside `warpgate.patch`; re-add them by hand on re-vendor.

### A random initial sequence number

`accept_config` used `ConnectionConfig::default()`, whose `initial_sequence_number` is 0,
so every SYN+ACK the server sent opened the same sequence space. MS-RDPEUDP 3.1.5.1.1
step 2, which 3.1.5.1.3 applies to the SYN+ACK, requires `snInitialSequenceNumber` to be
"a 32-bit number generated by using a truly random function". The fork draws it from the
operating system's generator (`rand::rngs::OsRng`), falling back to the thread-local
CSPRNG seeded from it, for both the socket and the sideband accept.
`multitransport::tests::every_accept_draws_its_own_initial_sequence_number` checks that
two accepts get different numbers.

### Replacing a client the embedder guessed

An embedder can tell which client answers a request for certain only from a version 3
SYN's `cookieHash` (MS-RDPEUDP 2.2.2.9). A SYN without one (version 1 or 2) can only be
matched by its source address, and a sender that forges that address can be handed the
request first. `SidebandDatagrams::replacement` (default: `None` at once) lets the
embedder hand over such a guess and still offer the right client once its hashed SYN
arrives: `accept_sideband` takes the future before each handshake and, when it resolves,
drops the guessed datagrams and starts the handshake over with the replacement, within the
same `UDP_ACCEPT_TIMEOUT`. A handshake that fails over guessed datagrams does not end the
wait: the forging sender can break its own handshake at once (a bad TLS record, or a bad
Tunnel Create Request; `ironrdp-rdpeudp` does not check the ACK's `snSourceAck` either),
so `accept_sideband` keeps awaiting the replacement until the deadline and only then gives
up. Dropping the future tells the embedder that the accept is over. The Tunnel Create
Request check (MS-RDPEMT 3.2.5.1) applies to whichever client finishes.
`multitransport::tests::sideband_restarts_the_handshake_with_a_replacement` brings the
transport up over the replacement after a guessed client that never answers,
`sideband_waits_for_a_replacement_after_a_guessed_handshake_fails` after one whose
handshake fails before the right client has sent its SYN, and
`sideband_without_a_replacement_ends_with_its_failed_handshake` checks that datagrams with
nothing to replace them still end the accept as soon as their handshake fails.

### UDP accepts end with their connection

`accept_finalize` spawns the UDP accept and hands its `JoinHandle` to `client_loop`;
dropping a `JoinHandle` only detaches the task, so an accept still running when the
connection ended kept its socket, or the embedder's pending request, until
`UDP_ACCEPT_TIMEOUT`. Upstream aborted it only when the client declined the transport.
MS-RDPEMT 1.3.3 has the multitransport connection go with the main RDP connection, so
`accept_finalize` now tracks the accepts it starts in an `AbortOnDrop` and aborts them
when it returns or is dropped. A deactivation-reactivation pass stays inside
`accept_finalize`, so it still leaves the accept running as before.
`server::preempt_tests::a_udp_accept_still_running_ends_with_its_connection` covers the
guard.
