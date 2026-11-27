//! The local policy file: the owner edits it on the Mac, never over the link.
//!
//! ```toml
//! roots = ["~/work/project"]
//! read = "on"          # off | ask | on
//! list = "on"
//! git = "on"
//! put = "ask"          # xfer.put into the roots; xfer.get follows `read`
//! ask_timeout_secs = 30
//!
//! [[command]]
//! name = "project-test"
//! argv = ["/usr/bin/make", "test"]
//! cwd = "~/work/project"
//! mode = "ask"         # off | ask | on; ask when absent
//! timeout_secs = 300
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// The longest a pending `ask` may wait for a click before it is refused.
const ASK_TIMEOUT_MAX: u64 = 900;
/// A named command's run limit when the policy names none.
const RUN_TIMEOUT_DEFAULT: u64 = 120;
/// The longest any named command may run.
const RUN_TIMEOUT_MAX: u64 = 3600;
/// The git the hands call unless the policy names another absolute path.
const GIT_DEFAULT: &str = "/usr/bin/git";

/// Whether a verb runs: never, after a local click, or by policy.
///
/// `#[non_exhaustive]` (clippy `exhaustive_enums`): a new posture is a likely
/// step of the ladder; matches live in another crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Posture {
    /// Refused without asking.
    Off,
    /// A local click per request; no click in time is a refusal.
    Ask,
    /// Allowed while the link is up and nothing is paused.
    On,
}

impl Posture {
    /// The word used in the policy and the activity log.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::On => "on",
        }
    }
}

/// A policy that can't be used; the hands stay refused until it is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(String);

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "hands policy: {}", self.0)
    }
}

impl std::error::Error for PolicyError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default = "posture_on")]
    read: Posture,
    #[serde(default = "posture_on")]
    list: Posture,
    #[serde(default = "posture_on")]
    git: Posture,
    /// The data stream's `xfer.put` outside full mode: a file written into a
    /// root, so a click unless the policy says otherwise.
    #[serde(default = "posture_ask")]
    put: Posture,
    #[serde(default)]
    git_program: Option<String>,
    #[serde(default)]
    ask_timeout_secs: Option<u64>,
    #[serde(default)]
    command: Vec<RawCommand>,
    /// Start in full mode (any shell command, any file) — the menu toggles it too.
    #[serde(default)]
    full: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    name: String,
    argv: Vec<String>,
    cwd: String,
    #[serde(default)]
    mode: Option<Posture>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const fn posture_on() -> Posture {
    Posture::On
}

const fn posture_ask() -> Posture {
    Posture::Ask
}

/// A command the policy names: fixed argv, fixed cwd inside a root.
///
/// `#[non_exhaustive]` (clippy `exhaustive_structs`): read from the tray side,
/// never built there; new fields must not break construction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NamedCommand {
    /// The program (absolute) and its fixed arguments.
    pub argv: Vec<String>,
    /// The working directory, a real path inside a root.
    pub cwd: PathBuf,
    /// Off, ask (default) or on.
    pub mode: Posture,
    /// The run limit; past it the process group is killed.
    pub timeout: Duration,
}

/// The checked policy: roots as real paths, commands validated.
#[derive(Debug, Clone)]
pub struct Policy {
    home: PathBuf,
    roots: Vec<PathBuf>,
    read: Posture,
    list: Posture,
    git: Posture,
    put: Posture,
    git_program: PathBuf,
    ask_timeout: Duration,
    commands: BTreeMap<String, NamedCommand>,
    own_file: Option<PathBuf>,
    full: bool,
}

impl Policy {
    /// Parses and checks a policy. Roots must exist; each becomes its real
    /// path. A root inside the deny list, a relative program, a command name
    /// outside `[A-Za-z0-9_.-]`, or a cwd outside every root is an error:
    /// a wrong policy refuses loudly instead of half-working.
    ///
    /// # Errors
    ///
    /// Returns why the policy can't be used.
    pub fn from_toml(
        input: &str,
        home: &Path,
        own_file: Option<&Path>,
    ) -> Result<Self, PolicyError> {
        let raw: RawPolicy =
            toml::from_str(input).map_err(|e| PolicyError(e.message().to_owned()))?;
        let own_file =
            own_file.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
        let mut policy = Self {
            home: home.to_path_buf(),
            roots: Vec::new(),
            read: raw.read,
            list: raw.list,
            git: raw.git,
            put: raw.put,
            git_program: PathBuf::from(raw.git_program.as_deref().unwrap_or(GIT_DEFAULT)),
            ask_timeout: Duration::from_secs(
                raw.ask_timeout_secs.unwrap_or(30).clamp(1, ASK_TIMEOUT_MAX),
            ),
            commands: BTreeMap::new(),
            own_file,
            full: raw.full,
        };
        if !policy.git_program.is_absolute() {
            return Err(PolicyError(
                "git_program must be an absolute path".to_owned(),
            ));
        }
        for root in &raw.roots {
            let real = std::fs::canonicalize(policy.expand(root))
                .map_err(|e| PolicyError(format!("root {root}: {e}")))?;
            if !real.is_dir() {
                return Err(PolicyError(format!("root {root} is not a directory")));
            }
            if policy.denied(&real) {
                return Err(PolicyError(format!("root {root} is on the deny list")));
            }
            policy.roots.push(real);
        }
        for command in raw.command {
            policy.add_command(command)?;
        }
        Ok(policy)
    }

