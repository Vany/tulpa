//! One side of the ears, in memory: capture [`Event`]s → [`Segmenter`] → [`Utterance`] →
//! [`Recognizer`] (with the [`recognize`] retry) → [`Heard`] on a channel.
//!
//! [`run`] is generic over where the events come from (the mic's or the tap's channel, or a test's list) and over
//! the recognizer, which the two sides share behind a [`Mutex`] (one model in memory). On the "me" side it looks
//! for the tray's `.close` marker ([`Pipeline::close`]) after every audio chunk, exactly as the `tulpa-segment`
//! binary does: a marker that is there is removed and closes the utterance in progress, with the chunk's audio in
//! it; the segmenter's noise floor is kept (no new warmup). The shadow rehearsal ([`run_with`], [`CloseMode::Watch`])
//! only looks at the marker.
//!
//! The live path also writes what the capture script's segmenter did, for those who read it: the side's heartbeat
//! ([`Beating`](crate::beat::Beating): `<dir>/.alive` — the tray's self-check judges a side by its directory changing, and silence makes
//! no utterance), and each recognized piece into the raw ring ([`Pipeline::with_ring`], [`Ring`]). Without them
//! (the shadow, the tests) nothing is written.
//! Retries and failures are said on stderr by side and length, never by content. With the debug switch
//! ([`Pipeline::with_debug`]) each recognized piece's passes, raw text included, are one [`DEBUG_MARKER`] line on
//! stdout.
//!
//! [`Heard`] carries what the line assembler ([`assemble`](crate::assemble)) needs: the span, the utterance's
//! length, whether the segmenter cut it at its maximum length, and how it ends.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use tulpa_stop::{Sender, Stop};

use tulpa_segment::{Anchor, Config, Contour, Segmenter, Utterance, stem};

use crate::assemble;
pub use crate::close::CloseMode;
use crate::close::Closes;
use crate::event::Event;
use crate::recognize::{RecognizeError, Recognizer, Retry, Transcript, recognize, to_f32};
use crate::ring::Ring;

/// Which side of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Side {
    /// The mic: the owner.
    Me,
    /// The system audio: the other party.
    Them,
}

impl Side {
    /// `me` or `them`, as on the line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Me => "me",
            Self::Them => "them",
        }
    }
}

/// One recognized utterance.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Heard {
    /// Whose.
    pub side: Side,
    /// Where it starts, epoch ms.
    pub t0_ms: u64,
    /// Where it ends, epoch ms.
    pub t1_ms: u64,
    /// Its length in ms, from its samples (the `<voice N.Ns>` / `<no speech N.Ns>` of an unread one).
    pub ms: u64,
    /// The segmenter cut it at its maximum length, not at a pause: the speech goes on in the next.
    pub cut: bool,
    /// How its pitch ends.
    pub contour: Contour,
    /// What was read (possibly nothing: heard, not read).
    pub transcript: Transcript,
    /// The piece's name, as the ring and heard.log know it (`c<n>`; a replayed piece's file stem).
    pub piece: String,
}

/// What a side's pipeline gives out.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Out {
    /// An utterance, recognized.
    Heard(Heard),
    /// The speech that went on after `side`'s last max-length cut ended with nothing to give: too little speech
    /// for an utterance, a pause with none, the stream's end, or a recognition that failed. Whoever holds the cut
    /// piece for its rest lets it go.
    Ended(Side),
}

/// Where a pipeline's utterances go.
///
/// Every path that ends a segment tells it here: an utterance through [`Deliver::heard`], an end with nothing
/// after a cut through [`Deliver::ended`].
pub trait Deliver {
    /// Takes one utterance; `false` when nobody listens any more.
    fn heard(&self, h: Heard) -> bool;
    /// `side`'s cut phrase got no rest ([`Out::Ended`]); `false` when nobody listens any more. Ignored by a
    /// receiver that only collects utterances.
    fn ended(&self, _side: Side) -> bool {
        true
    }
}

impl Deliver for Sender<Heard> {
    fn heard(&self, h: Heard) -> bool {
        self.send(h).is_ok()
    }
}

impl Deliver for Sender<Out> {
    fn heard(&self, h: Heard) -> bool {
        self.send(Out::Heard(h)).is_ok()
    }

    fn ended(&self, side: Side) -> bool {
        self.send(Out::Ended(side)).is_ok()
    }
}

/// One side's settings.
#[derive(Debug)]
#[non_exhaustive]
pub struct Pipeline<'a, R: ?Sized> {
    /// Whose audio this is.
    pub side: Side,
    /// When the side started, epoch ms: the pieces' numbering starts here and, without a [`Pipeline::clock`], so
    /// does the stream (`t0`/`t1` are this plus the utterance's offsets).
    pub start_ms: u64,
    /// The wall clock the stream is anchored on, by the samples read ([`Anchor`]): a stream whose samples begin
    /// late (the tap sends nothing until something plays) starts where they begin. `None`: at `start_ms`.
    pub clock: Option<fn() -> u64>,
    /// Whisper's language (`EARS_LANG`).
    pub lang: &'a str,
    /// The tray's close marker (`<EARS_STATE>/me/.close`), looked for after every audio chunk; `None` for "them".
    pub close: Option<PathBuf>,
    /// The recognizer, shared between the sides.
    pub recognizer: &'a Mutex<R>,
    /// The raw ring each recognized piece parks in; `None`: none kept.
    pub ring: Option<&'a Ring>,
    /// The run's stop. Once it is set this side only drains what it has: it takes no control marker any more
    /// (the tray's `.close`), since one written from then on is meant for the capture that comes next. `None`:
    /// never stopped.
    pub stop: Option<&'a Stop>,
    /// Whether each recognized piece writes its [`DEBUG_MARKER`] line ([`Pipeline::with_debug`]).
    pub debug: bool,
}

impl<'a, R: ?Sized> Pipeline<'a, R> {
    /// One side's settings; see the fields.
    #[must_use]
    pub const fn new(
        side: Side,
        start_ms: u64,
        lang: &'a str,
        close: Option<PathBuf>,
        recognizer: &'a Mutex<R>,
    ) -> Self {
        Self {
            side,
            start_ms,
            lang,
            close,
            recognizer,
            ring: None,
            clock: None,
            stop: None,
            debug: false,
        }
    }

    /// This side writing one [`DEBUG_MARKER`] line on stdout per recognized piece: `--ears` with the debug switch
    /// on ([`ears::DEBUG_ENV`](crate::ears::DEBUG_ENV)).
    #[must_use]
    pub const fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// This side's stream anchored on the samples read by `now_ms` (epoch ms), not at `start_ms`.
    #[must_use]
    pub const fn with_clock(mut self, now_ms: fn() -> u64) -> Self {
        self.clock = Some(now_ms);
        self
    }

