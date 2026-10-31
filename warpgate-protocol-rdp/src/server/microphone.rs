//! The viewer's microphone (MS-RDPEAI) for the target, relayed unchanged.
//!
//! The viewer is offered the same PCM formats as the target ([`DESKTOP_MICROPHONE_FORMATS`]).
//! Recording starts only once the target opens capture: the viewer is then asked for the
//! target's format, and what it records goes to the target as it comes.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_server::{
    RdpeaiServerBackend, RdpeaiServerFactory, RdpeaiServerMessage, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use warpgate_core::{DESKTOP_MICROPHONE_FORMATS, DesktopAudioFormat, DesktopInput};

use super::protocol::Event;
use crate::client::pcm_format;

#[derive(Debug, Default)]
struct State {
    /// The viewer's formats, in its order; Open and Format Change index them.
    negotiated: Vec<AudioFormat>,
    /// The negotiated format the viewer was asked to record in.
    recording: Option<u32>,
    /// What the target records in, while it records.
    wanted: Option<(DesktopAudioFormat, u32)>,
}

#[derive(Debug, Default)]
struct Shared {
    server: Mutex<Option<UnboundedSender<ServerEvent>>>,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, message: RdpeaiServerMessage) {
        if let Some(server) = self
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = server.send(ServerEvent::Rdpeai(message));
        }
    }

    /// Asks the viewer to record in the target's format, once both are known.
    fn sync(&self) {
        let message = {
            let mut state = self.state();
            let Some((format, frames_per_packet)) = state.wanted else {
                return;
            };
            let wire = pcm_format(format);
            let Some(index) = state
                .negotiated
                .iter()
                .position(|offered| offered.matches_for_negotiation(&wire))
                .and_then(|index| u32::try_from(index).ok())
            else {
                if !state.negotiated.is_empty() {
                    warn!(?format, "The viewer cannot record in the target's format");
                }
                return;
            };
            match state.recording.replace(index) {
                None => RdpeaiServerMessage::Open {
                    frames_per_packet,
                    initial_format: index,
                    capture_format: wire,
                },
                Some(current) if current != index => {
                    RdpeaiServerMessage::ChangeFormat { new_format: index }
                }
                Some(_) => return,
            }
        };
        self.send(message);
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ViewerMicrophone {
    shared: Arc<Shared>,
    out: Option<UnboundedSender<Event>>,
}

impl ViewerMicrophone {
    pub(super) fn new(out: UnboundedSender<Event>) -> Self {
        Self {
            shared: Arc::default(),
            out: Some(out),
        }
    }

    /// The target started recording, or switched format.
    pub(super) fn open(&self, format: DesktopAudioFormat, frames_per_packet: u32) {
        self.shared.state().wanted = Some((format, frames_per_packet));
        self.shared.sync();
    }

    /// The target stopped recording. MS-RDPEAI has no way to stop a client short of closing
    /// the channel, so the viewer may keep recording; what it sends is dropped.
    pub(super) fn close(&self) {
        self.shared.state().wanted = None;
    }
}

impl ServerEventSender for ViewerMicrophone {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self
            .shared
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }
}

impl RdpeaiServerFactory for ViewerMicrophone {
    fn build_backend(&self) -> Box<dyn RdpeaiServerBackend> {
        Box::new(Backend {
            formats: DESKTOP_MICROPHONE_FORMATS.map(pcm_format).to_vec(),
            shared: Arc::clone(&self.shared),
            out: self.out.clone(),
        })
    }
}

struct Backend {
    formats: Vec<AudioFormat>,
    shared: Arc<Shared>,
    out: Option<UnboundedSender<Event>>,
}

impl RdpeaiServerBackend for Backend {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn on_formats_negotiated(&mut self, negotiated: &[AudioFormat]) {
        debug!(count = negotiated.len(), "The viewer can record");
        {
            let mut state = self.shared.state();
            state.negotiated = negotiated.to_vec();
            state.recording = None;
        }
        self.shared.sync();
    }

    fn on_open_reply(&mut self, result: i32) {
        if result < 0 {
            warn!(result, "The viewer could not open its microphone");
            self.shared.state().recording = None;
        } else {
            info!("The viewer records its microphone");
        }
    }

