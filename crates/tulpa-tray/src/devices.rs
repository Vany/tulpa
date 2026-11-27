//! The Mac's audio devices by name, for the menu's two device pickers: the
//! microphone the ears' capture records («Ears mic») and the output the voice
//! plays on («Voice output»).
//!
//! The lists come from `system_profiler SPAudioDataType -json` ([`parse`]).
//! [`DeviceCache`] reads them in the background — at start, and again on every
//! `CoreAudio` device event (`tulpa-audio-events`, the same listener the ears'
//! mic follows: a device added or removed, a new default) — so the menu never
//! waits for it and its devices are current. A read that failed is tried again when the
//! menu is about to open (the pointer reaches or clicks the menu-bar item).
//! A device renamed in Audio MIDI Setup is such an event too (its name is
//! listened to), so it shows under its new name at once.
//! A device is always named, never numbered: an index moves whenever a device
//! comes or goes.
//!
//! A pick ([`pick`]) applies at once ([`Ears::set_mic`], [`Ears::set_output`])
//! and is kept in the config file as `[ears] mic_device` / `[voice]
//! output_device` (one line changed, [`crate::config::edit`]). A microphone
//! whose name the capture's ffmpeg would misread ([`check_mic_name`]) is
//! greyed out and never picked: [`MIC_SCRIPT_ONLY`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tulpa_hands::Journal;
use tulpa_hands::exec::{self, Ended};
use tulpa_stop::Stop;

use crate::ears::Ears;

/// Lists the audio devices; run directly, never through a shell.
pub const SYSTEM_PROFILER: &str = "/usr/sbin/system_profiler";
/// A `system_profiler` run is killed past this.
const READ_LIMIT: Duration = Duration::from_secs(15);
/// The most of its output kept.
const READ_MAX: usize = 1 << 20;
/// The longest device name taken (from the config or the list).
pub const NAME_MAX: usize = 200;
/// Every picker item's id starts so: then the role, then `:` and the name (none: the default).
const ITEM_PREFIX: &str = "tulpa.device.";
/// The default's row, first in each picker.
const DEFAULT_LABEL: &str = "Default";
/// Why a microphone is greyed out in its picker and refused as a pick ([`check_mic_name`]).
pub const MIC_SCRIPT_ONLY: &str = "use the capture script for this one";

/// What a device picker chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceRole {
    /// The microphone the ears' capture records (`[ears] mic_device`).
    Mic,
    /// The output `voice.say` plays on (`[voice] output_device`).
    Output,
}

impl DeviceRole {
    /// Both roles, in menu order.
    pub const ALL: [Self; 2] = [Self::Mic, Self::Output];

    /// The stable id in menu item ids.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::Output => "output",
        }
    }

    /// The picker's title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Mic => "Ears mic",
            Self::Output => "Voice output",
        }
    }

    /// The config table and key the pick is kept under.
    #[must_use]
    pub const fn config_key(self) -> (&'static str, &'static str) {
        match self {
            Self::Mic => ("ears", "mic_device"),
            Self::Output => ("voice", "output_device"),
        }
    }
}

/// The Mac's audio devices by name, each once, in `system_profiler`'s order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioDevices {
    /// Devices with input channels.
    pub inputs: Vec<String>,
    /// Devices with output channels.
    pub outputs: Vec<String>,
}

impl AudioDevices {
    /// The devices a `role` picker lists.
    #[must_use]
    pub fn of(&self, role: DeviceRole) -> &[String] {
        match role {
            DeviceRole::Mic => &self.inputs,
            DeviceRole::Output => &self.outputs,
        }
    }
}

/// The devices in `system_profiler SPAudioDataType -json` output.
///
/// Each device object (an `_items` entry, at any depth, with a `_name`) that
/// has input channels (`coreaudio_device_input`) is an input, one with output
/// channels (`coreaudio_device_output`) an output; a headset is both. Without a
/// channel count, a `coreaudio_input_source` / `coreaudio_output_source` says
/// the same. A name [`check_name`] refuses is left out.
///
/// # Errors
///
/// Text that is not JSON or has no `SPAudioDataType` list.
pub fn parse(json: &str) -> Result<AudioDevices, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("not JSON: {e}"))?;
    let tops = root
        .get("SPAudioDataType")
        .and_then(Value::as_array)
        .ok_or_else(|| "no SPAudioDataType list".to_owned())?;
    let mut devices = AudioDevices::default();
    for top in tops {
        walk(top, &mut devices, 0);
    }
    Ok(devices)
}

/// How deep `_items` nest before the walk stops looking.
const DEPTH_MAX: usize = 8;

fn walk(value: &Value, devices: &mut AudioDevices, depth: usize) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(name) = object.get("_name").and_then(Value::as_str)
        && check_name(name).is_ok()
    {
        if has_side(object, "input") && !devices.inputs.iter().any(|n| n == name) {
            devices.inputs.push(name.to_owned());
        }
        if has_side(object, "output") && !devices.outputs.iter().any(|n| n == name) {
            devices.outputs.push(name.to_owned());
        }
    }
    if depth < DEPTH_MAX
        && let Some(items) = object.get("_items").and_then(Value::as_array)
    {
        for item in items {
            walk(item, devices, depth.saturating_add(1));
        }
    }
}

/// Whether the device has an `input` or `output` side: its channel count, or
/// (no count given) a source of that side.
fn has_side(object: &serde_json::Map<String, Value>, side: &str) -> bool {
    object.get(&format!("coreaudio_device_{side}")).map_or_else(
        || object.contains_key(&format!("coreaudio_{side}_source")),
        |count| has_channels(Some(count)),
    )
}

