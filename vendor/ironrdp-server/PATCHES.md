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

## Last row of a sub-rectangle

`BitmapUpdate::sub` slices a changed region out of the frame with the frame's stride, so
its data ends at the region's last pixel, short of a full stride. The legacy bitmap
encoder counted rows as `len / stride`, which drops that last row: every region narrower
than the screen went out one row short and one row high, leaving a stale row behind
(horizontal lines in Jump Desktop while the screen changes). `src/encoder/bitmap.rs`
rounds up.

## Pointer AND mask

`DisplayUpdate::RGBAPointer` went out with an empty AND mask, which MS-RDPBCGR
2.2.9.1.1.4.4 does not allow; Jump Desktop rejects the update ("bad length for And mask").
`src/encoder/mod.rs` sends a 1 bpp mask, set where the pointer is fully transparent.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
