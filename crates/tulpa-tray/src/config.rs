//! Local TOML configuration for the tray organ.
//!
//! Only the sections the tray reads are parsed; every section is optional and
//! defaults to off. Organ postures accept an explicit `mode = "on" | "ask" |
//! "off"`, falling back to the section's `enabled` boolean when `mode` is absent.

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

use crate::state::{LinkState, ORGANS, Organ, OrganMode};

pub mod edit;

/// Default directory for the append-only activity log.
pub const DEFAULT_LOG_DIRECTORY: &str = "~/Library/Logs/tulpa";

/// Malformed local configuration.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tray config: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The subset of local TOML configuration the tray organ consumes.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct TrayAppConfig {
    /// The system-wide debug switch (top-level `debug = true`; off by default): the ears' capture writes each
    /// recognized piece's raw text, language and filter verdict into its side log ([`crate::ears::DEBUG_ENV`]).
    #[serde(default)]
    pub debug: bool,
    /// Where the activity log lives.
    #[serde(default)]
    pub app: AppSection,
    /// Whether the menu bar icon runs at all.
    #[serde(default)]
    pub tray: TraySection,
    /// Link display state (host alias, configured liveness).
    #[serde(default)]
    pub link: LinkSection,
    /// Organ posture sections, each read-only for the menu.
    #[serde(default)]
    pub hands: OrganSection,
    /// The ears organ: posture, capture command, self-check paths, voice.
    #[serde(default)]
    pub ears: EarsSection,
    /// Whether the eyes organ runs (screen capture).
    #[serde(default)]
    pub eyes: OrganSection,
    /// The voice: its posture and where it sounds (`output_device`).
    #[serde(default)]
    pub voice: VoiceSection,
    /// Whether the calls organ runs.
    #[serde(default)]
    pub calls: OrganSection,
    /// Tulpa's own keep-awake.
    #[serde(default)]
    pub keepawake: KeepAwakeSection,
    /// Port forwards on request ([`crate::forward`]): the ports this Mac allows.
    #[serde(default)]
    pub forward: ForwardSection,
    /// Link serves on request ([`crate::linkserve`]): the ports this Mac allows.
    #[serde(default)]
    pub linkserve: LinkServeSection,
}

/// Port forwards on request: off unless ports are listed.
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct ForwardSection {
    /// The loopback ports a forward may use, either way — this Mac's own list: a request for any other port is
    /// refused here, whatever the hub granted. Empty (absent): no forwards at all.
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// Link serves on request (#0375): off unless ports are listed.
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct LinkServeSection {
    /// The loopback ports a link may join, as either side — this Mac's own list: a serve for any
    /// other port is refused here, whatever the hub granted. Empty (absent): no link serves at all.
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// Tulpa's own keep-awake: the one thing that keeps the Mac awake, so the link is never lost to sleep.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct KeepAwakeSection {
    /// Keeps the Mac awake for as long as Tulpa runs (absent: `true`). With `false`, only while the
    /// agent works on it: a hands request in flight, the ears on.
    #[serde(default = "default_always_awake")]
    pub always: bool,
    /// caffeinate's assertion flags (absent: `-i -s`), e.g. `"-d -i -m -s -u"`: the letters d, i, m, s and u
    /// only. Read at start ([`crate::keepawake::configured_flags`]); a refused value is logged and the
    /// default is held instead.
    #[serde(default)]
    pub flags: Option<String>,
}

const fn default_always_awake() -> bool {
    true
}

impl Default for KeepAwakeSection {
    fn default() -> Self {
        Self {
            always: default_always_awake(),
            flags: None,
        }
    }
}

/// Application-level settings.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct AppSection {
    /// Directory holding the append-only activity log; `~` expands to `$HOME`.
    #[serde(default = "default_log_directory")]
    pub log_directory: String,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            log_directory: default_log_directory(),
        }
    }
}

/// Tray switch.
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct TraySection {
    /// Whether the menu bar icon starts.
    #[serde(default)]
    pub enabled: bool,
}

