//! The ears' signal chain, pure and std-only apart from `rubato`: any device rate and channel count in, 16 kHz mono
//! s16le out.
//!
//! Downmix by averaging the channels, one band-limited resample (rubato) from the device's native rate to 16 kHz,
//! convert to `i16`. Nothing else: no gain and no limiter (the capture runs at the device's own rate and level, and
//! the 16 kHz copy exists only for recognition). Only a sample beyond full scale saturates in the `i16` conversion.

use std::fmt;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// The output rate, samples per second.
pub const OUT_RATE: u32 = 16_000;
/// Input frames per resampler call: about 20 ms at 48 kHz.
const CHUNK: usize = 1024;

/// Averages each frame of `interleaved` (`channels` samples per frame) into one mono sample.
///
/// A trailing partial frame is dropped. `channels == 0` yields nothing.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a channel count is far below f32's exact-integer range"
    )]
    let n = channels as f32;
    interleaved
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / n)
        .collect()
}

/// One sample in [-1, 1] as `i16`, rounded; out-of-range input saturates and `NaN` is 0.
pub fn to_i16(s: f32) -> i16 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to ±32767 first; `as` saturates and maps NaN to 0"
    )]
    let v = (s * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
    v
}

/// `samples` as s16le bytes, the ears' wire format.
pub fn to_s16le(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Why a [`Chain`] could not be built or run.
#[derive(Debug)]
#[non_exhaustive]
pub enum DspError {
    /// The device reported no channels or a zero rate.
    BadFormat {
        /// The device's rate.
        rate: u32,
        /// The device's channel count.
        channels: u16,
    },
    /// The resampler refused the rates or a chunk.
    Resampler(String),
}

impl fmt::Display for DspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadFormat { rate, channels } => {
                write!(f, "unusable input format: {rate} Hz, {channels} channels")
            }
            Self::Resampler(e) => write!(f, "resampler: {e}"),
        }
    }
}

impl std::error::Error for DspError {}

/// The whole chain for one device: interleaved f32 frames at the device rate in, 16 kHz mono `i16` out.
///
/// Streaming: input is buffered until the resampler has a full chunk, so a call may return fewer samples than its
/// input implies (or none) and the rest comes with later calls. The FFT resampler adds a fixed delay of a few
/// milliseconds.
pub struct Chain {
    channels: usize,
    /// `None` when the device already runs at 16 kHz.
    resampler: Option<Fft<f32>>,
    pending: Vec<f32>,
    out: Vec<f32>,
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("channels", &self.channels)
            .field("resampling", &self.resampler.is_some())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Chain {
    /// A chain for a device at `rate` Hz with `channels` channels.
    ///
    /// # Errors
    ///
    /// [`DspError::BadFormat`] for a zero rate or channel count; [`DspError::Resampler`] when rubato refuses the
    /// ratio.
    pub fn new(rate: u32, channels: u16) -> Result<Self, DspError> {
        if rate == 0 || channels == 0 {
            return Err(DspError::BadFormat { rate, channels });
        }
        let resampler = if rate == OUT_RATE {
            None
        } else {
            let r = Fft::<f32>::new(rate as usize, OUT_RATE as usize, CHUNK, 1, FixedSync::Input)
                .map_err(|e| DspError::Resampler(e.to_string()))?;
            Some(r)
        };
        let out = vec![0.0; resampler.as_ref().map_or(0, Resampler::output_frames_max)];
        Ok(Self {
            channels: usize::from(channels),
            resampler,
            pending: Vec::new(),
            out,
        })
    }

    /// Feeds interleaved frames and returns the 16 kHz mono samples that are ready.
    ///
    /// # Errors
    ///
    /// [`DspError::Resampler`] when rubato rejects a chunk (it should not, given the buffers it sized).
    pub fn push(&mut self, interleaved: &[f32]) -> Result<Vec<i16>, DspError> {
        let mono = downmix(interleaved, self.channels);
        let ready = match self.resampler.as_mut() {
            None => mono,
            Some(r) => {
                self.pending.extend_from_slice(&mono);
                let mut ready = Vec::new();
                loop {
                    let need = r.input_frames_next();
                    if self.pending.len() < need {
                        break;
                    }
                    let input = InterleavedSlice::new(&self.pending, 1, need)
                        .map_err(|e| DspError::Resampler(e.to_string()))?;
                    let cap = self.out.len();
                    let mut output = InterleavedSlice::new_mut(&mut self.out, 1, cap)
                        .map_err(|e| DspError::Resampler(e.to_string()))?;
                    let (read, written) = r
                        .process_into_buffer(&input, &mut output, None)
                        .map_err(|e| DspError::Resampler(e.to_string()))?;
                    ready.extend_from_slice(self.out.get(..written).unwrap_or_default());
                    self.pending.drain(..read.min(self.pending.len()));
                }
                ready
            }
        };
        Ok(ready.into_iter().map(to_i16).collect())
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::cast_sign_loss,
        reason = "test signal generators turn indices into seconds and check exact positions"
    )]

