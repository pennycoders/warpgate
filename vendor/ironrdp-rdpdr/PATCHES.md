Fork of `ironrdp-rdpdr` 0.9.0.

The directory is the crate as published (`cargo package` output); `warpgate.patch` is the
source difference from it.

## Forwarding drive requests between two channels

Warpgate terminates RDPDR on both sides of a session: it is the target's RDPDR client and
the viewer's RDPDR server, and relays a viewer's redirected drives IRP by IRP so it can
apply its own policy (read-only drives, no device control) and log transfers. The server
side's `drive_*` methods cannot carry such a relay: each allocates its own `CompletionId`
and `drive_create` fixes `FileAttributes`, `SharedAccess` and `AllocationSize`, so the
viewer would see a different request than the target sent.

The fork adds `RdpdrServer::forward_drive_io_request`, which re-encodes a decoded
`ServerDriveIoRequest` whole, keeping its `DeviceId`, `FileId` and `CompletionId`. The
device must be one the client announced and the server accepted, and the id must not
already be pending; a request that fails either check, or is a device-control request
(whose input buffer `ServerDriveIoRequest` does not carry), goes to the new
`RdpdrServerBackend::on_forward_rejected` (default no-op) rather than failing the channel,
so the proxy can complete it toward the server it came from. `IrpTracker` gains
`register_as` for the caller-chosen id. `RdpdrServerMessage::Forward` carries the request
through the server event loop (dispatched by the `ironrdp-server` fork).

## Server User Logged On

`RdpdrServer` never sent the Server User Logged On PDU, and per [MS-RDPEFS] 3.2.5.1.6 a
client announces drives (anything but smart cards) only after it; mstsc and IronRDP's own
client wait for it, so no drive was ever announced to an `RdpdrServer`. The fork sends it
right after the client's capability response: an `ironrdp-server` session is already
authenticated by then.

## Visibility

`DeviceAnnounceHeader::device_type` and the fields of `ClientDriveSetInformationResponse`
were `pub(crate)`; a server backend needs the former to tell drives from other devices,
and a relay the latter to read a completion's header. Both are `pub` now, like the
neighbouring types.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is not
in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.

Drop the fork once a release has a way to forward a request with its ids and sends the
User Logged On PDU.