/// Link display and dial settings.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct LinkSection {
    /// Whether a link is wanted; drives the initial icon state.
    #[serde(default)]
    pub enabled: bool,
    /// Optional OpenSSH host alias, shown in the menu after `via`.
    #[serde(default)]
    pub host: String,
    /// Endpoint dialed on the server side; localhost there is the broker.
    #[serde(default = "default_target_host")]
    pub target_host: String,
    /// Port of the endpoint dialed on the server side.
    #[serde(default = "default_target_port")]
    pub target_port: u16,
    /// Device name carried by the protocol hello.
    #[serde(default = "default_device")]
    pub device: String,
    /// File holding the shared dial token, read fresh before every attempt.
    #[serde(default = "default_token_file")]
    pub token_file: String,
    /// Wi-Fi names of the office: on one of them the link dials the server directly.
    #[serde(default)]
    pub office_ssids: Vec<String>,
    /// The server's addresses on the office LAN, tried in order (an mDNS name first, then an IP).
    #[serde(default)]
    pub direct_hosts: Vec<String>,
}

fn default_target_host() -> String {
    "127.0.0.1".to_owned()
}

const fn default_target_port() -> u16 {
    18030
}

fn default_device() -> String {
    "tulpa".to_owned()
}

fn default_token_file() -> String {
    "~/.config/tulpa/token".to_owned()
}

impl Default for LinkSection {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            target_host: default_target_host(),
            target_port: default_target_port(),
            device: default_device(),
            token_file: default_token_file(),
            office_ssids: Vec::new(),
            direct_hosts: Vec::new(),
        }
    }
}

/// One organ's posture.
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct OrganSection {
    /// Legacy boolean posture; `mode` wins when both are set.
    #[serde(default)]
    pub enabled: bool,
    /// Explicit posture: `on`, `ask`, or `off`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Hands only: the local policy file (roots, named commands). Without
    /// one the hands refuse every verb.
    #[serde(default)]
    pub policy: Option<String>,
}

/// The ears organ's section: its posture plus what `ears.start` runs and
/// where the self-check looks. Paths and command elements expand a leading
/// `~` against `$HOME`.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct EarsSection {
    /// `mode = "on" | "off"` (or legacy `enabled`); anything but `on`
    /// refuses every `ears.*` and `voice.*` verb.
    #[serde(flatten)]
    pub organ: OrganSection,
    /// The capture command's argv, run as the tray's own child by `ears.start`.
    #[serde(default)]
    pub command: Vec<String>,
    /// Directory the capture writes the local microphone's chunks to.
    #[serde(default)]
    pub me_dir: Option<String>,
    /// Directory the capture writes the system audio's chunks to.
    #[serde(default)]
    pub them_dir: Option<String>,
    /// File the capture writes its errors to; its last line is the reason
    /// shown when the self-check finds it non-empty.
    #[serde(default)]
    pub err_file: Option<String>,
    /// Transcript files whose new lines may become events to the agent;
    /// `{device}` becomes the device name the capture gets in `TULPA_DEVICE`.
    #[serde(default)]
    pub transcript_files: Vec<String>,
    /// Inert since 2026-11-18 (the side decides: the microphone's lines always
    /// go, the system audio's only in a call): parsed so old configs load, set
    /// or not; a set key is noted in the journal once at startup.
    #[serde(default)]
    pub wake_words: Vec<String>,
    /// Inert since 2026-11-18, like `wake_words`: parsed, never read.
    #[serde(default)]
    pub wake_aliases: Vec<String>,
    /// Inert since 2026-11-18, like `wake_words`: parsed, never read.
    #[serde(default)]
    pub transcript_events: Option<String>,
    /// Inert since 2026-11-18, like `wake_words`: parsed, never read.
    #[serde(default)]
    pub followup_secs: Option<i64>,
    /// Inert since 2026-11-18, like `wake_words`: parsed, never read.
    #[serde(default)]
    pub followup_max_secs: Option<i64>,
    /// The `say` voice for `voice.say`.
    #[serde(default = "default_voice")]
    pub voice: String,
    /// Run the capture with the tray's whole environment instead of the
    /// cleared one (PATH, HOME, LANG, TMPDIR); `env` still applies on top.
    #[serde(default)]
    pub inherit_env: bool,
    /// Extra environment for the capture command, set as given.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// The agent's own voice: a synthesis command whose argv carries `{text}` where the text goes
    /// (one argument, no shell); it prints the path of a WAV it made. Empty: macOS `say` with `voice`.
    #[serde(default)]
    pub voice_synth: Vec<String>,
    /// The own voice kept loaded (streamed speech): a resident command (argv, no `{text}`) that
    /// reads one sentence per stdin line and answers each with a WAV path line (or `ERR <why>`).
    /// Preferred over `voice_synth` when set; `voice_synth`, then `say`, take over when it fails.
    #[serde(default)]
    pub voice_serve: Vec<String>,
    /// The resident plays the sound itself (streamed synthesis): `voice.say` sends it the whole text and
    /// reads `start <i> <ms>` / `end <i> <ms>` / `done` / `ERR` lines. Off by default.
    #[serde(default)]
    pub voice_serve_plays: bool,
    /// Working directory for `voice_synth` / `voice_serve` (models found relative to it).
    #[serde(default)]
    pub voice_cwd: Option<String>,
    /// Optional post-processing of the WAV: argv with `{in}` and `{out}` (e.g. a compressor via ffmpeg).
    #[serde(default)]
    pub voice_post: Vec<String>,
    /// Capture command per ears mode (`office`, `alone`, `meeting`, `call`,
    /// `stream`); `call` falls back to `command` when absent.
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, Vec<String>>,
    /// The microphone the capture records, by name (the menu's «Ears mic» keeps its pick here): the
    /// capture gets `EARS_MIC=":<name>"`. Empty or absent: `EARS_MIC` is left as `env` has it.
    #[serde(default)]
    pub mic_device: Option<String>,
}

