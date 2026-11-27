//! The ears' own logs on disk: appended `0600`, and rotated once past a bound.
//!
//! `heard.log` ([`assemble`](crate::assemble)), the shadow's `lines.log` ([`shadow`](crate::shadow)) and the mic's
//! `mic-switch.log` are each one file that grows by a line at a time; past [`BOUND`] it is renamed to `<name>.1`
//! (one kept) and started again.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The mic's switch log's file name inside the state directory (the shadow's dir, in the shadow).
pub const SWITCH_LOG: &str = "mic-switch.log";
/// A log past this many bytes (20 MB) is renamed to `<name>.1`.
pub const BOUND: u64 = 20_000_000;

/// Renames `path` to `<path>.1` (replacing an older one) when it is larger than `bound` bytes; `true` when it did.
/// A missing file is not rotated.
///
/// # Errors
///
/// The `rename`'s error, or the `metadata`'s for anything but a missing file.
pub fn rotate_past(path: &Path, bound: u64) -> io::Result<bool> {
    let len = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= bound {
        return Ok(false);
    }
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    match fs::rename(path, old) {
        Ok(()) => Ok(true),
        // Gone between the look and the rename: another writer of the same log rotated it first. Done already.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `path` for appending, created `0600`.
pub(crate) fn append(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

/// Appends `<epoch ms> <from> -> <to>` to `<dir>/mic-switch.log` (created `0600`), creating `dir`.
///
/// `from` is `none` after an outage. With `rotate`, the log is rotated to `.1` past [`BOUND`], as the other logs;
/// without (a capture that has been told to stop only appends to what it shares with the next one), it is left.
///
/// # Errors
///
/// The directory's or the append's; or the rotation's, said as one (the line itself is written then).
pub fn log_switch(dir: &Path, from: Option<&str>, to: &str, rotate: bool) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let ms = crate::now_ms();
    let path = dir.join(SWITCH_LOG);
    writeln!(append(&path)?, "{ms} {} -> {to}", from.unwrap_or("none"))?;
    if !rotate {
        return Ok(());
    }
    rotate_past(&path, BOUND)
        .map(drop)
        .map_err(|e| io::Error::new(e.kind(), format!("not rotated: {e}")))
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn switch_log_appends_one_line_per_switch() {
        let dir = std::env::temp_dir().join(format!("tulpa-hear-test-{}", std::process::id()));
        let _gone = std::fs::remove_dir_all(&dir);
        log_switch(&dir, Some("Built-in"), "USB", true).unwrap();
        log_switch(&dir, None, "Built-in", true).unwrap();
        let text = std::fs::read_to_string(dir.join(SWITCH_LOG)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let (ms, rest) = lines.first().unwrap().split_once(' ').unwrap();
        assert!(ms.parse::<u128>().unwrap() > 0);
        assert_eq!(rest, "Built-in -> USB");
        assert!(lines.get(1).unwrap().ends_with(" none -> Built-in"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Fable N7: the switch log stays `0600` across a rotation. The shadow makes it `0600`; past the bound it is
    /// renamed away, and the file the next switch starts must be the owner's alone too (it names devices).
    #[test]
    fn a_rotated_switch_log_is_started_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("tulpa-hear-log-mode-{}", std::process::id()));
        let _gone = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join(SWITCH_LOG);
        // Past the bound already, without writing 20 MB: a sparse file.
        append(&log)
            .and_then(|f| f.set_len(BOUND.saturating_add(1)))
            .unwrap();
        log_switch(&dir, Some("Built-in"), "USB", true).unwrap();
        assert!(dir.join(format!("{SWITCH_LOG}.1")).exists(), "rotated");
        log_switch(&dir, Some("USB"), "Built-in", true).unwrap();
        let mode = fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