    /// This side under `stop`: once it is set, the side takes no control marker any more ([`Pipeline::stop`]).
    #[must_use]
    pub const fn with_stop(mut self, stop: &'a Stop) -> Self {
        self.stop = Some(stop);
        self
    }

    /// This side keeping each recognized piece in `ring`.
    #[must_use]
    pub const fn with_ring(mut self, ring: &'a Ring) -> Self {
        self.ring = Some(ring);
        self
    }
}

/// Runs one side until `events` ends (the capture stopped) or the receiver of `heard` hangs up.
///
/// The end of the events closes the utterance in progress. The `.close` marker is taken or only watched, as `mode`
/// says.
pub fn run_with<R, I, D>(p: &Pipeline<'_, R>, mode: CloseMode, events: I, heard: &D)
where
    R: Recognizer + ?Sized,
    I: IntoIterator<Item = Event>,
    D: Deliver,
{
    let mut seg = Segmenter::new(config(p.side));
    let mut asked = Closes::new(mode);
    // The pieces are numbered from the side's start, as the script's segmenter numbers its files.
    let mut n = p.start_ms;
    let mut anchor = Anchor::default();
    // The stream time lost so far (`Event::Lost`): said on the drift line.
    let mut lost_ms = 0_u64;
    for ev in events {
        let chunk = match ev {
            Event::Audio(chunk) => chunk,
            Event::Device {
                name,
                rate,
                channels,
            } => {
                say(p.side, &format!("device {name}, {rate} Hz, {channels} ch"));
                continue;
            }
            ended @ (Event::StreamError(_)
            | Event::Switched { .. }
            | Event::NoInput
            | Event::Lost { .. }) => {
                match &ended {
                    Event::StreamError(e) => say(p.side, &format!("warning: stream: {e}")),
                    Event::NoInput => say(p.side, "no device"),
                    Event::Lost { ms } => {
                        lost_ms = lost_ms.saturating_add(*ms);
                        say(
                            p.side,
                            &format!(
                                "warning: {ms} ms of audio not passed on (a stall, or a backlog past its bound): \
                                 the stream's clock starts over"
                            ),
                        );
                    }
                    _ => {}
                }
                // The current stream is over (it failed, or the follower dropped it for another device or none), or
                // its timeline broke (a stall the fill does not cover):
                // the phrase in progress ends here, as at the capture's end. What comes next follows a gap
                // nobody counted: the anchor starts over, and the closes still waiting for a point in the old
                // stream are dropped with it.
                if !flush(p, &mut seg, &anchor, &mut n, heard) {
                    return;
                }
                anchor.restart();
                asked.clear();
                continue;
            }
            Event::Warning(w) => {
                say(p.side, &format!("warning: {w}"));
                continue;
            }
            // Judged where the tap's audio arrives ([`Beating`](crate::beat::Beating)), which says a deaf tap here
            // as a `Warning`.
            Event::OutputRunning(_) => continue,
        };
        // This thread reads its queued audio late while whisper runs, so a read's estimate trails the anchor by
        // whisper's backlog: that drift is said once it is long enough to matter (`behind_line`), with what the
        // side's queue dropped so far. The earliest estimate is kept: a gap
        // in the tap's input comes as silence ([`Beating`](crate::beat::Beating) fills it where the samples arrive), so the samples keep
        // the wall clock's time.
        // Read once per chunk: the anchor's estimate, and the time a close is taken at.
        let now = p.clock.map(|now| now());
        if let Some(now) = now {
            let (_anchor, drift) = anchor.read(chunk.len(), now);
            if let Some(line) = drift.and_then(|d| behind_line(d, lost_ms)) {
                say(p.side, &line);
            }
        }
        let start = stream_start(p, &anchor);
        let mut closed = seg.push(&chunk);
        let read_ms = anchor.read_ms();
        // The tray's `.close`, once the audio up to the ask has been pushed ([`Closes::due`]).
        if asked.due(p, now, start, read_ms)
            && let Some(u) = seg.finish()
        {
            closed.push(u);
        }
        for u in closed {
            if !deliver(p, &u, start, &mut n, heard) {
                return;
            }
        }
        // A cut phrase whose rest ended with nothing: said after what this chunk closed, the cut piece included.
        if seg.take_ended() && !heard.ended(p.side) {
            return;
        }
    }
    let _sent: bool = flush(p, &mut seg, &anchor, &mut n, heard);
}

/// How `side`'s audio is cut: the segmenter's defaults, and on "them" no warmup.
///
/// The warmup lets a room name its noise floor in the first second, and classifies nothing meanwhile. The tap has
/// no room: it sends nothing, or the fill's zeros (which never touch the floor), until something plays, so its
/// first audio is the speech itself. A warmup there made the tap's first sentence the floor and dropped it.
fn config(side: Side) -> Config {
    let mut cfg = Config::default();
    if side == Side::Them {
        cfg.warmup = 0.0;
    }
    cfg
}

/// The pipeline reads its audio at least this far behind the wall clock before it says so: whisper's usual second
/// or two per phrase is not worth a line.
const BEHIND_SAID_MS: u64 = 5_000;

/// The line for a pipeline reading `drift_ms` behind its audio, `lost_ms` of the stream dropped so far; `None`
/// while the drift is under [`BEHIND_SAID_MS`]. The anchor gives a drift at most once in 10 s.
fn behind_line(drift_ms: u64, lost_ms: u64) -> Option<String> {
    (drift_ms >= BEHIND_SAID_MS).then(|| {
        format!("warning: reading {drift_ms} ms behind the audio; {lost_ms} ms dropped so far")
    })
}

/// Where the stream began, epoch ms: the anchor on the samples read, or the side's start without a clock.
fn stream_start<R: ?Sized>(p: &Pipeline<'_, R>, anchor: &Anchor) -> u64 {
    p.clock.map_or(p.start_ms, |_| anchor.at())
}

/// Closes the utterance in progress (the capture ended, or its stream failed) and delivers it; `false` when the
/// receiver hung up.
fn flush<R: Recognizer + ?Sized>(
    p: &Pipeline<'_, R>,
    seg: &mut Segmenter,
    anchor: &Anchor,
    n: &mut u64,
    heard: &impl Deliver,
) -> bool {
    let delivered = seg
        .finish()
        .is_none_or(|u| deliver(p, &u, stream_start(p, anchor), n, heard));
    // The stream's end is the end of a cut phrase's rest too.
    delivered && (!seg.take_ended() || heard.ended(p.side))
}

/// Recognizes `u` (the `n`th piece; `n` counts on) of the stream that began at `start` (epoch ms) and sends it,
/// after parking it in the ring; `false` when the receiver hung up. A failed recognition is said and dropped.
fn deliver<R: Recognizer + ?Sized>(
    p: &Pipeline<'_, R>,
    u: &Utterance,
    start: u64,
    n: &mut u64,
    heard: &impl Deliver,
) -> bool {
    let ms = u.ms();
    let piece = stem(*n);
    *n = n.saturating_add(1);
    let Some(transcript) = transcribe(p.side, p.recognizer, p.lang, &u.samples, ms, p.debug) else {
        // Dropped: if it was the rest of a cut phrase, the cut piece must not wait for it.
        return heard.ended(p.side);
    };
    if let Some(ring) = p.ring {
        // A run told to stop still parks what it drains, and deletes nothing to make room: pruning the shared
        // ring is the next capture's alone.
        if p.stop.is_some_and(Stop::is_set) {
            ring.append_only();
        }
        ring.keep(p.side, &piece, u, start, &transcript);
    }
    heard.heard(Heard {
        side: p.side,
        t0_ms: start.saturating_add(u.start_ms),
        t1_ms: start.saturating_add(u.end_ms),
        ms,
        cut: u.cut,
        contour: u.intonation.contour,
        transcript,
        piece,
    })
}

/// One utterance's samples (`ms` long) through the shared recognizer, with the [`recognize`] retry.
///
/// A retry, and a piece that will read `<voice N.Ns>`, are said on stderr by side and length (`notes`); a failed
/// recognition is said and gives `None`. Never says content there. With `debug`, the passes (raw text, language,
/// the filter's verdict) are one [`DEBUG_MARKER`] line on stdout ([`debug_line`]).
pub fn transcribe<R: Recognizer + ?Sized>(
    side: Side,
    recognizer: &Mutex<R>,
    lang: &str,
    samples: &[i16],
    ms: u64,
    debug: bool,
) -> Option<Transcript> {
    // The recognizer's lock ends with this statement, before anything is said.
    let (result, seen) = Passes {
        rec: &mut *recognizer.lock().unwrap_or_else(PoisonError::into_inner),
        seen: debug.then(Vec::new),
    }
    .run(&to_f32(samples), ms, lang);
    if let Some(seen) = seen.filter(|s| !s.is_empty()) {
        // A debug line the tray does not take is no reason to drop the piece: the phrase's own line is what counts.
        let _said = crate::print(&format!("{}\n", debug_line(side, ms, &seen)));
    }
    match result {
        Ok((t, retry)) => {
            for line in notes(&t, ms, retry) {
                say(side, &line);
            }
            Some(t)
        }
        Err(e) => {
            say(side, &format!("{ms} ms utterance dropped: {e}"));
            None
        }
    }
}

/// The stderr lines for one recognized utterance: lengths, rules and outcomes, never content.
///
/// A piece that will read `<voice N.Ns>` gets one line, `unread N.N s: <rule>`, with the outcome of the retry with
/// language detection when one ran (2026-11-24, E2: four pieces lost as `<voice>` and stderr said nothing). A retry
/// the piece did not end unread after gives `retry <ms> ms: <outcome>`, as before.
fn notes(t: &Transcript, ms: u64, retry: Option<Retry>) -> Vec<String> {
    let unread = assemble::piece(t, 0, ms)
        .unread
        .map(|u| u.why)
        .filter(|w| w.heard_voice());
    // The retry with language detection is said on the unread line when there is one: one line, one piece.
    let folded = retry.filter(|r| r.detects() && unread.is_some());
    let mut lines = Vec::new();
    if let Some(r) = retry.filter(|_| folded.is_none()) {
        lines.push(format!("retry {ms} ms: {}", r.as_str()));
    }
    if let Some(why) = unread {
        let tail = folded.map_or_else(String::new, |r| format!(", {}", r.as_str()));
        lines.push(format!(
            "unread {} s: {}{tail}",
            assemble::tenths(ms),
            why.as_str()
        ));
    }
    lines
}

/// What starts the debug line of a recognized piece on `--ears`' stdout. The tray keeps it in its side log, beside
/// the phrases' lines, and sends nothing of it on.
pub const DEBUG_MARKER: &str = "TULPA-EARS-DEBUG ";

/// The recognizer as [`recognize`] uses it, keeping each pass's language and transcript when `seen` is `Some`.
struct Passes<'a, R: ?Sized> {
    rec: &'a mut R,
    seen: Option<Vec<Pass>>,
}

