//! The VAD's speech spans joined into one buffer for the recognizer, and token times mapped back to the utterance.
//!
//! [`join`] copies the shape of whisper.cpp's own `whisper_vad` (whisper-rs-sys 0.15.0, `src/whisper.cpp`, about
//! lines 6640-6790): every span as the VAD gave it (its `speech_pad_ms` already in), each but the last extended at
//! its end by the VAD's `samples_overlap`, and [`JOINT_SAMPLES`] of zeros between spans. The recognizer reads the
//! joined buffer in one pass, so it keeps its sentence context across spans. While joining, [`join`] keeps a
//! [`Piece`] per copied stretch, and [`map_tokens`] puts the recognizer's token times back on the utterance's
//! timeline through them. This module needs no `whisper` feature: it knows samples, spans and [`Token`]s, nothing
//! of whisper.

use crate::recognize::Token;

/// Samples per second: the recognizer's 16 kHz (whisper.cpp's `WHISPER_SAMPLE_RATE`).
pub const SAMPLE_RATE: u32 = 16_000;

/// Samples per ms at [`SAMPLE_RATE`].
const SAMPLES_PER_MS: u64 = 16;

/// The zeros put between two joined spans: 0.1 s, as whisper.cpp's `whisper_vad` inserts them
/// (`int silence_samples = 0.1 * WHISPER_SAMPLE_RATE;`).
pub const JOINT_SAMPLES: usize = 1600;

/// One stretch of the utterance copied whole into the joined buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Piece {
    /// Where it starts in the joined buffer (samples).
    pub joined_start: usize,
    /// Where it starts in the utterance (samples).
    pub orig_start: usize,
    /// Its length (samples), never 0.
    pub len: usize,
}

impl Piece {
    /// Its end in the utterance (exclusive, samples).
    #[must_use]
    pub const fn orig_end(self) -> usize {
        self.orig_start.saturating_add(self.len)
    }
}

/// The speech of an utterance, joined: the samples the recognizer reads, and where each piece came from.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Joined {
    /// The pieces and the zeros between them.
    pub samples: Vec<f32>,
    /// The copied pieces, in order (no joints).
    pub pieces: Vec<Piece>,
}

/// A VAD time in centiseconds to a sample index, as whisper.cpp's `cs_to_samples` (round half up); negative and NaN
/// go to 0.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float to int `as` saturates, and NaN goes to 0"
)]
fn cs_to_samples(cs: f32) -> usize {
    (f64::from(cs) / 100.0)
        .mul_add(f64::from(SAMPLE_RATE), 0.5)
        .floor()
        .max(0.0) as usize
}

/// `samples` cut to the VAD's speech and joined as whisper.cpp joins it.
///
/// `spans_cs` is the VAD's (start, end) in centiseconds, as `segments_from_samples` returns them; `overlap_s` is
/// the VAD's `samples_overlap` in seconds. Each span but the last is extended at its end by `overlap_s`; each is
/// clamped to the utterance and to start no earlier than the previous piece's end, so no sample is copied twice
/// (whisper.cpp clamps to the last sample but one, and lets an overlap be read twice); a span left empty is
/// dropped. [`JOINT_SAMPLES`] zeros go between two kept pieces. No spans give an empty [`Joined`].
#[must_use]
pub fn join(samples: &[f32], spans_cs: &[(f32, f32)], overlap_s: f32) -> Joined {
    // As whisper.cpp: `int overlap_samples = overlap_seconds * WHISPER_SAMPLE_RATE;` (truncation).
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float to int `as` saturates, as the C conversion truncates"
    )]
    let overlap = (f64::from(overlap_s) * f64::from(SAMPLE_RATE)).max(0.0) as usize;
    let n = samples.len();
    let mut out = Joined::default();
    let last = spans_cs.len().saturating_sub(1);
    for (i, &(start, end)) in spans_cs.iter().enumerate() {
        let floor = out.pieces.last().map_or(0, |p| p.orig_end());
        let mut end = cs_to_samples(end);
        if i < last {
            end = end.saturating_add(overlap);
        }
        let start = cs_to_samples(start).max(floor).min(n);
        let end = end.min(n);
        let Some(piece) = samples.get(start..end).filter(|p| !p.is_empty()) else {
            continue;
        };
        if !out.pieces.is_empty() {
            out.samples
                .resize(out.samples.len().saturating_add(JOINT_SAMPLES), 0.0);
        }
        out.pieces.push(Piece {
            joined_start: out.samples.len(),
            orig_start: start,
            len: piece.len(),
        });
        out.samples.extend_from_slice(piece);
    }
    out
}

/// A sample index in the joined buffer back to the utterance.
///
/// Inside a piece: its utterance start plus the distance into it. That is exact, because each piece was copied
/// contiguously (whisper.cpp instead interpolates between table points every 200 ms). In a joint (or exactly at a
/// piece's end): the end of the piece before it. Past the last piece: its end. No pieces: 0.
#[must_use]
pub fn map_sample(pieces: &[Piece], t: usize) -> usize {
    let Some(p) = piece_at(pieces, t).and_then(|i| pieces.get(i)) else {
        return 0;
    };
    let into = t.saturating_sub(p.joined_start);
    if into < p.len {
        p.orig_start.saturating_add(into)
    } else {
        p.orig_end()
    }
}

