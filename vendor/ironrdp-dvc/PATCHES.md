Fork of `ironrdp-dvc` 0.10.0.

## Data for a channel that is not open is dropped

Backport of IronRDP branch `fix/dvc-data-for-unopened-channel`: 0.10.0 ends the session
with "access to non existing DVC channel" on data for a channel it refused or closed.
FreeRDP's RDPSND server (GNOME Remote Desktop) opens `AUDIO_PLAYBACK_DVC` and sends its
formats before reading the client's answer, so a session to a GNOME target that the
client refuses the channel to ended right after connecting. Such data is now dropped.

Drop this fork once an `ironrdp-dvc` release has the fix.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
