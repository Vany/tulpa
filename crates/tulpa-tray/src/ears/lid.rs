//! Whether the Mac's lid is closed: a closed lid stops the capture, and the self-check says so.
//!
//! macOS keeps the lid's state in the I/O Registry as `AppleClamshellState` (`Yes`: closed). [`IoregLid`]
//! reads it with the registry's own tool, `/usr/sbin/ioreg`, run directly with the cleared environment and
//! a time limit. No `IOKit` binding: this workspace denies `unsafe` (bar `CoreAudio`'s listener), and the tool
//! ships with every Mac, so no new crate is needed either. A Mac without a lid has no such key, and its lid
//! reads as unknown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tulpa_hands::exec::{self, Ended};
use tulpa_stop::Stop;

/// The registry tool; run directly, never through a shell.
pub const IOREG: &str = "/usr/sbin/ioreg";
/// The registry key holding the lid's state.
const KEY: &str = "AppleClamshellState";
/// An `ioreg` run is killed past this.
const READ_LIMIT: Duration = Duration::from_secs(5);
/// The most of its output kept; the key comes early.
const READ_MAX: usize = 1 << 20;

/// Tells whether the Mac's lid is closed.
pub trait Lid: Send + Sync {
    /// `Some(true)`: closed; `Some(false)`: open; `None`: no lid (a desktop Mac), or it could not be read.
    fn closed(&self) -> Option<bool>;
}

/// The lid as `ioreg -r -k AppleClamshellState -d 4` shows it.
#[derive(Debug, Clone)]
pub struct IoregLid {
    program: PathBuf,
    home: PathBuf,
    /// The tray's Pause: it kills a read in flight (and Quit sets it), so a shutdown's wait for children is short.
    paused: Arc<Stop>,
    /// A read is killed past this: [`READ_LIMIT`] (a test sets it past its guard).
    read_limit: Duration,
}

impl IoregLid {
    /// Reads with [`IOREG`]; `home` is the cleared environment's `HOME`. A read in flight is killed when `paused`
    /// is set (the lid then reads as unknown), and none starts while it is.
    #[must_use]
    pub fn new(home: PathBuf, paused: Arc<Stop>) -> Self {
        Self::with_program(PathBuf::from(IOREG), home, paused)
    }

    /// Reads with `program` instead of [`IOREG`] (a stand-in in tests).
    #[must_use]
    pub const fn with_program(program: PathBuf, home: PathBuf, paused: Arc<Stop>) -> Self {
        Self {
            program,
            home,
            paused,
            read_limit: READ_LIMIT,
        }
    }
}

impl Lid for IoregLid {
    /// Unknown when `ioreg` cannot run, fails, or shows no lid.
    fn closed(&self) -> Option<bool> {
        let mut command = exec::cleared(&self.program, &self.home);
        command.args(["-r", "-k", KEY, "-d", "4"]);
        let outcome = exec::run(
            command,
            &self.paused,
            &Stop::new(),
            self.read_limit,
            READ_MAX,
        )
        .ok()?;
        if outcome.ended != Ended::Exit(Some(0)) {
            return None;
        }
        clamshell_closed(&String::from_utf8_lossy(&outcome.stdout))
    }
}

