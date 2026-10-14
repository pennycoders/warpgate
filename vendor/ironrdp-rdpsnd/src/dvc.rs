//! RDPSND over a dynamic virtual channel.
//!
//! [MS-RDPEA] 2.1 lets the audio output PDUs travel over the dynamic virtual channel
//! `AUDIO_PLAYBACK_DVC` instead of the `rdpsnd` static channel. The PDUs and the state
//! machines are the same, so [`RdpsndDvcClient`] and [`RdpsndDvcServer`] wrap [`Rdpsnd`] and
//! [`RdpsndServer`] and only change the transport.
//!
//! Servers that support both, such as Windows and GNOME Remote Desktop, offer the dynamic
//! channel first and fall back to the static one when the client refuses it.
//!
//! [MS-RDPEA]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/bea2d5cf-e3b9-4419-92e5-0e074ff9bc5b

use ironrdp_core::{Encode, EncodeResult, WriteCursor, ensure_size, impl_as_any};
use ironrdp_dvc::{DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::{PduResult, encode_err};
use ironrdp_svc::{SvcMessage, SvcProcessor as _};

use crate::client::Rdpsnd;
use crate::server::RdpsndServer;

/// Name of the reliable audio output dynamic virtual channel.
pub const AUDIO_PLAYBACK_DVC: &str = "AUDIO_PLAYBACK_DVC";

/// An RDPSND PDU already encoded by the static-channel state machine.
struct EncodedPdu(Vec<u8>);

impl Encode for EncodedPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RdpsndPdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for EncodedPdu {}

fn into_dvc_messages(messages: Vec<SvcMessage>) -> PduResult<Vec<DvcMessage>> {
    messages
        .into_iter()
        .map(|message| {
            let pdu = message.encode_unframed_pdu().map_err(|e| encode_err!(e))?;
            let message: DvcMessage = Box::new(EncodedPdu(pdu));
            Ok(message)
        })
        .collect()
}

/// RDPSND client on `AUDIO_PLAYBACK_DVC`.
///
/// Register it with `DrdynvcClient::with_dynamic_channel`, next to or instead of the static
/// [`Rdpsnd`]; the server picks the channel.
#[derive(Debug)]
pub struct RdpsndDvcClient {
    inner: Rdpsnd,
}

impl RdpsndDvcClient {
    pub fn new(inner: Rdpsnd) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &Rdpsnd {
        &self.inner
    }
}

impl_as_any!(RdpsndDvcClient);

impl DvcProcessor for RdpsndDvcClient {
    fn channel_name(&self) -> &str {
        AUDIO_PLAYBACK_DVC
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The server speaks first, with its Server Audio Formats PDU.
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.process(payload)?)
    }
}

impl DvcClientProcessor for RdpsndDvcClient {}

/// RDPSND server on `AUDIO_PLAYBACK_DVC`.
///
/// Open it with `DrdynvcServer::create_channel`; once the client accepts the channel, the
/// server sends its formats. Waves, volume and close go out through the methods below,
/// addressed to the channel by the caller.
#[derive(Debug)]
pub struct RdpsndDvcServer {
    inner: RdpsndServer,
}

impl RdpsndDvcServer {
    pub fn new(inner: RdpsndServer) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &RdpsndServer {
        &self.inner
    }

    /// See [`RdpsndServer::wave`].
    pub fn wave(&mut self, data: Vec<u8>, ts: u32) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.wave(data, ts)?.into())
    }

    /// See [`RdpsndServer::set_volume`].
    pub fn set_volume(&mut self, volume_left: u16, volume_right: u16) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.set_volume(volume_left, volume_right)?.into())
    }

    /// Sends the Close PDU; see [`RdpsndServer::close`].
    pub fn close_stream(&mut self) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.close()?.into())
    }
}

impl_as_any!(RdpsndDvcServer);

impl DvcProcessor for RdpsndDvcServer {
    fn channel_name(&self) -> &str {
        AUDIO_PLAYBACK_DVC
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.start()?)
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        into_dvc_messages(self.inner.process(payload)?)
    }
}

impl DvcServerProcessor for RdpsndDvcServer {}
