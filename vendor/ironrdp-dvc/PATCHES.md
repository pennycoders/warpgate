Fork of `ironrdp-dvc` 0.8.0.

## `DrdynvcClient::get_dvc_mut`

Backport of upstream IronRDP's by-type mutable DVC accessor (#1461): 0.8.0 can only
downcast a channel processor immutably, and the Graphics Pipeline compositor drain in
`vendor/ironrdp-session` has to take output out of `GraphicsPipelineClient`. The method is
upstream's verbatim; the private `DynamicChannelSet::get_by_type_id_mut` it relies on
mirrors the existing `get_by_type_id`.

Drop this fork once Warpgate builds against an `ironrdp-dvc` release that has
`get_dvc_mut` (anything after 0.8.0 cut from IronRDP master past #1461).

## Data for a channel that is not open is dropped

Backport of IronRDP branch `fix/dvc-data-for-unopened-channel`: 0.8.0 ended the session with
"access to non existing DVC channel" on data for a channel it refused or closed. FreeRDP's
RDPSND server (GNOME Remote Desktop) opens `AUDIO_PLAYBACK_DVC` and sends its formats before
reading the client's answer, so every session to a GNOME target with audio playback on
ended right after connecting. Such data is now dropped.

Drop this once an `ironrdp-dvc` release has the fix.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
