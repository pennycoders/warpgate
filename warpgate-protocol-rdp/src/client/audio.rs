//! Target-side audio output (MS-RDPEA): accept the target's sound as PCM in
//! [`DESKTOP_AUDIO_FORMAT`] and hand each block to the session as a [`DesktopEvent::Audio`].
//!
//! The sound can arrive on the `rdpsnd` static channel or on the `AUDIO_PLAYBACK_DVC`
//! dynamic channel; servers that have both (Windows, GNOME Remote Desktop) prefer the
//! dynamic one.

use std::borrow::Cow;

use bytes::Bytes;
use ironrdp::rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp::rdpsnd::dvc::{AUDIO_PLAYBACK_DVC, RdpsndDvcClient};
use ironrdp::rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info, warn};
use warpgate_core::{DESKTOP_AUDIO_FORMAT, DesktopAudioFormat, DesktopEvent};

/// The format offered to the target, as an MS-RDPEA format descriptor.
pub const fn pcm_format(format: DesktopAudioFormat) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: format.channels,
        n_samples_per_sec: format.samples_per_sec,
        n_avg_bytes_per_sec: format.avg_bytes_per_sec(),
        n_block_align: format.block_align(),
        bits_per_sample: format.bits_per_sample,
        data: None,
    }
}

/// The target's sound on `AUDIO_PLAYBACK_DVC`: the same PDUs and negotiation as the static
/// channel.
pub fn dynamic_audio(handler: AudioForwarder) -> RdpsndDvcClient {
    RdpsndDvcClient::new(Rdpsnd::new(Box::new(AudioForwarder {
        channel: AUDIO_PLAYBACK_DVC,
        ..handler
    })))
}

#[derive(Debug)]
pub struct AudioForwarder {
    formats: [AudioFormat; 1],
    events: Sender<DesktopEvent>,
    dropped: u64,
    /// The channel the sound arrives on, logged with the first block.
    channel: &'static str,
    playing: bool,
}

impl AudioForwarder {
    pub const fn new(events: Sender<DesktopEvent>) -> Self {
        Self {
            formats: [pcm_format(DESKTOP_AUDIO_FORMAT)],
            events,
            dropped: 0,
            channel: "rdpsnd",
            playing: false,
        }
    }
}