/// The voice's section: its posture plus where `voice.say` sounds.
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct VoiceSection {
    /// `mode` / `enabled`, shown like any organ's posture.
    #[serde(flatten)]
    pub organ: OrganSection,
    /// The output device `voice.say` plays on, by name (the menu's «Voice output» keeps its pick
    /// here). Empty or absent: the system default output.
    #[serde(default)]
    pub output_device: Option<String>,
    /// How fast `voice.say` speaks, 0.5 to 2 (absent: 1, normal). Applied once, by what plays the
    /// sound: `say -r` at 175 × rate words a minute, `afplay -r` for a synthesized clip, `--speed`
    /// for a resident that plays itself.
    #[serde(default)]
    pub rate: Option<f64>,
}

fn default_voice() -> String {
    "Milena".to_owned()
}

impl Default for EarsSection {
    fn default() -> Self {
        Self {
            organ: OrganSection::default(),
            command: Vec::new(),
            me_dir: None,
            them_dir: None,
            err_file: None,
            transcript_files: Vec::new(),
            wake_words: Vec::new(),
            wake_aliases: Vec::new(),
            transcript_events: None,
            followup_secs: None,
            followup_max_secs: None,
            voice: default_voice(),
            inherit_env: false,
            env: std::collections::BTreeMap::new(),
            mic_device: None,
            modes: std::collections::BTreeMap::new(),
            voice_synth: Vec::new(),
            voice_serve: Vec::new(),
            voice_serve_plays: false,
            voice_cwd: None,
            voice_post: Vec::new(),
        }
    }
}

impl OrganSection {
    /// Resolves the effective posture.
    ///
    /// # Errors
    ///
    /// Returns an error for an unrecognized `mode` string.
    pub fn posture(&self) -> Result<OrganMode, ConfigError> {
        match self.mode.as_deref() {
            Some("on") => Ok(OrganMode::On),
            Some("ask") => Ok(OrganMode::Ask),
            Some("off") => Ok(OrganMode::Off),
            Some(other) => Err(ConfigError(format!(
                "unknown organ mode {other:?}; expected \"on\", \"ask\", or \"off\""
            ))),
            None => Ok(if self.enabled {
                OrganMode::On
            } else {
                OrganMode::Off
            }),
        }
    }
}

