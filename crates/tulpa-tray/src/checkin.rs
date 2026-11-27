//! The check-in marker: proof, for an installer waiting on this Mac, that a freshly started Tulpa got its
//! link's hello accepted.
//!
//! `packaging/macos/update.sh` swaps the app bundle with a dead-man armed: a launchd job restores the previous
//! bundle unless a check-in newer than the swap, written by a process started from the new bundle's
//! executable, appears in time. The marker is rewritten every time a hello is accepted, atomically (a
//! temporary file in the same directory, then a rename), so a reader never sees half a marker. It is a few
//! `key=value` lines a POSIX shell reads with `sed`:
//!
//! ```text
//! version=0.1.0
//! commit=1a2b3c4d5e6f
//! exe_inode=1234567
//! pid=4242
//! utc=2026-11-15T15:30:00Z
//! epoch=1794756600
//! ```
//!
//! `exe_inode` is the inode of the executable this process was started from, taken at startup: an installer
//! knows the inode of the binary it put in place, so a check-in from the process it replaced never counts.
//! The marker carries no secrets and no payloads.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::log::format_utc;

/// Where the marker lives; the installer reads the same path.
pub const CHECKIN_PATH: &str = "~/Library/Application Support/tulpa/checkin";

/// The commit this build was made from, as `scripts/bundle-macos.sh` passes it in `TULPA_COMMIT`.
const BUILD_COMMIT: Option<&str> = option_env!("TULPA_COMMIT");

/// One process's check-in marker: what it says about the build, and where it is written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckIn {
    path: PathBuf,
    version: String,
    commit: String,
    exe_inode: Option<u64>,
    pid: u32,
}

impl CheckIn {
    /// This process's marker at [`CHECKIN_PATH`]: the crate version, the build's commit (`unknown` when the
    /// build was not given one), the inode of the executable this process was started from, and its pid.
    ///
    /// Call it at startup, before a later install can replace the executable under the same path.
    #[must_use]
    pub fn for_this_process() -> Self {
        let exe_inode = std::env::current_exe()
            .and_then(fs::metadata)
            .map(|metadata| metadata.ino())
            .ok();
        Self::new(
            crate::config::expand_tilde(CHECKIN_PATH),
            env!("CARGO_PKG_VERSION"),
            BUILD_COMMIT.unwrap_or(""),
            exe_inode,
            std::process::id(),
        )
    }

    /// A marker at `path` with explicit contents. A version or commit with characters outside
    /// `A-Za-z0-9._+-` (or empty) is written as `unknown`, so every value stays one shell-readable word.
    #[must_use]
    pub fn new(
        path: PathBuf,
        version: &str,
        commit: &str,
        exe_inode: Option<u64>,
        pid: u32,
    ) -> Self {
        Self {
            path,
            version: word_or_unknown(version),
            commit: word_or_unknown(commit),
            exe_inode,
            pid,
        }
    }

    /// The file the marker is written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The marker's text for a check-in at `now`; `None` when `now` is before the Unix epoch.
    #[must_use]
    pub fn contents(&self, now: SystemTime) -> Option<String> {
        let epoch = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
        let utc = format_utc(now)?;
        let inode = self
            .exe_inode
            .map_or_else(|| "unknown".to_owned(), |inode| inode.to_string());
        Some(format!(
            "version={}\ncommit={}\nexe_inode={inode}\npid={}\nutc={utc}\nepoch={epoch}\n",
            self.version, self.commit, self.pid
        ))
    }

    /// Writes the marker for a check-in at `now`, replacing the previous one atomically: the text goes to a
    /// temporary file beside the marker, which is then renamed over it. The directory is created if needed.
    ///
    /// # Errors
    ///
    /// Returns an error when `now` is before the Unix epoch, the marker has no parent directory, or the
    /// directory, the temporary file, or the rename fails; a failed write leaves no temporary file behind.
    pub fn write(&self, now: SystemTime) -> io::Result<()> {
        let text = self.contents(now).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "check-in time before the Unix epoch",
            )
        })?;
        let (Some(dir), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "check-in path has no directory or file name",
            ));
        };
        fs::create_dir_all(dir)?;
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}.tmp", self.pid));
        let temp = dir.join(temp_name);
        let written = fs::File::create(&temp)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
            })
            .and_then(|()| fs::rename(&temp, &self.path));
        if written.is_err() {
            let _removed: io::Result<()> = fs::remove_file(&temp);
        }
        written
    }
}

fn word_or_unknown(value: &str) -> String {
    let word = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte));
    if word {
        value.to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = format!(
            "tulpa-checkin-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        std::env::temp_dir().join(unique)
    }

    const AT: Duration = Duration::from_secs(1_763_220_600);

    #[test]
    fn contents_are_shell_readable_lines() {
        let checkin = CheckIn::new(
            PathBuf::from("/nowhere/checkin"),
            "0.1.0",
            "1a2b3c4d5e6f-dirty",
            Some(1_234_567),
            4242,
        );
        assert_eq!(
            checkin.contents(UNIX_EPOCH + AT).as_deref(),
            Some(
                "version=0.1.0\ncommit=1a2b3c4d5e6f-dirty\nexe_inode=1234567\npid=4242\n\
                 utc=2025-11-15T15:30:00Z\nepoch=1763220600\n"
            )
        );
    }

    #[test]
    fn odd_values_are_written_as_unknown() {
        let checkin = CheckIn::new(PathBuf::from("/x/checkin"), "", "a b\nepoch=9", None, 1);
        let text = checkin.contents(UNIX_EPOCH + AT).unwrap_or_default();
        assert!(text.starts_with("version=unknown\ncommit=unknown\nexe_inode=unknown\npid=1\n"));
        assert_eq!(
            text.matches("epoch=").count(),
            1,
            "no value can forge a line"
        );
    }

    #[test]
    fn write_creates_the_directory_and_replaces_the_marker_atomically() -> io::Result<()> {
        let dir = temp_dir("write");
        let path = dir.join("support").join("checkin");
        let checkin = CheckIn::new(path.clone(), "0.1.0", "abc", Some(7), 99);
        checkin.write(UNIX_EPOCH + AT)?;
        assert!(fs::read_to_string(&path)?.ends_with("epoch=1763220600\n"));
        checkin.write(UNIX_EPOCH + AT + Duration::from_secs(5))?;
        assert!(fs::read_to_string(&path)?.ends_with("epoch=1763220605\n"));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap_or(&dir))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<_>>()?;
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("checkin")],
            "no temporary left"
        );
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn a_failed_write_leaves_no_temporary() -> io::Result<()> {
        let dir = temp_dir("blocked");
        fs::create_dir_all(dir.join("checkin"))?;
        fs::write(
            dir.join("checkin").join("keep"),
            "a directory is in the way",
        )?;
        let checkin = CheckIn::new(dir.join("checkin"), "0.1.0", "abc", None, 5);
        assert!(checkin.write(UNIX_EPOCH + AT).is_err());
        let names: Vec<_> = fs::read_dir(&dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<_>>()?;
        assert_eq!(names, vec![std::ffi::OsString::from("checkin")]);
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn this_process_knows_its_executable() {
        let checkin = CheckIn::for_this_process();
        let text = checkin.contents(UNIX_EPOCH + AT).unwrap_or_default();
        assert!(!text.contains("exe_inode=unknown"), "{text}");
        assert!(text.starts_with(&format!("version={}\n", env!("CARGO_PKG_VERSION"))));
        assert!(
            checkin
                .path()
                .ends_with("Library/Application Support/tulpa/checkin")
        );
    }
}
