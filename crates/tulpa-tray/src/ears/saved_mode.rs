//! The ears mode kept across restarts: one line, the mode's id, in a small
//! state file beside the config ([`FILE_NAME`]), replaced atomically.
//!
//! Only a chosen mode is kept: a switch (menu or `ears.mode`), `ears.stop`,
//! Pause. Quit and a capture that ends on its own leave the file as it is, so
//! a restart or an app update comes back in the mode last chosen.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::EarsMode;

/// The state file's name, beside the config file.
pub(super) const FILE_NAME: &str = "ears-mode";

/// The saved mode; `None` when the file is missing, unreadable, or holds
/// anything but one known mode id.
pub(super) fn load(path: &Path) -> Option<EarsMode> {
    let text = fs::read_to_string(path).ok()?;
    EarsMode::from_id(text.trim())
}

/// Saves `mode` as one line: written to a temporary file beside `path`,
/// flushed, then renamed over it, so a reader finds the old line or the new
/// one, never a torn one.
///
/// # Errors
///
/// Returns the I/O error when the temporary file can't be written or renamed;
/// `path` then stays as it was and the temporary file is removed.
pub(super) fn save(path: &Path, mode: EarsMode) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let written = write_then_rename(&temporary, path, mode.id());
    if written.is_err() {
        let _removed: io::Result<()> = fs::remove_file(&temporary);
    }
    written
}

fn write_then_rename(temporary: &Path, path: &Path, id: &str) -> io::Result<()> {
    let mut file = File::create(temporary)?;
    file.write_all(format!("{id}\n").as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn temp_dir(name: &str) -> io::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-ears-mode-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn a_saved_mode_is_one_line_and_reads_back() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("saved")?;
        let path = dir.join(FILE_NAME);
        save(&path, EarsMode::Alone)?;
        assert_eq!(fs::read_to_string(&path)?, "alone\n");
        assert_eq!(load(&path), Some(EarsMode::Alone));
        save(&path, EarsMode::Off)?;
        assert_eq!(
            fs::read_to_string(&path)?,
            "off\n",
            "replaced, not appended"
        );
        assert_eq!(load(&path), Some(EarsMode::Off));
        let left: Vec<_> = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsString::from(FILE_NAME)],
            "no temporary file left"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_missing_or_unknown_file_gives_no_mode() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("unknown")?;
        let path = dir.join(FILE_NAME);
        assert_eq!(load(&path), None, "missing");
        for unknown in ["", "\n", "loud\n", "alone\noffice\n", "ALONE\n"] {
            fs::write(&path, unknown)?;
            assert_eq!(load(&path), None, "{unknown:?}");
        }
        fs::write(&path, "  meeting\r\n")?;
        assert_eq!(
            load(&path),
            Some(EarsMode::Meeting),
            "surrounding whitespace is ignored"
        );
        fs::write(&path, [0xff, 0xfe, b'\n'])?;
        assert_eq!(load(&path), None, "not UTF-8");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_failed_save_leaves_nothing_behind() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("failed")?;
        let path = dir.join("missing-dir").join(FILE_NAME);
        assert!(save(&path, EarsMode::Call).is_err());
        assert!(!dir.join("missing-dir").exists());
        // A directory where the file belongs: the rename fails, the temporary file goes.
        let blocked = dir.join(FILE_NAME);
        fs::create_dir(&blocked)?;
        assert!(save(&blocked, EarsMode::Call).is_err());
        assert!(!dir.join(format!("{FILE_NAME}.tmp")).exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