impl TrayAppConfig {
    /// Parses and validates local TOML without reading any file itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the TOML is invalid or an organ `mode` is unknown.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(input).map_err(|error| ConfigError(error.to_string()))?;
        for organ in ORGANS {
            config.section(organ).posture()?;
        }
        Ok(config)
    }

    /// Per-organ postures in display order.
    pub fn organ_modes(&self) -> Vec<(Organ, OrganMode)> {
        ORGANS
            .into_iter()
            .map(|organ| {
                let mode = self.section(organ).posture().unwrap_or(OrganMode::Off);
                (organ, mode)
            })
            .collect()
    }

    /// Link state the icon starts in: configured links begin at connecting.
    pub const fn initial_link_state(&self) -> LinkState {
        if self.link.enabled {
            LinkState::Connecting
        } else {
            LinkState::Off
        }
    }

    /// The dial spec for a configured link; `None` when the link is disabled.
    /// The token path's leading `~` expands against `$HOME`.
    ///
    /// # Errors
    ///
    /// Returns an error when the link is enabled but its settings are unusable
    /// (empty alias, bad device name, zero port, empty token path).
    pub fn dial_spec(&self) -> Result<Option<crate::link::DialSpec>, crate::link::LinkError> {
        if !self.link.enabled {
            return Ok(None);
        }
        Ok(Some(
            crate::link::DialSpec::new(
                &self.link.host,
                &self.link.target_host,
                self.link.target_port,
                &self.link.device,
                expand_tilde(&self.link.token_file),
            )?
            .with_direct(
                self.link.office_ssids.clone(),
                self.link.direct_hosts.clone(),
            ),
        ))
    }

    /// Host alias shown after `via`, if configured.
    pub fn host_alias(&self) -> Option<String> {
        Some(self.link.host.clone()).filter(|host| !host.is_empty())
    }

    /// Absolute path of the append-only activity log.
    pub fn log_path(&self) -> PathBuf {
        expand_tilde(&self.app.log_directory).join("tulpa.log")
    }

    /// Where the forwards' ssh pids are kept, beside the log, for the next start's sweep.
    pub fn forward_pid_path(&self) -> PathBuf {
        expand_tilde(&self.app.log_directory).join("forward.pids")
    }

    /// The forward ports allowed on this Mac, zero dropped, each once.
    pub fn forward_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .forward
            .ports
            .iter()
            .copied()
            .filter(|p| *p > 0)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// The link ports allowed on this Mac, zero dropped, each once.
    pub fn link_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .linkserve
            .ports
            .iter()
            .copied()
            .filter(|p| *p > 0)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    const fn section(&self, organ: Organ) -> &OrganSection {
        match organ {
            Organ::Hands => &self.hands,
            Organ::Ears => &self.ears.organ,
            Organ::Eyes => &self.eyes,
            Organ::Voice => &self.voice.organ,
            Organ::Calls => &self.calls,
        }
    }
}

fn default_log_directory() -> String {
    DEFAULT_LOG_DIRECTORY.to_owned()
}

/// Expands a leading `~` to `$HOME`; anything else passes through unchanged.
pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_with(std::env::var_os("HOME").map(PathBuf::from).as_deref(), path)
}

