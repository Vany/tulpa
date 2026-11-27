//! Cuts 16 kHz mono speech into whole utterances on pauses, and tags how each one ends.
//!
//! The capture used to cut its audio into fixed 4 s windows, so a phrase that crossed a boundary reached the
//! transcriber as two halves, each without the other's context. [`Segmenter`] cuts on pauses instead: an energy
//! gate over 30 ms frames, against a noise floor that follows the room (music included), with hysteresis. An
//! utterance closes after [`Config::close_silence`] of silence that follows speech; it keeps a little audio before
//! its first speech frame and after its last, is dropped when shorter than [`Config::min_speech`], and is cut at its
//! quietest frame near [`Config::max_len`] when nothing quiet enough comes (steady music must not hold it open).
//!
//! Each closed utterance carries [`Intonation`]: the pitch over its last half second of voiced frames, by YIN, as
//! rising, falling or flat. In Russian a question is often marked by intonation alone, so the agent gets it beside
//! the words.
//!
//! Std only, no allocation per sample beyond the utterance itself.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::Path;

/// Samples per second of the input.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per frame: 30 ms.
pub const FRAME: usize = 480;
/// A frame's length in milliseconds (480 samples at 16 kHz).
const FRAME_MS: u64 = 30;
/// Frames per second.
const FRAMES_PER_SEC: f32 = 1000.0 / 30.0;

/// Samples of audio between two heartbeats ([`ALIVE`]): 5 s — well inside the tray self-check's 15 s switch check
/// and 20 s freshness.
pub const BEAT: usize = 5 * 16_000;
/// The heartbeat's file name in a side's directory, rewritten every [`BEAT`] of audio.
///
/// Silence makes no utterance, and the tray's self-check judges a side by its directory changing: this file says the
/// audio still flows.
pub const ALIVE: &str = ".alive";

/// The `n`th utterance's file stem: `c<n>`, zero-padded to six digits (`c<n>.wav`, `c<n>.json`).
#[must_use]
pub fn stem(n: u64) -> String {
    format!("c{n:06}")
}

/// Writes `bytes` to `<dir>/.<name>.part`, created `0600` (raw audio and its transcript are the owner's alone),
/// then renames it to `<dir>/<name>`, so a reader never sees half a file.
///
/// # Errors
///
/// The write's or the rename's.
pub fn put(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = dir.join(format!(".{name}.part"));
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(bytes)?;
    std::fs::rename(&tmp, dir.join(name))
}

/// The stream's first sample in wall-clock (epoch) ms, from the samples read so far (akira).
///
/// Each read gives an estimate, the read's time less the audio read until then; audio that waited before it was read makes an estimate
/// late, never early, so the earliest is kept. A stream that starts late (a tap that sends nothing until something
/// plays) is anchored where its samples begin, not where the reader started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    total: u64,
    at: u64,
    noted: u64,
}

impl Default for Anchor {
    fn default() -> Self {
        Self {
            total: 0,
            at: u64::MAX,
            noted: 0,
        }
    }
}

impl Anchor {
    /// `samples` more were read at `now_ms`: the anchor (epoch ms) and, when this read's estimate is more than
    /// 500 ms past it (a backlog, or a gap in the input), that drift in ms — given at most once per 10 s, for the
    /// caller to say.
    pub fn read(&mut self, samples: usize, now_ms: u64) -> (u64, Option<u64>) {
        self.total = self
            .total
            .saturating_add(u64::try_from(samples).unwrap_or(u64::MAX));
        let estimate = now_ms.saturating_sub(self.read_ms());
        self.at = self.at.min(estimate);
        let drift = estimate.saturating_sub(self.at);
        let say = drift > 500 && now_ms.saturating_sub(self.noted) >= 10_000;
        if say {
            self.noted = now_ms;
        }
        (self.at, say.then_some(drift))
    }

    /// The anchor so far, epoch ms (`u64::MAX` before the first read).
    #[must_use]
    pub const fn at(&self) -> u64 {
        self.at
    }

    /// The audio read so far, ms.
    #[must_use]
    pub fn read_ms(&self) -> u64 {
        self.total
            .saturating_mul(1000)
            .checked_div(u64::from(SAMPLE_RATE))
            .unwrap_or(0)
    }

