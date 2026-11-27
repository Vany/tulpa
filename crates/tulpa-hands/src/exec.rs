//! Running a child for `git.*` and `run`.
//!
//! A cleared environment, its own process group, output capped
//! while it is read, and killed — the whole group — on pause, on stop, or past
//! its time limit.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use tulpa_stop::{Got, Receiver, Stop};

/// The only `PATH` a child sees.
const CHILD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin";

/// How a child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ended {
    /// Exited with this code (`None`: by a signal it got from elsewhere).
    Exit(Option<i32>),
    /// Killed because the hands were paused or stopped.
    Paused,
    /// Killed because the requester went away mid-run (the link died).
    Abandoned,
    /// Killed past its time limit.
    Timeout,
}

/// A finished child: how it ended, stdout then stderr (each capped), and
/// whether anything was cut.
#[derive(Debug)]
#[non_exhaustive]
pub struct Outcome {
    /// How the child ended.
    pub ended: Ended,
    /// What stdout produced, capped while read.
    pub stdout: Vec<u8>,
    /// What stderr produced, capped while read.
    pub stderr: Vec<u8>,
    /// True when a stream produced more than the cap.
    pub cut: bool,
}

/// A command with the cleared environment: `PATH`, `HOME`, `LANG`, `TMPDIR`
/// only — no `SSH_AUTH_SOCK`, no tokens.
pub fn cleared(program: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", CHILD_PATH)
        .env("HOME", home)
        .env("LANG", "en_US.UTF-8")
        .env("TMPDIR", std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
}

/// Reads `source` to its end on a thread, keeping `cap` bytes. A thread the OS will not start is an error, not a
/// panic: the caller (for a hands verb, the link's supervisor) must live on.
fn drain(
    mut source: impl Read + Send + 'static,
    cap: usize,
) -> std::io::Result<thread::JoinHandle<(Vec<u8>, bool)>> {
    thread::Builder::new()
        .name("tulpa-exec-drain".to_owned())
        .spawn(move || {
            let mut kept = Vec::new();
            let mut cut = false;
            let mut chunk = [0_u8; 8192];
            loop {
                match source.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = cap.saturating_sub(kept.len());
                        let take = n.min(room);
                        kept.extend_from_slice(chunk.get(..take).unwrap_or_default());
                        if take < n {
                            cut = true;
                        }
                    }
                }
            }
            (kept, cut)
        })
}

/// Runs `command` to its end, or kills its process group early.
///
/// The group dies when `paused` turns true, when `abandoned` turns true (the
/// requester went away — the link died; a stop that, once set, stays set), or
/// when `limit` passes. Output past
/// `cap` per stream is read and dropped. With `paused` already true nothing is
/// spawned: the run ends [`Ended::Paused`] at once.
///
/// # Errors
///
/// Returns an error when the child cannot be spawned or its status cannot be
/// read.
pub fn run(
    mut command: Command,
    paused: &Stop,
    abandoned: &Stop,
    limit: Duration,
    cap: usize,
) -> std::io::Result<Outcome> {
    let deadline = Instant::now().checked_add(limit);
    // Counted before it exists: a shutdown that sets Pause and then waits in `wait_reaped` either waits for this
    // child or is seen here, before any spawn. A failed spawn drops the count with the guard.
    let reaping = UNREAPED.enter();
    if paused.is_set() {
        return Ok(Outcome {
            ended: Ended::Paused,
            stdout: Vec::new(),
            stderr: Vec::new(),
            cut: false,
        });
    }
    let mut child = command.spawn()?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    // Watched before the drains start: a drain thread that cannot be made returns its error, and the group's drop
    // on the way out kills the child (the waiter reaps it) and uncounts it, instead of leaving it running. Not
    // `spawn_beside`: that reaps the child itself, and the waiter already owns the reaping.
    let (group, exited) = watch(child, reaping)?;
    let stdout = out.map(|s| drain(s, cap)).transpose()?;
    let stderr = err.map(|s| drain(s, cap)).transpose()?;
    let lost = || std::io::Error::other("the child's wait thread ended without its status");
    let ended = match exited.recv_until(&[paused, abandoned], deadline) {
        // Whatever the child left behind in its group (a `&` job holding the pipe) went with it, or the drains
        // would wait on it: the waiter killed the group between the exit and the reap (`Group::reap`).
        Got::Item(status) => Ended::Exit(status?.code()),
        Got::Ended => return Err(lost()),
        got @ (Got::Stopped | Got::Timeout) => {
            let why = killed_for(matches!(got, Got::Stopped), abandoned);
            group.kill();
            // Reaped before the outcome is told: nothing of it outlives `run`.
            match exited.recv_until(&[], None) {
                Got::Item(_) => why,
                _ => return Err(lost()),
            }
        }
    };
    let join = |h: Option<thread::JoinHandle<(Vec<u8>, bool)>>| {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    drop(group);
    let (stdout, cut_out) = join(stdout);
    let (stderr, cut_err) = join(stderr);
    Ok(Outcome {
        ended,
        stdout,
        stderr,
        cut: cut_out || cut_err,
    })
}

/// Why a run that did not exit is killed, from what its wait returned: `stopped` (a stop woke it) or not (the
/// limit passed). Not from a second read of Pause: a resume can clear it after it woke the wait.
///
/// Abandon is the only other stop, and it is read here, after the wake. That read is sound because it stays set:
/// the tray hands each request the stop of the link attempt it came on (`tulpa_stop::Attempts`), set when that
/// attempt's pipe dies and never cleared, so a redial between the wake and this read cannot turn a link-death kill
/// into `Paused`.
fn killed_for(stopped: bool, abandoned: &Stop) -> Ended {
    match (stopped, abandoned.is_set()) {
        (false, _) => Ended::Timeout,
        (true, true) => Ended::Abandoned,
        (true, false) => Ended::Paused,
    }
}

/// Hands `child`'s exit to a waiter thread (the exit is an event: Pause, the link's death and the limit end the same
/// wait), and returns its group, which keeps `counted` until it is dropped.
///
/// The count goes with the group's kill, on every way out of [`run`]: `wait_reaped` returns only once what the
/// child left in its group is killed too, not as soon as the child itself is reaped. A waiter that cannot start
/// drops the group here: killed, then uncounted.
fn watch(
    mut child: Child,
    counted: Reaping<'_>,
) -> std::io::Result<(Watched<'_>, Receiver<std::io::Result<ExitStatus>>)> {
    let group = Watched {
        group: Group::of(&child),
        _counted: counted,
    };
    let (tx, exited) = tulpa_stop::channel();
    let reaper = Arc::clone(&group.group);
    #[cfg(test)]
    tests::WATCHED.with(|w| *w.borrow_mut() = Some(Arc::clone(&group.group)));
    group.group.spawn_beside("tulpa-exec-wait", move || {
        let status = reaper.reap(&mut child);
        let _told: Result<(), tulpa_stop::Gone> = tx.send(status);
    })?;
    Ok((group, exited))
}

/// Starts `body` on a thread named `name` beside the live child `pid` (a child of this process, in its own process
/// group), which the thread or its caller owns.
///
/// Only for a child with no [`Group`]; one that has a [`Group`] uses [`Group::spawn_beside`], since its reap
/// must go through its group.
///
/// If the OS will not start a thread, the child's group is killed and the child reaped before the error is
/// returned: a child is never left running, or unreaped, for want of the thread that was to own it. `std`'s
/// `thread::spawn` would panic there instead, dropping the `Child`, which neither kills nor waits.
///
/// # Errors
///
/// The thread's start, after the child is gone.
pub fn spawn_beside<T: Send + 'static>(
    pid: u32,
    name: &str,
    body: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<thread::JoinHandle<T>> {
    start_beside(name, body, || {
        if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
            kill_and_reap(pid);
        }
    })
}

