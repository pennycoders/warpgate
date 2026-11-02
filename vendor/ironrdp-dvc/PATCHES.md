Fork of `ironrdp-dvc` 0.10.0.

## Data for a channel that is not open is dropped

Backport of IronRDP branch `fix/dvc-data-for-unopened-channel`: 0.10.0 ends the session
with "access to non existing DVC channel" on data for a channel it refused or closed.
FreeRDP's RDPSND server (GNOME Remote Desktop) opens `AUDIO_PLAYBACK_DVC` and sends its
formats before reading the client's answer, so a session to a GNOME target that the
client refuses the channel to ended right after connecting. Such data is now dropped.

## The server keeps a channel it closed until the client's Close response

0.10.0's `DrdynvcServer::close_channel` forgets the channel as it sends the Close Request, so
data the client sent before reading that request, and the client's Close response, arrive for
a channel the server no longer knows. Data then fails decoding with "invalid channel id" and
ends the session. Warpgate closes the viewer's AUDIO_INPUT channel when the target stops
recording, while the viewer (Windows App on macOS) is still sending audio, so a recording
ended the session with error 0x4.

[MS-RDPEDYC] (v20240423) describes the close as an exchange during which the client can
still be sending:

- 2.2.4: a DYNVC_CLOSE PDU "is used for both a close request and a close response."
- 1.3.3.3 and 3.3.5.2: "The DVC server manager sends a DYNVC_CLOSE (section 2.2.4) PDU
  (specifying the ChannelId to close) to the DVC client manager. The client replies with a
  DYNVC_CLOSE (section 2.2.4) PDU."
- 3.2.5.2: "When a DVC client manager receives a DYNVC_CLOSE (section 2.2.4) PDU, the client
  MAY respond", so the response may never come.
- 3.1.1: "The ChannelId is valid until the server or client issues a Closing a DVC PDU", so
  data that crosses the request has no channel left to go to.

The server now keeps the ID of a channel it closed in a closing set until the Close response
arrives, and accepts that response without answering it. The set holds at most 64 IDs, as
the client may not respond. DATA_FIRST, DATA and both compressed data PDUs (3.1.5.2) for a
closing or unknown channel, over DRDYNVC or a Soft-Sync tunnel, are dropped with a debug log,
and so is a Create Response for a channel the server closed before the client answered.
These PDUs are well-formed, so 3.1.5.2.4 (terminate on an unrecognized or malformed packet)
does not call for ending the session over them.

The client side needs no change: it already drops data for a channel it does not have (the
fix above), drops data for a channel it closed itself, and answers a Close for an unknown
channel with nothing.

Drop this fork once an `ironrdp-dvc` release has both fixes.

`Cargo.toml` additionally sets `[lints.rust] warnings = { level = "allow", priority = 1 }`
so this vendored path dependency's warnings don't surface in Warpgate's builds. This is
not in `warpgate.patch` (which is source-only); re-apply it by hand on re-vendor.
