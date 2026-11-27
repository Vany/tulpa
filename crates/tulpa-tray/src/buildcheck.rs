//! The build check's outcome marker, and its ride to the agent.
//!
//! `packaging/macos/build-check.sh` — started at every login by the
//! `LaunchAgent` `com.tulpa.build-check` — compares the running bundle's commit
//! marker (written by `scripts/bundle-macos.sh`) with `refs/tags/latest-good`
//! on the origin, installs the staged bundle when they differ, and writes the
//! outcome here, atomically, as `key=value` lines a POSIX shell reads:
//!
//! ```text
//! outcome=reverted
//! commit=1a2b3c4d5e6f
//! latest=1a2b3c4d5e6f
//! from=77aa88bb99cc
//! utc=2026-11-18T17:00:00Z
//! ```
//!
//! `outcome` is one word: `ok` (the running build is the latest good one),
//! `updated` (the staged latest-good bundle was installed over the build the
//! earlier marker saw running, or there was no earlier marker), `reverted`
//! (another build was running than the earlier marker saw — one installed by
//! hand between checks — and the check put latest-good over it, whether or
//! not latest-good moved meanwhile), or `failed-…` naming the step
//! that gave up (`failed-ls-remote`, `failed-staging`, `failed-install`).
//! `commit` is the build running when the marker was written; `from`, written
//! by `updated` and `reverted` only, is the build that ran before the install.
//!
//! The tray watches the marker at startup and then every
//! [`BUILD_CHECK_EVERY`], and when its meaning changes pushes one
//! `{"event":"link","state":"build-check","reason":…}` line: the agent's
//! device-events door already passes `link` events with a `reason` word, so a
//! Mac that fell behind (or failed to catch up) becomes visible on the bus
//! without anyone reading the Mac's log. A rewrite with the same meaning —
//! every login while `ok` — says nothing twice.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tulpa_hands::Journal;

use crate::link::Outbox;
use crate::log::format_utc;

/// Where the marker lives; the check script writes the same path.
pub const BUILD_CHECK_PATH: &str = "~/Library/Application Support/tulpa/build-check";

/// How often the marker is looked at; the check runs at login, so this only
/// sets how late the tray may notice.
pub const BUILD_CHECK_EVERY: Duration = Duration::from_secs(30);

/// What the marker may say, at most; a longer line is refused, not trimmed. A
/// full sha is 40 characters and `-dirty` makes 46, so anything shorter than
/// 47 would call the check's own commit `unknown` — the check's `word()` is
/// unbounded, and the two must agree across the marker seam.
const WORD_MAX: usize = 47;

/// One outcome marker: what the last build check said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildCheck {
    outcome: String,
    commit: String,
    latest: String,
    from: String,
}

impl BuildCheck {
    /// Reads and parses the marker at `path`; `None` when it is missing,
    /// unreadable, or says no `outcome` (the check has not run, or died
    /// mid-write — the atomic write makes a half marker impossible, a missing
    /// one simply means not yet).
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut outcome = None;
        let mut commit = "unknown".to_owned();
        let mut latest = "unknown".to_owned();
        let mut from = "unknown".to_owned();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = word_or_unknown(value);
            match key {
                "outcome" => outcome = Some(value),
                "commit" => commit = value,
                "latest" => latest = value,
                "from" => from = value,
                _ => {}
            }
        }
        Some(Self {
            outcome: outcome?,
            commit,
            latest,
            from,
        })
    }

    /// The `reason` word for this outcome, e.g. `ok 1a2b.. = latest-good`,
    /// `updated 1a2b.. -> 9f8e..` (the build replaced, the build now running)
    /// or `reverted 77aa.. -> 9f8e..` (the hand-installed build put back, the
    /// latest-good it was replaced with); every part is a single word, so the
    /// whole line rides the link's `reason` door, and a subscriber can match a
    /// `reason` starting with `reverted`.
    #[must_use]
    pub fn reason(&self) -> String {
        match self.outcome.as_str() {
            "ok" => format!("ok {} = latest-good {}", self.commit, self.latest),
            "updated" => format!("updated {} -> {}", self.from, self.commit),
            "reverted" => format!("reverted {} -> {}", self.from, self.latest),
            other => format!("{other} {} latest-good {}", self.commit, self.latest),
        }
    }

    /// The tray log's one line for this meaning: the reason, except that a
    /// revert says loudly what was running and what replaced it.
    fn journal_line(&self) -> String {
        if self.outcome == "reverted" {
            format!(
                "link: build check REVERTED: {} was running but is not latest-good {}; \
                 installed latest-good",
                self.from, self.latest
            )
        } else {
            format!("link: build check {}", self.reason())
        }
    }

    /// The link event for this outcome at `now`; `None` when `now` has no UTC
    /// form.
    #[must_use]
    pub fn event(&self, now: SystemTime) -> Option<Value> {
        Some(json!({
            "event": "link",
            "state": "build-check",
            "reason": self.reason(),
            "ts": format_utc(now)?,
        }))
    }
}

