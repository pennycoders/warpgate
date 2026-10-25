Fork of `ironrdp-acceptor` 0.10.0.

## `RDP` as the Demand Active source descriptor

Upstream sends an empty source descriptor in the server Demand Active PDU, encoded as a
single NUL byte. Jump Desktop rejects it ("bad packet - source Descriptor not set to RDP")
and Windows App on iOS ends the session with a protocol error (0x500d), both right after
the TLS upgrade, so neither can connect to Warpgate's RDP listener. The fork sends `RDP`,
encoded as `RDP\0`, the four bytes the FreeRDP server sends
(`src/connection.rs`, `AcceptorState::CapabilitiesSendServer`).

Upstream: Devolutions/IronRDP#2118, fixed in Devolutions/IronRDP#2119. Drop this fork
once a release carries it.

## Stray PDUs during reactivation

During a deactivation-reactivation the acceptor waits for the Confirm Active, but a client
keeps sending until it has processed the Deactivate All, so a Share Control PDU it sent
before that can arrive first. Upstream already drops what does not decode as a Share
Control PDU during reactivation, but ends the connection on one that does ("expected client
confirm active"). Jump Desktop on macOS hits this every time Warpgate resizes the viewer
to the target right after connecting; Windows App on iOS sometimes. The fork drops it too
and logs it at `warn` (`AcceptorState::CapabilitiesWaitConfirm` in `src/connection.rs`).

## The client's RDP version in Server Core Data

Upstream always answers RDP 5.0-8.1 (`0x00080004`) in Server Core Data, whatever the client
sends. Windows and the FreeRDP server answer with the client's version, up to the newest they
know. Windows App on iOS changes the session's orientation only with servers from Windows 8.1
on, and against Warpgate it sends no Display Control layout when the device rotates. The fork
answers like FreeRDP, up to RDP 10.12 (`common_rdp_version` in `src/connection.rs`).
