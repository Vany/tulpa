//! `tulpa-hear --watchdog`: stops the shadow rehearsal when the live path's emit latency rises while it runs.
//!
//! It reads the live path's log with `--compare`'s own reader ([`live_latencies`]: a line's emit time is its
//! `HH:MM:SS` + 500 ms), only what was appended since the last read ([`LiveTail`]), and judges, every [`TICK`], the
//! phrases of the last window against a baseline taken from before the watchdog started ([`Watch::judge`]). On a
//! rise past the bound with at least [`MIN_PHRASES`] phrases in the window it sends SIGTERM to the shadow's pid
//! ([`terminate`]) — only once that pid's command line has been read and is a shadow's, `tulpa-hear --shadow`
//! ([`check_pid`]); a pid it did not verify is never signalled.
//!
//! The live clock drifts: the live path stamps `t1` by its sample count, and a capture that drops samples puts it
//! behind the wall clock by an amount that only grows. So (emit − `t1`) is drift plus latency, and its absolute value
//! means nothing. The statistic used ([`Stat`]) is the change in (emit − `t1`) within a set of phrases: each one's
//! value less the set's smallest (its fastest phrase, which carries the same drift), then the p90 of that. A drift
//! that is steady across a window cancels; its blind spots are a latency rise that is the same for every phrase in
//! the window (the fastest phrase rises too) and a drift step inside the window (it reads as a rise).
//!
//! Counts only: never a phrase's text.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid as NixPid;

pub use crate::compare::live_latencies;
use crate::compare::rank;

/// How often the window is judged.
pub const TICK: Duration = Duration::from_secs(10);
/// The fewest phrases a window, or a baseline, is judged on.
pub const MIN_PHRASES: usize = 5;
/// `--rise-pct`'s default.
pub const DEFAULT_RISE_PCT: u64 = 30;
/// `--window-s`'s default.
pub const DEFAULT_WINDOW_S: u64 = 120;
/// `--baseline-s`'s default.
pub const DEFAULT_BASELINE_S: u64 = 600;

/// The watchdog's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Settings {
    /// The rise over the baseline that triggers, percent.
    pub rise_pct: u64,
    /// The window judged, ms back from now.
    pub window_ms: u64,
    /// The baseline's span, ms back from the start.
    pub baseline_ms: u64,
}

impl Settings {
    /// From the options' values, seconds for the spans.
    #[must_use]
    pub const fn new(rise_pct: u64, window_s: u64, baseline_s: u64) -> Self {
        Self {
            rise_pct,
            window_ms: window_s.saturating_mul(1000),
            baseline_ms: baseline_s.saturating_mul(1000),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new(DEFAULT_RISE_PCT, DEFAULT_WINDOW_S, DEFAULT_BASELINE_S)
    }
}

/// A set of phrases' latency statistic: the p90 of (emit − `t1`) less the set's smallest, ms; and how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stat {
    /// The p90 of the change in (emit − `t1`) over the set, ms.
    pub p90_ms: i64,
    /// Phrases in the set.
    pub n: usize,
}

impl Stat {
    /// A statistic of `n` phrases with this p90 (for a caller's own comparisons, as in tests).
    #[must_use]
    pub const fn new(p90_ms: i64, n: usize) -> Self {
        Self { p90_ms, n }
    }

    /// The statistic of these (emit − `t1`) values; `None` for none.
    #[must_use]
    pub fn of(values: &[i64]) -> Option<Self> {
        let floor = values.iter().copied().min()?;
        let mut change: Vec<i64> = values.iter().map(|v| v.saturating_sub(floor)).collect();
        Some(Self {
            p90_ms: rank(&mut change, 90)?,
            n: values.len(),
        })
    }
}

/// One judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// Fewer than [`MIN_PHRASES`] phrases to take a baseline from (this many); nothing is judged until there are.
    NoBaseline {
        /// The phrases there were.
        n: usize,
    },
    /// The window is within the bound, or has fewer than [`MIN_PHRASES`] phrases (`window` is `None` then).
    Within {
        /// The baseline.
        baseline: Stat,
        /// The window, when it has enough phrases.
        window: Option<Stat>,
        /// Phrases in the window.
        n: usize,
    },
    /// The window's p90 is past baseline × (1 + rise): stop the shadow.
    Rise {
        /// The baseline.
        baseline: Stat,
        /// The window.
        window: Stat,
    },
}