/// Starts `body` on a thread named `name`; if the OS refuses, runs `refused` (which ends the child) first.
fn start_beside<T: Send + 'static>(
    name: &str,
    body: impl FnOnce() -> T + Send + 'static,
    refused: impl FnOnce(),
) -> std::io::Result<thread::JoinHandle<T>> {
    #[cfg(test)]
    if tests::REFUSE.with(|r| r.borrow_mut().take_if(|refused| *refused == name).is_some()) {
        refused();
        return Err(std::io::Error::other("refused by the test"));
    }
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .inspect_err(|_| refused())
}

/// Kills `pid`'s process group, then reaps `pid`, a child of this process.
fn kill_and_reap(pid: Pid) {
    let _killed: rustix::io::Result<()> = rustix::process::kill_process_group(pid, Signal::KILL);
    let _reaped: rustix::io::Result<Option<(Pid, rustix::process::WaitStatus)>> =
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
}

/// A child's process group (the child leads it), signalled only while the child is unreaped.
///
/// A group's id is its leader's pid. Once the leader is reaped the kernel may give that pid, and so that group id,
/// to an unrelated process: a signal sent to the group after the reap could land there. So every signal goes
/// through here, under the lock the reap takes, and does nothing once the leader is reaped.
#[derive(Debug)]
pub struct Group {
    pid: Pid,
    reaped: Mutex<bool>,
    /// Whether its owner has stopped waiting for the reap ([`Group::give_up`]).
    given_up: std::sync::atomic::AtomicBool,
}

