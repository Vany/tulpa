//! The app's own life: one instance at a time, and children that die with it.
//!
//! Seen live on the Mac: a quit by signal (`launchctl kickstart -k`,
//! logout, `kill`) left the ears' capture group running, still writing into
//! the capture directories; and after a permission grant macOS relaunched the
//! app outside launchd, so two instances ran and the newer one's link was
//! refused. Here:
//! - [`TerminationWatch`]: SIGTERM, SIGINT and SIGHUP are blocked in every
//!   thread and taken by one thread with `sigwait`, which runs the shutdown
//!   (the capture group killed, the hands paused), then ends the process by
//!   that same signal. So a signal never ends Tulpa with exit status 0, as
//!   Quit does: under the installer's `KeepAlive = { SuccessfulExit = false }`
//!   launchd leaves Tulpa down after Quit and starts it again after a kill,
//!   as after a crash.
//!   Children start with an empty signal mask (the standard library resets it
//!   on spawn), so they still die by the signals they are sent.
//! - [`SingleInstance`]: an exclusive `flock` on a lock file holding the pid.
//!   A second instance takes over: it sends the holder SIGTERM (whose watch
//!   shuts it down cleanly), waits for the lock, then holds it itself.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, Signal};
use rustix::fs::{FlockOperation, flock};

/// How long a new instance waits for the old one to let go.
pub const TAKEOVER_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// Why the app could not become the one instance.
#[derive(Debug)]
#[non_exhaustive]
pub enum InstanceError {
    /// The lock file could not be opened or written.
    Io(std::io::Error),
    /// The holder did not let go within the wait.
    StillHeld {
        /// The holder's pid, as its lock file says.
        pid: Option<i32>,
    },
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "instance lock: {error}"),
            Self::StillHeld { pid } => match pid {
                Some(pid) => write!(f, "another tulpa (pid {pid}) still holds the instance lock"),
                None => f.write_str("another tulpa still holds the instance lock"),
            },
        }
    }
}

impl std::error::Error for InstanceError {}

/// The held instance lock; released when dropped or when the process ends.
#[derive(Debug)]
pub struct SingleInstance {
    _file: File,
    path: PathBuf,
}

impl SingleInstance {
    /// Becomes the one instance, taking over from a running one: `stop` is
    /// called with the holder's pid (the app sends it SIGTERM), then the lock
    /// is awaited up to `wait`.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Io`] when the lock file can't be opened or
    /// written, [`InstanceError::StillHeld`] when the holder keeps it.
    pub fn acquire(
        path: &Path,
        wait: Duration,
        stop: impl FnOnce(i32),
    ) -> Result<Self, InstanceError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(InstanceError::Io)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(InstanceError::Io)?;
        if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
            let holder = read_pid(&mut file);
            if let Some(pid) = holder {
                stop(pid);
            }
            let started = Instant::now();
            loop {
                if flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok() {
                    break;
                }
                if started.elapsed() >= wait {
                    return Err(InstanceError::StillHeld { pid: holder });
                }
                thread::sleep(POLL);
            }
        }
        file.set_len(0).map_err(InstanceError::Io)?;
        file.seek(SeekFrom::Start(0)).map_err(InstanceError::Io)?;
        writeln!(file, "{}", std::process::id()).map_err(InstanceError::Io)?;
        file.flush().map_err(InstanceError::Io)?;
        Ok(Self {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// Where the lock lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_pid(file: &mut File) -> Option<i32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(64).read_to_string(&mut text).ok()?;
    text.trim().parse().ok().filter(|pid: &i32| *pid > 0)
}

/// Sends SIGTERM to `pid` (the old instance); a pid that is gone is no error.
pub fn terminate(pid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid) {
        let _sent: rustix::io::Result<()> =
            rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

/// The termination signals, blocked in the calling thread and every thread it
/// starts afterwards; [`TerminationWatch::spawn`] takes them on its own thread.
#[derive(Debug)]
pub struct TerminationWatch {
    set: SigSet,
}

impl TerminationWatch {
    /// Blocks SIGTERM, SIGINT and SIGHUP. Call first, before any thread starts.
    ///
    /// # Errors
    ///
    /// Returns the error of `pthread_sigmask`.
    pub fn block() -> nix::Result<Self> {
        let mut set = SigSet::empty();
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            set.add(signal);
        }
        set.thread_block()?;
        Ok(Self { set })
    }

    /// Waits for the first of them on a thread of its own, runs `shutdown`
    /// with its name, then ends the process by that signal: a parent, and
    /// launchd, see the signal, never a successful exit.
    pub fn spawn(
        self,
        shutdown: impl FnOnce(&'static str) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            if let Ok(signal) = self.set.wait() {
                shutdown(signal.as_str());
                die_by(signal);
            }
        })
    }
}

/// Ends the process by `signal`, as if no watch had taken it: unblocked in
/// this thread and raised again, it takes its default action (the watch sets
/// no handler). Should the raise return (a handler set elsewhere), the process
/// exits with 128 plus the signal's number, as a shell reports one a signal
/// ended. Neither is a successful exit.
fn die_by(signal: Signal) -> ! {
    if SigSet::from(signal).thread_unblock().is_ok() {
        let _raised: nix::Result<()> = nix::sys::signal::raise(signal);
    }
    #[expect(
        clippy::exit,
        reason = "a termination signal ends the process once the shutdown ran"
    )]
    std::process::exit(128_i32.saturating_add(signal as i32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_lock(name: &str) -> Result<PathBuf, std::io::Error> {
        let dir =
            std::env::temp_dir().join(format!("tulpa-instance-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("tulpa.lock"))
    }

    #[test]
    fn the_first_holds_it_and_writes_its_pid() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_lock("first")?;
        let held = SingleInstance::acquire(&path, Duration::ZERO, |_| {})?;
        assert_eq!(
            std::fs::read_to_string(held.path())?.trim(),
            std::process::id().to_string()
        );
        drop(held);
        let mut stopped = false;
        let again = SingleInstance::acquire(&path, Duration::ZERO, |_| stopped = true);
        assert!(
            again.is_ok(),
            "a released lock (a stale file) is taken at once"
        );
        assert!(!stopped, "free: nobody to stop");
        Ok(())
    }

    #[test]
    fn a_second_takes_over_once_the_holder_lets_go() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_lock("takeover")?;
        let old = SingleInstance::acquire(&path, Duration::ZERO, |_| {})?;
        let (tx, rx) = mpsc::channel();
        // The old instance's shutdown, as its signal watch would run it: it lets go.
        let releaser = thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                thread::sleep(Duration::from_millis(200));
                drop(old);
            }
        });
        let mut told = None;
        let new = SingleInstance::acquire(&path, Duration::from_secs(5), |pid| {
            told = Some(pid);
            let _sent: Result<(), mpsc::SendError<()>> = tx.send(());
        })?;
        let _joined = releaser.join();
        assert_eq!(
            told,
            i32::try_from(std::process::id()).ok(),
            "the holder's pid from the file"
        );
        assert_eq!(
            std::fs::read_to_string(new.path())?.trim(),
            std::process::id().to_string()
        );
        Ok(())
    }

    #[test]
    fn a_holder_that_keeps_it_is_said_by_pid() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_lock("kept")?;
        let _old = SingleInstance::acquire(&path, Duration::ZERO, |_| {})?;
        let refused = SingleInstance::acquire(&path, Duration::from_millis(300), |_| {});
        assert!(matches!(
            refused,
            Err(InstanceError::StillHeld { pid: Some(_) })
        ));
        Ok(())
    }
}
