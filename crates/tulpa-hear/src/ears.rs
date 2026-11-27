//! Both sides of the ears in one process, from the captures to the lines: the pipeline `--ears` and `--shadow` share.
//!
//! The differences between them: where the lines go ([`Sink`]), how the tray's `.close` is treated
//! ([`CloseMode`]), and that only `--ears` writes what the capture script wrote beside its lines
//! ([`Ears::with_files`]: the sides' heartbeats and the raw ring).
//!
//! [`run`] starts, per side, the capture (a [`Capture`]: the mic's or the tap's in the binary, a fake in tests) and
//! its [`hear`](crate::hear) pipeline on threads of their own, sharing one recognizer; the calling thread assembles
//! the [`Heard`](crate::hear::Heard)s into lines ([`Assembler`]) and hands each to the sink. It returns when every
//! pipeline has ended, or — once `give_up` says so, or [`FAILED_GRACE`] after a failure — at once, leaving the threads
//! to the process's exit.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tulpa_stop::{Got, Stop};

use crate::assemble::Assembler;
use crate::beat::{self, Beating};
use crate::event::Event;
use crate::hear::{self, CloseMode, Pipeline, Side};
use crate::queue;
use crate::recognize::Recognizer;
use crate::ring::Ring;

/// Where the lines go.
pub trait Sink {
    /// Takes one `TULPA-EARS-LINE` (no newline), assembled at `emit_ms` (epoch ms).
    ///
    /// # Errors
    ///
    /// The write's; the run then stops.
    fn line(&mut self, line: &str, emit_ms: u64) -> io::Result<()>;
}

/// `--ears`' sink: each line on stdout, as it is.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Stdout;

impl Sink for Stdout {
    fn line(&mut self, line: &str, _emit_ms: u64) -> io::Result<()> {
        crate::print(&format!("{line}\n"))
    }
}

/// One side's capture: sends its [`Event`]s until `stop` is set, then returns; `Err` (said on stderr by side) when
/// it cannot go on.
pub trait Capture: Fn(Side, &Sender<Event>, &Stop) -> Result<(), String> + Send + Sync {}

impl<F> Capture for F where F: Fn(Side, &Sender<Event>, &Stop) -> Result<(), String> + Send + Sync {}

/// What the two sides share.
#[derive(Debug)]
#[non_exhaustive]
pub struct Ears<R> {
    /// The recognizer, one for both sides.
    pub recognizer: Arc<Mutex<R>>,
    /// Whisper's language.
    pub lang: String,
    /// The tray's `.close` marker for "me" (`<EARS_STATE>/me/.close`).
    pub close: PathBuf,
    /// Whether the marker is taken (`--ears`) or only watched (`--shadow`).
    pub close_mode: CloseMode,
    /// Epoch ms now: the sides' clocks (each anchored on its samples read), the lines' clock column and their emit
    /// time.
    pub now_ms: fn() -> u64,
    /// The state dir whose `me/` and `them/` get the sides' `.alive` heartbeats; `None`: none.
    pub state: Option<PathBuf>,
    /// The raw ring every recognized piece parks in; `None`: none.
    pub ring: Option<Arc<Ring>>,
    /// The most audio a side keeps waiting for its pipeline (whisper); past it the oldest is dropped
    /// ([`queue`]). [`beat::LOST_AFTER`] unless [`Ears::with_backlog`] says otherwise.
    pub backlog: Duration,
    /// How long [`run`] still waits for the pipelines once it has seen a failure: [`FAILED_GRACE`] unless
    /// [`Ears::with_failed_grace`] says otherwise.
    pub failed_grace: Duration,
    /// Whether each recognized piece writes its debug line on stdout ([`Ears::with_debug`]).
    pub debug: bool,
}

impl<R> Ears<R> {
    /// See the fields.
    #[must_use]
    pub const fn new(
        recognizer: Arc<Mutex<R>>,
        lang: String,
        close: PathBuf,
        close_mode: CloseMode,
        now_ms: fn() -> u64,
    ) -> Self {
        Self {
            recognizer,
            lang,
            close,
            close_mode,
            now_ms,
            state: None,
            ring: None,
            backlog: beat::LOST_AFTER,
            failed_grace: FAILED_GRACE,
            debug: false,
        }
    }

    /// These ears writing one [`DEBUG_MARKER`](crate::hear::DEBUG_MARKER) line on stdout per recognized piece
    /// (whisper's raw text, each pass's language and the filter's verdict): `--ears` with [`DEBUG_ENV`] on. Off,
    /// no transcript text goes anywhere it did not before.
    #[must_use]
    pub const fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// These ears keeping up to `backlog` of a side's audio waiting for its pipeline. For a caller that feeds audio
    /// faster than it is heard (a test's whole minute at once): a capture cannot, and keeps the default.
    #[must_use]
    pub const fn with_backlog(mut self, backlog: Duration) -> Self {
        self.backlog = backlog;
        self
    }

