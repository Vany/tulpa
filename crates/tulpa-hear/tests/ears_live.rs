//! `--ears`' one way to stop ([`tulpa_hear::live`]): the end of its stdin and a termination signal are the same
//! stop, and both write what a SIGKILL lost: a held turn and the phrase in progress.
//!
//! Each test holds a turn (a phrase cut at the segmenter's 60 s, its rest still being said) and then stops the run
//! one way. The line must be in the sink when the run returns, the run must end `Ended`, and it must return inside
//! the grace: the tray's KILL comes 7 s after its stop, and a line written after that is lost.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test signal generation on small, fixed values"
)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::ears::{Ears, Outcome, Sink};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::live;
use tulpa_hear::recognize::{RecognizeError, Recognizer, Token, Transcript};
use tulpa_hear::shadow::{Caught, CaughtSlot, Slot};
use tulpa_stop::Stop;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tulpa-hear-live-{tag}-{}", std::process::id()));
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

/// 1 s of room, then talk (0.9 s bursts, 0.3 s breaths) for a little over a minute: the segmenter cuts it at 60 s,
/// and the talk goes on, so the cut piece is held and its rest is a phrase in progress.
fn more_than_a_minute_of_talk() -> Vec<i16> {
    let mut out = quiet(1.0);
    for _ in 0..52 {
        out.extend(speech(0.9));
        out.extend(quiet(0.3));
    }
    out
}

/// Reads every piece as one word, and tells each call.
struct Says(Sender<()>);

impl Recognizer for Says {
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        let _told = self.0.send(());
        Ok(Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false))
    }
}

#[derive(Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Sink for Lines {
    fn line(&mut self, line: &str, _emit_ms: u64) -> std::io::Result<()> {
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line.to_owned());
        }
        Ok(())
    }
}

/// How a run that held a turn ended after `stop_it` stopped it: the outcome, its lines, and how long the run took
/// from the stop to its return. `stuck`: the capture never returns, stop or not (a start blocked on a grant).
fn stopped_with_a_held_turn(
    tag: &str,
    stuck: bool,
    grace: Duration,
    stop_it: impl FnOnce(&CaughtSlot, &Arc<Stop>),
) -> (Outcome, Vec<String>, Duration) {
    let state = temp(tag);
    let (heard_tx, heard) = mpsc::channel();
    let (_never_tx, never) = mpsc::channel::<()>();
    let never = Mutex::new(never);
    let audio = more_than_a_minute_of_talk();
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, stop: &Stop| {
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
        if stuck {
            let _never = never.lock().map(|rx| rx.recv());
        }
        // As the mic and the tap: until the stop, then the capture returns.
        let _stopped = stop.wait_until(None);
        Ok(())
    });
    let ears = Ears::new(
        Arc::new(Mutex::new(Says(heard_tx))),
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Take,
        tulpa_hear::now_ms,
    )
    // The minute comes at once, not as it is heard: none of it is a backlog to drop.
    .with_backlog(Duration::from_secs(600));
    let stop = Arc::new(Stop::new());
    let slot: CaughtSlot = Arc::new(Slot::new());
    let lines = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done) = mpsc::channel();
    let running = {
        let (stop, slot, lines, log) = (
            Arc::clone(&stop),
            Arc::clone(&slot),
            Arc::clone(&lines),
            state.clone(),
        );
        std::thread::spawn(move || {
            let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
            let out = live::run(
                &ears,
                &capture,
                &mut asm,
                &mut Lines(lines),
                &stop,
                &slot,
                grace,
            );
            let _sent = done_tx.send((out, Instant::now()));
        })
    };
    // The cut piece reached the recognizer: it is held now, and its rest is being said. A guard, not a timing claim.
    assert!(
        heard.recv_timeout(Duration::from_secs(120)).is_ok(),
        "the cut piece never reached the recognizer"
    );
    let before = lines.lock().map_or(usize::MAX, |l| l.len());
    assert_eq!(before, 0, "the cut piece is held, not written yet");
    let stopped = Instant::now();
    stop_it(&slot, &stop);
    let ended = done.recv_timeout(Duration::from_secs(120));
    assert!(ended.is_ok(), "the run never returned");
    let (out, at) = ended.unwrap_or((Outcome::GaveUp, stopped));
    if !stuck {
        let _joined = running.join();
    }
    let said = lines.lock().map(|l| l.clone()).unwrap_or_default();
    let _gone = std::fs::remove_dir_all(&state);
    (out, said, at.saturating_duration_since(stopped))
}

fn one_line_of_me(lines: &[String]) -> bool {
    matches!(lines, [l] if l.starts_with("TULPA-EARS-LINE ") && l.contains(" [me] да"))
}

/// The tray died (or closed the pipe): the stdin ends, with no signal. The held turn and the rest in progress are
/// written, the run ends `Ended`, inside the grace.
#[test]
fn the_stdin_ending_stops_the_run_and_writes_the_held_turn() {
    let pipe = std::io::pipe();
    assert!(pipe.is_ok(), "{pipe:?}");
    let Ok((reader, writer)) = pipe else { return };
    let (out, lines, took) =
        stopped_with_a_held_turn("eof", false, live::GRACE, move |slot, stop| {
            let watching = live::watch_eof(reader, Arc::clone(slot), Arc::clone(stop));
            assert!(watching.is_ok(), "{watching:?}");
            // The tray is gone: its end of the pipe closes.
            drop(writer);
        });
    assert_eq!(out, Outcome::Ended);
    assert!(one_line_of_me(&lines), "{lines:?}");
    assert!(took < live::GRACE, "returned {took:?} after the stop");
}

/// SIGTERM, as the signal thread puts it in the slot (`shadow::Signals::spawn`; that thread itself is tested in
/// `shadow.rs`): the same stop, the same line, the same end.
#[test]
fn a_termination_signal_stops_the_run_and_writes_the_held_turn() {
    let (out, lines, took) = stopped_with_a_held_turn("term", false, live::GRACE, |slot, stop| {
        slot.fill(Caught::new("SIGTERM", Instant::now()));
        stop.set();
    });
    assert_eq!(out, Outcome::Ended);
    assert!(one_line_of_me(&lines), "{lines:?}");
    assert!(took < live::GRACE, "returned {took:?} after the stop");
}

/// A side that does not end on the stop (a capture stuck in its start) is given up at the grace: the run returns,
/// it does not wait for the tray's KILL.
#[test]
fn a_side_stuck_at_the_stop_is_given_up_at_the_grace() {
    let grace = Duration::from_millis(500);
    let (out, _lines, took) = stopped_with_a_held_turn("stuck", true, grace, |slot, stop| {
        slot.fill(Caught::new("SIGTERM", Instant::now()));
        stop.set();
    });
    assert_eq!(out, Outcome::GaveUp);
    assert!(took >= grace, "returned {took:?} after the stop");
}

/// The gate's S2: every exit of the run writes what it holds. A side stuck at the stop is given up, and the turn
/// held from before (recognized already, waiting only for its rest) is written before the run returns, not lost
/// with it.
#[test]
fn a_give_up_still_writes_the_held_turn() {
    let grace = Duration::from_millis(500);
    let (out, lines, _took) =
        stopped_with_a_held_turn("give-up-held", true, grace, |slot, stop| {
            slot.fill(Caught::new("SIGTERM", Instant::now()));
            stop.set();
        });
    assert_eq!(out, Outcome::GaveUp);
    assert!(one_line_of_me(&lines), "{lines:?}");
}