/// The lid in `ioreg` output: the first `"AppleClamshellState" = Yes` (closed) or `= No` (open) line;
/// `None` without one (no lid) or with any other value.
#[must_use]
pub fn clamshell_closed(ioreg: &str) -> Option<bool> {
    let quoted = format!("\"{KEY}\"");
    ioreg.lines().find_map(|line| {
        let (_, after) = line.split_once(quoted.as_str())?;
        match after.trim_start().strip_prefix('=')?.trim() {
            "Yes" => Some(true),
            "No" => Some(false),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    /// The shape `ioreg -r -k AppleClamshellState -d 4` prints on a laptop, values made up.
    const CLOSED: &str = r#"+-o IOPMrootDomain  <class IOPMrootDomain, id 0x100000101, registered, matched, active, busy 0 (0 ms), retain 300>
  | {
  |   "IOPowerManagement" = {"CurrentPowerState"=2,"MaxPowerState"=2}
  |   "AppleClamshellCausesSleep" = No
  |   "SleepWakeUUID" = "00000000-0000-0000-0000-000000000000"
  |   "AppleClamshellState" = Yes
  |   "DesktopMode" = Yes
  | }
  |
  +-o IORootParent  <class IORootParent, id 0x100000102, registered, matched, active, busy 0 (0 ms), retain 6>
"#;

    #[test]
    fn the_lid_is_read_from_its_registry_line() {
        assert_eq!(clamshell_closed(CLOSED), Some(true));
        assert_eq!(
            clamshell_closed(&CLOSED.replace(
                "\"AppleClamshellState\" = Yes",
                "\"AppleClamshellState\" = No"
            )),
            Some(false)
        );
        assert_eq!(
            clamshell_closed("+-o IOPMrootDomain  <class IOPMrootDomain>\n  | {\n  | }\n"),
            None,
            "a desktop Mac has no lid"
        );
        assert_eq!(clamshell_closed(""), None);
        assert_eq!(
            clamshell_closed("  |   \"AppleClamshellState\" = Maybe\n"),
            None,
            "an unknown value is no answer"
        );
        assert_eq!(
            clamshell_closed("  |   \"AppleClamshellCausesSleep\" = Yes\n"),
            None,
            "another key is not the lid"
        );
    }

    /// The wedge guard of a wait in these tests.
    const GUARD: Duration = Duration::from_secs(60);

    #[test]
    fn ioreg_runs_with_the_key_and_its_answer_or_failure_is_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-lid-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir)?;
        let shown = dir.join("shown.txt");
        std::fs::write(&shown, CLOSED)?;
        let program = dir.join("ioreg");
        crate::testutil::write_script(
            &program,
            &format!(
                "#!/bin/sh\n[ \"$*\" = '-r -k AppleClamshellState -d 4' ] || exit 3\ncat {}\n",
                shown.display()
            ),
        )?;
        let lid = IoregLid::with_program(program.clone(), dir.clone(), Arc::new(Stop::new()));
        assert_eq!(lid.closed(), Some(true));
        std::fs::write(
            &shown,
            CLOSED.replace(
                "= Yes\n  |   \"DesktopMode\"",
                "= No\n  |   \"DesktopMode\"",
            ),
        )?;
        assert_eq!(lid.closed(), Some(false));
        crate::testutil::write_script(
            &program,
            "#!/bin/sh\necho '\"AppleClamshellState\" = Yes'\nexit 1\n",
        )?;
        assert_eq!(lid.closed(), None, "a failed run is no answer");
        // Pause (Quit sets it) kills a read in flight: unknown. The stand-in says it started (its pid in a marker),
        // then sleeps past the guard, and the read's own limit is past it too: only the Pause can end the read
        // before the 60 s guard. On a plain thread, so a read the Pause does not end fails the test there.
        let started = dir.join("started");
        crate::testutil::write_script(
            &program,
            &format!(
                "#!/bin/sh\necho $$ > {}.tmp && mv {0}.tmp {0}\nexec sleep 3600\n",
                started.display()
            ),
        )?;
        let paused = Arc::new(Stop::new());
        let mut slow = IoregLid::with_program(program, dir.clone(), Arc::clone(&paused));
        slow.read_limit = Duration::from_secs(3600);
        let (read, reading) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _told = read.send(slow.closed());
        });
        // A file has no event here (no kqueue binding in this crate): the marker is looked for every 10 ms, under
        // the guard.
        let guard = std::time::Instant::now();
        while !started.exists() && guard.elapsed() < GUARD {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.exists(), "the stand-in never started");
        paused.set();
        assert_eq!(reading.recv_timeout(GUARD)?, None);
        assert_eq!(
            IoregLid::with_program(dir.join("missing"), dir.clone(), Arc::new(Stop::new()))
                .closed(),
            None,
            "no ioreg (not a Mac): unknown"
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
