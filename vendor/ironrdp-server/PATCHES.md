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

## Audio output on `AUDIO_PLAYBACK_DVC` too

With a sound factory, the server also offers `AUDIO_PLAYBACK_DVC` (`RdpsndDvcServer` from the
vendored `ironrdp-rdpsnd`) next to the `rdpsnd` static channel, and sends each
`ServerEvent::Rdpsnd` on the static channel when the client negotiated a format there,
otherwise on the dynamic one. Jump Desktop takes audio only on the dynamic channel.

## Manifest

`Cargo.toml` is the snapshot's with the `*.workspace = true` package fields inlined, the
`path = "../..."` dependency entries removed (every dependency resolves to its crates.io
release), `[lints] workspace = true` replaced by `warnings = { level = "allow", priority = 1 }`
under `[lints.rust]`, and `zstd-safe = "8.2"` (upstream pins `7.2`; the streaming API the
code uses is unchanged). `LICENSE-*` are copies: in the IronRDP repository they are
symlinks to the workspace root. None of this is in `warpgate.patch` (source-only).