/// The live log read as it grows: each [`LiveTail::read`] parses only what was appended since the one before.
///
/// Its points are, at every read, exactly [`live_latencies`] of the whole file: a live line's point depends on that
/// line alone (its `HH:MM:SS` is placed on the day nearest its own `t1`), so no state is carried across lines. The
/// partial last line is parsed at each read as the whole read would, and kept only once its newline is there. A
/// file that shrank, another file under the path, or a failed read starts over from byte 0.
#[derive(Debug, Default)]
pub struct LiveTail {
    /// The file read, `(dev, inode)`.
    file: Option<(u64, u64)>,
    /// Bytes read through the last newline.
    done: u64,
    /// The points of the lines through `done`, then the partial last line's.
    points: Vec<(u64, i64)>,
    /// How many of `points` are the whole lines'.
    whole: usize,
}

impl LiveTail {
    /// The points of the file at `path` as it is now.
    ///
    /// # Errors
    /// The file cannot be opened or read; the next read starts over from byte 0.
    pub fn read(&mut self, path: &Path) -> io::Result<&[(u64, i64)]> {
        let bytes = match self.appended(path) {
            Ok(b) => b,
            Err(e) => {
                *self = Self::default();
                return Err(e);
            }
        };
        let cut = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i.saturating_add(1));
        let (lines, partial) = bytes.split_at(cut);
        self.points.truncate(self.whole);
        self.points
            .extend(live_latencies(&String::from_utf8_lossy(lines)));
        self.whole = self.points.len();
        self.done = self
            .done
            .saturating_add(u64::try_from(cut).unwrap_or(u64::MAX));
        self.points
            .extend(live_latencies(&String::from_utf8_lossy(partial)));
        Ok(&self.points)
    }

    /// The bytes after `done`, after starting over if the file is another one or shrank below `done`.
    fn appended(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let mut f = File::open(path)?;
        let meta = f.metadata()?;
        let id = (meta.dev(), meta.ino());
        if self.file != Some(id) || meta.len() < self.done {
            *self = Self {
                file: Some(id),
                ..Self::default()
            };
        }
        f.seek(SeekFrom::Start(self.done))?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// The watch: its settings, its start, and its baseline once there is one.
#[derive(Debug, Clone)]
pub struct Watch {
    settings: Settings,
    start_ms: u64,
    baseline: Option<Stat>,
}

/// The values of the points whose emit time is in `[from, to)`.
fn between(points: &[(u64, i64)], from: u64, to: u64) -> Vec<i64> {
    points
        .iter()
        .filter(|(emit, _)| (from..to).contains(emit))
        .map(|&(_, v)| v)
        .collect()
}

impl Watch {
    /// A watch started at `start_ms` (epoch ms).
    #[must_use]
    pub const fn new(settings: Settings, start_ms: u64) -> Self {
        Self {
            settings,
            start_ms,
            baseline: None,
        }
    }

    /// The baseline, once taken.
    #[must_use]
    pub const fn baseline(&self) -> Option<Stat> {
        self.baseline
    }

    /// Judges the live log's `(emit_ms, emit − t1)` points at `now_ms`.
    ///
    /// The baseline is the phrases emitted in the `baseline` span before the start. With fewer than
    /// [`MIN_PHRASES`] there, it is taken, once there are enough, from that span's start up to the current window's
    /// start instead (phrases heard while the shadow already ran: the best there is). Once taken it is kept.
    ///
    /// The window is the phrases emitted in the last `window` span, from the start on (never one the baseline
    /// holds). With at least [`MIN_PHRASES`] of them, a window p90 past baseline p90 × (1 + `rise_pct`/100) is a
    /// [`Verdict::Rise`].
    pub fn judge(&mut self, points: &[(u64, i64)], now_ms: u64) -> Verdict {
        let s = self.settings;
        let window_from = now_ms.saturating_sub(s.window_ms);
        let baseline = if let Some(b) = self.baseline {
            b
        } else {
            let from = self.start_ms.saturating_sub(s.baseline_ms);
            let mut set = between(points, from, self.start_ms);
            if set.len() < MIN_PHRASES {
                set = between(points, from, window_from);
            }
            match Stat::of(&set).filter(|b| b.n >= MIN_PHRASES) {
                Some(b) => *self.baseline.insert(b),
                None => return Verdict::NoBaseline { n: set.len() },
            }
        };
        let in_window = between(
            points,
            window_from.max(self.start_ms),
            now_ms.saturating_add(1),
        );
        let n = in_window.len();
        let window = Stat::of(&in_window).filter(|w| w.n >= MIN_PHRASES);
        match window {
            Some(w) if past(w.p90_ms, baseline.p90_ms, s.rise_pct) => Verdict::Rise {
                baseline,
                window: w,
            },
            _ => Verdict::Within {
                baseline,
                window,
                n,
            },
        }
    }
}

/// Whether `window` is past `baseline` × (1 + `rise_pct`/100), in whole integers.
fn past(window: i64, baseline: i64, rise_pct: u64) -> bool {
    let rise = i128::from(rise_pct).saturating_add(100);
    i128::from(window).saturating_mul(100) > i128::from(baseline).saturating_mul(rise)
}

/// What a pid is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PidIs {
    /// Running, with a shadow's command line ([`is_shadow`]).
    Shadow,
    /// Running, something else (or this very process).
    Other,
    /// Not running.
    Gone,
}

