//! Both captures fail before the first line: each side's reason reaches stderr.
//!
//! [`ears::run`] says a failed capture on stderr, by side. Both sides can fail at once (the mic's permission and the
//! tap's together): each sends its reason, then its pipeline ends. The test runs itself again as the child, with
//! stderr a pipe, and counts the reasons there.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::ears::{self, Ears, Outcome, Sink};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::recognize::{RecognizeError, Recognizer, Transcript};
use tulpa_stop::Stop;

/// Set in the child's environment: run the ears with two failing captures.
const CHILD: &str = "TULPA_HEAR_EARS_FAIL_CHILD";

/// The stuck-side tests' grace after a failure (the run's own is [`ears::FAILED_GRACE`], 5 s), and their guard
/// against a wedge: a run that never ends runs into it.
const GRACE: std::time::Duration = std::time::Duration::from_millis(300);
const GUARD: std::time::Duration = std::time::Duration::from_secs(30);

struct Never;

impl Recognizer for Never {
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        Err(RecognizeError::Run("not asked".to_owned()))
    }
}

struct Nowhere;

impl Sink for Nowhere {
    fn line(&mut self, _: &str, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

const fn now_ms() -> u64 {
    0
}

/// The child: both captures fail at once; exit 1 when the run says so.
fn child() {
    let dir = std::env::temp_dir().join(format!("tulpa-hear-fail-{}", std::process::id()));
    let ears = Ears::new(
        Arc::new(Mutex::new(Never)),
        "ru".to_owned(),
        dir.join("me/.close"),
        CloseMode::Take,
        now_ms,
    );
    let capture = Arc::new(|side: Side, _: &Sender<Event>, _: &Stop| {
        Err(format!("{} capture refused", side.as_str()))
    });
    let mut asm = Assembler::new(&dir, None, HOLD_MAX_MS);
    let stop = Arc::new(Stop::new());
    let out = ears::run(&ears, &capture, &mut asm, &mut Nowhere, &stop, None);
    let _gone = std::fs::remove_dir_all(&dir);
    assert_eq!(out, Outcome::Failed);
}

#[test]
fn both_failed_captures_are_said() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let exe = std::env::current_exe();
    assert!(exe.is_ok(), "{exe:?}");
    let Ok(exe) = exe else { return };
    let spawned = Command::new(exe)
        .args(["both_failed_captures_are_said", "--exact", "--nocapture"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    assert!(spawned.is_ok(), "{spawned:?}");
    let Ok(mut spawned) = spawned else { return };
    let err = spawned.stderr.take();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut err) = err {
            let _read = err.read_to_string(&mut text);
        }
        let _sent = tx.send(text);
    });
    // A guard against a wedge, not a timing claim: the child fails at once.
    let text = rx.recv_timeout(Duration::from_secs(60));
    let _killed = spawned.kill();
    let _waited = spawned.wait();
    let _joined = reader.join();
    assert!(text.is_ok(), "the child's stderr: {text:?}");
    let text = text.unwrap_or_default();
    for side in ["me", "them"] {
        let said = format!("tulpa-hear: {side}: {side} capture refused");
        assert_eq!(text.matches(&said).count(), 1, "{said}:\n{text}");
    }
}

/// Fable F2: the mic fails while the tap is stuck in its start (`play()` on a pending System Audio Recording grant:
/// it never returns, `stop` or not). With no give-up, as `--ears` runs, the run still ends `Failed` within
/// [`ears::FAILED_GRACE`] instead of waiting for the stuck side forever.
#[test]
fn a_failure_beside_a_stuck_capture_still_ends_the_run() {
    let dir = std::env::temp_dir().join(format!("tulpa-hear-stuck-{}", std::process::id()));
    let ears = Ears::new(
        Arc::new(Mutex::new(Never)),
        "ru".to_owned(),
        dir.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    .with_failed_grace(GRACE);
    // The stuck side's start: it waits on a channel nobody ever sends on (its sender is kept alive below).
    let (_never_tx, never) = mpsc::channel::<()>();
    let never = Mutex::new(never);
    let capture = Arc::new(move |side: Side, _: &Sender<Event>, _: &Stop| {
        if side == Side::Me {
            return Err("pinned mic missing".to_owned());
        }
        let _never = never.lock().map(|rx| rx.recv());
        Ok(())
    });
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let mut asm = Assembler::new(&dir, None, HOLD_MAX_MS);
        let stop = Arc::new(Stop::new());
        let out = ears::run(&ears, &capture, &mut asm, &mut Nowhere, &stop, None);
        let _gone = std::fs::remove_dir_all(&dir);
        let _sent = done_tx.send(out);
    });
    // A guard against a wedge, well past the grace.
    let out = done.recv_timeout(GUARD);
    assert_eq!(out, Ok(Outcome::Failed));
}