impl Group {
    /// The group `child` leads (a child started in its own process group, as [`cleared`] starts it).
    #[must_use]
    pub fn of(child: &Child) -> Arc<Self> {
        Arc::new(Self {
            pid: Pid::from_child(child),
            reaped: Mutex::new(false),
            given_up: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Sends `signal` to the group, unless its leader was reaped; whether it was sent (best effort: a group that
    /// is gone already takes nothing).
    pub fn signal(&self, signal: Signal) -> bool {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return false;
        }
        let _gone: rustix::io::Result<()> = rustix::process::kill_process_group(self.pid, signal);
        drop(reaped);
        true
    }

    /// Kills the group ([`Group::signal`] with `SIGKILL`); whether the signal was sent.
    pub fn kill(&self) -> bool {
        self.signal(Signal::KILL)
    }

    /// [`spawn_beside`] for this group's leader: a thread the OS refuses kills the group and reaps the leader
    /// through here ([`Group::kill_and_reap`]), so the group knows it is reaped and sends nothing more.
    ///
    /// # Errors
    ///
    /// The thread's start, after the leader is reaped.
    pub fn spawn_beside<T: Send + 'static>(
        &self,
        name: &str,
        body: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<thread::JoinHandle<T>> {
        start_beside(name, body, || self.kill_and_reap())
    }

    /// Kills the group and reaps its leader (what [`Group::spawn_beside`] does when no thread can be had), marking
    /// it reaped under the lock signals take: no signal follows the reap. Nothing at all once it is reaped.
    ///
    /// Only for a leader nobody else is reaping (no waiter thread holds its `Child`).
    ///
    /// The lock is held only to send the kill and, later, to reap: the wait for the exit between them is
    /// [`Group::reap`]'s, without it. A killed leader stuck in the kernel may take long to die, and a signal sent
    /// meanwhile (Pause, Quit) goes through at once; the pid is still the leader's until the reap.
    pub fn kill_and_reap(&self) {
        self.kill_and_reap_after(|| self.exited());
    }

    /// [`Group::kill_and_reap`], with the wait for the exit given (as [`Group::reap_after`]).
    fn kill_and_reap_after(&self, exit: impl Fn() -> rustix::io::Result<()>) {
        if self.kill() {
            let mut leader = self.pid;
            let _reaped: std::io::Result<()> = self.reap_after(&mut leader, exit);
        }
    }

    /// Waits for the leader's exit without reaping it (`waitid` with `WNOWAIT`).
    fn exited(&self) -> rustix::io::Result<()> {
        let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        rustix::process::waitid(WaitId::Pid(self.pid), exited).map(drop)
    }

    /// Its owner waits for the leader's reap no longer (the tray gives a capture up when even its kill did not end
    /// it). The reap goes on, so the leader is never left a zombie, but where its exit can only be looked for
    /// ([`Group::reap`]'s fallback) it is looked for every [`REAP_LOOK_GIVEN_UP`], not every [`REAP_LOOK`]: a
    /// leader that may never die is not woken for twenty times a second for the rest of the process's life.
    pub fn give_up(&self) {
        self.given_up
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`Group::give_up`] was called.
    #[must_use]
    pub fn is_given_up(&self) -> bool {
        self.given_up.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Waits for `child` (this group's leader) to exit, kills what it left behind in its group, and reaps it.
    ///
    /// The exit is learned without reaping (`waitid` with `WNOWAIT`): while the leader is a zombie its pid is
    /// still its own, so the kill between the exit and the reap can only reach its own group. From the reap on,
    /// [`Group::signal`] sends nothing.
    ///
    /// # Errors
    ///
    /// The reap's.
    pub fn reap(&self, child: &mut Child) -> std::io::Result<ExitStatus> {
        self.reap_after(child, || self.exited())
    }

    /// [`Group::reap`], with the wait for the exit given (`waitid` there; a test's stand-in here) and the leader's
    /// looks and reap through [`Leader`] (a test's stand-in [`Leader`] makes the looks fail).
    ///
    /// What the wait's answer means:
    /// - `Ok`: the leader has exited and is still a zombie. Its group is killed (what it left behind), then it is
    ///   reaped (a reap a signal interrupts is waited again; one that fails otherwise, not for want of the child,
    ///   is left to the looks below).
    /// - `EINTR`: a signal interrupted the wait. Waited again.
    /// - `ECHILD`: there is no such child to wait for: it is reaped already. That counts as exited, and the group
    ///   is NOT signalled: its id may be another process's by now. The reap below then says what it finds.
    /// - anything else (`EINVAL` or `ENOSYS` if this system refuses the options): the exit cannot be learned
    ///   without reaping, and the child may be alive. It is NOT killed. Its real exit is looked for every
    ///   [`REAP_LOOK`] ([`REAP_LOOK_GIVEN_UP`] once [`Group::give_up`] is called), the lock taken only for each
    ///   look, so a kill is never kept waiting; what it left in its group is then not killed (never after the
    ///   reap). Said once on stderr. Only a look that finds no such
    ///   child (`ECHILD`) counts as the reap; any other failed look leaves the group unreaped, and killable, and
    ///   the looks go on (said once on stderr too).
    fn reap_after<L: Leader>(
        &self,
        child: &mut L,
        exit: impl Fn() -> rustix::io::Result<()>,
    ) -> std::io::Result<L::Status> {
        let zombie = loop {
            match exit() {
                Ok(()) => break true,
                Err(rustix::io::Errno::INTR) => {}
                Err(rustix::io::Errno::CHILD) => break false,
                Err(e) => {
                    say_once(
                        &SAID_NO_WAITID,
                        &format!(
                            "waitid without a reap is refused ({e}): a child's leftover group is not killed at \
                             its exit"
                        ),
                    );
                    break false;
                }
            }
        };
        if zombie && let Some(status) = self.reap_zombie(child) {
            return status;
        }
        // The exit could not be learned without reaping (or the zombie's reap failed), and the child may live for
        // long. The lock a kill needs is never held across a wait: each look takes it for an instant, reaps if the
        // child is gone (and marks the group reaped in the same instant, so a signal can never follow the reap),
        // and lets it go. A kill sent between two looks goes through at once, and the next look reaps what it
        // killed.
        //
        // A POLL, and why: on this path there is no event for "the child exited" that does not reap it. `waitid`
        // with `WNOWAIT` is that event and was just refused; a blocking `wait` reaps, and would have to hold the
        // lock for the child's whole life; a kqueue watch needs `unsafe`. So `try_wait` every `REAP_LOOK`.
        loop {
            match self.look(child) {
                Ok(None) => {}
                Ok(Some(status)) => return Ok(status),
                // No such child any more (reaped already): nothing is left to wait for or to signal.
                Err(e) if no_such_child(&e) => return Err(e),
                // The child may well be alive: it stays unreaped, and killable, and is looked at again. Said with
                // the lock let go: a write to stderr may block, and a kill must never wait behind it.
                Err(e) => say_once(
                    &SAID_NO_LOOK,
                    &format!("a look at a child's exit failed ({e}); looked at again"),
                ),
            }
            // Read at each look: a give-up slows the very next one.
            thread::sleep(if self.is_given_up() {
                REAP_LOOK_GIVEN_UP
            } else {
                REAP_LOOK
            });
        }
    }

    /// Reaps `child` (this group's leader), an exited zombie, after killing what it left in its group (its pid is
    /// still its own until the reap). Under the lock signals take, which the reap of a zombie holds for an instant
    /// only: the group is marked reaped with the reap, so no signal follows it. `None` if the reap failed other
    /// than for want of the child (it is no reap: the caller looks again).
    fn reap_zombie<L: Leader>(&self, child: &mut L) -> Option<std::io::Result<L::Status>> {
        let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        // Reaped by another already (two refused starts at once): its id may be another process's by now.
        if !*reaped {
            let _gone: rustix::io::Result<()> =
                rustix::process::kill_process_group(self.pid, Signal::KILL);
        }
        let status = loop {
            match child.wait() {
                // A signal interrupted the wait: nothing was reaped, the zombie is waited for again (as std's
                // `Child::wait` does; a bare pid's `waitpid` does not).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // Only "no such child" is a reap: any other failure leaves the leader to a look.
                Err(e) if !no_such_child(&e) => break None,
                status => {
                    *reaped = true;
                    break Some(status);
                }
            }
        };
        drop(reaped);
        status
    }

    /// One look at `child` (this group's leader) without waiting: reaps it if it has exited, under the lock
    /// signals take, and marks the group reaped in the same instant (also when there is no such child: reaped
    /// already). `None` while it runs; any other failure leaves it unreaped, and killable.
    fn look<L: Leader>(&self, child: &mut L) -> std::io::Result<Option<L::Status>> {
        let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        let looked = child.try_wait();
        if matches!(&looked, Ok(Some(_))) || looked.as_ref().is_err_and(no_such_child) {
            *reaped = true;
        }
        looked
    }

    /// Reaps `child` (this group's leader) if it has exited, never waiting; `None` while it runs. As
    /// [`Group::reap`]: what the leader left in its group is killed between its exit and its reap, and from the
    /// reap on [`Group::signal`] sends nothing. Where the exit cannot be learned without reaping (`waitid` with
    /// `WNOWAIT` refused), it is one look ([`Group::reap`]'s fallback), and the group is not killed.
    ///
    /// # Errors
    ///
    /// The reap's; any but "no such child" leaves the leader unreaped, and killable.
    pub fn try_reap(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        match rustix::process::waitid(WaitId::Pid(self.pid), exited) {
            Ok(None) => Ok(None),
            Ok(Some(_)) => self
                .reap_zombie(child)
                .map_or_else(|| self.look(child), |status| status.map(Some)),
            // Reaped already (`ECHILD`), or the exit cannot be learned without reaping: a look tells which.
            Err(_) => self.look(child),
        }
    }
}

/// Whether a wait or a look failed because there is no such child (`ECHILD`): the one failure that means reaped.
fn no_such_child(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error())
}

/// A group's leader as [`Group::reap_after`] reaps it: its `Child` ([`Group::reap`]), or its bare pid
/// ([`Group::kill_and_reap`], which has no `Child`).
trait Leader {
    /// What a reap tells.
    type Status;
    /// Reaps the leader, waiting for its exit.
    fn wait(&mut self) -> std::io::Result<Self::Status>;
    /// Reaps the leader if it has exited.
    fn try_wait(&mut self) -> std::io::Result<Option<Self::Status>>;
}

impl Leader for Child {
    type Status = ExitStatus;
    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Self::wait(self)
    }
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Self::try_wait(self)
    }
}

impl Leader for Pid {
    type Status = ();
    fn wait(&mut self) -> std::io::Result<()> {
        Ok(
            rustix::process::waitpid(Some(*self), rustix::process::WaitOptions::empty())
                .map(drop)?,
        )
    }
    fn try_wait(&mut self) -> std::io::Result<Option<()>> {
        let now = rustix::process::waitpid(Some(*self), rustix::process::WaitOptions::NOHANG)?;
        Ok(now.map(drop))
    }
}

/// How often a child's exit is looked for when it cannot be waited for without reaping ([`Group::reap`]'s
/// fallback): a kill is reaped within this.
const REAP_LOOK: Duration = Duration::from_millis(50);

/// [`REAP_LOOK`] for a group whose owner has given up waiting ([`Group::give_up`]): still a poll (there is no
/// event on this path), slowed because the leader may never die; a kill is reaped within this.
const REAP_LOOK_GIVEN_UP: Duration = Duration::from_secs(5);

/// Whether a refused `waitid` was said on stderr: once per process.
static SAID_NO_WAITID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a failed look at a child's exit (other than "no such child") was said on stderr: once per process.
static SAID_NO_LOOK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says `what` on stderr, the first time `said` is asked only.
fn say_once(said: &std::sync::atomic::AtomicBool, what: &str) {
    if !said.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _said: std::io::Result<()> = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("tulpa-hands: {what}\n").as_bytes(),
        );
    }
}

