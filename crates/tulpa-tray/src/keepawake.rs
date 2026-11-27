//! Keeps the Mac awake: Tulpa's own keep-awake, the only one a Mac needs in normal operation.
//!
//! A sleeping Mac drops the link, and nothing reaches it again until someone wakes it. While any reason is
//! held, a `caffeinate` child runs with the [`Flags`] `[keepawake] flags` names, `-i -s` by default (no idle
//! sleep; no system sleep on AC power). By default one reason,
//! [`ALWAYS`], is held for as long as Tulpa runs (`[keepawake] always`); the agent's work holds its own — a
//! hands request in flight, the ears listening — which keep the Mac awake when `always` is off. The child
//! ends [`GRACE`] after the last reason is released, so back-to-back commands don't flap it, and is started
//! again when it dies while a reason is held: after [`BACKOFF_FIRST`], then twice as long each time it dies
//! again within [`STEADY_AFTER`], up to [`BACKOFF_MAX`] ([`Restarts`]), one log line per such episode. No
//! FFI: the system's own tool holds the assertion, and killing the child releases it (also when the tray
//! dies: `-w` watches the tray's pid).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tulpa_hands::Journal;

/// How long the assertion outlives the last reason.
pub const GRACE: Duration = Duration::from_secs(120);
/// The program holding the assertion on macOS.
pub const CAFFEINATE: &str = "/usr/bin/caffeinate";
/// The caffeinate assertions `[keepawake] flags` may name, in the order shown.
///
/// Display (`d`), idle (`i`), disk idle (`m`), system on AC power (`s`), user active (`u`); nothing that
/// takes a value (`-t`, `-w`).
pub const FLAG_LETTERS: [char; 5] = ['d', 'i', 'm', 's', 'u'];
/// The flags when `[keepawake] flags` is absent.
pub const DEFAULT_FLAGS: &str = "-i -s";
/// The reason held for as long as Tulpa runs, unless `[keepawake] always = false`.
pub const ALWAYS: &str = "always";
const TICK: Duration = Duration::from_millis(500);
/// The wait before a caffeinate that ended (or could not start) is started again.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two starts of a caffeinate that keeps ending.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A caffeinate up this long is well again: when it ends, the wait starts over at [`BACKOFF_FIRST`].
pub const STEADY_AFTER: Duration = Duration::from_secs(60);

/// Reads the time: [`Instant::now`], a fake clock in tests.
type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// Which assertions the keep-awake's caffeinate holds (`[keepawake] flags`): letters of [`FLAG_LETTERS`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flags {
    /// In [`FLAG_LETTERS`] order, each once.
    letters: Vec<char>,
}

impl Flags {
    /// Reads `-i -s`, `-dimsu` and the like: words of a `-` and one or more of [`FLAG_LETTERS`], a letter
    /// given twice taken once.
    ///
    /// # Errors
    ///
    /// No word at all, a word without its `-` or its letters, or any other letter; the reason quotes the value.
    pub fn parse(text: &str) -> Result<Self, String> {
        let allowed = || {
            let letters: Vec<String> = FLAG_LETTERS.iter().map(|c| format!("-{c}")).collect();
            letters.join(" ")
        };
        let mut seen = Vec::new();
        for word in text.split_whitespace() {
            let letters = word
                .strip_prefix('-')
                .filter(|letters| !letters.is_empty())
                .ok_or_else(|| format!("{text:?}: {word:?} is not -<letters>"))?;
            for letter in letters.chars() {
                if !FLAG_LETTERS.contains(&letter) {
                    return Err(format!("{text:?}: -{letter} is not one of {}", allowed()));
                }
                seen.push(letter);
            }
        }
        if seen.is_empty() {
            return Err(format!("{text:?}: no flag; give one of {}", allowed()));
        }
        Ok(Self {
            letters: FLAG_LETTERS
                .into_iter()
                .filter(|letter| seen.contains(letter))
                .collect(),
        })
    }