/// A channel count above 0 (a number, or a string holding one).
fn has_channels(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(n)) => {
            n.as_u64().is_some_and(|n| n > 0) || n.as_f64().is_some_and(|n| n > 0.0)
        }
        Some(Value::String(s)) => s.trim().parse::<u64>().is_ok_and(|n| n > 0),
        _ => false,
    }
}

/// A device name fit to pass on: not empty, at most [`NAME_MAX`] characters, no
/// control characters (it goes into argv and the environment as one value).
///
/// # Errors
///
/// The reason the name is refused.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        Err("an empty device name".to_owned())
    } else if name.chars().count() > NAME_MAX {
        Err(format!("a device name over {NAME_MAX} characters"))
    } else if name.chars().any(char::is_control) {
        Err(format!("device name {name:?} has control characters"))
    } else {
        Ok(())
    }
}

/// A name the capture can be given as its microphone, `EARS_MIC=":<name>"`.
///
/// ffmpeg's avfoundation input cuts that name at a `:`, and reads one that starts with a digit (after
/// blanks and a `+`, as `%d` does) as a device index: either way it could record another input than the one
/// picked, and nothing would say so.
///
/// # Errors
///
/// Such a name: [`MIC_SCRIPT_ONLY`].
pub fn check_mic_name(name: &str) -> Result<(), String> {
    let lead = name.trim_start();
    // A sign before a digit is read as a number too (sscanf "%d" takes '+' and '-'; review A2).
    let lead = lead.strip_prefix(['+', '-']).unwrap_or(lead);
    if name.contains(':') || lead.starts_with(|c: char| c.is_ascii_digit()) {
        Err(MIC_SCRIPT_ONLY.to_owned())
    } else {
        Ok(())
    }
}

/// One row of a device picker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PickerRow {
    /// The device this row picks; `None` is the default.
    pub name: Option<String>,
    /// The row's text for the menu (an `&` doubled: the menu reads a lone one as a mnemonic mark).
    pub label: String,
    /// Whether it is the current pick.
    pub checked: bool,
    /// Whether it can be picked: not a microphone [`check_mic_name`] refuses (shown greyed out).
    pub enabled: bool,
}

/// A `role` picker's rows: the default first, then each device by name.
///
/// A picked device that is not connected now stays listed (checked, marked
/// so). A microphone [`check_mic_name`] refuses is greyed out, marked
/// [`MIC_SCRIPT_ONLY`].
#[must_use]
pub fn picker_rows(role: DeviceRole, names: &[String], current: Option<&str>) -> Vec<PickerRow> {
    let row = |name: &str, connected: bool| {
        let enabled = role != DeviceRole::Mic || check_mic_name(name).is_ok();
        let notes: Vec<&str> = [
            (!connected).then_some("not connected"),
            (!enabled).then_some(MIC_SCRIPT_ONLY),
        ]
        .into_iter()
        .flatten()
        .collect();
        PickerRow {
            name: Some(name.to_owned()),
            label: if notes.is_empty() {
                menu_text(name)
            } else {
                format!("{} ({})", menu_text(name), notes.join("; "))
            },
            checked: current == Some(name),
            enabled,
        }
    };
    let mut rows = vec![PickerRow {
        name: None,
        label: DEFAULT_LABEL.to_owned(),
        checked: current.is_none(),
        enabled: true,
    }];
    rows.extend(names.iter().map(|name| row(name, true)));
    if let Some(current) = current
        && !names.iter().any(|n| n == current)
    {
        rows.push(row(current, false));
    }
    rows
}

/// The picker's title: its role and the current pick.
#[must_use]
pub fn picker_title(role: DeviceRole, current: Option<&str>) -> String {
    format!(
        "{}: {}",
        role.title(),
        current.map_or_else(|| DEFAULT_LABEL.to_lowercase(), menu_text)
    )
}

/// `text` shown as is in a menu: each `&` doubled.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// The menu item id of a picker row (`name` `None`: the default).
#[must_use]
pub fn item_id(role: DeviceRole, name: Option<&str>) -> String {
    name.map_or_else(
        || format!("{ITEM_PREFIX}{}", role.id()),
        |name| format!("{ITEM_PREFIX}{}:{name}", role.id()),
    )
}

/// The role and device a picker item id selects (`None` inside: the default);
/// `None` when the id is no picker item.
#[must_use]
pub fn pick_of(id: &str) -> Option<(DeviceRole, Option<String>)> {
    let rest = id.strip_prefix(ITEM_PREFIX)?;
    let (role, name) = rest
        .split_once(':')
        .map_or((rest, None), |(role, name)| (role, Some(name.to_owned())));
    DeviceRole::ALL
        .into_iter()
        .find(|r| r.id() == role)
        .map(|role| (role, name))
}

