Fork of `ironrdp-acceptor` 0.12.0.

## Stray PDUs during reactivation

During a deactivation-reactivation the acceptor waits for the Confirm Active, but a client
keeps sending until it has processed the Deactivate All, so a Share Control PDU it sent
before that can arrive first. Upstream already drops what does not decode as a Share
Control PDU during reactivation, but ends the connection on one that does ("expected client
confirm active"). Jump Desktop on macOS hits this every time Warpgate resizes the viewer
to the target right after connecting; Windows App on iOS sometimes. The fork drops it too
and logs it at `warn` (`AcceptorState::CapabilitiesWaitConfirm` in `src/connection.rs`).

IronRDP master still ends the connection there; drop this fork once a release does not.

## The client's RDP version in Server Core Data

Upstream always answers RDP 5.0-8.1 (`0x00080004`) in Server Core Data, whatever the client
sends. Windows and the FreeRDP server answer with the client's version, up to the newest they
know. Windows App on iOS changes the session's orientation only with servers from Windows 8.1
on, and against Warpgate it sends no Display Control layout when the device rotates. The fork
answers like FreeRDP, up to RDP 10.12 (`common_rdp_version` in `src/connection.rs`).

A Windows 11 host also sets `EDGE_ACTIONS_SUPPORTED_V2` in its early capabilities (with
`SKIP_CHANNELJOIN_SUPPORTED` when asked); the RDP version alone does not make Windows App turn
the session. The fork sets the edge actions for RDP 10 clients (`server_early_capabilities`).

(The `RDP` Demand Active source descriptor the 0.10.0 fork also carried is in 0.12.0,
upstream #2119.)

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
