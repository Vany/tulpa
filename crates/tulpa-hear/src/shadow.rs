//! The shadow rehearsal's files: `tulpa-hear --shadow <dir>` runs the `--ears` pipeline beside the live path and
//! writes only here, never to stdout and never under `EARS_STATE`.
//!
//! - `<dir>` is created `0700` (and made `0700`); `lines.log`, `heard.log` and `mic-switch.log` in it are created
//!   `0600` (and made `0600` when they exist), before anything is written.
//! - [`Shadow::record`]: one `<emit_ms>\t<record>` line of `lines.log` per `TULPA-EARS-LINE`, plus the
//!   `SHADOW start <pid> <version>` and `SHADOW stop <reason>` records the comparator counts restarts by. One lock
//!   orders them, and the stop record is the last: a line that comes after it is dropped.
//! - [`rotate_past`]: a log that has passed its bound ([`BOUND`], 20 MB) is renamed to `<name>.1`, one kept; the
//!   assembler's heard.log is bounded the same way ([`Assembler::with_log_bound`]).
//! - [`Signals`]: SIGTERM, SIGINT and SIGHUP taken on a thread of their own (`sigwait`), so the run can write its stop
//!   record and exit 0 instead of dying.
//!
//! [`Assembler::with_log_bound`]: crate::assemble::Assembler::with_log_bound

use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, Signal};
use tulpa_stop::Stop;

use crate::assemble::Assembler;
use crate::assemble::HEARD_LOG;
use crate::ears::{self, Capture, Ears, GiveUp, Outcome, Sink};
use crate::log::append;
pub use crate::log::{BOUND, SWITCH_LOG, rotate_past};
use crate::recognize::Recognizer;

/// The records' file in the shadow dir.
pub const LINES_LOG: &str = "lines.log";

/// The default shadow dir: `$HOME/Library/Logs/tulpa/shadow`; `None` without a `HOME`.
#[must_use]
pub fn default_dir(home: Option<&str>) -> Option<PathBuf> {
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join("Library/Logs/tulpa/shadow"))
}

/// The shadow's `lines.log`.
#[derive(Debug)]
pub struct Shadow {
    lines: PathBuf,
    bound: u64,
    /// Held over each record's write; `true` once the stop record is written.
    stopped: Mutex<bool>,
}

impl Shadow {
    /// Makes `dir` (`0700`) and its logs (`0600`), as the module says; `bound` is where they rotate ([`BOUND`]).
    ///
    /// # Errors
    ///
    /// The first failure to make the dir or a log, or to set its mode.
    pub fn open(dir: &Path, bound: u64) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, Permissions::from_mode(0o700))?;
        for name in [LINES_LOG, HEARD_LOG, SWITCH_LOG] {
            let path = dir.join(name);
            drop(append(&path)?);
            fs::set_permissions(&path, Permissions::from_mode(0o600))?;
            let mut old = path.into_os_string();
            old.push(".1");
            if Path::new(&old).exists() {
                fs::set_permissions(&old, Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self {
            lines: dir.join(LINES_LOG),
            bound,
            stopped: Mutex::new(false),
        })
    }

    /// Appends `<emit_ms>\t<record>`, then rotates `lines.log` if this took it past the bound; after the stop
    /// record, drops it.
    ///
    /// # Errors
    ///
    /// The open's, write's or rotation's error.
    pub fn record(&self, emit_ms: u64, record: &str) -> io::Result<()> {
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            return Ok(());
        }
        let written = self.write(emit_ms, record);
        drop(stopped);
        written
    }

    /// The write itself; the caller holds the lock.
    fn write(&self, emit_ms: u64, record: &str) -> io::Result<()> {
        let mut f = append(&self.lines)?;
        f.write_all(format!("{emit_ms}\t{record}\n").as_bytes())?;
        drop(f);
        rotate_past(&self.lines, self.bound).map(drop)
    }

    /// The `SHADOW start <pid> <version>` record.
    ///
    /// # Errors
    ///
    /// As [`Shadow::record`].
    pub fn start(&self, emit_ms: u64, pid: u32, version: &str) -> io::Result<()> {
        self.record(emit_ms, &format!("SHADOW start {pid} {version}"))
    }

    /// The `SHADOW stop <reason>` record: a signal's name (`SIGTERM`, `SIGINT`, `SIGHUP`), `end` when the captures
    /// ended on their own, `error` when the run failed. Once written, it is the last record: later ones (a second
    /// stop too) are dropped. One that could not be written changes nothing: the next stop tries again.
    ///
    /// # Errors
    ///
    /// As [`Shadow::record`].
    pub fn stop(&self, emit_ms: u64, reason: &str) -> io::Result<()> {
        let mut stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            return Ok(());
        }
        self.write(emit_ms, &format!("SHADOW stop {reason}"))?;
        *stopped = true;
        drop(stopped);
        Ok(())
    }
}