/// Applies a pick from the menu to the ears and keeps it in the config.
///
/// `name` `None` is the default. The pick goes to the ears first, then into the
/// config file at `config_path`; the activity-log line comes back. A pick that
/// applied but could not be kept says so: a restart reverts it.
///
/// # Errors
///
/// A microphone [`check_mic_name`] refuses (neither applied nor written), the
/// ears refuse it (off in config, an unusable name), or the capture did not
/// come back after the restart a new microphone needs; the line says which.
pub fn pick(
    ears: &Arc<Ears>,
    config_path: &Path,
    role: DeviceRole,
    name: Option<&str>,
) -> Result<String, String> {
    let shown = name.map_or_else(|| DEFAULT_LABEL.to_lowercase(), str::to_owned);
    if role == DeviceRole::Mic
        && let Some(name) = name
    {
        check_mic_name(name).map_err(|why| format!("{}: {shown} refused ({why})", role.title()))?;
    }
    let applied = match role {
        DeviceRole::Mic => ears.set_mic(name.map(str::to_owned)),
        DeviceRole::Output => ears.set_output(name.map(str::to_owned)),
    }
    .map_err(|why| format!("{}: {shown} refused ({why})", role.title()))?;
    let (table, key) = role.config_key();
    let kept = match crate::config::edit::set_string_in_file(
        config_path,
        table,
        key,
        name.unwrap_or_default(),
    ) {
        Ok(()) => format!("kept as [{table}] {key}"),
        Err(why) => format!("NOT kept in the config ({why}); a restart reverts it"),
    };
    Ok(format!(
        "{}: {shown} from the menu: {applied}; {kept}",
        role.title()
    ))
}

/// Registers for the Mac's device events: each event runs the given function, until the returned guard is dropped.
/// `Err` says why the events cannot be had (said in the log and the menu).
pub type Register =
    Box<dyn Fn(Box<dyn Fn() + Send + Sync>) -> Result<Box<dyn Send>, String> + Send + Sync>;

/// Whether the device events are heard.
#[derive(Default)]
struct Listened {
    /// The registration, while it holds.
    held: Option<Box<dyn Send>>,
    /// Why the last registration was refused (already said); `None` once one holds.
    refused: Option<String>,
}

/// The Mac's device lists, read in the background and again on each device change.
pub struct DeviceCache {
    program: PathBuf,
    home: PathBuf,
    /// The tray's Pause: it kills a read in flight (and Quit sets it), so a shutdown's wait for children is short.
    paused: Arc<Stop>,
    journal: Arc<dyn Journal>,
    changed: Box<dyn Fn() + Send + Sync>,
    state: Mutex<Cached>,
    /// A reading thread runs.
    reading: AtomicBool,
    /// A change came in since the running read began: read again.
    dirty: AtomicBool,
    /// How the device events are registered ([`DeviceCache::listening_with`]); `None`: they are not.
    register: Option<Register>,
    listened: Mutex<Listened>,
    /// How long one read may take ([`READ_LIMIT`]).
    read_limit: Duration,
}

#[derive(Debug, Default)]
struct Cached {
    devices: AudioDevices,
    /// Whether the last read failed (a failure is logged once, not on every retry).
    failing: bool,
}

impl fmt::Debug for DeviceCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceCache")
            .field("program", &self.program)
            .field("devices", &self.devices())
            .finish_non_exhaustive()
    }
}

/// The read thread's hold on the cache. A panic in a read or in `changed` unwinds through its drop, which clears
/// `reading` and marks the lists to be read again: otherwise `reading` stays set with no thread to clear it, and
/// nothing reads the lists for the life of the process. A thread that returns has cleared `reading` itself (or handed
/// it to the read after it), so the drop does nothing then.
struct ReadingGuard(Arc<DeviceCache>);

impl std::ops::Deref for ReadingGuard {
    type Target = DeviceCache;

    fn deref(&self) -> &DeviceCache {
        &self.0
    }
}

impl Drop for ReadingGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.reading.store(false, Ordering::SeqCst);
            self.0.read_again();
            self.0
                .journal
                .record("devices: reading the lists panicked; read again when the menu opens");
        }
    }
}

