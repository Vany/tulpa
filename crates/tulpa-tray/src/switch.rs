//! Every switch in the tray's menu tells the link what it changed, one event per change.
//!
//! `{"event":"tray","state":"switch","item":<Switch::id>,"from":<old>,"to":<new>,"source":"menu","ts":<UTC>}`; a
//! click that leaves the switch where it was sends nothing. The guild maps it to `senses.dev.<device>.tray.switch`
//! (guild src/dispatcher/device-events.ts); the one line both sides test against is guild
//! test/fixtures/tulpa-tray-switch.jsonl ([`tests`] here asserts its bytes). It is said in addition to the ears'
//! own `mode` event and Pause all's `pause` event ([`crate::pause`]), which stay as they are.
//!
//! The menu's other rows are actions, not switches: Quit, Open config, the ears line (opens a Settings pane),
//! a question's Allow / Deny, and a forward's or link serve's close row ([`of`] names each).

use std::time::SystemTime;

use serde_json::{Value, json};

use crate::devices::DeviceRole;
use crate::link::Outbox;
use crate::log::format_utc;
use crate::model::MenuEntry;

/// A menu switch, by its stable id on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Switch {
    /// «Pause all»: `on` | `off`.
    Pause,
    /// The ears' mode picker: an [`crate::ears::EarsMode`] id.
    EarsMode,
    /// «Hands: full»: `on` | `off`.
    HandsFull,
    /// A device picker: the device's name, or [`DEFAULT`].
    Device(DeviceRole),
}

impl Switch {
    /// The event's `"item"`: `pause`, `ears-mode`, `hands-full`, `device-mic`, `device-output`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::EarsMode => "ears-mode",
            Self::HandsFull => "hands-full",
            Self::Device(DeviceRole::Mic) => "device-mic",
            Self::Device(DeviceRole::Output) => "device-output",
        }
    }
}

/// A device picker's state when no device is picked (the system default).
pub const DEFAULT: &str = "default";

/// An on/off switch's state id.
#[must_use]
pub const fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// A device picker's state id: the device's name, or [`DEFAULT`].
#[must_use]
pub fn device(name: Option<&str>) -> &str {
    name.unwrap_or(DEFAULT)
}

/// The switch a menu row is, if it is one. No wildcard: a new row kind does not compile until it is placed here.
#[must_use]
pub const fn of(entry: &MenuEntry) -> Option<Switch> {
    match entry {
        MenuEntry::Pause { .. } => Some(Switch::Pause),
        MenuEntry::EarsModes { .. } => Some(Switch::EarsMode),
        MenuEntry::HandsFull { .. } => Some(Switch::HandsFull),
        MenuEntry::Devices { role, .. } => Some(Switch::Device(*role)),
        MenuEntry::Info(_)
        | MenuEntry::EarsLine(_)
        | MenuEntry::Separator
        | MenuEntry::Quit
        | MenuEntry::Allow { .. }
        | MenuEntry::Deny { .. }
        | MenuEntry::OpenConfig
        | MenuEntry::LogWindow { .. }
        | MenuEntry::Forward { .. }
        | MenuEntry::LinkServe { .. } => None,
    }
}

/// The link event for a menu click that moved `item` from `from` to `to` at `now`; `None` when nothing changed
/// (`from == to`) or `now` has no UTC form.
#[must_use]
pub fn event(item: Switch, from: &str, to: &str, now: SystemTime) -> Option<Value> {
    (from != to).then_some(())?;
    Some(json!({
        "event": "tray",
        "state": "switch",
        "item": item.id(),
        "from": from,
        "to": to,
        "source": "menu",
        "ts": format_utc(now)?,
    }))
}