/// akira A2: the failure comes after `stop` is already set (a stop first, then the mic fails while the tap is stuck
/// in its start). The grace still bounds the run: it ends `Failed`, not never.
#[test]
fn a_failure_after_the_stop_still_ends_the_run() {
    let dir = std::env::temp_dir().join(format!("tulpa-hear-late-{}", std::process::id()));
    let ears = Ears::new(
        Arc::new(Mutex::new(Never)),
        "ru".to_owned(),
        dir.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    .with_failed_grace(GRACE);
    let (_never_tx, never) = mpsc::channel::<()>();
    let (fail_tx, fail) = mpsc::channel::<()>();
    let (never, fail) = (Mutex::new(never), Mutex::new(fail));
    let capture = Arc::new(move |side: Side, _: &Sender<Event>, _: &Stop| {
        if side == Side::Me {
            let _told = fail.lock().map(|rx| rx.recv());
            return Err("pinned mic missing".to_owned());
        }
        let _never = never.lock().map(|rx| rx.recv());
        Ok(())
    });
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let mut asm = Assembler::new(&dir, None, HOLD_MAX_MS);
        let stop = Arc::new(Stop::new());
        stop.set();
        let out = ears::run(&ears, &capture, &mut asm, &mut Nowhere, &stop, None);
        let _gone = std::fs::remove_dir_all(&dir);
        let _sent = done_tx.send(out);
    });
    // The one settle here: the run takes its stop first (it gives the test no event for that); a failure that came
    // before it would be seen with the stop, which the test above covers.
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(fail_tx.send(()).is_ok());
    // A guard against a wedge, well past the grace.
    let out = done.recv_timeout(GUARD);
    assert_eq!(out, Ok(Outcome::Failed));
}

/// Runs the ears with `capture` and `recognizer` on a thread of its own (no give-up, as `--ears` runs) and returns
/// how the run ended, or `Err` past a guard against a wedge.
fn run_guarded<R, C>(
    tag: &str,
    recognizer: R,
    capture: C,
) -> Result<Outcome, mpsc::RecvTimeoutError>
where
    R: Recognizer + Send + 'static,
    C: ears::Capture + 'static,
{
    let dir = std::env::temp_dir().join(format!("tulpa-hear-{tag}-{}", std::process::id()));
    let ears = Ears::new(
        Arc::new(Mutex::new(recognizer)),
        "ru".to_owned(),
        dir.join("me/.close"),
        CloseMode::Take,
        now_ms,
    );
    let capture = Arc::new(capture);
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let mut asm = Assembler::new(&dir, None, HOLD_MAX_MS);
        let stop = Arc::new(Stop::new());
        let out = ears::run(&ears, &capture, &mut asm, &mut Nowhere, &stop, None);
        let _gone = std::fs::remove_dir_all(&dir);
        let _sent = done_tx.send(out);
    });
    done.recv_timeout(Duration::from_secs(60))
}

/// Fable F5: a capture thread that panics fails the run, as a capture's `Err` does: the other side is stopped and
/// the run ends `Failed`, instead of running half-deaf until something else stops it.
#[test]
fn a_panicking_capture_fails_the_run() {
    let out = run_guarded(
        "panic-capture",
        Never,
        |side: Side, _: &Sender<Event>, stop: &Stop| {
            // The tap's capture panics, on purpose.
            assert!(side != Side::Them, "the tap's capture panicked");
            stop.wait_until(None);
            Ok(())
        },
    );
    assert_eq!(out, Ok(Outcome::Failed));
}

/// A recognizer that panics: the pipeline thread that calls it panics.
struct Panics;

impl Recognizer for Panics {
    #[expect(clippy::panic, reason = "the test's pipeline must panic")]
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        panic!("the recognizer panicked");
    }
}

/// Fable F5: a pipeline thread that panics (here in its recognizer) fails the run. Its capture would go on sending
/// into a closed channel, and the other side would keep the run alive: the run is stopped and ends `Failed`.
#[test]
fn a_panicking_pipeline_fails_the_run() {
    let out = run_guarded(
        "panic-pipeline",
        Panics,
        |side: Side, tx: &Sender<Event>, stop: &Stop| {
            if side == Side::Them {
                let _sent = tx.send(Event::Device {
                    name: "Speakers".to_owned(),
                    rate: 48_000,
                    channels: 2,
                });
                // The room, then a phrase and the silence that closes it: the recognizer is asked.
                let room = (0..16_000).map(|i| if i % 2 == 0 { 40 } else { -40 });
                let tone = (0..16_000).map(|i| {
                    let x = f64::from(i) * 200.0 * std::f64::consts::TAU / 16_000.0;
                    // A test tone: small, fixed values.
                    #[expect(clippy::cast_possible_truncation, reason = "within i16")]
                    let s = (x.sin() * 8000.0) as i16;
                    s
                });
                let audio: Vec<i16> = room
                    .clone()
                    .chain(tone)
                    .chain(room.clone())
                    .chain(room.clone())
                    .chain(room)
                    .collect();
                for c in audio.chunks(1600) {
                    let _sent = tx.send(Event::Audio(c.to_vec()));
                }
            }
            stop.wait_until(None);
            Ok(())
        },
    );
    assert_eq!(out, Ok(Outcome::Failed));
}
