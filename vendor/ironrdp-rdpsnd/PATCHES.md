Fork of `ironrdp-rdpsnd` 0.9.0.

## The Training Confirm echoes the Training PDU's `wPackSize`

Backport of Devolutions/IronRDP#2019: 0.9.0 answered the server's Training PDU with the
length of its data instead of its `wPackSize` (the size of the whole PDU), as MS-RDPEA
2.2.3.2 requires. GNOME Remote Desktop checks it ("Received invalid Training Confirm PDU"),
never starts playback and ends the audio channel after 10 seconds, so a target's sound
never reached the client.

Drop this fork once Warpgate builds against an `ironrdp-rdpsnd` release with the fix.

## Audio output over `AUDIO_PLAYBACK_DVC`

Backport of IronRDP branch `feat/rdpsnd-dvc` (`dvc` module: `RdpsndDvcClient`,
`RdpsndDvcServer`) and `RdpsndServer::is_ready` from IronRDP master. Some clients, Jump
Desktop among them, take audio only on the dynamic channel and never open the `rdpsnd`
static channel. Adds the `ironrdp-dvc` dependency.

## Sample-rate reduction for 16-bit PCM

Backport of IronRDP branch `feature/rdpsnd-pcm-decimation` (`pcm` module, `PcmDecimator`).
Jump Desktop on macOS accepts only 22.05 kHz audio; viewers that take only half the
target's rate get the stream through it. Formats compare with `==` here, as 0.9.0 has no
`AudioFormat::matches_for_negotiation` and negotiates by equality.

## Pre-v8 clients get the WaveInfo PDU and the Wave PDU as two messages

0.9.0 sent a client below version 8 the WaveInfo PDU and the Wave PDU (MS-RDPEA 2.2.3.3
and 2.2.3.4) in one channel message; such clients read the second as its own message, so
Jump Desktop on macOS (version 5) played nothing. The encoded message is now split into
the two, as IronRDP master sends them (its WavePdu/WaveDataPdu rework).

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