    fn add_command(&mut self, raw: RawCommand) -> Result<(), PolicyError> {
        let name = raw.name;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(PolicyError(format!(
                "command name {name:?}: use [A-Za-z0-9_.-], 1-64"
            )));
        }
        let Some(program) = raw.argv.first() else {
            return Err(PolicyError(format!("command {name}: empty argv")));
        };
        if !Path::new(program).is_absolute() {
            return Err(PolicyError(format!(
                "command {name}: program must be an absolute path"
            )));
        }
        let cwd = std::fs::canonicalize(self.expand(&raw.cwd))
            .map_err(|e| PolicyError(format!("command {name} cwd: {e}")))?;
        if !self.inside_roots(&cwd) || self.denied(&cwd) {
            return Err(PolicyError(format!(
                "command {name}: cwd outside the roots"
            )));
        }
        let timeout = raw
            .timeout_secs
            .unwrap_or(RUN_TIMEOUT_DEFAULT)
            .clamp(1, RUN_TIMEOUT_MAX);
        let command = NamedCommand {
            argv: raw.argv,
            cwd,
            mode: raw.mode.unwrap_or(Posture::Ask),
            timeout: Duration::from_secs(timeout),
        };
        if self.commands.insert(name.clone(), command).is_some() {
            return Err(PolicyError(format!("command {name} is listed twice")));
        }
        Ok(())
    }

    /// `~` and `~/…` become the home directory; other paths are taken as given.
    #[must_use]
    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            return self.home.clone();
        }
        path.strip_prefix("~/")
            .map_or_else(|| PathBuf::from(path), |rest| self.home.join(rest))
    }

    /// True when `real` (already a real path) lies inside a listed root.
    #[must_use]
    pub fn inside_roots(&self, real: &Path) -> bool {
        let real = fold(real);
        self.roots.iter().any(|root| real.starts_with(fold(root)))
    }

    /// The deny list (§2, §10 B2) plus the policy file itself.
    #[must_use]
    pub fn denied(&self, path: &Path) -> bool {
        crate::deny::is_denied(path, &self.home)
            || self
                .own_file
                .as_deref()
                .is_some_and(|own| fold(path) == fold(own))
    }

    /// The roots, as real paths.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The posture of `read`, `list`, `git.*` and the data stream's
    /// `xfer.get` (as `read`) and `xfer.put`; `None` for any other verb.
    #[must_use]
    pub fn posture(&self, verb: &str) -> Option<Posture> {
        match verb {
            "read" | "xfer.get" => Some(self.read),
            "xfer.put" => Some(self.put),
            "list" => Some(self.list),
            "git.status" | "git.diff" | "git.log" => Some(self.git),
            _ => None,
        }
    }

    /// A named command, when the policy lists it.
    #[must_use]
    pub fn command(&self, name: &str) -> Option<&NamedCommand> {
        self.commands.get(name)
    }

    /// The git binary.
    #[must_use]
    pub fn git_program(&self) -> &Path {
        &self.git_program
    }

    /// How long a pending click may wait.
    #[must_use]
    pub const fn ask_timeout(&self) -> Duration {
        self.ask_timeout
    }

    /// Whether the policy starts the hands in full mode.
    #[must_use]
    pub const fn full(&self) -> bool {
        self.full
    }

    /// The home directory the policy was read against.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }
}

/// macOS volumes are case-insensitive by default: `~/.SSH` is `~/.ssh`. Paths
/// are compared folded there and as-is elsewhere.
pub fn fold(path: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_roots_commands_and_refuses_bad_ones() -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        std::fs::create_dir(home.path().join("proj"))?;
        std::fs::create_dir(home.path().join(".ssh"))?;
        let good = r#"
roots = ["~/proj"]
read = "on"
[[command]]
name = "t"
argv = ["/bin/echo", "hi"]
cwd = "~/proj"
"#;
        let policy = Policy::from_toml(good, home.path(), None)?;
        assert_eq!(policy.roots().len(), 1);
        assert_eq!(policy.command("t").map(|c| c.mode), Some(Posture::Ask));
        assert_eq!(policy.posture("git.diff"), Some(Posture::On));
        assert_eq!(policy.posture("write"), None);
        assert_eq!(
            policy.posture("xfer.put"),
            Some(Posture::Ask),
            "a put into the roots asks unless the policy says otherwise"
        );
        assert_eq!(
            policy.posture("xfer.get"),
            Some(Posture::On),
            "get follows read"
        );

        let denied_root = "roots = [\"~/.ssh\"]";
        assert!(Policy::from_toml(denied_root, home.path(), None).is_err());
        let relative =
            "roots = [\"~/proj\"]\n[[command]]\nname=\"t\"\nargv=[\"make\"]\ncwd=\"~/proj\"";
        assert!(Policy::from_toml(relative, home.path(), None).is_err());
        let outside =
            "roots = [\"~/proj\"]\n[[command]]\nname=\"t\"\nargv=[\"/bin/ls\"]\ncwd=\"~\"";
        assert!(Policy::from_toml(outside, home.path(), None).is_err());
        let unknown = "roots = []\nwrite = \"on\"";
        assert!(Policy::from_toml(unknown, home.path(), None).is_err());
        Ok(())
    }

    /// The installer's policy — full mode, no roots — loads.
    #[test]
    fn the_installers_full_policy_loads() -> Result<(), Box<dyn std::error::Error>> {
        let home = std::env::temp_dir();
        let policy = Policy::from_toml("full = true\nroots = []\n", &home, None)?;
        assert!(policy.full());
        assert!(
            !Policy::from_toml("roots = []\n", &home, None)?.full(),
            "off by default"
        );
        Ok(())
    }
}
