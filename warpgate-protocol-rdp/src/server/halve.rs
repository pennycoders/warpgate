//! Halves the sample rate of 16-bit stereo PCM, for viewers that cannot play the target's
//! rate (Jump Desktop on macOS accepts only 22.05 kHz): a 15-tap half-band low-pass
//! (Hamming-windowed sinc), then every second frame. The state carries over between blocks.

/// Q15; sums to 1.0.
const TAPS: [i32; 15] = [
    -120, 0, 530, 0, -2242, 0, 9993, 16446, 9993, 0, -2242, 0, 530, 0, -120,
];

const CHANNELS: usize = 2;
const FRAME_BYTES: usize = CHANNELS * 2;

#[derive(Debug)]
pub(super) struct Halver {
    history: [[i16; CHANNELS]; TAPS.len()],
    emit: bool,
}

impl Halver {
    pub(super) const fn new() -> Self {
        Self {
            history: [[0; CHANNELS]; TAPS.len()],
            emit: false,
        }
    }

    pub(super) fn process(&mut self, pcm: &[u8]) -> Vec<u8> {
        let (frames, _partial) = pcm.as_chunks::<FRAME_BYTES>();
        let mut out = Vec::with_capacity(frames.len() / 2 * FRAME_BYTES + FRAME_BYTES);
        for &[l0, l1, r0, r1] in frames {
            self.history.copy_within(1.., 0);
            if let Some(newest) = self.history.last_mut() {
                *newest = [i16::from_le_bytes([l0, l1]), i16::from_le_bytes([r0, r1])];
            }
            self.emit = !self.emit;
            if !self.emit {
                continue;
            }
            let (left, right) = TAPS.iter().zip(&self.history).fold(
                (0i32, 0i32),
                |(left, right), (tap, &[l, r])| {
                    (left + tap * i32::from(l), right + tap * i32::from(r))
                },
            );
            out.extend_from_slice(&to_sample(left).to_le_bytes());
            out.extend_from_slice(&to_sample(right).to_le_bytes());
        }
        out
    }
}

/// Rounds a Q15 sum back to a sample. The filter's ripple overshoots a full-scale input.
fn to_sample(acc: i32) -> i16 {
    let sample = ((acc + (1 << 14)) >> 15).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    i16::try_from(sample).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{FRAME_BYTES, Halver, TAPS};

    const RATE: f64 = 44_100.0;

    fn stereo(samples: impl IntoIterator<Item = (i16, i16)>) -> Vec<u8> {
        samples
            .into_iter()
            .flat_map(|(l, r)| [l.to_le_bytes(), r.to_le_bytes()].concat())
            .collect()
    }

    fn left(pcm: &[u8]) -> Vec<i16> {
        pcm.chunks_exact(FRAME_BYTES)
            .map(|f| i16::from_le_bytes([f[0], f[1]]))
            .collect()
    }

    fn sine(freq: f64, amplitude: f64, frames: usize) -> Vec<u8> {
        stereo((0..frames).map(|n| {
            let v =
                (amplitude * (2.0 * std::f64::consts::PI * freq * n as f64 / RATE).sin()) as i16;
            (v, v)
        }))
    }

    fn settled_peak(pcm: &[u8]) -> f64 {
        left(pcm)
            .iter()
            .skip(TAPS.len())
            .map(|s| f64::from(*s).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn taps_sum_to_one() {
        assert_eq!(TAPS.iter().sum::<i32>(), 1 << 15);
    }

    #[test]
    fn halves_the_number_of_frames() {
        let out = Halver::new().process(&sine(440.0, 1000.0, 4410));
        assert_eq!(out.len(), 4410 / 2 * FRAME_BYTES);
    }

    #[test]
    fn block_sizes_do_not_change_the_result() {
        let input = sine(1000.0, 8000.0, 1003);
        let whole = Halver::new().process(&input);
        let mut halver = Halver::new();
        let pieces: Vec<u8> = input
            .chunks(7 * FRAME_BYTES)
            .flat_map(|chunk| halver.process(chunk))
            .collect();
        assert_eq!(whole, pieces);
    }

    #[test]
    fn a_constant_level_passes_unchanged() {
        let out = Halver::new().process(&stereo(std::iter::repeat_n((1234, -4321), 200)));
        let settled: Vec<_> = out.chunks_exact(FRAME_BYTES).skip(TAPS.len()).collect();
        assert!(!settled.is_empty());
        for frame in settled {
            assert_eq!(
                frame,
                [1234i16.to_le_bytes(), (-4321i16).to_le_bytes()].concat()
            );
        }
    }

    #[test]
    fn speech_frequencies_keep_their_level() {
        for freq in [200.0, 1000.0, 4000.0] {
            let level = settled_peak(&Halver::new().process(&sine(freq, 10_000.0, 8820)));
            assert!(
                (level - 10_000.0).abs() < 200.0,
                "{freq} Hz came out at {level}"
            );
        }
    }

    #[test]
    fn frequencies_above_the_new_limit_are_removed() {
        // 18 kHz would fold back to 4.05 kHz.
        let level = settled_peak(&Halver::new().process(&sine(18_000.0, 10_000.0, 8820)));
        assert!(level < 100.0, "18 kHz leaked through at {level}");
    }

    #[test]
    fn full_scale_input_is_clamped_not_wrapped() {
        let square: Vec<i16> = (0..2000)
            .map(|n| {
                if (n / 20) % 2 == 0 {
                    i16::MAX
                } else {
                    i16::MIN
                }
            })
            .collect();
        let out = left(&Halver::new().process(&stereo(square.iter().map(|&s| (s, s)))));
        let mut history = [0f64; TAPS.len()];
        let mut expected = Vec::new();
        for (n, &s) in square.iter().enumerate() {
            history.copy_within(1.., 0);
            history[TAPS.len() - 1] = f64::from(s);
            if n % 2 == 0 {
                let acc: f64 = TAPS
                    .iter()
                    .zip(&history)
                    .map(|(t, h)| f64::from(*t) * h)
                    .sum();
                expected.push((acc / 32768.0).round().clamp(-32768.0, 32767.0));
            }
        }
        for (got, want) in out.iter().zip(&expected) {
            assert!(
                (f64::from(*got) - want).abs() <= 1.0,
                "got {got}, want {want}"
            );
        }
    }

    #[test]
    fn a_trailing_partial_frame_is_ignored() {
        let mut input = sine(440.0, 1000.0, 10);
        input.extend_from_slice(&[1, 2, 3]);
        assert_eq!(Halver::new().process(&input).len(), 5 * FRAME_BYTES);
    }
}
