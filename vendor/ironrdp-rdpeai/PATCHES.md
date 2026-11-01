Fork of `ironrdp-rdpeai` 0.2.0.

## The server's Sound Formats PDU carries its size

MS-RDPEAI 2.2.2.2 makes `cbSizeFormatsPacket` arbitrary in the Sound Formats PDU a server
sends, and 0.2.0 sends 0. A Windows server sets it to the size of the PDU without
`ExtraData`, as a client must (829 for the 30 formats Windows 11 offers), and Windows App on
macOS and iOS answers the server's Version PDU but never the Sound Formats PDU that carries 0:
no Incoming Data or Sound Formats PDU follows (3.2.5.1.4, 3.2.5.1.5), so recording never
starts. `FormatsPdu::server` now fills in the size, which is a value the field may hold.

Drop this fork once an `ironrdp-rdpeai` release sets the field.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