    /// The stream broke off for a time nobody counted (its stream failed and came back): the earliest estimate no
    /// longer holds, so the next read anchors anew. The count goes on, as the stream's offsets do.
    pub const fn restart(&mut self) {
        self.at = u64::MAX;
    }
}

/// How the segmenter cuts; [`Config::default`] holds the values the ears use.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Config {
    /// Silence after speech that closes an utterance.
    pub close_silence: f32,
    /// Shortest speech kept (first to last speech frame), in seconds.
    pub min_speech: f32,
    /// Longest utterance, in seconds; past it the utterance is cut at its quietest frame in the last
    /// [`Config::max_cut_window`].
    pub max_len: f32,
    /// Where a too-long utterance looks for its cut, back from its end, in seconds.
    pub max_cut_window: f32,
    /// Audio kept before the first speech frame, in seconds.
    pub pre_roll: f32,
    /// Audio kept after the last speech frame, in seconds.
    pub post_roll: f32,
    /// A frame is speech when its RMS is this many times the noise floor (and above [`Config::abs_min`]).
    pub over_floor: f32,
    /// A frame quieter than this RMS is never speech, whatever the floor.
    pub abs_min: f32,
    /// Speech frames in a row that start an utterance.
    pub start_frames: usize,
    /// After a (re)start, this many seconds settle the floor before any frame is classified:
    /// a capture opens with a fade-in quieter than the room, and the first frame's seed alone
    /// would name that fade-in as the floor (the room then reads as speech until the slow
    /// climb catches up — the 2026-11-18 chunk opened this way). Floor and counters run;
    /// classification waits. Zero (or less): no warmup, the first frame is classified.
    pub warmup: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // vany 2026-11-18: an end-of-phrase pause is at least 2 s, and any voice activity — an
            // unrecognised "эээ" included, his not-done-thinking signal — resets the close count,
            // which an energy gate does by construction: every speech frame zeroes the silence run.
            close_silence: 2.0,
            min_speech: 0.6,
            max_len: 60.0,
            max_cut_window: 2.0,
            pre_roll: 0.2,
            post_roll: 0.15,
            over_floor: 2.5,
            abs_min: 150.0,
            start_frames: 2,
            warmup: 1.0,
        }
    }
}

/// Seconds to whole frames, at least one.
fn frames(secs: f32) -> usize {
    let n = (secs * FRAMES_PER_SEC).round();
    if n.is_finite() && n >= 1.0 {
        // A count of frames: small, non-negative, finite.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked finite, >= 1, and small"
        )]
        let n = n as usize;
        n
    } else {
        1
    }
}

/// How an utterance ends, by its pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Contour {
    /// The pitch rose at the end (more than [`Intonation::STEP_SEMITONES`]).
    Rising,
    /// The pitch fell at the end.
    Falling,
    /// Neither, within the step.
    Flat,
    /// Too few voiced frames to tell.
    Unknown,
}

impl fmt::Display for Contour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rising => "rising",
            Self::Falling => "falling",
            Self::Flat => "flat",
            Self::Unknown => "unknown",
        })
    }
}

/// The end of an utterance, by its pitch.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Intonation {
    /// Its contour.
    pub contour: Contour,
    /// Mean pitch, Hz, over the first third of the voiced frames looked at; 0 when unknown.
    pub f0_start: f32,
    /// Mean pitch, Hz, over their last third; 0 when unknown.
    pub f0_end: f32,
}

impl Intonation {
    /// Semitones between start and end beyond which the contour is rising or falling.
    pub const STEP_SEMITONES: f32 = 2.0;
    /// The voiced frames looked at: the last half second's worth.
    const LOOK_FRAMES: usize = 17;
    /// Fewer voiced frames than this: unknown.
    const MIN_VOICED: usize = 4;