/// One pass of the recognizer: its language and what it gave.
type Pass = (String, Transcript);

/// What [`recognize`] gives.
type Recognized = Result<(Transcript, Option<Retry>), RecognizeError>;

impl<R: Recognizer + ?Sized> Passes<'_, R> {
    /// [`recognize`] through these passes: its result, and the passes kept.
    fn run(mut self, samples: &[f32], ms: u64, lang: &str) -> (Recognized, Option<Vec<Pass>>) {
        (recognize(&mut self, samples, ms, lang), self.seen)
    }
}

impl<R: Recognizer + ?Sized> Recognizer for Passes<'_, R> {
    fn transcribe(&mut self, samples: &[f32], lang: &str) -> Result<Transcript, RecognizeError> {
        let t = self.rec.transcribe(samples, lang)?;
        if let Some(seen) = &mut self.seen {
            seen.push((lang.to_owned(), t.clone()));
        }
        Ok(t)
    }
}

/// One piece's [`DEBUG_MARKER`] line: side, length, then each pass's language, the filter's verdict (`read`, or
/// `unread:<rule>`), how many credit-leak lines it dropped (and the flagged ones, [`assemble::flag_fields`]) and whisper's raw text (quoted, escapes as Rust's), passes
/// separated by ` | `. The verdict is [`assemble::piece`]'s, the one the line is made by.
fn debug_line(side: Side, ms: u64, passes: &[Pass]) -> String {
    let passes: Vec<String> = passes
        .iter()
        .map(|(lang, t)| {
            let p = assemble::piece(t, 0, ms);
            let verdict = p.unread.map_or_else(
                || "read".to_owned(),
                |u| format!("unread:{}", u.why.as_str()),
            );
            format!(
                "lang={lang} verdict={verdict} leak-lines={}{} raw={:?}",
                p.leaks,
                assemble::flag_fields(&p.flagged),
                t.text()
            )
        })
        .collect();
    format!(
        "{DEBUG_MARKER}[{}] {} s: {}",
        side.as_str(),
        assemble::tenths(ms),
        passes.join(" | ")
    )
}