    /// One argument per letter, e.g. `["-i", "-s"]`: the default is the argv Tulpa has always run.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        self.letters.iter().map(|c| format!("-{c}")).collect()
    }

    /// The flags as the status shows them, e.g. `-is` or `-dimsu`.
    #[must_use]
    pub fn shown(&self) -> String {
        std::iter::once('-')
            .chain(self.letters.iter().copied())
            .collect()
    }

    /// Tulpa's keep-awake on macOS holding these flags ([`KeepAwake::macos`]).
    #[must_use]
    pub fn keep_awake(&self, changed: Box<dyn Fn() + Send + Sync>) -> Arc<KeepAwake> {
        KeepAwake::macos(self, changed)
    }
}

impl Default for Flags {
    /// [`DEFAULT_FLAGS`]: `-i -s`.
    fn default() -> Self {
        Self {
            letters: vec!['i', 's'],
        }
    }
}

/// The flags to hold for `[keepawake] flags` (`None`: absent), and the log line when the value is refused:
/// a mistyped value must not leave the Mac without a keep-awake, so the default is held instead.
#[must_use]
pub fn configured_flags(value: Option<&str>) -> (Flags, Option<String>) {
    match value.map(Flags::parse) {
        None => (Flags::default(), None),
        Some(Ok(flags)) => (flags, None),
        Some(Err(why)) => {
            let flags = Flags::default();
            let line = format!(
                "keep-awake: [keepawake] flags refused ({why}); holding {} instead",
                flags.shown()
            );
            (flags, Some(line))
        }
    }
}

/// When a caffeinate that ended is started again, from the times it is told (no clock of its own).
///
/// The first end waits [`BACKOFF_FIRST`]; each end after a run shorter than [`STEADY_AFTER`] waits twice the
/// wait before, up to [`BACKOFF_MAX`]. Those ends make one episode, logged once, at its first end. A run of
/// [`STEADY_AFTER`] or more closes it, and so does a deliberate stop (`Restarts::default()`).
#[derive(Debug, Default)]
struct Restarts {
    /// When the running child started; `None` while none runs.
    since: Option<Instant>,
    /// The wait after the episode's last end; zero outside an episode.
    wait: Duration,
    /// The next start waits until then; `None`: at once.
    next: Option<Instant>,
}

impl Restarts {
    /// A child started at `now`.
    const fn started(&mut self, now: Instant) {
        self.since = Some(now);
        self.next = None;
    }

    /// The child ended, or a start failed, at `now`: the wait before the next start, and whether this end
    /// opens an episode (the one to log).
    fn ended(&mut self, now: Instant) -> (Duration, bool) {
        let steady = self
            .since
            .take()
            .is_some_and(|since| now.saturating_duration_since(since) >= STEADY_AFTER);
        if steady {
            self.wait = Duration::ZERO;
        }
        let first = self.wait.is_zero();
        self.wait = if first {
            BACKOFF_FIRST
        } else {
            self.wait.saturating_mul(2).min(BACKOFF_MAX)
        };
        self.next = Some(now.checked_add(self.wait).unwrap_or(now));
        (self.wait, first)
    }

    /// Whether a start may go at `now`.
    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }
}

#[derive(Debug, Default)]
struct State {
    reasons: BTreeSet<&'static str>,
    released_at: Option<Instant>,
    child: Option<Child>,
    restarts: Restarts,
}

/// The keep-awake holder; cheap to share.
pub struct KeepAwake {
    state: Mutex<State>,
    program: PathBuf,
    args: Vec<String>,
    /// The flags the status shows (e.g. `-is`); `None`: a holder that is no caffeinate shows none.
    shown: Option<String>,
    grace: Duration,
    changed: Box<dyn Fn() + Send + Sync>,
    clock: Clock,
    /// Where an episode of restarts is logged ([`KeepAwake::log_to`]); none: not logged.
    journal: Mutex<Option<Arc<dyn Journal>>>,
}

