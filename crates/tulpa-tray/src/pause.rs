//! The tray's Pause all on the link, one event per toggle.
//!
//! `{"event":"pause","state":"paused"|"resumed","scope":"all","senses":<held>,"ts":<UTC>}`, and a resume after a
//! Pause that stopped the ears adds `"reason"`: they stay off, and what turns them on ([`EARS_OFF_AFTER_PAUSE`]).
//! The guild maps it to `senses.dev.<device>.pause.state` (guild src/dispatcher/device-events.ts); the one line
//! both sides test against is guild test/fixtures/tulpa-pause-resumed.jsonl ([`tests`] here asserts its bytes).
//!
//! Pause all is the only pause the tray has: the ears mode menu's own switch is told by the ears' `mode` event.

use std::time::SystemTime;

use serde_json::{Value, json};

use crate::ears::EARS_OFF_AFTER_PAUSE;
use crate::link::Outbox;
use crate::log::format_utc;

/// What Pause all holds, comma-joined: hands verbs and transfers (with their asks), the ears' capture, the voice,
/// the eyes, the port forwards and the link serves.
pub const SENSES: &str = "hands,ears,voice,eyes,forwards,links";

/// The link event for Pause all turning `paused` on or off at `now`; `ears_off` (resume only): the pause stopped
/// the ears ([`crate::ears::Ears::off_by_pause`]). `None` when `now` has no UTC form.
#[must_use]
pub fn event(paused: bool, ears_off: bool, now: SystemTime) -> Option<Value> {
    let mut event = json!({
        "event": "pause",
        "state": if paused { "paused" } else { "resumed" },
        "scope": "all",
        "senses": SENSES,
        "ts": format_utc(now)?,
    });
    if !paused
        && ears_off
        && let Some(fields) = event.as_object_mut()
    {
        fields.insert(
            "reason".to_owned(),
            Value::from(format!("ears {EARS_OFF_AFTER_PAUSE}")),
        );
    }
    Some(event)
}

/// Queues [`event`] on the link's outbox, once; a clock with no UTC form sends nothing.
pub fn announce(outbox: &Outbox, paused: bool, ears_off: bool, now: SystemTime) {
    if let Some(event) = event(paused, ears_off, now) {
        outbox.push(event.to_string().into_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    /// 2026-11-25T19:30:00Z.
    fn at() -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(1_795_635_000))
            .unwrap_or(UNIX_EPOCH)
    }

    /// The seam with the guild: these exact bytes are its fixture test/fixtures/tulpa-pause-resumed.jsonl, fed to
    /// senseOf in test/dispatcher/device-events.test.ts. Change one, change both.
    #[test]
    fn a_resume_after_ears_were_stopped_is_the_guild_fixture_line() {
        let outbox = Outbox::default();
        announce(&outbox, false, true, at());
        assert_eq!(
            outbox.drain(),
            vec![
                b"{\"event\":\"pause\",\"reason\":\"ears off since Pause; Resume does not restart them, ears.mode or the menu turns them on\",\"scope\":\"all\",\"senses\":\"hands,ears,voice,eyes,forwards,links\",\"state\":\"resumed\",\"ts\":\"2026-11-25T19:30:00Z\"}\n".to_vec()
            ]
        );
    }

    #[test]
    fn each_toggle_queues_exactly_one_line_and_only_a_resume_says_the_ears_stay_off()
    -> Result<(), Box<dyn std::error::Error>> {
        let reason = format!("ears {EARS_OFF_AFTER_PAUSE}");
        for (paused, ears_off, want) in [
            (
                true,
                false,
                json!({"event": "pause", "state": "paused", "scope": "all", "senses": SENSES, "ts": "2026-11-25T19:30:00Z"}),
            ),
            (
                true,
                true,
                json!({"event": "pause", "state": "paused", "scope": "all", "senses": SENSES, "ts": "2026-11-25T19:30:00Z"}),
            ),
            (
                false,
                false,
                json!({"event": "pause", "state": "resumed", "scope": "all", "senses": SENSES, "ts": "2026-11-25T19:30:00Z"}),
            ),
            (
                false,
                true,
                json!({"event": "pause", "state": "resumed", "scope": "all", "senses": SENSES, "ts": "2026-11-25T19:30:00Z", "reason": reason}),
            ),
        ] {
            let outbox = Outbox::default();
            announce(&outbox, paused, ears_off, at());
            let lines: Vec<Value> = outbox
                .drain()
                .iter()
                .map(|line| serde_json::from_slice(line))
                .collect::<Result<_, _>>()?;
            assert_eq!(lines, vec![want], "paused={paused} ears_off={ears_off}");
        }
        Ok(())
    }

    #[test]
    fn a_clock_before_the_epoch_sends_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let outbox = Outbox::default();
        let before = UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .ok_or("no time before the epoch")?;
        announce(&outbox, true, false, before);
        assert!(outbox.drain().is_empty());
        Ok(())
    }
}