    fn on_audio_data(&mut self, _format: &AudioFormat, data: &[u8]) {
        if self.shared.state().wanted.is_none() {
            return;
        }
        if let Some(out) = &self.out {
            let _ = out.send(Event::Input(DesktopInput::Microphone(
                Bytes::copy_from_slice(data),
            )));
        }
    }

    fn on_close(&mut self) {
        let mut state = self.shared.state();
        state.negotiated.clear();
        state.recording = None;
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::encode_vec;
    use ironrdp_dvc::{DvcMessage, DvcProcessor as _};
    use ironrdp_rdpeai::pdu::{
        DataPdu, FormatChangePdu, FormatsPdu, OpenReplyPdu, RdpeaiPdu, Version, VersionPdu,
    };
    use ironrdp_rdpeai::server::RdpeaiServer;
    use ironrdp_rdpsnd::pdu::AudioFormat;
    use ironrdp_server::{
        RdpeaiServerFactory as _, RdpeaiServerMessage, ServerEvent, ServerEventSender as _,
    };
    use tokio::sync::mpsc;
    use warpgate_core::{DESKTOP_MICROPHONE_FORMATS, DesktopAudioFormat, DesktopInput};

    use super::ViewerMicrophone;
    use crate::client::pcm_format;
    use crate::server::protocol::Event;

    struct Viewer {
        server: RdpeaiServer,
        microphone: ViewerMicrophone,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        out: mpsc::UnboundedReceiver<Event>,
    }

    fn pdus(messages: Vec<DvcMessage>) -> Vec<RdpeaiPdu> {
        messages
            .iter()
            .map(|message| {
                let mut buf = vec![0; message.size()];
                message
                    .encode(&mut ironrdp_core::WriteCursor::new(&mut buf))
                    .expect("encodes");
                ironrdp_core::decode(&buf).expect("decodes")
            })
            .collect()
    }

    impl Viewer {
        fn new() -> Self {
            let (out_tx, out) = mpsc::unbounded_channel();
            let (events_tx, events) = mpsc::unbounded_channel();
            let mut microphone = ViewerMicrophone::new(out_tx);
            microphone.set_sender(events_tx);
            let server = RdpeaiServer::new(microphone.build_backend());
            Self {
                server,
                microphone,
                events,
                out,
            }
        }

        fn receive(&mut self, pdu: RdpeaiPdu) -> Vec<RdpeaiPdu> {
            pdus(
                self.server
                    .process(0, &encode_vec(&pdu).expect("encodes"))
                    .expect("processes"),
            )
        }

        /// Version and formats; the viewer takes `formats`.
        fn negotiate(&mut self, formats: Vec<AudioFormat>) {
            let started = pdus(self.server.start(0).expect("starts"));
            assert!(matches!(started.as_slice(), [RdpeaiPdu::Version(_)]));
            let offer = self.receive(RdpeaiPdu::Version(VersionPdu::new(Version::V2)));
            let [RdpeaiPdu::Formats(offer)] = offer.as_slice() else {
                panic!("expected the server's formats");
            };
            assert_eq!(
                offer.formats,
                DESKTOP_MICROPHONE_FORMATS.map(pcm_format).to_vec()
            );
            self.receive(RdpeaiPdu::Formats(FormatsPdu::client(formats)));
        }

        /// Applies the server events the way the RDP server loop does, returning the PDUs
        /// they send.
        fn drive(&mut self) -> Vec<RdpeaiPdu> {
            let mut sent = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                let ServerEvent::Rdpeai(message) = event else {
                    continue;
                };
                let messages = match message {
                    RdpeaiServerMessage::Open {
                        frames_per_packet,
                        initial_format,
                        capture_format,
                    } => self
                        .server
                        .open(frames_per_packet, initial_format, capture_format),
                    RdpeaiServerMessage::ChangeFormat { new_format } => {
                        self.server.change_format(new_format)
                    }
                    RdpeaiServerMessage::Error(_) => continue,
                };
                sent.extend(pdus(messages.expect("valid in this state")));
            }
            sent
        }

        fn microphone_input(&mut self) -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            while let Ok(event) = self.out.try_recv() {
                if let Event::Input(DesktopInput::Microphone(data)) = event {
                    out.push(data.to_vec());
                }
            }
            out
        }
    }

    const STEREO_44: DesktopAudioFormat = DESKTOP_MICROPHONE_FORMATS[0];
    const MONO_16: DesktopAudioFormat = DESKTOP_MICROPHONE_FORMATS[6];

    #[test]
    fn the_viewer_records_only_once_the_target_does() {
        let mut viewer = Viewer::new();
        viewer.negotiate(vec![pcm_format(MONO_16), pcm_format(STEREO_44)]);
        assert!(viewer.drive().is_empty());

        viewer.microphone.open(STEREO_44, 441);
        let sent = viewer.drive();
        let [RdpeaiPdu::Open(open)] = sent.as_slice() else {
            panic!("expected an Open, got {sent:?}");
        };
        // The viewer's list indexes the format: stereo 44.1 kHz is its second.
        assert_eq!(open.initial_format, 1);
        assert_eq!(open.frames_per_packet, 441);
        assert_eq!(open.capture_format, pcm_format(STEREO_44));
    }

    #[test]
    fn the_target_opening_first_waits_for_the_viewer() {
        let mut viewer = Viewer::new();
        viewer.microphone.open(MONO_16, 160);
        assert!(viewer.drive().is_empty());
        viewer.negotiate(vec![pcm_format(MONO_16)]);
        assert!(matches!(viewer.drive().as_slice(), [RdpeaiPdu::Open(_)]));
    }

    #[test]
    fn audio_reaches_the_target_while_it_records() {
        let mut viewer = Viewer::new();
        viewer.negotiate(vec![pcm_format(MONO_16)]);
        viewer.microphone.open(MONO_16, 160);
        viewer.drive();
        viewer.receive(RdpeaiPdu::FormatChange(FormatChangePdu::new(0)));
        viewer.receive(RdpeaiPdu::OpenReply(OpenReplyPdu::ok()));

        viewer.receive(RdpeaiPdu::DataIncoming);
        viewer.receive(RdpeaiPdu::Data(DataPdu::new(vec![1, 2, 3, 4])));
        assert_eq!(viewer.microphone_input(), vec![vec![1, 2, 3, 4]]);

        viewer.microphone.close();
        viewer.receive(RdpeaiPdu::Data(DataPdu::new(vec![5, 6])));
        assert!(viewer.microphone_input().is_empty());
    }

    #[test]
    fn a_new_format_from_the_target_changes_the_viewers() {
        let mut viewer = Viewer::new();
        viewer.negotiate(vec![pcm_format(MONO_16), pcm_format(STEREO_44)]);
        viewer.microphone.open(MONO_16, 160);
        viewer.drive();
        viewer.receive(RdpeaiPdu::FormatChange(FormatChangePdu::new(0)));
        viewer.receive(RdpeaiPdu::OpenReply(OpenReplyPdu::ok()));

        // The target closes and reopens in the same format: nothing to tell the viewer.
        viewer.microphone.close();
        viewer.microphone.open(MONO_16, 160);
        assert!(viewer.drive().is_empty());

        viewer.microphone.open(STEREO_44, 441);
        assert!(matches!(
            viewer.drive().as_slice(),
            [RdpeaiPdu::FormatChange(FormatChangePdu { new_format: 1 })]
        ));
    }

    #[test]
    fn a_viewer_without_the_targets_format_is_not_asked() {
        let mut viewer = Viewer::new();
        viewer.negotiate(vec![pcm_format(MONO_16)]);
        viewer.microphone.open(STEREO_44, 441);
        assert!(viewer.drive().is_empty());
    }

    #[test]
    fn a_failed_open_is_retried_when_the_target_opens_again() {
        let mut viewer = Viewer::new();
        viewer.negotiate(vec![pcm_format(MONO_16)]);
        viewer.microphone.open(MONO_16, 160);
        viewer.drive();
        viewer.receive(RdpeaiPdu::OpenReply(OpenReplyPdu::fail()));
        viewer.microphone.open(MONO_16, 160);
        assert!(matches!(viewer.drive().as_slice(), [RdpeaiPdu::Open(_)]));
    }
}
