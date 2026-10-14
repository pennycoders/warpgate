//! Viewer-side audio output (MS-RDPEA): play the target's sound on the viewer.
//!
//! The viewer is offered only [`DESKTOP_AUDIO_FORMAT`], the format the target is asked for,
//! so blocks pass through unchanged. Blocks are held back until the viewer has accepted
//! that format: the server rejects a block sent earlier, and ends the session with it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ironrdp::rdpsnd::pdu::AudioFormat;
use ironrdp::rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use warpgate_core::DESKTOP_AUDIO_FORMAT;

use crate::client::pcm_format;

#[derive(Debug, Default)]
struct Shared {
    server: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// Channels on which the viewer accepted the format: the rdpsnd static channel and
    /// AUDIO_PLAYBACK_DVC are both offered, and a viewer may negotiate either or both.
    streaming: AtomicUsize,
}

/// Hands the target's sound to the viewer-facing server.
#[derive(Debug, Clone)]
pub(super) struct ViewerSound {
    shared: Arc<Shared>,
    started: Instant,
}

impl ViewerSound {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::default(),
            started: Instant::now(),
        }
    }

    /// Play one block of PCM. Dropped until the viewer has accepted the format, and when
    /// it never will (it declined sound, or did not open the channel at all).
    pub(super) fn play(&self, data: &[u8]) {
        if self.shared.streaming.load(Ordering::Acquire) == 0 {
            return;
        }
        // Milliseconds since the session began; the viewer uses it only to pace playback,
        // so wrapping after ~49 days is harmless.
        let ts = self.started.elapsed().as_millis() as u32;
        if let Some(server) = self
            .shared
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = server.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                data.to_vec(),
                ts,
            )));
        }
    }
}

impl ServerEventSender for ViewerSound {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self
            .shared
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }
}

impl SoundServerFactory for ViewerSound {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Backend {
            formats: diagnostic_formats(),
            shared: Arc::clone(&self.shared),
            started: false,
        })
    }
}

/// Lab diagnostics (not for upstream): a broad menu, so the viewer's reply shows what it
/// can play. Only PCM in DESKTOP_AUDIO_FORMAT is ever streamed.
fn diagnostic_formats() -> Vec<AudioFormat> {
    use ironrdp::rdpsnd::pdu::WaveFormat;
    let fmt = |format, n_channels: u16, rate: u32, bits: u16| AudioFormat {
        format,
        n_channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(n_channels) * u32::from(bits / 8),
        n_block_align: n_channels * (bits / 8),
        bits_per_sample: bits,
        data: None,
    };
    let mut formats = vec![pcm_format(DESKTOP_AUDIO_FORMAT)];
    for rate in [48_000, 44_100, 22_050, 11_025, 8_000] {
        for channels in [2, 1] {
            let f = fmt(WaveFormat::PCM, channels, rate, 16);
            if !formats.contains(&f) {
                formats.push(f);
            }
        }
    }
    for rate in [44_100, 22_050, 8_000] {
        formats.push(fmt(WaveFormat::ALAW, 2, rate, 8));
        formats.push(fmt(WaveFormat::MULAW, 2, rate, 8));
    }
    let mut aac = fmt(WaveFormat::AAC_MS, 2, 44_100, 16);
    aac.n_avg_bytes_per_sec = 12_000;
    aac.n_block_align = 4;
    formats.push(aac);
    let mut opus = fmt(WaveFormat::OPUS, 2, 48_000, 16);
    opus.n_avg_bytes_per_sec = 12_000;
    formats.push(opus);
    formats
}

#[derive(Debug)]
struct Backend {
    formats: Vec<AudioFormat>,
    shared: Arc<Shared>,
    started: bool,
}

