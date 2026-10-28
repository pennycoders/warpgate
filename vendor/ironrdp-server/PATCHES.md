Fork of `ironrdp-server` 0.13.0.

## RemoteFX header blocks lost to a buffer retry

`RemoteFxHandler` carries the desktop size as an `Option`, and `Some` is what makes
`RfxEncoder::encode` emit the once-per-connection RFX header blocks (`TS_RFX_SYNC`,
`TS_RFX_CONTEXT`, `TS_RFX_CHANNELS`, `TS_RFX_CODEC_VERSIONS`). Upstream `take()`s that
`Option` *inside* the loop that grows the output buffer on `NotEnoughBytes`, so a first
frame whose encoding overruns the buffer consumes the headers on the attempt that failed
and re-encodes without them. The buffer is sized from the frame's raw BGRA length, and RFX
always emits whole 64x64 tiles, so anything tile-padded — a few-pixel rect, or a thin strip
like a 1px window border — encodes larger than its source and takes that path. The headers
are never re-armed, so every later frame is rejected (FreeRDP: `rfx_process_message:
incomplete header blocks processing`) and the viewer stays black for the life of the
connection while input still works. Which frame arrives first is the target's choice, which
is why this strikes some deployments and not others; see warp-tech/warpgate#2606.

`warpgate.patch` reads the size without consuming it and clears it only once a message
carrying the headers has been built. It also drops a `* 2` from the adjacent buffer-resize
`debug!`, which reported twice the size actually allocated.

## Fast-path updates capped at the client's MaxRequestSize

Upstream hands the update encoder the server's own `max_request_size` (8 MB) and never
reads the client's Multifragment Update Capability Set, although MS-RDPBCGR 2.2.7.2.6
makes the client's `MaxRequestSize` the size of its reassembly buffer. Jump Desktop sends
65536 and drops the connection on the first full-screen bitmap update ("FastPath: Not
enough space in the reassembly buffer"). `warpgate.patch` gives the encoder the smaller of
both sides, keeping the server's when the client sends no such capability set or a zero
size (`client_max_request_size` in `src/server.rs`).

Upstream: Devolutions/IronRDP#2122, fixed in Devolutions/IronRDP#2123.

## Bitmap scan width in bytes

The legacy bitmap path (clients without Surface Commands, such as Jump Desktop) writes the
Compressed Data Header's `cbScanWidth` as the width in pixels. MS-RDPBCGR words it that
way, but the FreeRDP server sends `width * 4` for 32 bpp and clients read it so: Jump
Desktop drew every bitmap at a quarter of its width and rejected widths whose quarter is
not a multiple of 4 ("bad width - must be divisible by 4"). `src/encoder/bitmap.rs` sends
the row length in bytes.

## Pointer AND mask

`DisplayUpdate::RGBAPointer` went out with an empty AND mask, which MS-RDPBCGR
2.2.9.1.1.4.4 does not allow; Jump Desktop rejects the update ("bad length for And mask").
`src/encoder/mod.rs` sends a 1 bpp mask, set where the pointer is fully transparent.

## Last row of a sub-rectangle

`BitmapUpdate::sub` slices a changed region out of the frame with the frame's stride, so
its data ends at the region's last pixel, short of a full stride. The legacy bitmap
encoder counted rows as `len / stride`, which drops that last row: every region narrower
than the screen went out one row short and one row high, leaving a stale row behind
(horizontal lines in Jump Desktop while the screen changes). `src/encoder/bitmap.rs`
rounds up.

## Audio output on `AUDIO_PLAYBACK_DVC` too

With a sound factory, the server also offers `AUDIO_PLAYBACK_DVC` (`RdpsndDvcServer` from the
vendored `ironrdp-rdpsnd`) next to the `rdpsnd` static channel, and sends each
`ServerEvent::Rdpsnd` on the static channel when the client negotiated a format there,
otherwise on the dynamic one. Jump Desktop takes audio only on the dynamic channel.

## Disconnecting the client with a reason

Upstream 0.13.0 ends a connection the server side closes (`ServerEvent::Quit`, or the
display's update stream ending) by dropping the socket, so the client sees a transport error
instead of a disconnection. The fork backports upstream's `ServerEvent::Disconnect(ErrorInfo)`,
which sends the client a Set Error Info PDU first, and ends every server-initiated
disconnection with an MCS Disconnect Provider Ultimatum (`src/server.rs`). Warpgate uses it
to pass the target's reason for ending a session on to the viewer.

## Bitmap widths that are not a multiple of 4 pixels

Upstream `BitmapEncoder::encode` refuses a rectangle whose width is not a multiple of 4 pixels
(`Width must be a multiple of 4`), and the error ends the connection. A target desktop of such a
width, as after a viewer resizes or reconnects at a new size, ends the session at its first
frame on the bitmap path.

`warpgate.patch` pads each row to the next multiple of 4 pixels with copies of its last pixel
and declares the padded width, while the destination rectangle keeps the real width, so the
client clips the padding. The scan width above is that of the padded row. xrdp sends its bitmap
updates the same way (`libxrdp_send_bitmap` in `libxrdp/libxrdp.c`).

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The same
goes for its `zstd-safe = "8.2"` requirement (upstream pins `7.2`); the streaming API the
code uses is unchanged between the two.

## A Windows NT platform in the General capability set

Upstream leaves the server's platform unspecified. Windows and xrdp report Windows NT
(`OSMAJORTYPE_WINDOWS`, `OSMINORTYPE_WINDOWS_NT`), and Windows App on iOS turns a session with
the device only on servers it takes for Windows 8.1 or later. The fork reports Windows NT
(`general_capabilities` in `src/capabilities.rs`).
