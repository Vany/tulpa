//! The ears organ behind the tray: a capture command run as the tray's own
//! child, a self-check that says when the capture went deaf, and `voice.say`.
//!
//! Verbs on the link (same line protocol as the hands, routed by verb):
//!
//! - `ears.start` spawns the configured argv as the tray's own child — so
//!   macOS attributes the microphone and audio capture to the app — in its
//!   own process group, with the cleared environment, stdout/stderr to
//!   `ears.log`. Already running → ok `"already running (pid N)"`. A link drop
//!   does not kill it and there is no time cap; `ears.stop`, Pause, Quit and
//!   the app's exit kill the whole group.
//! - `ears.stop` kills the group.
//! - `ears.status` answers `{"state":"off"|"running"|"degraded"|"deaf"|"exited", …}`.
//!   While a capture pid is claimed, the answer also carries evidence, not
//!   just the claim: `alive` (a signal-0 probe of the pid) and, when a real
//!   transcript line was ever published this session, `last_line_utc` and
//!   `last_line_age_s` (never: both `null`). A state that says `running`
//!   while `alive` is `false`, or while `last_line_age_s` keeps growing, is
//!   the signature of a wedged publish path — the reason these fields exist.
//! - `ears.test <text>` sends `text` as one transcript line of the
//!   microphone, the way a real line goes, marked `"test":true`
//!   ([`test_line_event`]) — whatever the mode filter — and answers `"sent"`.
//! - `voice.say <text>` runs `/usr/bin/say -v <voice> <text>` (argv, never a
//!   shell) and answers ok when `say` exits 0; refused while paused. With a
//!   voice output picked (`[voice] output_device`, the menu's «Voice output»)
//!   `say` gets `-a <name>` and a resident that plays itself `--device <name>`;
//!   a synthesized WAV plays with `afplay`, which cannot choose a device, so it
//!   follows the system default output. `[voice] rate` speeds speech up (or
//!   down) once per path, by what plays it: `say -r` ([`SAY_WPM`] × rate),
//!   `afplay -r` for every synthesized WAV, `--speed` for a resident that plays
//!   itself; a synth that only renders WAVs gets nothing (its clip is sped up
//!   by `afplay`, so a `--speed` there would multiply).
//!
//! [`SELF_CHECK_AFTER`] after a start, and every [`RECHECK_EVERY`] while it
//! runs, the capture's output is checked side by side ([`self_check`]): the
//! microphone (`me`) and the system audio (`them`). A side's directory goes
//! stale [`FRESH_WITHIN`] after its last change or after the capture's start,
//! whichever is later: what the previous capture left there does not count, so
//! a capture still loading its model is not heard yet, never stale. A side
//! named by an error in the capture's error file fails at once. One side
//! failing turns the
//! state DEGRADED — the capture keeps running, the other side is still heard —
//! and pushes `{"event":"ears","state":"degraded","side":"them","reason":"…",
//! "fix":"…"}`, `fix` naming the System Settings pane that grants that side
//! ([`Side::settings_pane`]; the menu line names it too and opens it on a
//! click, [`Ears::fix_url`]);
//! both failing turn it DEAF and push
//! `{"event":"ears","state":"deaf","reason":"…"}` to the link's [`Outbox`]; a
//! later check that finds both sides capturing again pushes
//! `{"event":"ears","state":"running","reason":"hearing again"}`. A capture
//! that exits on its own pushes `{"event":"ears","state":"exited","code":N}`;
//! a capture killed by Pause pushes `{"event":"ears","state":"off","reason":"paused"}`.
//!
//! The capture child gets [`DEVICE_ENV`] (`TULPA_DEVICE`), the link's device
//! name made safe for a file name ([`device_name`]), so one capture script can
//! name its files per device; with a microphone picked (`[ears] mic_device`,
//! the menu's «Ears mic») also [`MIC_ENV`] (`EARS_MIC=":<name>"`, an
//! avfoundation input by name, audio only). A new pick restarts a running
//! capture in its mode ([`Ears::set_mic`]). Since 2026-11-18 (#0393, the
//! owner's word: no files in the ears' path) the child also gets
//! [`LINES_STDOUT_ENV`] (`TULPA_LINES_STDOUT=1`): a capture that answers it
//! puts each recognised phrase on its stdout as one [`LINE_MARKER`] record
//! and writes no transcript files, and [`Ears::read_markers`] tees the stdout
//! to the side log and hands those records through the same [`line_event`]
//! with the same guards as the tailed files, plus one of its own: a
//! microphone phrase whose utterance span meets a remembered playback window
//! ([`Speaking`], in memory, per device) is the room's echo of our own voice
//! and never leaves the Mac.
//!
//! The tray tails the `transcript_files` from the first start on and pushes a
//! new line as `{"event":"ears","state":"line","side":"me"|"them","mode":…,
//! "text":…,"ts":…}`, with `"speaker":…` when the line carries a speaker tag
//! ([`line_event`]), by the owner's word of 2026-11-18: a line of the
//! microphone (`[me]`, his own speech) always goes, in any mode; a line of
//! the system audio (`[them]`) goes only while a call runs — the capture's
//! mode is `call` — and in any other mode it stays on the Mac: a video or a
//! song is nobody's words here. The mode switch stays the owner's privacy
//! switch: no line leaves while the mode is off, and none while paused or
//! while no capture runs — lines read then are dropped, never sent later.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{Pid, Signal};
use serde::Deserialize;
use serde_json::{Value, json};
use tulpa_hands::Journal;
use tulpa_hands::exec::{self, Ended};
use tulpa_stop::Stop;

use crate::config::{TrayAppConfig, expand_tilde_with};
use crate::flow;
use crate::link::{LineHandler, Outbox, held, refuse_line};
use crate::log::format_utc;
use crate::saylog::{Heard, SayLog, Wire};

/// A heard phrase's fate when it passes every gate: let through to the link, queued until the wire's word.
const LET_THROUGH: Heard = Heard::Wire(Wire::Queued);
use crate::state::OrganMode;
use crate::voicestream::{Resident, Spoken, split_sentences};

mod capture;
mod lid;
mod saved_mode;
mod transcript;

pub use lid::{IOREG, IoregLid, Lid, clamshell_closed};
use transcript::Tail;
pub use transcript::{
    LINE_MARKER, LINE_TEXT_MAX, line_event, line_verdict, marked_line, test_line_event,
};

/// How long after `ears.start` the self-check looks at the capture's output.
pub const SELF_CHECK_AFTER: Duration = Duration::from_secs(15);
/// After the first check, the self-check repeats this often while the capture runs.
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);
/// A mode switch is checked — both sides must produce a chunk this soon after the
/// new capture starts, else the switch is reported failed.
pub const SWITCH_CHECK_WITHIN: Duration = Duration::from_secs(15);
/// The newest chunk in each capture directory must be younger than this.
///
/// Counted from the capture's start when the directory has not changed since: a capture that has not
/// run this long yet is never stale (a capture may load a model before it opens a device).
pub const FRESH_WITHIN: Duration = Duration::from_secs(20);
/// The variable carrying the device name to the capture child.
pub const DEVICE_ENV: &str = "TULPA_DEVICE";
/// The variable carrying the picked microphone to the capture child: `:<name>`, an avfoundation input
/// by name (nothing before the `:` = no video, audio only).
pub const MIC_ENV: &str = "EARS_MIC";
/// Tells the capture child to put each recognised phrase on its stdout as one
/// [`LINE_MARKER`] record, not into transcript files.
///
/// vany 2026-11-18, #0393: no files in the ears' path. A capture that does
/// not know the variable ignores it, and its transcript files are still
/// tailed.
pub const LINES_STDOUT_ENV: &str = "TULPA_LINES_STDOUT";
/// The config's top-level `debug` as the capture child sees it: `1` on, absent off, whatever `env` says.
///
/// `tulpa_hear::ears::DEBUG_ENV` is the same name on its side. The capture's debug lines are kept in the side log
/// only: no [`LINE_MARKER`], so nothing of them crosses the link.
pub const DEBUG_ENV: &str = "TULPA_DEBUG";
/// Set for the `--ears` capture (`tulpa-hear --ears`), whose stdin is a pipe the tray holds.
///
/// The pipe's end is the capture's stop (`tulpa_hear::live::STDIN_STOP_ENV`, the same name on its side). A script
/// capture gets neither the variable nor the pipe: its stdin stays `/dev/null` (ffmpeg reads its stdin for keys).
pub const STDIN_STOP_ENV: &str = "TULPA_EARS_STDIN_STOP";
/// How long a capture that was told to stop has to exit before its group is killed.
///
/// Past the capture's own bound (`tulpa_hear::live::GRACE`, 5 s to finish what it holds), so the kill only meets
/// a capture that ignores the stop.
pub const STOP_GRACE: Duration = Duration::from_secs(7);
/// How long a capture's stdout is still read after the capture has exited and is reaped.
///
/// The end of the stream normally comes with the exit. It does not when something the capture started outside its
/// process group (a helper that called `setsid`) still holds the pipe: the wait for the end would then have none.
/// What the capture wrote before it exited is in the pipe already and is read well inside this.
pub const READ_AFTER_EXIT: Duration = Duration::from_secs(2);
/// The longest device name, as the link's hello allows.
const DEVICE_MAX: usize = 32;
/// The device name when the configured one leaves nothing usable.
const DEVICE_FALLBACK: &str = "tulpa";
/// Where a transcript path takes the device name.
const DEVICE_PLACEHOLDER: &str = "{device}";
/// How often the transcript files are looked at for new lines.
const TRANSCRIPT_EVERY: Duration = Duration::from_millis(500);
/// The longest text `voice.say` speaks, in characters.
pub const VOICE_TEXT_MAX: usize = 500;
/// The speech program; run directly, never through a shell.
pub const SAY_PROGRAM: &str = "/usr/bin/say";
/// A `say` run is killed past this.
const SAY_LIMIT: Duration = Duration::from_secs(120);
/// The own voice's synthesis gets this long before `say` takes over.
const VOICE_SYNTH_LIMIT: Duration = Duration::from_secs(20);
/// The resident synth's first answer also loads its model.
const VOICE_SERVE_FIRST_LIMIT: Duration = Duration::from_secs(90);
/// Plays the synthesized WAV.
const AFPLAY_PROGRAM: &str = "/usr/bin/afplay";
/// The own voice's playback volume, the same as the agent's other speaking path.
const VOICE_VOLUME: &str = "0.9";
/// `say`'s own speaking rate, in words a minute: `[voice] rate` scales it.
pub const SAY_WPM: f64 = 175.0;
/// The slowest and the fastest `[voice] rate` (afplay's play rate spans this).
pub const RATE_RANGE: std::ops::RangeInclusive<f64> = 0.5..=2.0;
/// How long a sentence waits for another one playing on the speaker.
const SPEAKER_WAIT: Duration = Duration::from_secs(60);
/// How often [`wait_for_speaker`] looks for an `afplay` again.
const SPEAKER_LOOK: Duration = Duration::from_millis(200);

/// What one piece of `voice.say` plays: a synthesized WAV (and when its synthesis started) or `say`.
enum Clip {
    Wav(PathBuf, SystemTime),
    Say(String),
}

/// Deletes a played (or abandoned) clip, and the directory the synth made for it.
fn forget_clip(wav: &Path, started: SystemTime) {
    discard_clip(wav);
    if let Some(dir) = clip_dir(wav, started) {
        let _gone: std::io::Result<()> = fs::remove_dir_all(dir);
    }
}

/// Removes a synthesized clip once it has served (only a `.wav`; a missing file is fine).
fn discard_clip(wav: &Path) {
    if wav
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    {
        let _gone: std::io::Result<()> = fs::remove_file(wav);
    }
}

/// How much earlier than the synth's start a fresh clip directory's birth time may read.
const CLIP_DIR_SLACK: Duration = Duration::from_secs(1);

/// The clip's directory when the synth created it for this sentence (born at or after `since`);
/// `None` for a directory that was there before (a shared output dir is never removed).
fn clip_dir(wav: &Path, since: SystemTime) -> Option<&Path> {
    let dir = wav.parent()?;
    let born = fs::metadata(dir).and_then(|m| m.created()).ok()?;
    // File-system birth times come from a coarse clock and may read a tick before `since`.
    let since = since.checked_sub(CLIP_DIR_SLACK).unwrap_or(since);
    (born >= since && dir.parent().is_some()).then_some(dir)
}

/// Waits while an `afplay` is playing (`pgrep -x afplay`), up to [`SPEAKER_WAIT`]; Pause ends the wait at once.
///
/// The wait itself is on Pause (its condvar: a pause wakes it at once), with the [`SPEAKER_WAIT`] end as its guard
/// against a wedge. Finding an `afplay` is the one poll here, every [`SPEAKER_LOOK`]: a NEW foreign `afplay` can
/// start at any time and macOS gives us no spawn event for it (kqueue's `NOTE_EXIT` could tell one's exit, but it
/// can't see a process that has not started yet).
fn wait_for_speaker(paused: &Stop) {
    let now = std::time::Instant::now;
    let end = now().checked_add(SPEAKER_WAIT);
    while !paused.is_set() && end.is_some_and(|end| now() < end) {
        let busy = std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "afplay"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !busy {
            return;
        }
        let next = now().checked_add(SPEAKER_LOOK).min(end);
        if paused.wait_until(next) {
            return;
        }
    }
}

/// How much of `say`'s stderr an error reply keeps.
const SAY_STDERR_MAX: usize = 300;
/// The longest voice name accepted from config.
const VOICE_NAME_MAX: usize = 64;
/// How much of the error file's tail is read for its last line.
const ERR_TAIL_BYTES: u64 = 4096;
/// How much of a deaf reason is kept.
const REASON_MAX: usize = 200;
/// How often a switch's check looks for fresh chunk files (no event for a file appearing here yet).
const CHECK_TICK: Duration = Duration::from_millis(100);

/// What the menu's ears line and the resume event say when Pause stopped the ears: Resume does not restart them.
pub const EARS_OFF_AFTER_PAUSE: &str =
    "off since Pause; Resume does not restart them, ears.mode or the menu turns them on";

/// Who changed the ears' mode: the `"source"` of every `mode` and `switched` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeSource {
    /// The tray menu's mode items.
    Menu,
    /// The tray's Pause.
    Pause,
    /// The mode the last run saved, restored at start.
    Restore,
    /// A hub verb (`ears.start`, `ears.stop`, `ears.mode`); the request's id.
    Request(String),
    /// The capture ended on its own.
    Exited,
    /// A mic pick that could not restart the capture.
    Mic,
    /// No change: the link came up and the hub is told the current mode ([`Ears::link_up`]).
    LinkUp,
}

impl ModeSource {
    /// The event's `"source"`: `menu`, `pause`, `restore`, `request:<id>`, `exited`, `mic`, `link-up`.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Menu => "menu".to_owned(),
            Self::Pause => "pause".to_owned(),
            Self::Restore => "restore".to_owned(),
            Self::Request(id) => format!("request:{id}"),
            Self::Exited => "exited".to_owned(),
            Self::Mic => "mic".to_owned(),
            Self::LinkUp => "link-up".to_owned(),
        }
    }
}

/// What the ears listen for, chosen from the menu or the link (`ears.mode`).
///
/// The id is stable ASCII (config keys, link events); the label is the menu's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EarsMode {
    /// Not listening.
    Off,
    /// At the office.
    Office,
    /// Alone with the owner.
    Alone,
    /// A live meeting in the room.
    Meeting,
    /// A call: microphone and system audio.
    Call,
    /// A stream.
    Stream,
}

impl EarsMode {
    /// Every mode, in menu order.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Office,
        Self::Alone,
        Self::Meeting,
        Self::Call,
        Self::Stream,
    ];

    /// The stable id: config keys and link events.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Office => "office",
            Self::Alone => "alone",
            Self::Meeting => "meeting",
            Self::Call => "call",
            Self::Stream => "stream",
        }
    }

    /// The menu label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Office => "office",
            Self::Alone => "alone",
            Self::Meeting => "meeting",
            Self::Call => "call",
            Self::Stream => "stream",
        }
    }

    /// The mode with this id.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.id() == id)
    }
}

/// Everything `ears.start` and `voice.say` need, resolved from `[ears]`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EarsSettings {
    argv: Vec<String>,
    me_dir: PathBuf,
    them_dir: PathBuf,
    err_file: Option<PathBuf>,
    /// [`DEVICE_ENV`] for the capture: the link's device name, made safe.
    device: String,
    /// Transcripts tailed for line events, `{device}` and `~` resolved.
    transcript_files: Vec<PathBuf>,
    voice: String,
    log_file: PathBuf,
    home: PathBuf,
    inherit_env: bool,
    env: Vec<(String, String)>,
    modes: Vec<(EarsMode, Vec<String>)>,
    voice_synth: Vec<String>,
    voice_serve: Vec<String>,
    voice_serve_plays: bool,
    voice_cwd: Option<PathBuf>,
    voice_post: Vec<String>,
    /// Plays a WAV (`afplay`; a stand-in in tests).
    player: PathBuf,
    /// `[ears] mic_device`: the microphone by name; `None` (unset or empty): `EARS_MIC` as `env` has it.
    mic_device: Option<String>,
    /// `[voice] output_device`: the voice's output by name; `None`: the system default output.
    output_device: Option<String>,
    /// `[voice] rate`; `None`: normal speed (unset or 1), nothing passed.
    rate: Option<f64>,
    /// The top-level `debug` switch, passed to the capture as [`DEBUG_ENV`].
    debug: bool,
}

impl EarsSettings {
    /// Resolves `[ears]` against `home`; the capture's output goes to
    /// `log_file`.
    ///
    /// # Errors
    ///
    /// Returns the refusal every ears verb answers with: the mode is not `on`,
    /// the command is empty or its program is not an absolute path, a
    /// self-check directory is missing, or the voice name is unusable.
    pub fn from_config(
        config: &TrayAppConfig,
        home: &Path,
        log_file: PathBuf,
    ) -> Result<Self, String> {
        let ears = &config.ears;
        match ears.organ.posture() {
            Ok(OrganMode::On) => {}
            Ok(OrganMode::Ask) => {
                return Err("ears: mode \"ask\" is not supported; set on or off".to_owned());
            }
            Ok(_) | Err(_) => return Err("ears: off in config".to_owned()),
        }
        // Modes: [ears.modes] id → argv; `call` falls back to [ears] command (the first config had only it).
        let mut modes = Vec::new();
        for (id, command) in &ears.modes {
            let mode = EarsMode::from_id(id)
                .filter(|m| *m != EarsMode::Off)
                .ok_or_else(|| format!("ears: [ears.modes] {id:?} is not a mode (office, alone, meeting, call, stream)"))?;
            modes.push((mode, capture_argv(command, Some(home))?));
        }
        if !modes.iter().any(|(m, _)| *m == EarsMode::Call) && !ears.command.is_empty() {
            modes.push((EarsMode::Call, capture_argv(&ears.command, Some(home))?));
        }
        if modes.is_empty() {
            return Err("ears: [ears] command is empty and no [ears.modes] is set".to_owned());
        }
        let argv = modes
            .iter()
            .find(|(m, _)| *m == EarsMode::Call)
            .map(|(_, argv)| argv.clone())
            .unwrap_or_default();
        let dir = |value: Option<&str>, key: &str| {
            value
                .filter(|v| !v.is_empty())
                .map(|v| expand_tilde_with(Some(home), v))
                .ok_or_else(|| format!("ears: [ears] {key} is not set"))
        };
        let me_dir = dir(ears.me_dir.as_deref(), "me_dir")?;
        let them_dir = dir(ears.them_dir.as_deref(), "them_dir")?;
        let err_file = ears
            .err_file
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| expand_tilde_with(Some(home), v));
        let voice = check_voice_name(&ears.voice)?;
        if let Some(bad) = ears
            .env
            .keys()
            .find(|k| k.is_empty() || k.contains('=') || k.contains('\0'))
        {
            return Err(format!("ears: [ears] env name {bad:?} is not usable"));
        }
        let expand = |argv: &[String]| -> Vec<String> {
            argv.iter()
                .map(|a| {
                    if a.starts_with('~') {
                        expand_tilde_with(Some(home), a).display().to_string()
                    } else {
                        a.clone()
                    }
                })
                .collect()
        };
        let device = device_name(&config.link.device);
        let transcript_files = transcript_files_of(ears, home, &device)?;
        if !ears.voice_synth.is_empty() && !ears.voice_synth.iter().any(|a| a.contains("{text}")) {
            return Err(
                "ears: [ears] voice_synth must carry {text} where the text goes".to_owned(),
            );
        }
        Ok(Self {
            mic_device: picked_name(ears.mic_device.as_deref(), "[ears] mic_device")?,
            output_device: picked_name(
                config.voice.output_device.as_deref(),
                "[voice] output_device",
            )?,
            rate: voice_rate(config.voice.rate)?,
            voice_synth: expand(&ears.voice_synth),
            voice_serve: expand(&ears.voice_serve),
            voice_serve_plays: ears.voice_serve_plays,
            voice_cwd: ears
                .voice_cwd
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| expand_tilde_with(Some(home), v)),
            voice_post: expand(&ears.voice_post),
            player: PathBuf::from(AFPLAY_PROGRAM),
            modes,
            inherit_env: ears.inherit_env,
            env: ears
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            argv,
            me_dir,
            them_dir,
            err_file,
            device,
            transcript_files,
            voice,
            log_file,
            home: home.to_path_buf(),
            debug: config.debug,
        })
    }

    /// What the capture child sees as `key`: the last `env` entry wins (as in `ears.start`); without one,
    /// the tray's own value under `inherit_env`, else only `HOME` (the cleared environment's).
    fn capture_env(&self, key: &str) -> Option<std::ffi::OsString> {
        if let Some((_, value)) = self.env.iter().rev().find(|(k, _)| k == key) {
            return Some(value.into());
        }
        if self.inherit_env {
            return std::env::var_os(key);
        }
        (key == "HOME").then(|| self.home.clone().into_os_string())
    }

    /// The capture's microphone cutter's `.close` (see `tulpa-segment`): `<state>/me/.close`, where
    /// `<state>` is the capture's state dir as `ears-capture.sh` names it — `EARS_STATE`, unset or empty:
    /// `$HOME/.cache/tulpa-ears`.
    fn me_close_file(&self) -> PathBuf {
        let state = self
            .capture_env("EARS_STATE")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || {
                    PathBuf::from(
                        self.capture_env("HOME")
                            .unwrap_or_else(|| self.home.clone().into_os_string()),
                    )
                    .join(".cache/tulpa-ears")
                },
                PathBuf::from,
            );
        state.join("me").join(".close")
    }

    /// The capture argv, `~` expanded.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The capture command of `mode`; `None` for off or a mode not configured.
    #[must_use]
    pub fn mode_argv(&self, mode: EarsMode) -> Option<&[String]> {
        self.modes
            .iter()
            .find(|(m, _)| *m == mode)
            .map(|(_, argv)| argv.as_slice())
    }

    /// The resident synth's argv: `voice_serve`, and for one that plays itself
    /// `--device <output>` when an output is picked and `--speed <rate>` when
    /// the rate is not normal, each unless the argv has its own. One that only
    /// renders WAVs gets neither: `afplay` plays (and speeds up) its clips.
    #[must_use]
    pub fn serve_argv(&self, output: Option<&str>) -> Vec<String> {
        let mut argv = self.voice_serve.clone();
        if !self.voice_serve_plays {
            return argv;
        }
        if let Some(device) = output
            && !has_flag(&argv, "--device")
        {
            argv.extend(["--device".to_owned(), device.to_owned()]);
        }
        if let Some(rate) = self.rate
            && !has_flag(&argv, "--speed")
        {
            argv.extend(["--speed".to_owned(), rate.to_string()]);
        }
        argv
    }

    /// Whether a transcript line could ever go: files listed. Which of them
    /// go is the owner's word of 2026-11-18 ([`transcript::line_event`]).
    const fn may_send_lines(&self) -> bool {
        !self.transcript_files.is_empty()
    }
}

/// Whether `argv` carries `flag`, alone or as `flag=value`.
fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| {
        a == flag
            || a.strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

/// `[voice] rate`: `None` when unset or 1 (normal speed, nothing passed).
///
/// # Errors
///
/// A rate outside [`RATE_RANGE`] (or not a number), naming the key.
fn voice_rate(rate: Option<f64>) -> Result<Option<f64>, String> {
    match rate {
        None => Ok(None),
        Some(rate) if !RATE_RANGE.contains(&rate) => Err(format!(
            "ears: [voice] rate {rate} is outside {}..{}",
            RATE_RANGE.start(),
            RATE_RANGE.end()
        )),
        Some(rate) if (rate - 1.0).abs() < 1e-9 => Ok(None),
        Some(rate) => Ok(Some(rate)),
    }
}

/// A device name from the config: empty or unset is `None` (the default).
///
/// # Errors
///
/// A name [`crate::devices::check_name`] refuses, naming the key.
fn picked_name(value: Option<&str>, key: &str) -> Result<Option<String>, String> {
    value
        .filter(|v| !v.is_empty())
        .map(|name| {
            crate::devices::check_name(name)
                .map(|()| name.to_owned())
                .map_err(|why| format!("ears: {key}: {why}"))
        })
        .transpose()
}

/// `[ears] transcript_files` (`{device}` and `~` resolved, duplicates dropped).
///
/// # Errors
///
/// A transcript path that is not absolute.
fn transcript_files_of(
    ears: &crate::config::EarsSection,
    home: &Path,
    device: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for file in &ears.transcript_files {
        let path = expand_tilde_with(Some(home), &file.replace(DEVICE_PLACEHOLDER, device));
        if !path.is_absolute() {
            return Err(format!(
                "ears: [ears] transcript_files entry {file:?} is not an absolute path"
            ));
        }
        if !files.contains(&path) {
            files.push(path);
        }
    }
    Ok(files)
}

/// The startup notes the owner asked for on 2026-11-18, so a Mac never runs
/// quietly wrong: the gating keys set in config though they are inert now, and
/// a transcript file whose name lacks this device's token (the 13:04Z bug:
/// home reading `call-me-work-mac.txt`). Both go to the journal; the file-name
/// one also rides the link as `{"event":"link","state":"config-warn",…}`, so
/// the agent learns of it without reading the Mac's log.
fn startup_notes(
    config: &TrayAppConfig,
    settings: &Result<EarsSettings, String>,
    outbox: &Outbox,
    journal: &dyn Journal,
) {
    let ears = &config.ears;
    let inert = [
        (!ears.wake_words.is_empty()).then_some("wake_words"),
        (!ears.wake_aliases.is_empty()).then_some("wake_aliases"),
        ears.transcript_events
            .is_some()
            .then_some("transcript_events"),
        ears.followup_secs.is_some().then_some("followup_secs"),
        ears.followup_max_secs
            .is_some()
            .then_some("followup_max_secs"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !inert.is_empty() {
        journal.record(&format!(
            "ears: [ears] {} set but inert since 2026-11-18 (the side decides: the \
             microphone's lines always go, the system audio's only in a call); drop the keys",
            inert.join(", ")
        ));
    }
    let Ok(s) = settings else { return };
    for path in &s.transcript_files {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if name.contains(&s.device) {
            continue;
        }
        let shown: String = name.chars().take(80).collect();
        journal.record(&format!(
            "ears: transcript file {shown:?} lacks this device's token {:?}: a config copied \
             from another Mac? The lines are still read",
            s.device
        ));
        if let Some(ts) = crate::log::format_utc(SystemTime::now()) {
            outbox.push(
                json!({
                    "event": "link",
                    "state": "config-warn",
                    "reason": format!(
                        "transcript file {shown} lacks this device's token {}; a config \
                         copied from another Mac?",
                        s.device
                    ),
                    "ts": ts,
                })
                .to_string()
                .into_bytes(),
            );
        }
    }
}

/// The device name the capture sees in [`DEVICE_ENV`], safe in a file name.
///
/// `[link] device` with every character outside `[A-Za-z0-9_-]` turned into
/// `_` (so no `/` and no `.`), at most 32 of them; `tulpa` when nothing is left.
#[must_use]
pub fn device_name(configured: &str) -> String {
    let safe: String = configured
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(DEVICE_MAX)
        .collect();
    if safe.is_empty() {
        DEVICE_FALLBACK.to_owned()
    } else {
        safe
    }
}

/// The capture command's argv with a leading `~` in every element expanded
/// against `home`.
///
/// # Errors
///
/// Returns an error when the command is empty or its program is not an
/// absolute path after expansion (the cleared `PATH` must not pick it).
pub fn capture_argv(command: &[String], home: Option<&Path>) -> Result<Vec<String>, String> {
    let argv: Vec<String> = command
        .iter()
        .map(|part| expand_tilde_with(home, part).to_string_lossy().into_owned())
        .collect();
    match argv.first() {
        None => Err("ears: [ears] command is empty".to_owned()),
        Some(program) if !Path::new(program).is_absolute() => Err(format!(
            "ears: [ears] command must start with an absolute program path, not {program:?}"
        )),
        Some(_) => Ok(argv),
    }
}

/// The startup warning when `[ears] command` is set but `[ears.modes]`
/// entries win with a different argv: names those modes, sorted. `None`
/// when the command is empty, no mode is set, or every mode runs the same.
fn command_overridden(ears: &crate::config::EarsSection, home: &Path) -> Option<String> {
    let command = capture_argv(&ears.command, Some(home)).ok()?;
    let mut differ: Vec<&str> = ears
        .modes
        .iter()
        .filter(|(_, argv)| capture_argv(argv, Some(home)).ok().as_ref() != Some(&command))
        .map(|(id, _)| id.as_str())
        .collect();
    if differ.is_empty() {
        return None;
    }
    differ.sort_unstable();
    Some(format!(
        "ears: [ears] command {command:?} is ignored for {}: [ears.modes] sets their commands",
        differ.join(", ")
    ))
}

fn check_voice_name(voice: &str) -> Result<String, String> {
    let ok = !voice.is_empty()
        && voice.len() <= VOICE_NAME_MAX
        && !voice.starts_with('-')
        && voice
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')' | '.'));
    if ok {
        Ok(voice.to_owned())
    } else {
        Err(format!(
            "ears: [ears] voice {voice:?} is not a usable voice name"
        ))
    }
}

/// The text `say` will speak: control characters become spaces, the ends are
/// trimmed, and a leading `-` is shielded by a space so `say` never reads the
/// text as an option.
///
/// # Errors
///
/// Returns an error for empty text or text over [`VOICE_TEXT_MAX`] characters
/// (refused rather than cut, so nothing is half-said).
pub fn clean_voice_text(text: &str) -> Result<String, String> {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = spaced.trim();
    if trimmed.is_empty() {
        return Err("voice: empty text".to_owned());
    }
    let count = trimmed.chars().count();
    if count > VOICE_TEXT_MAX {
        return Err(format!(
            "voice: text is {count} characters; the cap is {VOICE_TEXT_MAX}"
        ));
    }
    Ok(if trimmed.starts_with('-') {
        format!(" {trimmed}")
    } else {
        trimmed.to_owned()
    })
}

/// The arguments after [`SAY_PROGRAM`]: `-v <voice> [-a <device>] [-r <wpm>] <text>`.
///
/// `device` is the output by name (`None`: the system default output); `rate`
/// scales [`SAY_WPM`] (`None`: `say`'s own rate).
///
/// # Errors
///
/// Returns the [`clean_voice_text`] refusal.
pub fn say_args(
    voice: &str,
    text: &str,
    device: Option<&str>,
    rate: Option<f64>,
) -> Result<Vec<String>, String> {
    let mut args = vec!["-v".to_owned(), voice.to_owned()];
    if let Some(device) = device {
        args.extend(["-a".to_owned(), device.to_owned()]);
    }
    if let Some(rate) = rate {
        args.extend(["-r".to_owned(), format!("{:.0}", (SAY_WPM * rate).round())]);
    }
    args.push(clean_voice_text(text)?);
    Ok(args)
}

/// The options before the WAV for the clip player (`afplay`): `-v <volume>`,
/// and with a `rate` `-r <rate> -q 1` (its play rate, pitch kept, in its higher
/// quality).
#[must_use]
pub fn player_args(rate: Option<f64>) -> Vec<String> {
    let mut args = vec!["-v".to_owned(), VOICE_VOLUME.to_owned()];
    if let Some(rate) = rate {
        args.extend([
            "-r".to_owned(),
            rate.to_string(),
            "-q".to_owned(),
            "1".to_owned(),
        ]);
    }
    args
}

/// What the self-check saw in one capture directory.
///
/// Sizes are not looked at: a segmenting writer keeps the chunk it is filling
/// at 0 bytes until it closes, and the transcriber takes closed chunks away
/// within a second, so the newest file is nearly always empty while the ears
/// work. What moves while they work is the directory itself (a chunk created or
/// taken away) and the files' times.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DirLook {
    /// The directory could not be read.
    Unreadable(String),
    /// The latest change seen: the directory's own modification time or its
    /// newest regular file's, whichever is later.
    Changed(SystemTime),
}

/// Words that make an error-file line an error; any other line there (the
/// capture's own info, e.g. `sysaudio: capturing system audio …`) is not.
const ERROR_WORDS: &[&str] = &[
    "error",
    "declined",
    "denied",
    "not permitted",
    "failed",
    "fatal",
    "cannot",
    "can't",
    "unable",
    "no display",
];

/// True when an error-file line reads as an error, not as the capture's info.
fn is_error_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    ERROR_WORDS.iter().any(|word| lower.contains(word))
}

/// One side of the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Side {
    /// The local microphone (`me_dir`).
    Me,
    /// The system audio (`them_dir`).
    Them,
}

impl Side {
    /// The stable id in events, `ears.status` and the menu: `me` or `them`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Me => "me",
            Self::Them => "them",
        }
    }

    /// The side across.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Me => Self::Them,
            Self::Them => Self::Me,
        }
    }

    /// The System Settings pane that grants this side's capture, as the
    /// DEGRADED menu line and event (`fix`) name it.
    #[must_use]
    pub const fn settings_pane(self) -> &'static str {
        match self {
            Self::Me => "Privacy & Security → Microphone",
            Self::Them => "Privacy & Security → Screen & System Audio Recording",
        }
    }

    /// The URL that opens [`Side::settings_pane`] (given to `/usr/bin/open`).
    #[must_use]
    pub const fn settings_url(self) -> &'static str {
        match self {
            Self::Me => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Self::Them => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
        }
    }
}