    /// These ears waiting `grace` for their pipelines after a failure, not [`FAILED_GRACE`] (tests: no 5 s wait).
    #[must_use]
    pub const fn with_failed_grace(mut self, grace: Duration) -> Self {
        self.failed_grace = grace;
        self
    }

    /// These ears writing the sides' heartbeats under `state` (`<state>/me/.alive`, `<state>/them/.alive`) and
    /// keeping every recognized piece in `ring`: the live path's files.
    #[must_use]
    pub fn with_files(mut self, state: PathBuf, ring: Ring) -> Self {
        self.state = Some(state);
        self.ring = Some(Arc::new(ring));
        self
    }
}

/// The system-wide debug switch as the tray passes it (its config's top-level `debug = true`): `1` on; unset, empty
/// or `0` off.
pub const DEBUG_ENV: &str = "TULPA_DEBUG";

/// [`DEBUG_ENV`]'s value as a switch.
///
/// # Errors
///
/// Any other value, named: a switch meant on is never taken as off.
pub fn debug_switch(value: Option<&std::ffi::OsStr>) -> Result<bool, String> {
    match value.map(std::ffi::OsStr::to_string_lossy).as_deref() {
        None | Some("" | "0") => Ok(false),
        Some("1") => Ok(true),
        Some(v) => Err(format!("{DEBUG_ENV} must be 1 or 0, not {v:?}")),
    }
}

/// How [`run`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Every pipeline ended, and nothing failed.
    Ended,
    /// A capture failed (said on stderr), or the sink lost a line. The pipelines have ended, or are still running
    /// when `give_up` or [`FAILED_GRACE`] ended the run first: a failure wins over the give-up.
    Failed,
    /// `give_up` said so before the pipelines ended, and nothing failed; they are still running.
    GaveUp,
}

/// How long [`run`] still waits for the pipelines once it has seen a capture or the sink fail.
///
/// The healthy side's phrase in progress gets this long to come out, and a side that does not end (a capture stuck
/// in its start) is left to the process's exit. The run is [`Outcome::Failed`] either way.
pub const FAILED_GRACE: Duration = Duration::from_secs(5);

/// When [`run`] stops waiting for the pipelines once `stop` is set.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct GiveUp<'a> {
    /// The instant, or `None` to wait for them however long they take.
    pub at: &'a dyn Fn() -> Option<Instant>,
    /// Set when `at`'s answer may have changed (a signal came in); `at` is asked again then, also when `stop` was
    /// set long before.
    pub changed: &'a Stop,
}

impl std::fmt::Debug for GiveUp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiveUp")
            .field("changed", self.changed)
            .finish_non_exhaustive()
    }
}

impl<'a> GiveUp<'a> {
    /// See the fields.
    #[must_use]
    pub const fn new(at: &'a dyn Fn() -> Option<Instant>, changed: &'a Stop) -> Self {
        Self { at, changed }
    }
}

impl Outcome {
    /// The process's exit code for `--ears`: 1 when the run failed, else 0.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Failed => 1,
            Self::Ended | Self::GaveUp => 0,
        }
    }
}

