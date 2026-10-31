//! The viewer's microphone for the target (MS-RDPEAI), on the `AUDIO_INPUT` dynamic channel.
//!
//! Warpgate offers the target only the PCM formats in [`DESKTOP_MICROPHONE_FORMATS`]. When
//! the target opens capture, the viewer is asked for the same format, so its audio is
//! relayed unchanged: it is only cut into the packet size the target asked for.

use std::sync::{Arc, Mutex, PoisonError};

use ironrdp_pdu::pdu_other_err;
use ironrdp_rdpeai::client::{AudioPacketSink, RdpeaiCaptureHandler, RdpeaiClient};
use ironrdp_rdpeai::pdu::OpenReplyPdu;
use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_svc::SvcMessage;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{debug, info, warn};
use warpgate_core::{DESKTOP_MICROPHONE_FORMATS, DesktopAudioFormat, DesktopEvent};

use super::audio::pcm_format;

/// The relayed format `format` is, if it is one.
fn relayed_format(format: &AudioFormat) -> Option<DesktopAudioFormat> {
    DESKTOP_MICROPHONE_FORMATS
        .into_iter()
        .find(|candidate| pcm_format(*candidate).matches_for_negotiation(format))
}

#[derive(Default)]
struct Capture {
    /// Set while the target records.
    sink: Option<AudioPacketSink>,
    packet_size: usize,
    /// Audio short of a whole packet.
    pending: Vec<u8>,
}

/// The `AUDIO_INPUT` processor to register with the DRDYNVC client, the handle that feeds
/// it the viewer's audio, and the channel its packets come out of; those are DRDYNVC
/// messages for the session to send.
pub struct MicrophoneRelay {
    pub channel: RdpeaiClient,
    pub input: MicrophoneInput,
    pub uplink: UnboundedReceiver<Vec<SvcMessage>>,
}

impl MicrophoneRelay {
    pub fn new(events: Sender<DesktopEvent>) -> Self {
        let capture = Arc::new(Mutex::new(Capture::default()));
        let (uplink_tx, uplink) = unbounded_channel();
        let channel = RdpeaiClient::new(
            Box::new(Handler {
                formats: DESKTOP_MICROPHONE_FORMATS.map(pcm_format).to_vec(),
                capture: Arc::clone(&capture),
                events,
            }),
            uplink_sender(uplink_tx),
        );
        Self {
            channel,
            input: MicrophoneInput(capture),
            uplink,
        }
    }
}

fn uplink_sender(
    tx: UnboundedSender<Vec<SvcMessage>>,
) -> Box<dyn FnMut(u32, Vec<SvcMessage>) -> ironrdp_pdu::PduResult<()> + Send> {
    Box::new(move |_channel_id, messages| {
        tx.send(messages)
            .map_err(|_| pdu_other_err!("the target session has ended"))
    })
}

/// Takes the viewer's audio for the target.
pub struct MicrophoneInput(Arc<Mutex<Capture>>);

impl MicrophoneInput {
    /// Relays `data` while the target records, in packets of the size it asked for; drops
    /// it otherwise.
    pub fn push(&self, data: &[u8]) {
        let mut capture = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Capture {
            sink,
            packet_size,
            pending,
        } = &mut *capture;
        let Some(sink) = sink else {
            return;
        };
        pending.extend_from_slice(data);
        while *packet_size > 0 && pending.len() >= *packet_size {
            let rest = pending.split_off(*packet_size);
            sink(core::mem::replace(pending, rest));
        }
    }
}

struct Handler {
    formats: Vec<AudioFormat>,
    capture: Arc<Mutex<Capture>>,
    events: Sender<DesktopEvent>,
}

impl Handler {
    fn capture(&self) -> std::sync::MutexGuard<'_, Capture> {
        self.capture.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opening and closing are rare and must not be lost to a busy viewer, but this runs
    /// inside the session's PDU processing, which must not wait.
    fn notify(&self, event: DesktopEvent) {
        match self.events.try_send(event) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(event)) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let events = self.events.clone();
                    runtime.spawn(async move { events.send(event).await });
                }
            }
        }
    }

    fn start(&self, format: DesktopAudioFormat, packet_size: usize) {
        let frames_per_packet = packet_size / usize::from(format.block_align().max(1));
        info!(
            ?format,
            frames_per_packet, "The target records the microphone"
        );
        self.notify(DesktopEvent::MicrophoneOpen {
            format,
            frames_per_packet: u32::try_from(frames_per_packet).unwrap_or(u32::MAX),
        });
    }
}

