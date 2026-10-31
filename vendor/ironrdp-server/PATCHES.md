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

IronRDP master fixed this after `b13befa` (#2274, which also sizes the buffer from the tile
count up front); drop the hunk on a re-vendor past it.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The same
goes for its `zstd-safe = "8.2"` requirement (upstream pins `7.2`); the streaming API the
code uses is unchanged between the two.