impl Sink for Shadow {
    fn line(&mut self, line: &str, emit_ms: u64) -> io::Result<()> {
        self.record(emit_ms, line)
    }
}

/// After a termination signal, the phrase in progress is waited for this long, then dropped: the stop record and
/// the exit follow within 2 s of the signal.
pub const GRACE: Duration = Duration::from_millis(1500);

/// Runs the shared pipeline ([`ears::run`]) into the shadow's `lines.log`, then writes its stop record.
///
/// It runs until the pipeline ends, or until [`GRACE`] after a signal lands in `caught` (which also sets `stop`);
/// then it writes the `SHADOW stop <reason>` record and returns the reason ([`reason`]): `error` when a capture
/// failed or the sink lost a line (also after a signal), else the signal's name or `end`. A stop record that cannot
/// be written is said on stderr, and the run's reason is then `error`: it failed.
pub fn run<R, C>(
    ears: &Ears<R>,
    capture: &Arc<C>,
    assembler: &mut Assembler,
    sink: &mut Shadow,
    stop: &Arc<Stop>,
    caught: Option<&CaughtSlot>,
) -> &'static str
where
    R: Recognizer + Send + 'static,
    C: Capture + 'static,
{
    let signal = || caught.and_then(|c| c.get());
    let at = || signal().and_then(|c| c.at.checked_add(GRACE));
    let give_up = caught.map(|c| GiveUp::new(&at, &c.came));
    let outcome = ears::run(ears, capture, assembler, sink, stop, give_up);
    let reason = reason(signal().map(|c| c.name), outcome);
    if let Err(e) = sink.stop((ears.now_ms)(), reason) {
        crate::warn(&format!("tulpa-hear: --shadow: stop record: {e}"));
        // The comparator reads the log, not stderr: a run whose stop record is lost failed.
        return "error";
    }
    reason
}

/// The stop record's reason for a run that ended with `outcome`.
///
/// `signal` is the caught signal's name, if any. `error` for a failed run, signalled or not (a capture or the sink
/// failed, so the log is not whole); else the signal's name, or `end`.
#[must_use]
pub const fn reason(signal: Option<&'static str>, outcome: Outcome) -> &'static str {
    match (outcome, signal) {
        (Outcome::Failed, _) => "error",
        (_, Some(name)) => name,
        (_, None) => "end",
    }
}

/// The process's exit code for a `--shadow` run that ended with `reason` ([`run`]): 1 for `error`, else 0.
#[must_use]
pub fn exit_code(reason: &str) -> u8 {
    u8::from(reason == "error")
}

/// A termination signal that came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Caught {
    /// `SIGTERM`, `SIGINT` or `SIGHUP`.
    pub name: &'static str,
    /// When.
    pub at: Instant,
}

impl Caught {
    /// A signal by this name, caught at `at` (for a slot filled other than by [`Signals::spawn`], as in tests).
    #[must_use]
    pub const fn new(name: &'static str, at: Instant) -> Self {
        Self { name, at }
    }
}

/// The first termination signal, once one came in, and the event its arrival sets.
#[derive(Debug, Default)]
pub struct Slot {
    caught: Mutex<Option<Caught>>,
    came: Stop,
}

