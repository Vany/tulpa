//! A termination signal ends Tulpa by that same signal once its shutdown ran,
//! never with exit status 0, as Quit does: under the installer's
//! `KeepAlive = { SuccessfulExit = false }` launchd starts Tulpa again after
//! any end but a successful exit. A signal the process inherited as ignored
//! cannot end it; then it exits 128 plus the signal's number, as a shell
//! reports that signal's end.
//!
//! The test runs itself as the child it signals. It has no test harness: the
//! harness's main thread does not block the signals, so it would take one and
//! die by it before the watch saw it. In the app every thread blocks them.

use std::error::Error;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};
use tulpa_tray::lifecycle::TerminationWatch;

/// Set for the child: it runs the watch instead of the checks.
const CHILD: &str = "TULPA_TERMINATION_TEST_CHILD";

/// Set for the probe child, to a signal's number: it raises that signal at itself.
const PROBE: &str = "TULPA_TERMINATION_TEST_PROBE";

fn main() -> ExitCode {
    if let Some(number) = std::env::var_os(PROBE) {
        return probe(&number);
    }
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let mut stderr = io::stderr().lock();
    match parent() {
        Ok(()) => {
            let _said: io::Result<()> = writeln!(
                stderr,
                "termination: SIGTERM, SIGINT and SIGHUP each end the process by that signal, after the shutdown"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _said: io::Result<()> = writeln!(stderr, "termination: FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The app's start, cut short: the signals blocked before any thread starts,
/// the watch on its own thread with a shutdown that says it ran, then a wait.
fn child() -> ExitCode {
    let Ok(watch) = TerminationWatch::block() else {
        return ExitCode::from(2);
    };
    let _watcher = watch.spawn(|signal| {
        let mut out = io::stdout().lock();
        let _said: io::Result<()> = writeln!(out, "shutdown {signal}").and_then(|()| out.flush());
    });
    let mut out = io::stdout().lock();
    if writeln!(out, "ready").and_then(|()| out.flush()).is_err() {
        return ExitCode::from(2);
    }
    drop(out);
    // The watch ends the process; a child that no signal reaches gives up.
    thread::sleep(Duration::from_secs(30));
    ExitCode::from(3)
}

/// Raises the signal at itself, nothing blocked: dies by it when its action is the default, exits 0 when it
/// came in ignored.
fn probe(number: &std::ffi::OsStr) -> ExitCode {
    let signal = number
        .to_str()
        .and_then(|n| n.parse::<i32>().ok())
        .and_then(|n| nix::sys::signal::Signal::try_from(n).ok());
    match signal.map(nix::sys::signal::raise) {
        Some(Ok(())) => ExitCode::SUCCESS,
        _ => ExitCode::from(2),
    }
}

/// How a process this one starts ends when it raises `signal` at itself: by it, or (the signal inherited as
/// ignored) not at all. A non-interactive shell starts its `&` jobs with SIGINT ignored and `nohup` ignores
/// SIGHUP; the child inherits that, and no raise can end it by such a signal.
fn ignored_here(exe: &std::path::Path, signal: Signal) -> Result<bool, Box<dyn Error>> {
    let status = Command::new(exe)
        .env(PROBE, signal.as_raw().to_string())
        .stdin(Stdio::null())
        .status()?;
    match (status.signal(), status.code()) {
        (Some(by), _) if by == signal.as_raw() => Ok(false),
        (_, Some(0)) => Ok(true),
        _ => Err(format!(
            "the probe for signal {} ended with {status}",
            signal.as_raw()
        )
        .into()),
    }
}

fn parent() -> Result<(), Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    for (signal, name) in [
        (Signal::TERM, "SIGTERM"),
        (Signal::INT, "SIGINT"),
        (Signal::HUP, "SIGHUP"),
    ] {
        let mut child = Command::new(&exe)
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("the child has no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        if lines.next().transpose()?.as_deref() != Some("ready") {
            let _killed: io::Result<()> = child.kill();
            let status = child.wait()?;
            return Err(format!("the child never got ready ({status})").into());
        }
        // Ignored where this runs, the signal cannot end the child; the watch then exits as a shell reports
        // that signal's end (128 + its number), never 0.
        let ignored = ignored_here(&exe, signal)?;
        let pid = Pid::from_raw(i32::try_from(child.id())?).ok_or("the child has pid 0")?;
        kill_process(pid, signal)?;
        let said = lines.collect::<Result<Vec<String>, io::Error>>()?;
        let status = child.wait()?;
        if said != [format!("shutdown {name}")] {
            return Err(format!(
                "{name}: the shutdown did not run once before the end; the child said \"{}\"",
                said.join(" | ")
            )
            .into());
        }
        let by_it = if ignored {
            status.code() == Some(128_i32.saturating_add(signal.as_raw()))
        } else {
            status.signal() == Some(signal.as_raw())
        };
        if !by_it {
            let how = if ignored {
                "as 128 + its number (it came in ignored)"
            } else {
                "by it"
            };
            return Err(format!("{name}: the child ended with {status}, not {how}").into());
        }
    }
    Ok(())
}
