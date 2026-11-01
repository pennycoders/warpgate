Snapshot of `ironrdp-server` from IronRDP master at `1f4dee6b` (2026-10-27, "fix(server):
size the display for non-resizing clients before input", #2261), version `0.14.0`.

## Why a git snapshot

IronRDP published the rest of its crates on 2026-10-27/28 (`ironrdp-pdu` 0.11,
`ironrdp-connector` 0.12, `ironrdp-session` 0.13, `ironrdp-egfx` 0.5, ...) but not
`ironrdp-server`, whose last release (0.13.0) only builds against the previous generation.
`1f4dee6b` is the latest server commit whose dependencies are all those published
releases: the commits after it (#2232, #2272, #2293, #2107, ...) rely on changes to
`ironrdp-acceptor`, `ironrdp-error` and `ironrdp-dvc` that have no release yet.

Re-vendor from a published `ironrdp-server` 0.14+ once one exists.

## Backported upstream fixes

`warpgate.patch` carries three IronRDP master commits made after `1f4dee6b`, applied in
this order with their `ironrdp-server/src` changes only (the testsuite changes stay
upstream). Together they make `src/` identical to IronRDP master at `904a7da6` except for
#2293 (rdpsnd left unstarted for a client that asked for no audio), which needs an
`ironrdp-acceptor` change and is not taken. All three drop out on a re-vendor past
`904a7da6`. #2261 (size the display for non-resizing clients before input), which an
earlier base of this fork carried as a backport too, is the snapshot commit itself.

- **#2291, keep pointer updates within the client's pointer cache.** A pointer update whose
  `cache_index` is not below the client's `pointerCacheSize` is dropped, since mstsc ends
  the session and FreeRDP drops the connection on one. `DisplayContext` gains
  `pointer_cache_size` and `color_pointer_cache_size`.
- **#2123, cap fast-path updates at the client's MaxRequestSize** (Devolutions/IronRDP#2122).
  The encoder used the server's own `max_request_size` (8 MB) and ignored the client's
  Multifragment Update Capability Set; Jump Desktop sends 65536 and dropped the connection
  on the first full-screen bitmap update ("FastPath: Not enough space in the reassembly
  buffer"). It also counts the rows of a bitmap chunk with `div_ceil`: a sub-rectangle's
  data ends at its last pixel, short of a full stride, so the legacy bitmap path sent every
  region narrower than the screen one row short (horizontal lines in Jump Desktop).
- **#2274, fix pointer update encoding and reuse the RemoteFX buffer.** It also takes the
  desktop size once, outside the loop that grows the buffer, which is the fix for the RFX
  header blocks a buffer retry used to drop: `RemoteFxHandler` carries the desktop size as
  an `Option` whose `Some` makes `RfxEncoder::encode` emit the once-per-connection
  `TS_RFX_SYNC` / `TS_RFX_CONTEXT` / `TS_RFX_CHANNELS` / `TS_RFX_CODEC_VERSIONS` blocks, and
  the snapshot `take()`s it inside the loop, so a first frame whose encoding overran the
  buffer consumed the blocks on the failed attempt and every later frame was rejected
  (FreeRDP: `rfx_process_message: incomplete header blocks processing`), leaving the viewer
  black (warp-tech/warpgate#2606). The buffer is now sized from the tile count up front as
  well. This replaces Warpgate's own patch for the same bug, which read the size without
  consuming it and cleared it once a message carrying the blocks had been built.

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

## Forwarded drive requests

`RdpdrServerMessage::Forward` (added by the `ironrdp-rdpdr` fork, see its PATCHES.md) is
dispatched to `RdpdrServer::forward_drive_io_request`, next to the other RDPDR messages.
Drop with the `ironrdp-rdpdr` fork.

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

## Manifest

`Cargo.toml` is the snapshot's with the `*.workspace = true` package fields inlined, the
`path = "../..."` dependency entries removed (every dependency resolves to its crates.io
release), `[lints] workspace = true` replaced by `warnings = { level = "allow", priority = 1 }`
under `[lints.rust]`, and `zstd-safe = "8.2"` (upstream pins `7.2`; the streaming API the
code uses is unchanged). `LICENSE-*` are copies: in the IronRDP repository they are
symlinks to the workspace root. None of this is in `warpgate.patch` (source-only).
