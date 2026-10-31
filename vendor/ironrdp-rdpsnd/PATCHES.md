Fork of `ironrdp-rdpsnd` 0.11.0.

## The Training Confirm echoes the Training PDU's `wPackSize`

Backport of Devolutions/IronRDP#2294 (`f86e3c49`): 0.11.0 answers the server's Training
PDU with the length of its data instead of its `wPackSize` (the size of the whole PDU), as
MS-RDPEA 2.2.3.2 requires. GNOME Remote Desktop checks it, never starts playback and ends
the audio channel after 10 seconds, so a target's sound never reached the client.

## The server over `AUDIO_PLAYBACK_DVC`

Server half of IronRDP branch `feat/rdpsnd-dvc` (`dvc` module, `RdpsndDvcServer`); 0.11.0
has the client half (`client::RdpsndDvc`) but serves audio only on the `rdpsnd` static
channel. Some clients, Jump Desktop among them, take audio only on the dynamic channel and
never open the static one.

## Sample-rate reduction for 16-bit PCM

Backport of IronRDP branch `feature/rdpsnd-pcm-decimation` (`pcm` module,
`PcmDecimator`). Jump Desktop on macOS accepts only 22.05 kHz audio; viewers that take only
half the target's rate get the stream through it.

Drop this fork once Warpgate builds against an `ironrdp-rdpsnd` release with all three.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