impl std::fmt::Debug for KeepAwake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepAwake")
            .field("program", &self.program)
            .field("active", &self.active())
            .finish_non_exhaustive()
    }
}

impl KeepAwake {
    /// The macOS holder: `/usr/bin/caffeinate <flags> -w <this pid>`, by default `-i -s -w <this pid>`.
    #[must_use]
    pub fn macos(flags: &Flags, changed: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Self::caffeinate(PathBuf::from(CAFFEINATE), flags, GRACE, changed)
    }

    /// A holder running `program <flags> -w <this pid>` (a caffeinate, or a stand-in in tests), its status
    /// showing the flags.
    #[must_use]
    pub fn caffeinate(
        program: PathBuf,
        flags: &Flags,
        grace: Duration,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        let mut args = flags.args();
        args.extend(["-w".to_owned(), std::process::id().to_string()]);
        Self::build(program, args, Some(flags.shown()), grace, changed)
    }

    /// A holder running `program args` while a reason is held (tests use a harmless program).
    #[must_use]
    pub fn with(
        program: PathBuf,
        args: Vec<String>,
        grace: Duration,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        Self::build(program, args, None, grace, changed)
    }

    fn build(
        program: PathBuf,
        args: Vec<String>,
        shown: Option<String>,
        grace: Duration,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        let me = Self::unticked(program, args, shown, grace, changed, Box::new(Instant::now));
        let ticker = Arc::clone(&me);
        thread::spawn(move || {
            loop {
                thread::sleep(TICK);
                ticker.tick();
                if Arc::strong_count(&ticker) == 1 {
                    ticker.stop_child();
                    return;
                }
            }
        });
        me
    }

