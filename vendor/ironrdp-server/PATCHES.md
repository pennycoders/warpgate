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

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The same
goes for its `zstd-safe = "8.2"` requirement (upstream pins `7.2`); the streaming API the
code uses is unchanged between the two.
