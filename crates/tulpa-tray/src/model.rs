//! The pure tray model: menu structure, pause/stop actions, and the events they emit.
//!
//! The model never touches the platform. A presenter (the macOS adapter) renders
//! [`TrayModel::menu_entries`] and applies the [`TrayEvent`]s that human controls
//! produce; logging those events is the caller's job so this crate stays I/O-free.

use crate::devices::{AudioDevices, DeviceRole};
use crate::ears::EarsMode;
use crate::state::{LinkState, Organ, OrganMode};

/// What a human control did, to be logged and applied by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TrayEvent {
    /// "Pause all" was clicked; all senses are paused when `paused` is true and
    /// restored when it is false (a repeat click).
    PauseToggled {
        /// New pause state after the click.
        paused: bool,
    },
    /// "Quit" was clicked; the caller stops everything and exits.
    Quit,
}

/// Whether two menus have the same rows in the same order (only texts or check marks differ).
///
/// Then the shown menu is updated in place instead of rebuilt: rebuilding closes it while it is open (a hands
/// command every few seconds kept closing the menu). Allow and Deny for another question are other rows: their
/// items carry the question's number, so a click on an old menu's Allow can't answer the next question.
#[must_use]
pub fn same_layout(a: &[MenuEntry], b: &[MenuEntry]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (MenuEntry::Allow { ask: one }, MenuEntry::Allow { ask: other })
            | (MenuEntry::Deny { ask: one }, MenuEntry::Deny { ask: other }) => one == other,
            // Its item closes that forward: another forward is another item.
            (
                MenuEntry::Forward { port, dir, .. },
                MenuEntry::Forward {
                    port: other_port,
                    dir: other_dir,
                    ..
                },
            ) => port == other_port && dir == other_dir,
            // Its item closes that serve: another serve is another item.
            (
                MenuEntry::LinkServe { link, .. },
                MenuEntry::LinkServe {
                    link: other_link, ..
                },
            ) => link == other_link,
            _ => std::mem::discriminant(x) == std::mem::discriminant(y),
        })
}

/// One row of the tray menu, platform-free.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MenuEntry {
    /// Read-only information line, rendered disabled.
    Info(String),
    /// The ears' state line. While one side is DEGRADED a click opens the
    /// Settings pane that fixes it (the presenter asks the ears which).
    EarsLine(String),
    /// Visual separator.
    Separator,
    /// The "Pause all" toggle.
    Pause {
        /// Whether the toggle is currently checked.
        checked: bool,
    },
    /// The full-stop item.
    Quit,
    /// Allows the question first in line.
    Allow {
        /// The question's number ([`crate::hands::Asked::id`]).
        ask: u64,
    },
    /// Denies the question first in line.
    Deny {
        /// The question's number ([`crate::hands::Asked::id`]).
        ask: u64,
    },
    /// The ears' mode picker: a submenu titled with the current mode, one
    /// check item per mode.
    EarsModes {
        /// The mode now on (checked).
        current: EarsMode,
    },
    /// Opens the tray's config file in the default text editor.
    OpenConfig,
    /// The log window (#0423): the live heard/said log with the operator's
    /// line at the bottom. Checked while the window is open.
    LogWindow {
        /// Whether the window is open.
        open: bool,
    },
    /// The hands' full mode toggle («Hands: full»).
    HandsFull {
        /// Whether full mode is on.
        checked: bool,
    },
    /// An open port forward («53682 → once — click to close»): a click closes it.
    Forward {
        /// Its port.
        port: u16,
        /// Which way.
        dir: crate::forward::Dir,
        /// The row's text.
        line: String,
    },
    /// A served link side («l-1a2b3c4d tcp 25565 client — click to close»): a click closes it.
    LinkServe {
        /// The link's id.
        link: String,
        /// The row's text.
        line: String,
    },
    /// A device picker («Ears mic», «Voice output»): a submenu of the Mac's
    /// devices by name with the current one checked
    /// ([`crate::devices::picker_rows`]).
    Devices {
        /// What it picks.
        role: DeviceRole,
        /// The devices to list, by name.
        names: Vec<String>,
        /// The current pick; `None`: the default.
        current: Option<String>,
    },
}

