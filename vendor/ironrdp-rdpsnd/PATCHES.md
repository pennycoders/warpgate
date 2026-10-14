Fork of `ironrdp-rdpsnd` 0.9.0.

## The Training Confirm echoes the Training PDU's `wPackSize`

Backport of Devolutions/IronRDP#2019: 0.9.0 answered the server's Training PDU with the
length of its data instead of its `wPackSize` (the size of the whole PDU), as MS-RDPEA
2.2.3.2 requires. GNOME Remote Desktop checks it ("Received invalid Training Confirm PDU"),
never starts playback and ends the audio channel after 10 seconds, so a target's sound
never reached the client.

Drop this fork once Warpgate builds against an `ironrdp-rdpsnd` release with the fix.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