/// Words that put an error-file line on the system-audio side, anywhere in it
/// (the system-audio tool starts its lines with `sysaudio:`;
/// `ScreenCaptureKit` errors name `SCStream`).
const THEM_WORDS: &[&str] = &[
    "sysaudio",
    "system audio",
    "scstream",
    "screencapture",
    "screen capture",
    "screen recording",
    "no display",
];

/// The side an error-file line is about; `None` when it names neither side.
///
/// The line's tag before the first `:` (`me` or `mic`; `them` or `sysaudio`)
/// or a word only one side uses decides. A line naming neither side, or both,
/// counts for both.
#[must_use]
pub fn side_of(line: &str) -> Option<Side> {
    let lower = line.trim().to_lowercase();
    let tag = lower.split(':').next().unwrap_or_default().trim();
    let them = matches!(tag, "them" | "sysaudio") || THEM_WORDS.iter().any(|w| lower.contains(w));
    let me = matches!(tag, "me" | "mic")
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| matches!(word, "mic" | "microphone"));
    match (me, them) {
        (true, false) => Some(Side::Me),
        (false, true) => Some(Side::Them),
        _ => None,
    }
}

/// What the self-check found, side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Health {
    /// Both sides capture.
    Hearing,
    /// One side fails; the other still captures.
    Degraded {
        /// The failing side.
        side: Side,
        /// Why it fails.
        reason: String,
    },
    /// Both sides fail.
    Deaf {
        /// Why: the one shared reason, or each side's.
        reason: String,
    },
}

/// The self-check over what was seen, a pure function of its inputs.
///
/// Each side is judged on its own. Its error first: the newest error-file
/// line about that side (or about neither side, see [`side_of`]) when it reads
/// as an error — info lines the capture writes there don't count, and an
/// error the capture recovered from (it wrote its info line again after a
/// restart) no longer does. Then its directory's latest change, which must be
/// within [`FRESH_WITHIN`] of `now` (about three 4 s segments and the
/// transcriber's pickup), or, when it has not changed since the capture
/// started at `since`, the start must be ([`stopped_for`]). With `lid_closed`,
/// a side whose directory stopped says why: `no chunk rotated for 45 s (lid closed)`.
#[must_use]
pub fn self_check(
    me: &DirLook,
    them: &DirLook,
    err_text: Option<&str>,
    lid_closed: bool,
    since: SystemTime,
    now: SystemTime,
) -> Health {
    let (me_error, them_error) = (
        side_error(err_text, Side::Me),
        side_error(err_text, Side::Them),
    );
    // One error line naming neither side fails both, for one reason.
    if let (Some(shared), Some(other)) = (&me_error, &them_error)
        && shared == other
    {
        return Health::Deaf {
            reason: shared.clone(),
        };
    }
    let me = me_error.or_else(|| dir_fault(me, lid_closed, since, now));
    let them = them_error.or_else(|| dir_fault(them, lid_closed, since, now));
    match (me, them) {
        (None, None) => Health::Hearing,
        (Some(reason), None) => Health::Degraded {
            side: Side::Me,
            reason,
        },
        (None, Some(reason)) => Health::Degraded {
            side: Side::Them,
            reason,
        },
        (Some(me), Some(them)) => Health::Deaf {
            reason: cap_reason(&format!("me: {me}; them: {them}")),
        },
    }
}

/// The side's capture error: the newest non-blank error-file line about this
/// side or about neither, when that line reads as an error.
fn side_error(err_text: Option<&str>, side: Side) -> Option<String> {
    err_text?
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find(|l| side_of(l).is_none_or(|about| about == side))
        .filter(|l| is_error_line(l))
        .map(|l| cap_reason(&format!("capture error: {}", l.trim())))
}

/// Why a side's directory says it stopped capturing; `None` while it moves.
/// A closed lid is named as the likely cause of a directory that stopped.
fn dir_fault(
    look: &DirLook,
    lid_closed: bool,
    since: SystemTime,
    now: SystemTime,
) -> Option<String> {
    match look {
        DirLook::Unreadable(why) => Some(cap_reason(&format!("directory unreadable: {why}"))),
        DirLook::Changed(_) => stopped_for(look, since, now).map(|age| {
            let lid = if lid_closed { " (lid closed)" } else { "" };
            format!("no chunk rotated for {} s{lid}", age.as_secs())
        }),
    }
}

/// How long a directory has not changed for the capture started at `since`,
/// when that is past [`FRESH_WITHIN`]: from its last change made by this
/// capture ([`own_change`]), else from the start, so a side is never stale
/// for longer than its capture has run. `None` while it moves (a time in the
/// future, clock skew, counts as fresh) or when it is unreadable.
fn stopped_for(look: &DirLook, since: SystemTime, now: SystemTime) -> Option<Duration> {
    match look {
        DirLook::Unreadable(_) => None,
        DirLook::Changed(_) => {
            let from = own_change(look, since).unwrap_or(since);
            let age = now.duration_since(from).unwrap_or(Duration::ZERO);
            (age > FRESH_WITHIN).then_some(age)
        }
    }
}

/// The directory's last change when the capture started at `since` made it:
/// a change from before the start is a previous capture's, not this one's.
fn own_change(look: &DirLook, since: SystemTime) -> Option<SystemTime> {
    match look {
        DirLook::Changed(at) if *at >= since => Some(*at),
        DirLook::Changed(_) | DirLook::Unreadable(_) => None,
    }
}

fn cap_reason(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(REASON_MAX)
        .collect()
}

/// One step of a `voice.say` in `flow.log`: `queued`, `synth`, `played`, `ended` or `failed:<why>`,
/// with the text it is about (the whole say's, or one sentence's).
fn flow_voice(id: Option<&str>, stage: &str, text: &str, extra: &[(&str, &str)]) {
    let mut fields = vec![("stage", stage)];
    fields.extend_from_slice(extra);
    flow::record(
        flow::Dir::Note,
        id,
        flow::Kind::Voice,
        text.as_bytes(),
        &fields,
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// How long a playback window stays remembered.
const SPEAKING_KEEP_MS: u64 = 120_000;

/// The own playback windows — `voice.say`'s and the clips' — in memory, against
/// which a microphone line is dropped as the room's echo of our own voice.
///
/// Per device by construction: memory is not a file another Mac could share,
/// which the speaking file once was (a Syncthing-shared path let one Mac's
/// playback mute the other's microphone).
#[derive(Debug, Default)]
pub struct Speaking {
    /// Windows `t0..t1` in epoch ms; `t1 == 0` marks one still playing.
    windows: Mutex<Vec<(u64, u64)>>,
}

impl Speaking {
    /// Records the playback window `t0..t1` (`t1` 0: still playing): a window
    /// that started at `t0` is replaced, windows older than
    /// [`SPEAKING_KEEP_MS`] at `now` are dropped.
    pub fn mark(&self, t0: u64, t1: u64, now: u64) {
        let Ok(mut windows) = self.windows.lock() else {
            return;
        };
        windows.retain(|(a, b)| {
            let ended = if *b == 0 { now } else { *b };
            *a != t0 && now.saturating_sub(ended) <= SPEAKING_KEEP_MS
        });
        windows.push((t0, t1));
    }

    /// Whether an utterance `t0..t1` (epoch ms) overlaps any window at `now`:
    /// ±1 s around the utterance, a window's end held 1.5 s longer for the
    /// room, an open window reaching to `now` — the margins the capture's own
    /// gate used, kept so the move in memory changes where the windows live,
    /// not what they mean.
    #[must_use]
    pub fn echoes(&self, t0: u64, t1: u64, now: u64) -> bool {
        let Ok(windows) = self.windows.lock() else {
            return false;
        };
        windows.iter().any(|(a, b)| {
            let ends = if *b == 0 {
                now
            } else {
                b.saturating_add(1_500)
            };
            *a <= t1.saturating_add(1_000) && ends >= t0.saturating_sub(1_000)
        })
    }
}

/// Looks at one capture directory (regular files only, symlinks not followed).
pub fn look_dir(dir: &Path) -> DirLook {
    let own = match fs::metadata(dir).and_then(|meta| meta.modified()) {
        Ok(at) => at,
        Err(error) => return DirLook::Unreadable(error.to_string()),
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => return DirLook::Unreadable(error.to_string()),
    };
    let newest = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(fs::Metadata::is_file)
        .filter_map(|meta| meta.modified().ok())
        .max();
    DirLook::Changed(newest.map_or(own, |at| at.max(own)))
}

/// The last [`ERR_TAIL_BYTES`] of the error file; `None` when it is absent.
fn read_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > ERR_TAIL_BYTES {
        file.seek(SeekFrom::Start(len.saturating_sub(ERR_TAIL_BYTES)))
            .ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The ears' state, as `ears.status` and the menu show it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EarsState {
    /// No capture running (never started, stopped, or killed by Pause).
    Off,
    /// The capture runs and has not failed its self-check.
    Running {
        /// The child's pid, also its process group.
        pid: u32,
        /// When it was started.
        since: SystemTime,
    },
    /// The capture runs and one side failed its self-check; the other side
    /// is still heard.
    Degraded {
        /// The child's pid, also its process group.
        pid: u32,
        /// When it was started.
        since: SystemTime,
        /// The failing side.
        side: Side,
        /// Why it fails.
        reason: String,
    },
    /// The capture runs but both sides failed its self-check.
    Deaf {
        /// The child's pid, also its process group.
        pid: u32,
        /// When it was started.
        since: SystemTime,
        /// Why the self-check failed.
        reason: String,
    },
    /// The capture exited on its own.
    Exited {
        /// Its exit code; `None` when a signal from elsewhere ended it.
        code: Option<i32>,
    },
}

impl EarsState {
    /// The `ears.status` result object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let at = |since: &SystemTime| format_utc(*since).unwrap_or_default();
        match self {
            Self::Off => json!({"state": "off"}),
            Self::Running { pid, since } => {
                json!({"state": "running", "pid": pid, "since": at(since)})
            }
            Self::Degraded {
                pid,
                since,
                side,
                reason,
            } => {
                json!({"state": "degraded", "pid": pid, "since": at(since), "side": side.id(), "reason": reason, "fix": side.settings_pane()})
            }
            Self::Deaf { pid, since, reason } => {
                json!({"state": "deaf", "pid": pid, "since": at(since), "reason": reason})
            }
            Self::Exited { code } => json!({"state": "exited", "code": code}),
        }
    }

    const fn pid(&self) -> Option<u32> {
        match self {
            Self::Running { pid, .. } | Self::Degraded { pid, .. } | Self::Deaf { pid, .. } => {
                Some(*pid)
            }
            Self::Off | Self::Exited { .. } => None,
        }
    }

    /// The state a running capture (`pid`, started `since`) is in after a
    /// self-check found `health`.
    fn checked(pid: u32, since: SystemTime, health: Health) -> Self {
        match health {
            Health::Hearing => Self::Running { pid, since },
            Health::Degraded { side, reason } => Self::Degraded {
                pid,
                since,
                side,
                reason,
            },
            Health::Deaf { reason } => Self::Deaf { pid, since, reason },
        }
    }

    /// When the running capture was started; `None` when none runs. The
    /// system audio's lines go only from this moment on (less the
    /// transcript's since-slack), so a `[them]` line the last mode already
    /// heard stays local even when the mode has since turned to a call.
    const fn since(&self) -> Option<SystemTime> {
        match self {
            Self::Running { since, .. }
            | Self::Degraded { since, .. }
            | Self::Deaf { since, .. } => Some(*since),
            Self::Off | Self::Exited { .. } => None,
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: EarsState,
    /// The mode the running (or last started) capture is for; off when none runs.
    mode: EarsMode,
    /// Bumped on every start and stop: a late waiter or self-check whose
    /// start is no longer current leaves the state alone.
    generation: u64,
    /// Set when the current start is over (the next start or stop, or the
    /// capture's own exit), so its checks waiting on a timer return at once.
    ended: Arc<Stop>,
    /// The running capture's process group; `None` when none runs. Every signal to the capture goes through it:
    /// it sends nothing once the capture is reaped, when the group's id may be another process's.
    group: Option<Arc<exec::Group>>,
    /// The write end of the pipe on the running `--ears` capture's stdin ([`STDIN_STOP_ENV`]); `None` for any
    /// other capture, and when none runs. Closing it is the stop; the tray's death closes it too. Held here and
    /// nowhere else: a copy in another process would keep the capture's stdin open past the tray.
    stdin: Option<std::process::ChildStdin>,
    /// Set once the running capture has exited and is reaped (its own, per start).
    exited: Arc<Stop>,
    /// Set once the running capture's stdout is read to its end, or cut ([`Ears::read_grace`]); its own, per
    /// start.
    read: Arc<Stop>,
    /// The captures that were told to stop and may still be writing what they held, one entry per generation:
    /// their lines are taken until each is gone, though none is the current capture. Several at once when stops
    /// overlap (a stop, a start and another stop inside the grace).
    closing: Vec<Closing>,
    /// The captures that were given up: not gone after their kill's bound ([`Ears::give_up`]). Nobody waits on
    /// them any more, but they are not out of reach: Pause, Quit and the drop still kill each through its group's
    /// guard, for as long as it is not reaped.
    given_up: Vec<Closing>,
}

/// See [`Inner::closing`]. An entry stays until its capture is gone AND its stdout is read to the end
/// ([`Closing::done`]): the process can be reaped while its last lines are still unread in the pipe, and those
/// lines are the capture's as much as the earlier ones.
#[derive(Debug, Clone)]
struct Closing {
    generation: u64,
    mode: EarsMode,
    since: SystemTime,
    /// Set once the capture has exited and is reaped (its waiter sets it).
    exited: Arc<Stop>,
    /// Set once the reader thread has read the capture's stdout to its end, or the stream was cut
    /// ([`Ears::read_grace`]).
    read: Arc<Stop>,
    pid: u32,
    /// The capture's process group, while it may still be alive: Pause and Quit reach a closing capture through
    /// it, exactly as they reach the running one.
    group: Arc<exec::Group>,
}

impl Closing {
    /// Nothing more comes from this capture: it is reaped, and its stdout is read out.
    fn done(&self) -> bool {
        self.read.is_set() && self.exited.is_set()
    }
}

/// How a capture is ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    /// Told to stop: its stdin closed (the `--ears` capture's pipe) and TERM to its group. It writes what it holds
    /// (a turn held at a cut, the phrase in progress) and exits; one that has not after [`Ears::stop_grace`] is
    /// killed. The caller does not wait (the tray's Quit then waits for all of them at once).
    Stop,
    /// SIGKILL to its group, now: the tray's Pause.
    Kill,
}

impl Inner {
    /// Ends the current start and begins a new generation; its number and its end event.
    fn next_generation(&mut self) -> (u64, Arc<Stop>) {
        self.ended.set();
        self.ended = Arc::new(Stop::new());
        self.generation = self.generation.wrapping_add(1);
        (self.generation, Arc::clone(&self.ended))
    }

    /// The mode of the running capture; off when none runs.
    const fn current_mode(&self) -> EarsMode {
        if self.state.pid().is_some() {
            self.mode
        } else {
            EarsMode::Off
        }
    }
}

/// When the last REAL transcript line was published to the link (test lines
/// via `ears.test` don't count: they prove the path, not the organ); `None`
/// until one goes. `ears.status` reports it so a wedged publish path shows up
/// as a growing `last_line_age_s` instead of a healthy-looking `running`.
type LastLine = Mutex<Option<SystemTime>>;

/// The ears organ: settings (or the reason it refuses), the running capture,
/// the tray's pause flag, the link's outbox, and the activity log.
pub struct Ears {
    settings: Result<EarsSettings, String>,
    inner: Mutex<Inner>,
    paused: Arc<Stop>,
    outbox: Arc<Outbox>,
    journal: Arc<dyn Journal>,
    changed: Box<dyn Fn() + Send + Sync>,
    check_after: Duration,
    /// [`STOP_GRACE`]; tests set it shorter.
    stop_grace: Duration,
    /// [`READ_AFTER_EXIT`].
    read_grace: Duration,
    switch_within: Duration,
    /// How often a switch's check looks for fresh chunk files ([`CHECK_TICK`]).
    check_tick: Duration,
    /// How often the transcripts are looked at.
    tail_every: Duration,
    /// Set once the transcripts are tailed (from the first start on).
    tailing: AtomicBool,
    /// Set when Pause stopped a capture, cleared by the next mode set: Resume leaves the ears off, and the menu
    /// line and the resume event say so ([`Ears::off_by_pause`]).
    off_by_pause: AtomicBool,
    /// The own voice's resident synth, started on the first `voice.say` (streamed speech).
    resident: Mutex<Option<Resident>>,
    /// Where the chosen mode is kept across restarts ([`Ears::with_mode_file`]); none: not kept. The
    /// lock also keeps two saves from writing at once.
    mode_file: Mutex<Option<PathBuf>>,
    /// The microphone and the voice output picked (config, then the menu), by name.
    picked: Mutex<Picked>,
    /// Held across a stop and the start that follows it (a mode switch, a new microphone) and by
    /// `ears.stop`, so a stop from the link can't land between the two and be undone by the start.
    switching: Mutex<()>,
    /// Tells the keep-awake's state for `ears.status` ([`Ears::report_keepawake`]); none: not reported.
    keepawake: Mutex<Option<KeepAwakeStatus>>,
    /// Tells whether the lid is closed, for a self-check that finds a side stopped ([`Ears::with_lid`]);
    /// none: never said.
    lid: Option<Arc<dyn Lid>>,
    /// The own playback windows, in memory ([`Speaking`]): echo of `voice.say`
    /// and the clips is dropped from the microphone's lines tray-side.
    speaking: Arc<Speaking>,
    /// When the last real transcript line went to the link ([`LastLine`]).
    last_line: LastLine,
    /// The log window's log (#0423), when one is attached ([`Ears::with_saylog`]); none: no window.
    saylog: Option<Arc<Mutex<SayLog>>>,
    /// `voice.say` calls in flight ([`SayInFlight`]); the last to end closes the microphone's open utterance.
    saying: AtomicUsize,
    /// Under test, the live capture threads by name ([`Ears::capture_thread`]).
    #[cfg(test)]
    capture_threads: Arc<Mutex<Vec<&'static str>>>,
}

/// One `voice.say` in flight, counted on [`Ears`]: dropped on every way out of the say, and the last one
/// to go asks the microphone's cutter to close the utterance in progress ([`Ears::close_me`]) — the
/// mic heard our own voice; the owner's next words start a fresh utterance. The mic stays on.
struct SayInFlight<'a> {
    ears: &'a Ears,
    settings: &'a EarsSettings,
}

impl<'a> SayInFlight<'a> {
    fn start(ears: &'a Ears, settings: &'a EarsSettings) -> Self {
        ears.saying.fetch_add(1, Ordering::SeqCst);
        Self { ears, settings }
    }
}

impl Drop for SayInFlight<'_> {
    fn drop(&mut self) {
        if self.ears.saying.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.ears.close_me(self.settings);
        }
    }
}

/// Tells the keep-awake's state, e.g. `on (always)`.
pub type KeepAwakeStatus = Box<dyn Fn() -> String + Send + Sync>;

/// Devices picked by name; `None`: the default.
#[derive(Debug, Clone, Default)]
struct Picked {
    mic: Option<String>,
    output: Option<String>,
}

impl fmt::Debug for Ears {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ears")
            .field("settings", &self.settings)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Request {
    id: String,
    verb: String,
    #[serde(default)]
    arg: Option<String>,
}

impl Ears {
    /// Binds the organ. `settings` is `Err(reason)` when `[ears]` is off or
    /// unusable: every verb then answers with that reason. `changed` runs
    /// whenever the state changes, so the presenter can re-render.
    #[must_use]
    pub fn new(
        settings: Result<EarsSettings, String>,
        paused: Arc<Stop>,
        outbox: Arc<Outbox>,
        journal: Arc<dyn Journal>,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let picked = settings.as_ref().map_or_else(
            |_| Picked::default(),
            |s| Picked {
                mic: s.mic_device.clone(),
                output: s.output_device.clone(),
            },
        );
        Self {
            settings,
            picked: Mutex::new(picked),
            switching: Mutex::new(()),
            inner: Mutex::new(Inner {
                state: EarsState::Off,
                mode: EarsMode::Off,
                generation: 0,
                ended: Arc::new(Stop::new()),
                group: None,
                stdin: None,
                exited: Arc::new(Stop::new()),
                read: Arc::new(Stop::new()),
                closing: Vec::new(),
                given_up: Vec::new(),
            }),
            paused,
            outbox,
            journal,
            changed,
            check_after: SELF_CHECK_AFTER,
            stop_grace: STOP_GRACE,
            read_grace: READ_AFTER_EXIT,
            switch_within: SWITCH_CHECK_WITHIN,
            check_tick: CHECK_TICK,
            tail_every: TRANSCRIPT_EVERY,
            tailing: AtomicBool::new(false),
            off_by_pause: AtomicBool::new(false),
            resident: Mutex::new(None),
            mode_file: Mutex::new(None),
            keepawake: Mutex::new(None),
            lid: None,
            speaking: Arc::new(Speaking::default()),
            last_line: Mutex::new(None),
            saylog: None,
            saying: AtomicUsize::new(0),
            #[cfg(test)]
            capture_threads: Arc::default(),
        }
    }

    /// Names a closed lid in the self-check's reason for a side that stopped rotating chunks, e.g.
    /// `no chunk rotated for 45 s (lid closed)`. The lid is read only when a side has stopped.
    #[must_use]
    pub fn with_lid(mut self, lid: Arc<dyn Lid>) -> Self {
        self.lid = Some(lid);
        self
    }

    /// Attaches the log window's log (#0423): every heard phrase lands in it with its fate
    /// ([`Ears::fate`]), and every say that ended with what was said. The window ([`crate::macos`])
    /// renders the same log.
    #[must_use]
    pub fn with_saylog(mut self, saylog: Arc<Mutex<SayLog>>) -> Self {
        self.saylog = Some(saylog);
        self
    }

    /// Applies `f` to the log window's log when one is attached, poisoned or not ([`held`]): a heard
    /// line never goes untracked for a panic elsewhere.
    fn say_mark<R>(&self, f: impl FnOnce(&mut SayLog) -> R) -> Option<R> {
        Some(f(&mut held(self.saylog.as_ref()?)))
    }

    /// One transcript line and what became of it: `sent` or `dropped:<why>` in `flow.log`, and the
    /// same fate in the log window (#0423). Every heard line's verdict comes through here, so the
    /// window and `flow.log` cannot tell two stories. A line that is no phrase (no side or text) is
    /// noise and the window shows nothing of it. Returns the window's id for the phrase — a let-through
    /// one is queued under it ([`Ears::send_line`]), so its mark follows the wire's word — or `None`
    /// when the window has no entry for it.
    fn fate(&self, line: &str, fate: &Heard) -> Option<u64> {
        let line = line.trim_end_matches(['\n', '\r']);
        flow::record(
            flow::Dir::Note,
            None,
            flow::Kind::Ears,
            line.as_bytes(),
            &[("outcome", &fate.outcome())],
        );
        self.saylog.as_ref()?;
        let now = SystemTime::now();
        let (side, text) = transcript::phrase(line, now)?;
        let mode = self.mode().id();
        self.say_mark(|log| log.heard(now, side.id(), mode, &text, fate.clone()))
    }

    /// Adds the keep-awake's state to every `ears.status` answer, as `keepawake` (e.g. `"on (always)"`):
    /// `ears.status` is the agent's one way to read the Mac's state over the link.
    pub fn report_keepawake(&self, status: KeepAwakeStatus) {
        if let Ok(mut slot) = self.keepawake.lock() {
            *slot = Some(status);
        }
    }

    /// Keeps the chosen mode across restarts in `path`, one line, the mode's id
    /// ([`mode_file_beside`] the config): a switch, `ears.stop` and Pause save it,
    /// [`Ears::restore_mode`] starts in it. Quit and a capture that ends on its
    /// own leave it as it is.
    #[must_use]
    pub fn with_mode_file(mut self, path: PathBuf) -> Self {
        self.mode_file = Mutex::new(Some(path));
        self
    }

    /// At start: switches to the mode the last run saved
    /// ([`Ears::with_mode_file`]) when `[ears]` is on and that mode has a
    /// capture command, logged as `ears: mode <id> restored after restart`.
    /// A missing, unreadable or unknown file, a saved `off`, or a mode no
    /// longer configured leave the ears off, as every start did before.
    pub fn restore_mode(self: &Arc<Self>) {
        let file = self.mode_file.lock().ok().and_then(|file| file.clone());
        let saved = file.as_deref().and_then(saved_mode::load);
        let (Some(mode), Ok(settings)) = (saved, &self.settings) else {
            // Said, not silent: an ears that come back off after a restart show why in the log.
            if let (Some(path), Ok(_)) = (file.as_deref(), &self.settings) {
                self.journal.record(&format!(
                    "ears: no saved mode at {} (missing, unreadable or unknown); ears start off",
                    path.display()
                ));
            }
            return;
        };
        if mode == EarsMode::Off {
            return;
        }
        if settings.mode_argv(mode).is_none() {
            self.journal.record(&format!(
                "ears: saved mode {} not restored: not configured",
                mode.id()
            ));
            return;
        }
        match self.set_mode(mode, ModeSource::Restore) {
            Ok(_) => self
                .journal
                .record(&format!("ears: mode {} restored after restart", mode.id())),
            Err(why) => self
                .journal
                .record(&format!("ears: mode {} not restored ({why})", mode.id())),
        }
    }

    /// Keeps `mode` for the next start, when a mode file is set and `[ears]` is
    /// on. A mode other than off is not kept while paused: the pause won. A
    /// failure is logged; the next start then goes by the file as it was.
    fn save_mode(&self, mode: EarsMode) {
        if self.settings.is_err() || (mode != EarsMode::Off && self.paused.is_set()) {
            return;
        }
        let Ok(file) = self.mode_file.lock() else {
            return;
        };
        let Some(path) = file.as_deref() else {
            return;
        };
        if let Err(error) = saved_mode::save(path, mode) {
            self.journal.record(&format!(
                "ears: mode {} not kept for the next start ({error})",
                mode.id()
            ));
        }
    }

    /// Builds the organ from local config; the capture's log is `ears.log`
    /// next to the activity log.
    #[must_use]
    pub fn build(
        config: &TrayAppConfig,
        paused: Arc<Stop>,
        outbox: Arc<Outbox>,
        journal: Arc<dyn Journal>,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let home = crate::config::expand_tilde("~");
        let log_file = config.log_path().with_file_name("ears.log");
        let settings = EarsSettings::from_config(config, &home, log_file);
        match &settings {
            Ok(s) => journal.record(&format!("ears: ready, command {:?}", s.argv)),
            Err(why) => journal.record(&format!("{why}; refusing every ears/voice verb")),
        }
        if let (Ok(_), Some(warning)) = (&settings, command_overridden(&config.ears, &home)) {
            journal.record(&warning);
        }
        startup_notes(config, &settings, &outbox, journal.as_ref());
        Self::new(settings, paused, outbox, journal, changed)
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> EarsState {
        self.inner
            .lock()
            .map_or(EarsState::Off, |inner| inner.state.clone())
    }

    /// The menu line, `paused` taken into account.
    #[must_use]
    pub fn line(&self, paused: bool) -> String {
        if self.settings.is_err() {
            return "ears: off".to_owned();
        }
        match (self.state(), paused) {
            (EarsState::Off, true) => "ears: paused".to_owned(),
            (EarsState::Off, false) if self.off_by_pause() => {
                format!("ears: {EARS_OFF_AFTER_PAUSE}")
            }
            (EarsState::Off, false) => "ears: ready (not listening)".to_owned(),
            (EarsState::Running { pid, .. }, _) => format!("ears: listening (pid {pid})"),
            (EarsState::Degraded { side, reason, .. }, _) => format!(
                "ears: DEGRADED — {}: {reason} ({} still heard) — fix in {} (click to open)",
                side.id(),
                side.other().id(),
                side.settings_pane()
            ),
            (EarsState::Deaf { reason, .. }, _) => format!("ears: DEAF — {reason}"),
            (EarsState::Exited { code }, _) => code.map_or_else(
                || "ears: capture killed".to_owned(),
                |code| format!("ears: capture exited ({code})"),
            ),
        }
    }

    /// What a click on the menu's ears line opens: while one side is
    /// DEGRADED, the URL of that side's Settings pane ([`Side::settings_url`]);
    /// otherwise nothing.
    #[must_use]
    pub fn fix_url(&self) -> Option<&'static str> {
        if let EarsState::Degraded { side, .. } = self.state() {
            Some(side.settings_url())
        } else {
            None
        }
    }

    /// Answers one request line with one reply line (newline-terminated).
    #[must_use]
    pub fn handle(self: &Arc<Self>, line: &[u8]) -> Vec<u8> {
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            self.journal.record("ears: unparseable line refused");
            return refuse_line(line);
        };
        if request.id.is_empty() || request.id.len() > 64 {
            self.journal.record("ears: bad id refused");
            return refuse_line(line);
        }
        let arg = request.arg.as_deref().unwrap_or("");
        let done = self.perform(&request.id, &request.verb, arg);
        let result = match &done {
            Ok(_) => "ok".to_owned(),
            Err(why) => format!("refused ({why})"),
        };
        // The spoken text is a payload: only its length reaches the log.
        self.journal.record(&format!(
            "ears: id={} verb={} arg_chars={} result={result}",
            request.id,
            request.verb,
            arg.chars().count()
        ));
        let value = match done {
            Ok(result) => json!({"id": request.id, "ok": true, "result": result}),
            Err(err) => json!({"id": request.id, "ok": false, "err": err}),
        };
        let mut reply = value.to_string().into_bytes();
        reply.push(b'\n');
        reply
    }

    fn perform(self: &Arc<Self>, id: &str, verb: &str, arg: &str) -> Result<Value, String> {
        let settings = self.settings.as_ref().map_err(|why| {
            if verb == "voice.say" {
                flow_voice(
                    Some(id),
                    "refused:not-configured",
                    arg,
                    &[("detail", why.as_str())],
                );
            }
            why.clone()
        })?;
        match verb {
            "ears.start" if self.state().pid().is_some() => {
                self.start(settings, EarsMode::Call).map(Value::from)
            }
            "ears.start" => self
                .set_mode(EarsMode::Call, ModeSource::Request(id.to_owned()))
                .map(Value::from),
            "ears.stop" => {
                let switching = self.switching.lock();
                let was = self.mode();
                let stopped = self.stop_with("ears.stop", None, End::Stop).is_some();
                self.save_mode(EarsMode::Off);
                if was != EarsMode::Off {
                    self.mode_event(EarsMode::Off, was, &ModeSource::Request(id.to_owned()));
                }
                drop(switching);
                Ok(Value::from(if stopped { "stopped" } else { "not running" }))
            }
            "ears.mode" => {
                let mode = EarsMode::from_id(arg).ok_or_else(|| "arg".to_owned())?;
                self.set_mode(mode, ModeSource::Request(id.to_owned()))
                    .map(Value::from)
            }
            "ears.status" => {
                let state = self.state();
                let mut status = state.to_json();
                if let Some(object) = status.as_object_mut() {
                    object.insert("mode".to_owned(), Value::from(self.mode().id()));
                    let keepawake = self
                        .keepawake
                        .lock()
                        .ok()
                        .and_then(|slot| slot.as_ref().map(|status| status()));
                    if let Some(keepawake) = keepawake {
                        object.insert("keepawake".to_owned(), Value::from(keepawake));
                    }
                    if let Some(pid) = state.pid() {
                        object.insert("alive".to_owned(), Value::Bool(pid_alive(pid)));
                        let last_line = self.last_line.lock().ok().and_then(|slot| *slot);
                        let utc =
                            last_line.map_or(Value::Null, |at| json!(crate::log::format_utc(at)));
                        let age = last_line.map_or(Value::Null, |at| {
                            json!(
                                SystemTime::now()
                                    .duration_since(at)
                                    .unwrap_or(Duration::ZERO)
                                    .as_secs()
                            )
                        });
                        object.insert("last_line_utc".to_owned(), utc);
                        object.insert("last_line_age_s".to_owned(), age);
                    }
                }
                Ok(status)
            }
            "ears.test" => {
                let event = test_line_event(arg, self.mode(), SystemTime::now())
                    .ok_or_else(|| "arg".to_owned())?;
                self.send_line(&event, None);
                Ok(Value::from("sent"))
            }
            "voice.say" => self.say_as(Some(id), settings, arg).map(Value::from),
            _ => Err("verb".to_owned()),
        }
    }