pub(crate) fn expand_tilde_with(home: Option<&std::path::Path>, path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = home {
            return home.to_path_buf();
        }
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_defaults_everything_off() -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml("")?;
        assert!(!config.tray.enabled);
        assert_eq!(config.initial_link_state(), LinkState::Off);
        assert_eq!(config.host_alias(), None);
        assert_eq!(config.app.log_directory, DEFAULT_LOG_DIRECTORY);
        assert!(
            config
                .organ_modes()
                .iter()
                .all(|(_, mode)| *mode == OrganMode::Off)
        );
        Ok(())
    }

    #[test]
    fn forwards_are_off_until_ports_are_listed() -> Result<(), Box<dyn std::error::Error>> {
        assert!(TrayAppConfig::from_toml("")?.forward_ports().is_empty());
        assert_eq!(
            TrayAppConfig::from_toml("[forward]\nports = [8080, 53682, 0, 8080]\n")?
                .forward_ports(),
            vec![8080, 53682],
            "zero dropped, each once"
        );
        assert!(
            TrayAppConfig::from_toml("[forward]\nports = [70000]\n").is_err(),
            "not a port"
        );
        let config = TrayAppConfig::from_toml("[app]\nlog_directory = \"/tmp/tulpa-logs\"\n")?;
        assert_eq!(
            config.forward_pid_path(),
            PathBuf::from("/tmp/tulpa-logs/forward.pids")
        );
        Ok(())
    }

    #[test]
    fn links_are_off_until_ports_are_listed() -> Result<(), Box<dyn std::error::Error>> {
        assert!(TrayAppConfig::from_toml("")?.link_ports().is_empty());
        assert_eq!(
            TrayAppConfig::from_toml("[linkserve]\nports = [24642, 25565, 0, 24642]\n")?
                .link_ports(),
            vec![24642, 25565],
            "zero dropped, each once"
        );
        assert!(
            TrayAppConfig::from_toml("[linkserve]\nports = [70000]\n").is_err(),
            "not a port"
        );
        Ok(())
    }

    #[test]
    fn the_keep_awake_is_always_on_unless_switched_off() -> Result<(), Box<dyn std::error::Error>> {
        assert!(TrayAppConfig::from_toml("")?.keepawake.always);
        assert!(TrayAppConfig::from_toml("[keepawake]\n")?.keepawake.always);
        assert!(
            TrayAppConfig::from_toml("[keepawake]\nalways = true\n")?
                .keepawake
                .always
        );
        assert!(
            !TrayAppConfig::from_toml("[keepawake]\nalways = false\n")?
                .keepawake
                .always
        );
        assert!(TrayAppConfig::from_toml("[keepawake]\nalways = \"no\"\n").is_err());
        assert_eq!(TrayAppConfig::from_toml("")?.keepawake.flags, None);
        assert_eq!(
            TrayAppConfig::from_toml("[keepawake]\nflags = \"-dimsu\"\n")?
                .keepawake
                .flags
                .as_deref(),
            Some("-dimsu"),
            "read as given; checked when the keep-awake starts"
        );
        Ok(())
    }

    #[test]
    fn explicit_modes_win_and_unknown_modes_fail() -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml(
            r#"
            [hands]
            enabled = false
            mode = "ask"

            [eyes]
            enabled = true
            "#,
        )?;
        let modes = config.organ_modes();
        let posture = |organ: Organ| {
            modes
                .iter()
                .find(|(candidate, _)| *candidate == organ)
                .map(|(_, mode)| *mode)
        };
        assert_eq!(posture(Organ::Hands), Some(OrganMode::Ask));
        assert_eq!(posture(Organ::Eyes), Some(OrganMode::On));
        assert_eq!(posture(Organ::Ears), Some(OrganMode::Off));

        let rejected = TrayAppConfig::from_toml("[voice]\nmode = \"sometimes\"");
        let Err(error) = rejected else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown organ mode must be rejected",
            )
            .into());
        };
        assert!(error.to_string().contains("sometimes"));
        Ok(())
    }

    #[test]
    fn enabled_link_starts_connecting_with_alias() -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml(
            r#"
            [link]
            enabled = true
            host = "EXAMPLE_ALIAS"
            "#,
        )?;
        assert_eq!(config.initial_link_state(), LinkState::Connecting);
        assert_eq!(config.host_alias().as_deref(), Some("EXAMPLE_ALIAS"));
        let spec = config.dial_spec()?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "an enabled link must produce a dial spec",
            )
        })?;
        assert_eq!(spec.alias(), "EXAMPLE_ALIAS");
        assert_eq!(spec.target(), ("127.0.0.1", 18030));
        assert_eq!(spec.device(), "tulpa");
        Ok(())
    }

    #[test]
    fn disabled_link_has_no_dial_spec_and_bad_enabled_link_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml("")?;
        assert_eq!(config.dial_spec()?, None);

        let bad = TrayAppConfig::from_toml("[link]\nenabled = true")?;
        assert!(
            bad.dial_spec().is_err(),
            "an enabled link without a host alias is refused"
        );
        Ok(())
    }

    #[test]
    fn ears_section_parses_command_paths_and_voice() -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml(
            r#"
            [ears]
            mode = "on"
            command = ["/bin/bash", "~/capture/capture.sh"]
            me_dir = "~/capture/me"
            them_dir = "~/capture/them"
            err_file = "~/capture/err.log"
            transcript_files = ["~/capture/me.{device}.txt"]
            wake_words = ["example"]
            wake_aliases = ["exampel", "officeexample"]
            transcript_events = "wake"
            followup_secs = 12
            followup_max_secs = 45
            "#,
        )?;
        assert_eq!(config.ears.organ.posture()?, OrganMode::On);
        assert_eq!(config.ears.command, ["/bin/bash", "~/capture/capture.sh"]);
        assert_eq!(config.ears.me_dir.as_deref(), Some("~/capture/me"));
        assert_eq!(config.ears.them_dir.as_deref(), Some("~/capture/them"));
        assert_eq!(config.ears.err_file.as_deref(), Some("~/capture/err.log"));
        assert_eq!(config.ears.transcript_files, ["~/capture/me.{device}.txt"]);
        assert_eq!(config.ears.wake_words, ["example"]);
        assert_eq!(config.ears.wake_aliases, ["exampel", "officeexample"]);
        assert_eq!(config.ears.transcript_events.as_deref(), Some("wake"));
        assert_eq!(config.ears.followup_secs, Some(12));
        assert_eq!(config.ears.followup_max_secs, Some(45));
        assert_eq!(config.ears.voice, "Milena", "the default voice");

        let empty = TrayAppConfig::from_toml("")?;
        assert_eq!(empty.ears.organ.posture()?, OrganMode::Off);
        assert!(empty.ears.command.is_empty());
        assert!(empty.ears.transcript_files.is_empty() && empty.ears.wake_words.is_empty());
        assert!(empty.ears.wake_aliases.is_empty());
        assert_eq!(empty.ears.transcript_events, None);
        assert_eq!(empty.ears.followup_secs, None);
        assert_eq!(empty.ears.followup_max_secs, None);
        assert_eq!(empty.ears.voice, "Milena");

        let voiced = TrayAppConfig::from_toml("[ears]\nenabled = true\nvoice = \"Alex\"")?;
        assert_eq!(voiced.ears.organ.posture()?, OrganMode::On);
        assert_eq!(voiced.ears.voice, "Alex");

        assert!(
            TrayAppConfig::from_toml("[ears]\nmode = \"loud\"").is_err(),
            "an unknown ears mode is refused like any other organ's"
        );
        Ok(())
    }

    #[test]
    fn tilde_expands_against_home_and_only_home() {
        let home = std::path::Path::new("/tmp/example-home");
        assert_eq!(
            expand_tilde_with(Some(home), "~/Library/Logs/tulpa"),
            home.join("Library/Logs/tulpa")
        );
        assert_eq!(expand_tilde_with(Some(home), "~"), home.to_path_buf());
        assert_eq!(
            expand_tilde_with(Some(home), "/var/log/tulpa"),
            std::path::PathBuf::from("/var/log/tulpa")
        );
        assert_eq!(
            expand_tilde_with(None, "~/Library/Logs/tulpa"),
            std::path::PathBuf::from("~/Library/Logs/tulpa"),
            "without a home the tilde stays literal rather than guessing"
        );
        assert_eq!(
            expand_tilde_with(Some(home), "~other/place"),
            std::path::PathBuf::from("~other/place"),
            "another user's tilde is not ours to expand"
        );
    }
}