impl RdpsndClientHandler for AudioForwarder {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: Cow<'_, [u8]>) {
        // Only one format is offered, so the target can only pick that one.
        if format_no != 0 {
            warn!(format_no, "Ignoring audio in a format that was not offered");
            return;
        }
        // A block that does not end on a sample frame would shift every later sample.
        if !data
            .len()
            .is_multiple_of(usize::from(DESKTOP_AUDIO_FORMAT.block_align()))
        {
            warn!(
                len = data.len(),
                "Ignoring audio that is not whole sample frames"
            );
            return;
        }
        if !self.playing {
            self.playing = true;
            info!(channel = self.channel, "Playing the target's sound");
        }
        // Sound is the first thing worth dropping when the viewer falls behind: a late
        // block is useless, and waiting here would stall the target's whole session.
        match self
            .events
            .try_send(DesktopEvent::Audio(Bytes::copy_from_slice(&data)))
        {
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                debug!(
                    dropped = self.dropped,
                    "Dropping audio, the viewer is behind"
                );
            }
            Ok(()) | Err(TrySendError::Closed(_)) => {}
        }
    }

    // The viewer's own volume control applies; the target's is not forwarded.
    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use ironrdp::dvc::DvcMessage;
    use ironrdp::rdpsnd::client::RdpsndClientHandler as _;
    use ironrdp::rdpsnd::pdu::WaveFormat;
    use tokio::sync::mpsc;
    use warpgate_core::{DESKTOP_AUDIO_FORMAT, DesktopEvent};

    use super::AudioForwarder;

    /// A server PDU already encoded, as a DVC message.
    struct RawPdu(Vec<u8>);

    impl ironrdp::core::Encode for RawPdu {
        fn encode(
            &self,
            dst: &mut ironrdp::core::WriteCursor<'_>,
        ) -> ironrdp::core::EncodeResult<()> {
            dst.write_slice(&self.0);
            Ok(())
        }

        fn name(&self) -> &'static str {
            "RawPdu"
        }

        fn size(&self) -> usize {
            self.0.len()
        }
    }

    impl ironrdp::dvc::DvcEncode for RawPdu {}

    #[test]
    fn offers_16_bit_stereo_pcm_at_44_1_khz() {
        let (tx, _rx) = mpsc::channel(1);
        let forwarder = AudioForwarder::new(tx);
        let [format] = forwarder.get_formats() else {
            panic!("expected exactly one format");
        };
        assert_eq!(format.format, WaveFormat::PCM);
        assert_eq!(format.n_channels, 2);
        assert_eq!(format.n_samples_per_sec, 44_100);
        assert_eq!(format.bits_per_sample, 16);
        assert_eq!(format.n_block_align, 4);
        assert_eq!(format.n_avg_bytes_per_sec, 176_400);
        assert_eq!(format.data, None);
    }

    #[test]
    fn forwards_each_block_unchanged() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(0, 7, Cow::Borrowed(&[1, 2, 3, 4, 5, 6, 7, 8]));
        let Ok(DesktopEvent::Audio(data)) = rx.try_recv() else {
            panic!("expected an audio event");
        };
        assert_eq!(&data[..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn ignores_formats_it_did_not_offer() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(1, 0, Cow::Borrowed(&[0; 4]));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ignores_partial_sample_frames() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(0, 0, Cow::Borrowed(&[0; 6]));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drops_rather_than_waits_when_the_viewer_is_behind() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(0, 0, Cow::Borrowed(&[1; 4]));
        forwarder.wave(0, 0, Cow::Borrowed(&[2; 4]));
        assert_eq!(forwarder.dropped, 1);
        let Ok(DesktopEvent::Audio(data)) = rx.try_recv() else {
            panic!("expected the first block");
        };
        assert_eq!(&data[..], &[1; 4]);
        assert!(rx.try_recv().is_err());
    }

    /// The exchange a desktop runs on `AUDIO_PLAYBACK_DVC`, through IronRDP's DVC client:
    /// the channel is accepted, only PCM is offered back, training is confirmed, and a
    /// Wave2 block reaches the session.
    #[test]
    fn plays_sound_from_the_dynamic_channel() {
        use std::borrow::Cow;

        use ironrdp::core::{Decode as _, ReadCursor, encode_vec};
        use ironrdp::dvc::pdu::{
            CapabilitiesRequestPdu, CapsVersion, CreateRequestPdu, CreationStatus,
            DrdynvcClientPdu, DrdynvcDataPdu, DrdynvcServerPdu,
        };
        use ironrdp::dvc::{DrdynvcClient, encode_dvc_messages};
        use ironrdp::rdpsnd::pdu::{
            ClientAudioOutputPdu, ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingPdu, Version,
            Wave2Pdu,
        };
        use ironrdp::svc::{ChannelFlags, SvcProcessor as _};

        use super::{AUDIO_PLAYBACK_DVC, dynamic_audio, pcm_format};

        const ID: u32 = 5;
        let (tx, mut rx) = mpsc::channel(4);
        let mut drdynvc =
            DrdynvcClient::new().with_dynamic_channel(dynamic_audio(AudioForwarder::new(tx)));
        let mut exchange = |pdu: DrdynvcServerPdu| -> Vec<DrdynvcClientPdu> {
            drdynvc
                .process(&encode_vec(&pdu).unwrap())
                .unwrap()
                .iter()
                .map(|m| {
                    let bytes = m.encode_unframed_pdu().unwrap();
                    DrdynvcClientPdu::decode(&mut ReadCursor::new(&bytes)).unwrap()
                })
                .collect()
        };
        let audio = |pdu: ServerAudioOutputPdu<'_>| {
            let message: DvcMessage = Box::new(RawPdu(encode_vec(&pdu).unwrap()));
            let [svc] = encode_dvc_messages(ID, vec![message], ChannelFlags::empty())
                .unwrap()
                .try_into()
                .unwrap();
            let bytes = svc.encode_unframed_pdu().unwrap();
            DrdynvcServerPdu::decode(&mut ReadCursor::new(&bytes)).unwrap()
        };
        let replies = |responses: Vec<DrdynvcClientPdu>| -> Vec<ClientAudioOutputPdu> {
            responses
                .into_iter()
                .filter_map(|r| match r {
                    DrdynvcClientPdu::Data(data) => {
                        assert_eq!(data.channel_id(), ID);
                        let DrdynvcDataPdu::Data(data) = data else {
                            panic!("audio replies fit in one DVC PDU");
                        };
                        Some(
                            ClientAudioOutputPdu::decode(&mut ReadCursor::new(data.data()))
                                .unwrap(),
                        )
                    }
                    _ => None,
                })
                .collect()
        };

        exchange(DrdynvcServerPdu::Capabilities(CapabilitiesRequestPdu::new(
            CapsVersion::V3,
            None,
        )));
        let created = exchange(DrdynvcServerPdu::Create(CreateRequestPdu::new(
            ID,
            AUDIO_PLAYBACK_DVC.to_owned(),
        )));
        assert!(matches!(
            created.as_slice(),
            [DrdynvcClientPdu::Create(r)] if r.creation_status() == CreationStatus::OK
        ));

        let pcm = pcm_format(DESKTOP_AUDIO_FORMAT);
        let mut opus = pcm.clone();
        opus.format = WaveFormat::OPUS;
        let formats = replies(exchange(audio(ServerAudioOutputPdu::AudioFormat(
            ServerAudioFormatPdu {
                version: Version::V8,
                formats: vec![opus, pcm.clone()],
            },
        ))));
        assert!(matches!(
            formats.first(),
            Some(ClientAudioOutputPdu::AudioFormat(f)) if f.formats == vec![pcm.clone()]
        ));

        // GNOME Remote Desktop rejects a confirm whose wPackSize is not the Training PDU's
        // own (the size of the whole PDU, MS-RDPEA 2.2.3.2) and never starts playback.
        let training = TrainingPdu {
            timestamp: 3,
            data: vec![0xAB; 32],
        };
        let training_size = encode_vec(&ServerAudioOutputPdu::Training(training.clone()))
            .unwrap()
            .len();
        let confirm = replies(exchange(audio(ServerAudioOutputPdu::Training(training))));
        assert!(matches!(
            confirm.as_slice(),
            [ClientAudioOutputPdu::TrainingConfirm(c)]
                if c.timestamp == 3 && usize::from(c.pack_size) == training_size
        ));

        let confirmed = replies(exchange(audio(ServerAudioOutputPdu::Wave2(Wave2Pdu {
            timestamp: 9,
            format_no: 0,
            block_no: 1,
            audio_timestamp: 9,
            data: Cow::Owned(vec![1, 2, 3, 4]),
        }))));
        assert!(matches!(
            confirmed.as_slice(),
            [ClientAudioOutputPdu::WaveConfirm(c)] if c.block_no == 1
        ));
        let Ok(DesktopEvent::Audio(data)) = rx.try_recv() else {
            panic!("expected the block in the session");
        };
        assert_eq!(&data[..], &[1, 2, 3, 4]);
    }

    #[test]
    fn a_closed_session_is_not_an_error() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(0, 0, Cow::Borrowed(&[0; 4]));
        assert_eq!(forwarder.dropped, 0);
    }
}