    /// Whether `[ears]` is on and usable (the menu shows the mode picker).
    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.settings.is_ok()
    }

    /// The microphone picked, by name; `None`: not picked (`EARS_MIC` as `env` has it).
    #[must_use]
    pub fn mic(&self) -> Option<String> {
        self.picked.lock().ok().and_then(|p| p.mic.clone())
    }

    /// The voice output picked, by name; `None`: the system default output.
    #[must_use]
    pub fn output(&self) -> Option<String> {
        self.picked.lock().ok().and_then(|p| p.output.clone())
    }

    /// Picks the capture's microphone (`None`: unpicked): the next start gets
    /// [`MIC_ENV`]; a running capture is restarted in its mode at once (its
    /// self-check then judges the new microphone). Returns what happened; a
    /// capture that did not start again is said there (and the link hears it
    /// went off), the pick stands.
    ///
    /// # Errors
    ///
    /// The ears are off in config, or the name is unusable or one the
    /// capture's ffmpeg would misread ([`crate::devices::check_mic_name`]);
    /// nothing changes.
    pub fn set_mic(self: &Arc<Self>, name: Option<String>) -> Result<String, String> {
        let settings = self.settings.as_ref().map_err(Clone::clone)?;
        if let Some(name) = &name {
            crate::devices::check_name(name)?;
            crate::devices::check_mic_name(name)?;
        }
        let shown = name.clone().unwrap_or_else(|| "unpicked".to_owned());
        // The current one picked again (its menu item clicked twice): the capture is left alone.
        if self.mic() == name {
            return Ok(format!("mic {shown} already"));
        }
        if let Ok(mut picked) = self.picked.lock() {
            picked.mic = name;
        }
        let switching = self.switching.lock();
        let mode = self.mode();
        if mode == EarsMode::Off {
            return Ok(format!("mic {shown} from the next start"));
        }
        let _stopped: Option<u32> = self.stop_with(&format!("mic → {shown}"), None, End::Stop);
        let restarted = self.start(settings, mode);
        drop(switching);
        match restarted {
            Ok(started) => Ok(format!(
                "mic {shown}: capture restarted in {} ({started})",
                mode.id()
            )),
            Err(why) => {
                self.mode_event(EarsMode::Off, mode, &ModeSource::Mic);
                (self.changed)();
                Ok(format!(
                    "mic {shown} kept, but the capture did not start again: {why}"
                ))
            }
        }
    }

    /// Picks the voice output (`None`: the system default): `say` gets `-a
    /// <name>` from the next sentence, a resident that plays itself is started
    /// again with `--device <name>` on its next text. Returns what happened.
    ///
    /// # Errors
    ///
    /// The ears are off in config or the name is unusable; nothing changes.
    pub fn set_output(&self, name: Option<String>) -> Result<String, String> {
        self.settings.as_ref().map_err(Clone::clone)?;
        if let Some(name) = &name {
            crate::devices::check_name(name)?;
        }
        let shown = name
            .clone()
            .unwrap_or_else(|| "the system default".to_owned());
        if let Ok(mut picked) = self.picked.lock() {
            picked.output = name;
        }
        Ok(format!(
            "voice output {shown} from the next sentence (synthesized WAVs keep the system default)"
        ))
    }

    /// The current mode (off when no capture runs).
    #[must_use]
    pub fn mode(&self) -> EarsMode {
        self.inner
            .lock()
            .map_or(EarsMode::Off, |inner| inner.current_mode())
    }

    /// Tells the hub the current mode unprompted, off included:
    /// `{"event":"ears","state":"mode","mode":<id>,"from":<the same id>,"source":"link-up"}`. Called on every
    /// accepted hello, so a hub that restarted or a link that was down knows the mode without a switch.
    ///
    /// Read and pushed under the state's lock: every switch changes the state before it pushes its own mode
    /// event, so that event either comes after this line or this line already carries its mode. Lines queued while
    /// the link was down are older and go out before it. Either way the last mode line the hub reads is the mode.
    pub fn link_up(&self) {
        if let Ok(inner) = self.inner.lock() {
            let mode = inner.current_mode();
            self.mode_event(mode, mode, &ModeSource::LinkUp);
        }
    }

    /// Switches to `mode`: the running capture is told to stop (it writes what
    /// it holds on its way out; killed only if it has not gone after the grace), then the
    /// mode's own capture starts (off: none). The link hears
    /// `{"event":"ears","state":"mode","mode":<id>,"from":<id>,"source":<who>}` ([`ModeSource`]).
    ///
    /// # Errors
    ///
    /// Refused when the ears are off in config, paused, the mode has no
    /// capture command, or the capture can't start; the mode stays as it was.
    pub fn set_mode(
        self: &Arc<Self>,
        mode: EarsMode,
        source: ModeSource,
    ) -> Result<String, String> {
        let settings = self.settings.as_ref().map_err(Clone::clone)?;
        let switching = self.switching.lock();
        if self.paused.is_set() {
            self.journal
                .record(&format!("ears: mode {} refused: paused", mode.id()));
            return Err("paused".to_owned());
        }
        if mode != EarsMode::Off && settings.mode_argv(mode).is_none() {
            self.journal
                .record(&format!("ears: mode {} refused: not configured", mode.id()));
            return Err(format!(
                "mode {} is not configured ([ears.modes].{})",
                mode.id(),
                mode.id()
            ));
        }
        let from = self.mode();
        if from == mode && (mode == EarsMode::Off || self.state().pid().is_some()) {
            self.save_mode(mode);
            return Ok(format!("already {}", mode.id()));
        }
        let _stopped: Option<u32> = self.stop_with(
            &format!("mode {} → {}", from.id(), mode.id()),
            None,
            End::Stop,
        );
        let started_at = SystemTime::now();
        let said = if mode == EarsMode::Off {
            "off".to_owned()
        } else {
            self.start(settings, mode)?
        };
        // A switch made after a Pause is the one the resume event names: the ears are no longer held off. A refused
        // or failed one leaves the hold.
        self.off_by_pause.store(false, Ordering::SeqCst);
        self.journal.record(&format!(
            "ears: mode {} → {} ({})",
            from.id(),
            mode.id(),
            source.id()
        ));
        self.save_mode(mode);
        self.mode_event(mode, from, &source);
        (self.changed)();
        if mode != EarsMode::Off {
            // The switch's own check: the new capture makes chunks on both sides, or the agent hears it failed.
            let ended = self.inner.lock().map(|i| Arc::clone(&i.ended));
            if let Ok(ended) = ended {
                let checker = Arc::clone(self);
                thread::spawn(move || {
                    checker.check_switch(&ended, mode, from, &source, started_at);
                });
            }
        }
        drop(switching);
        Ok(said)
    }

    /// Waits up to `switch_within` for a fresh chunk on both sides after a switch, then tells the link
    /// `{"event":"ears","state":"switched","mode","from","source","check":"ok"|"degraded"|"failed","side"?,"reason"?}`:
    /// `degraded` names the one silent side, `failed` means neither side made a chunk. `ended` ends the wait
    /// silently, at once: a newer start or stop in the meantime, or the capture exiting on its own mid-check. That
    /// exit is not a failed switch: it is told on its own, as the state `exited` (`wait_for`).
    fn check_switch(
        &self,
        ended: &Stop,
        mode: EarsMode,
        from: EarsMode,
        source: &ModeSource,
        started_at: SystemTime,
    ) {
        let Ok(settings) = self.settings.as_ref() else {
            return;
        };
        let fresh = |dir: &Path| own_change(&look_dir(dir), started_at).is_some();
        let begun = std::time::Instant::now();
        loop {
            if ended.is_set() {
                return;
            }
            let (me, them) = (fresh(&settings.me_dir), fresh(&settings.them_dir));
            let verdict = if me && them {
                Some((
                    format!("ears: switch to {} checked: both sides capture", mode.id()),
                    json!({"event": "ears", "state": "switched", "mode": mode.id(), "from": from.id(), "source": source.id(), "check": "ok"}),
                ))
            } else if begun.elapsed() >= self.switch_within {
                // One silent side does not fail the switch: the other is heard (the capture keeps running).
                let silent = match (me, them) {
                    (false, true) => Some((Side::Me, "no chunk from the microphone")),
                    (true, false) => Some((Side::Them, "no chunk from the system audio")),
                    _ => None,
                };
                Some(silent.map_or_else(
                    || {
                        let reason = "no chunk on either side";
                        (
                            format!("ears: switch to {} FAILED its check: {reason}", mode.id()),
                            json!({"event": "ears", "state": "switched", "mode": mode.id(), "from": from.id(), "source": source.id(), "check": "failed", "reason": reason}),
                        )
                    },
                    |(side, reason)| {
                        (
                            format!("ears: switch to {} DEGRADED: {reason}", mode.id()),
                            json!({"event": "ears", "state": "switched", "mode": mode.id(), "from": from.id(), "source": source.id(), "check": "degraded", "side": side.id(), "reason": reason, "fix": side.settings_pane()}),
                        )
                    },
                ))
            } else {
                None
            };
            if let Some((line, event)) = verdict {
                self.say_if_current(ended, &line, &event);
                return;
            }
            // The chunk files are looked at on this tick: they have no event here (kqueue on the side
            // directories, or chunks over a channel from the Rust ears, are the poll inventory's later
            // steps). The end of this start wakes the wait at once.
            if ended.wait_until(Instant::now().checked_add(self.check_tick)) {
                return;
            }
        }
    }

    /// Logs `line` and pushes `event` for a start that is still current: `ended` is looked at under the lock it is
    /// set under (the next start, a stop, the capture's own exit), and the push happens under it too, so a start
    /// that ends while its check was looking is never reported on.
    fn say_if_current(&self, ended: &Stop, line: &str, event: &Value) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ended.is_set() {
            return;
        }
        self.journal.record(line);
        self.push_event(event);
        drop(inner);
    }

    fn mode_event(&self, mode: EarsMode, from: EarsMode, source: &ModeSource) {
        self.push_event(&json!({"event": "ears", "state": "mode", "mode": mode.id(), "from": from.id(), "source": source.id()}));
    }

    /// Starts tailing the transcript files, once, when a line could ever go
    /// ([`EarsSettings::may_send_lines`]); the thread ends with the organ.
    fn tail_transcripts(self: &Arc<Self>, settings: &EarsSettings) {
        if !settings.may_send_lines() || self.tailing.swap(true, Ordering::SeqCst) {
            return;
        }
        let tail = Tail::new(&settings.transcript_files);
        let ears = Arc::downgrade(self);
        let every = self.tail_every;
        self.journal.record(&format!(
            "ears: tailing {} transcript file(s); the microphone's lines go, the system audio's only in a call",
            settings.transcript_files.len(),
        ));
        thread::spawn(move || Self::line_loop(&ears, tail, every));
    }

    /// The running capture's mode, start generation, and start time; `None`
    /// when none runs.
    /// The mode and start time of the capture of `generation`, if it was told to stop and its last lines are
    /// still taken ([`Inner::closing`]).
    fn closing(&self, generation: u64) -> Option<(EarsMode, SystemTime)> {
        let inner = self.inner.lock().ok()?;
        let closing = inner
            .closing
            .iter()
            .find(|c| c.generation == generation)
            .map(|c| (c.mode, c.since));
        drop(inner);
        closing
    }

    /// A capture has no more lines (its own `read` flag is set already: its stdout ended, or was cut): its closing
    /// entry, if it has one, goes once the capture is reaped too ([`Closing::done`]). Every entry is looked at, so
    /// none is named.
    fn read_out(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.closing.retain(|c| !c.done());
    }

    fn capture_now(&self) -> Option<(EarsMode, u64, SystemTime)> {
        let inner = self.inner.lock().ok()?;
        let since = inner.state.since()?;
        Some((inner.mode, inner.generation, since))
    }

    /// Every `every`, the transcripts' new lines go through
    /// [`transcript::line_event`] and the events of those it lets go to the
    /// link, while a capture runs and the tray is not paused; lines read
    /// otherwise are dropped, never sent later. The log keeps no text.
    fn line_loop(ears: &std::sync::Weak<Self>, mut tail: Tail, every: Duration) {
        loop {
            thread::sleep(every);
            let Some(ears) = ears.upgrade() else {
                return;
            };
            let lines = tail.poll();
            let capture = if ears.paused.is_set() {
                None
            } else {
                ears.capture_now()
            };
            let now = SystemTime::now();
            let Some((mode, _, since)) = capture else {
                let why = if ears.paused.is_set() {
                    Heard::Dropped("paused".into())
                } else {
                    Heard::Dropped("no-capture".into())
                };
                for line in &lines {
                    ears.fate(line, &why);
                }
                continue;
            };
            let events: Vec<(Value, Option<u64>)> = lines
                .iter()
                .filter_map(|line| match line_verdict(line, mode, now, since) {
                    Ok(event) => {
                        let id = ears.fate(line, &LET_THROUGH);
                        Some((event, id))
                    }
                    Err(why) => {
                        ears.fate(line, &Heard::Dropped(why.into()));
                        None
                    }
                })
                .collect();
            for (event, id) in &events {
                ears.send_line(event, *id);
            }
            if !events.is_empty() {
                ears.mark_line_published(now);
            }
        }
    }

    /// Records that a real transcript line was published at `at`
    /// ([`LastLine`]); `ears.status` turns it into the age it reports.
    fn mark_line_published(&self, at: SystemTime) {
        if let Ok(mut slot) = self.last_line.lock() {
            *slot = Some(at);
        }
    }

    /// Reads the capture's stdout (#0393: no files in the ears' path): every
    /// line is teed to the side log (`ears.log` — a log, never a path the
    /// phrases travel by), and a [`LINE_MARKER`] record — one per recognised
    /// phrase, from a capture that answers [`LINES_STDOUT_ENV`] — goes through
    /// [`line_event`] under the same guards as the tailed transcript files,
    /// plus the tray's own echo gate: a microphone phrase whose utterance span
    /// meets a remembered playback window ([`Speaking`]) is the room hearing
    /// our own voice, not him, and never leaves the Mac. Reading ends with the
    /// pipe, or at the first line after the stream is cut ([`Ears::read_grace`]):
    /// a cut stream's thread stays parked in its read until whatever still holds
    /// the pipe writes or lets it go. A capture that was replaced speaks to a
    /// generation no longer current, and its records are dropped.
    fn read_markers(
        self: &Arc<Self>,
        stdout: std::process::ChildStdout,
        log: File,
        generation: u64,
        read: &Stop,
    ) {
        let mut log = log;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    // The end of this capture's stdout, whatever became of the capture (stopped, paused,
                    // exited by itself): said on its own flag first, then to its closing entry if it has one.
                    read.set();
                    self.read_out();
                    return;
                }
                // Set by someone else: the stream was cut ([`Ears::read_grace`]: the capture is gone, and
                // whatever still holds its stdout is not the capture). The pipe is let go here.
                Ok(_) if read.is_set() => return,
                Ok(_) => {}
            }
            let _kept = log.write_all(line.as_bytes());
            // A phrase the capture itself dropped (prompt echo, our own voice, nothing read):
            // only flow.log hears of it, with the capture's reason.
            if let Some((why, dropped)) = transcript::dropped_line(&line) {
                self.fate(dropped, &Heard::Dropped(why.into()));
                continue;
            }
            let Some(marked) = marked_line(&line) else {
                continue;
            };
            if self.paused.is_set() {
                self.fate(marked.line, &Heard::Dropped("paused".into()));
                continue;
            }
            // The current capture's line, or the last lines of one that was told to stop and is still writing
            // what it held.
            let (mode, since) = if let Some(closing) = self.closing(generation) {
                closing
            } else {
                let Some((mode, current, since)) = self.capture_now() else {
                    self.fate(marked.line, &Heard::Dropped("no-capture".into()));
                    continue;
                };
                if current != generation {
                    self.fate(marked.line, &Heard::Dropped("replaced-capture".into()));
                    continue;
                }
                (mode, since)
            };
            let now = SystemTime::now();
            if marked.side == Side::Me {
                let epoch = now_ms();
                let (t0, t1) = if marked.t0 == 0 && marked.t1 == 0 {
                    // The capture could not give the span: the phrase ended
                    // around now, so look back one utterance's worth.
                    (epoch.saturating_sub(8_000), epoch)
                } else {
                    (marked.t0, marked.t1)
                };
                if self.speaking.echoes(t0, t1, epoch) {
                    self.fate(marked.line, &Heard::Dropped("echo".into()));
                    self.journal.record(
                        "ears: one microphone phrase dropped as the room's echo of our own playback",
                    );
                    continue;
                }
            }
            let mut event = match line_verdict(marked.line, mode, now, since) {
                Ok(event) => event,
                Err(why) => {
                    self.fate(marked.line, &Heard::Dropped(why.into()));
                    continue;
                }
            };
            let id = self.fate(marked.line, &LET_THROUGH);
            // The phrase's word timings (tags inside the stream, 2026-11-18) go with it
            // when the record carries a whole phrase's worth — one bad record loses all
            // of them, never half ([`MarkedLine::words`]).
            if let Some(words) = marked.words()
                && let Some(object) = event.as_object_mut()
            {
                object.insert("words".to_owned(), Value::Array(words));
            }
            self.send_line(&event, id);
            // #0387's evidence stays true on the no-files path too: a
            // phrase that left here is a real transcript line published.
            self.mark_line_published(now);
        }
    }

    /// Sends one transcript-line event to the link: a real line's or a test's
    /// (`ears.test`). The log keeps its side, mode, whether it went as a
    /// continuation, and its length, never its text. A phrase the window shows goes under its `id`
    /// ([`Ears::fate`]), so the window hears the wire's word for it ([`Outbox::push_heard`]).
    fn send_line(&self, event: &Value, id: Option<u64>) {
        let field = |name: &str| event.get(name).and_then(Value::as_str);
        self.journal.record(&format!(
            "ears: one {}transcript line sent ({}, mode {}, {} characters)",
            if event.get("test") == Some(&Value::Bool(true)) {
                "test "
            } else {
                ""
            },
            field("side").unwrap_or("?"),
            field("mode").unwrap_or("?"),
            field("text").map_or(0, |text| text.chars().count())
        ));
        let line = event.to_string().into_bytes();
        match id {
            Some(id) => self.outbox.push_heard(id, line),
            None => self.outbox.push(line),
        }
    }

    /// The tray's Pause (and a start it overtook, `capture.rs`): kills a running capture (it is not restarted on
    /// resume, [`Ears::off_by_pause`]) and tells the link; off is kept for the next start.
    ///
    /// A kill, not a stop, on purpose: Pause means the microphone closes NOW. A capture told to stop keeps
    /// listening while it finishes its phrase; a paused one must not. The turn it held is lost, and that is the
    /// right cost (its lines would be dropped as `paused` anyway). Do not make Pause flush.
    pub fn pause(self: &Arc<Self>) {
        let from = self.mode();
        if self
            .stop_with("paused", Some("paused"), End::Kill)
            .is_some()
            && from != EarsMode::Off
        {
            self.off_by_pause.store(true, Ordering::SeqCst);
            self.mode_event(EarsMode::Off, from, &ModeSource::Pause);
        }
        // And every capture that was told to stop and is still on its way out: it holds the microphone too.
        self.kill_closing();
        self.save_mode(EarsMode::Off);
    }

    /// Whether the last Pause stopped a capture and no mode was set since: after Resume the ears stay off
    /// until `ears.mode` or the menu turns them on ([`EARS_OFF_AFTER_PAUSE`]).
    #[must_use]
    pub fn off_by_pause(&self) -> bool {
        self.off_by_pause.load(Ordering::SeqCst)
    }

    /// The tray's Quit and the app's exit: stops a running capture and waits until it and every capture still
    /// closing are gone, or given up ([`Ears::see_closing_off`]): nothing of the tray is left afterwards to do the
    /// kill.
    pub fn shutdown(self: &Arc<Self>) {
        // The running capture is told to stop like any other, and then every capture on its way out is seen
        // off together, under one grace: their stop threads die with the tray.
        let _stopped: Option<u32> = self.stop_with("quit", None, End::Stop);
        self.see_closing_off();
        let mut retired = Vec::new();
        if let Ok(mut slot) = self.resident.lock() {
            retire(&mut slot, &mut retired);
        }
        // Reaped with the lock let go.
        drop(retired);
    }

    /// `voice.say`: with an own voice configured, the text is cut into sentences and each next one
    /// renders while the current one plays (streamed speech); only the first
    /// waits. Without one, macOS `say` speaks the whole text.
    #[cfg(test)]
    fn say(&self, settings: &EarsSettings, text: &str) -> Result<String, String> {
        self.say_as(None, settings, text)
    }

    /// [`Self::say`] for the request `id`, each step logged to `flow.log` ([`flow_voice`]).
    fn say_as(
        &self,
        id: Option<&str>,
        settings: &EarsSettings,
        text: &str,
    ) -> Result<String, String> {
        // Refused before anything is rendered or played: logged as such, never as queued. The text is
        // cleaned once, here: what is spoken and what the window logs are this one result.
        let cleaned = if self.paused.is_set() {
            Err(("paused", "paused".to_owned()))
        } else {
            clean_voice_text(text).map_err(|why| ("text", why))
        };
        let full = match cleaned {
            Ok(full) => full,
            Err((word, why)) => {
                // Counted in flight all the same: the last say to end, refused or not, closes the
                // microphone's utterance, as before.
                let _in_flight = SayInFlight::start(self, settings);
                flow_voice(id, &format!("refused:{word}"), text, &[("detail", &why)]);
                return Err(why);
            }
        };
        flow_voice(id, "queued", text, &[]);
        let said = self.say_steps(id, settings, &full);
        match &said {
            Ok(_) => {
                flow_voice(id, "ended", text, &[]);
                // #0423: the window logs what was said, once it was; a refused or failed say said nothing.
                let now = SystemTime::now();
                self.say_mark(|log| log.said(now, &full));
            }
            Err(why) => flow_voice(id, &format!("failed:{why}"), text, &[]),
        }
        said
    }

    /// Speaks `full`, the text [`clean_voice_text`] already cleaned ([`Ears::say`]).
    fn say_steps(
        &self,
        id: Option<&str>,
        settings: &EarsSettings,
        full: &str,
    ) -> Result<String, String> {
        let _in_flight = SayInFlight::start(self, settings);
        if self.paused.is_set() {
            return Err("paused".to_owned());
        }
        let full = full.to_owned();
        if settings.voice_synth.is_empty() && settings.voice_serve.is_empty() {
            wait_for_speaker(&self.paused);
            flow_voice(id, "played", &full, &[("by", "say")]);
            return self
                .play(settings, &Clip::Say(full))
                .map(|()| "said".to_owned());
        }
        if settings.voice_serve_plays && !settings.voice_serve.is_empty() {
            wait_for_speaker(&self.paused);
            flow_voice(id, "synth", &full, &[("by", "resident")]);
            match self.speak_streamed(id, settings, &full) {
                (_, Ok(())) => return Ok("said".to_owned()),
                (true, Err(why)) => return Err(why),
                (false, Err(why)) => {
                    flow_voice(
                        id,
                        "fallback:resident-failed",
                        &full,
                        &[("detail", &why), ("then", "sentences")],
                    );
                    self.journal.record(&format!(
                        "voice: streamed voice failed before a sound ({why}); sentence by sentence instead"
                    ));
                }
            }
        }
        let pieces = split_sentences(&full);
        let stop = AtomicBool::new(false);
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<(String, Result<PathBuf, String>, SystemTime)>(1);
        thread::scope(|scope| {
            let (pieces, stop) = (&pieces, &stop);
            scope.spawn(move || {
                for piece in pieces {
                    if stop.load(Ordering::SeqCst) || self.paused.is_set() {
                        break;
                    }
                    let started = SystemTime::now();
                    flow_voice(id, "synth", piece, &[]);
                    let wav = self.render(settings, piece);
                    if let Err(std::sync::mpsc::SendError((_, Ok(wav), started))) =
                        tx.send((piece.clone(), wav, started))
                    {
                        forget_clip(&wav, started);
                        break;
                    }
                }
            });
            let mut first = true;
            let mut result = Ok(());
            for (piece, wav, started) in rx {
                if result.is_err() || self.paused.is_set() {
                    if let Ok(wav) = wav {
                        forget_clip(&wav, started);
                    }
                    continue;
                }
                if first {
                    wait_for_speaker(&self.paused);
                    first = false;
                }
                let clip = match wav {
                    Ok(wav) => Clip::Wav(wav, started),
                    Err(why) => {
                        self.journal.record(&format!(
                            "voice: own voice failed ({why}); say -v {} for this sentence",
                            settings.voice
                        ));
                        Clip::Say(piece.clone())
                    }
                };
                flow_voice(
                    id,
                    "played",
                    &piece,
                    &[(
                        "by",
                        if matches!(clip, Clip::Say(_)) {
                            "say"
                        } else {
                            "own voice"
                        },
                    )],
                );
                if let Err(why) = self.play(settings, &clip) {
                    stop.store(true, Ordering::SeqCst);
                    result = Err(why);
                }
            }
            if self.paused.is_set() && result.is_ok() {
                result = Err("paused".to_owned());
            }
            result.map(|()| "said".to_owned())
        })
    }

    /// Our playback ended: an empty `.close` beside the microphone's utterances ([`EarsSettings::me_close_file`])
    /// tells its cutter to close the one in progress. Only into a `me` dir that exists (a capture is running
    /// or ran); written under a temporary name and renamed. A failure is a journal line, never the say's.
    fn close_me(&self, settings: &EarsSettings) {
        let close = settings.me_close_file();
        let Some(me) = close.parent().filter(|me| me.is_dir()) else {
            return;
        };
        let tmp = me.join(".close.tray.part");
        if let Err(e) = fs::write(&tmp, b"").and_then(|()| fs::rename(&tmp, &close)) {
            drop(fs::remove_file(&tmp));
            self.journal
                .record(&format!("voice: {} not written: {e}", close.display()));
        }
    }

    /// One sentence in the own voice: the resident synth when configured (started on demand, dropped
    /// on any failure), else — or after it fails — the one-shot `voice_synth`.
    fn render(&self, settings: &EarsSettings, text: &str) -> Result<PathBuf, String> {
        let mut why = None;
        // A playing resident answers events, not WAV paths: it is never asked for one clip.
        if !settings.voice_serve.is_empty() && !settings.voice_serve_plays {
            let clean = clean_voice_text(text)?;
            let mut retired = Vec::new();
            let answer = self
                .resident
                .lock()
                .map_err(|_| "voice: lock".to_owned())
                .and_then(|mut slot| {
                    let argv = settings.serve_argv(self.output().as_deref());
                    // A resident that left by itself (idle, to free its memory) is started again.
                    if slot.as_mut().is_some_and(|r| !r.alive() || !r.runs(&argv)) {
                        retire(&mut slot, &mut retired);
                    }
                    if slot.is_none() {
                        *slot = Some(Resident::spawn(
                            &argv,
                            settings.voice_cwd.as_deref(),
                            &settings.home,
                        )?);
                    }
                    let resident = slot
                        .as_mut()
                        .ok_or_else(|| "voice: no resident".to_owned())?;
                    let limit = if resident.warm() {
                        VOICE_SYNTH_LIMIT
                    } else {
                        VOICE_SERVE_FIRST_LIMIT
                    };
                    let answer = resident.render(&clean, limit, &self.paused);
                    if answer.is_err() {
                        retire(&mut slot, &mut retired);
                    }
                    answer
                });
            // Reaped with the lock let go.
            drop(retired);
            match answer {
                Ok(wav) => return Ok(self.post(settings, wav)),
                Err(e) => why = Some(e),
            }
        }
        if settings.voice_synth.is_empty() {
            return Err(why.unwrap_or_else(|| "no own voice".to_owned()));
        }
        if let Some(why) = why {
            self.journal.record(&format!(
                "voice: resident synth failed ({why}); one-shot synth"
            ));
        }
        self.synthesize(settings, text)
    }

    /// `voice_serve_plays`: the resident renders and plays the whole text itself; each piece's start
    /// and end open and close the in-memory playback windows ([`Speaking`]). Any failure drops (kills the group of) the resident. Returns
    /// whether anything sounded, with the result.
    fn speak_streamed(
        &self,
        id: Option<&str>,
        settings: &EarsSettings,
        text: &str,
    ) -> (bool, Result<(), String>) {
        // Declared before the lock's guard, so dropped after it on every return: a resident retired here is
        // reaped (a wait) with the lock let go.
        let mut retired = Vec::new();
        let Ok(mut slot) = self.resident.lock() else {
            return (false, Err("voice: lock".to_owned()));
        };
        // Started with another output device (a new pick): started again with this one.
        let argv = settings.serve_argv(self.output().as_deref());
        if slot.as_mut().is_some_and(|r| !r.alive() || !r.runs(&argv)) {
            retire(&mut slot, &mut retired);
        }
        if slot.is_none() {
            match Resident::spawn(&argv, settings.voice_cwd.as_deref(), &settings.home) {
                Ok(r) => *slot = Some(r),
                Err(why) => return (false, Err(why)),
            }
        }
        let Some(resident) = slot.as_mut() else {
            return (false, Err("voice: no resident".to_owned()));
        };
        let first = if resident.warm() {
            VOICE_SYNTH_LIMIT
        } else {
            VOICE_SERVE_FIRST_LIMIT
        };
        let mut started = std::collections::BTreeMap::new();
        let (sounded, result) = resident.speak(
            text,
            first,
            VOICE_SYNTH_LIMIT,
            &self.paused,
            |event| match event {
                Spoken::Start(i, at) => {
                    flow_voice(id, "played", text, &[("piece", &i.to_string())]);
                    started.insert(i, at);
                    self.speaking.mark(at, 0, at);
                }
                Spoken::End(i, at) => {
                    let t0 = started.remove(&i).unwrap_or(at);
                    self.speaking.mark(t0, at, at);
                }
            },
        );
        if result.is_err() {
            retire(&mut slot, &mut retired);
            // A piece cut short still closes its window.
            let now = now_ms();
            for t0 in started.into_values() {
                self.speaking.mark(t0, now, now);
            }
        }
        (sounded, result)
    }

    /// Plays one clip (or speaks with `say`), its span marked in the in-memory playback windows; a WAV is deleted
    /// afterwards, played or not (clips are never needed again).
    fn play(&self, settings: &EarsSettings, clip: &Clip) -> Result<(), String> {
        let mut command = match clip {
            Clip::Say(text) => {
                let mut say = exec::cleared(Path::new(SAY_PROGRAM), &settings.home);
                say.args(say_args(
                    &settings.voice,
                    text,
                    self.output().as_deref(),
                    settings.rate,
                )?);
                say
            }
            Clip::Wav(wav, _) => {
                let mut play = exec::cleared(&settings.player, &settings.home);
                play.args(player_args(settings.rate)).arg(wav);
                play
            }
        };
        command.stdin(std::process::Stdio::null());
        // Echo: the playback window is on record while it lasts (t1 0) and closed after.
        let t0 = now_ms();
        self.speaking.mark(t0, 0, t0);
        // A link drop does not cut a sentence short; Pause does.
        let never = tulpa_stop::Stop::new();
        let outcome = exec::run(command, &self.paused, &never, SAY_LIMIT, SAY_STDERR_MAX)
            .map_err(|e| format!("voice: {SAY_PROGRAM}: {e}"));
        let t1 = now_ms();
        self.speaking.mark(t0, t1, t1);
        if let Clip::Wav(wav, started) = clip {
            forget_clip(wav, *started);
        }
        let outcome = outcome?;
        match outcome.ended {
            Ended::Exit(Some(0)) => Ok(()),
            Ended::Exit(code) => Err(format!(
                "voice: say exit {code:?}: {}",
                String::from_utf8_lossy(&outcome.stderr).trim()
            )),
            Ended::Paused => Err("paused (killed)".to_owned()),
            Ended::Timeout => Err("voice: timeout (killed)".to_owned()),
            _ => Err("voice: say ended early".to_owned()),
        }
    }

    /// Runs `voice_synth` with the text and returns the WAV it reports (post-processed when `voice_post` is set).
    fn synthesize(&self, settings: &EarsSettings, text: &str) -> Result<PathBuf, String> {
        let clean = clean_voice_text(text)?;
        let argv: Vec<String> = settings
            .voice_synth
            .iter()
            .map(|a| a.replace("{text}", &clean))
            .collect();
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| "empty voice_synth".to_owned())?;
        let mut command = exec::cleared(Path::new(program), &settings.home);
        command.args(rest).stdin(std::process::Stdio::null());
        if let Some(dir) = &settings.voice_cwd {
            command.current_dir(dir);
        }
        let never = tulpa_stop::Stop::new();
        let outcome = exec::run(
            command,
            &self.paused,
            &never,
            VOICE_SYNTH_LIMIT,
            SAY_STDERR_MAX.saturating_mul(64),
        )
        .map_err(|e| format!("{program}: {e}"))?;
        if outcome.ended != Ended::Exit(Some(0)) {
            return Err(format!("{program}: {:?}", outcome.ended));
        }
        let out = String::from_utf8_lossy(&outcome.stdout);
        let wav = out
            .split_whitespace()
            .rev()
            .find(|w| {
                Path::new(w)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            })
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .ok_or_else(|| "no WAV path in its output".to_owned())?;
        Ok(self.post(settings, wav))
    }

    /// Runs `voice_post` on a clip ({in} → {out}); the processed copy when it worked, else the clip.
    fn post(&self, settings: &EarsSettings, wav: PathBuf) -> PathBuf {
        let never = tulpa_stop::Stop::new();
        if settings.voice_post.is_empty() {
            return wav;
        }
        let processed = wav.with_extension("post.wav");
        let argv: Vec<String> = settings
            .voice_post
            .iter()
            .map(|a| {
                a.replace("{in}", &wav.display().to_string())
                    .replace("{out}", &processed.display().to_string())
            })
            .collect();
        let Some((program, rest)) = argv.split_first() else {
            return wav;
        };
        let mut post = exec::cleared(Path::new(program), &settings.home);
        post.args(rest).stdin(std::process::Stdio::null());
        match exec::run(
            post,
            &self.paused,
            &never,
            VOICE_SYNTH_LIMIT,
            SAY_STDERR_MAX,
        ) {
            Ok(o) if o.ended == Ended::Exit(Some(0)) && processed.is_file() => {
                discard_clip(&wav);
                processed
            }
            _ => wav,
        }
    }

    fn push_event(&self, event: &Value) {
        self.outbox.push(event.to_string().into_bytes());
    }
}

impl Drop for Ears {
    fn drop(&mut self) {
        let Ok(inner) = self.inner.get_mut() else {
            return;
        };
        let stopped = inner.closing.iter().chain(&inner.given_up);
        let stopped: Vec<Arc<exec::Group>> = stopped.map(|c| Arc::clone(&c.group)).collect();
        for group in inner.group.take().into_iter().chain(stopped) {
            let _sent: bool = group.kill();
        }
    }
}