/// Whether a command line is a shadow's: `--shadow` is the first argument after the `tulpa-hear` binary (a
/// `--replay … --shadow <dir>` is not one).
#[must_use]
pub fn is_shadow(command: &str) -> bool {
    command.contains("tulpa-hear --shadow")
}

/// Reads `pid`'s command line with `ps -p <pid> -o command=` (the same on macOS and Linux): no line (`ps` exits 1)
/// is [`PidIs::Gone`]. This process itself is never a shadow.
///
/// # Errors
///
/// `ps` could not be run.
pub fn check_pid(pid: u32) -> io::Result<PidIs> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    Ok(if !out.status.success() || line.is_empty() {
        PidIs::Gone
    } else if pid != std::process::id() && is_shadow(line) {
        PidIs::Shadow
    } else {
        PidIs::Other
    })
}

/// Sends SIGTERM to `pid` once [`check_pid`] says it is a shadow; what it found otherwise, and nothing is sent.
///
/// # Errors
///
/// A pid outside `2..=i32::MAX`, `ps` not run, or the `kill` refused.
pub fn terminate(pid: u32) -> Result<PidIs, String> {
    let raw = i32::try_from(pid)
        .ok()
        .filter(|&p| p > 1)
        .ok_or_else(|| format!("pid {pid} is not one to signal"))?;
    match check_pid(pid).map_err(|e| format!("ps: {e}"))? {
        PidIs::Shadow => {}
        other => return Ok(other),
    }
    match kill(NixPid::from_raw(raw), Signal::SIGTERM) {
        Ok(()) => Ok(PidIs::Shadow),
        Err(nix::errno::Errno::ESRCH) => Ok(PidIs::Gone),
        Err(e) => Err(format!("kill {pid}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stat_is_the_change_not_the_value() {
        let s = Stat::of(&[90_000, 91_000, 92_000, 90_500, 90_000]);
        assert_eq!(
            s,
            Some(Stat {
                p90_ms: 2_000,
                n: 5
            })
        );
        assert_eq!(
            Stat::of(&[0, 1_000, 2_000, 500, 0]),
            s,
            "the same set without the drift"
        );
        assert_eq!(Stat::of(&[]), None);
    }

    #[test]
    fn past_is_strictly_beyond_the_bound() {
        assert!(!past(1_300, 1_000, 30));
        assert!(past(1_301, 1_000, 30));
        assert!(past(1, 0, 30));
        assert!(!past(0, 0, 30));
        assert!(!past(i64::MAX, i64::MAX, 0));
    }

    #[test]
    fn only_a_tulpa_hear_shadow_command_line_is_one() {
        assert!(is_shadow(
            "/Applications/x.app/Contents/MacOS/tulpa-hear --shadow"
        ));
        assert!(is_shadow("tulpa-hear --shadow /tmp/x"));
        assert!(!is_shadow("tulpa-hear --ears"));
        assert!(!is_shadow("sleep --shadow"));
        assert!(!is_shadow("tulpa-hear --replay /tmp/ring --shadow /tmp/x"));
        assert!(!is_shadow("sleep 30"));
    }
}