/// A word of the token class the marker allows, or `unknown`; a value with
/// spaces, controls, or past [`WORD_MAX`] is not a word the check wrote.
fn word_or_unknown(value: &str) -> String {
    let word = !value.is_empty()
        && value.len() <= WORD_MAX
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte));
    if word {
        value.to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// Watches the marker and speaks its changes to the link.
///
/// The first look happens at startup: the login check often finishes before
/// the tray does, and a check that finishes later is caught by the next poll.
pub struct BuildCheckWatch {
    path: PathBuf,
    outbox: Arc<Outbox>,
    journal: Arc<dyn Journal>,
    seen: Mutex<Option<BuildCheck>>,
}

impl std::fmt::Debug for BuildCheckWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildCheckWatch")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl BuildCheckWatch {
    /// A watch on `path`, speaking through `outbox` and noting changes in
    /// `journal`.
    #[must_use]
    pub fn new(path: PathBuf, outbox: Arc<Outbox>, journal: Arc<dyn Journal>) -> Self {
        Self {
            path,
            outbox,
            journal,
            seen: Mutex::new(None),
        }
    }

    /// Looks at the marker once: a meaning not seen before (the first one
    /// included) is journalled and pushed to the link; a rewrite with the same
    /// meaning — every login while `ok` — says nothing. A new `reverted`
    /// meaning is journalled in words — what was put back, and with what — so
    /// the tray log is as loud as the check's own.
    pub fn poll(&self) {
        let Some(check) = BuildCheck::read(&self.path) else {
            return;
        };
        let fresh = {
            let Ok(mut seen) = self.seen.lock() else {
                return;
            };
            if seen.as_ref() == Some(&check) {
                false
            } else {
                *seen = Some(check.clone());
                true
            }
        };
        if !fresh {
            return;
        }
        self.journal.record(&check.journal_line());
        if let Some(event) = check.event(SystemTime::now()) {
            self.outbox.push(event.to_string().into_bytes());
        }
    }

    /// Looks at the marker every `every`, until the watch itself is dropped.
    pub fn spawn(self: &Arc<Self>, every: Duration) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                let Some(watch) = weak.upgrade() else {
                    return;
                };
                watch.poll();
                drop(watch);
                std::thread::sleep(every);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = format!(
            "tulpa-buildcheck-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        std::env::temp_dir().join(unique)
    }

    const AT: Duration = Duration::from_secs(1_763_220_600);

    fn write_marker(
        path: &Path,
        outcome: &str,
        commit: &str,
        latest: &str,
        utc: &str,
    ) -> std::io::Result<()> {
        fs::write(
            path,
            format!("outcome={outcome}\ncommit={commit}\nlatest={latest}\nutc={utc}\n"),
        )
    }

    #[test]
    fn a_marker_parses_and_odd_values_become_unknown() -> std::io::Result<()> {
        let dir = temp_dir("parse");
        fs::create_dir_all(&dir)?;
        let path = dir.join("build-check");
        write_marker(
            &path,
            "ok",
            "1a2b3c4d5e6f",
            "1a2b3c4d5e6f",
            "2026-11-18T17:00:00Z",
        )?;
        assert_eq!(
            BuildCheck::read(&path),
            Some(BuildCheck {
                outcome: "ok".to_owned(),
                commit: "1a2b3c4d5e6f".to_owned(),
                latest: "1a2b3c4d5e6f".to_owned(),
                from: "unknown".to_owned(),
            }),
            "no from line: from is unknown"
        );
        fs::write(
            &path,
            "outcome=reverted\ncommit=9f8e7d\nlatest=9f8e7d\nfrom=77aa88-dirty\nutc=x\n",
        )?;
        assert_eq!(
            BuildCheck::read(&path).map(|check| check.from),
            Some("77aa88-dirty".to_owned()),
            "from parses"
        );
        write_marker(&path, "failed-install", "a b", "x\ty", "utc")?;
        assert_eq!(
            BuildCheck::read(&path),
            Some(BuildCheck {
                outcome: "failed-install".to_owned(),
                commit: "unknown".to_owned(),
                latest: "unknown".to_owned(),
                from: "unknown".to_owned(),
            }),
            "values that are not one word are unknown, not trimmed"
        );
        fs::write(&path, "commit=abc\n")?;
        assert_eq!(BuildCheck::read(&path), None, "no outcome: not a marker");
        // An `outcome` with no value is not "no outcome": the check ran and
        // its marker is weird, which the event says as `unknown …` — better
        // vague than silent (akira's ruling, 2026-11-18). Pinned here so the
        // emission stays deliberate.
        fs::write(&path, "outcome=\ncommit=abc\n")?;
        let empty_outcome = BuildCheck::read(&path);
        assert_eq!(
            empty_outcome,
            Some(BuildCheck {
                outcome: "unknown".to_owned(),
                commit: "abc".to_owned(),
                latest: "unknown".to_owned(),
                from: "unknown".to_owned(),
            }),
            "an empty outcome is unknown, not absent"
        );
        assert_eq!(
            empty_outcome
                .as_ref()
                .and_then(|check| check.event(UNIX_EPOCH))
                .and_then(|event| event.pointer("/reason").cloned()),
            Some(json!("unknown abc latest-good unknown")),
            "the event says the check ran and the marker is weird"
        );
        assert_eq!(
            BuildCheck::read(&dir.join("absent")),
            None,
            "no file yet: the check has not run"
        );
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn the_reason_is_one_door_safe_line() {
        let reason = BuildCheck {
            outcome: "updated".to_owned(),
            commit: "9f8e7d6c5b4a".to_owned(),
            latest: "9f8e7d6c5b4a".to_owned(),
            from: "1a2b3c4d5e6f".to_owned(),
        }
        .reason();
        assert_eq!(
            reason, "updated 1a2b3c4d5e6f -> 9f8e7d6c5b4a",
            "updated names the build replaced, then the one running"
        );
        let reason = BuildCheck {
            outcome: "reverted".to_owned(),
            commit: "unknown".to_owned(),
            latest: "9f8e7d6c5b4a".to_owned(),
            from: "77aa88bb99cc-dirty".to_owned(),
        }
        .reason();
        assert_eq!(
            reason, "reverted 77aa88bb99cc-dirty -> 9f8e7d6c5b4a",
            "reverted names the hand build put back, then latest-good"
        );
        for outcome in ["ok", "updated", "reverted", "failed-ls-remote"] {
            let reason = BuildCheck {
                outcome: outcome.to_owned(),
                commit: "1a2b3c4d5e6f-dirty".to_owned(),
                latest: "unknown".to_owned(),
                from: "unknown".to_owned(),
            }
            .reason();
            assert!(
                !reason.chars().any(char::is_control) && reason.chars().count() <= 200,
                "{reason}"
            );
            assert!(reason.starts_with(outcome), "{reason}");
        }
    }

    /// The field, or Null when missing: clippy keeps us off indexing panics.
    fn field<'a>(event: &'a Value, key: &str) -> &'a Value {
        event.get(key).unwrap_or(&Value::Null)
    }

    #[test]
    fn the_event_has_the_receiving_shape() {
        let event = BuildCheck {
            outcome: "ok".to_owned(),
            commit: "abc".to_owned(),
            latest: "abc".to_owned(),
            from: "unknown".to_owned(),
        }
        .event(UNIX_EPOCH + AT)
        .unwrap_or_default();
        assert_eq!(
            (
                field(&event, "event").as_str(),
                field(&event, "state").as_str()
            ),
            (Some("link"), Some("build-check"))
        );
        assert!(
            field(&event, "ts")
                .as_str()
                .is_some_and(|ts| ts.ends_with('Z') && ts.len() == 20)
        );
    }

    #[derive(Debug, Default)]
    struct Notes(Mutex<Vec<String>>);
    impl Journal for Notes {
        fn record(&self, line: &str) {
            if let Ok(mut lines) = self.0.lock() {
                lines.push(line.to_owned());
            }
        }
    }
    impl Notes {
        fn take(&self) -> Vec<String> {
            self.0
                .lock()
                .map(|mut lines| std::mem::take(&mut *lines))
                .unwrap_or_default()
        }
    }

    #[test]
    fn a_revert_is_journalled_once_in_words() -> std::io::Result<()> {
        let dir = temp_dir("revert");
        fs::create_dir_all(&dir)?;
        let path = dir.join("build-check");
        let notes = Arc::new(Notes::default());
        let watch = BuildCheckWatch::new(
            path.clone(),
            Arc::new(Outbox::default()),
            Arc::clone(&notes) as Arc<dyn Journal>,
        );

        write_marker(&path, "ok", "9f8e7d", "9f8e7d", "17:00:00")?;
        watch.poll();
        assert_eq!(notes.take().len(), 1);
        drop(watch.outbox.drain());

        let reverted = "outcome=reverted\ncommit=9f8e7d\nlatest=9f8e7d\nfrom=77aa88\nutc=";
        fs::write(&path, format!("{reverted}18:00:00\n"))?;
        watch.poll();
        assert_eq!(
            notes.take(),
            vec![
                "link: build check REVERTED: 77aa88 was running but is not latest-good 9f8e7d; \
                 installed latest-good"
                    .to_owned()
            ],
            "one loud line, naming what was put back and with what"
        );
        let lines = watch.outbox.drain();
        let event = lines
            .first()
            .and_then(|line| serde_json::from_slice::<Value>(line).ok())
            .unwrap_or_default();
        assert_eq!(
            (lines.len(), field(&event, "reason")),
            (1, &json!("reverted 77aa88 -> 9f8e7d"))
        );

        fs::write(&path, format!("{reverted}19:00:00\n"))?;
        watch.poll();
        assert!(notes.take().is_empty(), "the same revert is not said twice");
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn the_watch_speaks_each_meaning_once() -> std::io::Result<()> {
        let dir = temp_dir("watch");
        fs::create_dir_all(&dir)?;
        let path = dir.join("build-check");

        let watch = BuildCheckWatch::new(
            path.clone(),
            Arc::new(Outbox::default()),
            Arc::new(Notes::default()),
        );
        watch.poll();
        assert!(
            watch.outbox.drain().is_empty(),
            "no marker yet: the check has not run"
        );

        write_marker(&path, "failed-ls-remote", "c14899d", "c14899d", "17:00:00")?;
        watch.poll();
        let lines = watch.outbox.drain();
        assert_eq!(lines.len(), 1);
        let event = lines
            .first()
            .and_then(|line| serde_json::from_slice::<Value>(line).ok())
            .unwrap_or_default();
        assert_eq!(field(&event, "state"), "build-check");
        assert_eq!(
            field(&event, "reason"),
            "failed-ls-remote c14899d latest-good c14899d"
        );

        write_marker(&path, "ok", "c14899d", "c14899d", "18:00:00")?;
        watch.poll();
        let lines = watch.outbox.drain();
        assert_eq!(lines.len(), 1, "a new meaning is spoken");
        let event = lines
            .first()
            .and_then(|line| serde_json::from_slice::<Value>(line).ok())
            .unwrap_or_default();
        assert_eq!(field(&event, "reason"), "ok c14899d = latest-good c14899d");

        write_marker(&path, "ok", "c14899d", "c14899d", "19:00:00")?;
        watch.poll();
        assert!(
            watch.outbox.drain().is_empty(),
            "the same meaning again — every login while ok — says nothing"
        );
        fs::remove_dir_all(&dir)
    }
}