/// One line to stderr: `tulpa-hear: <side>: <msg>`.
pub(crate) fn say(side: Side, msg: &str) {
    crate::warn(&format!("tulpa-hear: {}: {msg}", side.as_str()));
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::indexing_slicing,
        reason = "test signal generation on small, fixed values"
    )]

    use std::path::{Path, PathBuf};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::assemble::{Assembler, HOLD_MAX_MS, piece};
    use crate::beat::Beating;
    use crate::close::close_asked;
    use crate::recognize::{RecognizeError, Token};
    use crate::testkit::{speakers, temp_state};

    /// Answers from a script, one entry per call (empty once the script runs out), and counts the calls.
    #[derive(Default)]
    struct Fake {
        script: Vec<Transcript>,
        calls: Vec<usize>,
    }

    impl Recognizer for Fake {
        fn transcribe(
            &mut self,
            samples: &[f32],
            _lang: &str,
        ) -> Result<Transcript, RecognizeError> {
            self.calls.push(samples.len());
            Ok(if self.script.is_empty() {
                Transcript::default()
            } else {
                self.script.remove(0)
            })
        }
    }

    fn said(words: &str) -> Transcript {
        Transcript::new(vec![Token::new(format!(" {words}"), 0, 500, 0.9)], false)
    }

    /// A 200 Hz tone, loud: speech to the energy gate.
    fn speech(secs: f32) -> Vec<i16> {
        let n = (secs * 16_000.0) as usize;
        (0..n)
            .map(|i| ((i as f32 * 200.0 * std::f32::consts::TAU / 16_000.0).sin() * 8000.0) as i16)
            .collect()
    }

    /// A quiet room: low, non-zero noise.
    fn quiet(secs: f32) -> Vec<i16> {
        let n = (secs * 16_000.0) as usize;
        (0..n).map(|i| if i % 2 == 0 { 40 } else { -40 }).collect()
    }

    /// 0.1 s chunks, as the capture sends them.
    fn chunks(audio: &[i16]) -> Vec<Event> {
        audio
            .chunks(1600)
            .map(|c| Event::Audio(c.to_vec()))
            .collect()
    }

    const START: u64 = 1_700_000_000_000;

    /// Every path under `dir`, sorted.
    fn listing(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = e.path();
                if path.is_dir() {
                    todo.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }

    fn drive<I: IntoIterator<Item = Event>>(
        side: Side,
        close: Option<PathBuf>,
        rec: &Mutex<Fake>,
        events: I,
    ) -> Vec<Heard> {
        let (tx, rx) = tulpa_stop::channel();
        let p = Pipeline::new(side, START, "ru", close, rec);
        run_with(&p, CloseMode::Take, events, &tx);
        drop(tx);
        std::iter::from_fn(|| rx.try_recv()).collect()
    }

    /// The live path's ring: each recognized piece in it, named from the stream's start and counting up, as
    /// the script's segmenter names its files.
    #[test]
    fn the_live_side_rings_its_pieces() -> std::io::Result<()> {
        let state = temp_state("ring");
        let ring_dir = state.join("ring");
        let ring = Ring::open(&ring_dir, 64 * 1024 * 1024, nix::unistd::getuid().as_raw())?;
        let rec = Mutex::new(Fake {
            script: vec![said("раз"), said("два")],
            ..Fake::default()
        });
        let mut audio = quiet(1.0);
        audio.extend(speech(1.5));
        audio.extend(quiet(3.0));
        audio.extend(speech(1.5));
        audio.extend(quiet(3.0));
        let (tx, rx) = tulpa_stop::channel();
        let p = Pipeline::new(Side::Them, START, "ru", None, &rec).with_ring(&ring);
        run_with(&p, CloseMode::Take, chunks(&audio), &tx);
        drop(tx);
        let heard: Vec<Heard> = std::iter::from_fn(|| rx.try_recv()).collect();
        let names: Vec<&str> = heard.iter().map(|h| h.piece.as_str()).collect();
        assert_eq!(
            names,
            [format!("c{START}"), format!("c{}", START + 1)],
            "{heard:?}"
        );
        let mut files: Vec<String> = std::fs::read_dir(&ring_dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        let one = |n: u64, ext: &str| format!("them-c{n}.{ext}");
        assert_eq!(
            files,
            [
                one(START, "json"),
                one(START, "wav"),
                one(START, "whisper.json"),
                one(START + 1, "json"),
                one(START + 1, "wav"),
                one(START + 1, "whisper.json"),
            ]
        );
        std::fs::remove_dir_all(&state)
    }

    /// Fable F1, through the pipeline as `ears::run` wires it: the tap plays a phrase, then goes silent with no
    /// callbacks. The phrase comes out during the silence (the segmenter is given it), not when the sound comes
    /// back; and the next phrase's `t0` keeps the wall clock: it follows the first by about the silence, not by the
    /// few ms the samples alone would say.
    #[test]
    fn a_silent_tap_closes_its_phrase_and_keeps_the_clock() {
        let rec = Mutex::new(Fake {
            script: vec![said("раз"), said("два")],
            ..Fake::default()
        });
        let (tx, rx) = mpsc::channel();
        let (to, events) = mpsc::channel();
        let (heard_tx, heard) = tulpa_stop::channel::<Heard>();
        let token = Arc::new(());
        let reader = Arc::downgrade(&token);
        let beating = Beating::new(rx, Side::Them, None)
            .with_every(Duration::from_millis(50), mpsc::channel().0);
        std::thread::scope(|s| {
            s.spawn(move || beating.forward(&to, reader));
            let rec = &rec;
            s.spawn(move || {
                let p = Pipeline::new(Side::Them, START, "ru", None, rec).with_clock(crate::now_ms);
                run_with(&p, CloseMode::Watch, events, &heard_tx);
                drop(token);
            });
            let _sent = tx.send(speakers());
            // The room first: the segmenter's warmup names its floor.
            let mut audio = quiet(1.0);
            audio.extend(speech(1.0));
            for ev in chunks(&audio) {
                let _sent = tx.send(ev);
            }
            let ended = Instant::now();
            // A guard against a wedge, not a timing claim: the phrase closes about 2 s into the silence.
            let first = heard.recv_until(&[], Instant::now().checked_add(Duration::from_secs(60)));
            let silent = ended.elapsed();
            // At the pace a capture sends it: audio never comes ahead of its time, which the anchor relies on.
            for ev in chunks(&speech(1.0)) {
                let _sent = tx.send(ev);
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(tx);
            let second = heard.recv_until(&[], Instant::now().checked_add(Duration::from_secs(60)));
            let two = matches!(
                (&first, &second),
                (tulpa_stop::Got::Item(_), tulpa_stop::Got::Item(_))
            );
            assert!(two, "two phrases, the first during the silence");
            let (tulpa_stop::Got::Item(first), tulpa_stop::Got::Item(second)) = (first, second)
            else {
                return;
            };
            let apart = second.t0_ms.saturating_sub(first.t1_ms);
            let silent = u64::try_from(silent.as_millis()).unwrap_or(u64::MAX);
            // Less the pre-roll and the post-roll (0.35 s) and some slack for the fill's 200 ms grain.
            assert!(
                apart + 1000 >= silent,
                "t0 apart {apart} ms, the silence {silent} ms"
            );
            // From below only: a fill that came twice is not seen here (the anchor keeps the earliest estimate, which
            // takes the extra silence back out); `a_gap_in_the_tap_is_passed_on_as_silence` bounds the fill itself.
        });
    }

    /// The tray's `.close` mid-utterance closes it there, long before the 2 s pause would, and the segmenter is not
    /// re-warmed: the speech right after the ask is its own utterance, starting at once (a restarted segmenter would
    /// first spend its 1 s warmup classifying nothing).
    #[test]
    fn a_close_marker_ends_the_utterance_and_the_floor_is_kept() {
        let state = temp_state("close");
        let marker = state.join("me").join(".close");
        // 2 s room, then 6 s of unbroken speech; the tray asks at 5 s (3 s into the speech); then 3 s of room.
        let mut audio = quiet(2.0);
        audio.extend(speech(6.0));
        audio.extend(quiet(3.0));
        let ask_at = 50; // chunk index: 5.0 s
        let events = chunks(&audio).into_iter().enumerate().map(|(i, ev)| {
            if i == ask_at {
                std::fs::write(&marker, b"").ok();
            }
            ev
        });
        let rec = Mutex::new(Fake::default());
        let heard = drive(Side::Me, Some(marker.clone()), &rec, events);
        assert!(!marker.exists(), "the ask is removed");
        assert_eq!(heard.len(), 2, "{heard:?}");
        let (first, second) = (&heard[0], &heard[1]);
        // Closed at the ask's chunk (5.0–5.1 s), not at the max or the pause.
        assert!(
            (START + 4_900..=START + 5_200).contains(&first.t1_ms),
            "closed at the ask: {first:?}"
        );
        // The speech after the ask starts its own utterance right away: no warmup gap.
        assert!(
            (START + 5_000..=START + 5_300).contains(&second.t0_ms),
            "no re-warm: {second:?}"
        );
        assert!(second.t1_ms >= START + 8_000, "{second:?}");
        assert_eq!(listing(&state), vec![state.join("me")], "nothing written");
        std::fs::remove_dir_all(&state).ok();
    }

    /// The clock of [`a_close_asked_while_whisper_runs_cuts_where_it_was_asked`]: it follows the audio as it is
    /// read, until whisper holds the thread ([`HELD`]); the chunks queued meanwhile are read late.
    static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(START);
    static HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn clock_now() -> u64 {
        CLOCK.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Reads every piece as one word; at its first pass (whisper holds the thread for `held_ms`) the tray asks for
    /// the close at `ask_ms` into the stream: the marker is written then, with that time.
    struct AskDuring {
        marker: PathBuf,
        ask_ms: u64,
        held_ms: u64,
    }

    impl Recognizer for AskDuring {
        fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
            use std::sync::atomic::Ordering::SeqCst;
            if !HELD.swap(true, SeqCst) {
                CLOCK.store(START.saturating_add(self.held_ms), SeqCst);
                let at = Duration::from_millis(START.saturating_add(self.ask_ms));
                let at = std::time::UNIX_EPOCH
                    .checked_add(at)
                    .unwrap_or(std::time::UNIX_EPOCH);
                let made = std::fs::File::create(&self.marker).and_then(|f| f.set_modified(at));
                assert!(made.is_ok(), "{made:?}");
            }
            Ok(said("да"))
        }
    }

    /// The tray asks for the close while whisper holds "me"'s thread: the audio heard meanwhile is queued, and the
    /// marker is there at the first queued chunk. The cut lands where the ask was in the stream, not at that chunk:
    /// the speech before the ask and the speech after it are two pieces.
    #[test]
    fn a_close_asked_while_whisper_runs_cuts_where_it_was_asked() {
        use std::sync::atomic::Ordering::SeqCst;
        let state = temp_state("close-held");
        let marker = state.join("me").join(".close");
        // A at 1.0–2.5 s (closed by its pause at ~4.5 s: whisper runs), then B 5.0–6.0 s, the ask at 6.0 s, C 6.0–7.0
        // s straight on, then room. Whisper holds the thread until 10 s.
        let mut audio = quiet(1.0);
        audio.extend(speech(1.5));
        audio.extend(quiet(2.5));
        audio.extend(speech(1.0));
        audio.extend(speech(1.0));
        audio.extend(quiet(3.0));
        let events = chunks(&audio).into_iter().enumerate().map(|(i, ev)| {
            if !HELD.load(SeqCst) {
                CLOCK.store(START + (i as u64 + 1) * 100, SeqCst);
            }
            ev
        });
        let rec = Mutex::new(AskDuring {
            marker: marker.clone(),
            ask_ms: 6_000,
            held_ms: 10_000,
        });
        let (tx, rx) = tulpa_stop::channel();
        let p =
            Pipeline::new(Side::Me, START, "ru", Some(marker.clone()), &rec).with_clock(clock_now);
        run_with(&p, CloseMode::Take, events, &tx);
        drop(tx);
        let heard: Vec<Heard> = std::iter::from_fn(|| rx.try_recv()).collect();
        assert!(!marker.exists(), "the ask is taken");
        let spans: Vec<(u64, u64)> = heard
            .iter()
            .map(|h| (h.t0_ms - START, h.t1_ms - START))
            .collect();
        assert_eq!(heard.len(), 3, "A, B and C: {spans:?}");
        assert!(
            spans[1].1.abs_diff(6_000) <= 200,
            "B ends at the ask: {spans:?}"
        );
        assert!(
            spans[2].0.abs_diff(6_000) <= 300,
            "C starts at the ask: {spans:?}"
        );
        std::fs::remove_dir_all(&state).ok();
    }

    /// A close asked while "me" is quiet (the voice was echo-cancelled, nothing in progress) cuts nothing, and the
    /// owner's next phrase, later, comes whole: a marker kept until an utterance shows up would cut it at its start.
    #[test]
    fn a_close_asked_in_quiet_leaves_the_next_phrase_whole() {
        let state = temp_state("close-quiet");
        let marker = state.join("me").join(".close");
        let mut audio = quiet(3.0);
        audio.extend(speech(1.5)); // 3.0–4.5 s
        audio.extend(quiet(3.0));
        let ask_at = 20; // 2.0 s: quiet
        let events = chunks(&audio).into_iter().enumerate().map(|(i, ev)| {
            if i == ask_at {
                std::fs::write(&marker, b"").ok();
            }
            ev
        });
        let rec = Mutex::new(Fake::default());
        let heard = drive(Side::Me, Some(marker.clone()), &rec, events);
        assert!(!marker.exists(), "the ask is taken");
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].t0_ms.abs_diff(START + 3_000) <= 250, "{heard:?}");
        assert!(heard[0].t1_ms.abs_diff(START + 4_500) <= 250, "{heard:?}");
        std::fs::remove_dir_all(&state).ok();
    }

    /// Drives `hear::run` on "me" with a clock the events set: each event comes with the clock at its reading, and
    /// a marker can be written with its mtime (ms after [`START`]; `u64::MAX`: before 1970) just before an event is
    /// read. Returns the pieces' spans, ms after
    /// [`START`].
    fn run_clocked(
        clock: &'static std::sync::atomic::AtomicU64,
        now: fn() -> u64,
        tag: &str,
        events: Vec<(u64, Option<u64>, Event)>,
    ) -> Vec<(u64, u64)> {
        use std::sync::atomic::Ordering::SeqCst;
        let state = temp_state(tag);
        let marker = state.join("me").join(".close");
        let mark = marker.clone();
        let events = events.into_iter().map(move |(at, ask, ev)| {
            clock.store(START.saturating_add(at), SeqCst);
            if let Some(ask) = ask {
                // `u64::MAX`: a date before 1970, which has no epoch ms.
                let t = if ask == u64::MAX {
                    std::time::UNIX_EPOCH.checked_sub(Duration::from_secs(86_400))
                } else {
                    std::time::UNIX_EPOCH
                        .checked_add(Duration::from_millis(START.saturating_add(ask)))
                };
                let t = t.unwrap_or(std::time::UNIX_EPOCH);
                let made = std::fs::File::create(&mark).and_then(|f| f.set_modified(t));
                assert!(made.is_ok(), "{made:?}");
            }
            ev
        });
        let rec = Mutex::new(Fake::default());
        let (tx, rx) = tulpa_stop::channel::<Heard>();
        let p = Pipeline::new(Side::Me, START, "ru", Some(marker), &rec).with_clock(now);
        run_with(&p, CloseMode::Take, events, &tx);
        drop(tx);
        std::fs::remove_dir_all(&state).ok();
        std::iter::from_fn(|| rx.try_recv())
            .map(|h| (h.t0_ms.saturating_sub(START), h.t1_ms.saturating_sub(START)))
            .collect()
    }

    /// #0497 (4): the tap's first sentence is heard whole. Something is already playing when the tap opens, so
    /// its very first audio is speech: on "them" no first second is spent settling a floor (the tap is silent, or
    /// zeros, when nothing plays: there is no room to learn). On "me" the same audio still warms up: the room's
    /// first second names its floor, as before.
    #[test]
    fn the_taps_first_sentence_is_heard_from_its_first_frame() {
        let heard = |side: Side| {
            let rec = Mutex::new(Fake {
                script: vec![said("раз")],
                ..Fake::default()
            });
            let (tx, rx) = tulpa_stop::channel::<Heard>();
            let p = Pipeline::new(side, START, "ru", None, &rec);
            let mut audio = speech(1.5);
            audio.extend(quiet(3.0));
            let mut events = vec![speakers()];
            events.extend(chunks(&audio));
            run_with(&p, CloseMode::Watch, events, &tx);
            drop(tx);
            let mut got = Vec::new();
            while let tulpa_stop::Got::Item(h) = rx.recv_until(&[], None) {
                got.push(h.ms);
            }
            got
        };
        let them = heard(Side::Them);
        assert!(
            matches!(them.as_slice(), [ms] if (1_400..=2_000).contains(ms)),
            "them: the whole 1.5 s phrase: {them:?}"
        );
        assert_eq!(
            heard(Side::Me),
            [0_u64; 0],
            "me: the first second is warmup"
        );
    }

    /// Fable F7: a pipeline that falls behind says so, with what was dropped; a second or two of whisper does not.
    #[test]
    fn a_long_drift_is_said_with_the_drops() {
        assert_eq!(behind_line(4_999, 0), None);
        assert_eq!(
            behind_line(12_000, 30_000).as_deref(),
            Some("warning: reading 12000 ms behind the audio; 30000 ms dropped so far")
        );
    }

    /// The events that end the current stream: it failed, the follower moved to another device (the old stream is
    /// dropped first), there is no device at all, or its timeline broke (a stall past the fill's bound).
    fn stream_ends() -> [Event; 4] {
        [
            Event::StreamError("stopped".to_owned()),
            Event::Switched {
                from: Some("Built-in Mic".to_owned()),
                to: "USB Mic".to_owned(),
            },
            Event::NoInput,
            Event::Lost { ms: 31_000 },
        ]
    }

    /// `audio` as events read in real time from `from_ms` (each chunk when its end is heard).
    fn live(from_ms: u64, audio: &[i16]) -> Vec<(u64, Option<u64>, Event)> {
        chunks(audio)
            .into_iter()
            .enumerate()
            .map(|(i, ev)| {
                let heard = (i as u64).saturating_add(1).saturating_mul(100);
                (from_ms.saturating_add(heard), None, ev)
            })
            .collect()
    }

    /// A close still waiting for its point in the stream when the stream ends (it failed, the device switched, or
    /// went away) is dropped with that stream: it does not cut the first phrase after the reopen.
    #[test]
    fn a_close_waiting_when_a_stream_ends_does_not_cut_after_the_reopen() {
        static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(START);
        fn now() -> u64 {
            CLOCK.load(std::sync::atomic::Ordering::SeqCst)
        }
        for end in stream_ends() {
            // A phrase from 1.0 s, still going at 2.5 s; its last chunk is read late (at 6.0 s), when the tray's
            // close, asked at 4.0 s, is there: it waits for the stream to reach 4.0 s. The stream ends; a stream is
            // back at 20 s with a 3 s phrase.
            let mut first = quiet(1.0);
            first.extend(speech(1.5));
            let mut events = live(0, &first);
            if let Some(last) = events.last_mut() {
                *last = (6_000, Some(4_000), last.2.clone());
            }
            events.push((6_000, None, end.clone()));
            let mut second = speech(3.0);
            second.extend(quiet(3.0));
            events.extend(live(20_000, &second));
            let spans = run_clocked(&CLOCK, now, "close-end", events);
            assert_eq!(
                spans.len(),
                2,
                "{end:?}: the phrase at the end, the phrase after it whole: {spans:?}"
            );
        }
    }

    /// A stream that ends (failed, switched, gone) and comes back after a gap is stamped where its audio is: the
    /// gap is not folded away.
    #[test]
    fn after_a_stream_ends_and_a_gap_the_times_follow_the_audio() {
        static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(START);
        fn now() -> u64 {
            CLOCK.load(std::sync::atomic::Ordering::SeqCst)
        }
        for end in stream_ends() {
            // A phrase at 1.0–2.0 s (after the segmenter's 1 s warm-up), the stream ends at 2.0 s, and one is back
            // at 4.0 s: room, then speech at 5.0 s.
            let mut first = quiet(1.0);
            first.extend(speech(1.0));
            let mut events = live(0, &first);
            events.push((2_000, None, end.clone()));
            let mut second = quiet(1.0);
            second.extend(speech(1.5));
            second.extend(quiet(3.0));
            events.extend(live(4_000, &second));
            let spans = run_clocked(&CLOCK, now, "end-gap", events);
            assert_eq!(spans.len(), 2, "{end:?}: {spans:?}");
            assert!(
                spans[1].0.abs_diff(5_000) <= 250,
                "{end:?}: speech at +5.0 s: {spans:?}"
            );
            assert!(spans[1].1.abs_diff(6_500) <= 250, "{end:?}: {spans:?}");
        }
    }

    /// A marker whose mtime is ahead of the clock (a clock step, or a skewed writer) asks for the close now, not
    /// when the stream would reach that time.
    #[test]
    fn a_close_dated_in_the_future_cuts_now() {
        static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(START);
        fn now() -> u64 {
            CLOCK.load(std::sync::atomic::Ordering::SeqCst)
        }
        let mut audio = quiet(2.0);
        audio.extend(speech(6.0));
        audio.extend(quiet(3.0));
        let mut events = live(0, &audio);
        // Asked when the 5.0 s chunk is read, dated a minute ahead.
        if let Some(e) = events.get_mut(50) {
            e.1 = Some(65_000);
        }
        let spans = run_clocked(&CLOCK, now, "close-future", events);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(
            spans[0].1.abs_diff(5_100) <= 200,
            "cut at the ask: {spans:?}"
        );
    }

    /// A marker in a dir this process may not search cannot be stat'ed: that is said (once), not skipped in silence
    /// at every chunk.
    #[test]
    fn a_marker_that_cannot_be_read_is_said() {
        use std::os::unix::fs::PermissionsExt as _;
        let state = temp_state("close-unreadable");
        let me = state.join("me");
        let marker = me.join(".close");
        assert!(std::fs::write(&marker, b"").is_ok());
        let shut = std::fs::set_permissions(&me, std::fs::Permissions::from_mode(0o000));
        assert!(shut.is_ok(), "{shut:?}");
        let rec = Mutex::new(Fake::default());
        let p = Pipeline::new(Side::Me, START, "ru", Some(marker), &rec);
        let mut said = false;
        let asked = close_asked(&p, &mut said, None);
        let _opened = std::fs::set_permissions(&me, std::fs::Permissions::from_mode(0o700));
        std::fs::remove_dir_all(&state).ok();
        assert_eq!(asked, None);
        assert!(said, "the unreadable marker is said");
    }

    /// A stream that ends (it failed, the device switched, or went away) ends the phrase in progress as the
    /// capture's end does: it becomes a line now, not never (the capture goes on with another stream or none, and
    /// the tray's Pause is a SIGKILL, which writes nothing).
    #[test]
    fn a_stream_that_ends_closes_the_phrase_in_progress() {
        for end in stream_ends() {
            let mut audio = quiet(1.0);
            audio.extend(speech(1.5));
            let rec = Mutex::new(Fake {
                script: vec![said("раз")],
                ..Fake::default()
            });
            let (ev_tx, ev_rx) = mpsc::channel();
            let (tx, rx) = tulpa_stop::channel::<Heard>();
            std::thread::scope(|s| {
                let side = s.spawn(|| {
                    let p = Pipeline::new(Side::Them, START, "ru", None, &rec);
                    run_with(&p, CloseMode::Take, ev_rx, &tx);
                });
                for ev in chunks(&audio) {
                    assert!(ev_tx.send(ev).is_ok());
                }
                assert!(ev_tx.send(end.clone()).is_ok());
                // A guard against a wedge, not a timing claim: under a loaded parallel run the phrase took up to 6 s.
                let got = rx.recv_until(&[], Instant::now().checked_add(Duration::from_secs(60)));
                drop(ev_tx);
                let _joined = side.join();
                let got = match got {
                    tulpa_stop::Got::Item(h) => Some(h.transcript.text()),
                    _ => None,
                };
                assert_eq!(
                    got.as_deref(),
                    Some("раз"),
                    "{end:?}: the phrase, before the capture ends"
                );
            });
        }
    }

    /// A marker whose date cannot be read (here: before 1970) is taken as asked now, never as asked at 0: while
    /// whisper holds the thread, 0 would cut the queued audio from before the ask.
    #[test]
    fn a_close_with_an_unreadable_date_is_asked_now() {
        static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(START);
        fn now() -> u64 {
            CLOCK.load(std::sync::atomic::Ordering::SeqCst)
        }
        // Speech 1.0–5.0 s; the 2.0 s chunk is read late, at 3.5 s, and the marker is there, dated before 1970.
        let mut audio = quiet(1.0);
        audio.extend(speech(4.0));
        audio.extend(quiet(3.0));
        let mut events = live(0, &audio);
        if let Some(e) = events.get_mut(19) {
            *e = (3_500, Some(u64::MAX), e.2.clone());
        }
        let spans = run_clocked(&CLOCK, now, "close-undated", events);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(
            spans[0].1.abs_diff(3_500) <= 200,
            "cut where it was taken: {spans:?}"
        );
    }

    /// The shadow's watch: the marker closes the utterance at the ask as the live path's does, but it stays where
    /// it is (the live path takes it), the state dir is not written, and a marker that stays asks only once.
    #[test]
    fn a_watched_marker_closes_once_and_is_left_alone() {
        let state = temp_state("watch");
        let marker = state.join("me").join(".close");
        let mut audio = quiet(2.0);
        audio.extend(speech(6.0));
        audio.extend(quiet(3.0));
        let ask_at = 50;
        let events = chunks(&audio).into_iter().enumerate().map(|(i, ev)| {
            if i == ask_at {
                std::fs::write(&marker, b"").ok();
            }
            ev
        });
        let rec = Mutex::new(Fake::default());
        let (tx, rx) = tulpa_stop::channel();
        let p = Pipeline::new(Side::Me, START, "ru", Some(marker.clone()), &rec);
        run_with(&p, CloseMode::Watch, events, &tx);
        drop(tx);
        let heard: Vec<Heard> = std::iter::from_fn(|| rx.try_recv()).collect();
        assert!(marker.exists(), "only looked at");
        assert_eq!(
            heard.len(),
            2,
            "one close for a marker that stays: {heard:?}"
        );
        assert!(
            (START + 4_900..=START + 5_200).contains(&heard[0].t1_ms),
            "closed at the ask: {:?}",
            heard[0]
        );
        assert_eq!(
            listing(&state),
            vec![state.join("me"), marker.clone()],
            "nothing else"
        );
        std::fs::remove_dir_all(&state).ok();
    }

    /// The "them" side has no marker: a `.close` in `me/` doesn't touch it.
    #[test]
    fn them_ignores_the_me_marker() {
        let state = temp_state("them");
        let marker = state.join("me").join(".close");
        std::fs::write(&marker, b"").ok();
        let mut audio = quiet(2.0);
        audio.extend(speech(3.0));
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake::default());
        let heard = drive(Side::Them, None, &rec, chunks(&audio));
        assert_eq!(heard.len(), 1);
        assert!(marker.exists(), "not ours to remove");
        std::fs::remove_dir_all(&state).ok();
    }

    /// One line per utterance, `t0`/`t1` from the fixed start plus the utterance's offsets, and nothing written
    /// anywhere (the state dir holds what it held).
    #[test]
    fn one_line_per_utterance_with_wall_clock_spans() {
        let state = temp_state("lines");
        let before = listing(&state);
        let mut audio = quiet(2.0);
        audio.extend(speech(1.5)); // 2.0–3.5 s
        audio.extend(quiet(3.0));
        audio.extend(speech(2.0)); // 6.5–8.5 s
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake {
            script: vec![said("раз"), said("два")],
            ..Fake::default()
        });
        let heard = drive(
            Side::Me,
            Some(state.join("me/.close")),
            &rec,
            chunks(&audio),
        );
        let log_dir = temp_state("lines-log");
        let mut asm = Assembler::new(&log_dir, None, HOLD_MAX_MS);
        let lines: Vec<String> = heard.iter().flat_map(|h| asm.take(h, 3_723_000)).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        for (h, (t0, t1)) in heard.iter().zip([(2_000, 3_500), (6_500, 8_500)]) {
            // pre-roll 0.2 s before the first speech frame, post-roll 0.15 s after the last (frame-rounded)
            assert!(h.t0_ms.abs_diff(START + t0) <= 250, "{h:?}");
            assert!(h.t1_ms.abs_diff(START + t1) <= 250, "{h:?}");
        }
        let (t0, t1) = (heard[0].t0_ms, heard[0].t1_ms);
        assert!(
            lines[0].starts_with(&format!("TULPA-EARS-LINE {t0} {t1} 01:02:03 [me] раз")),
            "{lines:?}"
        );
        assert!(
            lines[0].ends_with(&format!("\t{t0},{},раз", t0 + 500)),
            "{lines:?}"
        );
        assert!(lines[1].contains("[me] два"), "{lines:?}");
        std::fs::remove_dir_all(&log_dir).ok();
        assert_eq!(listing(&state), before, "no file created");
        std::fs::remove_dir_all(&state).ok();
    }

    /// An utterance of at least 2.5 s read as nothing is transcribed exactly once more; still no token at all is
    /// `<no speech N.Ns>`.
    #[test]
    fn a_long_empty_utterance_is_retried_once_and_stays_unread() {
        let mut audio = quiet(2.0);
        audio.extend(speech(3.0));
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake::default());
        let heard = drive(Side::Them, None, &rec, chunks(&audio));
        assert_eq!(heard.len(), 1);
        let calls = rec.lock().map(|r| r.calls.clone()).unwrap_or_default();
        assert_eq!(calls.len(), 2, "one retry: {calls:?}");
        assert_eq!(calls[0], calls[1], "the same audio");
        let h = &heard[0];
        let secs = h.ms as f64 / 1000.0;
        assert_eq!(
            piece(&h.transcript, h.t0_ms, h.ms).text,
            format!("<no speech {secs:.1}s>")
        );
    }

    /// Reads each new piece as whisper's markers only (the deaf signature), and the same audio again as a
    /// subtitle-credit line, as a second pass may.
    #[derive(Default)]
    struct Deaf {
        last: usize,
        calls: usize,
    }

    impl Recognizer for Deaf {
        fn transcribe(&mut self, samples: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
            self.calls = self.calls.saturating_add(1);
            let again = std::mem::replace(&mut self.last, samples.len()) == samples.len();
            Ok(if again {
                said("Продолжение следует...")
            } else {
                Transcript::new(Vec::new(), false).with_specials(2)
            })
        }
    }

    /// A long piece whisper read as its own markers only is not retried: it stands, so a run of them raises
    /// `<deaf-suspect>` even when a second pass would have hallucinated a credit line.
    #[test]
    fn markers_only_pieces_are_kept_and_raise_deaf_suspect() {
        let mut audio = quiet(2.0);
        for secs in [3.0, 3.3, 3.6, 3.9] {
            audio.extend(speech(secs));
            audio.extend(quiet(3.0));
        }
        let rec = Mutex::new(Deaf::default());
        let (tx, rx) = tulpa_stop::channel();
        run_with(
            &Pipeline::new(Side::Me, START, "ru", None, &rec),
            CloseMode::Take,
            chunks(&audio),
            &tx,
        );
        drop(tx);
        let heard: Vec<Heard> = std::iter::from_fn(|| rx.try_recv()).collect();
        assert_eq!(heard.len(), 4, "{heard:?}");
        assert_eq!(rec.lock().map_or(0, |r| r.calls), 4, "no retry");
        let log_dir = temp_state("deaf-log");
        let mut asm = Assembler::new(&log_dir, None, HOLD_MAX_MS).with_deaf_s(10);
        let lines: Vec<String> = heard.iter().flat_map(|h| asm.take(h, 3_723_000)).collect();
        let deaf = lines
            .iter()
            .filter(|l| l.contains("<deaf-suspect markers-only"));
        assert_eq!(deaf.count(), 1, "{lines:?}");
        std::fs::remove_dir_all(&log_dir).ok();
    }

    /// The retry that reads something wins.
    #[test]
    fn a_long_empty_utterance_read_on_retry_is_text() {
        let mut audio = quiet(2.0);
        audio.extend(speech(3.0));
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake {
            script: vec![Transcript::default(), said("вот")],
            ..Fake::default()
        });
        let heard = drive(Side::Me, None, &rec, chunks(&audio));
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].transcript.text(), "вот");
        assert_eq!(rec.lock().map_or(0, |r| r.calls.len()), 2);
    }

    /// Shorter than 2.5 s, or read the first time: no retry.
    #[test]
    fn no_retry_when_short_or_read() {
        let mut audio = quiet(2.0);
        audio.extend(speech(1.5));
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake::default());
        let heard = drive(Side::Me, None, &rec, chunks(&audio));
        assert_eq!(heard.len(), 1);
        assert!(heard[0].transcript.is_empty());
        assert_eq!(rec.lock().map_or(0, |r| r.calls.len()), 1, "short");

        let mut audio = quiet(2.0);
        audio.extend(speech(3.0));
        audio.extend(quiet(3.0));
        let rec = Mutex::new(Fake {
            script: vec![said("да")],
            ..Fake::default()
        });
        let heard = drive(Side::Me, None, &rec, chunks(&audio));
        assert_eq!(heard.len(), 1);
        assert_eq!(rec.lock().map_or(0, |r| r.calls.len()), 1, "read");
    }
}