/// A [`run`]'s child's group, counted in [`UNREAPED`]: killed on drop if its leader is still unreaped, then
/// uncounted.
struct Watched<'a> {
    group: Arc<Group>,
    _counted: Reaping<'a>,
}

impl Watched<'_> {
    fn kill(&self) {
        let _sent: bool = self.group.kill();
    }
}

impl Drop for Watched<'_> {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Children [`run`] started and has not reaped yet, in this whole process.
static UNREAPED: Unreaped = Unreaped::new();

/// Waits until every child [`run`] started is reaped and its group killed, at most `cap`; whether they all are.
///
/// It counts every `run` in the process. A run ends early only on its own `paused` or `abandoned`, so a shutdown that
/// sets one stop returns at once only when every run in flight was given that stop; any other waits for its child's
/// exit or its limit, up to `cap`. A shutdown waits on this instead of a fixed sleep.
pub fn wait_reaped(cap: Duration) -> bool {
    UNREAPED.wait_none(Instant::now().checked_add(cap))
}

/// A count of children not yet reaped, and the event of it reaching zero.
#[derive(Debug)]
struct Unreaped {
    count: Mutex<usize>,
    none: Condvar,
}

/// One child counted in [`Unreaped`]; dropped once it is reaped.
struct Reaping<'a>(&'a Unreaped);

