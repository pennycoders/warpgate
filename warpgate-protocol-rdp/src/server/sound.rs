//! The target's sound for the viewer (MS-RDPEA), relayed unchanged, or at half the rate to
//! viewers that cannot play the target's (Jump Desktop on macOS).

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
use warpgate_core::{DESKTOP_AUDIO_FORMAT, DesktopAudioFormat};

use super::halve::Halver;
use crate::client::pcm_format;

const HALF_RATE: DesktopAudioFormat = DesktopAudioFormat {
    samples_per_sec: DESKTOP_AUDIO_FORMAT.samples_per_sec / 2,
    ..DESKTOP_AUDIO_FORMAT
};

#[derive(Debug, Default)]
struct Shared {
    server: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// A viewer may negotiate both the rdpsnd static channel and AUDIO_PLAYBACK_DVC.
    streaming: AtomicUsize,
    halver: Mutex<Option<Halver>>,
}

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

    pub(super) fn play(&self, data: &[u8]) {
        // The server ends the session on a block sent before a format is agreed.
        if self.shared.streaming.load(Ordering::Acquire) == 0 {
            return;
        }
        let ts = self.started.elapsed().as_millis() as u32;
        let data = match self
            .shared
            .halver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            Some(halver) => halver.process(data),
            None => data.to_vec(),
        };
        if let Some(server) = self
            .shared
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = server.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
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
            formats: [pcm_format(DESKTOP_AUDIO_FORMAT), pcm_format(HALF_RATE)],
            shared: Arc::clone(&self.shared),
            started: false,
        })
    }
}

#[derive(Debug)]
struct Backend {
    formats: [AudioFormat; 2],
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
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        debug!(format = ?format.format(), "Viewer accepted audio");
        *self
            .shared
            .halver
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            (*format.format() == pcm_format(HALF_RATE)).then(Halver::new);
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
            vec![
                pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT),
                pcm_format(super::HALF_RATE)
            ]
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
        assert!(server.wave(played[0].0.clone(), played[0].1).is_ok());
    }

    #[test]
    fn the_targets_rate_is_preferred_and_passed_through() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        negotiate(
            &mut server,
            vec![
                pcm_format(super::HALF_RATE),
                pcm_format(warpgate_core::DESKTOP_AUDIO_FORMAT),
            ],
        );
        let block: Vec<u8> = (0..64).collect();
        sound.play(&block);
        assert_eq!(waves(&mut rx)[0].0, block);
    }

    #[test]
    fn a_viewer_that_takes_only_half_the_rate_gets_it_resampled() {
        let (sound, mut rx) = viewer();
        let mut server = RdpsndServer::new(sound.build_backend());
        negotiate(&mut server, vec![pcm_format(super::HALF_RATE)]);
        sound.play(&[0; 4 * 100]);
        let played = waves(&mut rx);
        assert_eq!(played[0].0.len(), 4 * 50);
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