impl RdpsndServerHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // Only the format the target sends is streamed.
        let pcm = pcm_format(DESKTOP_AUDIO_FORMAT);
        common.iter().find(|f| *f.format() == pcm)
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        debug!(format = ?format.format(), "Viewer accepted audio");
        if !self.started {
            self.started = true;
            self.shared.streaming.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    fn stop(&mut self) {
        if self.started {
            self.started = false;
            self.shared.streaming.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp::core::{Decode as _, ReadCursor, encode_vec};
    use ironrdp::rdpsnd::pdu::{
        AudioFormatFlags, ClientAudioFormatPdu, ClientAudioOutputPdu, QualityMode, QualityModePdu,
        ServerAudioOutputPdu, TrainingConfirmPdu, Version, WaveFormat,
    };
    use ironrdp::rdpsnd::server::RdpsndServer;
    use ironrdp::svc::SvcProcessor as _;
    use ironrdp_server::{
        RdpsndServerMessage, ServerEvent, ServerEventSender as _, SoundServerFactory as _,
    };
    use tokio::sync::mpsc;

    use super::ViewerSound;
    use crate::client::pcm_format;

    fn encode(pdu: ClientAudioOutputPdu) -> Vec<u8> {
        encode_vec(&pdu).expect("encodes")
    }

    /// Drive a real server channel through the viewer's side of the negotiation.
    fn negotiate(
        server: &mut RdpsndServer,
        viewer_formats: Vec<ironrdp::rdpsnd::pdu::AudioFormat>,
    ) {
        let offer = server.start().expect("start");
        let offer = offer[0].encode_unframed_pdu().expect("encodes");
        let ServerAudioOutputPdu::AudioFormat(offer) =
            ServerAudioOutputPdu::decode(&mut ReadCursor::new(&offer)).expect("decodes")
        else {
            panic!("expected the server's formats first");
        };
        assert_eq!(
            offer.formats,
            vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)]
        );
        server
            .process(&encode(ClientAudioOutputPdu::AudioFormat(
                ClientAudioFormatPdu {
                    version: Version::V8,
                    flags: AudioFormatFlags::ALIVE,
                    formats: viewer_formats,
                    volume_left: 0xFFFF,
                    volume_right: 0xFFFF,
                    pitch: 0x0001_0000,
                    dgram_port: 0,
                },
            )))
            .expect("formats");
        server
            .process(&encode(ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            })))
            .expect("quality mode");
        server
            .process(&encode(ClientAudioOutputPdu::TrainingConfirm(
                TrainingConfirmPdu {
                    timestamp: 0,
                    pack_size: 0,
                },
            )))
            .expect("training");
    }

    fn waves(rx: &mut mpsc::UnboundedReceiver<ServerEvent>) -> Vec<(Vec<u8>, u32)> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)) = event {
                out.push((data, ts));
            }
        }
        out
    }

    fn viewer() -> (ViewerSound, mpsc::UnboundedReceiver<ServerEvent>) {
        let mut sound = ViewerSound::new();
        let (tx, rx) = mpsc::unbounded_channel();
        sound.set_sender(tx);
        (sound, rx)
    }

    #[test]
    fn plays_once_the_viewer_accepts_pcm() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        let pcm = pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT);
        let mut other = pcm.clone();
        other.format = WaveFormat::OPUS;
        negotiate(&mut server, vec![other, pcm]);

        sound.play(&[1, 2, 3, 4]);
        let played = waves(&mut rx);
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].0, vec![1, 2, 3, 4]);
        // The server can now encode it: the viewer's format index is known.
        assert!(server.wave(played[0].0.clone(), played[0].1).is_ok());
    }

    #[test]
    fn holds_sound_back_until_the_viewer_has_negotiated() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        sound.play(&[0; 4]);
        assert!(waves(&mut rx).is_empty());
        let _ = server.start();
        sound.play(&[0; 4]);
        assert!(waves(&mut rx).is_empty());
    }

    #[test]
    fn plays_nothing_to_a_viewer_without_pcm() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        let mut opus = pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT);
        opus.format = WaveFormat::OPUS;
        negotiate(&mut server, vec![opus]);
        sound.play(&[0; 4]);
        assert!(waves(&mut rx).is_empty());
    }

    #[test]
    fn stops_when_the_channel_closes() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        negotiate(
            &mut server,
            vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)],
        );
        drop(server);
        sound.play(&[0; 4]);
        assert!(waves(&mut rx).is_empty());
    }

    /// The static channel and AUDIO_PLAYBACK_DVC each get a backend; a viewer that negotiates
    /// both keeps its sound until both are gone.
    #[test]
    fn sound_lasts_while_any_channel_is_negotiated() {
        let (sound, mut rx) = viewer();
        let mut on_static = RdpsndServer::new(sound.build_backend());
        let mut on_dynamic = RdpsndServer::new(sound.build_backend());
        negotiate(
            &mut on_static,
            vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)],
        );
        negotiate(
            &mut on_dynamic,
            vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)],
        );

        drop(on_static);
        sound.play(&[1; 4]);
        assert_eq!(waves(&mut rx).len(), 1);

        drop(on_dynamic);
        sound.play(&[2; 4]);
        assert!(waves(&mut rx).is_empty());
    }

    /// A viewer that only opens AUDIO_PLAYBACK_DVC (Jump Desktop) negotiates through the
    /// dynamic-channel wrapper and gets sound.
    #[test]
    fn a_viewer_on_the_dynamic_channel_gets_sound() {
        use ironrdp::dvc::{DrdynvcClient, DrdynvcServer};
        use ironrdp::rdpsnd::client::Rdpsnd;
        use ironrdp::rdpsnd::dvc::{RdpsndDvcClient, RdpsndDvcServer};

        #[derive(Debug)]
        struct PcmOnly(Vec<ironrdp::rdpsnd::pdu::AudioFormat>);
        impl ironrdp::rdpsnd::client::RdpsndClientHandler for PcmOnly {
            fn get_formats(&self) -> &[ironrdp::rdpsnd::pdu::AudioFormat] {
                &self.0
            }
            fn wave(&mut self, _: usize, _: u32, _: std::borrow::Cow<'_, [u8]>) {}
            fn set_volume(&mut self, _: ironrdp::rdpsnd::pdu::VolumePdu) {}
            fn set_pitch(&mut self, _: ironrdp::rdpsnd::pdu::PitchPdu) {}
            fn close(&mut self) {}
        }

        let (sound, mut rx) = viewer();
        let mut server = DrdynvcServer::new().with_dynamic_channel(RdpsndDvcServer::new(
            RdpsndServer::new(sound.build_backend()),
        ));
        let mut client =
            DrdynvcClient::new().with_dynamic_channel(RdpsndDvcClient::new(Rdpsnd::new(Box::new(
                PcmOnly(vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)]),
            ))));
        let mut to_client = server.start().unwrap();
        for _ in 0..16 {
            if to_client.is_empty() {
                break;
            }
            let mut to_server = Vec::new();
            for message in to_client.drain(..) {
                to_server.extend(
                    client
                        .process(&message.encode_unframed_pdu().unwrap())
                        .unwrap(),
                );
            }
            for message in to_server {
                to_client.extend(
                    server
                        .process(&message.encode_unframed_pdu().unwrap())
                        .unwrap(),
                );
            }
        }

        sound.play(&[3; 4]);
        assert_eq!(waves(&mut rx).len(), 1);
    }

    #[test]
    fn timestamps_do_not_go_backwards() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        negotiate(
            &mut server,
            vec![pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT)],
        );
        sound.play(&[0; 4]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        sound.play(&[0; 4]);
        let played = waves(&mut rx);
        assert_eq!(played.len(), 2);
        assert!(played[1].1 >= played[0].1 + 5);
    }

    #[test]
    fn plays_nothing_before_the_server_is_running() {
        let sound = ViewerSound::new();
        // No sender yet and nothing negotiated: must not panic.
        sound.play(&[0; 4]);
    }

    #[test]
    fn encodes_wave_pdus_for_the_negotiated_index() {
        let (sound, _rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        let pcm = pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT);
        let mut opus = pcm.clone();
        opus.format = WaveFormat::OPUS;
        negotiate(&mut server, vec![opus, pcm]);
        let msgs = server.wave(vec![9; 4], 0).expect("wave");
        let messages: Vec<_> = msgs.into();
        let bytes = messages[0].encode_unframed_pdu().expect("encodes");
        let ServerAudioOutputPdu::Wave2(wave) =
            ServerAudioOutputPdu::decode(&mut ReadCursor::new(&bytes)).expect("decodes")
        else {
            panic!("expected Wave2 at version 8");
        };
        // wFormatNo indexes the viewer's list, where PCM was second.
        assert_eq!(wave.format_no, 1);
        assert_eq!(&wave.data[..], &[9; 4]);
    }
}