/// Queues [`event`] on the link's outbox, once; no change, or a clock with no UTC form, sends nothing.
pub fn announce(outbox: &Outbox, item: Switch, from: &str, to: &str, now: SystemTime) {
    if let Some(event) = event(item, from, to, now) {
        outbox.push(event.to_string().into_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::ears::EarsMode;

    /// 2026-11-25T19:30:00Z.
    fn at() -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(1_795_635_000))
            .unwrap_or(UNIX_EPOCH)
    }

    const ALL: [Switch; 5] = [
        Switch::Pause,
        Switch::EarsMode,
        Switch::HandsFull,
        Switch::Device(DeviceRole::Mic),
        Switch::Device(DeviceRole::Output),
    ];

    /// The seam with the guild: these exact bytes are its fixture test/fixtures/tulpa-tray-switch.jsonl, fed to
    /// senseOf in test/dispatcher/device-events.test.ts. Change one, change both.
    #[test]
    fn an_ears_mode_click_is_the_guild_fixture_line() {
        let outbox = Outbox::default();
        announce(
            &outbox,
            Switch::EarsMode,
            EarsMode::Off.id(),
            EarsMode::Office.id(),
            at(),
        );
        assert_eq!(
            outbox.drain(),
            vec![
                b"{\"event\":\"tray\",\"from\":\"off\",\"item\":\"ears-mode\",\"source\":\"menu\",\"state\":\"switch\",\"to\":\"office\",\"ts\":\"2026-11-25T19:30:00Z\"}\n".to_vec()
            ]
        );
    }

    #[test]
    fn a_change_queues_exactly_one_line_and_no_change_none()
    -> Result<(), Box<dyn std::error::Error>> {
        for item in ALL {
            let outbox = Outbox::default();
            announce(&outbox, item, "off", "on", at());
            let lines: Vec<Value> = outbox
                .drain()
                .iter()
                .map(|line| serde_json::from_slice(line))
                .collect::<Result<_, _>>()?;
            assert_eq!(
                lines,
                vec![
                    json!({"event": "tray", "state": "switch", "item": item.id(), "from": "off", "to": "on", "source": "menu", "ts": "2026-11-25T19:30:00Z"})
                ],
                "{item:?}"
            );
            announce(&outbox, item, "on", "on", at());
            assert!(outbox.drain().is_empty(), "{item:?}: no change, no line");
        }
        Ok(())
    }

    #[test]
    fn item_and_state_ids_are_stable_ascii_and_distinct() {
        let ids: Vec<&str> = ALL.iter().map(|s| s.id()).collect();
        for id in &ids {
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{id}"
            );
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        assert_eq!((on_off(true), on_off(false)), ("on", "off"));
        assert_eq!(device(None), DEFAULT);
        assert_eq!(device(Some("Desk Speakers")), "Desk Speakers");
    }

    #[test]
    fn every_switch_row_kind_is_one_switch() {
        let rows = [
            MenuEntry::Pause { checked: false },
            MenuEntry::EarsModes {
                current: EarsMode::Off,
            },
            MenuEntry::HandsFull { checked: false },
            MenuEntry::Devices {
                role: DeviceRole::Mic,
                names: vec![],
                current: None,
            },
            MenuEntry::Devices {
                role: DeviceRole::Output,
                names: vec![],
                current: None,
            },
        ];
        let found: Vec<Option<Switch>> = rows.iter().map(of).collect();
        assert_eq!(found, ALL.map(Some).to_vec());
        for action in [MenuEntry::Quit, MenuEntry::OpenConfig, MenuEntry::Separator] {
            assert_eq!(of(&action), None);
        }
    }

    /// The menu's click handler is macOS-only and never runs here: its source is read instead. Each switch's
    /// branch announces it exactly once, and nothing else in the handler does.
    #[test]
    fn the_menu_handler_announces_each_switch_once() {
        let source = include_str!("macos.rs");
        for (item, needle) in [
            (Switch::Pause, "Switch::Pause,"),
            (Switch::EarsMode, "Switch::EarsMode,"),
            (Switch::HandsFull, "Switch::HandsFull,"),
            (Switch::Device(DeviceRole::Mic), "Switch::Device(role),"),
        ] {
            assert_eq!(
                source.matches(needle).count(),
                1,
                "{item:?}: one announce in macos.rs"
            );
        }
        assert_eq!(
            source.matches("crate::switch::announce(").count(),
            4,
            "four switch announces in macos.rs (both device pickers share one)"
        );
    }
}