impl RdpeaiCaptureHandler for Handler {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn open(
        &mut self,
        _capture_format: &AudioFormat,
        encode_format: &AudioFormat,
        packet_size: usize,
        sink: AudioPacketSink,
    ) -> i32 {
        let Some(format) = relayed_format(encode_format) else {
            warn!(format = ?encode_format, "The target asked for a microphone format that was not offered");
            return OpenReplyPdu::E_FAIL;
        };
        *self.capture() = Capture {
            sink: Some(sink),
            packet_size,
            pending: Vec::new(),
        };
        self.start(format, packet_size);
        OpenReplyPdu::S_OK
    }

    fn set_format(&mut self, encode_format: &AudioFormat, packet_size: usize) -> bool {
        let Some(format) = relayed_format(encode_format) else {
            return false;
        };
        {
            let mut capture = self.capture();
            capture.packet_size = packet_size;
            capture.pending.clear();
        }
        self.start(format, packet_size);
        true
    }

    fn close(&mut self) {
        let was_open = self.capture().sink.take().is_some();
        if was_open {
            debug!("The target stopped recording the microphone");
            self.notify(DesktopEvent::MicrophoneClose);
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{Decode as _, ReadCursor, encode_vec};
    use ironrdp_dvc::pdu::{
        CapabilitiesRequestPdu, CapsVersion, CreateRequestPdu, CreationStatus, DrdynvcClientPdu,
        DrdynvcDataPdu, DrdynvcServerPdu,
    };
    use ironrdp_dvc::{DrdynvcClient, DvcMessage, encode_dvc_messages};
    use ironrdp_rdpeai::pdu::{
        FormatChangePdu, FormatsPdu, OpenPdu, RdpeaiPdu, Version, VersionPdu,
    };
    use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
    use ironrdp_svc::{ChannelFlags, SvcMessage, SvcProcessor as _};
    use tokio::sync::mpsc;
    use warpgate_core::{DESKTOP_MICROPHONE_FORMATS, DesktopAudioFormat, DesktopEvent};

    use super::MicrophoneRelay;
    use crate::client::audio::pcm_format;

    const ID: u32 = 7;

    struct Target {
        drdynvc: DrdynvcClient,
    }

    impl Target {
        fn new(channel: ironrdp_rdpeai::client::RdpeaiClient) -> Self {
            let mut target = Self {
                drdynvc: DrdynvcClient::new().with_dynamic_channel(channel),
            };
            target.exchange(DrdynvcServerPdu::Capabilities(CapabilitiesRequestPdu::new(
                CapsVersion::V3,
                None,
            )));
            let created = target.exchange(DrdynvcServerPdu::Create(CreateRequestPdu::new(
                ID,
                ironrdp_rdpeai::CHANNEL_NAME.to_owned(),
            )));
            assert!(matches!(
                created.as_slice(),
                [DrdynvcClientPdu::Create(r)] if r.creation_status() == CreationStatus::OK
            ));
            target
        }

        fn exchange(&mut self, pdu: DrdynvcServerPdu) -> Vec<DrdynvcClientPdu> {
            decode_client(
                &self
                    .drdynvc
                    .process(&encode_vec(&pdu).expect("encodes"))
                    .expect("processes"),
            )
        }

        /// Sends `pdu` on AUDIO_INPUT and returns the PDUs Warpgate answers with on it.
        fn send(&mut self, pdu: RdpeaiPdu) -> Vec<RdpeaiPdu> {
            let message: DvcMessage = Box::new(pdu);
            let [svc] = encode_dvc_messages(ID, vec![message], ChannelFlags::empty())
                .expect("encodes")
                .try_into()
                .expect("one message");
            let bytes = svc.encode_unframed_pdu().expect("encodes");
            let pdu = DrdynvcServerPdu::decode(&mut ReadCursor::new(&bytes)).expect("decodes");
            audio_input(self.exchange(pdu))
        }
    }

    fn decode_client(messages: &[SvcMessage]) -> Vec<DrdynvcClientPdu> {
        messages
            .iter()
            .map(|m| {
                let bytes = m.encode_unframed_pdu().expect("encodes");
                DrdynvcClientPdu::decode(&mut ReadCursor::new(&bytes)).expect("decodes")
            })
            .collect()
    }

    fn audio_input(pdus: Vec<DrdynvcClientPdu>) -> Vec<RdpeaiPdu> {
        pdus.into_iter()
            .filter_map(|pdu| match pdu {
                DrdynvcClientPdu::Data(data) => {
                    assert_eq!(data.channel_id(), ID);
                    let DrdynvcDataPdu::Data(data) = data else {
                        panic!("AUDIO_INPUT PDUs here fit in one DVC PDU");
                    };
                    Some(ironrdp_core::decode(data.data()).expect("decodes"))
                }
                _ => None,
            })
            .collect()
    }

    fn mono_16k() -> DesktopAudioFormat {
        DesktopAudioFormat {
            channels: 1,
            samples_per_sec: 16_000,
            bits_per_sample: 16,
        }
    }

    /// Version and formats, with the target offering AAC first and mono 16 kHz PCM.
    fn negotiate(target: &mut Target) -> Vec<AudioFormat> {
        let replies = target.send(RdpeaiPdu::Version(VersionPdu::new(Version::V2)));
        assert!(matches!(replies.as_slice(), [RdpeaiPdu::Version(_)]));
        let mut aac = pcm_format(mono_16k());
        aac.format = WaveFormat::AAC_MS;
        let replies = target.send(RdpeaiPdu::Formats(FormatsPdu::server(vec![
            aac,
            pcm_format(mono_16k()),
        ])));
        let [RdpeaiPdu::DataIncoming, RdpeaiPdu::Formats(formats)] = replies.as_slice() else {
            panic!("expected the client's formats, got {replies:?}");
        };
        formats.formats.clone()
    }

    fn open(target: &mut Target, initial_format: u32, frames_per_packet: u32) -> Vec<RdpeaiPdu> {
        target.send(RdpeaiPdu::Open(OpenPdu {
            frames_per_packet,
            initial_format,
            capture_format: pcm_format(mono_16k()),
        }))
    }

    #[test]
    fn offers_the_target_only_the_relayed_pcm_formats_it_offered() {
        let (tx, _rx) = mpsc::channel(4);
        let relay = MicrophoneRelay::new(tx);
        let mut target = Target::new(relay.channel);
        assert_eq!(negotiate(&mut target), vec![pcm_format(mono_16k())]);
    }

    #[test]
    fn every_relayed_format_is_16_bit_pcm() {
        for format in DESKTOP_MICROPHONE_FORMATS {
            let wire = pcm_format(format);
            assert_eq!(wire.format, WaveFormat::PCM);
            assert_eq!(wire.bits_per_sample, 16);
            assert_eq!(wire.n_block_align, format.channels * 2);
        }
    }

    #[test]
    fn opening_capture_asks_the_viewer_and_relays_its_audio_in_target_sized_packets() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut relay = MicrophoneRelay::new(tx);
        let mut target = Target::new(relay.channel);
        negotiate(&mut target);

        let replies = open(&mut target, 0, 4);
        assert!(matches!(
            replies.as_slice(),
            [RdpeaiPdu::FormatChange(FormatChangePdu { new_format: 0 }), RdpeaiPdu::OpenReply(r)]
                if r.result == 0
        ));
        let Ok(DesktopEvent::MicrophoneOpen {
            format,
            frames_per_packet,
        }) = rx.try_recv()
        else {
            panic!("expected the viewer to be asked");
        };
        assert_eq!(format, mono_16k());
        assert_eq!(frames_per_packet, 4);

        // 4 frames of mono 16-bit: 8-byte packets, whatever the viewer's block sizes.
        relay.input.push(&[1, 2, 3, 4, 5]);
        relay.input.push(&[6, 7, 8, 9, 10, 11]);
        let mut sent = Vec::new();
        while let Ok(messages) = relay.uplink.try_recv() {
            sent.extend(audio_input(decode_client(&messages)));
        }
        let data: Vec<_> = sent
            .into_iter()
            .filter_map(|pdu| match pdu {
                RdpeaiPdu::Data(data) => Some(data.data),
                RdpeaiPdu::DataIncoming => None,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(data, vec![vec![1, 2, 3, 4, 5, 6, 7, 8]]);
    }

    #[test]
    fn audio_before_the_target_records_is_dropped() {
        let (tx, _rx) = mpsc::channel(4);
        let mut relay = MicrophoneRelay::new(tx);
        let mut target = Target::new(relay.channel);
        negotiate(&mut target);
        relay.input.push(&[0; 64]);
        assert!(relay.uplink.try_recv().is_err());
    }

    #[test]
    fn closing_the_channel_tells_the_viewer_once() {
        let (tx, mut rx) = mpsc::channel(4);
        let relay = MicrophoneRelay::new(tx);
        let mut target = Target::new(relay.channel);
        negotiate(&mut target);
        open(&mut target, 0, 4);
        assert!(matches!(
            rx.try_recv(),
            Ok(DesktopEvent::MicrophoneOpen { .. })
        ));

        let closed = target.exchange(DrdynvcServerPdu::Close(ironrdp_dvc::pdu::ClosePdu::new(ID)));
        assert!(!closed.is_empty());
        assert!(matches!(rx.try_recv(), Ok(DesktopEvent::MicrophoneClose)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_format_it_did_not_offer_fails_the_open() {
        let (tx, mut rx) = mpsc::channel(4);
        let relay = MicrophoneRelay::new(tx);
        let mut target = Target::new(relay.channel);
        negotiate(&mut target);
        // Index 1 is past the one negotiated format.
        let replies = open(&mut target, 1, 4);
        assert!(matches!(
            replies.as_slice(),
            [RdpeaiPdu::OpenReply(r)] if r.result < 0
        ));
        assert!(rx.try_recv().is_err());
    }
}
