//! The capture process's life: its start, its exit, its self-check and its stop.
//!
//! One start is one generation ([`Inner`]): the waiter, the reader and the self-check of a capture that was
//! replaced or stopped speak to a generation no longer current, and leave the state alone.

#[cfg_attr(
    not(test),
    expect(
        clippy::wildcard_imports,
        reason = "a second impl block of the parent's type: it uses what the parent's file uses"
    )
)]
use super::*;

/// Whether the thread `name` is to be refused, with the error the refusal gives: never, outside tests.
#[cfg(not(test))]
const fn refused(_name: &str) -> Option<std::io::Error> {
    None
}

/// Under test, whether the thread `name` is to be refused ([`REFUSE`]: its hook runs first), with the error the
/// refusal gives.
#[cfg(test)]
fn refused(name: &str) -> Option<std::io::Error> {
    let (_, hook) = REFUSE.with(|r| r.borrow_mut().take_if(|(refused, _)| *refused == name))?;
    hook();
    Some(std::io::Error::other("refused by the test"))
}

/// A thread name to refuse, and what runs just before ([`REFUSE`]).
#[cfg(test)]
pub(super) type Refusal = (&'static str, Box<dyn FnOnce()>);

/// Under test, a live capture thread: named in [`Ears`]'s `capture_threads` until it is dropped, at the thread's
/// end (or its unwind, or a start that never ran it).
#[cfg(test)]
struct Live {
    threads: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
}

#[cfg(test)]
impl Drop for Live {
    fn drop(&mut self) {
        let mut threads = self
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = threads.iter().position(|name| *name == self.name) {
            threads.swap_remove(at);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The test seam for a refused thread start: the next capture thread of this name (on the thread that starts
    /// the capture) is refused, after the hook runs (a stop at that moment, say).
    pub(super) static REFUSE: std::cell::RefCell<Option<Refusal>> =
        const { std::cell::RefCell::new(None) };
}

impl Ears {
    /// [`exec::Group::spawn_beside`] for a capture thread ([`Ears::capture_thread`]); under test, the thread named
    /// in [`REFUSE`] is refused ([`refused`]) as the OS may refuse one, through the same reap.
    fn spawn_beside<T: Send + 'static>(
        &self,
        group: &exec::Group,
        name: &'static str,
        body: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<thread::JoinHandle<T>> {
        if let Some(e) = refused(name) {
            group.kill_and_reap();
            return Err(e);
        }
        group.spawn_beside(name, self.capture_thread(name, body))
    }

    /// `body`, the capture thread `name`'s (its reader, its waiter, its self-check, a stop's): as it is, outside
    /// tests.
    #[expect(
        clippy::unused_self,
        reason = "under test it names the thread in `capture_threads`"
    )]
    #[cfg(not(test))]
    const fn capture_thread<F>(&self, _name: &'static str, body: F) -> F {
        body
    }

    /// `body`, the capture thread `name`'s (its reader, its waiter, its self-check, a stop's), named in
    /// `capture_threads` from now until it ends ([`Live`]): a test asks which capture threads are alive.
    #[cfg(test)]
    fn capture_thread<T: Send + 'static>(
        &self,
        name: &'static str,
        body: impl FnOnce() -> T + Send + 'static,
    ) -> impl FnOnce() -> T + Send + 'static {
        self.capture_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name);
        let live = Live {
            threads: Arc::clone(&self.capture_threads),
            name,
        };
        move || {
            let ended = body();
            drop(live);
            ended
        }
    }

    pub(super) fn start(
        self: &Arc<Self>,
        settings: &EarsSettings,
        mode: EarsMode,
    ) -> Result<String, String> {
        if self.paused.is_set() {
            return Err("paused".to_owned());
        }
        let mic = self.mic();
        // Before the child can write: what the transcripts hold now is old.
        self.tail_transcripts(settings);
        let Ok(mut inner) = self.inner.lock() else {
            return Err("ears: state lock poisoned".to_owned());
        };
        if let Some(pid) = inner.state.pid() {
            return Ok(format!("already running (pid {pid})"));
        }
        let (program, args) = settings
            .mode_argv(mode)
            .and_then(<[String]>::split_first)
            .ok_or_else(|| format!("ears: mode {} has no capture command", mode.id()))?;
        let log = open_log(&settings.log_file)
            .map_err(|e| format!("ears: {}: {e}", settings.log_file.display()))?;
        let log_err = log.try_clone().map_err(|e| format!("ears: log: {e}"))?;
        // The cleared environment by default; `inherit_env` keeps the tray's own (a capture tool that
        // needs more: one had mic noise under the tray); `env` sets extra variables either way.
        let mut command = if settings.inherit_env {
            let mut own = std::process::Command::new(program);
            own.stdin(std::process::Stdio::null());
            std::os::unix::process::CommandExt::process_group(&mut own, 0);
            own
        } else {
            exec::cleared(Path::new(program), &settings.home)
        };
        command.envs(settings.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        // Set last: the capture's per-device file names follow the link's device, whatever `env` says;
        // a picked microphone, by name, wins over an `EARS_MIC` in `env` too.
        command.env(DEVICE_ENV, &settings.device);
        command.env(LINES_STDOUT_ENV, "1");
        if let Some(mic) = mic {
            command.env(MIC_ENV, format!(":{mic}"));
        }
        // The config's `debug` is the one switch: an `env` entry or the tray's own environment does not turn it on.
        if settings.debug {
            command.env(DEBUG_ENV, "1");
        } else {
            command.env_remove(DEBUG_ENV);
        }
        // The `--ears` capture's stdin is a pipe the tray holds, and it is told so: the pipe's end is its stop,
        // also when the tray dies without stopping it. std makes the tray's end close-on-exec, so no other child
        // of the tray inherits it. Any other capture keeps `/dev/null`.
        let hear = is_hear(program, args);
        if hear {
            command
                .stdin(std::process::Stdio::piped())
                .env(STDIN_STOP_ENV, "1");
        }
        // #0393: stdout is the phrases' channel — the reader thread tees it to
        // the side log and hands the line markers to the link — and stderr
        // keeps its own handle to the same log.
        command
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(log_err);
        let mut child = command
            .spawn()
            .map_err(|e| format!("ears: {program} did not start: {e}"))?;
        let pid = child.id();
        let group = exec::Group::of(&child);
        let (generation, ended) = inner.next_generation();
        let exited = Arc::new(Stop::new());
        inner.group = Some(Arc::clone(&group));
        // Out of the `Child` before the waiter has it: `Child::wait` closes a stdin it still holds.
        inner.stdin = child.stdin.take();
        inner.exited = Arc::clone(&exited);
        let read = Arc::new(Stop::new());
        inner.read = Arc::clone(&read);
        inner.state = EarsState::Running {
            pid,
            since: SystemTime::now(),
        };
        inner.mode = mode;
        drop(inner);
        self.journal
            .record(&format!("ears: capture started, pid {pid}"));
        // Started beside the live capture: a thread the OS refuses kills and reaps the capture through its group
        // (`exec::Group::spawn_beside`), instead of a panic that drops the `Child` and leaves it running.
        if let Some(stdout) = child.stdout.take() {
            let reader = Arc::clone(self);
            let reading = Arc::clone(&read);
            if let Err(e) = self.spawn_beside(&group, "tulpa-ears-read", move || {
                reader.read_markers(stdout, log, generation, &reading);
            }) {
                return Err(self.no_thread(generation, &exited, &read, &e));
            }
        }
        let waiter = Arc::clone(self);
        let owner = Arc::clone(&group);
        let (its_exit, its_read) = (Arc::clone(&exited), Arc::clone(&read));
        if let Err(e) = self.spawn_beside(&owner, "tulpa-ears-wait", move || {
            waiter.wait_for(child, &group, &its_exit, &its_read, generation);
        }) {
            return Err(self.no_thread(generation, &exited, &read, &e));
        }
        let checker = Arc::clone(self);
        thread::spawn(self.capture_thread("tulpa-ears-check", move || {
            checker.check_later(generation, &ended);
        }));
        (self.changed)();
        // Pause clicked while the child was spawning wins.
        if self.paused.is_set() {
            self.pause();
            return Err("paused".to_owned());
        }
        Ok(format!("started (pid {pid})"))
    }

    /// A start whose capture was killed and reaped through its group for want of a thread
    /// (`exec::Group::spawn_beside`): it is over, and said so. Returns the error to give the caller.
    ///
    /// Its reap is told as any other ([`Ears::reaped`]), whether or not a stop came meanwhile: a closing entry
    /// for it goes, and is never given up. Its stream is over too: a reader that was refused never reads it, and
    /// one that runs stops at its next wake (a killed capture writes nothing more).
    pub(super) fn no_thread(
        &self,
        generation: u64,
        exited: &Stop,
        read: &Stop,
        e: &std::io::Error,
    ) -> String {
        read.set();
        self.reaped(exited);
        if let Ok(mut inner) = self.inner.lock()
            && inner.generation == generation
        {
            inner.next_generation();
            inner.state = EarsState::Off;
            inner.group = None;
            inner.stdin = None;
            // Under the lock, as every ears state event: in the order the state changed.
            self.push_event(&json!({"event": "ears", "state": "off", "reason": "no thread"}));
        }
        let said = format!("ears: no thread for the capture ({e}); it was killed");
        self.journal.record(&said);
        (self.changed)();
        said
    }

    /// Ends the capture, if one runs, as `how` says ([`End`]); `event_reason` pushes an `off` event with that
    /// reason. Returns the ended capture's pid. The state is off at once either way.
    pub(super) fn stop_with(
        self: &Arc<Self>,
        why: &str,
        event_reason: Option<&str>,
        how: End,
    ) -> Option<u32> {
        let (pid, group, stdin, exited, generation) = {
            let mut inner = self.inner.lock().ok()?;
            let pid = inner.state.pid()?;
            // A capture told to stop writes what it holds on its way out: those lines still count.
            let closing = inner
                .state
                .since()
                .zip(inner.group.clone())
                .filter(|_| how != End::Kill)
                .map(|(since, group)| Closing {
                    generation: inner.generation,
                    mode: inner.mode,
                    since,
                    exited: Arc::clone(&inner.exited),
                    read: Arc::clone(&inner.read),
                    pid,
                    group,
                })
                // One that is reaped and read out already has nothing more to say.
                .filter(|c| !c.done());
            let generation = closing.as_ref().map(|c| c.generation);
            inner.next_generation();
            inner.state = EarsState::Off;
            // Beside any capture still closing: a later stop does not end an earlier one's last lines.
            inner.closing.extend(closing);
            // Under the lock, as every ears state event: in the order the state changed.
            if let Some(reason) = event_reason {
                self.push_event(&json!({"event": "ears", "state": "off", "reason": reason}));
            }
            (
                pid,
                inner.group.take(),
                inner.stdin.take(),
                Arc::clone(&inner.exited),
                generation,
            )
        };
        // Every signal through the group's guard: a capture that has exited and been reaped meanwhile is sent
        // nothing.
        let said = match (how, group) {
            (End::Kill, group) => {
                drop(stdin);
                if let Some(group) = group {
                    let _sent: bool = group.kill();
                }
                "killed"
            }
            (_, Some(group)) => {
                // The one stop, both ways: the pipe's end (the `--ears` capture) and TERM to the group.
                drop(stdin);
                let _sent: bool = group.signal(Signal::TERM);
                let ears = Arc::clone(self);
                let grace = self.stop_grace;
                let killer = Arc::clone(&group);
                let finish = self.capture_thread("tulpa-ears-stop", move || {
                    ears.finish_stop(pid, &group, &exited, generation, grace);
                });
                let started = refused("tulpa-ears-stop").map_or_else(
                    || {
                        thread::Builder::new()
                            .name("tulpa-ears-stop".to_owned())
                            .spawn(finish)
                    },
                    Err,
                );
                if let Err(e) = started {
                    // No thread to wait for its exit: it is killed now, as a stop used to be, through its group's
                    // guard like every other signal (never by a bare pid, which may be another process's by
                    // now). Its waiter reaps it, and its closing entry goes as any other does.
                    let _sent: bool = killer.kill();
                    self.journal.record(&format!(
                        "ears: no thread to wait for capture pid {pid} ({e}); killed"
                    ));
                }
                "told to stop"
            }
            (_, None) => "gone",
        };
        self.journal
            .record(&format!("ears: capture pid {pid} {said} ({why})"));
        (self.changed)();
        Some(pid)
    }

    /// The captures that were told to stop and are not reaped yet.
    fn closing_alive(&self) -> Vec<Closing> {
        let closing = self
            .inner
            .lock()
            .map(|inner| inner.closing.clone())
            .unwrap_or_default();
        closing.into_iter().filter(|c| !c.exited.is_set()).collect()
    }

    /// The tray's Pause, for every capture still on its way out: killed at once, through its group's guard. The
    /// given-up ones too.
    pub(super) fn kill_closing(&self) {
        for c in self.closing_alive() {
            if c.group.kill() {
                self.journal
                    .record(&format!("ears: closing capture pid {} killed", c.pid));
            }
        }
        self.kill_given_up();
    }

    /// Kills, once more, every capture that was given up (each is still unreaped: its reap takes it off the list,
    /// [`Ears::reaped`]). Never waits: a capture is given up because waiting for it had no end.
    fn kill_given_up(&self) {
        let alive = self
            .inner
            .lock()
            .map(|inner| inner.given_up.clone())
            .unwrap_or_default();
        for c in alive {
            if c.group.kill() {
                self.journal.record(&format!(
                    "ears: given-up capture pid {} killed again",
                    c.pid
                ));
            }
        }
    }

    /// The tray's Quit: every capture on its way out (each was sent TERM at its own stop) gets ONE grace, shared,
    /// from now; whatever is still there then is killed, and their reaps are waited for under a second grace,
    /// shared too. The time is at most two graces however many captures there are, and the call returns only when
    /// all are gone or given up ([`Ears::give_up`]).
    pub(super) fn see_closing_off(&self) {
        let closing = self.closing_alive();
        let grace = Instant::now().checked_add(self.stop_grace);
        for c in &closing {
            let _gone: bool = c.exited.wait_until(grace);
        }
        for c in &closing {
            if !c.exited.is_set() && c.group.kill() {
                self.journal.record(&format!(
                    "ears: capture pid {} did not stop in {} s; killed",
                    c.pid,
                    self.stop_grace.as_secs()
                ));
            }
        }
        let reaped = Instant::now().checked_add(self.stop_grace);
        for c in &closing {
            if !c.exited.wait_until(reaped) {
                self.give_up(c.pid, Some(c.generation));
            }
        }
        // And what was given up before, or just now: killed once more, not waited for.
        self.kill_given_up();
    }

    /// A capture's exit has not come inside every bound, the kill's too (a process that KILL does not end, or an
    /// exit that cannot be learned): its closing entry is given up, said once (the stop thread and Quit may both
    /// come here for one capture). Nobody waits on it any longer. It stays within reach: its group moves to the
    /// given-up list, which Pause, Quit and the drop still kill.
    fn give_up(&self, pid: u32, generation: Option<u64>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // Not one reaped meanwhile: its reap has looked at the lists already ([`Ears::reaped`]), and would not
        // come back for it.
        let Some(at) = inner
            .closing
            .iter()
            .position(|c| Some(c.generation) == generation && !c.exited.is_set())
        else {
            return;
        };
        let given_up = inner.closing.remove(at);
        // Its waiter still reaps it when it dies, but looks for that slowly where it has to look.
        given_up.group.give_up();
        inner.given_up.push(given_up);
        drop(inner);
        self.journal.record(&format!(
            "ears: capture pid {pid} is still not gone after its kill; given up (Pause and Quit still kill it)"
        ));
    }

    /// The end of a stop: waits for the capture's exit (an event, bounded by `grace`) and kills its group if it
    /// has not come; the reap after the kill is waited for under the same bound, and a capture still not gone
    /// then is given up ([`Ears::give_up`]). It only waits and kills: the capture's last lines are taken until
    /// its reader has read its stdout to the end ([`Ears::read_out`]) or that is cut.
    fn finish_stop(
        &self,
        pid: u32,
        group: &exec::Group,
        exited: &Stop,
        generation: Option<u64>,
        grace: Duration,
    ) {
        if !exited.wait_until(Instant::now().checked_add(grace)) && group.kill() {
            self.journal.record(&format!(
                "ears: capture pid {pid} did not stop in {} s; killed",
                grace.as_secs()
            ));
        }
        // Reaped by its waiter, which sets `exited`: nothing survives the kill, but the wait is bounded all the
        // same.
        if !exited.wait_until(Instant::now().checked_add(grace)) {
            self.give_up(pid, generation);
        }
    }

    /// A capture is reaped (`exited` is its own flag, set here), whoever reaped it: its waiter, or a refused thread
    /// start ([`Ears::no_thread`]). No list keeps it past this: a given-up entry goes at once, a closing one once
    /// its stdout is read out too ([`Closing::done`]). Set before the lists are looked at, under their lock: a
    /// stop or a give-up that comes later sees it reaped.
    fn reaped(&self, exited: &Stop) {
        exited.set();
        if let Ok(mut inner) = self.inner.lock() {
            inner.closing.retain(|c| !c.done());
            inner.given_up.retain(|c| !c.exited.is_set());
        }
    }

    /// Reaps the capture; an exit while this start is still current turns the
    /// state `exited` and tells the link.
    pub(super) fn wait_for(
        &self,
        mut child: Child,
        group: &exec::Group,
        exited: &Stop,
        read: &Stop,
        generation: u64,
    ) {
        let pid = child.id();
        // Whatever the capture left behind in its group goes with it: killed between its exit and its reap, never
        // after the reap.
        let status = group.reap(&mut child);
        self.reaped(exited);
        let code = status.ok().and_then(|s| s.code());
        let current = self.inner.lock().ok().is_some_and(|mut inner| {
            let current = inner.generation == generation;
            if current {
                inner.state = EarsState::Exited { code };
                inner.ended.set();
                inner.group = None;
                inner.stdin = None;
                // Under the lock, as every ears state event: in the order the state changed.
                self.push_event(&json!({"event": "ears", "state": "exited", "code": code}));
            }
            current
        });
        if current {
            self.journal
                .record(&format!("ears: capture exited on its own, code {code:?}"));
            let from = self.inner.lock().map_or(EarsMode::Off, |inner| inner.mode);
            if from != EarsMode::Off {
                self.mode_event(EarsMode::Off, from, &ModeSource::Exited);
            }
            (self.changed)();
        }
        // The capture is gone; its stdout's end is waited for (an event), but not without end: something it
        // started outside its group can hold the pipe open long after it. Past the bound the stream is cut: the
        // capture's closing entry is finished, and the reader lets the pipe go at its next wake.
        if !read.wait_until(Instant::now().checked_add(self.read_grace)) {
            self.journal.record(&format!(
                "ears: capture pid {pid}: its stdout was still held open {} s after it exited (a child outside \
                 its group?); cut, what comes on it now is not read",
                self.read_grace.as_secs()
            ));
            // On the capture's own flag: its reader stops at its next wake, whether or not it has an entry.
            read.set();
            self.read_out();
        }
    }

    /// Runs the self-check once, [`SELF_CHECK_AFTER`] after the start it
    /// belongs to, then every [`RECHECK_EVERY`], until that start is over
    /// (`ended`, which wakes the wait at once).
    pub(super) fn check_later(&self, generation: u64, ended: &Stop) {
        // The first check after check_after; then every RECHECK_EVERY while this start runs (the
        // system-audio side dies while the display sleeps and comes back when the capture script restarts
        // it — the state follows both ways, degraded or deaf and hearing again). The check itself stays on
        // a timer: it looks for chunks that did NOT come (a deaf side), and an absence has no event.
        let mut wait = self.check_after;
        loop {
            if ended.wait_until(Instant::now().checked_add(wait)) {
                return;
            }
            self.check_once(generation);
            wait = RECHECK_EVERY;
        }
    }

    /// One self-check of the start `generation`: the state follows what it
    /// found, side by side. The capture is never stopped here — a side that
    /// failed may come back (the capture script restarts the system audio by
    /// itself), and while one side fails the other is still heard. Entering
    /// DEGRADED, DEAF, or running again tells the link once; a new reason in
    /// the same state only updates the menu.
    pub(super) fn check_once(&self, generation: u64) {
        let Ok(settings) = self.settings.as_ref() else {
            return;
        };
        // The start this check belongs to: a side is not stale for longer than its capture has run.
        let since = (self.inner.lock().ok())
            .filter(|inner| inner.generation == generation)
            .and_then(|inner| inner.state.since());
        let Some(since) = since else {
            return;
        };
        let err_text = settings.err_file.as_deref().and_then(read_tail);
        let (me, them, now) = (
            look_dir(&settings.me_dir),
            look_dir(&settings.them_dir),
            SystemTime::now(),
        );
        // A closed lid stops both sides: read it only when a side has stopped, to say so.
        let lid_closed = (stopped_for(&me, since, now).is_some()
            || stopped_for(&them, since, now).is_some())
            && self
                .lid
                .as_ref()
                .is_some_and(|lid| lid.closed() == Some(true));
        let health = self_check(&me, &them, err_text.as_deref(), lid_closed, since, now);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.generation != generation {
            return;
        }
        let (Some(pid), Some(since)) = (inner.state.pid(), inner.state.since()) else {
            return;
        };
        let next = EarsState::checked(pid, since, health);
        if next == inner.state {
            drop(inner);
            if matches!(next, EarsState::Running { .. }) {
                self.journal.record("ears: self-check passed");
            }
            return;
        }
        let before = std::mem::replace(&mut inner.state, next.clone());
        // What the change is to the link and to the journal. The event leaves under the lock that decided it, so
        // the link gets the ears' events in the order the state changed: a self-check that finds its capture deaf
        // just as the capture is replaced speaks before the new capture's start, never after it.
        let (event, note) = match (&before, &next) {
            (EarsState::Degraded { side: was, .. }, EarsState::Degraded { side, .. })
                if was == side =>
            {
                (None, None)
            }
            (EarsState::Deaf { .. }, EarsState::Deaf { .. }) => (None, None),
            (_, EarsState::Degraded { side, reason, .. }) => (
                Some(
                    json!({"event": "ears", "state": "degraded", "side": side.id(), "reason": reason, "fix": side.settings_pane()}),
                ),
                Some(format!(
                    "ears: DEGRADED: {}: {reason}; {} still heard, the capture keeps running",
                    side.id(),
                    side.other().id()
                )),
            ),
            (_, EarsState::Deaf { reason, .. }) => (
                Some(json!({"event": "ears", "state": "deaf", "reason": reason})),
                Some(format!("ears: DEAF: {reason}")),
            ),
            (EarsState::Degraded { side, .. }, _) => (
                Some(
                    json!({"event": "ears", "state": "running", "reason": "hearing again", "side": side.id()}),
                ),
                Some(format!("ears: hearing again ({} is back)", side.id())),
            ),
            _ => (
                Some(json!({"event": "ears", "state": "running", "reason": "hearing again"})),
                Some("ears: hearing again".to_owned()),
            ),
        };
        if let Some(event) = event {
            self.push_event(&event);
        }
        drop(inner);
        if let Some(note) = note {
            self.journal.record(&note);
        }
        (self.changed)();
    }
}

/// Whether a capture command is the `--ears` capture (`tulpa-hear --ears`): the one that stops on its stdin's end.
fn is_hear(program: &str, args: &[String]) -> bool {
    Path::new(program).file_name().and_then(|n| n.to_str()) == Some("tulpa-hear")
        && args.first().map(String::as_str) == Some("--ears")
}
