Fork of `ironrdp-session` 0.13.0.

## Logon information

Upstream reports a Server Save Session Info PDU ([MS-RDPBCGR] 2.2.10.1) only as
`ActiveStageOutput::SessionId` and a value-free `ActiveStageOutput::SaveSessionInfo {
logon_complete }`; the account the server logged the session on as is not surfaced.
`warpgate-protocol-rdp` audits that account with the target's own logon. The fork adds
`ProcessorOutput::LogonInfo` / `ActiveStageOutput::LogonInfo`, carrying the PDU's
`LogonInfo` (session ID, user name and domain; nothing else) for Logon Info Version 1
and 2. It is emitted between the `SessionId` and the `SaveSessionInfo` outputs of the same
PDU, so a consumer pairs it with the `SaveSessionInfo` that follows it in the same batch
(`completed_logons` in `warpgate-protocol-rdp/src/client/logon.rs`). The value-free
`SaveSessionInfo` stays as upstream has it: the extended logon info, which carries the
auto-reconnect cookie, is still not surfaced.

Drop this fork once a release surfaces the logon account.

## Error info kept in the disconnect reason

Upstream turns a Set Error Info PDU into `GracefulDisconnectReason::Other` with only its
description, so a caller cannot tell which disconnection it was or pass the code on.
`GracefulDisconnectReason` gains an `ErrorInfo` variant carrying it (`src/active_stage.rs`),
which Warpgate forwards to its own viewers.

Upstream: pennycoders/IronRDP `fix/session-disconnect-error-info`.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor. The same
goes for its `zstd-safe = "8.2"` requirement (upstream pins `7.2`); the streaming API the
code uses is unchanged between the two.
