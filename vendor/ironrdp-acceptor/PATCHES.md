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