    use super::*;
    use std::f32::consts::TAU;

    fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (TAU * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Sign changes from negative to non-negative or back, over `samples`.
    fn crossings(samples: &[i16]) -> usize {
        samples
            .windows(2)
            .filter(|w| (w[0] < 0) != (w[1] < 0))
            .count()
    }

    fn rms(s: &[i16]) -> f64 {
        (s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
    }

    #[test]
    fn chain_applies_no_gain_or_limiting() {
        // At 16 kHz there is no resampler: the samples come out exactly as `to_i16` makes them, a 0.99 peak included
        // (a limiter would hold it at 0.95, a gain would scale it).
        let input = sine(440.0, 16_000, 1.0, 0.99);
        let mut c = Chain::new(16_000, 1).unwrap();
        let out = c.push(&input).unwrap();
        let expect: Vec<i16> = input.iter().copied().map(to_i16).collect();
        assert_eq!(out, expect);

        // At 48 kHz the only change is the resample: the RMS of a 1 kHz sine at 0.99 stays within 1 %, and its
        // peaks still pass 0.95 of full scale.
        let input = sine(1000.0, 48_000, 1.0, 0.99);
        let mut c = Chain::new(48_000, 1).unwrap();
        let out = c.push(&input).unwrap();
        let steady = &out[1600..];
        let want = 0.99 * 32767.0 / std::f64::consts::SQRT_2;
        let got = rms(steady);
        assert!((got - want).abs() / want < 0.01, "rms {got} want {want}");
        let ceiling = to_i16(0.95).unsigned_abs();
        assert!(steady.iter().any(|s| s.unsigned_abs() > ceiling));
    }

    #[test]
    fn stereo_downmix_averages() {
        let m = downmix(&[1.0, 0.0, 0.5, -0.5, -1.0, -0.2, 0.3], 2);
        assert_eq!(m, vec![0.5, 0.0, -0.6]);
        assert_eq!(downmix(&[1.0], 0), Vec::<f32>::new());
    }

    #[test]
    fn resamples_a_48k_1khz_sine_to_16k_keeping_1khz() {
        let mut c = Chain::new(48_000, 1).unwrap();
        let input = sine(1000.0, 48_000, 2.0, 0.5);
        let mut out = Vec::new();
        // Device-sized callbacks of 512 frames, not aligned to the resampler's chunk.
        for chunk in input.chunks(512) {
            out.extend(c.push(chunk).unwrap());
        }
        // Two seconds in: about 32000 out, less what is still buffered (one chunk) and the filter delay.
        assert!(
            (31_000..=32_000).contains(&out.len()),
            "{} samples for 2 s",
            out.len()
        );
        // Skip the filter's start-up, then count: 1 kHz crosses zero 2000 times a second.
        let steady = &out[1600..17_600];
        let per_sec = crossings(steady);
        assert!((1980..=2020).contains(&per_sec), "{per_sec} crossings/s");
    }

    #[test]
    fn resamples_stereo_44k1_to_16k() {
        let mono = sine(1000.0, 44_100, 1.0, 0.5);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut c = Chain::new(44_100, 2).unwrap();
        let out = c.push(&stereo).unwrap();
        assert!((15_000..=16_000).contains(&out.len()), "{}", out.len());
        let per_sec = crossings(&out[1600..]) as f32 / ((out.len() - 1600) as f32 / 16_000.0);
        assert!((per_sec - 2000.0).abs() < 25.0, "{per_sec} crossings/s");
    }

    #[test]
    fn s16le_is_little_endian() {
        assert_eq!(to_s16le(&[1, -2]), vec![0x01, 0x00, 0xFE, 0xFF]);
        assert_eq!(to_i16(1.5), 32767);
        assert_eq!(to_i16(f32::NAN), 0);
    }

    #[test]
    fn zero_format_is_refused() {
        assert!(matches!(Chain::new(0, 1), Err(DspError::BadFormat { .. })));
        assert!(matches!(
            Chain::new(48_000, 0),
            Err(DspError::BadFormat { .. })
        ));
    }
}
