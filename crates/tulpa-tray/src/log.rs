//! Append-only local activity log for human controls.
//!
//! One event per line, UTC timestamp first, so a human can read the file directly
//! and a future reader can replay it. The log never truncates or rewrites:
//! [`ActivityLog::open`] always appends.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::TrayEvent;

/// Failure to open or append to the activity log.
#[derive(Debug)]
#[non_exhaustive]
pub enum LogError {
    /// The underlying filesystem error.
    Io(io::Error),
}

impl fmt::Display for LogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "activity log I/O failed: {error}"),
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for LogError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An append-only handle to the tray activity log.
#[derive(Debug)]
pub struct ActivityLog {
    file: File,
    path: PathBuf,
}

impl ActivityLog {
    /// Opens (creating if needed) the log at `path`, always in append mode.
    ///
    /// Parent directories are created. An existing file is never truncated.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created or the
    /// file cannot be opened for appending.
    pub fn open(path: &Path) -> Result<Self, LogError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// The file this log appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one timestamped event line.
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp cannot be produced (a system clock
    /// before the Unix epoch) or the write fails.
    pub fn record(&mut self, event: &TrayEvent) -> Result<(), LogError> {
        let line = match event {
            TrayEvent::PauseToggled { paused } => {
                format!("pause-all {}", if *paused { "on" } else { "off" })
            }
            TrayEvent::Quit => "quit".to_owned(),
        };
        let timestamp = format_utc(SystemTime::now()).ok_or_else(|| {
            LogError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "system clock predates the Unix epoch",
            ))
        })?;
        writeln!(self.file, "{timestamp} {line}")?;
        self.file.flush()?;
        Ok(())
    }

    /// Appends one timestamped free-form note line for presenter diagnostics.
    /// Notes carry no control semantics; human controls go through [`record`].
    ///
    /// [`record`]: Self::record
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp cannot be produced (a system clock
    /// before the Unix epoch) or the write fails.
    pub fn note(&mut self, message: &str) -> Result<(), LogError> {
        let timestamp = format_utc(SystemTime::now()).ok_or_else(|| {
            LogError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "system clock predates the Unix epoch",
            ))
        })?;
        writeln!(self.file, "{timestamp} note {message}")?;
        self.file.flush()?;
        Ok(())
    }
}

/// Formats a system time as `YYYY-MM-DDTHH:MM:SSZ` in real UTC.
///
/// Implemented by hand (epoch day arithmetic, no time crate) so the dependency
/// stays minimal; leap seconds are smudged like POSIX `date -u`.
pub fn format_utc(time: SystemTime) -> Option<String> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day.div_euclid(3_600);
    let minute = seconds_of_day.rem_euclid(3_600).div_euclid(60);
    let second = seconds_of_day.rem_euclid(60);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Converts epoch days to a civil (year, month, day), proleptic Gregorian.
const fn civil_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    // Howard Hinnant's algorithm; day 0 is 1970-01-01. Inputs are always
    // non-negative epoch days, so Euclidean division matches Hinnant's
    // truncation exactly here.
    let z = days_since_epoch.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = day_of_era
        .saturating_sub(day_of_era.div_euclid(1_460))
        .saturating_add(day_of_era.div_euclid(36_524))
        .saturating_sub(day_of_era.div_euclid(146_096))
        .div_euclid(365);
    let mut year = year_of_era.saturating_add(era.saturating_mul(400));
    let day_of_year = day_of_era.saturating_sub(
        year_of_era
            .saturating_mul(365)
            .saturating_add(year_of_era.div_euclid(4))
            .saturating_sub(year_of_era.div_euclid(100)),
    );
    let month_prime = day_of_year
        .saturating_mul(5)
        .saturating_add(2)
        .div_euclid(153);
    let day = day_of_year
        .saturating_sub(
            month_prime
                .saturating_mul(153)
                .saturating_add(2)
                .div_euclid(5),
        )
        .saturating_add(1);
    let month = if month_prime < 10 {
        month_prime.saturating_add(3)
    } else {
        month_prime.saturating_sub(9)
    };
    if month <= 2 {
        year = year.saturating_add(1);
    }
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_log(name: &str) -> PathBuf {
        let unique = format!(
            "tulpa-tray-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn format_utc_hits_known_anchors() {
        assert_eq!(
            format_utc(UNIX_EPOCH),
            Some("1970-01-01T00:00:00Z".to_owned())
        );
        let day_one = UNIX_EPOCH + std::time::Duration::from_secs(86_400);
        assert_eq!(format_utc(day_one), Some("1970-01-02T00:00:00Z".to_owned()));
        let leap_day = UNIX_EPOCH + std::time::Duration::from_secs(86_400 * (365 * 4 + 1));
        assert_eq!(
            format_utc(leap_day),
            Some("1974-01-01T00:00:00Z".to_owned())
        );
        let afternoon =
            UNIX_EPOCH + std::time::Duration::from_secs(86_400 + 15 * 3_600 + 9 * 60 + 7);
        assert_eq!(
            format_utc(afternoon),
            Some("1970-01-02T15:09:07Z".to_owned())
        );
    }

    #[test]
    fn clock_before_epoch_is_rejected() {
        let before = UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert!(format_utc(before).is_none());
    }

    #[test]
    fn log_appends_without_truncating() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_log("append");
        {
            let mut log = ActivityLog::open(&path)?;
            assert_eq!(log.path(), path.as_path());
            log.record(&TrayEvent::PauseToggled { paused: true })?;
            log.record(&TrayEvent::Quit)?;
        }
        {
            let mut log = ActivityLog::open(&path)?;
            log.record(&TrayEvent::PauseToggled { paused: false })?;
        }
        let content = fs::read_to_string(&path)?;
        let mut lines = content.lines();
        assert!(
            lines
                .next()
                .is_some_and(|line| line.ends_with(" pause-all on"))
        );
        assert!(lines.next().is_some_and(|line| line.ends_with(" quit")));
        assert!(
            lines
                .next()
                .is_some_and(|line| line.ends_with(" pause-all off"))
        );
        assert!(lines.next().is_none(), "exactly three lines");
        assert!(
            content.lines().all(|line| line.len() > 20),
            "each line carries a timestamp"
        );
        fs::remove_file(&path)?;
        Ok(())
    }

    #[test]
    fn notes_and_events_share_one_append_only_file() -> Result<(), Box<dyn std::error::Error>> {
        let path = temp_log("notes");
        {
            let mut log = ActivityLog::open(&path)?;
            log.record(&TrayEvent::PauseToggled { paused: true })?;
            log.note("status item failed: synthetic")?;
            log.record(&TrayEvent::Quit)?;
        }
        let content = fs::read_to_string(&path)?;
        let mut lines = content.lines();
        assert!(
            lines
                .next()
                .is_some_and(|line| line.ends_with(" pause-all on"))
        );
        assert!(
            lines
                .next()
                .is_some_and(|line| line.ends_with(" note status item failed: synthetic"))
        );
        assert!(lines.next().is_some_and(|line| line.ends_with(" quit")));
        assert!(lines.next().is_none(), "exactly three lines");
        fs::remove_file(&path)?;
        Ok(())
    }

    #[test]
    fn open_creates_missing_parent_directories() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_log("dirs");
        let path = dir.join("nested").join("tulpa.log");
        let mut log = ActivityLog::open(&path)?;
        log.record(&TrayEvent::Quit)?;
        assert!(path.is_file());
        fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
