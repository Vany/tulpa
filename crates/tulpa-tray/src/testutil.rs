//! Fixtures the tests of several modules share.

use std::io::{self, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `text` to `path` as an executable script, from a shell of its own.
///
/// No descriptor that can write the file ever lives in the test process. Written here, another test's thread
/// forking meanwhile would carry a copy of that descriptor into its child until the child's exec, and the
/// script's exec in that window fails with ETXTBSY ("text file busy"). Also for a script rewritten between runs.
///
/// # Errors
///
/// The shell's start or the write to it failing, or the shell not writing the file.
pub fn write_script(path: &Path, text: &str) -> io::Result<()> {
    let mut sh = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()?;
    let written = sh
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("the shell has no stdin"))
        .and_then(|mut stdin| stdin.write_all(text.as_bytes()));
    let status = sh.wait()?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "writing {}: the shell ended with {status}",
            path.display()
        )))
    }
}

/// A test's child, killed with its process group and reaped when the test ends, however it ends: a failed assert
/// or an early `?` leaves no `sleep 60` behind.
#[derive(Debug)]
pub struct Killed(std::process::Child);

impl Killed {
    /// Spawns `command` as the leader of a process group of its own, so the kill also takes what it starts.
    ///
    /// # Errors
    ///
    /// The spawn's.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        std::os::unix::process::CommandExt::process_group(command, 0)
            .spawn()
            .map(Self)
    }
}

impl std::ops::Deref for Killed {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Killed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Killed {
    fn drop(&mut self) {
        // Only an unreaped leader's pid is surely still this group's: a reaped one's may be anyone's by now.
        if matches!(self.0.try_wait(), Ok(None)) {
            let group = rustix::process::Pid::from_child(&self.0);
            let _killed: rustix::io::Result<()> =
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _reaped: io::Result<std::process::ExitStatus> = self.0.wait();
    }
}