    /// From the utterance's frames, newest last: the pitch of the voiced ones among its last speech frames.
    fn of(frames_f0: &[Option<f32>]) -> Self {
        let voiced: Vec<f32> = frames_f0
            .iter()
            .rev()
            .filter_map(|f| *f)
            .take(Self::LOOK_FRAMES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if voiced.len() < Self::MIN_VOICED {
            return Self {
                contour: Contour::Unknown,
                f0_start: 0.0,
                f0_end: 0.0,
            };
        }
        let third = (voiced.len() / 3).max(1);
        let mean = |s: &[f32]| -> f32 {
            let n = f32::from(u16::try_from(s.len()).unwrap_or(u16::MAX));
            s.iter().sum::<f32>() / n
        };
        let start = voiced.get(..third).map_or(0.0, mean);
        let end = voiced
            .get(voiced.len().saturating_sub(third)..)
            .map_or(0.0, mean);
        let semis = if start > 0.0 && end > 0.0 {
            12.0 * (end / start).log2()
        } else {
            0.0
        };
        let contour = if semis > Self::STEP_SEMITONES {
            Contour::Rising
        } else if semis < -Self::STEP_SEMITONES {
            Contour::Falling
        } else {
            Contour::Flat
        };
        Self {
            contour,
            f0_start: start,
            f0_end: end,
        }
    }

    /// One line of JSON for the transcriber, with the utterance's length in ms.
    #[must_use]
    pub fn json(&self, ms: u64) -> String {
        format!(
            "{{\"intonation\":\"{}\",\"f0_start\":{},\"f0_end\":{},\"ms\":{ms}}}",
            self.contour,
            json_num(self.f0_start),
            json_num(self.f0_end)
        )
    }
}

/// One closed utterance.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Utterance {
    /// Its samples, 16 kHz mono.
    pub samples: Vec<i16>,
    /// How it ends.
    pub intonation: Intonation,
    /// It was cut at [`Config::max_len`], not at a pause: the speech goes on in the next.
    pub cut: bool,
    /// Where it starts and ends in the stream, ms from the stream's first sample (akira: the self-echo gate checks
    /// these, as wall-clock times, against Kira's own playback — not the file's mtime, which is the close delay late).
    pub start_ms: u64,
    /// See [`Utterance::start_ms`].
    pub end_ms: u64,
}