/// Human-facing state of the menu-bar body.
#[derive(Debug)]
#[non_exhaustive]
pub struct TrayModel {
    link: LinkState,
    host_alias: Option<String>,
    modes: Vec<(Organ, OrganMode)>,
    paused: bool,
    hands_line: Option<String>,
    ears_line: Option<String>,
    eyes_line: Option<String>,
    ask: Option<(u64, String)>,
    ears_mode: Option<EarsMode>,
    hands_full: Option<bool>,
    log_window: bool,
    route_line: Option<String>,
    awake_line: Option<String>,
    /// The device pickers' rows, in menu order; empty hides them.
    devices: Vec<MenuEntry>,
    /// The open port forwards, by port.
    forwards: Vec<crate::forward::View>,
    /// The served link sides.
    links: Vec<crate::linkserve::View>,
}

impl TrayModel {
    /// Builds a model from link state, an optional host alias, and per-organ modes.
    pub fn new(
        link: LinkState,
        host_alias: Option<String>,
        modes: Vec<(Organ, OrganMode)>,
    ) -> Self {
        Self {
            link,
            host_alias: host_alias.filter(|alias| !alias.is_empty()),
            modes,
            paused: false,
            hands_line: None,
            ears_line: None,
            eyes_line: None,
            ask: None,
            ears_mode: None,
            hands_full: None,
            log_window: false,
            route_line: None,
            awake_line: None,
            devices: Vec::new(),
            forwards: Vec::new(),
            links: Vec::new(),
        }
    }

    /// The open port forwards ([`crate::forward::Forwards::list`]); each is a row that closes it.
    pub fn set_forwards(&mut self, forwards: Vec<crate::forward::View>) {
        self.forwards = forwards;
    }

    /// The served link sides ([`crate::linkserve::LinkServe::list`]); each is a row that closes it.
    pub fn set_links(&mut self, links: Vec<crate::linkserve::View>) {
        self.links = links;
    }

    /// The device pickers: `lists` `None` hides them (the ears are off in
    /// config); otherwise «Ears mic» lists the inputs and «Voice output» the
    /// outputs, by name, with `mic` and `output` (`None`: the default) current.
    /// `unheard` (why the device events are not heard) is a line above them.
    pub fn set_devices(
        &mut self,
        lists: Option<&AudioDevices>,
        mic: Option<String>,
        output: Option<String>,
        unheard: Option<&str>,
    ) {
        self.devices = lists.map_or_else(Vec::new, |lists| {
            let unheard = unheard.map(|why| {
                MenuEntry::Info(format!(
                    "devices: not followed ({why}); tried again when this menu opens"
                ))
            });
            unheard
                .into_iter()
                .chain([
                    MenuEntry::Devices {
                        role: DeviceRole::Mic,
                        names: lists.of(DeviceRole::Mic).to_vec(),
                        current: mic,
                    },
                    MenuEntry::Devices {
                        role: DeviceRole::Output,
                        names: lists.of(DeviceRole::Output).to_vec(),
                        current: output,
                    },
                ])
                .collect()
        });
    }

    /// Tulpa's own keep-awake, e.g. «keep-awake: on (always)» or «keep-awake: off»; `None` hides it.
    pub fn set_awake_line(&mut self, line: Option<String>) {
        self.awake_line = line;
    }

    /// The link's route («link: direct (…)» / «link: via …»); shown under the link line.
    pub fn set_route_line(&mut self, line: Option<String>) {
        self.route_line = line.filter(|l| !l.is_empty());
    }

    /// The hands' full mode; `None` (no hands engine) shows no toggle.
    pub const fn set_hands_full(&mut self, full: Option<bool>) {
        self.hands_full = full;
    }

    /// The log window's visibility (#0423): the «Log window» item is checked
    /// while it is open.
    pub const fn set_log_window(&mut self, open: bool) {
        self.log_window = open;
    }

    /// The ears' current mode; `None` (ears off in config) shows no picker.
    pub const fn set_ears_mode(&mut self, mode: Option<EarsMode>) {
        self.ears_mode = mode;
    }

    /// Current link state, reflected by the icon tint.
    pub const fn link_state(&self) -> LinkState {
        self.link
    }