impl DeviceCache {
    /// A cache that runs `program SPAudioDataType -json` (with the cleared
    /// environment and `home`); `changed` runs, from the reading thread, when
    /// a read finds lists other than before. Failures go to `journal`. A read
    /// in flight is killed when `paused` is set, and none starts while it is.
    #[must_use]
    pub fn new(
        program: PathBuf,
        home: PathBuf,
        paused: Arc<Stop>,
        journal: Arc<dyn Journal>,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            program,
            home,
            paused,
            journal,
            changed,
            state: Mutex::new(Cached::default()),
            reading: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            register: None,
            listened: Mutex::new(Listened::default()),
            read_limit: READ_LIMIT,
        }
    }

    /// Registers for device events with `register` ([`DeviceCache::listen`]).
    #[must_use]
    pub fn listening_with(mut self, register: Register) -> Self {
        self.register = Some(register);
        self
    }

    /// Registers for device events, if not yet: each one reads the lists again ([`DeviceCache::invalidate`]). A
    /// refusal is said once in the log and stays the pickers' error line ([`DeviceCache::unheard`]) until a later
    /// call succeeds: [`DeviceCache::retry_failed`] calls this, on each menu open. No clock.
    pub fn listen(self: &Arc<Self>) {
        let Some(register) = &self.register else {
            return;
        };
        let mut listened = self.listened();
        if listened.held.is_some() {
            return;
        }
        // Weak: the registration lives in the cache, and must not keep it alive.
        let cache = Arc::downgrade(self);
        match register(Box::new(move || {
            if let Some(cache) = cache.upgrade() {
                cache.invalidate();
            }
        })) {
            Ok(held) => {
                listened.held = Some(held);
                if listened.refused.take().is_some() {
                    self.journal
                        .record("devices: the device events are heard again");
                }
                drop(listened);
                // What changed while they were not heard.
                self.invalidate();
            }
            Err(why) => {
                if listened.refused.is_none() {
                    self.journal.record(&format!(
                        "devices: {why}: the pickers keep the last list; tried again when the menu opens"
                    ));
                }
                listened.refused = Some(why);
            }
        }
    }

    /// Why the device events are not heard, while they are not.
    #[must_use]
    pub fn unheard(&self) -> Option<String> {
        self.listened().refused.clone()
    }

    /// The registration's state. A registration that panicked under this lock poisoned it; what it guards is whole
    /// (a registration is stored only once made), so the poison is cleared and said once, and the state read on.
    fn listened(&self) -> std::sync::MutexGuard<'_, Listened> {
        self.listened.lock().unwrap_or_else(|poisoned| {
            self.listened.clear_poison();
            self.journal
                .record("devices: a device-event registration panicked; registering again");
            poisoned.into_inner()
        })
    }

    /// The lists last read (empty before the first read worked).
    #[must_use]
    pub fn devices(&self) -> AudioDevices {
        self.state
            .lock()
            .map(|s| s.devices.clone())
            .unwrap_or_default()
    }

    /// The lists may have changed (a device event): reads them again on a thread
    /// of its own. A burst is one or two reads: a change during a read makes
    /// that thread read once more, never two threads at once. Never blocks, so
    /// `CoreAudio`'s notification thread may call it.
    pub fn invalidate(self: &Arc<Self>) {
        self.dirty.store(true, Ordering::SeqCst);
        if self.reading.swap(true, Ordering::SeqCst) {
            return;
        }
        let cache = Arc::clone(self);
        // Spawned with the error returned, not a panic: this runs on CoreAudio's thread, which swallows a panic, and
        // `reading` must not stay set with no thread to clear it.
        let spawned = std::thread::Builder::new()
            .name("tulpa-devices-read".to_owned())
            .spawn(move || {
                let cache = ReadingGuard(cache);
                loop {
                    cache.dirty.store(false, Ordering::SeqCst);
                    if cache.read_now() {
                        (cache.changed)();
                    }
                    cache.reading.store(false, Ordering::SeqCst);
                    if !cache.dirty.load(Ordering::SeqCst)
                        || cache.reading.swap(true, Ordering::SeqCst)
                    {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            self.reading.store(false, Ordering::SeqCst);
            self.journal.record(&format!(
                "devices: no thread to read the lists ({e}); the next device event or menu open tries again"
            ));
            self.read_again();
        }
    }

    /// Marks the lists to be read again, as a failed read does: the next menu open reads them
    /// ([`DeviceCache::retry_failed`]).
    fn read_again(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failing = true;
    }

    /// Reads again if the last read failed, and registers for device events if
    /// that was refused ([`DeviceCache::listen`]): no event says when either
    /// would work; the menu about to open does.
    pub fn retry_failed(self: &Arc<Self>) {
        self.listen();
        if self.state.lock().is_ok_and(|s| s.failing) {
            self.invalidate();
        }
    }

    /// One read, here and now; whether the lists changed. A failed read keeps
    /// the lists as they were.
    pub fn read_now(&self) -> bool {
        let read = self.run();
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        match read {
            Ok(devices) => {
                if state.failing {
                    self.journal
                        .record("devices: the audio device list reads again");
                }
                state.failing = false;
                let changed = devices != state.devices;
                state.devices = devices;
                changed
            }
            Err(why) => {
                if !state.failing {
                    self.journal.record(&format!(
                        "devices: {} failed ({why}); the pickers keep the last list",
                        self.program.display()
                    ));
                }
                state.failing = true;
                false
            }
        }
    }

    fn run(&self) -> Result<AudioDevices, String> {
        let mut command = exec::cleared(&self.program, &self.home);
        command.args(["SPAudioDataType", "-json"]);
        let outcome = exec::run(
            command,
            &self.paused,
            &Stop::new(),
            self.read_limit,
            READ_MAX,
        )
        .map_err(|e| e.to_string())?;
        if outcome.ended != Ended::Exit(Some(0)) {
            return Err(format!("{:?}", outcome.ended));
        }
        if outcome.cut {
            return Err(format!("output over {READ_MAX} bytes"));
        }
        parse(&String::from_utf8_lossy(&outcome.stdout))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use super::*;

    /// The shape `system_profiler SPAudioDataType -json` prints, names made up.
    const RECORDED: &str = r#"{
  "SPAudioDataType" : [
    {
      "_items" : [
        {
          "_name" : "Example Headset",
          "coreaudio_device_input" : 1,
          "coreaudio_device_manufacturer" : "Example Inc.",
          "coreaudio_device_output" : 2,
          "coreaudio_device_srate" : 48000,
          "coreaudio_device_transport" : "coreaudio_device_type_usb",
          "coreaudio_input_source" : "spaudio_default",
          "coreaudio_output_source" : "spaudio_default"
        },
        {
          "_name" : "Built-in Microphone",
          "coreaudio_default_audio_input_device" : "spaudio_yes",
          "coreaudio_device_input" : 1,
          "coreaudio_device_manufacturer" : "Example Inc.",
          "coreaudio_device_srate" : 48000,
          "coreaudio_device_transport" : "coreaudio_device_type_builtin",
          "coreaudio_input_source" : "Built-in Microphone"
        },
        {
          "_name" : "Built-in Output",
          "coreaudio_default_audio_output_device" : "spaudio_yes",
          "coreaudio_default_audio_system_device" : "spaudio_yes",
          "coreaudio_device_manufacturer" : "Example Inc.",
          "coreaudio_device_output" : 2,
          "coreaudio_device_srate" : 48000,
          "coreaudio_device_transport" : "coreaudio_device_type_builtin",
          "coreaudio_output_source" : "Built-in Output"
        },
        {
          "_name" : "Desk Speakers & Line",
          "coreaudio_device_output" : "2",
          "coreaudio_device_transport" : "coreaudio_device_type_hdmi"
        },
        {
          "_name" : "Silent Aggregate",
          "coreaudio_device_input" : 0,
          "coreaudio_device_output" : 0
        },
        {
          "_name" : "Built-in Microphone",
          "coreaudio_device_input" : 1
        }
      ],
      "_name" : "coreaudio_device"
    }
  ]
}"#;

    #[test]
    fn a_recorded_list_gives_inputs_and_outputs_by_name() -> Result<(), String> {
        let devices = parse(RECORDED)?;
        assert_eq!(
            devices.inputs,
            vec!["Example Headset", "Built-in Microphone"],
            "inputs in order, each once; a device with 0 channels is none"
        );
        assert_eq!(
            devices.outputs,
            vec!["Example Headset", "Built-in Output", "Desk Speakers & Line"],
            "a headset is both; a count given as a string counts"
        );
        assert_eq!(devices.of(DeviceRole::Mic), devices.inputs.as_slice());
        assert_eq!(parse(r#"{"SPAudioDataType":[]}"#)?, AudioDevices::default());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"SPDisplaysDataType":[]}"#).is_err());
        let nested = r#"{"SPAudioDataType":[{"_name":"x","_items":[{"_name":"group","_items":[{"_name":"Deep Mic","coreaudio_device_input":2}]}]}]}"#;
        assert_eq!(parse(nested)?.inputs, vec!["Deep Mic"], "any depth");
        let sources = r#"{"SPAudioDataType":[{"_items":[{"_name":"Old Mic","coreaudio_input_source":"spaudio_default"},{"_name":"Old Out","coreaudio_output_source":"spaudio_default","coreaudio_device_input":0}]}]}"#;
        let old = parse(sources)?;
        assert_eq!(
            (old.inputs, old.outputs),
            (vec!["Old Mic".to_owned()], vec!["Old Out".to_owned()]),
            "no count: the source says; a count of 0 still wins"
        );
        let bad = r#"{"SPAudioDataType":[{"_items":[{"_name":"Bad\nName","coreaudio_device_input":1},{"_name":"","coreaudio_device_output":1}]}]}"#;
        assert_eq!(
            parse(bad)?,
            AudioDevices::default(),
            "unusable names left out"
        );
        Ok(())
    }

    #[test]
    fn rows_list_the_default_then_each_device_and_keep_a_missing_pick() {
        let names = vec!["Built-in Microphone".to_owned(), "Mic & Line".to_owned()];
        let rows = picker_rows(DeviceRole::Mic, &names, None);
        let shown: Vec<(&str, bool)> = rows.iter().map(|r| (r.label.as_str(), r.checked)).collect();
        assert_eq!(
            shown,
            vec![
                ("Default", true),
                ("Built-in Microphone", false),
                ("Mic && Line", false)
            ],
            "an & doubled for the menu"
        );
        assert_eq!(
            rows.get(2).and_then(|r| r.name.as_deref()),
            Some("Mic & Line")
        );
        let rows = picker_rows(DeviceRole::Mic, &names, Some("Built-in Microphone"));
        assert_eq!(
            rows.iter()
                .filter(|r| r.checked)
                .map(|r| r.name.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Built-in Microphone")]
        );
        let rows = picker_rows(DeviceRole::Mic, &names, Some("Example USB Mic"));
        assert_eq!(
            rows.last().map(|r| (r.label.as_str(), r.checked)),
            Some(("Example USB Mic (not connected)", true))
        );
        assert_eq!(picker_title(DeviceRole::Mic, None), "Ears mic: default");
        assert_eq!(
            picker_title(DeviceRole::Output, Some("A & B")),
            "Voice output: A && B"
        );
    }

    /// A microphone the capture's ffmpeg would misread is greyed out and marked; an output picker lists
    /// such a name as usual.
    #[test]
    fn a_mic_named_like_an_index_or_with_a_colon_is_greyed_out() {
        let names: Vec<String> = ["Built-in Microphone", "2nd Mic", "Desk: USB"]
            .map(str::to_owned)
            .into();
        let rows = picker_rows(DeviceRole::Mic, &names, None);
        let shown: Vec<(&str, bool)> = rows.iter().map(|r| (r.label.as_str(), r.enabled)).collect();
        assert_eq!(
            shown,
            vec![
                ("Default", true),
                ("Built-in Microphone", true),
                ("2nd Mic (use the capture script for this one)", false),
                ("Desk: USB (use the capture script for this one)", false),
            ]
        );
        assert_eq!(
            rows.iter().filter(|r| !r.enabled).count(),
            2,
            "two disabled rows"
        );
        assert!(
            picker_rows(DeviceRole::Output, &names, None)
                .iter()
                .all(|r| r.enabled),
            "an output is not ffmpeg's input"
        );
        assert_eq!(
            picker_rows(
                DeviceRole::Mic,
                names.get(..1).unwrap_or_default(),
                Some("2nd Mic")
            )
            .last()
            .map(|r| (r.label.as_str(), r.checked, r.enabled)),
            Some((
                "2nd Mic (not connected; use the capture script for this one)",
                true,
                false
            )),
            "a misread pick from before stays shown, greyed out"
        );
        for (name, usable) in [
            ("2nd Mic", false),
            ("Desk: USB", false),
            ("USB:", false),
            (" 3 Mic", false),
            ("+1 Mic", false),
            ("-1 Mic", false),
            ("Mic 2", true),
            ("Built-in Microphone", true),
        ] {
            assert_eq!(check_mic_name(name).is_ok(), usable, "{name:?}");
        }
    }

    /// Picking such a microphone from the menu is refused: the ears and the config stay as they were.
    #[test]
    fn picking_a_misread_mic_leaves_the_config_unchanged() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-devices-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir)?;
        let config_path = dir.join("config.toml");
        let text = "[ears]\nmode = \"on\"\ncommand = [\"/bin/sh\", \"-c\", \"sleep 30\"]\nme_dir = \"/tmp/example-me\"\nthem_dir = \"/tmp/example-them\"\nmic_device = \"Built-in Microphone\"\n";
        std::fs::write(&config_path, text)?;
        let config = crate::config::TrayAppConfig::from_toml(text)?;
        let journal: Arc<dyn Journal> = Arc::new(Notes::default());
        let ears = Arc::new(Ears::build(
            &config,
            Arc::new(tulpa_stop::Stop::new()),
            Arc::new(crate::link::Outbox::default()),
            journal,
            Box::new(|| {}),
        ));
        for name in ["2nd Mic", "Desk: USB"] {
            assert_eq!(
                pick(&ears, &config_path, DeviceRole::Mic, Some(name)),
                Err(format!(
                    "Ears mic: {name} refused (use the capture script for this one)"
                ))
            );
            assert_eq!(
                std::fs::read_to_string(&config_path)?,
                text,
                "{name}: the config is unchanged"
            );
            assert_eq!(
                ears.mic().as_deref(),
                Some("Built-in Microphone"),
                "{name}: the ears' pick too"
            );
        }
        let said = pick(
            &ears,
            &config_path,
            DeviceRole::Mic,
            Some("Example USB Mic"),
        )?;
        assert!(said.ends_with("kept as [ears] mic_device"), "{said}");
        assert!(
            std::fs::read_to_string(&config_path)?.contains("mic_device = \"Example USB Mic\"")
        );
        let said = pick(&ears, &config_path, DeviceRole::Output, Some("Desk: USB"))?;
        assert!(said.ends_with("kept as [voice] output_device"), "{said}");
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The last device picked, then unplugged: the same rows by name, another text. An in-place update
    /// that only compared names would keep the old text.
    #[test]
    fn a_pick_that_comes_or_goes_keeps_its_row_and_changes_its_text() {
        let all = vec![
            "Built-in Microphone".to_owned(),
            "Example USB Mic".to_owned(),
        ];
        let rest = vec!["Built-in Microphone".to_owned()];
        let plugged = picker_rows(DeviceRole::Mic, &all, Some("Example USB Mic"));
        let unplugged = picker_rows(DeviceRole::Mic, &rest, Some("Example USB Mic"));
        let names = |rows: &[PickerRow]| rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&plugged), names(&unplugged));
        assert_eq!(
            plugged.last().map(|r| r.label.as_str()),
            Some("Example USB Mic")
        );
        assert_eq!(
            unplugged.last().map(|r| r.label.as_str()),
            Some("Example USB Mic (not connected)")
        );
    }

    #[test]
    fn item_ids_carry_the_role_and_the_name_back() {
        for role in DeviceRole::ALL {
            for name in [
                None,
                Some("Built-in Output"),
                Some("odd: name: with colons"),
            ] {
                assert_eq!(
                    pick_of(&item_id(role, name)),
                    Some((role, name.map(str::to_owned)))
                );
            }
        }
        assert_eq!(pick_of("tulpa.device.camera:x"), None);
        assert_eq!(pick_of("tulpa.ears-mode.alone"), None);
    }

    #[test]
    fn names_with_control_characters_or_too_long_are_refused() {
        assert!(check_name("Built-in Microphone").is_ok());
        assert!(
            check_name("-starts with a dash").is_ok(),
            "argv takes it as one value"
        );
        assert!(check_name("").is_err());
        assert!(check_name("tab\there").is_err());
        assert!(check_name(&"m".repeat(NAME_MAX.saturating_add(1))).is_err());
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

    /// Runs of the fake lister so far.
    fn runs_in(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |t| t.lines().count())
    }

    /// Waits until `done` holds: 60 s at most, a wedge guard far above any wait here. A test's own state (a file,
    /// a flag of the cache) has no event to wait on, so this looks every 5 ms.
    fn until(done: impl Fn() -> bool) -> bool {
        let waited = Instant::now();
        while !done() && waited.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    /// Each device event reads the lists again in the background, a burst at most twice and never two reads at
    /// once; a changed list is told; a failed read is tried again on [`DeviceCache::retry_failed`] only.
    #[test]
    fn the_cache_reads_again_on_each_device_event() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-devices-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let json = dir.join("audio.json");
        std::fs::write(&json, RECORDED)?;
        let runs = dir.join("runs");
        let lister = dir.join("lister.sh");
        crate::testutil::write_script(
            &lister,
            &format!(
                "#!/bin/sh\n[ \"$1 $2\" = 'SPAudioDataType -json' ] || exit 3\necho run >> {}\ncat {}\n",
                runs.display(),
                json.display()
            ),
        )?;
        let told = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&told);
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let cache = Arc::new(DeviceCache::new(
            lister,
            dir.clone(),
            Arc::new(Stop::new()),
            journal,
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        ));
        assert_eq!(cache.devices(), AudioDevices::default());
        for _ in 0..5 {
            cache.invalidate();
        }
        assert!(until(
            || !cache.reading.load(Ordering::SeqCst) && told.load(Ordering::SeqCst) == 1
        ));
        let burst = runs_in(&runs);
        assert!(
            (1..=2).contains(&burst),
            "a burst is one or two reads: {burst}"
        );
        assert_eq!(told.load(Ordering::SeqCst), 1, "a new list is told once");
        assert_eq!(
            cache.devices().inputs,
            vec!["Example Headset", "Built-in Microphone"]
        );
        cache.retry_failed();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(runs_in(&runs), burst, "nothing failed: no read");
        std::fs::write(
            &json,
            r#"{"SPAudioDataType":[{"_items":[{"_name":"New Mic","coreaudio_device_input":1}]}]}"#,
        )?;
        cache.invalidate();
        assert!(
            until(|| told.load(Ordering::SeqCst) == 2),
            "the next event reads the new list"
        );
        assert_eq!(cache.devices().inputs, vec!["New Mic"]);
        std::fs::write(&json, "garbage")?;
        cache.invalidate();
        assert!(until(
            || runs_in(&runs) == burst + 2 && !cache.reading.load(Ordering::SeqCst)
        ));
        std::fs::write(&json, RECORDED)?;
        cache.retry_failed();
        assert!(
            until(|| told.load(Ordering::SeqCst) == 3),
            "a failed read is tried again on the touch"
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A `changed` that panics kills its read thread; the next device event still reads the lists (its `changed`
    /// runs), and the panic is logged.
    #[test]
    #[expect(clippy::panic, reason = "the test is about a read thread that panics")]
    fn a_read_thread_that_panicked_does_not_stop_later_reads()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-devices-panic-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let json = dir.join("audio.json");
        std::fs::write(&json, RECORDED)?;
        let lister = dir.join("lister.sh");
        crate::testutil::write_script(&lister, &format!("#!/bin/sh\ncat {}\n", json.display()))?;
        let told = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&told);
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let cache = Arc::new(DeviceCache::new(
            lister,
            dir.clone(),
            Arc::new(Stop::new()),
            journal,
            Box::new(move || {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::panic::panic_any("changed panicked (fake)");
                }
            }),
        ));
        cache.invalidate();
        let ended =
            until(|| told.load(Ordering::SeqCst) == 1 && !cache.reading.load(Ordering::SeqCst));
        std::fs::write(
            &json,
            r#"{"SPAudioDataType":[{"_items":[{"_name":"New Mic","coreaudio_device_input":1}]}]}"#,
        )?;
        cache.invalidate();
        let read = until(|| told.load(Ordering::SeqCst) == 2);
        std::fs::remove_dir_all(dir)?;
        assert!(ended, "the panicked thread left `reading` set");
        assert!(read, "the next event did not read the lists");
        assert_eq!(cache.devices().inputs, vec!["New Mic"]);
        let said = notes
            .0
            .lock()
            .map(|l| l.iter().filter(|l| l.contains("panicked")).count())
            .unwrap_or_default();
        assert_eq!(said, 1);
        Ok(())
    }

    /// Quit (it sets Pause) while a read is in flight: Pause wakes the read, which is killed and reaped, and the
    /// lister is gone. The lister and the read's limit both run an hour: only the pause ends the read before the
    /// 60 s guard (a pause seen at the limit would end it too).
    #[test]
    fn quit_kills_a_read_in_flight() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-devices-quit-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let pid = dir.join("pid");
        let lister = dir.join("lister.sh");
        crate::testutil::write_script(
            &lister,
            &format!(
                "#!/bin/sh
echo $$ > {}
exec sleep 3600
",
                pid.display()
            ),
        )?;
        let paused = Arc::new(Stop::new());
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let mut cache = DeviceCache::new(
            lister,
            dir.clone(),
            Arc::clone(&paused),
            journal,
            Box::new(|| {}),
        );
        cache.read_limit = Duration::from_secs(3600);
        let cache = Arc::new(cache);
        cache.invalidate();
        assert!(until(
            || std::fs::read_to_string(&pid).is_ok_and(|p| p.ends_with('\n'))
        ));
        let child = std::fs::read_to_string(&pid)?.trim().parse::<i32>()?;
        paused.set();
        // The read's own end, not `exec::wait_reaped`: that counts every child in this test process, other tests'
        // too. The read is uncounted before it ends (its group killed and reaped).
        let reaped = until(|| !cache.reading.load(Ordering::SeqCst));
        let gone = rustix::process::Pid::from_raw(child)
            .is_none_or(|p| rustix::process::test_kill_process(p).is_err());
        std::fs::remove_dir_all(dir)?;
        assert!(reaped, "the read outlived the pause");
        assert!(gone, "the lister outlived the quit");
        Ok(())
    }

    /// Device events that cannot be registered are said once and shown, and registered on the next menu open
    /// (`retry_failed`); once they are, an event reads the lists again.
    #[test]
    fn refused_device_events_are_shown_and_tried_again_on_the_menu()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::atomic::AtomicUsize;
        type OnEvent = Box<dyn Fn() + Send + Sync>;
        let dir = std::env::temp_dir().join(format!("tulpa-devices-listen-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let runs = dir.join("runs");
        let lister = dir.join("lister.sh");
        crate::testutil::write_script(
            &lister,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"SPAudioDataType\":[]}}'\n",
                runs.display()
            ),
        )?;
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let tries = Arc::new(AtomicUsize::new(0));
        let wake: Arc<Mutex<Option<OnEvent>>> = Arc::new(Mutex::new(None));
        let register: Register = {
            let (tries, wake) = (Arc::clone(&tries), Arc::clone(&wake));
            Box::new(move |on_event| {
                // Refused twice (at start, and a second look), then held.
                if tries.fetch_add(1, Ordering::SeqCst) < 2 {
                    return Err("device events not registered (fake)".to_owned());
                }
                if let Ok(mut w) = wake.lock() {
                    *w = Some(on_event);
                }
                Ok(Box::new(()))
            })
        };
        let cache = Arc::new(
            DeviceCache::new(
                lister,
                dir.clone(),
                Arc::new(Stop::new()),
                journal,
                Box::new(|| {}),
            )
            .listening_with(register),
        );
        cache.listen();
        assert_eq!(
            cache.unheard().as_deref(),
            Some("device events not registered (fake)")
        );
        cache.listen();
        let said = |what: &str| {
            notes
                .0
                .lock()
                .map(|l| l.iter().filter(|l| l.contains(what)).count())
                .unwrap_or_default()
        };
        assert_eq!(said("not registered"), 1, "a refusal is said once");
        cache.retry_failed();
        assert_eq!(cache.unheard(), None, "the menu open registered them");
        assert_eq!(said("heard again"), 1);
        assert!(until(
            || runs_in(&runs) == 1 && !cache.reading.load(Ordering::SeqCst)
        ));
        cache.retry_failed();
        assert_eq!(
            tries.load(Ordering::SeqCst),
            3,
            "a held registration is not made again"
        );
        if let Ok(w) = wake.lock()
            && let Some(event) = w.as_ref()
        {
            event();
        }
        assert!(
            until(|| runs_in(&runs) == 2),
            "an event reads the lists again"
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A registration that panicked (it held the lock: `CoreAudio`'s call runs under it) does not leave `listen` a
    /// silent no-op: the next call registers again. A retry that is still refused shows its refusal, and the panic
    /// is said once over later calls.
    #[test]
    #[expect(clippy::panic, reason = "the test is about a registration that panics")]
    fn a_registration_that_panicked_is_tried_again() -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::atomic::AtomicUsize;
        let dir = std::env::temp_dir().join(format!("tulpa-devices-poison-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let tries = Arc::new(AtomicUsize::new(0));
        let register: Register = {
            let tries = Arc::clone(&tries);
            Box::new(move |_on_event| {
                if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::panic::panic_any("register panicked (fake)");
                }
                Ok(Box::new(()))
            })
        };
        let cache = Arc::new(
            DeviceCache::new(
                dir.join("no-lister"),
                dir.clone(),
                Arc::new(Stop::new()),
                journal,
                Box::new(|| {}),
            )
            .listening_with(register),
        );
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.listen()));
        assert!(panicked.is_err());
        cache.listen();
        assert_eq!(tries.load(Ordering::SeqCst), 2, "registered again");
        assert_eq!(cache.unheard(), None);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// After a registration panicked, a retry `CoreAudio` still refuses is the menu's line, and the panic is said once
    /// over two later listens.
    #[test]
    #[expect(clippy::panic, reason = "the test is about a registration that panics")]
    fn a_refusal_after_a_panic_is_shown_and_the_panic_said_once() {
        use std::sync::atomic::AtomicUsize;
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let tries = Arc::new(AtomicUsize::new(0));
        let register: Register = {
            let tries = Arc::clone(&tries);
            Box::new(move |_on_event| {
                if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::panic::panic_any("register panicked (fake)");
                }
                Err("device events not registered (fake)".to_owned())
            })
        };
        let dir = std::env::temp_dir();
        let cache = Arc::new(
            DeviceCache::new(
                dir.join("tulpa-no-lister"),
                dir,
                Arc::new(Stop::new()),
                journal,
                Box::new(|| {}),
            )
            .listening_with(register),
        );
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.listen()));
        assert!(panicked.is_err());
        cache.listen();
        cache.listen();
        assert_eq!(tries.load(Ordering::SeqCst), 3, "tried again each time");
        assert_eq!(
            cache.unheard().as_deref(),
            Some("device events not registered (fake)"),
            "the refusal is the menu's line"
        );
        let said = notes
            .0
            .lock()
            .map(|l| l.iter().filter(|l| l.contains("panicked")).count())
            .unwrap_or_default();
        assert_eq!(said, 1, "the panic is said once");
    }

    /// One read at a time, by hand: a same list is no change, a failure keeps the lists and is logged once.
    #[test]
    fn read_now_tells_a_change_and_logs_a_failure_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-devices-now-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let json = dir.join("audio.json");
        std::fs::write(&json, RECORDED)?;
        let lister = dir.join("lister.sh");
        crate::testutil::write_script(&lister, &format!("#!/bin/sh\ncat {}\n", json.display()))?;
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
        let cache = DeviceCache::new(
            lister,
            dir.clone(),
            Arc::new(Stop::new()),
            journal,
            Box::new(|| {}),
        );
        assert!(cache.read_now(), "the first list is a change");
        assert!(!cache.read_now(), "the same list again: no change");
        std::fs::write(&json, "garbage")?;
        assert!(!cache.read_now(), "a failed read keeps the lists");
        assert!(!cache.read_now());
        assert_eq!(cache.devices().outputs.len(), 3);
        let failed = notes
            .0
            .lock()
            .map(|l| l.iter().filter(|l| l.contains("failed")).count())
            .unwrap_or_default();
        assert_eq!(failed, 1, "a failure is logged once");
        std::fs::write(
            &json,
            r#"{"SPAudioDataType":[{"_items":[{"_name":"New Mic","coreaudio_device_input":1}]}]}"#,
        )?;
        assert!(cache.read_now(), "a new device is a change");
        assert_eq!(cache.devices().inputs, vec!["New Mic"]);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