impl Utterance {
    /// Its length in milliseconds.
    #[must_use]
    pub fn ms(&self) -> u64 {
        u64::try_from(self.samples.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(1000)
            .checked_div(u64::from(SAMPLE_RATE))
            .unwrap_or(0)
    }

    /// Its JSON line beside its WAV: intonation, length, and (akira) its wall-clock span `t0`/`t1`, epoch ms, with
    /// `anchor_ms` the stream's first sample. `"cut":true` marks a max-length cut: the phrase went on (the owner,
    /// 2026-11-18: only the pause ends a turn), so the assembler holds the line and merges what follows.
    #[must_use]
    pub fn json_line(&self, anchor_ms: u64) -> String {
        let j = self.intonation.json(self.ms());
        let t0 = anchor_ms.saturating_add(self.start_ms);
        let t1 = anchor_ms.saturating_add(self.end_ms);
        format!(
            "{},\"t0\":{t0},\"t1\":{t1}{}}}\n",
            j.strip_suffix('}').unwrap_or(&j),
            if self.cut { ",\"cut\":true" } else { "" }
        )
    }
}

/// One frame of an utterance in progress.
#[derive(Debug, Clone)]
struct Frame {
    /// Its place in the stream: frames since the first (akira: the utterance's own times, for the self-echo gate).
    idx: u64,
    samples: Vec<i16>,
    rms: f32,
    speech: bool,
    f0: Option<f32>,
}

/// The cutter: feed it samples, take closed utterances.
#[derive(Debug)]
pub struct Segmenter {
    cfg: Config,
    /// Samples not yet a whole frame.
    partial: Vec<i16>,
    /// Frames kept before speech starts (pre-roll and the start run).
    before: VecDeque<Frame>,
    /// The utterance in progress, once speech started.
    current: Vec<Frame>,
    /// Silent frames since the last speech frame of `current`.
    silence: usize,
    /// The room's level, RMS.
    floor: f32,
    /// Speech frames in a row, before an utterance starts.
    run: usize,
    /// Frames seen since the start of the stream.
    seen: u64,
    /// Frames the gate marked speech since the start.
    speech_seen: u64,
    /// The last utterance given was cut at the maximum length, and what follows it has not ended yet: `Some` of
    /// the silent frames since, while no utterance is in progress.
    after_cut: Option<usize>,
    /// What followed a cut ended with nothing ([`Segmenter::take_ended`]).
    ended: bool,
}

impl Segmenter {
    /// A segmenter with `cfg`.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            partial: Vec::with_capacity(FRAME),
            before: VecDeque::new(),
            current: Vec::new(),
            silence: 0,
            floor: cfg.abs_min,
            run: 0,
            seen: 0,
            speech_seen: 0,
            after_cut: None,
            ended: false,
        }
    }

    /// The noise floor now, RMS (for logs and tests).
    #[must_use]
    pub const fn floor(&self) -> f32 {
        self.floor
    }

    /// Frames the gate marked speech since the start — diagnostics: the real-room replay
    /// test reads it, and a log can name a room that never goes quiet.
    #[must_use]
    pub const fn speech_frames(&self) -> u64 {
        self.speech_seen
    }

    /// Whether the speech that went on after a max-length cut has ended with nothing since the last call.
    ///
    /// The rest held too little speech to be an utterance, or none came for [`Config::close_silence`], or the input
    /// ended ([`Segmenter::finish`]). Whoever holds the cut piece for its rest ([`Utterance::cut`]) lets it go on
    /// this. A rest that is an utterance is returned as one, and is not said here.
    pub const fn take_ended(&mut self) -> bool {
        std::mem::replace(&mut self.ended, false)
    }

    /// What followed a cut is over, with nothing to give.
    const fn end_after_cut(&mut self) {
        if self.after_cut.take().is_some() {
            self.ended = true;
        }
    }

    /// Feeds samples; returns the utterances they closed, oldest first.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Utterance> {
        let mut out = Vec::new();
        for &s in samples {
            self.partial.push(s);
            if self.partial.len() == FRAME {
                let frame = std::mem::replace(&mut self.partial, Vec::with_capacity(FRAME));
                if let Some(u) = self.frame(frame) {
                    out.push(u);
                }
            }
        }
        out
    }

    /// The end of the input: an utterance in progress closes if it holds enough speech. Pushing may go on
    /// after it (the binary's `.close`): the floor and the stream's frame count are kept, and the next
    /// utterance starts as any start does, on a run of speech.
    pub fn finish(&mut self) -> Option<Utterance> {
        if self.current.is_empty() {
            self.end_after_cut();
            return None;
        }
        self.close(false)
    }

    fn frame(&mut self, samples: Vec<i16>) -> Option<Utterance> {
        let rms = rms(&samples);
        // A dropout: the avfoundation → aresample async chain delivers whole frames of exact
        // zeros (≈13% of a quiet room's frames, in runs of 0.5–1 s). Missing audio is not
        // quiet audio: it must not touch the floor — the 2026-11-18 incident, where every
        // zero run dragged the fast-down floor to its minimum and the room's noise then read
        // as speech, so the close never fired (279 of 279 real chunks cut at the max) — and
        // it counts as silence: nothing was said in it.
        let dropout = samples.iter().all(|&s| s == 0);
        // Warmup: the floor only rises for the first `warmup` seconds after a (re)start — the
        // loudest thing that second heard names the starting floor. A capture opens with a
        // fade-in quieter than the room (the real 2026-11-18 chunk did): taking any single
        // frame, or following the EMA down into it, names the fade-in as the floor and the
        // room then reads as speech for the ~9 s the slow climb needs. A slam that opens
        // louder is covered from above; the EMA pulls the floor down fast after warmup if the
        // room is quieter. Classification waits through the same window (below).
        // No warmup at all when the config says none (`frames` gives at least one frame).
        let warm = if self.cfg.warmup > 0.0 {
            frames(self.cfg.warmup) as u64
        } else {
            0
        };
        // The floor follows the room: down fast to a quieter frame, up slowly (≈ ×3.3 over 10 s), so steady music
        // becomes the floor and stops counting as speech, while a pause in speech pulls it back down at once.
        // A low percentile over a window was weighed and not taken: over all frames it rises through a long
        // utterance's own speech and flips mid-speech; over quiet frames alone it cannot bootstrap in a room
        // noisier than abs_min·over_floor. The EMA with dropouts skipped is what the real silent-room replay
        // validated (floor ≈362, speech frames 1658 → 128, longest silence 43.7 s of 58.75 s).
        if !dropout {
            if self.seen < warm {
                self.floor = self.floor.max(rms);
            } else if rms < self.floor {
                self.floor = (0.9f32.mul_add(self.floor, 0.1 * rms))
                    .max(self.cfg.abs_min / self.cfg.over_floor);
            } else {
                self.floor *= 1.004;
            }
        }
        let speech = self.seen >= warm
            && !dropout
            && rms > self.cfg.abs_min
            && rms > self.floor * self.cfg.over_floor;
        let f0 = if speech { yin(&samples) } else { None };
        let idx = self.seen;
        self.seen = self.seen.saturating_add(1);
        if speech {
            self.speech_seen = self.speech_seen.saturating_add(1);
        }
        let frame = Frame {
            idx,
            samples,
            rms,
            speech,
            f0,
        };
        if self.current.is_empty() {
            return self.idle(frame);
        }
        self.current.push(frame);
        if speech {
            self.silence = 0;
        } else {
            self.silence = self.silence.saturating_add(1);
        }
        if self.silence >= frames(self.cfg.close_silence) {
            return self.close(false);
        }
        if self.current.len() >= frames(self.cfg.max_len) {
            return self.cut_long();
        }
        None
    }

    /// No utterance yet: keep the pre-roll; a run of speech frames starts one.
    fn idle(&mut self, frame: Frame) -> Option<Utterance> {
        self.run = if frame.speech {
            self.run.saturating_add(1)
        } else {
            0
        };
        // After a cut, the pause that would have closed an utterance ends the wait for the cut's rest.
        if let Some(silent) = &mut self.after_cut {
            *silent = if frame.speech {
                0
            } else {
                silent.saturating_add(1)
            };
            if *silent >= frames(self.cfg.close_silence) {
                self.end_after_cut();
            }
        }
        self.before.push_back(frame);
        let keep = frames(self.cfg.pre_roll).saturating_add(self.cfg.start_frames);
        while self.before.len() > keep {
            self.before.pop_front();
        }
        if self.run >= self.cfg.start_frames {
            self.current = self.before.drain(..).collect();
            self.silence = 0;
            self.run = 0;
        }
        None
    }

    /// Closes the utterance in progress: trailing silence trimmed to the post-roll; too little speech, dropped.
    /// A dropped one that followed a cut is said ([`Segmenter::take_ended`]).
    fn close(&mut self, cut: bool) -> Option<Utterance> {
        let closed = self.close_kept(cut);
        match &closed {
            // The rest of an earlier cut, whole: it ends that wait itself; a cut one starts the wait (again).
            Some(_) => self.after_cut = cut.then_some(0),
            None => self.end_after_cut(),
        }
        closed
    }

    fn close_kept(&mut self, cut: bool) -> Option<Utterance> {
        let mut frames_now = std::mem::take(&mut self.current);
        self.silence = 0;
        let last = frames_now.iter().rposition(|f| f.speech)?;
        let first = frames_now.iter().position(|f| f.speech).unwrap_or(last);
        frames_now.truncate(
            last.saturating_add(1)
                .saturating_add(frames(self.cfg.post_roll))
                .min(frames_now.len()),
        );
        if last.saturating_sub(first).saturating_add(1) < frames(self.cfg.min_speech) {
            return None;
        }
        let f0s: Vec<Option<f32>> = frames_now
            .get(..=last)
            .unwrap_or(&[])
            .iter()
            .map(|f| f.f0)
            .collect();
        let intonation = Intonation::of(&f0s);
        let start_frame = frames_now.first().map_or(0, |f| f.idx);
        let end_frame = frames_now
            .last()
            .map_or(start_frame, |f| f.idx.saturating_add(1));
        let samples: Vec<i16> = frames_now.into_iter().flat_map(|f| f.samples).collect();
        Some(Utterance {
            samples,
            intonation,
            cut,
            start_ms: start_frame.saturating_mul(FRAME_MS),
            end_ms: end_frame.saturating_mul(FRAME_MS),
        })
    }

    /// Too long: cut at the quietest frame in the last window; what follows it starts the next utterance.
    fn cut_long(&mut self) -> Option<Utterance> {
        let len = self.current.len();
        let from = len.saturating_sub(frames(self.cfg.max_cut_window));
        let at = self
            .current
            .iter()
            .enumerate()
            .skip(from)
            .min_by(|a, b| a.1.rms.total_cmp(&b.1.rms))
            .map_or(len, |(i, _)| i.saturating_add(1));
        let rest = self.current.split_off(at.min(len));
        let done = self.close(true);
        self.current = rest;
        self.silence = 0;
        // The rest holds no speech yet: it waits for speech like any start.
        if !self.current.iter().any(|f| f.speech) {
            self.before = self.current.drain(..).collect();
        }
        done
    }
}