fn open_log(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Takes the resident synth out of its slot, under the slot's lock, and kills it there (a signal, never a wait).
/// Its reap is a wait, and no lock is held across a wait: it goes into `retired`, which the caller drops once the
/// lock is let go (a resident whose reap does not come, one in uninterruptible sleep, would otherwise hold every
/// voice and Quit with it).
fn retire(slot: &mut Option<Resident>, retired: &mut Vec<Resident>) {
    if let Some(mut resident) = slot.take() {
        resident.kill();
        retired.push(resident);
    }
}

/// Reaps `child`, killed already. A wait, so never under a lock: one whose reap does not come (a process in
/// uninterruptible sleep) would hold whoever needs that lock with it.
pub(crate) fn reap_killed(child: &mut Child) {
    #[cfg(test)]
    if let Some(hook) = BEFORE_REAP.with(std::cell::RefCell::take) {
        hook();
    }
    let _reaped: std::io::Result<std::process::ExitStatus> = child.wait();
}

#[cfg(test)]
thread_local! {
    /// The test seam for [`reap_killed`]'s wait: runs once, on the thread that reaps, just before it.
    pub(crate) static BEFORE_REAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether process `pid` still exists (a signal-0 probe): `ears.status`
/// reports it as `alive`, so a state that claims `running` can be checked
/// against the process it names instead of trusted. A pid that exists but is
/// not ours (EPERM) still counts as alive: the probe asks "is anyone there",
/// not "can I stop it".
#[must_use]
pub(crate) fn pid_alive(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .is_some_and(|pid| match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            Err(error) => error == rustix::io::Errno::PERM,
        })
}

/// The file the ears mode is kept in across restarts ([`Ears::with_mode_file`]):
/// `ears-mode` beside the config file at `config_path`.
#[must_use]
pub fn mode_file_beside(config_path: &Path) -> PathBuf {
    config_path.with_file_name(saved_mode::FILE_NAME)
}

/// The one line handler for the link: `ears.*` and `voice.*` verbs go to the
/// ears, everything else to the hands (or a refusal when there are none).
#[must_use]
pub fn route(hands: Option<LineHandler>, ears: Arc<Ears>) -> LineHandler {
    #[derive(Deserialize)]
    struct Verb {
        verb: String,
    }
    Arc::new(move |line: &[u8]| {
        let organ_is_ears = serde_json::from_slice::<Verb>(line)
            .is_ok_and(|v| v.verb.starts_with("ears.") || v.verb.starts_with("voice."));
        if organ_is_ears {
            ears.handle(line)
        } else if let Some(hands) = &hands {
            hands(line)
        } else {
            refuse_line(line)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Instant;

    use super::*;
    use crate::link::{End, LinkError, Pipe, Recv, serve_attempt};

    #[derive(Debug, Default)]
    struct Notes(Mutex<Vec<String>>);

    impl Journal for Notes {
        fn record(&self, line: &str) {
            if let Ok(mut lines) = self.0.lock() {
                lines.push(line.to_owned());
            }
        }
    }

    fn temp_dir(name: &str) -> std::io::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-ears-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn settings(dir: &Path, script: &str) -> EarsSettings {
        EarsSettings {
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            me_dir: dir.join("me"),
            them_dir: dir.join("them"),
            err_file: Some(dir.join("err.log")),
            device: "test-mac".to_owned(),
            transcript_files: Vec::new(),
            voice: "Milena".to_owned(),
            log_file: dir.join("ears.log"),
            home: dir.to_path_buf(),
            inherit_env: false,
            env: Vec::new(),
            modes: vec![(
                EarsMode::Call,
                vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            )],
            voice_synth: Vec::new(),
            voice_serve: Vec::new(),
            voice_serve_plays: false,
            voice_cwd: None,
            voice_post: Vec::new(),
            player: PathBuf::from(AFPLAY_PROGRAM),
            mic_device: None,
            output_device: None,
            rate: None,
            debug: false,
        }
    }

    fn organ(settings: Result<EarsSettings, String>) -> (Arc<Ears>, Arc<Outbox>, Arc<Stop>) {
        let outbox = Arc::new(Outbox::default());
        let paused = Arc::new(Stop::new());
        let ears = Ears::new(
            settings,
            Arc::clone(&paused),
            Arc::clone(&outbox),
            Arc::new(Notes::default()),
            Box::new(|| {}),
        );
        (Arc::new(ears), outbox, paused)
    }

    fn ask(ears: &Arc<Ears>, verb: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let line = format!("{{\"id\":\"t1\",\"verb\":\"{verb}\"}}");
        Ok(serde_json::from_slice(&ears.handle(line.as_bytes()))?)
    }

    /// One field of a reply, `Null` when absent.
    fn at(value: &Value, pointer: &str) -> Value {
        value.pointer(pointer).cloned().unwrap_or(Value::Null)
    }

    fn group_alive(pid: u32) -> bool {
        i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .is_some_and(|group| rustix::process::test_kill_process_group(group).is_ok())
    }

    /// Waits up to `limit` for the group to be gone; zombies a non-reaping
    /// container init leaves behind still answer signal 0, so a group whose
    /// members are all zombies counts as gone.
    fn group_gone_within(pid: u32, limit: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < limit {
            if !group_alive(pid) || only_zombies_in(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn only_zombies_in(group: u32) -> bool {
        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };
        let group = group.to_string();
        entries.filter_map(Result::ok).all(|entry| {
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            // pid (comm) state ppid pgrp …: fields after the last ')'.
            let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
            let fields: Vec<&str> = after.split_whitespace().collect();
            match (fields.first(), fields.get(2)) {
                (Some(state), Some(pgrp)) if *pgrp == group => *state == "Z",
                _ => true,
            }
        })
    }

    #[cfg(not(target_os = "linux"))]
    const fn only_zombies_in(_group: u32) -> bool {
        false
    }

    /// The outbox lines other than mode changes.
    fn not_mode(lines: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        lines
            .into_iter()
            .filter(|l| !String::from_utf8_lossy(l).contains("\"state\":\"mode\""))
            .collect()
    }

    /// Modes: a switch kills the old group and starts the new mode's argv; an unconfigured mode is
    /// refused and nothing changes; every switch tells the link mode and from (the handshake keys on them).
    #[test]
    fn modes_switch_groups_refuse_unconfigured_and_tell_mode_and_from()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("modes")?;
        let mut s = settings(&dir, "sleep 30 & sleep 30");
        s.modes.push((
            EarsMode::Office,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sleep 31 & sleep 31".to_owned(),
            ],
        ));
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(ears.mode(), EarsMode::Off);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let call_pid = pid_of(&ears.state())?;
        assert_eq!(ears.mode(), EarsMode::Call);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "office")?, "/ok"), true);
        assert!(
            group_gone_within(call_pid, Duration::from_secs(1)),
            "the call capture's group is gone"
        );
        let office_pid = pid_of(&ears.state())?;
        assert_ne!(office_pid, call_pid);
        assert_eq!(ears.mode(), EarsMode::Office);
        let refused = ask_arg(&ears, "ears.mode", "stream")?;
        assert_eq!(at(&refused, "/ok"), false);
        assert_eq!(ears.mode(), EarsMode::Office, "refused: stays");
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "nonsense")?, "/err"), "arg");
        assert_eq!(at(&ask(&ears, "ears.status")?, "/result/mode"), "office");
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "off")?, "/ok"), true);
        assert!(group_gone_within(office_pid, Duration::from_secs(1)));
        let modes: Vec<Value> = outbox
            .drain()
            .iter()
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .filter(|v| v["state"] == "mode")
            .collect();
        assert_eq!(
            modes,
            vec![
                json!({"event": "ears", "state": "mode", "mode": "call", "from": "off", "source": "request:t1"}),
                json!({"event": "ears", "state": "mode", "mode": "office", "from": "call", "source": "request:t1"}),
                json!({"event": "ears", "state": "mode", "mode": "off", "from": "office", "source": "request:t1"}),
            ]
        );
        assert_eq!(
            EarsMode::from_id("alone").map(EarsMode::label),
            Some("alone")
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn ask_arg(
        ears: &Arc<Ears>,
        verb: &str,
        arg: &str,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let line = json!({"id": "t1", "verb": verb, "arg": arg}).to_string();
        Ok(serde_json::from_slice(&ears.handle(line.as_bytes()))?)
    }

    fn pid_of(state: &EarsState) -> Result<u32, Box<dyn std::error::Error>> {
        state
            .pid()
            .ok_or_else(|| format!("expected a running capture, got {state:?}").into())
    }

    #[test]
    fn capture_argv_expands_tilde_and_wants_an_absolute_program() {
        let home = Path::new("/tmp/example-home");
        let command = ["/bin/bash".to_owned(), "~/path/to/capture.sh".to_owned()];
        assert_eq!(
            capture_argv(&command, Some(home)),
            Ok(vec![
                "/bin/bash".to_owned(),
                "/tmp/example-home/path/to/capture.sh".to_owned()
            ])
        );
        assert_eq!(
            capture_argv(&["~/bin/capture".to_owned()], Some(home)),
            Ok(vec!["/tmp/example-home/bin/capture".to_owned()])
        );
        assert!(capture_argv(&[], Some(home)).is_err());
        assert!(
            capture_argv(&["bash".to_owned()], Some(home)).is_err(),
            "a bare program name would be looked up on the cleared PATH"
        );
    }

    #[test]
    fn device_name_is_safe_in_a_file_name() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(device_name("desk-2_b"), "desk-2_b");
        assert_eq!(device_name("My Mac/../x"), "My_Mac____x");
        assert_eq!(device_name("мак"), "___");
        assert_eq!(device_name("  "), "tulpa");
        assert_eq!(device_name(&"a".repeat(40)), "a".repeat(32));
        let config = TrayAppConfig::from_toml(
            "[link]\ndevice = \"home.mac\"\n[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"",
        )?;
        let settings =
            EarsSettings::from_config(&config, Path::new("/tmp/h"), PathBuf::from("/tmp/l"))?;
        assert_eq!(
            settings.device, "home_mac",
            "the link's device, even with the link off"
        );
        Ok(())
    }

    #[test]
    fn transcript_files_resolve_the_device_and_refuse_relative_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = Path::new("/tmp/example-home");
        let log = PathBuf::from("/tmp/example-home/ears.log");
        let base = "[link]\ndevice = \"desk mac\"\n[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\n";
        let config = TrayAppConfig::from_toml(&format!(
            "{base}transcript_files = [\"~/t/call-me.{{device}}.txt\", \"/var/t/call-them.{{device}}.txt\", \"~/t/call-me.{{device}}.txt\"]\nwake_words = [\" Tulpa \"]"
        ))?;
        let s = EarsSettings::from_config(&config, home, log.clone())?;
        assert_eq!(
            s.transcript_files,
            vec![
                home.join("t/call-me.desk_mac.txt"),
                PathBuf::from("/var/t/call-them.desk_mac.txt")
            ],
            "{{device}} is the name TULPA_DEVICE carries; duplicates dropped"
        );
        assert!(
            s.may_send_lines(),
            "files listed is all a line needs now: the side decides (vany 2026-11-18)"
        );
        let config = TrayAppConfig::from_toml(&format!("{base}transcript_files = [\"t/me.txt\"]"))?;
        assert!(
            EarsSettings::from_config(&config, home, log.clone()).is_err(),
            "a relative path is refused"
        );
        let none = EarsSettings::from_config(&TrayAppConfig::from_toml(base)?, home, log)?;
        assert!(none.transcript_files.is_empty() && !none.may_send_lines());
        Ok(())
    }

    /// `[ears] command` set while `[ears.modes]` runs something else: one
    /// warning naming the differing modes; silent when nothing is overridden.
    #[test]
    fn command_overridden_names_the_modes_that_win() -> Result<(), Box<dyn std::error::Error>> {
        let home = Path::new("/tmp/example-home");
        let cfg = |ears: &str| {
            TrayAppConfig::from_toml(&format!(
                "[ears]\nmode = \"on\"\nme_dir = \"/a\"\nthem_dir = \"/b\"\n{ears}"
            ))
        };
        let differ = cfg(
            "command = [\"~/patched.sh\"]\n[ears.modes]\ncall = [\"/app/capture.sh\", \"call\"]\noffice = [\"~/patched.sh\"]\nalone = [\"/app/capture.sh\"]",
        )?;
        let warning = command_overridden(&differ.ears, home);
        assert_eq!(
            warning.as_deref(),
            Some(
                "ears: [ears] command [\"/tmp/example-home/patched.sh\"] is ignored for alone, call: [ears.modes] sets their commands"
            ),
            "sorted, and office (same argv once ~ resolves) is not named"
        );
        let same = cfg(
            "command = [\"/app/capture.sh\"]\n[ears.modes]\ncall = [\"/app/capture.sh\"]\noffice = [\"/app/capture.sh\"]",
        )?;
        assert_eq!(command_overridden(&same.ears, home), None);
        let no_command = cfg("[ears.modes]\ncall = [\"/app/capture.sh\"]")?;
        assert_eq!(command_overridden(&no_command.ears, home), None);
        let no_modes = cfg("command = [\"/app/capture.sh\"]")?;
        assert_eq!(command_overridden(&no_modes.ears, home), None);
        Ok(())
    }

    /// The startup notes: inert keys and a foreign transcript name go to the
    /// journal, the name also to the link as one `config-warn` event; a file
    /// named with the device's own token says nothing.
    #[test]
    fn startup_notes_flag_inert_keys_and_foreign_transcript_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = Path::new("/tmp/example-home");
        let log = PathBuf::from("/tmp/example-home/ears.log");
        let base = "[link]\ndevice = \"desk mac\"\n[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\n";
        let config = TrayAppConfig::from_toml(&format!(
            "{base}transcript_files = [\"/tmp/t/call-me.desk_mac.txt\", \
             \"/tmp/t/call-me.work_mac.txt\"]\nwake_words = [\"tulpa\"]\nfollowup_secs = 12\n"
        ))?;
        let settings = EarsSettings::from_config(&config, home, log)?;
        let outbox = Outbox::default();
        let notes = Notes::default();
        startup_notes(&config, &Ok(settings), &outbox, &notes);
        let notes = notes.0.lock().map_err(|_| "poisoned")?;
        assert!(
            notes
                .iter()
                .any(|n| n.contains("wake_words, followup_secs set but inert"))
        );
        assert_eq!(
            notes
                .iter()
                .filter(|n| n.contains("lacks this device's token"))
                .count(),
            1,
            "the own-token file says nothing"
        );
        let lines = outbox.drain();
        assert_eq!(lines.len(), 1, "one event, for the foreign name only");
        let event: Value = serde_json::from_slice(lines.first().ok_or("no event")?)?;
        assert_eq!(
            (
                event.get("event").and_then(Value::as_str),
                event.get("state").and_then(Value::as_str)
            ),
            (Some("link"), Some("config-warn"))
        );
        let reason = event
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            reason.contains("call-me.work_mac.txt") && reason.contains("desk_mac"),
            "{reason}"
        );
        assert!(
            !reason.chars().any(char::is_control) && reason.chars().count() <= 200,
            "{reason}"
        );
        assert!(
            event
                .get("ts")
                .and_then(Value::as_str)
                .is_some_and(|ts| ts.ends_with('Z'))
        );
        drop(notes);

        // A name long enough to overflow: the reason stays within the door's 200.
        let oversized = format!("/tmp/t/call-me.{}.txt", "x".repeat(120));
        let config = TrayAppConfig::from_toml(&format!(
            "{base}transcript_files = [\"{oversized}\"]\nwake_words = [\"tulpa\"]\n"
        ))?;
        let settings = EarsSettings::from_config(&config, home, PathBuf::from("/tmp/l"))?;
        let outbox = Outbox::default();
        startup_notes(&config, &Ok(settings), &outbox, &Notes::default());
        let lines = outbox.drain();
        assert_eq!(lines.len(), 1);
        let event: Value = serde_json::from_slice(lines.first().ok_or("no event")?)?;
        let reason = event
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(reason.chars().count() <= 200, "{reason}");
        Ok(())
    }

    #[test]
    fn settings_need_mode_on_a_command_and_both_dirs() -> Result<(), Box<dyn std::error::Error>> {
        let home = Path::new("/tmp/example-home");
        let log = PathBuf::from("/tmp/example-home/ears.log");
        let on = TrayAppConfig::from_toml(
            r#"
            [ears]
            mode = "on"
            command = ["/bin/bash", "~/path/to/capture.sh"]
            me_dir = "~/me"
            them_dir = "/var/them"
            "#,
        )?;
        let settings = EarsSettings::from_config(&on, home, log.clone())?;
        assert_eq!(settings.device, "tulpa", "the link's default device");
        assert_eq!(settings.me_dir, home.join("me"));
        assert_eq!(settings.them_dir, PathBuf::from("/var/them"));
        assert_eq!(settings.err_file, None);
        assert_eq!(settings.voice, "Milena");

        let off = TrayAppConfig::from_toml("[ears]\ncommand = [\"/bin/true\"]")?;
        assert_eq!(
            EarsSettings::from_config(&off, home, log.clone()),
            Err("ears: off in config".to_owned())
        );
        let ask = TrayAppConfig::from_toml("[ears]\nmode = \"ask\"")?;
        assert!(
            EarsSettings::from_config(&ask, home, log.clone())
                .is_err_and(|why| why.contains("not supported"))
        );
        let no_dirs = TrayAppConfig::from_toml("[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]")?;
        assert!(
            EarsSettings::from_config(&no_dirs, home, log.clone())
                .is_err_and(|why| why.contains("me_dir"))
        );
        let bad_voice = TrayAppConfig::from_toml(
            "[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\nvoice = \"-o /tmp/x\"",
        )?;
        assert!(EarsSettings::from_config(&bad_voice, home, log).is_err());
        Ok(())
    }

    #[test]
    fn voice_text_is_capped_cleaned_and_shielded() {
        assert_eq!(
            clean_voice_text("  Привет, мир  "),
            Ok("Привет, мир".to_owned())
        );
        assert_eq!(
            clean_voice_text("one\ntwo\tthree\u{7}"),
            Ok("one two three".to_owned()),
            "control characters become spaces"
        );
        assert_eq!(
            clean_voice_text("-o /tmp/out"),
            Ok(" -o /tmp/out".to_owned()),
            "a leading dash never reaches say as an option"
        );
        assert!(clean_voice_text(" \n\t ").is_err());
        let at_cap = "я".repeat(VOICE_TEXT_MAX);
        assert_eq!(clean_voice_text(&at_cap), Ok(at_cap.clone()));
        assert!(clean_voice_text(&format!("{at_cap}я")).is_err_and(|why| why.contains("cap")));
        assert_eq!(
            say_args("Milena", "hello", None, None),
            Ok(vec![
                "-v".to_owned(),
                "Milena".to_owned(),
                "hello".to_owned()
            ])
        );
        assert_eq!(
            say_args("Milena", "-hi", Some("Desk Speakers"), None),
            Ok(vec![
                "-v".to_owned(),
                "Milena".to_owned(),
                "-a".to_owned(),
                "Desk Speakers".to_owned(),
                " -hi".to_owned()
            ]),
            "the output by name; the text still last and shielded"
        );
    }

    /// `[voice] rate` comes from the config, 0.5 to 2; 1 (or none) is normal speed and passes nothing.
    #[test]
    fn the_rate_comes_from_the_config_within_its_range() -> Result<(), Box<dyn std::error::Error>> {
        let base = "[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\n[voice]\n";
        let rate = |line: &str| -> Result<Option<f64>, String> {
            let config =
                TrayAppConfig::from_toml(&format!("{base}{line}")).map_err(|e| e.to_string())?;
            EarsSettings::from_config(&config, Path::new("/tmp/h"), PathBuf::from("/tmp/l"))
                .map(|s| s.rate)
        };
        assert_eq!(rate("")?, None, "unset: normal");
        assert_eq!(rate("rate = 1.0")?, None, "1: nothing passed");
        assert_eq!(rate("rate = 1")?, None, "an integer is a rate too");
        assert_eq!(rate("rate = 1.5")?, Some(1.5));
        assert_eq!(rate("rate = 0.5")?, Some(0.5));
        assert_eq!(rate("rate = 2")?, Some(2.0));
        for bad in [
            "rate = 2.5",
            "rate = 0.4",
            "rate = 0",
            "rate = -1",
            "rate = nan",
            "rate = inf",
        ] {
            assert!(
                rate(bad).is_err_and(|why| why.contains("[voice] rate")),
                "{bad}"
            );
        }
        Ok(())
    }

    use crate::flow::fl;

    /// flow.log follows a `voice.say` by its request id: queued, synth, played, ended — or failed.
    #[test]
    fn flow_follows_a_say_by_its_id() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("flow-say")?;
        let clips = dir.join("clips");
        let wav = clips.join("1").join("clip.wav");
        let player = dir.join("player.sh");
        crate::testutil::write_script(&player, "#!/bin/sh\nexit 0\n")?;
        let mut s = settings(&dir, "true");
        s.player = player;
        s.voice_synth = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "mkdir -p {}; : > {}; echo {}",
                clips.join("1").display(),
                wav.display(),
                wav.display()
            ),
            "synth".to_owned(),
        ];
        let (ears, _, _) = organ(Ok(s));
        let said: Value = serde_json::from_slice(
            &ears.handle(
                json!({"id": "flow-say-1", "verb": "voice.say", "arg": "Флоу раз."})
                    .to_string()
                    .as_bytes(),
            ),
        )?;
        assert_eq!(at(&said, "/ok"), true, "{said}");
        let stages: Vec<Value> = crate::flow::test_entries(|e| {
            fl(e, "/kind") == "voice" && fl(e, "/id") == "flow-say-1"
        })
        .iter()
        .map(|e| fl(e, "/stage"))
        .collect();
        assert_eq!(
            stages,
            ["queued", "synth", "played", "ended"].map(Value::from)
        );
        // A say refused before anything renders or plays (nothing to say) is one line: refused,
        // with its reason, never queued.
        let _refused = ears.handle(
            json!({"id": "flow-say-2", "verb": "voice.say", "arg": " \n "})
                .to_string()
                .as_bytes(),
        );
        let refused = crate::flow::test_entries(|e| fl(e, "/id") == "flow-say-2");
        assert_eq!(refused.len(), 1, "{refused:?}");
        let line = crate::flow::nth(&refused, 0);
        assert_eq!(
            (fl(&line, "/kind"), fl(&line, "/stage"), fl(&line, "/frame")),
            ("voice".into(), "refused:text".into(), " \n ".into())
        );
        assert_eq!(fl(&line, "/detail"), "voice: empty text");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The rate reaches each path once: `say -r` (175 words a minute × rate), `afplay -r <rate> -q 1`
    /// for a synthesized WAV, `--speed <rate>` for a resident that plays itself (not one that only
    /// renders WAVs, nor the one-shot synth: afplay already speeds their clips up).
    #[test]
    fn the_rate_goes_to_each_path_once() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            say_args("Milena", "hi", None, Some(1.5))?,
            ["-v", "Milena", "-r", "263", "hi"],
            "175 × 1.5 = 262.5, rounded"
        );
        assert_eq!(
            say_args("Milena", "hi", Some("Desk Speakers"), Some(0.8))?,
            ["-v", "Milena", "-a", "Desk Speakers", "-r", "140", "hi"]
        );
        assert_eq!(player_args(None), ["-v", "0.9"]);
        assert_eq!(
            player_args(Some(1.35)),
            ["-v", "0.9", "-r", "1.35", "-q", "1"]
        );
        let dir = temp_dir("rate")?;
        let mut s = settings(&dir, "true");
        s.rate = Some(1.5);
        s.voice_serve = vec!["/bin/serve".to_owned()];
        assert_eq!(s.serve_argv(None), ["/bin/serve"], "a WAV renderer: none");
        s.voice_serve_plays = true;
        assert_eq!(
            s.serve_argv(Some("Desk Speakers")),
            ["/bin/serve", "--device", "Desk Speakers", "--speed", "1.5"]
        );
        s.voice_serve = vec![
            "/bin/serve".to_owned(),
            "--speed".to_owned(),
            "1.2".to_owned(),
        ];
        assert_eq!(
            s.serve_argv(None),
            ["/bin/serve", "--speed", "1.2"],
            "its own wins"
        );
        s.rate = None;
        s.voice_serve = vec!["/bin/serve".to_owned()];
        assert_eq!(s.serve_argv(None), ["/bin/serve"], "normal speed: nothing");
        // A WAV from the one-shot synth plays at the rate; the synth itself gets no --speed.
        // The synth makes a directory per clip, as a real one may: played clips go with theirs.
        let clips = dir.join("clips");
        let wav = clips.join("1").join("clip.wav");
        let synth_args = dir.join("synth.txt");
        let heard = dir.join("heard.txt");
        let player = dir.join("player.sh");
        crate::testutil::write_script(
            &player,
            &format!("#!/bin/sh\necho \"$*\" >> {}\n", heard.display()),
        )?;
        let mut s = settings(&dir, "true");
        s.rate = Some(1.5);
        s.player = player;
        s.voice_synth = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "printf '%s\\n' \"$*\" > {}; mkdir -p {}; : > {}; echo {}",
                synth_args.display(),
                clips.join("1").display(),
                wav.display(),
                wav.display()
            ),
            "synth".to_owned(),
            "{text}".to_owned(),
            "1".to_owned(),
        ];
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(ears.say(&s, "Раз.")?, "said");
        assert_eq!(
            fs::read_to_string(&synth_args)?,
            "Раз. 1\n",
            "no --speed for the synth"
        );
        assert_eq!(
            fs::read_to_string(&heard)?,
            format!("-v 0.9 -r 1.5 -q 1 {}\n", wav.display())
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The picked microphone reaches the capture as `EARS_MIC=":<name>"` (avfoundation, audio only),
    /// over an `EARS_MIC` in `env`; a new pick restarts a running capture in its mode, unpicking leaves
    /// `env`'s value.
    #[test]
    fn a_picked_mic_reaches_the_capture_and_a_new_pick_restarts_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("mic")?;
        let out = dir.join("env.txt");
        let mut s = settings(
            &dir,
            &format!("env > {}.new; mv {0}.new {0}; sleep 30", out.display()),
        );
        s.env = vec![("EARS_MIC".to_owned(), ":From Env".to_owned())];
        let (ears, outbox, _) = organ(Ok(s));
        let mic_seen = |pid: u32| -> Result<Option<String>, Box<dyn std::error::Error>> {
            let started = Instant::now();
            loop {
                let text = fs::read_to_string(&out).unwrap_or_default();
                // The file is the newest capture's once it names that capture's pid group.
                if text.lines().any(|l| l == "TULPA_DEVICE=test-mac") && group_alive(pid) {
                    return Ok(text
                        .lines()
                        .find_map(|l| l.strip_prefix("EARS_MIC="))
                        .map(str::to_owned));
                }
                if started.elapsed() > Duration::from_secs(3) {
                    return Err(format!("no env from pid {pid}: {text}").into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        };
        assert_eq!(
            ears.set_mic(Some("Example USB Mic".to_owned()))?,
            "mic Example USB Mic from the next start"
        );
        assert_eq!(ears.mic().as_deref(), Some("Example USB Mic"));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let first = pid_of(&ears.state())?;
        assert_eq!(
            mic_seen(first)?.as_deref(),
            Some(":Example USB Mic"),
            "the pick wins over env"
        );
        fs::remove_file(&out)?;
        let said = ears.set_mic(Some("Built-in Microphone".to_owned()))?;
        assert!(said.contains("capture restarted in call"), "{said}");
        let second = pid_of(&ears.state())?;
        assert_eq!(
            ears.set_mic(Some("Built-in Microphone".to_owned()))?,
            "mic Built-in Microphone already"
        );
        assert_eq!(
            pid_of(&ears.state())?,
            second,
            "the same pick again restarts nothing"
        );
        assert_ne!(second, first);
        assert!(
            group_gone_within(first, Duration::from_secs(1)),
            "the old capture is gone"
        );
        assert_eq!(ears.mode(), EarsMode::Call, "the mode stays");
        assert_eq!(mic_seen(second)?.as_deref(), Some(":Built-in Microphone"));
        fs::remove_file(&out)?;
        ears.set_mic(None)?;
        let third = pid_of(&ears.state())?;
        assert_eq!(
            mic_seen(third)?.as_deref(),
            Some(":From Env"),
            "unpicked: env's value"
        );
        assert!(ears.set_mic(Some("bad\nname".to_owned())).is_err());
        for misread in ["2nd Mic", "Desk: USB"] {
            assert_eq!(
                ears.set_mic(Some(misread.to_owned())),
                Err(crate::devices::MIC_SCRIPT_ONLY.to_owned()),
                "{misread}"
            );
        }
        assert_eq!(ears.mic(), None, "a refused name changes nothing");
        assert!(
            not_mode(outbox.drain())
                .iter()
                .all(|l| !String::from_utf8_lossy(l).contains("\"state\":\"off\"")),
            "a restart for a new microphone tells no stop"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0393: a capture answers `TULPA_LINES_STDOUT=1` by putting its phrases
    /// on stdout as marker records; the tray sends them like transcript lines,
    /// tees all of stdout to the side log (a log, not a path the phrases need),
    /// and drops a microphone phrase that meets a remembered playback window —
    /// the room's echo of our own voice, in memory per device.
    #[test]
    fn stdout_markers_go_to_the_link_and_our_own_echo_does_not()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("stdout-markers")?;
        let s = settings(
            &dir,
            "printf 'TULPA-EARS-LINE 1000 2500 12:00:10 [me] первая фраза\\t\
             1100,1500,первая;1900,2300,фраза\\n'; \
             printf 'plain stdout noise, not a marker\\n'; sleep 2; \
             printf 'TULPA-EARS-DEBUG [me] 2.0 s: lang=ru verdict=read leak-lines=0 raw=\"отладка\"\\n'; \
             printf 'TULPA-EARS-LINE 5000 7000 12:00:12 [me] эхо комнаты\\n'; sleep 30",
        );
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let events = |outbox: &Arc<Outbox>| -> Vec<Value> {
            outbox
                .drain()
                .iter()
                .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
                .collect()
        };
        // The first phrase goes out.
        let first = {
            let started = Instant::now();
            loop {
                let found = events(&outbox)
                    .into_iter()
                    .find(|v| v.get("text").and_then(Value::as_str) == Some("первая фраза"));
                if let Some(found) = found {
                    assert_eq!(found.get("side").and_then(Value::as_str), Some("me"));
                    assert_eq!(found.get("mode").and_then(Value::as_str), Some("call"));
                    assert_eq!(
                        found.pointer("/words/0/w").and_then(Value::as_str),
                        Some("первая"),
                        "the phrase's word timings go with it"
                    );
                    assert_eq!(
                        found.pointer("/words/1/t0").and_then(Value::as_u64),
                        Some(1_900),
                        "epoch ms, the utterance's own clock"
                    );
                    break found;
                }
                if started.elapsed() > Duration::from_secs(5) {
                    return Err("the first stdout phrase never went out".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        };
        assert!(
            !first
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
        );
        // While the capture runs: our own playback opens a window over the
        // second phrase's span, so it is dropped as echo — never sent; the debug line (E2 r2 R2) in no event.
        ears.speaking.mark(6_000, 0, now_ms());
        let started = Instant::now();
        loop {
            let sent = events(&outbox);
            if sent.iter().any(|v| v.to_string().contains("отладка")) {
                return Err("the debug line went out".into());
            }
            let leaked = sent
                .iter()
                .any(|v| v.get("text").and_then(Value::as_str) == Some("эхо комнаты"));
            if leaked {
                return Err("the echo phrase went out".into());
            }
            if started.elapsed() > Duration::from_secs(3) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // The side log keeps every stdout line, the dropped echo included.
        let log = fs::read_to_string(dir.join("ears.log"))?;
        assert!(
            log.contains("TULPA-EARS-DEBUG [me] 2.0 s"),
            "the side log lost the debug line: {log}"
        );
        for kept in ["первая фраза", "plain stdout noise", "эхо комнаты"] {
            assert!(log.contains(kept), "the side log lost {kept:?}: {log}");
        }
        // flow.log has each phrase once, with what became of it.
        let outcome = |text: &str| -> Vec<Value> {
            crate::flow::test_entries(|e| {
                fl(e, "/kind") == "ears"
                    && fl(e, "/frame").as_str().is_some_and(|f| f.contains(text))
            })
            .iter()
            .map(|e| fl(e, "/outcome"))
            .collect()
        };
        assert_eq!(outcome("первая фраза"), vec![Value::from("sent")]);
        assert_eq!(outcome("эхо комнаты"), vec![Value::from("dropped:echo")]);
        assert!(
            crate::flow::test_entries(|e| fl(e, "/frame")
                .as_str()
                .is_some_and(|f| f.contains("plain stdout noise")))
            .is_empty(),
            "stdout chatter is no transcript line"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0454 B1: the capture's verdict on a `<voice N.Ns>` piece (`TULPA-EARS-DEBUG [<side>]
    /// voice-unread:<rule> …`) lands in the side log, beside the line it explains, and never in the outbox.
    #[test]
    fn an_unread_verdict_stays_in_the_side_log() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("unread-verdict")?;
        let verdict = "TULPA-EARS-DEBUG [me] voice-unread:maxrun nt=7 m=7 maxrun=7 ms=4000";
        let s = settings(
            &dir,
            &format!(
                "printf '{verdict}\\n'; \
                 printf 'TULPA-EARS-LINE 1000 5000 12:00:10 [me] <voice 4.0s>\\n'; \
                 printf 'TULPA-EARS-LINE 6000 7000 12:00:11 [me] после\\n'; sleep 30"
            ),
        );
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        // The phrase after the verdict goes: by then the verdict was read.
        let mut sent: Vec<String> = Vec::new();
        let started = Instant::now();
        while !sent.iter().any(|l| l.contains("после")) {
            if started.elapsed() > Duration::from_secs(30) {
                return Err(
                    format!("the phrase after the verdict never went out: {sent:?}").into(),
                );
            }
            thread::sleep(Duration::from_millis(50));
            sent.extend(
                outbox
                    .drain()
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned()),
            );
        }
        assert!(sent.iter().any(|l| l.contains("<voice 4.0s>")), "{sent:?}");
        assert!(!sent.iter().any(|l| l.contains("voice-unread")), "{sent:?}");
        let log = fs::read_to_string(dir.join("ears.log"))?;
        assert!(
            log.contains(&format!("{verdict}\n")),
            "the side log lost the verdict: {log}"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0423 P1: the log window is fed from the one place a heard line's fate is decided
    /// ([`Ears::fate`]): every phrase that reaches `flow.log` with an outcome reaches the window with
    /// the same outcome — sent, dropped by the tray (`mode`), dropped by the capture (`prompt-echo`) —
    /// and a say that ended shows what was said. Noise (no side or text) shows nowhere.
    #[test]
    fn the_window_hears_what_flow_log_hears_with_the_same_fate()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("window-fate")?;
        let clips = dir.join("clips");
        let wav = clips.join("1").join("clip.wav");
        let player = dir.join("player.sh");
        crate::testutil::write_script(&player, "#!/bin/sh\nexit 0\n")?;
        let mut s = settings(
            &dir,
            "printf 'TULPA-EARS-DROP prompt-echo [me] окно подсказка\\n'; \
             printf 'TULPA-EARS-LINE 1000 2500 12:00:10 [me] окно первая\\n'; \
             printf 'TULPA-EARS-LINE 3000 4000 12:00:11 [them] окно видео\\n'; \
             printf 'TULPA-EARS-LINE 5000 6000 12:00:12 [me]\\n'; \
             printf 'TULPA-EARS-LINE 7000 8000 12:00:13 [me] окно последняя\\n'; sleep 30",
        );
        // Outside a call, so the system audio's phrase stays on the Mac: the mode filter's drop.
        let call = s
            .modes
            .first()
            .map(|(_, command)| command.clone())
            .unwrap_or_default();
        s.modes = vec![(EarsMode::Alone, call)];
        s.player = player;
        s.voice_synth = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "mkdir -p {}; : > {}; echo {}",
                clips.join("1").display(),
                wav.display(),
                wav.display()
            ),
            "synth".to_owned(),
        ];
        let saylog = Arc::new(Mutex::new(SayLog::new()));
        let outbox = Arc::new(Outbox::default());
        let ears = Arc::new(
            Ears::new(
                Ok(s.clone()),
                Arc::new(Stop::new()),
                Arc::clone(&outbox),
                Arc::new(Notes::default()),
                Box::new(|| {}),
            )
            .with_saylog(Arc::clone(&saylog)),
        );
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        let flow_outcome = |text: &str| -> Vec<Value> {
            crate::flow::test_entries(|e| {
                fl(e, "/kind") == "ears"
                    && fl(e, "/frame").as_str().is_some_and(|f| f.contains(text))
            })
            .iter()
            .map(|e| fl(e, "/outcome"))
            .collect()
        };
        let started = Instant::now();
        while flow_outcome("окно последняя").is_empty() {
            if started.elapsed() > Duration::from_secs(5) {
                return Err("the last phrase never reached flow.log".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _stopped = ask(&ears, "ears.stop")?;
        assert_eq!(ears.say(&s, "окно сказано.")?, "said");
        let rendered = held(&saylog).render();
        // Each phrase: once in flow.log, once in the window, the same fate in both.
        for (text, outcome, mark) in [
            (
                "окно подсказка",
                "dropped:prompt-echo",
                "[me dropped: prompt-echo]",
            ),
            ("окно первая", "sent", "[me → kira, queued]"),
            ("окно видео", "dropped:mode", "[them dropped: mode]"),
            ("окно последняя", "sent", "[me → kira, queued]"),
        ] {
            assert_eq!(flow_outcome(text), vec![Value::from(outcome)], "{text}");
            let shown: Vec<&str> = rendered.lines().filter(|l| l.contains(text)).collect();
            assert_eq!(shown.len(), 1, "{text} once in the window:\n{rendered}");
            assert!(
                shown.iter().all(|l| l.contains(mark)),
                "{text} marked {mark}:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("[tulpa] окно сказано."),
            "the said line:\n{rendered}"
        );
        assert_eq!(
            rendered.lines().count(),
            5,
            "four phrases and one say, the empty phrase nowhere:\n{rendered}"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// [`heard_rig`]'s scratch dir, organ, outbox and window log.
    type HeardRig = (PathBuf, Arc<Ears>, Arc<Outbox>, Arc<Mutex<SayLog>>);

    /// #0423 r3 S1: the two paths that set a heard mark — the capture's stdout records
    /// ([`Ears::read_markers`]) and the tailed transcript files ([`Ears::line_loop`]). Every R1 test runs
    /// on each, so a path that stops passing its phrase's id to the outbox goes red on its own.
    #[derive(Clone, Copy)]
    enum HeardFrom {
        Markers,
        Tailed,
    }

    /// Waits until the window's line for `text` contains `word`; `what` names the step for the error.
    fn wait_window(
        saylog: &Mutex<SayLog>,
        text: &str,
        word: &str,
        what: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let started = Instant::now();
        // POLL, and why: the window log is a plain Mutex<SayLog> with no change event; a test-only
        // wait, bounded at 5 s.
        while !window_line(saylog, text).contains(word) {
            if started.elapsed() > Duration::from_secs(5) {
                return Err(format!("{what} never reached the window").into());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// #0423 r2 rig: the ears hear each of `phrases` as a microphone phrase `from` one of the two
    /// paths, the window attached to the outbox as the app attaches it ([`crate::saylog::attach`]),
    /// and the wait until every phrase is queued. Markers: the capture's last record is a drop the
    /// reading thread decides after the last phrase's push. Tailed: the end line is appended only once
    /// the phrases are in the window, so a later look of the tail reads it, after this look's pushes.
    /// Either way its mark in the window means the pushes are done.
    fn heard_rig(
        name: &str,
        from: HeardFrom,
        phrases: &[&str],
    ) -> Result<HeardRig, Box<dyn std::error::Error>> {
        use std::fmt::Write as _;

        crate::flow::test_global();
        let path = match from {
            HeardFrom::Markers => "markers",
            HeardFrom::Tailed => "tailed",
        };
        let dir = temp_dir(&format!("{name}-{path}"))?;
        let transcript = dir.join("call-me.test-mac.txt");
        let mut script = String::new();
        if matches!(from, HeardFrom::Markers) {
            for (n, phrase) in phrases.iter().enumerate() {
                let t0 = n.saturating_mul(2_000).saturating_add(1_000);
                let t1 = t0.saturating_add(1_000);
                write!(
                    script,
                    "printf 'TULPA-EARS-LINE {t0} {t1} 12:00:{n:02} [me] {phrase}\\n'; "
                )?;
            }
            script.push_str("printf 'TULPA-EARS-DROP prompt-echo [me] конец стенда\\n'; ");
        } else {
            fs::write(&transcript, "")?;
        }
        script.push_str("sleep 30");
        let mut s = settings(&dir, &script);
        s.modes = vec![(EarsMode::Alone, s.argv.clone())];
        if matches!(from, HeardFrom::Tailed) {
            s.transcript_files = vec![transcript.clone()];
        }
        let saylog = Arc::new(Mutex::new(SayLog::new()));
        let outbox = Arc::new(Outbox::default());
        crate::saylog::attach(&saylog, &outbox);
        let mut ears = Ears::new(
            Ok(s),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::new(Notes::default()),
            Box::new(|| {}),
        )
        .with_saylog(Arc::clone(&saylog));
        ears.tail_every = Duration::from_millis(20);
        ears.check_after = Duration::from_secs(3600);
        let ears = Arc::new(ears);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        if matches!(from, HeardFrom::Tailed) {
            settle();
            let mut lines = String::new();
            for (n, phrase) in phrases.iter().enumerate() {
                writeln!(lines, "12:00:{n:02} [me] {phrase}")?;
            }
            append(&transcript, &lines)?;
            for phrase in phrases {
                wait_window(&saylog, phrase, "[me", "a tailed phrase")?;
            }
            append(&transcript, "12:00:59 [them] конец стенда\n")?;
        }
        wait_window(&saylog, "конец стенда", "dropped", "the rig's last record")?;
        Ok((dir, ears, outbox, saylog))
    }

    /// The window's one line showing `text`, or an empty string when none does (two would fail).
    fn window_line(saylog: &Mutex<SayLog>, text: &str) -> String {
        let rendered = held(saylog).render();
        let shown: Vec<&str> = rendered.lines().filter(|l| l.contains(text)).collect();
        assert!(shown.len() <= 1, "{text} more than once:\n{rendered}");
        shown
            .first()
            .map_or_else(String::new, |line| (*line).to_owned())
    }

    /// #0423 r3 S5: a panic under the window log's lock poisons it; a heard line still gets its entry,
    /// and the window still renders it — every lock of the log recovers in one place ([`held`]).
    #[test]
    fn a_poisoned_window_log_still_shows_a_heard_line() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("poisoned-saylog")?;
        let saylog = Arc::new(Mutex::new(SayLog::new()));
        let ears = Ears::new(
            Ok(settings(&dir, "sleep 30")),
            Arc::new(Stop::new()),
            Arc::new(Outbox::default()),
            Arc::new(Notes::default()),
            Box::new(|| {}),
        )
        .with_saylog(Arc::clone(&saylog));
        let poisoner = Arc::clone(&saylog);
        #[expect(
            clippy::panic,
            reason = "a panic under the log's lock is what is tested"
        )]
        let panicked = thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("rig: poison the window's log");
        })
        .join();
        assert!(
            panicked.is_err() && saylog.is_poisoned(),
            "the rig poisoned the log"
        );
        let id = ears.fate("12:00:00 [me] слух после паники", &LET_THROUGH);
        assert!(id.is_some(), "the heard line got its entry");
        let shown = window_line(&saylog, "слух после паники");
        assert!(shown.contains("[me → kira, queued]"), "rendered: {shown:?}");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0423 r2 R1, never written: a heard phrase the ears let through waits in the outbox until the
    /// link writes it, and the window says so — queued, never "sent", never a bare "→ kira".
    fn a_heard_line_not_yet_written_reads_queued(
        from: HeardFrom,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (dir, ears, _outbox, saylog) = heard_rig("heard-queued", from, &["слух ждёт"])?;
        let shown = window_line(&saylog, "слух ждёт");
        assert!(shown.contains("[me → kira, queued]"), "{shown}");
        assert!(!shown.to_lowercase().contains("sent"), "{shown}");
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0423 r2 R1, evicted: the ears lane is full ([`crate::link::OUTBOX_MAX`], drop-oldest) and the
    /// oldest phrase falls out unwritten: the window says lost, with why, for that phrase only — the
    /// newer one, still in the lane, stays queued.
    fn a_heard_line_evicted_from_the_ears_lane_reads_lost(
        from: HeardFrom,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (dir, ears, outbox, saylog) =
            heard_rig("heard-evicted", from, &["слух старый", "слух новый"])?;
        for _ in 0..crate::link::OUTBOX_MAX {
            if !window_line(&saylog, "слух старый").contains("queued") {
                break;
            }
            outbox.push(br#"{"event":"rig","state":"filler"}"#.to_vec());
        }
        let old = window_line(&saylog, "слух старый");
        assert!(
            old.contains("[me → kira, lost: the queue was full]"),
            "{old}"
        );
        let new = window_line(&saylog, "слух новый");
        assert!(new.contains("[me → kira, queued]"), "{new}");
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0423 r2 R1, broken: the pipe breaks while the ears lane is being written. The phrase written
    /// before the break is on the wire (never "sent"); the one whose write failed and the one behind it
    /// went with the attempt, and the window says lost, with why.
    fn heard_lines_lost_with_a_broken_attempt_read_lost(
        from: HeardFrom,
    ) -> Result<(), Box<dyn std::error::Error>> {
        /// Up after the hello; any write carrying `breaks_at` fails, every other write succeeds.
        struct BreaksAt(&'static str, usize);
        impl Pipe for BreaksAt {
            fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                if String::from_utf8_lossy(bytes).contains(self.0) {
                    return Err(LinkError::Send(std::io::Error::other(
                        "rig: the pipe broke",
                    )));
                }
                Ok(())
            }
            fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
                self.1 = self.1.saturating_add(1);
                Ok(if self.1 == 1 {
                    Recv::Timeout
                } else {
                    Recv::Eof
                })
            }
            fn waker(&self) -> crate::link::Waker {
                Arc::new(|| {})
            }
        }
        let (dir, ears, outbox, saylog) = heard_rig(
            "heard-broken",
            from,
            &["слух записан", "слух оборван", "слух следом"],
        )?;
        let mut pipe = BreaksAt("слух оборван", 0);
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            Duration::from_millis(1),
            &Stop::new(),
            &mut || {},
            &|_| Vec::new(),
            &outbox,
        );
        assert!(matches!(end, End::Io(_)), "{end:?}");
        let written = window_line(&saylog, "слух записан");
        assert!(written.contains("[me → kira, on the wire]"), "{written}");
        for gone in ["слух оборван", "слух следом"] {
            let shown = window_line(&saylog, gone);
            assert!(
                shown.contains("[me → kira, lost: the link broke before it was written]"),
                "{shown}"
            );
        }
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_heard_line_not_yet_written_reads_queued_markers() -> Result<(), Box<dyn std::error::Error>>
    {
        a_heard_line_not_yet_written_reads_queued(HeardFrom::Markers)
    }

    #[test]
    fn a_heard_line_not_yet_written_reads_queued_tailed() -> Result<(), Box<dyn std::error::Error>>
    {
        a_heard_line_not_yet_written_reads_queued(HeardFrom::Tailed)
    }

    #[test]
    fn a_heard_line_evicted_from_the_ears_lane_reads_lost_markers()
    -> Result<(), Box<dyn std::error::Error>> {
        a_heard_line_evicted_from_the_ears_lane_reads_lost(HeardFrom::Markers)
    }

    #[test]
    fn a_heard_line_evicted_from_the_ears_lane_reads_lost_tailed()
    -> Result<(), Box<dyn std::error::Error>> {
        a_heard_line_evicted_from_the_ears_lane_reads_lost(HeardFrom::Tailed)
    }

    #[test]
    fn heard_lines_lost_with_a_broken_attempt_read_lost_markers()
    -> Result<(), Box<dyn std::error::Error>> {
        heard_lines_lost_with_a_broken_attempt_read_lost(HeardFrom::Markers)
    }

    #[test]
    fn heard_lines_lost_with_a_broken_attempt_read_lost_tailed()
    -> Result<(), Box<dyn std::error::Error>> {
        heard_lines_lost_with_a_broken_attempt_read_lost(HeardFrom::Tailed)
    }

    /// A phrase the capture dropped itself (its `TULPA-EARS-DROP` record) lands in flow.log with
    /// the capture's reason, and never in the outbox; a malformed record is noise.
    #[test]
    fn a_capture_drop_record_lands_in_flow_only() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("drop-records")?;
        let s = settings(
            &dir,
            "printf 'TULPA-EARS-DROP prompt-echo [me] Продолжение следует, подписывайтесь\\n'; \
             printf 'TULPA-EARS-DROP Bad-Why [me] не запись\\n'; \
             printf 'TULPA-EARS-LINE 1000 2500 12:00:10 [me] после сброса\\n'; sleep 30",
        );
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let ears_lines = |text: &str| {
            crate::flow::test_entries(|e| {
                fl(e, "/kind") == "ears"
                    && fl(e, "/frame").as_str().is_some_and(|f| f.contains(text))
            })
        };
        // The phrase after the records goes: by then both records were read.
        let started = Instant::now();
        while ears_lines("после сброса").is_empty() {
            if started.elapsed() > Duration::from_secs(5) {
                return Err("the phrase after the drop records never reached flow.log".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let echo = ears_lines("Продолжение следует");
        assert_eq!(echo.len(), 1, "{echo:?}");
        let echo = crate::flow::nth(&echo, 0);
        assert_eq!(
            (
                fl(&echo, "/dir"),
                fl(&echo, "/frame"),
                fl(&echo, "/outcome")
            ),
            (
                "note".into(),
                "[me] Продолжение следует, подписывайтесь".into(),
                "dropped:prompt-echo".into()
            )
        );
        assert!(
            ears_lines("не запись").is_empty(),
            "a malformed record is noise"
        );
        let sent: Vec<String> = outbox
            .drain()
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert!(sent.iter().any(|l| l.contains("после сброса")), "{sent:?}");
        assert!(!sent.iter().any(|l| l.contains("Продолжение")), "{sent:?}");
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A new microphone whose restart fails still stands and is kept: the next start uses it, the link
    /// hears the capture went off.
    #[test]
    fn a_mic_pick_stands_when_the_restart_fails() -> Result<(), Box<dyn std::error::Error>> {
        use crate::devices::{DeviceRole, pick};
        let dir = temp_dir("mic-fail")?;
        let logs = dir.join("logs");
        let mut s = settings(&dir, "sleep 30");
        s.log_file = logs.join("ears.log");
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let pid = pid_of(&ears.state())?;
        // The log's directory turns into a file: the next start can't open its log.
        fs::remove_dir_all(&logs)?;
        fs::write(&logs, "not a directory")?;
        let path = dir.join("config.toml");
        fs::write(&path, "[ears]\nmode = \"on\"\n")?;
        let line = pick(&ears, &path, DeviceRole::Mic, Some("Example USB Mic"))?;
        assert!(
            line.contains("did not start again") && line.contains("kept as [ears] mic_device"),
            "{line}"
        );
        assert_eq!(ears.mic().as_deref(), Some("Example USB Mic"));
        assert_eq!(ears.state(), EarsState::Off);
        assert!(group_gone_within(pid, Duration::from_secs(1)));
        assert!(fs::read_to_string(&path)?.contains("mic_device = \"Example USB Mic\""));
        let told: Vec<Value> = outbox
            .drain()
            .iter()
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .collect();
        assert!(
            told.contains(
                &json!({"event": "ears", "state": "mode", "mode": "off", "from": "call", "source": "mic"})
            ),
            "the link hears the capture went off: {told:?}"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A pick from the menu applies at once and is kept in the config: one line changed or added,
    /// every other line as it was.
    #[test]
    fn a_menu_pick_applies_and_is_kept_in_the_config() -> Result<(), Box<dyn std::error::Error>> {
        use crate::devices::{DeviceRole, pick};
        let dir = temp_dir("pick")?;
        let path = dir.join("config.toml");
        let original = "# my config\n[ears]\nmode = \"on\" # listening\nmic_device = \"Old Mic\"\n\n[voice]\nenabled = false\n";
        fs::write(&path, original)?;
        let (ears, _, _) = organ(Ok(settings(&dir, "true")));
        let line = pick(&ears, &path, DeviceRole::Mic, Some("Example USB Mic"))?;
        assert!(line.contains("kept as [ears] mic_device"), "{line}");
        assert_eq!(ears.mic().as_deref(), Some("Example USB Mic"));
        pick(&ears, &path, DeviceRole::Output, Some("Desk Speakers"))?;
        assert_eq!(ears.output().as_deref(), Some("Desk Speakers"));
        assert_eq!(
            fs::read_to_string(&path)?,
            original
                .replace("\"Old Mic\"", "\"Example USB Mic\"")
                .replace("[voice]\n", "[voice]\noutput_device = \"Desk Speakers\"\n")
        );
        pick(&ears, &path, DeviceRole::Output, None)?;
        assert_eq!(ears.output(), None);
        assert!(
            fs::read_to_string(&path)?.contains("output_device = \"\"\n"),
            "the default is empty"
        );
        let kept = fs::read_to_string(&path)?;
        assert!(pick(&ears, &path, DeviceRole::Mic, Some("a\u{7}b")).is_err());
        assert_eq!(
            fs::read_to_string(&path)?,
            kept,
            "a refused pick writes nothing"
        );
        let missing = pick(
            &ears,
            &dir.join("gone.toml"),
            DeviceRole::Mic,
            Some("Example USB Mic"),
        )?;
        assert!(
            missing.contains("NOT kept"),
            "applied, but said it is not kept: {missing}"
        );
        let (off, _, _) = organ(Err("ears: off in config".to_owned()));
        assert!(pick(&off, &path, DeviceRole::Mic, None).is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The device names come from `[ears] mic_device` and `[voice] output_device`; empty is the default.
    #[test]
    fn picked_devices_come_from_the_config() -> Result<(), Box<dyn std::error::Error>> {
        let base = "[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\n";
        let home = Path::new("/tmp/h");
        let config = TrayAppConfig::from_toml(&format!(
            "{base}mic_device = \"Example USB Mic\"\n[voice]\noutput_device = \"Desk Speakers\"\n"
        ))?;
        let s = EarsSettings::from_config(&config, home, PathBuf::from("/tmp/l"))?;
        assert_eq!(s.mic_device.as_deref(), Some("Example USB Mic"));
        assert_eq!(s.output_device.as_deref(), Some("Desk Speakers"));
        let (ears, _, _) = organ(Ok(s));
        assert_eq!(
            ears.output().as_deref(),
            Some("Desk Speakers"),
            "the organ starts with them"
        );
        let empty = TrayAppConfig::from_toml(&format!("{base}mic_device = \"\"\n"))?;
        let s = EarsSettings::from_config(&empty, home, PathBuf::from("/tmp/l"))?;
        assert_eq!((s.mic_device, s.output_device), (None, None));
        let bad = TrayAppConfig::from_toml(&format!("{base}mic_device = \"a\\tb\"\n"))?;
        assert!(
            EarsSettings::from_config(&bad, home, PathBuf::from("/tmp/l"))
                .is_err_and(|why| why.contains("mic_device"))
        );
        Ok(())
    }

    /// Only a resident that plays itself gets the output, as `--device <name>`, and only when its argv
    /// has none; a new pick starts it again with the new argv.
    #[test]
    fn a_playing_resident_gets_the_picked_output_and_restarts_for_a_new_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("device")?;
        let args = dir.join("args.txt");
        let serve = format!(
            "printf '[%s]\\n' \"$*\" >> {}; while read -r line; do echo start 1 1000; echo end 1 2000; echo done 1s; done",
            args.display()
        );
        let mut s = settings(&dir, "true");
        s.voice_serve = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            serve,
            "serve".to_owned(),
        ];
        s.voice_serve_plays = true;
        let plain = s.voice_serve.clone();
        assert_eq!(s.serve_argv(None), plain);
        let mut with = plain.clone();
        with.extend(["--device".to_owned(), "Desk Speakers".to_owned()]);
        assert_eq!(s.serve_argv(Some("Desk Speakers")), with);
        let mut own = s.clone();
        own.voice_serve.push("--device=Headphones".to_owned());
        assert_eq!(
            own.serve_argv(Some("Desk Speakers")),
            own.voice_serve,
            "its own wins"
        );
        let mut rendering = s.clone();
        rendering.voice_serve_plays = false;
        assert_eq!(
            rendering.serve_argv(Some("Desk Speakers")),
            plain,
            "a WAV renderer plays nothing"
        );
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(ears.say(&s, "Раз.")?, "said");
        ears.set_output(Some("Desk Speakers".to_owned()))?;
        assert_eq!(ears.say(&s, "Два.")?, "said");
        assert_eq!(ears.say(&s, "Три.")?, "said");
        assert_eq!(
            fs::read_to_string(&args)?,
            "[]\n[--device Desk Speakers]\n",
            "started again once, for the new pick"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The pane named to fix a side, and the URL a click opens, side by side.
    #[test]
    fn each_side_names_and_opens_its_settings_pane() {
        assert_eq!(Side::Me.settings_pane(), "Privacy & Security → Microphone");
        assert_eq!(
            Side::Me.settings_url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        );
        assert_eq!(
            Side::Them.settings_pane(),
            "Privacy & Security → Screen & System Audio Recording"
        );
        assert_eq!(
            Side::Them.settings_url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        );
    }

    fn degraded(side: Side, reason: &str) -> Health {
        Health::Degraded {
            side,
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn self_check_puts_each_error_on_its_side() {
        let now = SystemTime::now();
        let fresh = DirLook::Changed(now - Duration::from_secs(3));
        assert_eq!(
            self_check(&fresh, &fresh, None, false, SystemTime::UNIX_EPOCH, now),
            Health::Hearing
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("\n  \n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            Health::Hearing
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("sysaudio: capturing system audio, s16le 16k mono\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            Health::Hearing,
            "the capture's info line is not an error (seen live on the Mac)"
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some(
                    "sysaudio: capturing system audio, s16le 16k mono\nSCStreamErrorDomain -3801: user declined\n"
                ),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(
                Side::Them,
                "capture error: SCStreamErrorDomain -3801: user declined"
            ),
            "a screen-capture refusal fails the system audio, not the microphone"
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("old line\nmic: permission denied\n\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(Side::Me, "capture error: mic: permission denied"),
            "the microphone failing, the system audio fine"
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("sysaudio: no display\nmic: capturing\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(Side::Them, "capture error: sysaudio: no display"),
            "the microphone's newer info line does not clear the system audio's error"
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("sysaudio: no display\nsysaudio: capturing system audio\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            Health::Hearing,
            "the side's own info line after its error: recovered"
        );
        assert_eq!(
            self_check(
                &fresh,
                &fresh,
                Some("starting\nfatal: out of memory\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            Health::Deaf {
                reason: "capture error: fatal: out of memory".to_owned()
            },
            "an error naming neither side counts for both"
        );
    }

    #[test]
    fn self_check_judges_each_directory_on_its_own() {
        let now = SystemTime::now();
        let fresh = DirLook::Changed(now - Duration::from_secs(3));
        let stale = DirLook::Changed(now - Duration::from_secs(45));
        assert_eq!(
            self_check(&stale, &fresh, None, false, SystemTime::UNIX_EPOCH, now),
            degraded(Side::Me, "no chunk rotated for 45 s")
        );
        assert_eq!(
            self_check(&fresh, &stale, None, false, SystemTime::UNIX_EPOCH, now),
            degraded(Side::Them, "no chunk rotated for 45 s")
        );
        assert_eq!(
            self_check(
                &fresh,
                &stale,
                Some("screen capture: not permitted\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(Side::Them, "capture error: screen capture: not permitted"),
            "the side's error line is a better reason than its stale directory"
        );
        assert_eq!(
            self_check(&stale, &stale, None, false, SystemTime::UNIX_EPOCH, now),
            Health::Deaf {
                reason: "me: no chunk rotated for 45 s; them: no chunk rotated for 45 s".to_owned()
            }
        );
        assert_eq!(
            self_check(
                &stale,
                &fresh,
                Some("sysaudio: no display\n"),
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            Health::Deaf {
                reason: "me: no chunk rotated for 45 s; them: capture error: sysaudio: no display"
                    .to_owned()
            },
            "both sides failing, each for its own reason"
        );
        let edge = DirLook::Changed(now - FRESH_WITHIN);
        assert_eq!(
            self_check(&edge, &edge, None, false, SystemTime::UNIX_EPOCH, now),
            Health::Hearing
        );
        let future = DirLook::Changed(now + Duration::from_secs(5));
        assert_eq!(
            self_check(&future, &future, None, false, SystemTime::UNIX_EPOCH, now),
            Health::Hearing
        );
        assert_eq!(
            self_check(
                &DirLook::Unreadable("gone".to_owned()),
                &fresh,
                None,
                false,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(Side::Me, "directory unreadable: gone")
        );
    }

    /// A closed lid names itself as the cause of a directory that stopped, and of nothing else.
    #[test]
    fn a_closed_lid_is_named_where_chunks_stopped() {
        let now = SystemTime::now();
        let fresh = DirLook::Changed(now - Duration::from_secs(3));
        let stale = DirLook::Changed(now - Duration::from_secs(45));
        assert_eq!(
            self_check(&stale, &fresh, None, true, SystemTime::UNIX_EPOCH, now),
            degraded(Side::Me, "no chunk rotated for 45 s (lid closed)")
        );
        assert_eq!(
            self_check(&stale, &stale, None, true, SystemTime::UNIX_EPOCH, now),
            Health::Deaf {
                reason: "me: no chunk rotated for 45 s (lid closed); them: no chunk rotated for 45 s (lid closed)"
                    .to_owned()
            }
        );
        assert_eq!(
            self_check(&stale, &fresh, Some("sysaudio: no display\n"), true, SystemTime::UNIX_EPOCH, now),
            Health::Deaf {
                reason: "me: no chunk rotated for 45 s (lid closed); them: capture error: sysaudio: no display"
                    .to_owned()
            },
            "an error line stays as the capture wrote it"
        );
        assert_eq!(
            self_check(&fresh, &fresh, None, true, SystemTime::UNIX_EPOCH, now),
            Health::Hearing,
            "a closed lid with chunks still rotating is no fault"
        );
        assert_eq!(
            self_check(
                &DirLook::Unreadable("gone".to_owned()),
                &fresh,
                None,
                true,
                SystemTime::UNIX_EPOCH,
                now
            ),
            degraded(Side::Me, "directory unreadable: gone")
        );
    }

    #[test]
    fn error_lines_are_put_on_their_side() {
        for (line, side) in [
            ("sysaudio: no display", Some(Side::Them)),
            ("sysaudio: Error Domain=x Code=-3801", Some(Side::Them)),
            ("SCStreamErrorDomain -3801: user declined", Some(Side::Them)),
            ("screen capture: not permitted", Some(Side::Them)),
            ("them: stopped", Some(Side::Them)),
            ("mic: permission denied", Some(Side::Me)),
            ("me: device lost", Some(Side::Me)),
            ("Microphone access denied", Some(Side::Me)),
            ("fatal: dynamic library not loaded", None),
            ("starting", None),
            ("mic and system audio failed", None),
        ] {
            assert_eq!(side_of(line), side, "{line}");
        }
        assert_eq!(Side::Me.other(), Side::Them);
        assert_eq!(Side::Them.id(), "them");
    }

    /// Seen live: the chunk being written stays 0 bytes and closed
    /// ones are taken away — an empty newest file, or no file at all, is alive
    /// as long as the directory keeps changing.
    #[test]
    fn look_dir_sees_rotation_not_size() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("look")?;
        let now = SystemTime::now();
        let alive = |look: &DirLook| matches!(look, DirLook::Changed(at) if now.duration_since(*at).unwrap_or(Duration::ZERO) <= FRESH_WITHIN);
        assert!(alive(&look_dir(&dir)), "a fresh empty dir: just rotated");
        assert!(matches!(
            look_dir(&dir.join("missing")),
            DirLook::Unreadable(_)
        ));
        fs::create_dir(dir.join("sub"))?;
        fs::write(dir.join("c000019.wav"), b"")?;
        let look = look_dir(&dir);
        assert!(alive(&look), "a 0-byte chunk being written is alive");
        assert_eq!(
            self_check(
                &look,
                &look,
                None,
                false,
                SystemTime::UNIX_EPOCH,
                SystemTime::now()
            ),
            Health::Hearing
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn off_refuses_every_ears_and_voice_verb() -> Result<(), Box<dyn std::error::Error>> {
        let (ears, _, _) = organ(Err("ears: off in config".to_owned()));
        for verb in ["ears.start", "ears.stop", "ears.status", "voice.say"] {
            let reply = ask(&ears, verb)?;
            assert_eq!(at(&reply, "/ok"), false, "{verb}");
            assert_eq!(at(&reply, "/err"), "ears: off in config", "{verb}");
            assert_eq!(at(&reply, "/id"), "t1");
        }
        assert_eq!(ears.line(false), "ears: off");
        Ok(())
    }

    #[test]
    fn status_tells_the_keepawake_once_it_is_reported() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("keepawake")?;
        let (ears, _, _) = organ(Ok(settings(&dir, "sleep 30")));
        assert_eq!(
            at(&ask(&ears, "ears.status")?, "/result/keepawake"),
            Value::Null
        );
        ears.report_keepawake(Box::new(|| "on (always)".to_owned()));
        let status = ask(&ears, "ears.status")?;
        assert_eq!(at(&status, "/result/keepawake"), "on (always)");
        assert_eq!(at(&status, "/result/state"), "off");
        assert_eq!(at(&status, "/result/mode"), "off");
        Ok(())
    }

    #[test]
    fn status_carries_evidence_of_the_capture_and_the_last_line()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("evidence")?;
        let (ears, _, _) = organ(Ok(settings(&dir, "sleep 30")));

        // No capture claimed: no evidence fields at all, on any state.
        let idle = ask(&ears, "ears.status")?;
        assert_eq!(at(&idle, "/result/state"), "off");
        assert_eq!(at(&idle, "/result/alive"), Value::Null);
        assert_eq!(at(&idle, "/result/last_line_utc"), Value::Null);
        assert_eq!(at(&idle, "/result/last_line_age_s"), Value::Null);

        // A capture claimed, no line published yet: the pid is probed live,
        // the line evidence says "never" rather than staying absent.
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let running = ask(&ears, "ears.status")?;
        assert_eq!(at(&running, "/result/state"), "running");
        assert_eq!(at(&running, "/result/alive"), true);
        assert_eq!(at(&running, "/result/last_line_utc"), Value::Null);
        assert_eq!(at(&running, "/result/last_line_age_s"), Value::Null);

        // A real line published 10 s ago: both line fields appear, the age
        // measured against now. A test line (`ears.test`) would not count.
        ears.mark_line_published(SystemTime::now() - Duration::from_secs(10));
        let heard = ask(&ears, "ears.status")?;
        let utc = at(&heard, "/result/last_line_utc");
        assert!(utc.as_str().is_some_and(|utc| utc.ends_with('Z')), "{utc}");
        let age = at(&heard, "/result/last_line_age_s");
        let age = age.as_u64().unwrap_or(u64::MAX);
        assert!((10..=12).contains(&age), "age {age}");

        // Stopped: back to no claim, no evidence.
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/result"), "stopped");
        let stopped = ask(&ears, "ears.status")?;
        assert_eq!(at(&stopped, "/result/alive"), Value::Null);
        assert_eq!(at(&stopped, "/result/last_line_age_s"), Value::Null);
        Ok(())
    }

    #[test]
    fn pid_alive_sees_a_live_and_a_reaped_process() -> Result<(), Box<dyn std::error::Error>> {
        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let pid = child.id();
        assert!(pid_alive(pid), "a just-spawned child is alive");
        child.kill()?;
        child.wait()?;
        assert!(!pid_alive(pid), "a reaped child is gone");
        assert!(!pid_alive(u32::MAX), "no such pid");
        Ok(())
    }

    #[test]
    fn start_then_stop_kills_the_whole_group() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("stop")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30 & sleep 30")));
        assert_eq!(at(&ask(&ears, "ears.status")?, "/result/state"), "off");
        let started = ask(&ears, "ears.start")?;
        assert_eq!(at(&started, "/ok"), true, "{started}");
        let pid = pid_of(&ears.state())?;
        assert_eq!(
            at(&ask(&ears, "ears.start")?, "/result"),
            format!("already running (pid {pid})")
        );
        let status = ask(&ears, "ears.status")?;
        assert_eq!(at(&status, "/result/state"), "running");
        assert_eq!(at(&status, "/result/pid"), pid);
        assert!(group_alive(pid));

        let stopping = Instant::now();
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/result"), "stopped");
        assert!(
            group_gone_within(pid, Duration::from_secs(1)),
            "the whole group is gone within a second"
        );
        assert!(stopping.elapsed() < Duration::from_secs(2));
        assert_eq!(ears.state(), EarsState::Off);
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/result"), "not running");
        assert!(
            not_mode(outbox.drain()).is_empty(),
            "an asked-for stop is no event but the mode change"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// `env` reaches the capture either way; `inherit_env` keeps the tray's
    /// own variables, the default clears them (mic noise under
    /// the tray, clean under Terminal).
    #[test]
    fn env_and_inherit_env_reach_the_capture() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("env")?;
        let out = dir.join("env.txt");
        let script = format!("env > {}", out.display());
        for inherit in [false, true] {
            let mut s = settings(&dir, &script);
            s.inherit_env = inherit;
            s.env = vec![
                ("TULPA_EARS_TEST".to_owned(), "x1".to_owned()),
                ("TULPA_DEVICE".to_owned(), "not-this".to_owned()),
            ];
            let (ears, _, _) = organ(Ok(s));
            assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
            let started = Instant::now();
            while !fs::read_to_string(&out).is_ok_and(|t| t.contains("TULPA_EARS_TEST"))
                && started.elapsed() < Duration::from_secs(3)
            {
                thread::sleep(Duration::from_millis(50));
            }
            let text = fs::read_to_string(&out)?;
            assert!(text.contains("TULPA_EARS_TEST=x1"), "{text}");
            assert!(
                text.lines().any(|l| l == "TULPA_DEVICE=test-mac"),
                "the device name reaches the capture, inherit_env={inherit}: {text}"
            );
            // The test process's own CARGO_* variables are the tray's here.
            assert_eq!(
                text.contains("CARGO_"),
                inherit,
                "inherit_env={inherit}: {text}"
            );
            let _stopped = ask(&ears, "ears.stop")?;
            fs::remove_file(&out)?;
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// E2 r2 R2: the config's top-level `debug` is the one source of the capture's `TULPA_DEBUG`: on, it is `1`;
    /// off (the default), it is absent even when `[ears] env` sets it.
    #[test]
    fn the_debug_switch_reaches_the_capture_from_the_config_only()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = "[ears]\nmode = \"on\"\ncommand = [\"/bin/true\"]\nme_dir = \"/a\"\nthem_dir = \"/b\"\n";
        let home = Path::new("/tmp/h");
        let on = TrayAppConfig::from_toml(&format!("debug = true\n{base}"))?;
        let off = TrayAppConfig::from_toml(base)?;
        let read = |c: &TrayAppConfig| EarsSettings::from_config(c, home, PathBuf::from("/tmp/l"));
        assert_eq!((read(&on)?.debug, read(&off)?.debug), (true, false));
        let dir = temp_dir("debug")?;
        for debug in [true, false] {
            // The capture writes its environment into a FIFO: the read below waits for that write, no poll.
            let fifo = dir.join(format!("env-{debug}"));
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()?
                    .success()
            );
            let mut s = settings(&dir, &format!("env > '{}'; sleep 60", fifo.display()));
            s.debug = debug;
            s.env = vec![(DEBUG_ENV.to_owned(), "1".to_owned())];
            let (ears, _, _) = organ(Ok(s));
            assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
            let (tx, rx) = std::sync::mpsc::channel();
            let path = fifo.clone();
            thread::spawn(move || {
                let _sent = tx.send(fs::read_to_string(path));
            });
            // A guard against a capture that never starts, not a timing claim.
            let text = rx.recv_timeout(Duration::from_secs(30))??;
            let _stopped = ask(&ears, "ears.stop")?;
            assert!(
                text.lines().any(|l| l == "TULPA_DEVICE=test-mac"),
                "the capture's own environment: {text}"
            );
            let seen: Vec<&str> = text
                .lines()
                .filter(|l| l.starts_with("TULPA_DEBUG="))
                .collect();
            let want: &[&str] = if debug { &["TULPA_DEBUG=1"] } else { &[] };
            assert_eq!(seen, want, "debug={debug}: {text}");
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A window is recorded open while playback runs, closed after; old windows
    /// age out; an utterance overlapping one is the room's echo of our own
    /// voice, with the margins the capture's own gate used.
    #[test]
    fn speaking_windows_open_close_and_age_out() {
        let speaking = Speaking::default();
        speaking.mark(1_000, 0, 1_000);
        speaking.mark(1_000, 4_000, 4_000);
        let windows = speaking
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            windows,
            vec![(1_000, 4_000)],
            "the open window is replaced, not added"
        );
        speaking.mark(200_000, 0, 200_000);
        let windows = speaking
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            windows,
            vec![(200_000, 0)],
            "a window over 2 min old is dropped"
        );
        speaking.mark(400_000, 0, 400_000);
        let windows = speaking
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(windows, vec![(200_000, 0), (400_000, 0)], "{windows:?}");
        // The margins: an utterance ending 1.5 s before the window's
        // end is still echo (the room rings on); one well past it is not.
        let s = Speaking::default();
        s.mark(10_000, 20_000, 30_000);
        assert!(
            s.echoes(8_500, 9_500, 30_000),
            "reaches over the 1 s lead-in"
        );
        assert!(
            s.echoes(20_500, 21_000, 30_000),
            "the 1.5 s tail for the room"
        );
        assert!(!s.echoes(30_000, 31_000, 30_000), "well past the window");
        // An open window reaches to now.
        let open = Speaking::default();
        open.mark(10_000, 0, 50_000);
        assert!(open.echoes(49_000, 50_000, 50_000));
        assert!(!open.echoes(70_000, 71_000, 50_000));
    }

    /// Own voice: the text reaches the synth as one argument (never a shell word), the WAV it prints
    /// is played, post-processing runs on {in}/{out}; no WAV (or a failure) is an error the caller falls
    /// back from.
    #[test]
    fn own_voice_synth_takes_the_text_as_one_arg_and_returns_its_wav()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("voice")?;
        let wav = dir.join("out.wav");
        let got = dir.join("arg.txt");
        let mut s = settings(&dir, "true");
        s.voice_synth = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "printf '%s' \"$1\" > {}; : > {}; echo loaded model; echo {}",
                got.display(),
                wav.display(),
                wav.display()
            ),
            "synth".to_owned(),
            "{text}".to_owned(),
        ];
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(ears.synthesize(&s, "hi; touch /tmp/never")?, wav);
        assert_eq!(
            fs::read_to_string(&got)?,
            "hi; touch /tmp/never",
            "one argument, not shell words"
        );
        s.voice_post = vec!["/bin/cp".to_owned(), "{in}".to_owned(), "{out}".to_owned()];
        assert_eq!(ears.synthesize(&s, "x")?, dir.join("out.post.wav"));
        assert!(
            !wav.exists(),
            "the raw clip goes once the processed one exists"
        );
        discard_clip(&dir.join("out.post.wav"));
        assert!(
            !dir.join("out.post.wav").exists(),
            "the played clip goes too"
        );
        discard_clip(&got);
        assert!(got.exists(), "only a .wav is ever removed");
        // A directory older than the synth run is shared: never a clip dir. One born after it is.
        thread::sleep(Duration::from_millis(1500));
        let before = SystemTime::now();
        assert_eq!(clip_dir(&got, before), None, "the old dir stays");
        let fresh = dir.join("1115-003616-clone");
        fs::create_dir_all(&fresh)?;
        if fs::metadata(&fresh)?.created().is_ok() {
            assert_eq!(
                clip_dir(&fresh.join("c1.wav"), before),
                Some(fresh.as_path())
            );
        }
        s.voice_synth = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo nothing {text}".to_owned(),
        ];
        assert!(
            ears.synthesize(&s, "x")
                .is_err_and(|e| e.contains("no WAV"))
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Streamed speech: sentences are rendered by the resident synth one line
    /// each and played in order, each opening a playback window; every clip and its directory are gone
    /// after; a dead resident is restarted on the next sentence.
    #[test]
    fn say_streams_sentences_through_the_resident_synth_and_cleans_up()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("stream")?;
        let out = dir.join("out");
        fs::create_dir_all(&out)?;
        let heard_log = dir.join("played.log");
        let player = dir.join("player.sh");
        crate::testutil::write_script(
            &player,
            &format!(
                "#!/bin/sh\necho \"$3\" >> {}\nsleep 0.1\n",
                heard_log.display()
            ),
        )?;
        // One directory per sentence, as a real synth may make: out/<n>/c1.wav.
        let serve = format!(
            "n=0; while read -r line; do n=$((n+1)); mkdir -p {o}/$n; printf '%s' \"$line\" > {o}/$n/text; : > {o}/$n/c1.wav; sleep 0.1; echo {o}/$n/c1.wav 0.4s; done",
            o = out.display()
        );
        let mut s = settings(&dir, "true");
        s.voice_serve = vec!["/bin/sh".to_owned(), "-c".to_owned(), serve];
        s.player = player;
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(
            ears.say(
                &s,
                "Первое предложение. Второе предложение! Третье, последнее?"
            )?,
            "said"
        );
        let log = fs::read_to_string(&heard_log)?;
        let order: Vec<&str> = log.lines().collect();
        assert_eq!(
            order,
            vec![
                format!("{}/1/c1.wav", out.display()),
                format!("{}/2/c1.wav", out.display()),
                format!("{}/3/c1.wav", out.display()),
            ],
            "played in order"
        );
        assert_eq!(fs::read_dir(&out)?.count(), 0, "every clip dir is gone");
        assert!(out.is_dir(), "the shared out dir stays");
        let windows = ears
            .speaking
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(windows.len(), 3, "{windows:?}");
        // The resident is kept between calls: numbering goes on.
        ears.say(&s, "Четвёртое предложение.")?;
        assert!(fs::read_to_string(&heard_log)?.contains("/4/c1.wav"));
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0310: the microphone's open utterance is closed (an empty `<state>/me/.close`) only when the last
    /// of overlapping says ends — a failed say counts down too — and never into a `me` dir that isn't there.
    #[test]
    fn the_last_say_to_end_closes_the_microphones_utterance()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("say-close")?;
        let out = dir.join("out");
        fs::create_dir_all(&out)?;
        let heard_log = dir.join("played.log");
        // The long say's first clip plays until the test lets it go: it is surely still playing when the
        // other say ends, however loaded the box (no clip length races the other say).
        let hold = fifo(&dir, "hold.fifo")?;
        let player = dir.join("player.sh");
        crate::testutil::write_script(
            &player,
            &format!(
                "#!/bin/sh\necho \"$3\" >> {}\ncase \"$(cat \"${{3%/*}}/text\")\" in *Раз*) read -r _ < \"{}\";; esac\n",
                heard_log.display(),
                hold.display()
            ),
        )?;
        let serve = format!(
            "n=0; while read -r line; do n=$((n+1)); mkdir -p {o}/$n; printf '%s' \"$line\" > {o}/$n/text; : > {o}/$n/c1.wav; echo {o}/$n/c1.wav 0.4s; done",
            o = out.display()
        );
        let state = dir.join("state");
        let me = state.join("me");
        fs::create_dir_all(&me)?;
        let close = me.join(".close");
        let mut s = settings(&dir, "true");
        s.voice_serve = vec!["/bin/sh".to_owned(), "-c".to_owned(), serve];
        s.player = player;
        s.env = vec![("EARS_STATE".to_owned(), state.display().to_string())];
        assert_eq!(s.me_close_file(), close, "EARS_STATE from [ears] env");
        let (ears, _, _) = organ(Ok(s.clone()));
        let long = {
            let (ears, s) = (Arc::clone(&ears), s.clone());
            thread::spawn(move || ears.say(&s, "Раз. Два. Три. Четыре. Пять."))
        };
        let started = Instant::now();
        while fs::read_to_string(&heard_log).map_or(true, |log| log.is_empty()) {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the long say never played"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ears.say(&s, " \n ").is_err(), "an empty say fails");
        assert_eq!(ears.say(&s, "Шесть.")?, "said");
        assert!(!long.is_finished(), "the long say still plays");
        assert!(!close.exists(), "no close while a say is in flight");
        assert!(let_go(&hold), "the long say's first clip never took its go");
        assert_eq!(long.join().map_err(|_| "the long say panicked")??, "said");
        assert!(close.is_file(), "the last say to end closes the utterance");
        assert_eq!(fs::read(&close)?.len(), 0);
        assert_eq!(ears.saying.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read_dir(&me)?.count(),
            1,
            "nothing but the close: no temporary left"
        );
        // A failed say alone counts down and closes too.
        fs::remove_file(&close)?;
        assert!(ears.say(&s, "").is_err());
        assert!(close.is_file(), "a failed say closes too");
        ears.shutdown();
        // The default state dir, `<home>/.cache/tulpa-ears`, with no `me` in it: nothing is created.
        let mut bare = s;
        bare.env = Vec::new();
        let default = dir.join(".cache/tulpa-ears");
        assert_eq!(bare.me_close_file(), default.join("me/.close"));
        let (quiet, _, _) = organ(Ok(bare.clone()));
        assert_eq!(quiet.say(&bare, "Семь.")?, "said");
        assert!(!default.exists(), "no state dir made");
        quiet.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Streamed voice: a playing resident gets the whole text; its start/end events become
    /// playback windows; a resident that fails before any sound hands over to the pipeline.
    #[test]
    fn a_playing_resident_speaks_the_whole_text_and_marks_each_piece()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("plays")?;
        let got = dir.join("got.txt");
        let serve = format!(
            "while read -r line; do printf '%s\\n' \"$line\" >> {}; echo start 1 1000; echo end 1 2000; echo start 2 2100; echo end 2 3000; echo done 2s; done",
            got.display()
        );
        let mut s = settings(&dir, "true");
        s.voice_serve = vec!["/bin/sh".to_owned(), "-c".to_owned(), serve];
        s.voice_serve_plays = true;
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(ears.say(&s, "Раз, два. Три!")?, "said");
        assert_eq!(
            fs::read_to_string(&got)?,
            "Раз, два. Три!\n",
            "the whole text, one line"
        );
        let windows = ears
            .speaking
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            windows.contains(&(1_000, 2_000)) && windows.contains(&(2_100, 3_000)),
            "{windows:?}"
        );
        // Fails before a sound and nothing else is configured: the error comes back (no say on Linux).
        s.voice_serve = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "read -r l; echo ERR no model".to_owned(),
        ];
        let (ears, _, _) = organ(Ok(s.clone()));
        assert!(ears.say(&s, "Раз.").is_err());
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Whether the resident synth's lock was free when the first resident dropped on this thread during `act` was
    /// reaped ([`BEFORE_REAP`]): `None` if none was.
    fn reaped_unlocked(ears: &Arc<Ears>, act: impl FnOnce()) -> Option<bool> {
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (told, organ) = (std::rc::Rc::clone(&seen), Arc::downgrade(ears));
        BEFORE_REAP.set(Some(Box::new(move || {
            told.set(organ.upgrade().map(|e| e.resident.try_lock().is_ok()));
        })));
        act();
        BEFORE_REAP.set(None);
        seen.get()
    }

    /// #0530 (the gate's F5): no lock is held across a wait. A resident synth is retired (killed, then reaped: a
    /// wait) with its slot's lock let go, on every path that retires one: Quit, a failed speak or render, and one
    /// found stale (another argv). A resident whose reap does not come (in uninterruptible sleep) would otherwise
    /// hold every voice, and Quit, with it.
    #[test]
    fn a_retired_resident_is_reaped_outside_its_lock() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("retire-unlocked")?;
        let serve = |script: &str| vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];
        let plays = "while read -r l; do echo start 1 1000; echo end 1 2000; echo done 1s; done";
        let fails = "while read -r l; do echo ERR no model; done";
        let mut s = settings(&dir, "true");
        s.voice_serve_plays = true;
        // Plays itself (`speak_streamed`): Quit, a failure, a stale one.
        s.voice_serve = serve(plays);
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(ears.say(&s, "Раз.")?, "said");
        assert_eq!(
            reaped_unlocked(&ears, || ears.shutdown()),
            Some(true),
            "quit"
        );
        let mut failing = s.clone();
        failing.voice_serve = serve(fails);
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(
            reaped_unlocked(&ears, || assert!(ears.say(&failing, "Раз.").is_err())),
            Some(true),
            "a failed speak"
        );
        let mut other = s.clone();
        other.voice_serve = serve(&format!("{plays} # another"));
        assert_eq!(ears.say(&s, "Раз.")?, "said");
        assert_eq!(
            reaped_unlocked(&ears, || assert!(ears.say(&other, "Раз.").is_ok())),
            Some(true),
            "a stale speaker"
        );
        ears.shutdown();
        // Renders WAVs (`render`): a failure, a stale one.
        let wav = dir.join("a.wav");
        fs::write(&wav, b"RIFF")?;
        let renders = format!("while read -r l; do echo {}; done", wav.display());
        s.voice_serve_plays = false;
        s.voice_serve = serve(fails);
        let (ears, _, _) = organ(Ok(s.clone()));
        assert_eq!(
            reaped_unlocked(&ears, || assert!(ears.render(&s, "Раз.").is_err())),
            Some(true),
            "a failed render"
        );
        s.voice_serve = serve(&renders);
        assert_eq!(ears.render(&s, "Раз.")?, wav);
        other = s;
        other.voice_serve = serve(&format!("{renders} # another"));
        assert_eq!(
            reaped_unlocked(&ears, || assert!(ears.render(&other, "Раз.").is_ok())),
            Some(true),
            "a stale renderer"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Every switch is checked — chunks on both sides → "check":"ok"; only the
    /// microphone's → "degraded" with the silent side named (the switch stands, the mic is heard).
    /// A start's self-check and its switch check both end when the start ends, woken by it, and say nothing. Their
    /// own clocks (the self-check's first look, the switch check's look for chunks and its limit) are set an hour
    /// out: only the start's end can end them before the 60 s guard.
    #[test]
    fn a_start_s_checks_end_the_moment_the_start_ends() -> Result<(), Box<dyn std::error::Error>> {
        const WAIT: Duration = Duration::from_secs(60);
        const HOUR: Duration = Duration::from_secs(3600);
        let dir = temp_dir("checks-end")?;
        fs::create_dir_all(dir.join("me"))?;
        fs::create_dir_all(dir.join("them"))?;
        let (mut ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        let organ = Arc::get_mut(&mut ears).ok_or("the organ is shared")?;
        (organ.check_after, organ.check_tick, organ.switch_within) = (HOUR, HOUR, HOUR);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let (generation, ended) = ears
            .inner
            .lock()
            .map(|i| (i.generation, Arc::clone(&i.ended)))
            .map_err(|_| "poisoned")?;
        let started_at = SystemTime::now();
        // Plain threads, not a scope: a check that is never woken fails at the guard instead of hanging a join.
        let (done, checks) = std::sync::mpsc::channel();
        for check in ["self", "switch"] {
            let (ears, ended, done) = (Arc::clone(&ears), Arc::clone(&ended), done.clone());
            thread::spawn(move || {
                if check == "self" {
                    ears.check_later(generation, &ended);
                } else {
                    let source = ModeSource::Menu;
                    ears.check_switch(&ended, EarsMode::Call, EarsMode::Off, &source, started_at);
                }
                let _told = done.send(check);
            });
        }
        thread::sleep(Duration::from_millis(150));
        ask(&ears, "ears.stop")?;
        let mut ended_checks = vec![checks.recv_timeout(WAIT)?, checks.recv_timeout(WAIT)?];
        ended_checks.sort_unstable();
        assert_eq!(ended_checks, ["self", "switch"]);
        let events: Vec<Value> = outbox
            .drain()
            .iter()
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .filter(|v| v["state"] == "switched")
            .collect();
        assert!(events.is_empty(), "{events:?}");
        Ok(())
    }

    /// A check's result goes out only for a start still current at the push: once `ended` is set (it is set under
    /// the same lock), nothing is said.
    #[test]
    fn a_check_result_is_said_only_while_its_start_is_current()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("say-if-current")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        let ended = Stop::new();
        let event = json!({"event": "ears", "state": "switched", "check": "ok"});
        ears.say_if_current(&ended, "ears: current", &event);
        ended.set();
        ears.say_if_current(&ended, "ears: over", &event);
        let switched = outbox
            .drain()
            .iter()
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .filter(|v| v["state"] == "switched")
            .count();
        assert_eq!(switched, 1, "only the current start's result");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_switch_is_checked_and_reported() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("switch")?;
        let (me, them) = (dir.join("me"), dir.join("them"));
        fs::create_dir_all(&me)?;
        fs::create_dir_all(&them)?;
        let both = format!(
            "sleep 0.3; touch {}/c1.wav {}/c1.wav; sleep 30",
            me.display(),
            them.display()
        );
        let mic_only = format!("sleep 0.3; touch {}/c2.wav; sleep 30", me.display());
        let mut s = settings(&dir, &both);
        s.modes.push((
            EarsMode::Office,
            vec!["/bin/sh".to_owned(), "-c".to_owned(), mic_only],
        ));
        s.modes.push((
            EarsMode::Stream,
            vec!["/bin/sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
        ));
        let (ears, outbox, _) = organ(Ok(s));
        let mut ears = ears;
        if let Some(organ) = Arc::get_mut(&mut ears) {
            organ.switch_within = Duration::from_millis(1500);
        }
        let switched = |outbox: &Outbox| -> Vec<Value> {
            outbox
                .drain()
                .iter()
                .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
                .filter(|v| v["state"] == "switched")
                .collect()
        };
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            switched(&outbox),
            vec![
                json!({"event": "ears", "state": "switched", "mode": "call", "from": "off", "source": "request:t1", "check": "ok"})
            ]
        );
        // Both dirs are older than the next start now: only the microphone moves.
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "office")?, "/ok"), true);
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(
            switched(&outbox),
            vec![
                json!({"event": "ears", "state": "switched", "mode": "office", "from": "call", "source": "request:t1", "check": "degraded", "side": "them", "reason": "no chunk from the system audio", "fix": "Privacy & Security → Screen & System Audio Recording"})
            ]
        );
        assert_eq!(
            ears.mode(),
            EarsMode::Office,
            "a degraded switch keeps the capture"
        );
        // Neither side makes a chunk: the switch failed.
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "stream")?, "/ok"), true);
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(
            switched(&outbox),
            vec![
                json!({"event": "ears", "state": "switched", "mode": "stream", "from": "office", "source": "request:t1", "check": "failed", "reason": "no chunk on either side"})
            ]
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The hands kill a running child when the link drops; the ears must not.
    #[test]
    fn a_link_drop_does_not_kill_the_capture() -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct Scripted(Vec<Recv>);
        impl Pipe for Scripted {
            fn send(&mut self, _bytes: &[u8]) -> Result<(), LinkError> {
                Ok(())
            }
            fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
                Ok(if self.0.is_empty() {
                    Recv::Eof
                } else {
                    self.0.remove(0)
                })
            }
            fn waker(&self) -> crate::link::Waker {
                std::sync::Arc::new(|| {}) // `recv` never waits
            }
        }
        let dir = temp_dir("drop")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        let handler = route(None, Arc::clone(&ears));
        let mut pipe = Scripted(vec![
            Recv::Timeout,
            Recv::Line(b"{\"id\":\"1\",\"verb\":\"ears.start\"}".to_vec()),
            Recv::Eof,
        ]);
        let stop = Stop::new();
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            Duration::from_millis(10),
            &stop,
            &mut || {},
            handler.as_ref(),
            &outbox,
        );
        assert_eq!(end, End::Dropped);
        thread::sleep(Duration::from_millis(300));
        let pid = pid_of(&ears.state())?;
        assert!(group_alive(pid), "the capture outlives the link");
        ears.shutdown();
        assert!(group_gone_within(pid, Duration::from_secs(1)));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The hub's side of each attempt: the hello is accepted (the grace passes), whatever the Mac writes is kept,
    /// then the pipe ends. The supervisor is stopped after `attempts` attempts.
    struct HubDialer<'a> {
        outbox: &'a Outbox,
        stop: &'a Stop,
        attempts: usize,
        /// The mode lines the hub read, one list per attempt.
        heard: RefCell<Vec<Vec<Value>>>,
    }

    impl crate::link::Dialer for HubDialer<'_> {
        fn attempt(&self, on_connected: &mut dyn FnMut(), stop: &Stop) -> Result<End, LinkError> {
            struct Recording(Vec<Recv>, Vec<Vec<u8>>);
            impl Pipe for Recording {
                fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                    self.1.push(bytes.to_vec());
                    Ok(())
                }
                fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
                    Ok(if self.0.is_empty() {
                        Recv::Eof
                    } else {
                        self.0.remove(0)
                    })
                }
                fn waker(&self) -> crate::link::Waker {
                    Arc::new(|| {}) // `recv` never waits
                }
            }
            let mut pipe = Recording(vec![Recv::Timeout], Vec::new());
            let end = serve_attempt(
                &mut pipe,
                b"hello\n",
                Duration::from_millis(10),
                stop,
                on_connected,
                &refuse_line,
                self.outbox,
            );
            let modes = pipe
                .1
                .iter()
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .filter(|v| v["event"] == "ears" && v["state"] == "mode")
                .collect();
            let mut heard = self.heard.borrow_mut();
            heard.push(modes);
            if heard.len() >= self.attempts {
                self.stop.set();
            }
            Ok(end)
        }
    }

    /// Runs the link's supervisor over a [`HubDialer`], the ears told on each Connected as the tray does;
    /// `down` runs between attempts, while the link is down. Returns the mode lines each attempt delivered.
    fn link_ups(
        ears: &Arc<Ears>,
        outbox: &Outbox,
        dir: &Path,
        attempts: usize,
        down: &mut dyn FnMut(),
    ) -> Vec<Vec<Value>> {
        let stop = Stop::new();
        let dialer = HubDialer {
            outbox,
            stop: &stop,
            attempts,
            heard: RefCell::new(Vec::new()),
        };
        crate::link::supervise(
            &dialer,
            &dir.join("activity.log"),
            None,
            &stop,
            &mut |state| {
                if state == crate::state::LinkState::Connected {
                    ears.link_up();
                }
            },
            &mut |_| down(),
        );
        dialer.heard.into_inner()
    }

    /// #0551b: a menu switch's own `mode` line keeps its exact bytes (glados's wake-ons read it); the tray's
    /// generic switch event (`crate::switch`) is said beside it, never instead.
    #[test]
    fn a_menu_mode_switch_line_keeps_its_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("mode-bytes")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        let _before = outbox.drain();
        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        let mode_lines: Vec<Vec<u8>> = outbox
            .drain()
            .into_iter()
            .filter(|l| String::from_utf8_lossy(l).contains("\"state\":\"mode\""))
            .collect();
        assert_eq!(
            mode_lines,
            vec![b"{\"event\":\"ears\",\"from\":\"off\",\"mode\":\"call\",\"source\":\"menu\",\"state\":\"mode\"}\n".to_vec()]
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Every attempt that reaches Connected tells the hub the current mode once, as `link-up` with `from` equal
    /// to `mode`; nothing else about the mode goes when nothing switched.
    #[test]
    fn each_link_up_tells_the_current_mode_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("link-up")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        let _before = outbox.drain();
        let heard = link_ups(&ears, &outbox, &dir, 2, &mut || {});
        let up = json!({"event": "ears", "state": "mode", "mode": "call", "from": "call", "source": "link-up"});
        assert_eq!(heard, vec![vec![up.clone()], vec![up]]);
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A switch made while the link is down goes out after the next hello, and the hub's last mode line is the
    /// new mode (off here: off is a mode too).
    #[test]
    fn a_switch_while_down_leaves_the_hub_on_the_new_mode() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("link-up-switch")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, "sleep 30")));
        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        let _before = outbox.drain();
        let switcher = Arc::clone(&ears);
        let heard = link_ups(&ears, &outbox, &dir, 2, &mut || {
            assert!(switcher.set_mode(EarsMode::Off, ModeSource::Menu).is_ok());
        });
        assert_eq!(
            heard,
            vec![
                vec![
                    json!({"event": "ears", "state": "mode", "mode": "call", "from": "call", "source": "link-up"})
                ],
                vec![
                    json!({"event": "ears", "state": "mode", "mode": "off", "from": "call", "source": "menu"}),
                    json!({"event": "ears", "state": "mode", "mode": "off", "from": "off", "source": "link-up"}),
                ],
            ]
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn pause_kills_the_capture_refuses_and_tells_the_link() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("pause")?;
        let (ears, outbox, paused) = organ(Ok(settings(&dir, "sleep 30")));
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let pid = pid_of(&ears.state())?;
        paused.set();
        ears.pause();
        assert!(group_gone_within(pid, Duration::from_secs(1)));
        assert_eq!(ears.line(true), "ears: paused");
        let lines = not_mode(outbox.drain());
        assert_eq!(
            lines,
            vec![b"{\"event\":\"ears\",\"reason\":\"paused\",\"state\":\"off\"}\n".to_vec()]
        );
        for verb in ["ears.start", "voice.say"] {
            assert_eq!(at(&ask(&ears, verb)?, "/err"), "paused", "{verb}");
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0551: Resume never restarts the ears a Pause stopped, and the menu line and the resume event's flag say
    /// so until a mode is set again; a Pause that found the ears off holds nothing.
    #[test]
    fn resume_leaves_the_ears_off_and_says_how_to_turn_them_on()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("pause-resume")?;
        let (ears, outbox, paused) = organ(Ok(settings(&dir, "sleep 30")));
        paused.set();
        ears.pause();
        paused.clear();
        assert!(!ears.off_by_pause(), "nothing was running");
        assert_eq!(ears.line(false), "ears: ready (not listening)");

        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        let pid = pid_of(&ears.state())?;
        paused.set();
        ears.pause();
        assert!(group_gone_within(pid, Duration::from_secs(1)));
        paused.clear();
        let _before = outbox.drain();
        assert!(ears.off_by_pause());
        assert_eq!(ears.state(), EarsState::Off);
        assert_eq!(ears.line(false), format!("ears: {EARS_OFF_AFTER_PAUSE}"));
        // Nothing turns them on by itself: no capture, no mode event, a while after the resume.
        thread::sleep(Duration::from_millis(300));
        assert_eq!(ears.state(), EarsState::Off);
        assert_eq!(outbox.drain(), Vec::<Vec<u8>>::new());

        // A refused ask sets no mode: the hold stays (GLM r1 F1 — Office is not configured here).
        assert!(ears.set_mode(EarsMode::Office, ModeSource::Menu).is_err());
        assert!(ears.off_by_pause(), "a refused ask keeps the hold");
        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        assert!(!ears.off_by_pause(), "a mode set ends the hold");
        assert!(ears.line(false).starts_with("ears: listening"));
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Sets a capture directory and its files `ago` into the past: that side stopped making chunks.
    fn make_stale(dir: &Path, ago: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let then = SystemTime::now()
            .checked_sub(ago)
            .ok_or("clock before the epoch")?;
        for entry in fs::read_dir(dir)? {
            File::options()
                .write(true)
                .open(entry?.path())?
                .set_modified(then)?;
        }
        File::open(dir)?.set_modified(then)?;
        Ok(())
    }

    /// Moves the running capture's start `ago` into the past: it has been capturing that long.
    fn backdate(ears: &Ears, ago: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = ears.inner.lock().map_err(|e| e.to_string())?;
        match &mut inner.state {
            EarsState::Running { since, .. }
            | EarsState::Degraded { since, .. }
            | EarsState::Deaf { since, .. } => {
                *since = since.checked_sub(ago).ok_or("clock before the epoch")?;
                Ok(())
            }
            state => Err(format!("no running capture: {state:?}").into()),
        }
    }

    /// The events other than mode changes, parsed.
    fn events(outbox: &Outbox) -> Vec<Value> {
        not_mode(outbox.drain())
            .iter()
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .collect()
    }

    fn append(path: &Path, text: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(text.as_bytes())
    }

    /// The transcript-line events the outbox gets within `limit` (and a moment after the first),
    /// each `ts` checked to be ISO-8601 UTC and taken out.
    fn lines_within(outbox: &Outbox, limit: Duration) -> Vec<Value> {
        let lines = |outbox: &Outbox| -> Vec<Value> {
            events(outbox)
                .into_iter()
                .filter(|e| e.pointer("/state") == Some(&json!("line")))
                .collect()
        };
        let started = Instant::now();
        let mut got = Vec::new();
        while started.elapsed() < limit {
            got.extend(lines(outbox));
            if !got.is_empty() {
                thread::sleep(Duration::from_millis(150));
                got.extend(lines(outbox));
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        for event in &mut got {
            let ts = event.as_object_mut().and_then(|e| e.remove("ts"));
            let ts = ts.as_ref().and_then(Value::as_str).unwrap_or_default();
            assert!(
                ts.len() == 20 && ts.starts_with("20") && ts.ends_with('Z'),
                "ts {ts:?}"
            );
        }
        got
    }

    fn line(side: &str, mode: &str, text: &str) -> Value {
        json!({"event": "ears", "state": "line", "side": side, "mode": mode, "text": text})
    }

    /// An organ tailing `transcript` every 20 ms, with an alone and a call capture.
    fn line_organ(dir: &Path, transcript: &Path) -> (Arc<Ears>, Arc<Outbox>, Arc<Stop>) {
        let mut s = settings(dir, "sleep 30");
        s.modes.push((EarsMode::Alone, s.argv.clone()));
        s.transcript_files = vec![transcript.to_path_buf()];
        let (mut ears, outbox, paused) = organ(Ok(s));
        if let Some(organ) = Arc::get_mut(&mut ears) {
            organ.tail_every = Duration::from_millis(20);
            organ.check_after = Duration::from_secs(3600);
        }
        (ears, outbox, paused)
    }

    /// A few looks of the tail at the capture just started: every line of a look goes only once
    /// the same alone capture ran since the look before.
    fn settle() {
        thread::sleep(Duration::from_millis(300));
    }

    /// `at` as the transcripts write their times: `HH:MM:SS` UTC.
    fn utc_hms(at: SystemTime) -> String {
        let secs = at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3_600 % 24,
            secs / 60 % 60,
            secs % 60
        )
    }

    /// Alone: the microphone's new line goes once, with its side, its speaker tag as `speaker`,
    /// and the text without time, side and tag; the system audio's stays (no call), and what the
    /// file held before is old.
    #[test]
    fn alone_sends_only_the_microphones_lines() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("alone")?;
        let transcript = dir.join("call-me.test-mac.txt");
        fs::write(&transcript, "11:59:00 [me] an old line\n")?;
        let (ears, outbox, _) = line_organ(&dir, &transcript);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        settle();
        append(
            &transcript,
            "12:00:04 [me] [owner] just thinking aloud\n12:00:08 [them] Tulpa, a question\n",
        )?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![
                json!({"event": "ears", "state": "line", "side": "me", "speaker": "owner",
                "mode": "alone", "text": "just thinking aloud"})
            ],
            "the system audio's words need a call (vany 2026-11-18)"
        );
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            lines_within(&outbox, Duration::ZERO),
            Vec::<Value>::new(),
            "a line is sent once"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A call: every line of both sides goes, each once, wake word or not; none while paused or
    /// stopped, and not later.
    #[test]
    fn a_call_sends_every_side_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("call-lines")?;
        let transcript = dir.join("call-them.test-mac.txt");
        let (ears, outbox, paused) = line_organ(&dir, &transcript);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        append(
            &transcript,
            "12:00:00 [them] nothing for us\n12:00:04 [them] Hey TULPA, are you there?\n12:00:06 [me] and my own words\n",
        )?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![
                line("them", "call", "nothing for us"),
                line("them", "call", "Hey TULPA, are you there?"),
                line("me", "call", "and my own words")
            ]
        );
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            lines_within(&outbox, Duration::ZERO),
            Vec::<Value>::new(),
            "a line is sent once"
        );
        paused.set();
        append(&transcript, "12:00:12 [me] tulpa while paused\n")?;
        thread::sleep(Duration::from_millis(200));
        paused.clear();
        let _stopped = ask(&ears, "ears.stop")?;
        append(&transcript, "12:00:16 [me] tulpa while stopped\n")?;
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            lines_within(&outbox, Duration::from_millis(300)),
            Vec::<Value>::new()
        );
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        append(&transcript, "12:00:20 [me] tulpa, back\n")?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![line("me", "call", "tulpa, back")],
            "listening again: only what comes now"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A `[them]` line the last mode already heard stays local when the mode
    /// has since turned to a call: the words are stamped before this capture
    /// began — a video's, a song's — so the call does not take them, while a
    /// line said while it runs goes.
    #[test]
    fn a_them_line_from_before_the_call_stays() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("call-since")?;
        let transcript = dir.join("call-them.test-mac.txt");
        let (ears, outbox, _) = line_organ(&dir, &transcript);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        settle();
        let stamp = utc_hms(SystemTime::now() - Duration::from_secs(10));
        append(&transcript, &format!("{stamp} [them] a video's words\n"))?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(2)),
            Vec::<Value>::new(),
            "said ten seconds before the call began: stays local"
        );
        let stamp = utc_hms(SystemTime::now());
        append(
            &transcript,
            &format!("{stamp} [them] the call's own words\n"),
        )?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![line("them", "call", "the call's own words")],
            "said while the call runs: goes"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// `ears.test`: one microphone line in the current mode, marked as a test, through the real
    /// lines' path and whatever the mode filter (no wake words, a call, `transcript_events =
    /// "none"`, paused); empty text is refused, and every test is refused with the ears off.
    #[test]
    fn a_test_line_always_goes_marked_as_a_test() -> Result<(), Box<dyn std::error::Error>> {
        let test_line = |mode: &str, text: &str| {
            let mut event = line("me", mode, text);
            if let Some(fields) = event.as_object_mut() {
                fields.insert("test".to_owned(), Value::Bool(true));
            }
            event
        };
        let dir = temp_dir("test-line")?;
        let transcript = dir.join("call-me.test-mac.txt");
        let (ears, outbox, paused) = line_organ(&dir, &transcript);
        assert_eq!(
            ask_arg(&ears, "ears.test", "hello\tthere ")?,
            json!({"id": "t1", "ok": true, "result": "sent"})
        );
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(1)),
            vec![test_line("off", "hello there")],
            "no capture: the mode is off"
        );
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        assert_eq!(
            at(&ask_arg(&ears, "ears.test", "in a call")?, "/result"),
            "sent"
        );
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(1)),
            vec![test_line("call", "in a call")]
        );
        paused.set();
        assert_eq!(
            at(&ask_arg(&ears, "ears.test", "paused")?, "/result"),
            "sent"
        );
        paused.clear();
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(1)),
            vec![test_line("call", "paused")],
            "gated as ears.status is"
        );
        let refused = ask_arg(&ears, "ears.test", " \n ")?;
        assert_eq!(
            (at(&refused, "/ok"), at(&refused, "/err")),
            (json!(false), json!("arg"))
        );
        let _stopped = ask(&ears, "ears.stop")?;

        let (off, off_outbox, _) = organ(Err("ears: [ears] is off".to_owned()));
        let refused = ask_arg(&off, "ears.test", "hello")?;
        assert_eq!(
            (at(&refused, "/ok"), at(&refused, "/err")),
            (json!(false), json!("ears: [ears] is off"))
        );
        assert!(
            off_outbox.drain().is_empty(),
            "nothing sent with the ears off"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The side decides in a call too (the microphone goes), and the files are read whenever they
    /// are listed — an alone capture no longer gates the tail.
    #[test]
    fn the_side_decides_and_files_alone_are_read() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("no-wake")?;
        let transcript = dir.join("call-me.test-mac.txt");
        let (ears, outbox, _) = line_organ(&dir, &transcript);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        append(&transcript, "12:00:00 [me] tulpa, in a call\n")?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![line("me", "call", "tulpa, in a call")]
        );
        let _stopped = ask(&ears, "ears.stop")?;

        let mut s = settings(&dir, "sleep 30");
        s.transcript_files = vec![transcript];
        let (ears, _, _) = organ(Ok(s));
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        assert!(
            ears.tailing.load(Ordering::SeqCst),
            "files listed is all a line needs"
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A system-audio line not yet read when the mode turns alone stays: the side's lines need
    /// the call still running when they are read.
    #[test]
    fn a_them_line_unread_when_the_mode_turns_alone_stays() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("switch-lines")?;
        let transcript = dir.join("call-them.test-mac.txt");
        let (mut ears, outbox, _) = line_organ(&dir, &transcript);
        if let Some(organ) = Arc::get_mut(&mut ears) {
            organ.tail_every = Duration::from_millis(300);
        }
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        thread::sleep(Duration::from_millis(400));
        append(&transcript, "12:00:00 [them] a caller's last words\n")?;
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        assert_eq!(
            lines_within(&outbox, Duration::from_millis(800)),
            Vec::<Value>::new()
        );
        append(&transcript, "12:00:04 [me] alone now\n")?;
        assert_eq!(
            lines_within(&outbox, Duration::from_secs(3)),
            vec![line("me", "alone", "alone now")]
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A journal that hands each line on: a test waits for the line it expects, not for a time.
    struct Tells(Mutex<std::sync::mpsc::Sender<String>>);

    impl Journal for Tells {
        fn record(&self, line: &str) {
            let _told = self.0.lock().map(|tx| tx.send(line.to_owned()));
        }
    }

    /// A timer pushed out of a test's way.
    const HOUR: Duration = Duration::from_secs(3600);

    /// The test organ, `tune`d before it is shared, its journal lines on the returned channel.
    fn organ_told(
        settings: EarsSettings,
        tune: impl FnOnce(Ears) -> Ears,
    ) -> (Arc<Ears>, Arc<Outbox>, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let outbox = Arc::new(Outbox::default());
        let ears = Ears::new(
            Ok(settings),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::new(Tells(Mutex::new(tx))),
            Box::new(|| {}),
        );
        (Arc::new(tune(ears)), outbox, rx)
    }

    /// No switch check: a start's check for fresh chunks would tell `switched` into a health test's outbox
    /// whenever the test outlasts `switch_within` or writes the chunks it looks for.
    fn no_switch_check(mut ears: Ears) -> Ears {
        (ears.check_tick, ears.switch_within) = (HOUR, HOUR);
        ears
    }

    /// Checked by hand only: no switch check and no timed self-check, so the test's own `check_once` calls are
    /// the only checks, whatever the load.
    fn by_hand(mut ears: Ears) -> Ears {
        ears.check_after = HOUR;
        no_switch_check(ears)
    }

    /// The first journal line `wanted` takes, the others skipped; each line waited for at most 30 s (a loaded box).
    fn told(
        rx: &std::sync::mpsc::Receiver<String>,
        wanted: impl Fn(&str) -> bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        loop {
            let line = rx.recv_timeout(Duration::from_secs(30))?;
            if wanted(&line) {
                return Ok(line);
            }
        }
    }

    /// What the first timed check journaled, the organ, its outbox and its directory.
    type FirstCheck = (String, Arc<Ears>, Arc<Outbox>, PathBuf);

    /// Both side directories last changed a minute before the start (the previous run's), the first
    /// check `check_after` after it: what that timed check journals, and the organ.
    fn first_check_over_old_dirs(
        name: &str,
        err: Option<&str>,
    ) -> Result<FirstCheck, Box<dyn std::error::Error>> {
        let dir = temp_dir(name)?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
            make_stale(&dir.join(side), Duration::from_secs(60))?;
        }
        if let Some(err) = err {
            fs::write(dir.join("err.log"), err)?;
        }
        let (ears, outbox, rx) = organ_told(settings(&dir, "sleep 30"), |mut ears| {
            ears.check_after = Duration::from_millis(200);
            no_switch_check(ears)
        });
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let checked = told(&rx, |line| {
            line == "ears: self-check passed"
                || line.starts_with("ears: DEAF")
                || line.starts_with("ears: DEGRADED")
        })?;
        Ok((checked, ears, outbox, dir))
    }

    /// F1: a capture that loads a model before it opens a device leaves both directories with the previous
    /// run's times. The first timed check finds this capture not heard yet, which is hearing (no state of its
    /// own): not deaf, not degraded, nothing told.
    #[test]
    fn a_new_capture_is_not_stale_before_it_has_run_fresh_within()
    -> Result<(), Box<dyn std::error::Error>> {
        let (checked, ears, outbox, dir) = first_check_over_old_dirs("not-heard-yet", None)?;
        assert_eq!(checked, "ears: self-check passed");
        assert!(
            matches!(ears.state(), EarsState::Running { .. }),
            "{:?}",
            ears.state()
        );
        assert_eq!(events(&outbox), Vec::<Value>::new());
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// F1: not heard yet ends `FRESH_WITHIN` after the start: the same capture, still nothing new on either side
    /// `FRESH_WITHIN` and a margin after it, is deaf, its age counted from the start (the rule is not off).
    #[test]
    fn a_new_capture_still_silent_past_fresh_within_is_deaf()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_first, ears, outbox, dir) = first_check_over_old_dirs("silent-past-fresh", None)?;
        let _first_told = events(&outbox);
        let margin = Duration::from_secs(5);
        backdate(&ears, FRESH_WITHIN + margin)?;
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        ears.check_once(generation);
        let state = ears.state();
        assert!(
            matches!(&state, EarsState::Deaf { reason, .. } if reason.starts_with("me: no chunk rotated for 25 s; them: no chunk rotated for 25 s")),
            "the age counts from the start, not from the old chunks: {state:?}"
        );
        assert_eq!(
            events(&outbox)
                .iter()
                .map(|e| at(e, "/state"))
                .collect::<Vec<_>>(),
            vec![json!("deaf")]
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// F1: the capture's start does not hold back an error it names: that side degrades at the first check.
    #[test]
    fn a_named_error_degrades_its_side_at_the_first_check() -> Result<(), Box<dyn std::error::Error>>
    {
        let (checked, ears, outbox, dir) = first_check_over_old_dirs(
            "named-error",
            Some("sysaudio: Error Domain=SCStreamErrorDomain Code=-3801 user declined\n"),
        )?;
        let reason =
            "capture error: sysaudio: Error Domain=SCStreamErrorDomain Code=-3801 user declined";
        assert!(checked.starts_with("ears: DEGRADED: them: "), "{checked}");
        assert!(
            matches!(&ears.state(), EarsState::Degraded { side: Side::Them, reason: r, .. } if r == reason),
            "{:?}",
            ears.state()
        );
        assert_eq!(
            events(&outbox),
            vec![
                json!({"event": "ears", "state": "degraded", "side": "them", "reason": reason, "fix": "Privacy & Security → Screen & System Audio Recording"})
            ]
        );
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A screen-capture refusal in the error file and the "them" directory stale, the
    /// microphone fine — DEGRADED on "them", told once; the capture (and its microphone) keeps running;
    /// the recheck turns it back to running once the system audio captures again.
    #[test]
    fn a_failing_system_audio_degrades_and_keeps_the_microphone()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("degraded")?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
        }
        make_stale(&dir.join("them"), Duration::from_secs(60))?;
        fs::write(
            dir.join("err.log"),
            "sysaudio: capturing system audio, s16le 16k mono\nsysaudio: Error Domain=SCStreamErrorDomain Code=-3801 user declined\n",
        )?;
        let (ears, outbox, rx) = organ_told(settings(&dir, "sleep 30"), |mut ears| {
            ears.check_after = Duration::from_millis(200);
            no_switch_check(ears)
        });
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let pid = pid_of(&ears.state())?;
        // The timed check's own word that it degraded: its state and its event are in by then.
        told(&rx, |line| line.starts_with("ears: DEGRADED"))?;
        let state = ears.state();
        let reason =
            "capture error: sysaudio: Error Domain=SCStreamErrorDomain Code=-3801 user declined";
        assert!(
            matches!(&state, EarsState::Degraded { side: Side::Them, reason: r, .. } if r == reason),
            "{state:?}"
        );
        assert!(group_alive(pid), "the capture keeps running");
        assert_eq!(
            ears.line(false),
            format!(
                "ears: DEGRADED — them: {reason} (me still heard) — fix in Privacy & Security → Screen & System Audio Recording (click to open)"
            )
        );
        assert_eq!(
            ears.fix_url(),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"),
            "a click on the line opens the system audio's pane"
        );
        let status = ask(&ears, "ears.status")?;
        assert_eq!(at(&status, "/result/state"), "degraded");
        assert_eq!(at(&status, "/result/side"), "them");
        assert_eq!(
            at(&status, "/result/fix"),
            "Privacy & Security → Screen & System Audio Recording"
        );
        assert_eq!(at(&status, "/result/pid"), pid);
        assert_eq!(at(&status, "/result/mode"), "call");
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        ears.check_once(generation);
        assert_eq!(
            events(&outbox),
            vec![
                json!({"event": "ears", "state": "degraded", "side": "them", "reason": reason, "fix": "Privacy & Security → Screen & System Audio Recording"})
            ],
            "one event, however many rechecks find the same"
        );
        // The capture script's own retry brings the system audio back: its info line, a fresh chunk.
        fs::write(
            dir.join("err.log"),
            "sysaudio: Error Domain=SCStreamErrorDomain Code=-3801 user declined\nsysaudio: capturing system audio, s16le 16k mono\n",
        )?;
        fs::write(dir.join("them").join("c2.wav"), b"")?;
        ears.check_once(generation);
        assert_eq!(
            ears.state(),
            EarsState::Running {
                pid,
                since: ears.state().since().ok_or("since")?
            }
        );
        assert_eq!(
            events(&outbox),
            vec![
                json!({"event": "ears", "state": "running", "reason": "hearing again", "side": "them"})
            ]
        );
        assert_eq!(ears.fix_url(), None, "hearing: the line opens nothing");
        assert!(group_alive(pid));
        let _stopped = ask(&ears, "ears.stop")?;
        assert!(group_gone_within(pid, Duration::from_secs(1)));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Every transition between running, degraded (either side) and deaf, each told once; a new reason in
    /// the same state is no event. Only both sides failing is DEAF.
    #[test]
    fn health_transitions_are_told_once_each() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("transitions")?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
        }
        let err = dir.join("err.log");
        let (ears, outbox, _) = organ_told(settings(&dir, "sleep 30"), by_hand);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let pid = pid_of(&ears.state())?;
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        let mut log = String::new();
        let mut step = |line: &str| -> Result<(EarsState, Vec<Value>), Box<dyn std::error::Error>> {
            log.push_str(line);
            log.push('\n');
            fs::write(&err, &log)?;
            ears.check_once(generation);
            Ok((ears.state(), events(&outbox)))
        };
        // The microphone fails, the system audio is fine: degraded on "me".
        let (state, told) = step("mic: permission denied")?;
        assert!(
            matches!(state, EarsState::Degraded { side: Side::Me, .. }),
            "{state:?}"
        );
        assert_eq!(
            told,
            vec![
                json!({"event": "ears", "state": "degraded", "side": "me", "reason": "capture error: mic: permission denied", "fix": "Privacy & Security → Microphone"})
            ]
        );
        // A new reason on the same side: the menu follows, the link is not told again.
        let (state, told) = step("mic: unable to open the input device")?;
        assert!(
            matches!(&state, EarsState::Degraded { side: Side::Me, reason, .. } if reason == "capture error: mic: unable to open the input device"),
            "{state:?}"
        );
        assert_eq!(told, Vec::<Value>::new());
        // An error naming neither side: both fail, deaf.
        let (state, told) = step("fatal: capture broke")?;
        assert!(matches!(state, EarsState::Deaf { .. }), "{state:?}");
        assert_eq!(
            told,
            vec![
                json!({"event": "ears", "state": "deaf", "reason": "capture error: fatal: capture broke"})
            ]
        );
        assert!(group_alive(pid), "deaf ears are not stopped either");
        // The microphone captures again: degraded on "them" (its newest line is still the error).
        let (state, told) = step("mic: capturing")?;
        assert!(
            matches!(
                state,
                EarsState::Degraded {
                    side: Side::Them,
                    ..
                }
            ),
            "{state:?}"
        );
        assert_eq!(
            told,
            vec![
                json!({"event": "ears", "state": "degraded", "side": "them", "reason": "capture error: fatal: capture broke", "fix": "Privacy & Security → Screen & System Audio Recording"})
            ]
        );
        // The failing side flips within one check: the system audio is back, the microphone fails.
        let (state, told) = step("sysaudio: capturing system audio\nmic: permission denied")?;
        assert!(
            matches!(state, EarsState::Degraded { side: Side::Me, .. }),
            "{state:?}"
        );
        assert_eq!(
            told,
            vec![
                json!({"event": "ears", "state": "degraded", "side": "me", "reason": "capture error: mic: permission denied", "fix": "Privacy & Security → Microphone"})
            ]
        );
        // A restart's info line naming neither side clears both: running, told without a side.
        let (state, told) = step("starting")?;
        assert_eq!(
            state,
            EarsState::Running {
                pid,
                since: state.since().ok_or("since")?
            }
        );
        assert_eq!(
            told,
            vec![
                json!({"event": "ears", "state": "running", "reason": "hearing again", "side": "me"})
            ]
        );
        assert_eq!(ears.line(false), format!("ears: listening (pid {pid})"));
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Both sides failing is DEAF as before, told once, the capture kept; both back is running again.
    /// A lid that answers `closed` and counts how often it was asked.
    #[derive(Debug, Default)]
    struct ClosedLid(std::sync::atomic::AtomicUsize);

    impl Lid for ClosedLid {
        fn closed(&self) -> Option<bool> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some(true)
        }
    }

    /// The organ with a lid: a deaf capture under a closed lid says so in the state, the menu line and the
    /// event; the lid is not asked while both sides rotate.
    #[test]
    fn deaf_under_a_closed_lid_says_lid_closed() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("lid")?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
        }
        let lid = Arc::new(ClosedLid::default());
        let (ears, outbox, _) = organ_told(settings(&dir, "sleep 30"), |ears| {
            by_hand(ears).with_lid(Arc::<ClosedLid>::clone(&lid))
        });
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        ears.check_once(generation);
        assert!(matches!(ears.state(), EarsState::Running { .. }));
        assert_eq!(
            lid.0.load(Ordering::SeqCst),
            0,
            "both sides rotate: the lid is not read"
        );
        make_stale(&dir.join("me"), Duration::from_secs(60))?;
        make_stale(&dir.join("them"), Duration::from_secs(60))?;
        backdate(&ears, Duration::from_secs(60))?;
        ears.check_once(generation);
        let state = ears.state();
        assert!(
            matches!(&state, EarsState::Deaf { reason, .. } if reason.starts_with("me: no chunk rotated for 60 s (lid closed); them: no chunk rotated for 60 s (lid closed)")),
            "{state:?}"
        );
        assert!(
            ears.line(false)
                .starts_with("ears: DEAF — me: no chunk rotated for 60 s (lid closed)")
        );
        let told = events(&outbox);
        assert!(
            told.iter().any(|e| at(e, "/state") == json!("deaf")
                && at(e, "/reason")
                    .as_str()
                    .is_some_and(|r| r.contains("(lid closed)"))),
            "{told:?}"
        );
        assert_eq!(lid.0.load(Ordering::SeqCst), 1);
        let _stopped = ask(&ears, "ears.stop")?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn deaf_turns_back_to_hearing_when_both_sides_recover() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("recover")?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
        }
        make_stale(&dir.join("me"), Duration::from_secs(60))?;
        make_stale(&dir.join("them"), Duration::from_secs(60))?;
        let (ears, outbox, _) = organ_told(settings(&dir, "sleep 30"), by_hand);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        backdate(&ears, Duration::from_secs(60))?;
        let pid = pid_of(&ears.state())?;
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        ears.check_once(generation);
        ears.check_once(generation);
        let state = ears.state();
        assert!(
            matches!(&state, EarsState::Deaf { reason, .. } if reason.starts_with("me: no chunk rotated for 60 s; them: no chunk rotated for 60 s")),
            "{state:?}"
        );
        assert!(
            ears.line(false)
                .starts_with("ears: DEAF — me: no chunk rotated")
        );
        assert_eq!(at(&ask(&ears, "ears.status")?, "/result/state"), "deaf");
        let told = events(&outbox);
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told.first().map(|e| at(e, "/state")), Some(json!("deaf")));
        assert!(group_alive(pid), "deaf ears are not stopped");
        for side in ["me", "them"] {
            fs::write(dir.join(side).join("c2.wav"), b"")?;
        }
        ears.check_once(generation);
        assert!(
            matches!(ears.state(), EarsState::Running { .. }),
            "{:?}",
            ears.state()
        );
        assert_eq!(
            events(&outbox),
            vec![json!({"event": "ears", "state": "running", "reason": "hearing again"})]
        );
        assert_eq!(
            at(&ask(&ears, "ears.stop")?, "/result"),
            "stopped",
            "checked ears still stop"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn a_capture_that_exits_reports_exited() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("exit")?;
        let (ears, outbox, rx) = organ_told(settings(&dir, "echo hi; exit 3"), |ears| ears);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        told(&rx, |line| {
            line.starts_with("ears: capture exited on its own")
        })?;
        assert_eq!(ears.state(), EarsState::Exited { code: Some(3) });
        assert_eq!(
            not_mode(outbox.drain()),
            vec![b"{\"code\":3,\"event\":\"ears\",\"state\":\"exited\"}\n".to_vec()]
        );
        // The exit is told at the reap; its stdout reaches the log when the reader has read it to the end.
        let read = Arc::clone(&ears.inner.lock().map_err(|e| e.to_string())?.read);
        assert!(
            read.wait_until(Instant::now().checked_add(Duration::from_secs(30))),
            "the capture's stdout was never read to its end"
        );
        assert_eq!(
            fs::read_to_string(dir.join("ears.log"))?,
            "hi\n",
            "stdout goes to the ears log"
        );
        assert_eq!(
            at(&ask(&ears, "ears.start")?, "/ok"),
            true,
            "an exited capture restarts"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn route_sends_ears_and_voice_verbs_to_the_ears_and_the_rest_to_the_hands()
    -> Result<(), Box<dyn std::error::Error>> {
        let (ears, _, _) = organ(Err("ears: off in config".to_owned()));
        let hands: LineHandler = Arc::new(|_line: &[u8]| b"hands\n".to_vec());
        let handler = route(Some(hands), Arc::clone(&ears));
        assert_eq!(
            handler(b"{\"id\":\"1\",\"verb\":\"read\",\"arg\":\"x\"}"),
            b"hands\n"
        );
        assert_eq!(handler(b"garbage"), b"hands\n");
        let reply: Value = serde_json::from_slice(&handler(
            b"{\"id\":\"2\",\"verb\":\"voice.say\",\"arg\":\"hi\"}",
        ))?;
        assert_eq!(at(&reply, "/err"), "ears: off in config");

        let bare = route(None, ears);
        assert_eq!(
            bare(b"{\"id\":\"3\",\"verb\":\"ping\"}"),
            b"{\"id\":\"3\",\"err\":\"verb\"}\n"
        );
        Ok(())
    }

    /// An organ that keeps its mode in `file`, with its log and pause flag.
    fn kept_organ(
        settings: Result<EarsSettings, String>,
        file: &Path,
    ) -> (Arc<Ears>, Arc<Notes>, Arc<Stop>) {
        let notes = Arc::new(Notes::default());
        let journal = Arc::clone(&notes);
        let journal: Arc<dyn Journal> = journal;
        let paused = Arc::new(Stop::new());
        let ears = Ears::new(
            settings,
            Arc::clone(&paused),
            Arc::new(Outbox::default()),
            journal,
            Box::new(|| {}),
        )
        .with_mode_file(file.to_path_buf());
        (Arc::new(ears), notes, paused)
    }

    fn noted(notes: &Notes, needle: &str) -> bool {
        notes
            .0
            .lock()
            .is_ok_and(|lines| lines.iter().any(|line| line.contains(needle)))
    }

    /// The chosen mode survives a restart: a switch (the verb or the menu's `set_mode`) keeps it, a
    /// refused one and Quit leave it, the next start comes back in it; Pause and `ears.stop` keep off,
    /// and off restores off.
    #[test]
    fn the_chosen_mode_is_kept_and_comes_back_after_a_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("kept")?;
        let file = mode_file_beside(&dir.join("config.toml"));
        assert_eq!(file, dir.join("ears-mode"));
        let mut s = settings(&dir, "sleep 30 & sleep 30");
        s.modes.push((
            EarsMode::Alone,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sleep 32 & sleep 32".to_owned(),
            ],
        ));

        // First run: no file yet, so off; the agent's verb switches to alone; Quit keeps it.
        let (ears, _, _) = kept_organ(Ok(s.clone()), &file);
        ears.restore_mode();
        assert_eq!(ears.mode(), EarsMode::Off, "no file: starts off");
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        assert_eq!(fs::read_to_string(&file)?, "alone\n");
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "stream")?, "/ok"), false);
        assert_eq!(fs::read_to_string(&file)?, "alone\n", "a refused switch");
        ears.shutdown();
        assert_eq!(fs::read_to_string(&file)?, "alone\n", "Quit keeps the mode");

        // Second run: back in alone, said in the log; the menu's switch keeps call.
        let (ears, notes, _) = kept_organ(Ok(s.clone()), &file);
        ears.restore_mode();
        assert_eq!(ears.mode(), EarsMode::Alone);
        let alone_pid = pid_of(&ears.state())?;
        assert!(noted(&notes, "ears: mode alone restored after restart"));
        ears.set_mode(EarsMode::Call, ModeSource::Menu)?;
        assert!(group_gone_within(alone_pid, Duration::from_secs(1)));
        assert_eq!(fs::read_to_string(&file)?, "call\n");
        // Pause keeps off.
        ears.pause();
        assert_eq!(fs::read_to_string(&file)?, "off\n");
        ears.shutdown();

        // Third run: off restores off.
        let (ears, notes, _) = kept_organ(Ok(s), &file);
        ears.restore_mode();
        assert_eq!(ears.mode(), EarsMode::Off);
        assert_eq!(ears.state().pid(), None);
        assert!(!noted(&notes, "restored"));
        // ears.stop keeps off too.
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "alone")?, "/ok"), true);
        assert_eq!(fs::read_to_string(&file)?, "alone\n");
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert_eq!(fs::read_to_string(&file)?, "off\n");
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A missing, unknown or unconfigured saved mode starts the ears as before (off); so does a paused
    /// start; with the ears off in config nothing is restored or written.
    #[test]
    fn a_missing_unknown_or_unconfigured_saved_mode_starts_as_before()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("not-kept")?;
        let file = dir.join("ears-mode");
        let s = settings(&dir, "sleep 30 & sleep 30");
        for content in [
            None,
            Some(""),
            Some("loud\n"),
            Some("alone\ncall\n"),
            Some("off\n"),
        ] {
            if let Some(content) = content {
                fs::write(&file, content)?;
            }
            let (ears, notes, _) = kept_organ(Ok(s.clone()), &file);
            ears.restore_mode();
            assert_eq!(ears.mode(), EarsMode::Off, "{content:?}");
            assert_eq!(ears.state().pid(), None, "{content:?}");
            assert!(!noted(&notes, "restored"), "{content:?}");
            // A missing, empty or unknown file is said at start; a saved off is not news.
            assert_eq!(
                noted(&notes, "ears: no saved mode at"),
                content != Some("off\n"),
                "{content:?}"
            );
        }

        // Saved, but the config no longer has it: not started, said, the file kept.
        fs::write(&file, "stream\n")?;
        let (ears, notes, _) = kept_organ(Ok(s.clone()), &file);
        ears.restore_mode();
        assert_eq!(ears.mode(), EarsMode::Off);
        assert!(noted(
            &notes,
            "ears: saved mode stream not restored: not configured"
        ));
        assert_eq!(fs::read_to_string(&file)?, "stream\n");

        // Paused at start: refused, said, nothing written.
        fs::write(&file, "call\n")?;
        let (ears, notes, paused) = kept_organ(Ok(s), &file);
        paused.set();
        ears.restore_mode();
        assert_eq!(ears.mode(), EarsMode::Off);
        assert!(noted(&notes, "ears: mode call not restored (paused)"));
        assert_eq!(fs::read_to_string(&file)?, "call\n");

        // The ears off in config: nothing restored, and Pause writes nothing.
        let (ears, notes, _) = kept_organ(Err("ears: off in config".to_owned()), &file);
        ears.restore_mode();
        ears.pause();
        assert_eq!(ears.mode(), EarsMode::Off);
        assert!(!noted(&notes, "restored"));
        assert_eq!(fs::read_to_string(&file)?, "call\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Every mode event says who changed the mode, through the real entry points: the restore at
    /// start, the menu's `set_mode`, Pause, a hub request (its own id), and a capture that ends on its own.
    #[test]
    fn every_mode_event_says_its_source() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("source")?;
        let file = dir.join("ears-mode");
        let mut s = settings(&dir, "sleep 30 & sleep 30");
        s.modes.push((
            EarsMode::Office,
            vec!["/bin/sh".to_owned(), "-c".to_owned(), "sleep 31".to_owned()],
        ));
        s.modes.push((
            EarsMode::Stream,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sleep 0.3".to_owned(),
            ],
        ));
        let sources = |ears: &Ears| -> Vec<(Value, Value)> {
            ears.outbox
                .drain()
                .iter()
                .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
                .filter(|v| v["state"] == "mode")
                .map(|v| (at(&v, "/mode"), at(&v, "/source")))
                .collect()
        };
        let pair = |mode: &str, source: &str| (json!(mode), json!(source));
        fs::write(&file, "call\n")?;
        let (ears, _, paused) = kept_organ(Ok(s), &file);
        ears.restore_mode();
        ears.set_mode(EarsMode::Office, ModeSource::Menu)?;
        paused.set();
        ears.pause();
        paused.clear();
        assert_eq!(
            sources(&ears),
            vec![
                pair("call", "restore"),
                pair("office", "menu"),
                pair("off", "pause")
            ]
        );
        for (id, verb) in [("r7", "ears.start"), ("r8", "ears.stop")] {
            let line = json!({"id": id, "verb": verb}).to_string();
            let reply: Value = serde_json::from_slice(&ears.handle(line.as_bytes()))?;
            assert_eq!(at(&reply, "/ok"), true, "{verb}");
        }
        let line = json!({"id": "r9", "verb": "ears.mode", "arg": "stream"}).to_string();
        let reply: Value = serde_json::from_slice(&ears.handle(line.as_bytes()))?;
        assert_eq!(at(&reply, "/ok"), true);
        let started = Instant::now();
        let mut got = sources(&ears);
        while got.len() < 4 && started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
            got.extend(sources(&ears));
        }
        assert_eq!(
            got,
            vec![
                pair("call", "request:r7"),
                pair("off", "request:r8"),
                pair("stream", "request:r9"),
                pair("off", "exited")
            ]
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The text of every line event in the outbox, drained.
    fn line_texts(outbox: &Outbox) -> Vec<String> {
        outbox
            .drain()
            .iter()
            .filter(|l| String::from_utf8_lossy(l).contains("\"state\":\"line\""))
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .filter_map(|v| v.get("text").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// Whether a line reading `text` goes out within the guard (a guard against a wedge, not a timing claim). A
    /// poll, in the test only: the outbox gives the test no event.
    fn line_goes_out(outbox: &Outbox, text: &str) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            if line_texts(outbox).iter().any(|t| t == text) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The capture threads (readers, waiters, self-checks, stops) alive now, by name, sorted: each is named in
    /// `capture_threads` for its life ([`Ears::capture_thread`]).
    fn capture_threads(ears: &Ears) -> Vec<&'static str> {
        let mut threads = ears
            .capture_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        threads.sort_unstable();
        threads
    }

    /// Whether the capture threads come to exactly `left` (by name), within the guard (against a wedge, not a
    /// timing claim). A poll, in the test only: a thread's end gives the test no event.
    fn capture_threads_come_to(ears: &Ears, left: &[&str]) -> bool {
        let started = Instant::now();
        while capture_threads(ears) != left && started.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(20));
        }
        capture_threads(ears) == left
    }

    /// Whether every thread the ears started for a capture (its reader, its waiter, its self-check, a stop's) has
    /// ended ([`capture_threads_come_to`] none). A reader ends at the end of its stdout or at its first line after
    /// a cut: once this is true, it has read all it ever will.
    fn capture_threads_end(ears: &Ears) -> bool {
        capture_threads_come_to(ears, &[])
    }

    /// A capture that says `first`, then waits; told to stop (TERM) it says `held` and exits.
    const HOLDS_A_LINE: &str = "trap 'printf \"TULPA-EARS-LINE 1000 2500 12:00:10 [me] held\\n\"; exit 0' TERM; \
         printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60 & wait $!";

    /// An executable `tulpa-hear` in `dir`, as `script` (a fake of the `--ears` capture), and settings that run it
    /// as `tulpa-hear --ears`.
    fn fake_hear(dir: &Path, script: &str) -> std::io::Result<EarsSettings> {
        let program = dir.join("tulpa-hear");
        crate::testutil::write_script(&program, &format!("#!/bin/sh\n{script}\n"))?;
        let mut s = settings(dir, "exit 0");
        s.modes = vec![(
            EarsMode::Call,
            vec![program.to_string_lossy().into_owned(), "--ears".to_owned()],
        )];
        Ok(s)
    }

    /// Fable N2: a stop is no longer a kill. The capture is told to stop (TERM to its group), writes what it holds,
    /// and that last line still goes to the link, though the capture is no longer the current one.
    #[test]
    fn a_stop_lets_the_capture_write_what_it_holds() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("stop-held")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, HOLDS_A_LINE)));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "held"),
            "the stopped capture's last line never went out"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Fable F6, the tray's half: the `--ears` capture gets a pipe on its stdin and is told so; closing it is the
    /// stop. This fake ignores TERM, so only the pipe's end can have stopped it.
    #[test]
    fn closing_its_stdin_stops_the_hear_capture() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("stop-eof")?;
        let s = fake_hear(
            &dir,
            "trap '' TERM\n\
             [ -n \"$TULPA_EARS_STDIN_STOP\" ] || exec sleep 60\n\
             printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'\n\
             cat >/dev/null\n\
             printf 'TULPA-EARS-LINE 1000 2500 12:00:10 [me] eof\\n'",
        )?;
        let (ears, outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "first"),
            "the capture was not told that its stdin is the tray's pipe"
        );
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "eof"),
            "the capture never saw its stdin end"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The tray of [`a_dead_tray_ends_the_hear_capture_though_a_sibling_lives`], in a process of its own: it starts
    /// the fake capture, then a sibling the way the tray starts its children, says `ready` and waits to be killed.
    fn tray_to_be_killed(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let s = fake_hear(
            dir,
            "trap '' TERM\n\
             [ -n \"$TULPA_EARS_STDIN_STOP\" ] || exec sleep 60\n\
             cat >/dev/null\n\
             echo ended > \"$HOME/ended.fifo\"",
        )?;
        let (ears, _outbox, _) = organ(Ok(s));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        // A sibling of the capture, started after it: whatever of the tray's it inherits, it keeps while it lives.
        let _sibling = exec::cleared(Path::new("/bin/sleep"), dir)
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        std::io::stdout().write_all(b"ready\n")?;
        std::io::stdout().flush()?;
        loop {
            thread::park();
        }
    }

    /// Fable F6 with akira's condition: the tray is killed (no stop, no signal to the capture) while a sibling it
    /// started lives on. The capture's stdin still ends, because the pipe's write end was the tray's alone, and
    /// the capture ends.
    #[test]
    fn a_dead_tray_ends_the_hear_capture_though_a_sibling_lives()
    -> Result<(), Box<dyn std::error::Error>> {
        const TRAY: &str = "TULPA_TRAY_TEST_TRAY_TO_BE_KILLED";
        if let Some(dir) = std::env::var_os(TRAY) {
            return tray_to_be_killed(Path::new(&dir));
        }
        let dir = temp_dir("dead-tray")?;
        let fifo = dir.join("ended.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success()
        );
        let mut tray = std::process::Command::new(std::env::current_exe()?)
            .args([
                "ears::tests::a_dead_tray_ends_the_hear_capture_though_a_sibling_lives",
                "--exact",
                "--nocapture",
            ])
            .env(TRAY, &dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let out = tray.stdout.take().ok_or("no stdout")?;
        let ready = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .any(|l| l.contains("ready"));
        // The tray dies as a crash kills it: nothing of it runs again.
        tray.kill()?;
        tray.wait()?;
        assert!(ready, "the tray never started its capture");
        // The capture says so on a fifo once its stdin ended: the open and the read wait for it (an event), on a
        // thread, so that a capture that never ends runs into the guard here.
        let (told, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _told = told.send(fs::read_to_string(&fifo));
        });
        let said = ended.recv_timeout(Duration::from_secs(20));
        assert!(
            matches!(&said, Ok(Ok(text)) if text.trim() == "ended"),
            "the capture outlived its tray: {said:?}"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Pause is not a stop: it is an immediate SIGKILL, on purpose. Pause means the microphone closes NOW; a
    /// capture left to finish its phrase would keep listening for seconds more. What it held is lost, and that is
    /// the right cost. (Do not make Pause flush.)
    #[test]
    fn pause_kills_the_capture_at_once_and_takes_no_last_line()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("pause-kill")?;
        let (ears, outbox, _) = organ(Ok(settings(&dir, HOLDS_A_LINE)));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        let pid = ears.state().pid().ok_or("no capture")?;
        ears.pause();
        // A guard against a wedge: a killed capture is gone at once, and the waiter reaps it.
        let started = Instant::now();
        while pid_alive(pid) && started.elapsed() < Duration::from_secs(20) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "the paused capture lives on");
        // Its reader has read the stdout to its end, so whatever the capture wrote was read by then.
        assert!(
            capture_threads_end(&ears),
            "the capture's reader never ended"
        );
        // Its TERM trap never ran: neither the link nor the side log (the tee of its stdout) has the line.
        assert!(!line_texts(&outbox).iter().any(|t| t == "held"));
        assert!(!fs::read_to_string(dir.join("ears.log"))?.contains("] held"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// akira F5: every stopped capture's held lines are written, however many stops overlap. The first capture is
    /// stopped and takes a second to write what it holds; meanwhile a second capture is started and stopped too.
    /// Both last lines go out: a later stop does not make the tray deaf to an earlier capture still closing.
    #[test]
    fn two_stops_within_the_grace_both_write_what_they_hold()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("two-stops")?;
        // Each run numbers itself (a count in its HOME), says `first<n>`, and on TERM says `held<n>` a second later.
        let script = "n=$(cat \"$HOME/n\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$HOME/n\"; \
             trap 'sleep 1; printf \"TULPA-EARS-LINE 1000 2500 12:00:10 [me] held$n\\n\"; exit 0' TERM; \
             printf \"TULPA-EARS-LINE 500 900 12:00:09 [me] first$n\\n\"; sleep 60 & wait $!";
        let (ears, outbox, _) = organ(Ok(settings(&dir, script)));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "first1"),
            "the first capture never ran"
        );
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "first2"),
            "the second capture never ran"
        );
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        // Both captures are closing now. A guard against a wedge, not a timing claim; a poll, in the test only.
        let mut held = Vec::new();
        let started = Instant::now();
        while held.len() < 2 && started.elapsed() < Duration::from_secs(20) {
            held.extend(
                line_texts(&outbox)
                    .into_iter()
                    .filter(|t| t.starts_with("held")),
            );
            thread::sleep(Duration::from_millis(50));
        }
        held.sort();
        assert_eq!(
            held,
            ["held1", "held2"],
            "every stopped capture's last line"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A journal that holds its caller at the "echo" note until released: the reader thread writes that note for a
    /// microphone line it drops as our own playback, so this holds the reader back at a line of the test's choosing.
    struct HoldsTheReader {
        entered: Mutex<std::sync::mpsc::Sender<()>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Journal for HoldsTheReader {
        fn record(&self, line: &str) {
            if line.contains("echo") {
                let _told = self.entered.lock().map(|tx| tx.send(()));
                let _released = self.release.lock().map(|rx| rx.recv());
            }
        }
    }

    /// The gate's S1: no line a capture wrote is dropped because its reader was slow. The reader is held back at an
    /// earlier line while the capture is stopped, writes its last line, exits and is reaped; only then is the
    /// reader let go. The last line still goes out as that capture's.
    #[test]
    fn a_line_read_after_the_reap_still_goes_out() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("slow-reader")?;
        // An echo of our own playback first (the reader stops there), then, on TERM, the other side's last line.
        let script = "trap 'printf \"TULPA-EARS-LINE 1000 2500 12:00:10 [them] held\\n\"; exit 0' TERM; \
             printf 'TULPA-EARS-LINE 5000 7000 12:00:12 [me] echo of us\\n'; sleep 60 & wait $!";
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        let outbox = Arc::new(Outbox::default());
        let ears = Arc::new(Ears::new(
            Ok(settings(&dir, script)),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::new(HoldsTheReader {
                entered: Mutex::new(entered_tx),
                release: Mutex::new(release),
            }),
            Box::new(|| {}),
        ));
        ears.speaking.mark(6_000, 0, now_ms());
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let pid = ears.state().pid().ok_or("no capture")?;
        // Guards against a wedge, not timing claims.
        assert!(
            entered.recv_timeout(Duration::from_secs(20)).is_ok(),
            "the reader never reached the echo"
        );
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        let started = Instant::now();
        while pid_alive(pid) && started.elapsed() < Duration::from_secs(20) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "the stopped capture never exited");
        // The one settle here: the capture is reaped, and whatever follows its reap has had its time. The reader
        // is still held, with the capture's last line unread in the pipe.
        thread::sleep(Duration::from_millis(500));
        assert!(release_tx.send(()).is_ok());
        assert!(
            line_goes_out(&outbox, "held"),
            "the line read after the reap was dropped"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The gate's S3: Quit reaches a capture that is still closing. One that ignores its stop is still alive
    /// inside the grace when the tray quits; nothing of the tray is left afterwards to kill it, so Quit does.
    #[test]
    fn quit_kills_a_capture_that_is_still_closing() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("quit-closing")?;
        let script =
            "trap '' TERM; printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60";
        let (mut ears, outbox, _) = organ(Ok(settings(&dir, script)));
        Arc::get_mut(&mut ears).ok_or("shared")?.stop_grace = Duration::from_secs(3);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        let pid = ears.state().pid().ok_or("no capture")?;
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert!(group_alive(pid), "it ignores the stop: still closing");
        ears.shutdown();
        assert!(!pid_alive(pid), "the tray quit and left a capture alive");
        // What it left in its group was killed before its reap; the kernel may take a moment to clear it away.
        let quit = Instant::now();
        while group_alive(pid) && quit.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !group_alive(pid),
            "the capture's group outlived the tray's quit"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The gate's S3: Pause reaches a capture that is still closing too. Pause means the microphone closes now,
    /// whichever capture holds it.
    #[test]
    fn pause_kills_a_capture_that_is_still_closing() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("pause-closing")?;
        let script =
            "trap '' TERM; printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60";
        let (mut ears, outbox, _) = organ(Ok(settings(&dir, script)));
        Arc::get_mut(&mut ears).ok_or("shared")?.stop_grace = Duration::from_secs(5);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        let pid = ears.state().pid().ok_or("no capture")?;
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        let stopped = Instant::now();
        ears.pause();
        // Gone at once, long before the stop's own 5 s grace would kill it.
        while group_alive(pid) && stopped.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!group_alive(pid), "paused, and a closing capture lives on");
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// How many captures are still closing.
    fn closing_now(ears: &Ears) -> usize {
        ears.inner.lock().map_or(usize::MAX, |i| i.closing.len())
    }

    /// Set for the test binary run again as a capture ([`fake_capture_that_leaves_a_holder`]): what its helper runs.
    const HOLDER: &str = "TULPA_TRAY_TEST_HOLDER";

    /// Set beside [`HOLDER`]: what the fake capture itself runs once its helper is started.
    const HOLDER_CAPTURE: &str = "TULPA_TRAY_TEST_HOLDER_CAPTURE";

    /// A fake capture, when the tray under test starts this test binary as one: it starts a helper OUTSIDE its own
    /// process group that inherits its stdout (std's `process_group(0)`: no `setsid(1)`, which macOS lacks), then
    /// becomes (`exec`, same pid and group) a shell running [`HOLDER_CAPTURE`]'s script, which can answer a TERM
    /// as the real capture does. The helper outlives it and holds the pipe. Run as a plain test (no [`HOLDER`]) it
    /// does nothing.
    #[test]
    fn fake_capture_that_leaves_a_holder() -> Result<(), Box<dyn std::error::Error>> {
        let (Some(script), Some(capture)) =
            (std::env::var_os(HOLDER), std::env::var_os(HOLDER_CAPTURE))
        else {
            return Ok(());
        };
        let mut helper = std::process::Command::new("/bin/sh");
        helper
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut helper, 0);
        let _holder = helper.spawn()?;
        let mut itself = std::process::Command::new("/bin/sh");
        itself.arg("-c").arg(capture);
        Err(std::os::unix::process::CommandExt::exec(&mut itself).into())
    }

    /// A fifo `name` in `dir`: a script that reads it waits until the test lets it go ([`let_go`]).
    fn fifo(dir: &Path, name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let fifo = dir.join(name);
        let made = std::process::Command::new("mkfifo").arg(&fifo).status()?;
        if !made.success() {
            return Err(format!("mkfifo {name}: {made}").into());
        }
        Ok(fifo)
    }

    /// Lets go the script that waits on `fifo` ([`fifo`]): whether it took the word within the guard. The open
    /// waits for the script's read end, on a thread, so a script that is gone runs into the guard here instead of
    /// hanging the test.
    fn let_go(fifo: &Path) -> bool {
        let fifo = fifo.to_path_buf();
        let (told, wrote) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _told = told.send(fs::write(&fifo, "go\n"));
        });
        matches!(wrote.recv_timeout(Duration::from_secs(30)), Ok(Ok(())))
    }

    /// Ears, their outbox and their journal's notes.
    type WithNotes = (Arc<Ears>, Arc<Outbox>, Arc<Notes>);

    /// The ears of a tray whose capture is [`fake_capture_that_leaves_a_holder`] running `capture`, with `helper`
    /// as its helper's script, a stdout cut `read_grace` after the capture's exit, and the journal's notes.
    fn ears_with_a_holder(
        dir: &Path,
        capture: &str,
        helper: &str,
        read_grace: Duration,
    ) -> Result<WithNotes, Box<dyn std::error::Error>> {
        let mut s = settings(dir, "exit 0");
        s.modes = vec![(
            EarsMode::Call,
            vec![
                std::env::current_exe()?.to_string_lossy().into_owned(),
                "ears::tests::fake_capture_that_leaves_a_holder".to_owned(),
                "--exact".to_owned(),
                "--nocapture".to_owned(),
            ],
        )];
        s.env.push((HOLDER.to_owned(), helper.to_owned()));
        s.env.push((HOLDER_CAPTURE.to_owned(), capture.to_owned()));
        let outbox = Arc::new(Outbox::default());
        let notes = Arc::new(Notes::default());
        let mut ears = Ears::new(
            Ok(s),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::clone(&notes) as Arc<dyn Journal>,
            Box::new(|| {}),
        );
        ears.read_grace = read_grace;
        Ok((Arc::new(ears), outbox, notes))
    }

    /// Whether the journal has said a stdout was cut.
    fn cut_said(notes: &Notes) -> bool {
        notes
            .0
            .lock()
            .is_ok_and(|n| n.iter().any(|l| l.contains("stdout was still held open")))
    }

    /// Round 7, B1: a wait for a capture's stdout to end has an end itself. The capture leaves a helper outside its
    /// group holding its stdout, so the end of the stream never comes after the capture is gone. Within the bound
    /// its closing entry is finished and the cut is said. And the gate's F4: the capture answers its stop with a
    /// last line, which is still taken. The capture exits only once
    /// that line is out, and the helper holds the pipe until the test lets it go: no timer races the bound.
    #[test]
    fn a_stdout_held_open_past_the_exit_is_cut_after_a_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("held-stdout")?;
        let (exit, hold) = (fifo(&dir, "exit.fifo")?, fifo(&dir, "hold.fifo")?);
        let capture = format!(
            "trap 'printf \"TULPA-EARS-LINE 1000 2500 12:00:10 [me] held\\n\"; read -r _ < \"{}\"; exit 0' TERM; \
             printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60 & wait $!",
            exit.display()
        );
        let helper = format!("read -r _ < \"{}\"", hold.display());
        let (ears, outbox, notes) =
            ears_with_a_holder(&dir, &capture, &helper, Duration::from_millis(300))?;
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "held"),
            "a stopped capture's last line was not taken"
        );
        assert!(let_go(&exit), "the capture never took its go");
        // A guard against a wedge, far past the bound (0.3 s after the reap); a poll, in the test only: the entry's
        // end gives the test no event.
        let stopped = Instant::now();
        while closing_now(&ears) > 0 && stopped.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            closing_now(&ears),
            0,
            "the closing entry waits on a pipe that never ends"
        );
        assert!(cut_said(&notes), "the cut is said");
        assert!(let_go(&hold), "the helper never took its go");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The gate's N2: "its stdout ended" is known for every capture, a paused one too. A Pause kills the capture,
    /// its stdout ends at once, and nothing is cut: the waiter's wait for the end of the stream ends on the
    /// reader's word, long before its bound, and the journal says no "still held open". The bound is far past the
    /// guard, so no load can make it come first: a waiter that did wait for it shows as a thread that never ends.
    #[test]
    fn a_pause_writes_no_cut_line() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("pause-no-cut")?;
        let outbox = Arc::new(Outbox::default());
        let notes = Arc::new(Notes::default());
        let mut ears = Ears::new(
            Ok(settings(&dir, HOLDS_A_LINE)),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::clone(&notes) as Arc<dyn Journal>,
            Box::new(|| {}),
        );
        ears.read_grace = Duration::from_secs(120);
        let ears = Arc::new(ears);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        ears.pause();
        assert!(
            capture_threads_end(&ears),
            "the waiter waited for its bound: the reader's end of stream never reached it"
        );
        assert!(!cut_said(&notes), "a Pause was logged as a cut stdout");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The gate's N2, the other side: a real cut reaches a paused capture's reader. The capture is killed by Pause
    /// while its helper holds the pipe. Once the cut is said, the helper is let write one more line (on a fifo the
    /// test opens then: an event, not a timer), and exits. The reader has stopped at the cut: that late line is
    /// not read, so it is not in the side log.
    #[test]
    fn a_paused_capture_whose_pipe_is_held_is_cut_and_its_reader_stops()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("pause-cut")?;
        let fifo = fifo(&dir, "late.fifo")?;
        let helper = format!(
            "read -r _ < '{}'; echo 'TULPA-EARS-LINE 3000 3500 12:00:13 [me] late'",
            fifo.display()
        );
        let (ears, outbox, notes) =
            ears_with_a_holder(&dir, HOLDS_A_LINE, &helper, Duration::from_millis(300))?;
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        ears.pause();
        // A guard against a wedge; a poll, in the test only: the journal gives the test no event.
        let paused = Instant::now();
        while !cut_said(&notes) && paused.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            cut_said(&notes),
            "the held pipe of a paused capture was not cut"
        );
        assert!(let_go(&fifo), "the helper never took its go");
        // The reader has read all it ever will once it ends: at the late line, or at the pipe's end after it.
        assert!(
            capture_threads_end(&ears),
            "the capture's reader never ended"
        );
        assert!(
            !fs::read_to_string(dir.join("ears.log"))?.contains("] late"),
            "the reader went on reading a cut stream"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0530 (akira's B1 on round 7): a cut stream's reader is parked while something outside the capture's group
    /// holds the pipe, one per cut capture, and each ends when its holder dies. Two captures are paused while
    /// their helpers hold their pipes: after the cuts, the two readers are the only capture threads left; when
    /// the helpers end, so do they. A guard, not red on the base: the base ends them the same
    /// way. What it does not do, and cannot without a new design, is end a reader while a silent holder lives.
    #[test]
    fn cut_readers_end_when_their_holders_die() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("cut-readers")?;
        let hold = fifo(&dir, "hold.fifo")?;
        let helper = format!("read -r _ < '{}'", hold.display());
        let (ears, outbox, _) =
            ears_with_a_holder(&dir, HOLDS_A_LINE, &helper, Duration::from_millis(300))?;
        for _ in 0..2 {
            assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
            assert!(line_goes_out(&outbox, "first"), "a capture never ran");
            ears.pause();
        }
        // Each waiter ends at its cut, the self-checks with their captures: the parked readers are left.
        assert!(
            capture_threads_come_to(&ears, &["tulpa-ears-read", "tulpa-ears-read"]),
            "not one parked reader per held pipe, and nothing else: {:?}",
            capture_threads(&ears)
        );
        // One word on the fifo ends both helpers: one reads it, the other the writer's close.
        assert!(let_go(&hold), "the helpers never took their go");
        assert!(
            capture_threads_end(&ears),
            "a reader outlived its pipe's holder"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// The gate's N4: nothing the tray started stays out of reach. A capture that was given up (not gone after its
    /// kill's bound) and is still alive is killed by Quit, and by Pause, without being waited for.
    #[test]
    fn a_given_up_capture_still_alive_is_killed_by_quit_and_by_pause()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("given-up-alive")?;
        for quit in [true, false] {
            let (ears, _, _) = organ(Ok(settings(&dir, "exit 0")));
            let mut alive = exec::cleared(Path::new("/bin/sh"), &dir)
                .args(["-c", "sleep 30"])
                .spawn()?;
            let group = exec::Group::of(&alive);
            let read = Arc::new(Stop::new());
            read.set();
            ears.inner
                .lock()
                .map_err(|e| e.to_string())?
                .given_up
                .push(Closing {
                    generation: 999,
                    mode: EarsMode::Call,
                    since: SystemTime::now(),
                    exited: Arc::new(Stop::new()),
                    read,
                    pid: alive.id(),
                    group: Arc::clone(&group),
                });
            if quit {
                ears.shutdown();
            } else {
                ears.pause();
            }
            // Killed: its reap comes at once (a guard far under its own 30 s), and by a signal.
            let (reaped_tx, reaped) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _told = reaped_tx.send(group.reap(&mut alive).map(|s| s.code()).ok());
            });
            assert_eq!(
                reaped.recv_timeout(Duration::from_secs(5)),
                Ok(Some(None)),
                "quit {quit}: a given-up capture was left alive"
            );
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Round 7, B2: Quit has ONE grace for everything it stops, not one per capture. Two captures that ignore
    /// their stop are still closing; Quit returns within one grace (and a margin), and both are dead. A guard, not
    /// red on the base of round 7: each stop's own thread already bounded the wait there.
    #[test]
    fn quit_gives_all_closing_captures_one_grace() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("quit-one-grace")?;
        let script =
            "trap '' TERM; printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60";
        let (mut ears, outbox, _) = organ(Ok(settings(&dir, script)));
        let grace = Duration::from_secs(3);
        Arc::get_mut(&mut ears).ok_or("shared")?.stop_grace = grace;
        let mut pids = Vec::new();
        for _ in 0..2 {
            assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
            assert!(line_goes_out(&outbox, "first"), "a capture never ran");
            pids.push(ears.state().pid().ok_or("no capture")?);
            assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        }
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        assert!(
            line_goes_out(&outbox, "first"),
            "the last capture never ran"
        );
        pids.push(ears.state().pid().ok_or("no capture")?);
        let quit = Instant::now();
        ears.shutdown();
        let took = quit.elapsed();
        for pid in pids {
            assert!(
                !pid_alive(pid),
                "the tray quit and left capture {pid} alive"
            );
        }
        assert!(
            took < grace + Duration::from_millis(1500),
            "quit took {took:?} for three captures: more than one grace"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Round 7, B3: a capture whose exit never comes (a kill that did not end it, or an exit that cannot be
    /// learned) is not waited for without end, and not hung on: past the bounds the tray says so and gives its
    /// entry up. Here the entry's exit event is never set (nobody reaps its child). And the gate's F1 (N4): the
    /// given-up entry stays within reach. It is on the given-up list, and a process still alive in its group (one
    /// that joined the group after the kill, standing in for a capture the kill did not end) is killed by the next
    /// Quit, and by Pause.
    #[test]
    fn a_closing_capture_whose_exit_never_comes_is_given_up()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("never-exits")?;
        for quit in [true, false] {
            let notes = Arc::new(Notes::default());
            let mut ears = Ears::new(
                Ok(settings(&dir, "exit 0")),
                Arc::new(Stop::new()),
                Arc::new(Outbox::default()),
                Arc::clone(&notes) as Arc<dyn Journal>,
                Box::new(|| {}),
            );
            ears.stop_grace = Duration::from_millis(200);
            let ears = Arc::new(ears);
            // Unreaped by the tray (no waiter): its exit is never told, so its group stays a live target.
            let mut leader = exec::cleared(Path::new("/bin/sh"), &dir)
                .args(["-c", "sleep 30"])
                .spawn()?;
            let pid = leader.id();
            let read = Arc::new(Stop::new());
            read.set();
            ears.inner
                .lock()
                .map_err(|e| e.to_string())?
                .closing
                .push(Closing {
                    generation: 999,
                    mode: EarsMode::Call,
                    since: SystemTime::now(),
                    exited: Arc::new(Stop::new()),
                    read,
                    pid,
                    group: exec::Group::of(&leader),
                });
            let quit_at = Instant::now();
            ears.shutdown();
            assert!(
                quit_at.elapsed() < Duration::from_secs(5),
                "quit hung on a capture whose exit never came"
            );
            assert_eq!(closing_now(&ears), 0, "the entry was given up");
            let given_up: Vec<u32> = ears
                .inner
                .lock()
                .map(|i| i.given_up.iter().map(|c| c.pid).collect())
                .unwrap_or_default();
            assert_eq!(given_up, [pid], "the given-up capture is out of reach");
            let slowed = ears
                .inner
                .lock()
                .is_ok_and(|i| i.given_up.iter().all(|c| c.group.is_given_up()));
            assert!(slowed, "its group was not told it is given up");
            let said = notes.0.lock().map(|n| n.join("\n")).unwrap_or_default();
            assert!(said.contains("given up"), "it is said: {said}");
            // Alive in the given-up group (its leader is a zombie, so the group id is still its own).
            let mut joined = std::process::Command::new("/bin/sleep");
            joined
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            std::os::unix::process::CommandExt::process_group(&mut joined, i32::try_from(pid)?);
            let mut joined = joined.spawn()?;
            if quit {
                ears.shutdown();
            } else {
                ears.pause();
            }
            // Killed: its reap comes at once (a guard far under its own 30 s), and by a signal.
            let (reaped_tx, reaped) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _told = reaped_tx.send(joined.wait().map(|s| s.code()).ok());
            });
            assert_eq!(
                reaped.recv_timeout(Duration::from_secs(5)),
                Ok(Some(None)),
                "quit {quit}: a process in a given-up capture's group was left alive"
            );
            // The ears first: their drop kills the given-up group, which must still be the leader's then.
            drop(ears);
            leader.wait()?;
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A capture that ignores the stop (here: TERM, and it has no stdin to end) is killed once the grace is over:
    /// a stop always ends the capture.
    #[test]
    fn a_capture_that_ignores_the_stop_is_killed_after_the_grace()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("stop-ignored")?;
        let script =
            "trap '' TERM; printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60";
        let (mut ears, outbox, _) = organ(Ok(settings(&dir, script)));
        Arc::get_mut(&mut ears).ok_or("shared")?.stop_grace = Duration::from_millis(300);
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        let pid = ears.state().pid().ok_or("no capture")?;
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        // A guard against a wedge: the kill comes 0.3 s after the stop.
        let started = Instant::now();
        while pid_alive(pid) && started.elapsed() < Duration::from_secs(20) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "the capture outlived its stop");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A journal that, the first time it is told the ears are DEAF, changes the mode off and on again: a new
    /// capture, started while the self-check that found the old one deaf is still on its way out.
    #[derive(Default)]
    struct RestartsOnDeaf {
        ears: Mutex<Option<std::sync::Weak<Ears>>>,
        done: AtomicBool,
    }

    impl Journal for RestartsOnDeaf {
        fn record(&self, line: &str) {
            if !line.contains("DEAF") || self.done.swap(true, Ordering::SeqCst) {
                return;
            }
            let ears = self.ears.lock().ok().and_then(|e| e.clone());
            if let Some(ears) = ears.and_then(|e| e.upgrade()) {
                let _off = ask_arg(&ears, "ears.mode", "off");
                let _on = ask_arg(&ears, "ears.mode", "call");
            }
        }
    }

    /// #0454 s2 follow-up 3: an ears state event leaves under the lock that decided it, so the link gets the events
    /// in the order the state changed. A self-check finds the capture deaf; before it has finished, the capture is
    /// replaced. The `deaf` is the old capture's and comes before the new one's start, never after it.
    #[test]
    fn a_late_self_check_does_not_speak_after_a_new_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("late-check")?;
        for side in ["me", "them"] {
            fs::create_dir_all(dir.join(side))?;
            fs::write(dir.join(side).join("c1.wav"), b"")?;
            make_stale(&dir.join(side), Duration::from_secs(60))?;
        }
        let outbox = Arc::new(Outbox::default());
        let journal = Arc::new(RestartsOnDeaf::default());
        let ears = Arc::new(Ears::new(
            Ok(settings(&dir, "sleep 30")),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::clone(&journal) as Arc<dyn Journal>,
            Box::new(|| {}),
        ));
        *journal.ears.lock().map_err(|e| e.to_string())? = Some(Arc::downgrade(&ears));
        assert_eq!(at(&ask_arg(&ears, "ears.mode", "call")?, "/ok"), true);
        let _started = outbox.drain();
        backdate(&ears, Duration::from_secs(60))?;
        let generation = ears.inner.lock().map(|i| i.generation).unwrap_or_default();
        ears.check_once(generation);
        ears.check_once(generation);
        assert!(
            journal.done.load(Ordering::SeqCst),
            "the self-check never found it deaf"
        );
        let told: Vec<String> = outbox
            .drain()
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        let deaf = told.iter().position(|l| l.contains("\"state\":\"deaf\""));
        let restarted = told.iter().rposition(|l| l.contains("\"mode\":\"call\""));
        assert!(deaf.is_some() && restarted.is_some(), "{told:?}");
        assert!(
            deaf < restarted,
            "the old capture's deaf came after the new capture's start: {told:?}"
        );
        ears.shutdown();
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0532: the next capture thread named `name` is refused (the test seam, [`capture::REFUSE`]), after `hook`
    /// runs on the starting thread; then the start. Returns the start's reply, and the capture's group as it was
    /// when the hook ran.
    fn start_refused(
        ears: &Arc<Ears>,
        name: &'static str,
        hook: impl FnOnce() + 'static,
    ) -> Result<(Value, Arc<exec::Group>), Box<dyn std::error::Error>> {
        let seen = Arc::new(Mutex::new(None));
        let (looked, kept) = (Arc::clone(ears), Arc::clone(&seen));
        let hook: Box<dyn FnOnce()> = Box::new(move || {
            let group = looked.inner.lock().ok().and_then(|i| i.group.clone());
            if let Ok(mut kept) = kept.lock() {
                *kept = group;
            }
            hook();
        });
        capture::REFUSE.with(|r| r.replace(Some((name, hook))));
        let reply = ask(ears, "ears.start")?;
        let group = seen.lock().map_err(|e| e.to_string())?.take();
        Ok((reply, group.ok_or("the refused thread's hook never ran")?))
    }

    /// #0532: a capture thread the OS refuses (either of the two) ends the capture through its group: the start
    /// fails, the state is off, and the group knows its leader is reaped, so a later kill sends nothing to a group
    /// id that may be another process's by now.
    #[test]
    fn a_refused_capture_thread_leaves_its_group_reaped() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("refused-thread")?;
        for name in ["tulpa-ears-read", "tulpa-ears-wait"] {
            let (ears, _, _) = organ(Ok(settings(&dir, "sleep 30")));
            let (reply, group) = start_refused(&ears, name, || {})?;
            assert_eq!(at(&reply, "/ok"), false, "{name}: {reply}");
            assert_eq!(ears.state(), EarsState::Off, "{name}");
            assert!(
                !group.kill(),
                "{name}: the reaped capture's group was signalled"
            );
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0530 (r6 nit): a stop whose own thread the OS refuses leaks nothing. The capture, which ignores TERM, is
    /// killed at once through its group; its waiter reaps it, and its closing entry goes as any other does: none
    /// is left closing or given up. A guard, not red on the base: the kill through the group came in round 6, and
    /// the entry's end at the reap in #0532.
    #[test]
    fn a_refused_stop_thread_kills_the_capture_and_leaves_no_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let dir = temp_dir("refused-stop")?;
        let script =
            "trap '' TERM; printf 'TULPA-EARS-LINE 500 900 12:00:09 [me] first\\n'; sleep 60";
        let notes = Arc::new(Notes::default());
        let outbox = Arc::new(Outbox::default());
        let ears = Arc::new(Ears::new(
            Ok(settings(&dir, script)),
            Arc::new(Stop::new()),
            Arc::clone(&outbox),
            Arc::clone(&notes) as Arc<dyn Journal>,
            Box::new(|| {}),
        ));
        assert_eq!(at(&ask(&ears, "ears.start")?, "/ok"), true);
        assert!(line_goes_out(&outbox, "first"), "the capture never ran");
        let pid = ears.state().pid().ok_or("no capture")?;
        capture::REFUSE.with(|r| r.replace(Some(("tulpa-ears-stop", Box::new(|| {})))));
        assert_eq!(at(&ask(&ears, "ears.stop")?, "/ok"), true);
        assert!(
            capture::REFUSE.with(|r| r.borrow().is_none()),
            "the stop thread was not asked for"
        );
        // Its waiter reaps it, its reader reads to the end: every capture thread ends (far under its 60 s).
        assert!(capture_threads_end(&ears), "the capture outlived its stop");
        assert!(!pid_alive(pid), "the capture lives on");
        let (closing, given_up) = ears.inner.lock().map_or((usize::MAX, usize::MAX), |i| {
            (i.closing.len(), i.given_up.len())
        });
        assert_eq!((closing, given_up), (0, 0), "an entry was left behind");
        let said = notes.0.lock().map(|n| n.join("\n")).unwrap_or_default();
        assert!(said.contains("no thread to wait for capture"), "{said}");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0532, Fable F3: a stop at the moment a capture thread is refused. The capture was reaped through its group
    /// on the refusal, and its reap is told as any other: its closing entry is never given up, Quit finds nothing
    /// to kill or give up, and a later Pause sends nothing to the dead group (nothing said killed, and the group
    /// itself takes no signal).
    #[test]
    fn a_refused_capture_thread_with_a_stop_at_that_moment_leaves_nothing_given_up()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("refused-thread-stop")?;
        let notes = Arc::new(Notes::default());
        let mut ears = Ears::new(
            Ok(settings(&dir, "trap '' TERM; sleep 30")),
            Arc::new(Stop::new()),
            Arc::new(Outbox::default()),
            Arc::clone(&notes) as Arc<dyn Journal>,
            Box::new(|| {}),
        );
        // Short: on the bug, the entry is given up past two of these.
        ears.stop_grace = Duration::from_millis(200);
        let ears = Arc::new(ears);
        let stopper = Arc::clone(&ears);
        let (reply, group) = start_refused(&ears, "tulpa-ears-wait", move || {
            let _stopped = stopper.stop_with("test stop", None, super::End::Stop);
        })?;
        assert_eq!(at(&reply, "/ok"), false, "{reply}");
        ears.shutdown();
        let given_up = ears.inner.lock().map_or(usize::MAX, |i| i.given_up.len());
        assert_eq!(given_up, 0, "a reaped capture was given up");
        ears.pause();
        let said = notes.0.lock().map(|n| n.join("\n")).unwrap_or_default();
        assert!(
            said.contains("ears: capture pid") && said.contains("told to stop"),
            "the stop came: {said}"
        );
        assert!(
            !said.contains("killed again"),
            "Pause killed the dead group: {said}"
        );
        assert!(
            !said.contains("did not stop in"),
            "Quit killed the dead group: {said}"
        );
        assert!(!group.kill(), "the reaped capture's group takes a signal");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// #0532, invariant: no list holds a reaped capture. A capture given up (its exit came past every bound) and
    /// then reaped by its waiter leaves the given-up list at its reap, not at the next Pause.
    #[test]
    fn a_given_up_capture_leaves_the_list_at_its_reap() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("given-up-reaped")?;
        let (ears, _, _) = organ(Ok(settings(&dir, "exit 0")));
        let child = exec::cleared(Path::new("/bin/sh"), &dir)
            .args(["-c", "exit 0"])
            .spawn()?;
        let group = exec::Group::of(&child);
        let (exited, read) = (Arc::new(Stop::new()), Arc::new(Stop::new()));
        read.set();
        ears.inner
            .lock()
            .map_err(|e| e.to_string())?
            .given_up
            .push(Closing {
                generation: 999,
                mode: EarsMode::Call,
                since: SystemTime::now(),
                exited: Arc::clone(&exited),
                read: Arc::clone(&read),
                pid: child.id(),
                group: Arc::clone(&group),
            });
        ears.wait_for(child, &group, &exited, &read, 999);
        let given_up = ears.inner.lock().map_or(usize::MAX, |i| i.given_up.len());
        assert_eq!(given_up, 0, "a reaped capture stayed given up");
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