impl Slot {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `caught` in the slot unless one is there (the first stop is the one that counts: its time starts the
    /// grace), then sets its event.
    pub fn fill(&self, caught: Caught) {
        self.caught
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(caught);
        self.came.set();
    }

    /// The event a filled slot sets.
    #[must_use]
    pub const fn came(&self) -> &Stop {
        &self.came
    }

    /// The signal, once one came in.
    #[must_use]
    pub fn get(&self) -> Option<Caught> {
        *self.caught.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The slot the signal thread fills.
pub type CaughtSlot = Arc<Slot>;

/// SIGTERM, SIGINT and SIGHUP, blocked in the calling thread and every thread it starts afterwards.
#[derive(Debug)]
pub struct Signals {
    set: SigSet,
}

impl Signals {
    /// Blocks the three. Call before any thread starts, so that every thread inherits the mask and only the
    /// [`Signals::spawn`] thread takes them.
    ///
    /// # Errors
    ///
    /// `pthread_sigmask`'s.
    pub fn block() -> nix::Result<Self> {
        let mut set = SigSet::empty();
        for s in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            set.add(s);
        }
        set.thread_block()?;
        Ok(Self { set })
    }

    /// Waits for the first of them on its own thread; it is put in the slot and `stop` is set. Later ones stay
    /// blocked (pending) until the process exits.
    pub fn spawn(self, stop: Arc<Stop>) -> (CaughtSlot, thread::JoinHandle<()>) {
        let slot: CaughtSlot = Arc::new(Slot::new());
        let mine = Arc::clone(&slot);
        let handle = thread::spawn(move || {
            if let Ok(s) = self.set.wait() {
                mine.fill(Caught {
                    name: s.as_str(),
                    at: Instant::now(),
                });
                stop.set();
            }
        });
        (slot, handle)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::thread::JoinHandleExt;

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tulpa-hear-shadow-{tag}-{}", std::process::id()));
        let _gone = fs::remove_dir_all(&dir);
        dir
    }

    fn mode(p: &Path) -> Option<u32> {
        fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok()
    }

    /// The dir is 0700 and the logs 0600 from the start, also when they were there already, looser.
    #[test]
    fn dir_0700_and_logs_0600_also_when_they_exist() {
        let base = temp("modes");
        let dir = base.join("shadow");
        assert!(fs::create_dir_all(&dir).is_ok());
        assert!(fs::set_permissions(&dir, Permissions::from_mode(0o755)).is_ok());
        assert!(fs::write(dir.join(LINES_LOG), b"").is_ok());
        assert!(fs::set_permissions(dir.join(LINES_LOG), Permissions::from_mode(0o644)).is_ok());
        assert!(fs::write(dir.join("heard.log.1"), b"").is_ok());
        assert!(
            fs::set_permissions(dir.join("heard.log.1"), Permissions::from_mode(0o644)).is_ok()
        );
        assert!(Shadow::open(&dir, BOUND).is_ok());
        assert_eq!(mode(&dir), Some(0o700));
        for f in [LINES_LOG, HEARD_LOG, SWITCH_LOG, "heard.log.1"] {
            assert_eq!(mode(&dir.join(f)), Some(0o600), "{f}");
        }
        // A fresh dir, nested.
        let fresh = base.join("a/b");
        assert!(Shadow::open(&fresh, BOUND).is_ok());
        assert_eq!(mode(&fresh), Some(0o700));
        assert_eq!(mode(&fresh.join(LINES_LOG)), Some(0o600));
        fs::remove_dir_all(&base).ok();
    }

    /// Records carry the emit ms and a TAB; start and stop records look as the comparator reads them.
    #[test]
    fn records_are_prefixed_by_the_emit_ms() {
        let dir = temp("records");
        let s = Shadow::open(&dir, BOUND);
        assert!(s.is_ok(), "{s:?}");
        let Ok(mut s) = s else { return };
        assert!(s.start(1_000, 42, "0.1.0").is_ok());
        assert!(s.line("TULPA-EARS-LINE 1 2 00:00:00 [me] x", 2_000).is_ok());
        assert!(s.stop(3_000, "SIGTERM").is_ok());
        let text = fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default();
        assert_eq!(
            text,
            "1000\tSHADOW start 42 0.1.0\n2000\tTULPA-EARS-LINE 1 2 00:00:00 [me] x\n3000\tSHADOW stop SIGTERM\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A stop record that could not be written is not a stop: the next one writes, and only then are lines dropped.
    #[test]
    fn a_failed_stop_record_is_tried_again() {
        let dir = temp("stop-fails");
        let s = Shadow::open(&dir, BOUND);
        assert!(s.is_ok(), "{s:?}");
        let Ok(s) = s else { return };
        // A directory where the log is: the append fails.
        let log = dir.join(LINES_LOG);
        assert!(fs::remove_file(&log).is_ok() && fs::create_dir(&log).is_ok());
        assert!(s.stop(1, "SIGTERM").is_err());
        assert!(fs::remove_dir(&log).is_ok());
        assert!(s.record(2, "TULPA-EARS-LINE after the failed stop").is_ok());
        assert!(s.stop(3, "SIGTERM").is_ok());
        assert!(s.record(4, "TULPA-EARS-LINE after the stop").is_ok());
        let text = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            text,
            "2\tTULPA-EARS-LINE after the failed stop\n3\tSHADOW stop SIGTERM\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Lines from other threads never land after (or inside) the stop record: it is the last line of the log.
    #[test]
    fn the_stop_record_is_last_whatever_writes_beside_it() {
        let dir = temp("last");
        let s = Shadow::open(&dir, BOUND);
        assert!(s.is_ok(), "{s:?}");
        let Ok(s) = s else { return };
        thread::scope(|scope| {
            for t in 0..4_u64 {
                let s = &s;
                scope.spawn(move || {
                    for i in 0..300_u64 {
                        assert!(s.record(t, &format!("TULPA-EARS-LINE {i}")).is_ok());
                    }
                });
            }
            thread::sleep(Duration::from_millis(5));
            assert!(s.stop(9, "SIGTERM").is_ok());
        });
        assert!(s.stop(10, "end").is_ok());
        let text = fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default();
        assert!(
            text.lines().all(|l| l.split('\t').count() == 2),
            "whole lines only"
        );
        assert_eq!(text.lines().last(), Some("9\tSHADOW stop SIGTERM"));
        assert_eq!(text.matches("SHADOW stop").count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// Past the bound the log becomes `lines.log.1` (one kept, 0600 still) and a new one starts.
    #[test]
    fn lines_log_rotates_past_the_bound_keeping_one() {
        let dir = temp("rotate");
        let s = Shadow::open(&dir, 49);
        assert!(s.is_ok(), "{s:?}");
        let Ok(s) = s else { return };
        let rec = "TULPA-EARS-LINE 1 2 00:00:00 [me] abcdefghij"; // 1000\t + 44 + \n = 50 bytes: past 49 on its own
        assert!(s.record(1000, "short").is_ok());
        assert!(!dir.join("lines.log.1").exists(), "under the bound");
        assert!(s.record(1000, rec).is_ok());
        let old = fs::read_to_string(dir.join("lines.log.1")).unwrap_or_default();
        assert!(
            old.starts_with("1000\tshort\n") && old.ends_with("abcdefghij\n"),
            "{old}"
        );
        assert!(
            !dir.join(LINES_LOG).exists(),
            "a new one starts on the next record"
        );
        assert!(s.record(2000, "next").is_ok());
        assert_eq!(
            fs::read_to_string(dir.join(LINES_LOG)).ok().as_deref(),
            Some("2000\tnext\n")
        );
        assert_eq!(mode(&dir.join(LINES_LOG)), Some(0o600));
        assert!(s.record(3000, rec).is_ok());
        assert!(s.record(4000, rec).is_ok());
        let kept = fs::read_to_string(dir.join("lines.log.1")).unwrap_or_default();
        assert!(
            kept.starts_with("4000\t"),
            "the newest full one kept: {kept}"
        );
        assert_eq!(mode(&dir.join("lines.log.1")), Some(0o600));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_dir_is_under_library_logs() {
        assert_eq!(
            default_dir(Some("/home/x")),
            Some(PathBuf::from("/home/x/Library/Logs/tulpa/shadow"))
        );
        assert_eq!(default_dir(Some("")), None);
        assert_eq!(default_dir(None), None);
    }

    /// A SIGTERM aimed at the watch thread is taken there: named in the slot, `stop` set, nothing dies.
    #[test]
    fn a_sigterm_is_caught_and_sets_stop() {
        let signals = Signals::block();
        assert!(signals.is_ok(), "{signals:?}");
        let Ok(signals) = signals else { return };
        let stop = Arc::new(Stop::new());
        let (slot, handle) = signals.spawn(Arc::clone(&stop));
        let thread = handle.as_pthread_t();
        assert!(nix::sys::pthread::pthread_kill(thread, Signal::SIGTERM).is_ok());
        assert!(handle.join().is_ok());
        let caught = slot.get();
        assert_eq!(caught.map(|c| c.name), Some("SIGTERM"));
        assert!(stop.is_set());
        // This test thread keeps the three blocked; unblock them again for whatever runs on it next.
        let mut set = SigSet::empty();
        for s in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            set.add(s);
        }
        assert!(set.thread_unblock().is_ok());
    }
}
