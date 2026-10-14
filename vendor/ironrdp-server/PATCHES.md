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

## Disconnecting the client with a reason

Upstream 0.13.0 ends a connection the server side closes (`ServerEvent::Quit`, or the
display's update stream ending) by dropping the socket, so the client sees a transport error
instead of a disconnection. The fork backports upstream's `ServerEvent::Disconnect(ErrorInfo)`,
which sends the client a Set Error Info PDU first, and ends every server-initiated
disconnection with an MCS Disconnect Provider Ultimatum (`src/server.rs`). Warpgate uses it
to pass the target's reason for ending a session on to the viewer.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
