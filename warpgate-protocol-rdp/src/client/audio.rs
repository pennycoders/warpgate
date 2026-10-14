//! Target-side audio output (MS-RDPEA): accept the target's sound as PCM in
//! [`DESKTOP_AUDIO_FORMAT`] and hand each block to the session as a [`DesktopEvent::Audio`].

use std::borrow::Cow;

use bytes::Bytes;
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};
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

#[derive(Debug)]
pub struct AudioForwarder {
    formats: [AudioFormat; 1],
    events: Sender<DesktopEvent>,
    dropped: u64,
}

impl AudioForwarder {
    pub const fn new(events: Sender<DesktopEvent>) -> Self {
        Self {
            formats: [pcm_format(DESKTOP_AUDIO_FORMAT)],
            events,
            dropped: 0,
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

    use ironrdp::rdpsnd::client::RdpsndClientHandler as _;
    use ironrdp::rdpsnd::pdu::WaveFormat;
    use tokio::sync::mpsc;
    use warpgate_core::DesktopEvent;

    use super::AudioForwarder;

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

    #[test]
    fn a_closed_session_is_not_an_error() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut forwarder = AudioForwarder::new(tx);
        forwarder.wave(0, 0, Cow::Borrowed(&[0; 4]));
        assert_eq!(forwarder.dropped, 0);
    }
}