    /// A holder reading the time from `clock`, without the thread that ticks it (tests tick by hand).
    fn unticked(
        program: PathBuf,
        args: Vec<String>,
        shown: Option<String>,
        grace: Duration,
        changed: Box<dyn Fn() + Send + Sync>,
        clock: Clock,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            program,
            args,
            shown,
            grace,
            changed,
            clock,
            journal: Mutex::new(None),
        })
    }

    /// Logs to `journal` from now on: one line when a caffeinate that ended opens an episode of restarts.
    pub fn log_to(&self, journal: Arc<dyn Journal>) {
        if let Ok(mut slot) = self.journal.lock() {
            *slot = Some(journal);
        }
    }

    /// Holds `reason` (idempotent); starts the assertion if none runs, unless a restart is waiting out its
    /// backoff.
    pub fn hold(&self, reason: &'static str) {
        let now = (self.clock)();
        let (started, note) = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            let new = st.reasons.insert(reason);
            st.released_at = None;
            let note = if st.child.is_none() && st.restarts.due(now) {
                self.start(&mut st, now)
            } else {
                None
            };
            (new, note)
        };
        if let Some(line) = note {
            self.note(&line);
        }
        if started {
            (self.changed)();
        }
    }

    /// Releases `reason`; with none left, the assertion ends after the grace.
    pub fn release(&self, reason: &'static str) {
        let removed = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            let removed = st.reasons.remove(reason);
            if st.reasons.is_empty() && removed {
                st.released_at = Some((self.clock)());
            }
            removed
        };
        if removed {
            (self.changed)();
        }
    }

    /// The state, as `ears.status` reports it: `on (always, -is)` or `on (ears, hands, -is)` with the reasons
    /// held and the flags, `on (finishing, -is)` in the grace after the last one, `off`, or `FAILED (…:
    /// caffeinate is not running)` while a reason is held but its child ended or could not be started (it is
    /// tried again after [`BACKOFF_FIRST`], backing off up to [`BACKOFF_MAX`] while it keeps ending).
    #[must_use]
    pub fn status(&self) -> String {
        let Ok(st) = self.state.lock() else {
            return "unknown".to_owned();
        };
        let reasons = st.reasons.iter().copied().collect::<Vec<_>>().join(", ");
        let flags = self
            .shown
            .as_deref()
            .map(|flags| format!(", {flags}"))
            .unwrap_or_default();
        match (reasons.is_empty(), st.child.is_some()) {
            (false, true) => format!("on ({reasons}{flags})"),
            (false, false) => format!("FAILED ({reasons}: caffeinate is not running)"),
            (true, true) => format!("on (finishing{flags})"),
            (true, false) => "off".to_owned(),
        }
    }

    /// The menu line, e.g. `keep-awake: on (always, -is)`: [`KeepAwake::status`] with its name.
    #[must_use]
    pub fn line(&self) -> String {
        format!("keep-awake: {}", self.status())
    }

    /// Whether the assertion's child runs now.
    #[must_use]
    pub fn active(&self) -> bool {
        self.state.lock().is_ok_and(|st| st.child.is_some())
    }

    /// Starts the child at `now`. A start that fails counts as an end: the log line when it opens an
    /// episode of restarts.
    fn start(&self, st: &mut State, now: Instant) -> Option<String> {
        let spawned = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                st.child = Some(child);
                st.restarts.started(now);
                None
            }
            Err(error) => Self::ended(st, now, &format!("could not start ({error})")),
        }
    }

    /// Notes that the child ended (`how`) at `now`: the log line when this end opens an episode.
    fn ended(st: &mut State, now: Instant, how: &str) -> Option<String> {
        let (wait, first) = st.restarts.ended(now);
        first.then(|| {
            format!(
                "keep-awake: caffeinate {how}; starting it again in {} s, backing off up to {} s while it \
                 keeps ending (said once, until one stays up {} s)",
                wait.as_secs(),
                BACKOFF_MAX.as_secs(),
                STEADY_AFTER.as_secs()
            )
        })
    }

    fn note(&self, line: &str) {
        let journal = self.journal.lock().ok().and_then(|slot| slot.clone());
        if let Some(journal) = journal {
            journal.record(line);
        }
    }

    fn tick(&self) {
        let now = (self.clock)();
        // A child that goes is taken out under the lock and killed and reaped (a wait) with it let go.
        let mut gone = None;
        let (changed, note) = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            if st.reasons.is_empty() {
                match st.released_at {
                    Some(at) if now.saturating_duration_since(at) >= self.grace => {
                        st.released_at = None;
                        st.restarts = Restarts::default();
                        gone = st.child.take();
                        (true, None)
                    }
                    _ => (false, None),
                }
            } else {
                // A reason is held: a child that ended (killed by hand, crashed) or never started is started
                // again once its backoff is over.
                let ended = match st.child.as_mut().map(Child::try_wait) {
                    None | Some(Ok(None)) => None,
                    Some(Ok(Some(status))) => Some(format!("exited ({status})")),
                    Some(Err(error)) => Some(format!("could not be watched ({error})")),
                };
                let note = ended.as_deref().and_then(|how| {
                    gone = st.child.take();
                    Self::ended(&mut st, now, how)
                });
                // An end just now waits at least BACKOFF_FIRST: never started again in the same tick.
                let (started, failed) = if st.child.is_none() && st.restarts.due(now) {
                    let failed = self.start(&mut st, now);
                    (st.child.is_some(), failed)
                } else {
                    (false, None)
                };
                (ended.is_some() || started, note.or(failed))
            }
        };
        Self::end(gone);
        if let Some(line) = note {
            self.note(&line);
        }
        if changed {
            (self.changed)();
        }
    }

    fn stop_child(&self) {
        Self::end(self.state.lock().ok().and_then(|mut st| st.child.take()));
    }

    /// Kills and reaps a child taken out of the state: never under its lock (the reap is a wait).
    fn end(child: Option<Child>) {
        if let Some(mut child) = child {
            let _killed: std::io::Result<()> = child.kill();
            crate::ears::reap_killed(&mut child);
        }
    }

    /// Ends the assertion now (Quit, app exit).
    pub fn shutdown(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.reasons.clear();
            st.restarts = Restarts::default();
        }
        self.stop_child();
    }

    #[cfg(test)]
    fn child_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .ok()
            .and_then(|st| st.child.as_ref().map(Child::id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeper(grace: Duration) -> Arc<KeepAwake> {
        KeepAwake::with(
            PathBuf::from("/bin/sleep"),
            vec!["30".to_owned()],
            grace,
            Box::new(|| {}),
        )
    }

    #[test]
    fn holds_while_a_reason_is_held_and_ends_after_the_grace() {
        let k = sleeper(Duration::from_millis(300));
        assert!(!k.active());
        assert_eq!(k.line(), "keep-awake: off");
        k.hold("hands");
        k.hold("ears");
        assert!(k.active());
        assert_eq!(k.line(), "keep-awake: on (ears, hands)");
        k.release("hands");
        thread::sleep(Duration::from_millis(1200));
        assert!(k.active(), "ears still held");
        k.release("ears");
        assert_eq!(k.line(), "keep-awake: on (finishing)");
        thread::sleep(Duration::from_millis(1500));
        assert!(!k.active(), "ended after the grace");
        assert_eq!(k.status(), "off");
        k.hold("hands");
        k.release("hands");
        k.hold("hands");
        thread::sleep(Duration::from_millis(1200));
        assert!(k.active(), "re-held inside the grace: no flap");
        k.shutdown();
        assert!(!k.active());
    }

    #[test]
    fn always_outlasts_the_work_and_its_grace() {
        let k = sleeper(Duration::from_millis(300));
        k.hold(ALWAYS);
        assert_eq!(k.status(), "on (always)");
        k.hold("hands");
        assert_eq!(k.line(), "keep-awake: on (always, hands)");
        k.release("hands");
        thread::sleep(Duration::from_millis(1200));
        assert!(k.active(), "the grace after the work ends nothing");
        assert_eq!(k.line(), "keep-awake: on (always)");
        k.shutdown();
        assert_eq!(k.status(), "off");
    }

    #[test]
    fn a_child_that_dies_while_held_is_started_again() -> Result<(), Box<dyn std::error::Error>> {
        let k = sleeper(Duration::from_millis(300));
        k.hold(ALWAYS);
        let first = k.child_pid().ok_or("no child after hold")?;
        let killed = Command::new("kill")
            .args(["-KILL", &first.to_string()])
            .status()?;
        assert!(killed.success());
        let waiting = Instant::now();
        while k.child_pid().is_none_or(|pid| pid == first)
            && waiting.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(100));
        }
        let second = k.child_pid().ok_or("no child after the first died")?;
        assert_ne!(second, first, "a new caffeinate runs");
        assert_eq!(k.status(), "on (always)");
        k.shutdown();
        Ok(())
    }

    #[test]
    fn flags_take_only_caffeinates_assertion_letters() -> Result<(), String> {
        let default = Flags::parse(DEFAULT_FLAGS)?;
        assert_eq!(default, Flags::default());
        assert_eq!(
            default.args(),
            vec!["-i", "-s"],
            "the argv Tulpa always ran"
        );
        assert_eq!(default.shown(), "-is");
        assert_eq!(Flags::parse("-dimsu")?.shown(), "-dimsu");
        let mixed = Flags::parse(" -u\t-d -ii ")?;
        assert_eq!(mixed.args(), vec!["-d", "-i", "-u"], "in order, each once");
        assert_eq!(mixed.shown(), "-diu");
        for bad in [
            "", "  ", "-", "--", "-x", "-I", "-is -t", "-i -w 1", "i s", "-i5", "-is;", "-i,-s",
        ] {
            assert!(Flags::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(configured_flags(None), (Flags::default(), None));
        assert_eq!(
            configured_flags(Some("-d -i -m -s -u")),
            (Flags::parse("-dimsu")?, None)
        );
        let (held, line) = configured_flags(Some("-ix"));
        assert_eq!(held, Flags::default(), "a refused value holds the default");
        assert_eq!(
            line.as_deref(),
            Some(
                "keep-awake: [keepawake] flags refused (\"-ix\": -x is not one of -d -i -m -s -u); holding -is instead"
            )
        );
        Ok(())
    }

    /// The caffeinate runs with the flags and Tulpa's pid, and the status shows the flags.
    #[test]
    fn the_caffeinate_gets_the_flags_and_the_status_shows_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-keepawake-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir)?;
        let said = dir.join("args");
        let program = dir.join("caffeinate");
        crate::testutil::write_script(
            &program,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > {0}.new && mv {0}.new {0}\nexec sleep 30\n",
                said.display()
            ),
        )?;
        let pid = std::process::id();
        for (flags, argv, status) in [
            (
                Flags::default(),
                format!("-i -s -w {pid}"),
                "on (always, -is)",
            ),
            (
                Flags::parse("-dimsu")?,
                format!("-d -i -m -s -u -w {pid}"),
                "on (always, -dimsu)",
            ),
        ] {
            let _gone: std::io::Result<()> = std::fs::remove_file(&said);
            let k = KeepAwake::caffeinate(
                program.clone(),
                &flags,
                Duration::from_millis(300),
                Box::new(|| {}),
            );
            assert_eq!(k.status(), "off");
            k.hold(ALWAYS);
            assert_eq!(k.status(), status);
            assert_eq!(k.line(), format!("keep-awake: {status}"));
            let waiting = Instant::now();
            while !said.exists() && waiting.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(std::fs::read_to_string(&said)?.trim_end(), argv);
            k.hold("hands");
            k.release(ALWAYS);
            k.release("hands");
            assert_eq!(
                k.status(),
                format!("on (finishing, {})", flags.shown()),
                "the grace shows the flags too"
            );
            k.shutdown();
            assert_eq!(k.status(), "off");
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The fake clock's reading `ms` after `t0`.
    fn at(t0: Instant, ms: u64) -> Instant {
        t0.checked_add(Duration::from_millis(ms)).unwrap_or(t0)
    }

    /// A fake clock: a fixed start plus what the test advanced it by.
    #[derive(Clone)]
    struct FakeClock {
        t0: Instant,
        passed: Arc<Mutex<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                t0: Instant::now(),
                passed: Arc::new(Mutex::new(Duration::ZERO)),
            }
        }

        fn advance(&self, by: Duration) {
            if let Ok(mut passed) = self.passed.lock() {
                *passed = passed.saturating_add(by);
            }
        }

        fn reader(&self) -> Clock {
            let clock = self.clone();
            Box::new(move || {
                let passed = clock.passed.lock().map_or(Duration::ZERO, |p| *p);
                clock.t0.checked_add(passed).unwrap_or(clock.t0)
            })
        }
    }

    #[derive(Debug, Default)]
    struct Notes(Mutex<Vec<String>>);

    impl Journal for Notes {
        fn record(&self, line: &str) {
            if let Ok(mut lines) = self.0.lock() {
                lines.push(line.to_owned());
            }
        }
    }

    impl Notes {
        fn lines(&self) -> Vec<String> {
            self.0.lock().map(|l| l.clone()).unwrap_or_default()
        }
    }

    impl KeepAwake {
        /// Waits (real time, up to 5 s) until the running child has ended; false when none runs or it
        /// does not end. Its status is kept, so the next tick sees the end.
        fn child_ended(&self) -> bool {
            let waiting = Instant::now();
            while waiting.elapsed() < Duration::from_secs(5) {
                let ended = self.state.lock().ok().is_some_and(|mut st| {
                    st.child
                        .as_mut()
                        .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
                });
                if ended {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        }
    }

    /// #0530 (the gate's F5 family): no lock is held across a wait. The caffeinate ended at the grace's end is
    /// killed and reaped (a wait) with the state's lock let go: one whose reap does not come (in uninterruptible
    /// sleep) would otherwise hold every hold, release and status with it.
    #[test]
    fn a_child_ended_at_the_grace_is_reaped_outside_the_state_lock() {
        let clock = FakeClock::new();
        let k = KeepAwake::unticked(
            PathBuf::from("/bin/sleep"),
            vec!["30".to_owned()],
            None,
            Duration::from_secs(1),
            Box::new(|| {}),
            clock.reader(),
        );
        k.hold("hands");
        k.tick();
        assert!(k.child_pid().is_some(), "started");
        k.release("hands");
        clock.advance(Duration::from_secs(2));
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (told, looked) = (std::rc::Rc::clone(&seen), Arc::downgrade(&k));
        crate::ears::BEFORE_REAP.set(Some(Box::new(move || {
            told.set(looked.upgrade().map(|k| k.state.try_lock().is_ok()));
        })));
        k.tick();
        crate::ears::BEFORE_REAP.set(None);
        assert_eq!(seen.get(), Some(true), "reaped under the state's lock");
        assert!(k.child_pid().is_none(), "ended after the grace");
    }

    #[test]
    fn restarts_back_off_from_1_s_to_60_s_and_start_over_after_a_steady_run() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let mut r = Restarts::default();
        assert!(r.due(t0), "nothing ended yet: a start goes at once");
        r.started(t0);
        assert_eq!(
            r.ended(at(t0, 100)),
            (s(1), true),
            "the first end opens an episode"
        );
        assert!(!r.due(at(t0, 1_099)));
        assert!(r.due(at(t0, 1_100)));
        // Every start ends 100 ms later: the waits double up to the cap, all in one episode.
        let mut now: u64 = 1_100;
        let mut waits = Vec::new();
        for _ in 0..7 {
            r.started(at(t0, now));
            let (wait, first) = r.ended(at(t0, now.saturating_add(100)));
            assert!(!first, "the same episode: not logged again");
            waits.push(wait.as_secs());
            now = now
                .saturating_add(100)
                .saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
            assert!(!r.due(at(t0, now.saturating_sub(1))));
            assert!(r.due(at(t0, now)));
        }
        assert_eq!(waits, vec![2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(
            r.ended(at(t0, now)),
            (s(60), false),
            "a start that fails is one more end"
        );
        // A run of 60 s closes the episode: the next end waits 1 s and is logged again.
        r.started(at(t0, now));
        now = now.saturating_add(60_000);
        assert_eq!(r.ended(at(t0, now)), (s(1), true));
        r.started(at(t0, now));
        now = now.saturating_add(59_999);
        assert_eq!(r.ended(at(t0, now)), (s(2), false), "just under 60 s: no");
    }

    /// The holder on a fake clock, ticked by hand: a caffeinate that ends at once is started again after
    /// 1 s, 2 s, 4 s, not every tick, and logged once; one that then stays up 60 s and is killed starts over
    /// at 1 s, logged again.
    #[test]
    fn a_caffeinate_that_ends_at_once_is_started_again_with_backoff_and_logged_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-keepawake-backoff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir)?;
        let runs = dir.join("runs");
        // Ends at once (exit 3) its first three times, then stays up.
        let script = format!(
            "echo run >> {0}; [ \"$(grep -c run {0})\" -gt 3 ] && exec sleep 30; exit 3",
            runs.display()
        );
        let count = || std::fs::read_to_string(&runs).map_or(0, |text| text.lines().count());
        // The runs so far, once the newest has written its line (real time, up to 5 s).
        let runs_reach = |want: usize| {
            let waiting = Instant::now();
            while count() < want && waiting.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(10));
            }
            count()
        };
        let clock = FakeClock::new();
        let notes = Arc::new(Notes::default());
        let k = KeepAwake::unticked(
            PathBuf::from("/bin/sh"),
            vec!["-c".to_owned(), script],
            None,
            Duration::from_millis(300),
            Box::new(|| {}),
            clock.reader(),
        );
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        k.log_to(journal);
        let ms = Duration::from_millis;
        k.hold(ALWAYS);
        assert!(k.child_ended());
        k.tick();
        assert_eq!(runs_reach(1), 1);
        assert!(!k.active());
        assert_eq!(k.status(), "FAILED (always: caffeinate is not running)");
        // Each end waits twice as long as the one before: 1 s, 2 s, 4 s; nothing starts before.
        for (run, wait) in [(2, 1_000), (3, 2_000), (4, 4_000)] {
            clock.advance(ms(wait - 1));
            k.tick();
            assert!(!k.active(), "run {run}: not before {wait} ms");
            clock.advance(ms(1));
            k.tick();
            assert!(k.active(), "run {run}: started after {wait} ms");
            assert_eq!(runs_reach(run), run);
            if run < 4 {
                assert!(k.child_ended());
                k.tick();
            }
        }
        assert_eq!(k.status(), "on (always)", "the fourth stays up");
        let said = notes.lines();
        assert_eq!(said.len(), 1, "one line for the episode: {said:?}");
        assert!(
            said.first().is_some_and(|l| l.starts_with(
                "keep-awake: caffeinate exited (exit status: 3); starting it again in 1 s, backing off up to 60 s"
            )),
            "{said:?}"
        );
        // Up 60 s, then killed by hand: a new episode, 1 s again, logged again.
        clock.advance(Duration::from_secs(60));
        k.tick();
        assert!(k.active());
        let pid = k.child_pid().ok_or("no child")?;
        assert!(
            Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()?
                .success()
        );
        assert!(k.child_ended());
        k.tick();
        assert_eq!(notes.lines().len(), 2, "the new episode is logged");
        clock.advance(ms(999));
        k.tick();
        assert!(!k.active());
        clock.advance(ms(1));
        k.tick();
        assert!(k.active(), "started again after 1 s");
        assert_eq!(runs_reach(5), 5);
        k.shutdown();
        assert_eq!(k.status(), "off");
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A caffeinate that cannot start is tried with the same backoff, logged once.
    #[test]
    fn a_caffeinate_that_cannot_start_is_tried_with_backoff() {
        let clock = FakeClock::new();
        let notes = Arc::new(Notes::default());
        let k = KeepAwake::unticked(
            PathBuf::from("/nonexistent/caffeinate"),
            Vec::new(),
            None,
            Duration::from_millis(300),
            Box::new(|| {}),
            clock.reader(),
        );
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        k.log_to(journal);
        k.hold(ALWAYS);
        for wait in [1, 2, 4, 8] {
            clock.advance(Duration::from_secs(wait));
            k.tick();
            k.hold("hands");
            k.release("hands");
        }
        let said = notes.lines();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said.first()
                .is_some_and(|l| l.starts_with("keep-awake: caffeinate could not start (")),
            "{said:?}"
        );
        assert_eq!(k.status(), "FAILED (always: caffeinate is not running)");
        k.shutdown();
    }

    #[test]
    fn a_holder_that_cannot_start_says_so() {
        let k = KeepAwake::with(
            PathBuf::from("/nonexistent/caffeinate"),
            Vec::new(),
            Duration::from_millis(300),
            Box::new(|| {}),
        );
        k.hold(ALWAYS);
        assert!(!k.active());
        assert_eq!(
            k.line(),
            "keep-awake: FAILED (always: caffeinate is not running)"
        );
        k.shutdown();
        assert_eq!(k.status(), "off");
    }
}