/// The index of the piece a joined sample index belongs to: the last piece starting at or before it (so a joint
/// belongs to the piece before it, as in [`map_sample`]). `None` only without pieces.
fn piece_at(pieces: &[Piece], t: usize) -> Option<usize> {
    pieces
        .partition_point(|p| p.joined_start <= t)
        .checked_sub(1)
}

/// ms to a sample index at [`SAMPLE_RATE`].
fn ms_to_sample(t_ms: u64) -> usize {
    usize::try_from(t_ms.saturating_mul(SAMPLES_PER_MS)).unwrap_or(usize::MAX)
}

/// A sample index to the nearest ms.
fn sample_to_ms(s: usize) -> u64 {
    u64::try_from(s)
        .unwrap_or(u64::MAX)
        .saturating_add(SAMPLES_PER_MS / 2)
        / SAMPLES_PER_MS
}

/// A time in ms in the joined buffer back to ms in the utterance ([`map_sample`]; rounded to the nearest ms).
#[must_use]
pub fn map_ms(pieces: &[Piece], t_ms: u64) -> u64 {
    sample_to_ms(map_sample(pieces, ms_to_sample(t_ms)))
}

/// One token's (start, end) in the joined buffer (ms) back to the utterance (ms).
///
/// Each end maps by [`map_ms`], with one rule more for a token that straddles a joint: when its start lies in one
/// piece and its end in a later one, it starts at that later piece's start (its end unchanged). whisper pins the
/// t0 of the first token after a silence to the previous token's end; in the original audio that silence was cut
/// out, and the joint makes it visible: the token would otherwise begin before the cut, at the previous word's end,
/// though its sound is all after it. A token ending exactly at its piece's end, or in the joint after it, is in
/// that piece and is left alone.
#[must_use]
pub fn map_span_ms(pieces: &[Piece], start_ms: u64, end_ms: u64) -> (u64, u64) {
    let (s, e) = (ms_to_sample(start_ms), ms_to_sample(end_ms));
    let end = sample_to_ms(map_sample(pieces, e));
    let start = match (piece_at(pieces, s), piece_at(pieces, e)) {
        (Some(a), Some(b)) if b > a => pieces.get(b).map_or(0, |p| p.orig_start),
        _ => map_sample(pieces, s),
    };
    (sample_to_ms(start), end)
}