/// Runs both sides until their pipelines end, or until the instant `give_up` names once `stop` is set.
///
/// The pipelines end after `stop` is set, a capture fails or the sink fails; without `give_up` (or while it names
/// no instant) the run waits for them however long they take, except after a failure: then at most
/// [`FAILED_GRACE`]. No tick: the loop wakes on a heard line, on `stop`
/// and on `give_up.changed` (to learn when to give up), and on the pipelines' end.
pub fn run<R, C, S>(
    ears: &Ears<R>,
    capture: &Arc<C>,
    assembler: &mut Assembler,
    sink: &mut S,
    stop: &Arc<Stop>,
    give_up: Option<GiveUp<'_>>,
) -> Outcome
where
    R: Recognizer + Send + 'static,
    C: Capture + 'static,
    S: Sink + ?Sized,
{
    let now_ms = ears.now_ms;
    // Set by the first failure (a capture's, a panic's, the sink's): an event the loop waits on, beside `stop`.
    let failed = Arc::new(Stop::new());
    // The three exits read `failed` here only: a run whose capture or sink failed is `Failed` however it ends, the
    // give-up after a signal included.
    let end = |otherwise: Outcome| {
        if failed.is_set() {
            Outcome::Failed
        } else {
            otherwise
        }
    };
    // Every pipeline holds a sender: when the last one ends (or panics), the channel says so.
    let (heard_tx, heard_rx) = tulpa_stop::channel();
    let mut threads = Vec::new();
    for side in [Side::Me, Side::Them] {
        let fails = Fails::new(side, &failed, stop);
        threads.extend(spawn_side(side, ears, capture, &heard_tx, fails));
    }
    drop(heard_tx);
    // Each event is waited on until it is seen set; once `stop` is, the give-up instant bounds the wait, and is
    // asked again when `give_up.changed` comes after it. A failure is its own event: it bounds the wait whether it
    // comes with the stop, before it or after it.
    let (mut stopped, mut changed, mut deadline, mut failed_at) = (false, false, None, None);
    // The loop's three ways out all go on to the flush below: no exit of the run leaves a held turn behind.
    let how = loop {
        let mut waits_on: Vec<&Stop> = Vec::with_capacity(3);
        if !stopped {
            waits_on.push(stop);
        }
        if failed_at.is_none() {
            waits_on.push(&failed);
        }
        if let Some(g) = give_up.filter(|_| !changed) {
            waits_on.push(g.changed);
        }
        match heard_rx.recv_until(&waits_on, deadline) {
            // The give-up instant is a hard cap: a line still queued past it (a backlog, or a slow sink) is dropped,
            // not written.
            Got::Item(_) if deadline.is_some_and(|d| Instant::now() >= d) => {
                break Outcome::GaveUp;
            }
            // A cut phrase got no rest: its held piece goes out now, not at the side's next speech.
            Got::Item(hear::Out::Ended(side)) => {
                append_only_once_stopped(assembler, stop);
                let now = now_ms();
                if let Some(line) = assembler.flush(side, now)
                    && sink.line(&line, now).is_err()
                {
                    failed.set();
                    stop.set();
                }
            }
            Got::Item(hear::Out::Heard(h)) => {
                append_only_once_stopped(assembler, stop);
                let now = now_ms();
                // A line the sink lost fails the run, as a failed capture does: the pipelines stop, and the run
                // ends `Failed` (a non-zero exit, the shadow's `error`), never `Ended`. One Heard may give several
                // lines; the give-up cap above is per Heard, so a Heard taken before it writes all of its lines.
                for line in assembler.take(&h, now) {
                    if sink.line(&line, now).is_err() {
                        failed.set();
                        stop.set();
                        break;
                    }
                }
            }
            Got::Stopped => {
                stopped = stop.is_set();
                changed = give_up.is_some_and(|g| g.changed.is_set());
                // A failed run waits for its pipelines at most the grace after the failure was seen: a capture
                // stuck in its start (`play()` on a pending grant) never ends its side, and without a give-up
                // (`--ears`) the run would wait for it forever with the other side already gone.
                failed_at = failed_at.or_else(|| failed.is_set().then(Instant::now));
                let bound = failed_at.and_then(|t| t.checked_add(ears.failed_grace));
                let given = give_up.filter(|_| stopped).and_then(|g| (g.at)());
                deadline = given.into_iter().chain(bound).min();
            }
            Got::Timeout => break Outcome::GaveUp,
            Got::Ended => break Outcome::Ended,
        }
    };
    // Given up: the threads are left to the process's exit (one may be stuck in a start, or in whisper).
    if how == Outcome::Ended {
        for t in threads {
            let _joined = t.join();
        }
    }
    // A phrase held for a successor that never came (a max-length cut, then too little speech to close one) is
    // written now, not lost with the run: on every way out, the give-up too (the turn was recognized already and
    // only waited for its rest; what a stuck pipeline still holds is lost with it).
    append_only_once_stopped(assembler, stop);
    let now = now_ms();
    for side in [Side::Me, Side::Them] {
        if let Some(line) = assembler.flush(side, now)
            && sink.line(&line, now).is_err()
        {
            failed.set();
        }
    }
    end(how)
}

/// Once `stop` is set the run only appends to what it shares with the capture that comes next: heard.log is not
/// rotated any more ([`Assembler::append_only`]). Asked before every write, since the stop can come at any time.
fn append_only_once_stopped(assembler: &mut Assembler, stop: &Stop) {
    if stop.is_set() {
        assembler.append_only();
    }
}