impl Drop for Reaping<'_> {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap_or_else(PoisonError::into_inner);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.0.none.notify_all();
        }
    }
}

impl Unreaped {
    const fn new() -> Self {
        Self {
            count: Mutex::new(0),
            none: Condvar::new(),
        }
    }

    fn enter(&self) -> Reaping<'_> {
        {
            let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
            *count = count.saturating_add(1);
        }
        Reaping(self)
    }

    fn wait_none(&self, deadline: Option<Instant>) -> bool {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        while *count > 0 {
            let Some(left) = deadline.and_then(|d| d.checked_duration_since(Instant::now())) else {
                return false;
            };
            count = self
                .none
                .wait_timeout(count, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(count);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Test seam: the next thread start of this name on this thread is refused, as the OS may refuse it.
        pub(super) static REFUSE: std::cell::RefCell<Option<&'static str>> =
            const { std::cell::RefCell::new(None) };
        /// The group the last [`watch`] on this thread made, kept for the test to look at after [`run`].
        pub(super) static WATCHED: std::cell::RefCell<Option<Arc<Group>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A stop that woke the wait decides the kill, not a second read of the flags: Pause set (it woke the wait),
    /// then a resume cleared it before the flags were read, is still `Paused`, never `Timeout` (the limit is far
    /// off). Abandon is the only other stop. Only the deadline is a `Timeout`.
    #[test]
    fn a_pause_cleared_after_it_woke_the_wait_is_still_paused() {
        let (paused, abandoned) = (Stop::new(), Stop::new());
        paused.set();
        paused.clear();
        assert!(!paused.is_set());
        assert_eq!(killed_for(true, &abandoned), Ended::Paused);
        abandoned.set();
        assert_eq!(killed_for(true, &abandoned), Ended::Abandoned);
        assert_eq!(killed_for(false, &abandoned), Ended::Timeout);
    }

    #[test]
    fn the_child_sees_only_the_cleared_environment() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/usr/bin/env"), Path::new("/home/x"));
        command.env_remove("TMPDIR");
        let paused = Stop::new();
        let never = Stop::new();
        let out = run(command, &paused, &never, Duration::from_secs(5), 4096)?;
        let text = String::from_utf8(out.stdout)?;
        assert!(text.contains("PATH=/usr/bin"), "{text}");
        assert!(text.contains("HOME=/home/x"));
        assert!(!text.contains("SSH_AUTH_SOCK"), "{text}");
        assert_eq!(
            text.lines().count(),
            3,
            "PATH, HOME, LANG and nothing else: {text}"
        );
        Ok(())
    }

    /// Sets `stop` once `run` is waiting on its child, and returns when it was set.
    /// The guard on a wait in these tests: far above how long any step takes.
    const WAIT: Duration = Duration::from_secs(60);
    /// A run's limit past [`WAIT`]: a stop seen at the limit also ends the run `Paused` or `Abandoned`, so only a stop
    /// that woke the run ends it before the guard.
    const PAST_GUARD: Duration = Duration::from_secs(3600);

    fn set_soon(stop: &std::sync::Arc<Stop>) -> thread::JoinHandle<Instant> {
        let stop = std::sync::Arc::clone(stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let at = Instant::now();
            stop.set();
            at
        })
    }

    /// Pause kills the whole group (a background job too), and the pause wakes `run` (the old loop looked every 50 ms).
    #[test]
    fn pause_kills_the_group_at_once() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 30 & sleep 30"]);
        let paused = std::sync::Arc::new(Stop::new());
        let stopper = set_soon(&paused);
        let never = Stop::new();
        let out = run(command, &paused, &never, PAST_GUARD, 4096)?;
        let took = stopper.join().map_err(|_| "setter")?.elapsed();
        assert_eq!(out.ended, Ended::Paused);
        assert!(took < WAIT, "{took:?}");
        Ok(())
    }

    #[test]
    fn abandon_kills_the_group_at_once() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 30 & sleep 30"]);
        let paused = Stop::new();
        let dead = std::sync::Arc::new(Stop::new());
        let dropper = set_soon(&dead);
        let out = run(command, &paused, &dead, PAST_GUARD, 4096)?;
        let took = dropper.join().map_err(|_| "setter")?.elapsed();
        assert_eq!(out.ended, Ended::Abandoned);
        assert!(took < WAIT, "{took:?}");
        Ok(())
    }

    /// A child that exits ends `run`, woken by the exit (the old loop looked every 50 ms): the limit is the guard, and
    /// a run it ends is `Timeout`, not an exit.
    #[test]
    fn an_exit_ends_the_run_at_once() -> Result<(), Box<dyn std::error::Error>> {
        let never = Stop::new();
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 0.2"]);
        let out = run(command, &never, &never, WAIT, 64)?;
        assert_eq!(out.ended, Ended::Exit(Some(0)));
        Ok(())
    }

    /// A child that exits on its own, leaving a job in its process group: the job is killed between the child's
    /// exit and its reap (never after it: a reaped leader's group id may be another process's by then), so it is
    /// dead once the exit is told. The group stays counted (shutdown's `wait_reaped` waits) until it is dropped.
    #[test]
    fn a_job_left_behind_dies_at_the_reap_and_the_group_is_counted_until_dropped()
    -> Result<(), Box<dyn std::error::Error>> {
        // Its own count, not the process-wide one other tests share.
        static COUNTED: Unreaped = Unreaped::new();
        let count = || *COUNTED.count.lock().unwrap_or_else(PoisonError::into_inner);
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 60 & exit 0"]);
        let counted = COUNTED.enter();
        let mut child = command.spawn()?;
        // The job keeps the pipe open: its EOF is the job's death.
        let job_out = child.stdout.take().ok_or("no stdout")?;
        let (group, exited) = watch(child, counted)?;
        let far = Instant::now().checked_add(Duration::from_secs(10));
        assert!(
            matches!(exited.recv_until(&[], far), Got::Item(Ok(_))),
            "the leader exits"
        );
        let eof = eof_of(job_out);
        assert!(ended_in_time(&eof), "the job outlived its leader's reap");
        assert_eq!(count(), 1, "reaped, its job dead: counted until the drop");
        assert!(
            !group.group.kill(),
            "nothing is sent to the group of a reaped leader"
        );
        drop(group);
        assert_eq!(count(), 0);
        Ok(())
    }

    /// `kill_and_reap` (what `spawn_beside` does when no thread can be had) leaves nothing of the child: its group is
    /// killed, and the child is reaped (a second wait finds no child).
    #[test]
    fn kill_and_reap_leaves_no_child() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 60 & sleep 60"]);
        let mut child = command.spawn()?;
        let eof = eof_of(child.stdout.take().ok_or("no stdout")?);
        kill_and_reap(Pid::from_child(&child));
        assert!(
            child.try_wait().is_err(),
            "reaped already: no child to wait for"
        );
        assert!(ended_in_time(&eof), "the job outlived the kill");
        Ok(())
    }

    /// #0532: a child that has a [`Group`] is killed and reaped through it on a refused thread start
    /// ([`Group::spawn_beside`]): its job dies, the child is reaped, and the group knows it, so a later kill (a
    /// Pause, a Quit) sends nothing to a group id that may be another process's by now.
    #[test]
    fn a_kill_and_reap_through_the_group_leaves_it_reaped() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 60 & sleep 60"]);
        let mut child = command.spawn()?;
        let eof = eof_of(child.stdout.take().ok_or("no stdout")?);
        let group = Group::of(&child);
        group.kill_and_reap();
        assert!(
            child.try_wait().is_err(),
            "reaped already: no child to wait for"
        );
        assert!(ended_in_time(&eof), "the job outlived the kill");
        assert!(!group.kill(), "reaped: a later kill sends nothing");
        Ok(())
    }

    /// The EOF of a pipe a `sleep 60` job holds, told on a channel by a reader thread.
    fn eof_of(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<()> {
        let (tx, eof) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut rest = Vec::new();
            if pipe.read_to_end(&mut rest).is_ok() {
                let _told = tx.send(());
            }
        });
        eof
    }

    /// Whether the pipe ended within 5 s: a killed job closes it at once (even before it is reaped), and on its own
    /// the job holds it 60 s, far past this bound, so an EOF in time is the kill's doing.
    fn ended_in_time(eof: &std::sync::mpsc::Receiver<()>) -> bool {
        eof.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    /// The reaped count wakes its waiter when the last child is reaped, and gives up at the cap.
    #[test]
    fn wait_none_wakes_on_the_last_reap() -> Result<(), Box<dyn std::error::Error>> {
        let unreaped = std::sync::Arc::new(Unreaped::new());
        let held = std::sync::Arc::clone(&unreaped);
        let (tx, rx) = std::sync::mpsc::channel();
        let reaper = thread::spawn(move || {
            let one = held.enter();
            tx.send(())?;
            thread::sleep(Duration::from_millis(100));
            drop(one);
            Ok::<(), std::sync::mpsc::SendError<()>>(())
        });
        rx.recv()?;
        let short = Instant::now().checked_add(Duration::from_millis(20));
        assert!(!unreaped.wait_none(short), "still one unreaped at the cap");
        // The wait has no tick: only the reap's wake ends it before the guard.
        assert!(unreaped.wait_none(Instant::now().checked_add(WAIT)));
        reaper.join().map_err(|_| "reaper")??;
        Ok(())
    }

    /// A run that starts after Pause spawns nothing: shutdown's [`wait_reaped`] cannot pass while a child is being
    /// made. A program that does not exist shows it: it is never looked up.
    #[test]
    fn a_run_after_pause_spawns_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let command = cleared(Path::new("/nonexistent/tulpa-exec-test"), Path::new("/"));
        let paused = Stop::new();
        paused.set();
        let out = run(command, &paused, &Stop::new(), Duration::from_secs(5), 64)?;
        assert_eq!(out.ended, Ended::Paused);
        Ok(())
    }

    #[test]
    fn caps_output_and_times_out() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "yes | head -c 100000"]);
        let paused = Stop::new();
        let never = Stop::new();
        let out = run(command, &paused, &never, Duration::from_secs(5), 1000)?;
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.cut);
        let mut slow = cleared(Path::new("/bin/sleep"), Path::new("/"));
        slow.arg("10");
        let out = run(slow, &paused, &never, Duration::from_millis(300), 1000)?;
        assert_eq!(out.ended, Ended::Timeout);
        Ok(())
    }

    /// The gate's S5: a wait for the exit that fails does not kill a child that may be alive. With the wait
    /// refused (`EINVAL`, as a system without these `waitid` options would), and with no such child (`ECHILD`),
    /// the child runs to its own end and its own exit code: it was not signalled.
    #[test]
    fn a_refused_wait_for_the_exit_does_not_kill_the_child()
    -> Result<(), Box<dyn std::error::Error>> {
        for refused in [rustix::io::Errno::INVAL, rustix::io::Errno::CHILD] {
            let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
            command.args(["-c", "sleep 0.3; exit 7"]);
            let mut child = command.spawn()?;
            let group = Group::of(&child);
            let status = group.reap_after(&mut child, || Err(refused))?;
            assert_eq!(status.code(), Some(7), "{refused:?}: it ran to its own end");
            assert!(!group.kill(), "{refused:?}: reaped, nothing more is sent");
        }
        Ok(())
    }

    /// A [`Leader`] whose looks at the exit are the test's (`look`); its blocking reap is the child's own.
    struct Looked<F> {
        child: Child,
        look: F,
    }

    impl<F: FnMut(&mut Child) -> std::io::Result<Option<ExitStatus>>> Leader for Looked<F> {
        type Status = ExitStatus;
        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            self.child.wait()
        }
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            (self.look)(&mut self.child)
        }
    }

    /// #0530: a reap that fails is not the reap, unless for want of the child. The zombie leader's first reap
    /// fails: interrupted (`EINTR`, as a bare pid's `waitpid` can be), it is waited again; failed otherwise
    /// (`EINVAL`), it is left to the looks. Either way the reap returns the leader's own status, no zombie is left,
    /// and the group knows it.
    #[test]
    fn a_failed_reap_is_not_taken_as_the_reap() -> Result<(), Box<dyn std::error::Error>> {
        /// The leader, whose first reap fails with `first`.
        struct Failing {
            child: Child,
            first: Option<rustix::io::Errno>,
            waits: usize,
        }
        impl Leader for Failing {
            type Status = ExitStatus;
            fn wait(&mut self) -> std::io::Result<ExitStatus> {
                self.waits += 1;
                if let Some(first) = self.first.take() {
                    return Err(std::io::Error::from_raw_os_error(first.raw_os_error()));
                }
                self.child.wait()
            }
            fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
                self.child.try_wait()
            }
        }
        for (first, waits) in [(rustix::io::Errno::INTR, 2), (rustix::io::Errno::INVAL, 1)] {
            let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
            command.args(["-c", "exit 3"]);
            let child = command.spawn()?;
            let group = Group::of(&child);
            let pid = group.pid;
            let mut leader = Failing {
                child,
                first: Some(first),
                waits: 0,
            };
            let status = group.reap_after(&mut leader, || group.exited());
            assert_eq!(
                status.map(|s| s.code()).ok(),
                Some(Some(3)),
                "{first:?}: the failed reap was taken as the reap"
            );
            assert_eq!(leader.waits, waits, "{first:?}: the blocking reaps");
            assert_eq!(
                rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).err(),
                Some(rustix::io::Errno::CHILD),
                "{first:?}: no zombie is left"
            );
            assert!(!group.kill(), "{first:?}: reaped, nothing more is sent");
        }
        Ok(())
    }

    /// #0530: only "no such child" (`ECHILD`) means reaped. With the wait for the exit refused, a look that fails
    /// any other way leaves the group unreaped: a kill still goes through, and the looks go on until the child's
    /// real exit, which they reap. A look that finds no such child ends the reap, and nothing more is sent.
    #[test]
    fn only_no_such_child_counts_as_reaped() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 30"]);
        let child = command.spawn()?;
        let group = Group::of(&child);
        let reaper = Arc::clone(&group);
        // The looks fail (EINVAL) until the test has killed the child; then they are real.
        let killed = Arc::new(Stop::new());
        let (failed, failing) = (
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::clone(&killed),
        );
        let failures = Arc::clone(&failed);
        let (ended_tx, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut looked = Looked {
                child,
                look: |child: &mut Child| {
                    if failing.is_set() {
                        return child.try_wait();
                    }
                    failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(std::io::Error::from_raw_os_error(
                        rustix::io::Errno::INVAL.raw_os_error(),
                    ))
                },
            };
            let status = reaper.reap_after(&mut looked, || Err(rustix::io::Errno::INVAL));
            let _told = ended_tx.send(status.map(|s| s.code()).ok());
        });
        // A guard against a wedge; a poll, in the test only: the looks give the test no event.
        let started = Instant::now();
        while failed.load(std::sync::atomic::Ordering::SeqCst) < 2
            && started.elapsed() < Duration::from_secs(30)
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            failed.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a failed look ended the looks"
        );
        assert!(group.kill(), "a failed look left the group unkillable");
        killed.set();
        assert_eq!(
            ended.recv_timeout(Duration::from_secs(30)),
            Ok(Some(None)),
            "the child died of the kill, and was reaped"
        );
        assert!(!group.kill(), "reaped: nothing more is sent");
        // No such child: that is the reap.
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "exit 0"]);
        let child = command.spawn()?;
        let gone = Group::of(&child);
        let mut looked = Looked {
            child,
            look: |child: &mut Child| {
                child.wait()?;
                Err(std::io::Error::from_raw_os_error(
                    rustix::io::Errno::CHILD.raw_os_error(),
                ))
            },
        };
        let status = gone.reap_after(&mut looked, || Err(rustix::io::Errno::INVAL));
        assert!(status.is_err(), "no such child: no status");
        assert!(!gone.kill(), "no such child: nothing is sent");
        Ok(())
    }

    /// #0530: a given-up leader is still reaped, but its exit is no longer looked for every 50 ms. With the wait
    /// for the exit refused, a group given up gets one look per [`REAP_LOOK_GIVEN_UP`]; a kill still ends the
    /// leader, and a later look reaps it (no zombie is left).
    #[test]
    fn a_given_up_group_is_looked_at_slowly_and_still_reaped()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 30"]);
        let child = command.spawn()?;
        let group = Group::of(&child);
        group.give_up();
        let reaper = Arc::clone(&group);
        let looks = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&looks);
        let (ended_tx, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut looked = Looked {
                child,
                look: |child: &mut Child| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    child.try_wait()
                },
            };
            let status = reaper.reap_after(&mut looked, || Err(rustix::io::Errno::INVAL));
            let _told = ended_tx.send(status.map(|s| s.code()).ok());
        });
        // A guard against a wedge; a poll, in the test only: the looks give the test no event.
        let started = Instant::now();
        while looks.load(Ordering::SeqCst) == 0 && started.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(10));
        }
        let first = Instant::now();
        // The window the slow looks are counted over: twenty 50 ms looks would fit in it.
        thread::sleep(Duration::from_secs(1));
        let (seen, window) = (looks.load(Ordering::SeqCst), first.elapsed());
        let allowed = 1 + usize::try_from(window.as_millis() / REAP_LOOK_GIVEN_UP.as_millis())?;
        assert!(
            (1..=allowed).contains(&seen),
            "{seen} looks in {window:?} at a given-up group"
        );
        assert!(group.kill(), "a given-up group is still killable");
        assert_eq!(
            ended.recv_timeout(REAP_LOOK_GIVEN_UP + Duration::from_secs(30)),
            Ok(Some(None)),
            "the killed leader was not reaped"
        );
        Ok(())
    }

    /// The gate's N1: no lock a kill needs is held across a blocking wait. With the wait for the exit refused
    /// (`EINVAL`), the reap has to wait for the child's real exit; a kill sent meanwhile goes through at once and
    /// the child dies of it, long before its own end.
    #[test]
    fn a_kill_during_a_refused_wait_goes_through_at_once() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 30"]);
        let mut child = command.spawn()?;
        let group = Group::of(&child);
        let reaper = Arc::clone(&group);
        let (ended_tx, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let status = reaper.reap_after(&mut child, || Err(rustix::io::Errno::INVAL));
            let _told = ended_tx.send(status.map(|s| s.code()).ok());
        });
        // The one settle here: the reaper is inside its wait (it gives the test no event for that).
        thread::sleep(Duration::from_millis(300));
        let killer = Arc::clone(&group);
        let (sent_tx, sent) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _told = sent_tx.send(killer.kill());
        });
        // Guards against a wedge, far under the child's own 30 s.
        assert_eq!(
            sent.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the kill waited behind the reap's wait"
        );
        assert_eq!(
            ended.recv_timeout(Duration::from_secs(5)),
            Ok(Some(None)),
            "the child died of the kill, and was reaped"
        );
        assert!(!group.kill(), "reaped: nothing more is sent");
        Ok(())
    }

    /// #0532 r2: a run whose waiter thread the OS refuses kills and reaps its child through its [`Group`]: `run`
    /// fails, the group knows its leader is reaped, and the kill `run`'s way out sends (and any later one) sends
    /// nothing to a group id that may be another process's by now.
    #[test]
    fn a_refused_waiter_reaps_through_the_group() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 60"]);
        let never = Stop::new();
        REFUSE.with(|r| *r.borrow_mut() = Some("tulpa-exec-wait"));
        let ran = run(command, &never, &never, WAIT, 64);
        assert!(ran.is_err(), "no waiter: the run fails");
        let group = WATCHED
            .with(|w| w.borrow_mut().take())
            .ok_or("the run made no group")?;
        assert!(
            *group.reaped.lock().unwrap_or_else(PoisonError::into_inner),
            "the group knows its leader is reaped"
        );
        assert!(!group.kill(), "reaped: a later kill sends nothing");
        Ok(())
    }

    /// #0532 r2: a kill-and-reap holds no lock a signal needs across its wait for the leader's exit. While the
    /// wait is held (a stand-in that waits for the test's word, then the real one), a signal goes through at once;
    /// then the reap finishes and nothing more is sent.
    #[test]
    fn a_signal_during_a_kill_and_reap_goes_through_at_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut command = cleared(Path::new("/bin/sh"), Path::new("/"));
        command.args(["-c", "sleep 60"]);
        let mut child = command.spawn()?;
        let group = Group::of(&child);
        let (waiting_tx, waiting) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let go = Mutex::new(go);
        let (done_tx, done) = std::sync::mpsc::channel();
        let reaper = Arc::clone(&group);
        thread::spawn(move || {
            reaper.kill_and_reap_after(|| {
                let _told = waiting_tx.send(());
                // The test's word, or its end (the sender dropped).
                let _word = go.lock().unwrap_or_else(PoisonError::into_inner).recv();
                reaper.exited()
            });
            let _told = done_tx.send(());
        });
        waiting.recv_timeout(WAIT)?;
        let sender = Arc::clone(&group);
        let (sent_tx, sent) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _told = sent_tx.send(sender.signal(Signal::CONT));
        });
        let went = sent.recv_timeout(WAIT);
        drop(go_tx);
        assert_eq!(
            went,
            Ok(true),
            "the signal waited behind the kill-and-reap's wait"
        );
        done.recv_timeout(WAIT)?;
        assert!(
            child.try_wait().is_err(),
            "reaped already: no child to wait for"
        );
        assert!(!group.kill(), "reaped: nothing more is sent");
        Ok(())
    }
}
