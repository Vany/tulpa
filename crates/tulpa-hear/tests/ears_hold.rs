//! A turn held at a max-length cut is let go by an event, not only by the side's next speech (Fable F4, #0511).
//!
//! The segmenter cuts a phrase that runs past 60 s, and the assembler holds the cut piece for its rest. Every way
//! the rest can end with nothing must let the held piece go: too little speech to make an utterance, no speech at
//! all, the stream's end, a recognition that failed. Each test below is one of those ways, through [`ears::run`]
//! with the capture still open: the line must come while the run goes on (at the run's end it is flushed anyway).
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test signal generation on small, fixed values"
)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::ears::{self, Ears, Outcome, Sink};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::recognize::{RecognizeError, Recognizer, Token, Transcript};
use tulpa_stop::Stop;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tulpa-hear-hold-{tag}-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&d);
    assert!(std::fs::create_dir_all(&d).is_ok());
    d
}

fn speech(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0).round() as usize;
    (0..n)
        .map(|i| ((i as f32 * 200.0 * std::f32::consts::TAU / 16_000.0).sin() * 8000.0) as i16)
        .collect()
}

fn quiet(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0).round() as usize;
    (0..n).map(|i| if i % 2 == 0 { 40 } else { -40 }).collect()
}

/// 1 s of room, then talk (0.9 s bursts, 0.3 s breaths) whose last burst ends 59.5 s into the stream.
///
/// The utterance begins with the first burst, so the segmenter's 60 s cut comes about 60.7 s in, whatever follows
/// here: it cuts at the first quiet frame of its last 2 s, the breath at 59.5 s. What a test appends is the rest.
fn a_minute_of_talk() -> Vec<i16> {
    let mut out = quiet(1.0);
    for _ in 0..48 {
        out.extend(speech(0.9));
        out.extend(quiet(0.3));
    }
    out.extend(speech(0.9));
    out
}

fn now_ms() -> u64 {
    tulpa_hear::now_ms()
}

/// Reads every piece as one word; fails the calls listed in `fails` (0 is the first).
struct Script {
    calls: usize,
    fails: Vec<usize>,
}

impl Recognizer for Script {
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        let n = self.calls;
        self.calls = self.calls.saturating_add(1);
        if self.fails.contains(&n) {
            return Err(RecognizeError::Run("scripted".to_owned()));
        }
        Ok(Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false))
    }
}

/// Tells each line as it is taken.
struct Tell(Sender<String>);

impl Sink for Tell {
    fn line(&mut self, line: &str, _emit_ms: u64) -> std::io::Result<()> {
        let _told = self.0.send(line.to_owned());
        Ok(())
    }
}

/// Runs "me" through the minute of talk, then `rest` (audio, then the events of `then`), and keeps the capture open.
/// The line that came while the capture was still open, if one did; the guard is far above the pipeline's work on
/// the audio, and a held turn that is never let go runs into it.
fn line_while_open(
    tag: &str,
    rest: Vec<i16>,
    then: Vec<Event>,
    fails: Vec<usize>,
) -> Option<String> {
    let (end_tx, end) = mpsc::channel::<()>();
    let end = Mutex::new(end);
    let mut audio = a_minute_of_talk();
    audio.extend(rest);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        let _sent = tx.send(Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        });
        for c in audio.chunks(1600) {
            tx.send(Event::Audio(c.to_vec()))
                .map_err(|e| e.to_string())?;
        }
        for ev in &then {
            tx.send(ev.clone()).map_err(|e| e.to_string())?;
        }
        // Open until the test has looked: the run's end would flush the held turn anyway.
        let _ended = end.lock().unwrap_or_else(PoisonError::into_inner).recv();
        Ok(())
    });
    let state = temp(tag);
    let ears = Ears::new(
        Arc::new(Mutex::new(Script { calls: 0, fails })),
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    // The minute comes at once, not as it is heard: none of it is a backlog to drop.
    .with_backlog(Duration::from_secs(600));
    let stop = Arc::new(Stop::new());
    let (told_tx, told) = mpsc::channel();
    let log = state.clone();
    let running = std::thread::spawn(move || {
        let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
        ears::run(&ears, &capture, &mut asm, &mut Tell(told_tx), &stop, None)
    });
    let line = first_line(&told);
    drop(end_tx);
    let out = running.join().unwrap_or(Outcome::GaveUp);
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    let _gone = std::fs::remove_dir_all(&state);
    line
}

fn first_line(told: &Receiver<String>) -> Option<String> {
    // A guard, not a timing claim: the segmenter's work on the minute takes seconds, more beside other tests.
    told.recv_timeout(Duration::from_secs(120)).ok()
}

fn is_the_held_minute(line: Option<&str>) -> bool {
    line.is_some_and(|l| l.starts_with("TULPA-EARS-LINE ") && l.contains(" [me] "))
}

/// The rest holds 0.3 s of speech: too little for an utterance, so the segment closes with nothing.
#[test]
fn a_rest_with_too_little_speech_lets_the_held_turn_go() {
    let mut rest = quiet(0.3);
    rest.extend(speech(0.3));
    rest.extend(quiet(4.0));
    let line = line_while_open("little", rest, Vec::new(), Vec::new());
    assert!(is_the_held_minute(line.as_deref()), "{line:?}");
}

/// Nothing is said after the cut: no segment even begins, and the pause that would have closed one lets it go.
#[test]
fn silence_after_the_cut_lets_the_held_turn_go() {
    let line = line_while_open("silence", quiet(5.0), Vec::new(), Vec::new());
    assert!(is_the_held_minute(line.as_deref()), "{line:?}");
}

/// The stream ends (here: it fails) with the rest still open and too short to be an utterance.
#[test]
fn a_stream_that_ends_lets_the_held_turn_go() {
    let mut rest = quiet(0.3);
    rest.extend(speech(0.3));
    rest.extend(quiet(0.9));
    let then = vec![Event::StreamError("stopped".to_owned())];
    let line = line_while_open("ends", rest, then, Vec::new());
    assert!(is_the_held_minute(line.as_deref()), "{line:?}");
}

/// The rest is a whole utterance, and its recognition fails: it is dropped, and the held piece must not wait for it.
#[test]
fn a_rest_whose_recognition_fails_lets_the_held_turn_go() {
    let mut rest = quiet(0.3);
    rest.extend(speech(1.5));
    rest.extend(quiet(3.0));
    let line = line_while_open("fails", rest, Vec::new(), vec![1]);
    assert!(is_the_held_minute(line.as_deref()), "{line:?}");
}

/// The control: a rest that is read comes out with the held piece as one line, as before.
#[test]
fn a_rest_that_is_read_ends_the_held_turn_as_before() {
    let mut rest = quiet(0.3);
    rest.extend(speech(1.5));
    rest.extend(quiet(3.0));
    let line = line_while_open("read", rest, Vec::new(), Vec::new());
    assert!(is_the_held_minute(line.as_deref()), "{line:?}");
    assert!(
        line.as_deref()
            .and_then(|l| l.split('\t').next())
            .is_some_and(|text| text.ends_with(" [me] да да")),
        "the cut piece and its rest, one line: {line:?}"
    );
}