/// Starts `side`'s threads, each failing the run through `fails` if it panics: the capture, the heartbeat (which also
/// fills the tap's silences) and the pipeline, whose `Heard`s go to `heard_tx`.
fn spawn_side<R, C>(
    side: Side,
    ears: &Ears<R>,
    capture: &Arc<C>,
    heard_tx: &tulpa_stop::Sender<hear::Out>,
    fails: Fails,
) -> Vec<std::thread::JoinHandle<()>>
where
    R: Recognizer + Send + 'static,
    C: Capture + 'static,
{
    let now_ms = ears.now_ms;
    let mut threads = Vec::with_capacity(3);
    let (ev_tx, ev_rx) = mpsc::channel();
    let (capture, guard) = (Arc::clone(capture), fails.clone().on_panic("capture"));
    threads.push(std::thread::spawn(move || {
        if let Err(e) = capture(side, &ev_tx, &guard.fails.stop) {
            // Said here, at once: the other side and the pipelines stop on `stop`.
            guard.fails.fail(&e);
        }
    }));
    let heard = heard_tx.clone();
    let close = (side == Side::Me).then(|| ears.close.clone());
    let (recognizer, lang, mode, debug) = (
        Arc::clone(&ears.recognizer),
        ears.lang.clone(),
        ears.close_mode,
        ears.debug,
    );
    let start_ms = now_ms();
    // The heartbeat beats, and the tap's silences are filled, where the samples arrive, on a thread of its own:
    // the pipeline's thread waits on whisper (and on the other side's whisper), longer than the tray waits for a
    // sign of life, and a gap is told by when the samples come. The shadow gets the fill and no heartbeat.
    let ev_rx = {
        let dir = ears.state.as_ref().map(|s| s.join(side.as_str()));
        // Bounded: audio waiting for whisper longer than `backlog` is dropped, oldest first (`queue`).
        let (tx, rx) = queue::bounded(beat::samples(ears.backlog));
        let token = Arc::new(());
        let reader = Arc::downgrade(&token);
        let beating = Beating::new(ev_rx, side, dir).with_stop(Arc::clone(&fails.stop));
        let guard = fails.clone().on_panic("heartbeat");
        threads.push(std::thread::spawn(move || {
            beating.forward(&tx, reader);
            drop(guard);
        }));
        (rx, token)
    };
    let ring = ears.ring.clone();
    let guard = fails.on_panic("pipeline");
    threads.push(std::thread::spawn(move || {
        let (ev_rx, _token) = ev_rx;
        let mut pipeline = Pipeline::new(side, start_ms, &lang, close, &recognizer)
            .with_clock(now_ms)
            .with_stop(&guard.fails.stop)
            .with_debug(debug);
        if let Some(ring) = &ring {
            pipeline = pipeline.with_ring(ring);
        }
        hear::run_with(&pipeline, mode, ev_rx, &heard);
        drop(guard);
    }));
    threads
}

/// How a side's thread fails the run: said on stderr by side, then `failed` and `stop` set.
#[derive(Clone)]
struct Fails {
    side: Side,
    failed: Arc<Stop>,
    stop: Arc<Stop>,
}

impl Fails {
    fn new(side: Side, failed: &Arc<Stop>, stop: &Arc<Stop>) -> Self {
        Self {
            side,
            failed: Arc::clone(failed),
            stop: Arc::clone(stop),
        }
    }

    /// Fails the run: `why` said on stderr by side, the run marked failed, and everything told to stop.
    fn fail(&self, why: &str) {
        hear::say(self.side, why);
        self.failed.set();
        self.stop.set();
    }

    /// A guard for the side's `what` thread: if the thread panics, it fails the run on its way down. A panic
    /// otherwise only drops that thread's channel: the side goes deaf while the other side keeps the run alive, and
    /// the run would end `Ended`.
    const fn on_panic(self, what: &'static str) -> OnPanic {
        OnPanic { fails: self, what }
    }
}

/// See [`Fails::on_panic`].
struct OnPanic {
    fails: Fails,
    what: &'static str,
}

impl Drop for OnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.fails
                .fail(&format!("the {} thread panicked", self.what));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::debug_switch;

    /// `1` turns the debug lines on; unset, empty and `0` leave them off; anything else is refused, never off.
    #[test]
    fn the_debug_switch_is_one_or_zero() {
        for (value, want) in [
            (None, Ok(false)),
            (Some(""), Ok(false)),
            (Some("0"), Ok(false)),
            (Some("1"), Ok(true)),
        ] {
            assert_eq!(debug_switch(value.map(OsStr::new)), want, "{value:?}");
        }
        for bad in ["true", "yes", " 1"] {
            assert!(
                debug_switch(Some(OsStr::new(bad))).is_err_and(|e| e.contains("TULPA_DEBUG")),
                "{bad}"
            );
        }
    }
}