/// A number for JSON: one decimal, or `null` when it isn't finite (akira: `NaN` is not JSON).
fn json_num(v: f32) -> String {
    if v.is_finite() {
        format!("{v:.1}")
    } else {
        "null".to_owned()
    }
}

/// Root mean square of a frame.
fn rms(s: &[i16]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let sum: f64 = s
        .iter()
        .map(|&x| {
            let v = f64::from(x);
            v * v
        })
        .sum();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a frame's length is 480; an RMS fits f32"
    )]
    let r = (sum / s.len() as f64).sqrt() as f32;
    r
}

/// YIN pitch of one frame, Hz, 70–400; None when unvoiced.
fn yin(frame: &[i16]) -> Option<f32> {
    const MIN_HZ: u32 = 70;
    const MAX_HZ: u32 = 400;
    const THRESHOLD: f32 = 0.15;
    let signal: Vec<f32> = frame.iter().map(|&v| f32::from(v)).collect();
    let tau_min = usize::try_from(SAMPLE_RATE / MAX_HZ).ok()?;
    let tau_max = usize::try_from(SAMPLE_RATE / MIN_HZ)
        .ok()?
        .min(signal.len() / 2);
    if tau_max <= tau_min {
        return None;
    }
    let window = signal.len().saturating_sub(tau_max);
    // Difference function d(tau), cumulative mean normalised.
    let mut diffs = vec![0.0f32; tau_max.saturating_add(1)];
    for tau in 1..=tau_max {
        let mut sum = 0.0f32;
        for (left, right) in signal
            .iter()
            .take(window)
            .zip(signal.iter().skip(tau).take(window))
        {
            let diff = left - right;
            sum = diff.mul_add(diff, sum);
        }
        if let Some(slot) = diffs.get_mut(tau) {
            *slot = sum;
        }
    }
    let mut running = 0.0f32;
    let mut cmnd = vec![1.0f32; tau_max.saturating_add(1)];
    for tau in 1..=tau_max {
        let dt = diffs.get(tau).copied().unwrap_or(0.0);
        running += dt;
        let n = f32::from(u16::try_from(tau).ok()?);
        if let Some(slot) = cmnd.get_mut(tau) {
            *slot = if running > 0.0 { dt * n / running } else { 1.0 };
        }
    }
    let mut tau = tau_min;
    while tau < tau_max {
        let value = cmnd.get(tau).copied().unwrap_or(1.0);
        if value < THRESHOLD {
            // The local minimum after the dip.
            while tau.saturating_add(1) < tau_max
                && cmnd.get(tau.saturating_add(1)).copied().unwrap_or(1.0)
                    < cmnd.get(tau).copied().unwrap_or(1.0)
            {
                tau = tau.saturating_add(1);
            }
            // Parabolic interpolation around it.
            let (before, here, after) = (
                cmnd.get(tau.saturating_sub(1)).copied()?,
                cmnd.get(tau).copied()?,
                cmnd.get(tau.saturating_add(1)).copied()?,
            );
            let denom = 2.0f32.mul_add(here, -before) - after;
            let shift = if denom.abs() > f32::EPSILON {
                (after - before) / (2.0 * -denom)
            } else {
                0.0
            };
            let period = f32::from(u16::try_from(tau).ok()?) + shift.clamp(-1.0, 1.0);
            #[expect(clippy::cast_precision_loss, reason = "16000 is exact in f32")]
            let hz = SAMPLE_RATE as f32 / period;
            return (f32::from(u16::try_from(MIN_HZ).ok()?)
                ..=f32::from(u16::try_from(MAX_HZ).ok()?))
                .contains(&hz)
                .then_some(hz);
        }
        tau = tau.saturating_add(1);
    }
    None
}

/// A canonical 44-byte WAV header for 16 kHz mono s16le, then the samples.
#[must_use]
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(samples.len().saturating_mul(2)).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44usize.saturating_add(samples.len().saturating_mul(2)));
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests;