/// The recognizer's tokens (times from the joined buffer's start) with their start and end mapped back.
///
/// Each token goes through [`map_span_ms`]. No pieces give no tokens. [`map_ms`] never goes backwards; a
/// straddler's moved start could pass a later token's start only if whisper began that token inside the straddler,
/// in an earlier piece (not seen on the fixtures, whose times `tests/whisper_fixture.rs` checks never go backwards).
#[must_use]
pub fn map_tokens(pieces: &[Piece], tokens: Vec<Token>) -> Vec<Token> {
    if pieces.is_empty() {
        return Vec::new();
    }
    tokens
        .into_iter()
        .map(|mut tok| {
            (tok.start_ms, tok.end_ms) = map_span_ms(pieces, tok.start_ms, tok.end_ms);
            tok
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ramp, so a sample's value is its index: copied samples show where they came from.
    #[expect(clippy::cast_precision_loss, reason = "small test indices")]
    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    const fn piece(joined_start: usize, orig_start: usize, len: usize) -> Piece {
        Piece {
            joined_start,
            orig_start,
            len,
        }
    }

    fn tok(text: &str, start_ms: u64, end_ms: u64) -> Token {
        Token::new(text, start_ms, end_ms, 1.0)
    }

    /// Two spans: 1.00-2.00 s and 5.00-6.00 s of a 10 s utterance, the default 0.1 s overlap.
    fn two() -> Joined {
        join(&ramp(160_000), &[(100.0, 200.0), (500.0, 600.0)], 0.1)
    }

    #[test]
    fn spans_are_joined_as_whisper_cpp_joins_them() {
        let j = two();
        // The first span with its 0.1 s overlap (1.0-2.1 s), the joint, the last span without overlap.
        assert_eq!(
            j.pieces,
            [piece(0, 16_000, 17_600), piece(19_200, 80_000, 16_000)]
        );
        assert_eq!(j.samples.len(), 35_200);
        assert_eq!(j.samples.first().copied(), Some(16_000.0));
        assert_eq!(j.samples.get(17_599).copied(), Some(33_599.0));
        assert!(
            j.samples
                .get(17_600..19_200)
                .is_some_and(|z| z.iter().all(|s| s.to_bits() == 0)),
            "the joint is zeros"
        );
        assert_eq!(j.samples.get(19_200).copied(), Some(80_000.0));
        assert_eq!(j.samples.last().copied(), Some(95_999.0));
    }

    #[test]
    fn a_time_inside_a_piece_maps_exactly() {
        let j = two();
        // 0.5 s into the joined buffer = 0.5 s into the first span = 1.5 s.
        assert_eq!(map_ms(&j.pieces, 500), 1500);
        // 1.45 s into the joined buffer (past piece 1 and the joint) = 0.25 s into the second span.
        assert_eq!(map_ms(&j.pieces, 1450), 5250);
        assert_eq!(map_sample(&j.pieces, 19_200), 80_000);
    }

    #[test]
    fn the_overlap_maps_to_the_audio_it_was_copied_from() {
        let j = two();
        // 1.05 s into the joined buffer is in the first span's 0.1 s overlap: 2.05 s of the utterance, past the
        // VAD's end of that span (2.00 s), because those are the samples that were copied there.
        assert_eq!(map_ms(&j.pieces, 1050), 2050);
    }

    #[test]
    fn a_time_in_a_joint_maps_to_the_earlier_piece_end() {
        let j = two();
        for t in [1100, 1150, 1199] {
            assert_eq!(map_ms(&j.pieces, t), 2100, "{t}");
        }
        assert_eq!(map_ms(&j.pieces, 1200), 5000);
    }

    #[test]
    fn a_time_past_the_end_maps_to_the_last_piece_end() {
        let j = two();
        assert_eq!(map_ms(&j.pieces, 2200), 6000);
        assert_eq!(map_ms(&j.pieces, 30_000), 6000);
    }

    #[test]
    fn tokens_map_in_order_and_never_go_backwards() {
        let j = two();
        let got = map_tokens(
            &j.pieces,
            vec![
                tok(" a", 0, 400),
                tok(" b", 900, 1150),
                tok(" c", 1200, 1500),
                tok(" d", 2100, 2900),
            ],
        );
        let times: Vec<(u64, u64)> = got.iter().map(|t| (t.start_ms, t.end_ms)).collect();
        assert_eq!(
            times,
            [(1000, 1400), (1900, 2100), (5000, 5300), (5900, 6000)]
        );
        for (a, b) in got.iter().zip(got.iter().skip(1)) {
            assert!(
                a.start_ms <= b.start_ms && a.end_ms <= b.end_ms,
                "{a:?} {b:?}"
            );
        }
    }

    #[test]
    fn a_token_straddling_a_joint_starts_at_the_later_piece() {
        let j = two();
        // whisper's t0 at 0.95 s (in piece 1, 1.95 s), t1 at 1.40 s (in piece 2, 5.20 s): the sound is after the cut.
        assert_eq!(map_span_ms(&j.pieces, 950, 1400), (5000, 5200));
        // Starting in the joint (which belongs to piece 1) and ending in piece 2 is a straddle too.
        assert_eq!(map_span_ms(&j.pieces, 1150, 1300), (5000, 5100));
        // Past the last piece's end still counts as in the last piece.
        assert_eq!(map_span_ms(&j.pieces, 950, 9000), (5000, 6000));
        let got = map_tokens(&j.pieces, vec![tok(" Today", 950, 1400)]);
        assert_eq!(
            got.iter()
                .map(|t| (t.start_ms, t.end_ms))
                .collect::<Vec<_>>(),
            [(5000, 5200)]
        );
    }

    #[test]
    fn a_token_inside_one_piece_is_untouched() {
        let j = two();
        assert_eq!(map_span_ms(&j.pieces, 200, 900), (1200, 1900));
        assert_eq!(map_span_ms(&j.pieces, 1300, 1700), (5100, 5500));
    }

    #[test]
    fn a_token_ending_on_its_piece_end_or_in_the_joint_is_untouched() {
        let j = two();
        // Piece 1 ends at 1.10 s joined (2.10 s); the joint runs to 1.20 s.
        assert_eq!(map_span_ms(&j.pieces, 900, 1100), (1900, 2100));
        assert_eq!(map_span_ms(&j.pieces, 900, 1150), (1900, 2100));
    }

    #[test]
    fn spans_are_clamped_and_never_read_twice() {
        let j = join(
            &ramp(48_000),
            &[
                (-5.0, 50.0),   // before the start: from 0; overlap to 0.6 s
                (55.0, 120.0),  // starts inside the previous overlap: from 0.6 s; overlap to 1.3 s
                (100.0, 110.0), // wholly read already: dropped (and no joint for it)
                (290.0, 900.0), // past the end, last: to 3.0 s
            ],
            0.1,
        );
        assert_eq!(
            j.pieces,
            [
                piece(0, 0, 9600),
                piece(11_200, 9600, 11_200),
                piece(24_000, 46_400, 1600),
            ]
        );
        assert_eq!(j.samples.len(), 25_600);
    }

    #[test]
    fn zero_spans_give_nothing() {
        let j = join(&ramp(16_000), &[], 0.1);
        assert_eq!(j, Joined::default());
        assert_eq!(map_tokens(&j.pieces, vec![tok("lost", 0, 10)]), []);
    }
}