    /// Replaces the link state; the link organ drives this once it exists.
    pub const fn set_link_state(&mut self, link: LinkState) {
        self.link = link;
    }

    /// The live hands line (state, not the configured posture); `None`
    /// shows the configured posture.
    pub fn set_hands_line(&mut self, line: Option<String>) {
        self.hands_line = line;
    }

    /// The live ears line (listening, deaf, …); `None` shows the configured
    /// posture.
    pub fn set_ears_line(&mut self, line: Option<String>) {
        self.ears_line = line;
    }

    /// The eyes' line (camera on request, paused, the last photo); `None`
    /// while the eyes are off in config shows no eyes row.
    pub fn set_eyes_line(&mut self, line: Option<String>) {
        self.eyes_line = line;
    }

    /// The question first in line, by its number and menu line; shown with Allow / Deny for that number.
    pub fn set_ask(&mut self, ask: Option<(u64, String)>) {
        self.ask = ask;
    }

    /// Whether all senses are currently paused.
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// The link status line, e.g. `link: connected via work-box`.
    pub fn link_line(&self) -> String {
        self.host_alias.as_ref().map_or_else(
            || format!("link: {}", self.link.label()),
            |alias| format!("link: {} via {alias}", self.link.label()),
        )
    }

    /// Rows for the menu, top to bottom.
    pub fn menu_entries(&self) -> Vec<MenuEntry> {
        let mut entries = Vec::with_capacity(self.modes.len().saturating_add(5));
        entries.push(MenuEntry::Info(self.link_line()));
        if let Some(route) = &self.route_line {
            entries.push(MenuEntry::Info(route.clone()));
        }
        entries.push(MenuEntry::Separator);
        if let Some((ask, line)) = &self.ask {
            entries.push(MenuEntry::Info(line.clone()));
            entries.push(MenuEntry::Allow { ask: *ask });
            entries.push(MenuEntry::Deny { ask: *ask });
            entries.push(MenuEntry::Separator);
        }
        // The ears (with their modes), the hands, and the eyes while they are
        // on; voice and calls are not organs on this menu (calls is an ears mode).
        for (organ, mode) in &self.modes {
            match organ {
                Organ::Ears => {
                    if let Some(current) = self.ears_mode {
                        entries.push(MenuEntry::EarsModes { current });
                    }
                    entries.push(MenuEntry::EarsLine(
                        self.ears_line
                            .clone()
                            .unwrap_or_else(|| format!("{}: {}", organ.label(), mode.label())),
                    ));
                    entries.extend(self.devices.iter().cloned());
                }
                Organ::Hands => {
                    entries.push(MenuEntry::Info(
                        self.hands_line
                            .clone()
                            .unwrap_or_else(|| format!("{}: {}", organ.label(), mode.label())),
                    ));
                    if let Some(checked) = self.hands_full {
                        entries.push(MenuEntry::HandsFull { checked });
                    }
                }
                Organ::Eyes => {
                    if let Some(line) = &self.eyes_line {
                        entries.push(MenuEntry::Info(line.clone()));
                    }
                }
                _ => {}
            }
        }
        if let Some(awake) = &self.awake_line {
            entries.push(MenuEntry::Info(awake.clone()));
        }
        // Open forwards, under the organs: each its own row, a click closes it.
        for view in &self.forwards {
            entries.push(MenuEntry::Forward {
                port: view.port,
                dir: view.dir,
                line: view.menu_line(),
            });
        }
        // Served link sides, beside the forwards: each its own row, a click closes it.
        for view in &self.links {
            entries.push(MenuEntry::LinkServe {
                link: view.link.clone(),
                line: view.menu_line(),
            });
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::LogWindow {
            open: self.log_window,
        });
        entries.push(MenuEntry::OpenConfig);
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Pause {
            checked: self.paused,
        });
        entries.push(MenuEntry::Quit);
        entries
    }

    /// Toggles "Pause all" and returns the event to log and apply.
    pub const fn toggle_pause(&mut self) -> TrayEvent {
        self.paused = !self.paused;
        TrayEvent::PauseToggled {
            paused: self.paused,
        }
    }

    /// Returns the quit event; the model itself stays unchanged so the caller
    /// can still render a final menu state while shutting down.
    pub const fn quit(&self) -> TrayEvent {
        TrayEvent::Quit
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn a_changed_text_or_check_keeps_the_layout_a_new_row_does_not() {
        let a = vec![
            MenuEntry::Info("hands: idle".to_owned()),
            MenuEntry::Pause { checked: false },
            MenuEntry::Quit,
        ];
        let b = vec![
            MenuEntry::Info("hands: 1 in flight".to_owned()),
            MenuEntry::Pause { checked: true },
            MenuEntry::Quit,
        ];
        assert!(same_layout(&a, &b));
        let c = vec![
            MenuEntry::Info("x".to_owned()),
            MenuEntry::Allow { ask: 1 },
            MenuEntry::Quit,
        ];
        assert!(!same_layout(&a, &c), "a question's Allow where Pause was");
        assert!(
            !same_layout(&a, a.get(..2).unwrap_or_default()),
            "a row fewer"
        );
        let same_question = vec![
            MenuEntry::Info("y".to_owned()),
            MenuEntry::Allow { ask: 1 },
            MenuEntry::Quit,
        ];
        assert!(same_layout(&c, &same_question));
        let next_question = vec![
            MenuEntry::Info("x".to_owned()),
            MenuEntry::Allow { ask: 2 },
            MenuEntry::Quit,
        ];
        assert!(
            !same_layout(&c, &next_question),
            "another question's Allow is another item"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ORGANS;

    fn model(link: LinkState, alias: Option<&str>) -> TrayModel {
        let modes = ORGANS
            .into_iter()
            .map(|organ| (organ, OrganMode::Off))
            .collect();
        TrayModel::new(link, alias.map(String::from), modes)
    }

    #[test]
    fn link_line_mentions_alias_only_when_set() {
        assert_eq!(model(LinkState::Off, None).link_line(), "link: off");
        assert_eq!(
            model(LinkState::Connected, Some("work-box")).link_line(),
            "link: connected via work-box"
        );
        assert_eq!(
            model(LinkState::Connecting, Some("")).link_line(),
            "link: connecting"
        );
    }

    #[test]
    fn menu_is_link_ears_hands_open_config_pause_and_quit() {
        let mut m = model(LinkState::Off, Some("work-box"));
        m.set_ears_mode(Some(EarsMode::Call));
        m.set_hands_line(Some("Hands: 1".to_owned()));
        let entries = m.menu_entries();
        // Eyes, voice and calls are not on the menu; the ears carry their mode picker.
        for gone in ["eyes", "voice", "calls"] {
            assert!(
                !entries
                    .iter()
                    .any(|e| matches!(e, MenuEntry::Info(l) if l.starts_with(gone))),
                "{gone}"
            );
        }
        assert!(entries.iter().any(|e| matches!(
            e,
            MenuEntry::EarsModes {
                current: EarsMode::Call
            }
        )));
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(l) if l == "Hands: 1"))
        );
        assert!(entries.iter().any(|e| matches!(e, MenuEntry::OpenConfig)));
        assert!(entries.first().is_some_and(
            |entry| matches!(entry, MenuEntry::Info(line) if line == "link: off via work-box")
        ));
        let info_lines = entries
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Info(_)))
            .count();
        assert_eq!(info_lines, 2, "link, hands");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, MenuEntry::EarsLine(line) if line == "ears: off"))
                .count(),
            1,
            "the ears line, its own row"
        );
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry, MenuEntry::Pause { checked: false }))
        );
        assert!(entries.iter().any(|entry| matches!(entry, MenuEntry::Quit)));
        assert!(
            entries
                .iter()
                .filter(|entry| matches!(entry, MenuEntry::Separator))
                .count()
                >= 2
        );
    }

    #[test]
    fn the_eyes_row_follows_the_ears_while_the_eyes_are_on() {
        let mut m = model(LinkState::Connected, None);
        m.set_eyes_line(Some("eyes: camera on request".to_owned()));
        let entries = m.menu_entries();
        let ears = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::EarsLine(_)));
        let eyes = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Info(l) if l == "eyes: camera on request"));
        assert!(
            ears.zip(eyes).is_some_and(|(ears, eyes)| eyes > ears),
            "{entries:?}"
        );
        m.set_eyes_line(None);
        assert!(
            !m.menu_entries()
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(l) if l.starts_with("eyes"))),
            "off in config: no eyes row"
        );
    }

    #[test]
    fn pause_toggle_flips_checked_state_and_emits_event() {
        let mut tray = model(LinkState::Off, None);
        let event = tray.toggle_pause();
        assert!(matches!(event, TrayEvent::PauseToggled { paused: true }));
        assert!(tray.is_paused());
        let entries = tray.menu_entries();
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry, MenuEntry::Pause { checked: true }))
        );
        let event = tray.toggle_pause();
        assert!(matches!(event, TrayEvent::PauseToggled { paused: false }));
        assert!(!tray.is_paused());
    }

    #[test]
    fn quit_reports_without_mutating() {
        let tray = model(LinkState::Connected, None);
        let entries_before = tray.menu_entries();
        assert!(matches!(tray.quit(), TrayEvent::Quit));
        assert_eq!(tray.menu_entries(), entries_before);
        assert_eq!(tray.link_state(), LinkState::Connected);
    }

    #[test]
    fn ask_shows_allow_and_deny_and_hands_line_is_live() {
        let mut tray = model(LinkState::Connected, None);
        tray.set_ask(Some((3, "The agent asks (t): read x".to_owned())));
        tray.set_hands_line(Some("hands: paused".to_owned()));
        let entries = tray.menu_entries();
        assert!(entries.contains(&MenuEntry::Allow { ask: 3 }));
        assert!(entries.contains(&MenuEntry::Deny { ask: 3 }));
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(l) if l == "hands: paused"))
        );
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(l) if l == "hands: off"))
        );
        tray.set_ask(None);
        assert!(
            !tray
                .menu_entries()
                .iter()
                .any(|e| matches!(e, MenuEntry::Allow { .. }))
        );
    }

    #[test]
    fn ears_line_replaces_the_configured_posture() {
        let mut tray = model(LinkState::Connected, None);
        tray.set_ears_line(Some("ears: DEAF — me: no capture file yet".to_owned()));
        let entries = tray.menu_entries();
        assert!(entries.iter().any(
            |e| matches!(e, MenuEntry::EarsLine(l) if l == "ears: DEAF — me: no capture file yet")
        ));
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, MenuEntry::EarsLine(l) if l == "ears: off"))
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(l) if l == "hands: off")),
            "the other organs keep their posture lines"
        );
    }

    #[test]
    fn device_pickers_follow_the_ears_and_list_by_name() {
        let mut tray = model(LinkState::Connected, None);
        let pickers = |tray: &TrayModel| -> Vec<MenuEntry> {
            tray.menu_entries()
                .into_iter()
                .filter(|e| matches!(e, MenuEntry::Devices { .. }))
                .collect()
        };
        assert!(pickers(&tray).is_empty(), "none before the ears say on");
        let lists = AudioDevices {
            inputs: vec![
                "Built-in Microphone".to_owned(),
                "Example Headset".to_owned(),
            ],
            outputs: vec!["Example Headset".to_owned()],
        };
        tray.set_devices(Some(&lists), Some("Example Headset".to_owned()), None, None);
        assert_eq!(
            pickers(&tray),
            vec![
                MenuEntry::Devices {
                    role: DeviceRole::Mic,
                    names: lists.inputs.clone(),
                    current: Some("Example Headset".to_owned()),
                },
                MenuEntry::Devices {
                    role: DeviceRole::Output,
                    names: lists.outputs.clone(),
                    current: None,
                },
            ]
        );
        let entries = tray.menu_entries();
        let ears_line = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::EarsLine(_)));
        let mic = entries.iter().position(|e| {
            matches!(
                e,
                MenuEntry::Devices {
                    role: DeviceRole::Mic,
                    ..
                }
            )
        });
        assert_eq!(
            mic,
            ears_line.map(|at| at.saturating_add(1)),
            "right under the ears line"
        );
        let before = tray.menu_entries();
        tray.set_devices(Some(&AudioDevices::default()), None, None, None);
        assert!(
            same_layout(&before, &tray.menu_entries()),
            "a new list is updated in place, not rebuilt"
        );
        tray.set_devices(
            Some(&AudioDevices::default()),
            None,
            None,
            Some("device events not registered"),
        );
        assert!(
            tray.menu_entries().iter().any(|e| matches!(
                e,
                MenuEntry::Info(line) if line.contains("not followed (device events not registered)")
            )),
            "device events not heard: said above the pickers"
        );
        tray.set_devices(None, None, None, Some("device events not registered"));
        assert!(pickers(&tray).is_empty(), "ears off: hidden");
        assert!(
            !tray
                .menu_entries()
                .iter()
                .any(|e| matches!(e, MenuEntry::Info(line) if line.contains("not followed"))),
            "ears off: no device line either"
        );
    }

    #[test]
    fn open_forwards_are_rows_under_the_organs_each_its_own_item() {
        use crate::forward::{Dir, View};
        let mut tray = model(LinkState::Connected, None);
        let view = |port, dir, once| View {
            port,
            dir,
            left_s: 100,
            once,
            conns: 0,
        };
        tray.set_awake_line(Some("keep-awake: on".to_owned()));
        tray.set_forwards(vec![
            view(53682, Dir::Out, true),
            view(8080, Dir::In, false),
        ]);
        let entries = tray.menu_entries();
        let rows: Vec<&MenuEntry> = entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::Forward { .. }))
            .collect();
        assert_eq!(
            rows,
            vec![
                &MenuEntry::Forward {
                    port: 53682,
                    dir: Dir::Out,
                    line: "53682 → once — click to close".to_owned()
                },
                &MenuEntry::Forward {
                    port: 8080,
                    dir: Dir::In,
                    line: "8080 ← — click to close".to_owned()
                },
            ]
        );
        let awake = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Info(l) if l == "keep-awake: on"));
        let first = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Forward { .. }));
        assert_eq!(
            first,
            awake.map(|at| at.saturating_add(1)),
            "under the organs"
        );
        let before = tray.menu_entries();
        tray.set_forwards(vec![
            view(53682, Dir::Out, true),
            view(9090, Dir::In, false),
        ]);
        assert!(
            !same_layout(&before, &tray.menu_entries()),
            "another forward is another item: rebuilt, never a stale close"
        );
        tray.set_forwards(Vec::new());
        assert!(
            !tray
                .menu_entries()
                .iter()
                .any(|e| matches!(e, MenuEntry::Forward { .. }))
        );
    }

    #[test]
    fn served_links_are_rows_beside_the_forwards_each_its_own_item() {
        use crate::linkserve::{Proto, Side, View};
        let mut tray = model(LinkState::Connected, None);
        let view = |link: &str, side| View {
            link: link.to_owned(),
            proto: Proto::Udp,
            port: 24642,
            side,
            left_s: 100,
            peers: (1, 2),
        };
        tray.set_links(vec![view("l-0123abcd", Side::Client)]);
        let entries = tray.menu_entries();
        let rows: Vec<&MenuEntry> = entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::LinkServe { .. }))
            .collect();
        assert_eq!(
            rows,
            vec![&MenuEntry::LinkServe {
                link: "l-0123abcd".to_owned(),
                line: "l-0123abcd udp 24642 client — click to close".to_owned()
            }]
        );
        let before = tray.menu_entries();
        tray.set_links(vec![view("l-0123abce", Side::Host)]);
        assert!(
            !same_layout(&before, &tray.menu_entries()),
            "another serve is another item: rebuilt, never a stale close"
        );
        tray.set_links(Vec::new());
        assert!(
            !tray
                .menu_entries()
                .iter()
                .any(|e| matches!(e, MenuEntry::LinkServe { .. }))
        );
    }

    #[test]
    fn link_state_updates_flow_to_menu() {
        let mut tray = model(LinkState::Off, Some("work-box"));
        tray.set_link_state(LinkState::Connecting);
        assert!(tray.menu_entries().iter().any(|entry| {
            matches!(entry, MenuEntry::Info(line) if line == "link: connecting via work-box")
        }));
    }
}
