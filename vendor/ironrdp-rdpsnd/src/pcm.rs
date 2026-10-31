//! Sample-rate reduction for 16-bit PCM.
//!
//! A server produces audio at one rate, but some clients accept only lower ones (Jump Desktop on macOS takes only 22.05 kHz).
//! The server can offer [`PcmDecimator::output_format`] next to its own format, and run the stream through a [`PcmDecimator`] for clients that pick it.

use crate::pdu::{AudioFormat, WaveFormat};

/// Half-band low-pass filter taps (Hamming-windowed sinc, 15 taps) in Q15.
///
/// They sum to `1 << 15`, so a constant signal passes unchanged.
const HALF_BAND: [i32; 15] = [-120, 0, 530, 0, -2242, 0, 9993, 16446, 9993, 0, -2242, 0, 530, 0, -120];

/// Converts 16-bit PCM to `1 / factor` of its sample rate.
///
/// Each halving is a half-band low-pass filter followed by keeping every second frame, so that frequencies the lower rate cannot carry are removed instead of folding back.
/// The filter state carries over between calls to [`PcmDecimator::process`], so a stream gives the same output whatever its block sizes.
#[derive(Debug, Clone)]
pub struct PcmDecimator {
    /// INVARIANT: `1 <= channels`
    channels: usize,
    /// One stage per halving.
    stages: Vec<HalfBand>,
    samples: Vec<i16>,
    scratch: Vec<i16>,
}

impl PcmDecimator {
    /// The factors a sample rate can be divided by.
    pub const FACTORS: [u32; 2] = [2, 4];

    /// `format` at `1 / factor` of its sample rate.
    ///
    /// Returns `None` unless `format` is 16-bit PCM, `factor` is one of [`PcmDecimator::FACTORS`] and the rate divides by it.
    pub fn output_format(format: &AudioFormat, factor: u32) -> Option<AudioFormat> {
        if !(format.format == WaveFormat::PCM && format.bits_per_sample == 16 && Self::FACTORS.contains(&factor)) {
            return None;
        }

        if !format.n_samples_per_sec.is_multiple_of(factor) {
            return None;
        }

        let n_samples_per_sec = format.n_samples_per_sec / factor;

        Some(AudioFormat {
            format: WaveFormat::PCM,
            n_channels: format.n_channels,
            n_samples_per_sec,
            n_avg_bytes_per_sec: n_samples_per_sec.checked_mul(u32::from(format.n_block_align))?,
            n_block_align: format.n_block_align,
            bits_per_sample: 16,
            data: None,
        })
    }

    /// A decimator from `from` to `to`.
    ///
    /// Returns `None` unless `to` is [`PcmDecimator::output_format`] of `from` for one of [`PcmDecimator::FACTORS`].
    pub fn new(from: &AudioFormat, to: &AudioFormat) -> Option<Self> {
        let factor = Self::FACTORS.into_iter().find(|&factor| {
            Self::output_format(from, factor).is_some_and(|format| format.matches_for_negotiation(to))
        })?;

        if from.n_channels == 0 {
            return None;
        }
        let channels = usize::from(from.n_channels);

        let stage_count = usize::try_from(factor.trailing_zeros()).ok()?;
        let stages = vec![HalfBand::new(channels); stage_count];

        Some(Self {
            channels,
            stages,
            samples: Vec::new(),
            scratch: Vec::new(),
        })
    }

    /// Converts the whole frames of `input` (interleaved little-endian samples) and appends them to `output`.
    ///
    /// A trailing partial frame is ignored.
    pub fn process(&mut self, input: &[u8], output: &mut Vec<u8>) {
        let Self {
            channels,
            stages,
            samples,
            scratch,
        } = self;

        let frame_bytes = 2 * *channels;
        let whole_frames = &input[..input.len() - input.len() % frame_bytes];

        samples.clear();
        samples.extend(
            whole_frames
                .chunks_exact(2)
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]])),
        );

        for stage in stages {
            scratch.clear();
            stage.process(*channels, samples, scratch);
            core::mem::swap(samples, scratch);
        }

        output.reserve(samples.len() * 2);
        for sample in samples.iter() {
            output.extend_from_slice(&sample.to_le_bytes());
        }
    }
}

/// One halving: [`HALF_BAND`] low-pass, then every second frame.
#[derive(Debug, Clone)]
struct HalfBand {
    /// The last `HALF_BAND.len()` input frames, oldest first.
    history: Vec<i16>,
    /// Whether the next input frame is kept.
    keep_next: bool,
}

impl HalfBand {
    fn new(channels: usize) -> Self {
        Self {
            history: vec![0; HALF_BAND.len() * channels],
            keep_next: true,
        }
    }

    fn process(&mut self, channels: usize, input: &[i16], output: &mut Vec<i16>) {
        output.reserve(input.len() / 2 + channels);

        for frame in input.chunks_exact(channels) {
            self.history.copy_within(channels.., 0);
            let newest = self.history.len() - channels;
            self.history[newest..].copy_from_slice(frame);

            let keep = self.keep_next;
            self.keep_next = !keep;
            if !keep {
                continue;
            }

            for channel in 0..channels {
                // Per invariants: the taps' magnitudes sum to 42 216 and |sample| <= 32 768, so |acc| < 2^31.
                let acc: i32 = HALF_BAND
                    .iter()
                    .zip(self.history.chunks_exact(channels))
                    .map(|(tap, frame)| tap * i32::from(frame[channel]))
                    .sum();

                // The filter overshoots a full-scale input slightly, so the result saturates.
                let sample = (acc + (1 << 14)) >> 15;
                output.push(i16::try_from(sample).unwrap_or(if sample < 0 { i16::MIN } else { i16::MAX }));
            }
        }
    }
}
