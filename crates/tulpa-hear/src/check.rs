//! `tulpa-hear --check`: the preflight the tray or a human runs, and the gate `--ears` passes before it opens any
//! device. Each check is one line, `<name>: ok` (with a detail) or `<name>: FAIL <why>`:
//!
//! - `macos`: this is macOS 14.6 or later (the process tap's floor, [`crate::osver::TAP_MIN`]), from
//!   `SystemVersion.plist`;
//! - `model` and `vad`: [`MODEL_FILE`] and [`VAD_FILE`] in the models directory ([`models_dir`]) exist, are regular
//!   files, open for reading and are not empty;
//! - `whisper`: this build has the `whisper` feature.
//!
//! Nothing here opens an audio device or writes a file; the model files are opened, not read. Pure over its
//! arguments, so it runs on every platform (off macOS the `macos` line is `FAIL not macOS`).

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::osver;

/// The whisper model's file name inside `EARS_MODELS` (as the old capture script).
pub const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
/// The Silero VAD model's file name inside `EARS_MODELS`.
pub const VAD_FILE: &str = "ggml-silero-v5.1.2.bin";

/// Where the models are: `EARS_MODELS`, else `$HOME/.cache/calltx`.
#[must_use]
pub fn models_dir(ears_models: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    ears_models.filter(|s| !s.is_empty()).map_or_else(
        || {
            home.filter(|h| !h.is_empty())
                .map(|h| Path::new(h).join(".cache/calltx"))
        },
        |m| Some(PathBuf::from(m)),
    )
}

/// One check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Check {
    /// What was checked: `macos`, `model`, `vad` or `whisper`.
    pub name: &'static str,
    /// `Ok(detail)` (possibly empty) or `Err(why)`.
    pub result: Result<String, String>,
}

impl Check {
    /// Whether it passed.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.result.is_ok()
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(d) if d.is_empty() => write!(f, "{}: ok", self.name),
            Ok(d) => write!(f, "{}: ok {d}", self.name),
            Err(why) => write!(f, "{}: FAIL {why}", self.name),
        }
    }
}

/// Runs every check, in the order `macos`, `model`, `vad`, `whisper`; none stops the others.
///
/// The binary passes `cfg!(target_os = "macos")`, [`osver::SYSTEM_VERSION_PLIST`], the [`models_dir`] (`None` when
/// neither `EARS_MODELS` nor `HOME` is set) and `cfg!(feature = "whisper")`; tests pass temp files and either answer.
#[must_use]
pub fn run(on_macos: bool, plist: &Path, models: Option<&Path>, whisper: bool) -> Vec<Check> {
    let macos = if on_macos {
        osver::check_tap_at(plist)
            .map(|v| v.to_string())
            .map_err(|e| e.to_string())
    } else {
        Err("not macOS".to_owned())
    };
    let file = |name: &str| {
        models.map_or_else(
            || Err("neither EARS_MODELS nor HOME is set".to_owned()),
            |dir| model_file(&dir.join(name)),
        )
    };
    vec![
        Check {
            name: "macos",
            result: macos,
        },
        Check {
            name: "model",
            result: file(MODEL_FILE),
        },
        Check {
            name: "vad",
            result: file(VAD_FILE),
        },
        Check {
            name: "whisper",
            result: if whisper {
                Ok(String::new())
            } else {
                Err("built without the `whisper` feature".to_owned())
            },
        },
    ]
}

/// A model file that exists, is a regular file, opens for reading and is not empty; `Ok` carries its path.
fn model_file(path: &Path) -> Result<String, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    if meta.len() == 0 {
        return Err(format!("{shown}: empty"));
    }
    File::open(path).map_err(|e| format!("{shown}: not readable: {e}"))?;
    Ok(shown.to_string())
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests")]

    use std::fs;

    use super::*;

    /// A temp dir with a fake plist and both model files, removed on drop.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tulpa-hear-check-{tag}-{}", std::process::id()));
            let _gone = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("models")).unwrap();
            fs::write(
                dir.join("SystemVersion.plist"),
                "<plist><dict><key>ProductVersion</key><string>27.2.1</string></dict></plist>",
            )
            .unwrap();
            fs::write(dir.join("models").join(MODEL_FILE), b"ggml").unwrap();
            fs::write(dir.join("models").join(VAD_FILE), b"ggml").unwrap();
            Self { dir }
        }

        fn plist(&self) -> PathBuf {
            self.dir.join("SystemVersion.plist")
        }

        fn models(&self) -> PathBuf {
            self.dir.join("models")
        }

        fn run(&self, on_macos: bool, whisper: bool) -> Vec<Check> {
            let (plist, models) = (self.plist(), self.models());
            run(on_macos, &plist, Some(&models), whisper)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _gone = fs::remove_dir_all(&self.dir);
        }
    }

    fn lines(checks: &[Check]) -> Vec<String> {
        checks.iter().map(ToString::to_string).collect()
    }

    /// The names of the failing checks.
    fn failing(checks: &[Check]) -> Vec<&'static str> {
        checks.iter().filter(|c| !c.ok()).map(|c| c.name).collect()
    }

    #[test]
    fn all_ok() {
        let f = Fixture::new("ok");
        let checks = f.run(true, true);
        let models = f.models();
        assert_eq!(
            lines(&checks),
            [
                "macos: ok 27.2.1".to_owned(),
                format!("model: ok {}", models.join(MODEL_FILE).display()),
                format!("vad: ok {}", models.join(VAD_FILE).display()),
                "whisper: ok".to_owned(),
            ]
        );
        assert!(checks.iter().all(Check::ok));
    }

    #[test]
    fn off_macos_the_version_fails_alone() {
        let f = Fixture::new("linux");
        let checks = f.run(false, true);
        assert_eq!(failing(&checks), ["macos"]);
        assert_eq!(checks.first().unwrap().to_string(), "macos: FAIL not macOS");
    }

    #[test]
    fn an_old_or_unreadable_macos_fails_alone() {
        let f = Fixture::new("old");
        fs::write(
            f.plist(),
            "<plist><dict><key>ProductVersion</key><string>14.5</string></dict></plist>",
        )
        .unwrap();
        let checks = f.run(true, true);
        assert_eq!(failing(&checks), ["macos"]);
        assert!(checks.first().unwrap().to_string().contains("14.5"));
        fs::remove_file(f.plist()).unwrap();
        assert_eq!(failing(&f.run(true, true)), ["macos"]);
    }

    #[test]
    fn a_missing_empty_or_odd_model_file_fails_alone() {
        for (name, file) in [("model", MODEL_FILE), ("vad", VAD_FILE)] {
            let f = Fixture::new(name);
            let path = f.models().join(file);
            fs::remove_file(&path).unwrap();
            let checks = f.run(true, true);
            assert_eq!(failing(&checks), [name], "missing {name}");
            fs::write(&path, b"").unwrap();
            let checks = f.run(true, true);
            assert_eq!(failing(&checks), [name], "empty {name}");
            assert!(lines(&checks).iter().any(|l| l.ends_with(": empty")));
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            let checks = f.run(true, true);
            assert_eq!(failing(&checks), [name], "dir {name}");
            assert!(
                lines(&checks)
                    .iter()
                    .any(|l| l.ends_with("not a regular file"))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_model_file_fails() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("perm");
        let path = f.models().join(MODEL_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything; the check is then honestly ok.
        if File::open(&path).is_err() {
            assert_eq!(failing(&f.run(true, true)), ["model"]);
        }
    }

    #[test]
    fn no_models_dir_fails_both_model_checks() {
        let f = Fixture::new("nodir");
        let plist = f.plist();
        let checks = run(true, &plist, None, true);
        assert_eq!(failing(&checks), ["model", "vad"]);
    }

    #[test]
    fn a_build_without_whisper_fails_alone() {
        let f = Fixture::new("nowhisper");
        let checks = f.run(true, false);
        assert_eq!(failing(&checks), ["whisper"]);
        assert_eq!(
            checks.last().unwrap().to_string(),
            "whisper: FAIL built without the `whisper` feature"
        );
    }

    #[test]
    fn models_dir_prefers_ears_models_then_home() {
        assert_eq!(
            models_dir(Some("/m"), Some("/h")),
            Some(PathBuf::from("/m"))
        );
        assert_eq!(
            models_dir(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.cache/calltx"))
        );
        assert_eq!(models_dir(None, Some("")), None);
        assert_eq!(models_dir(None, None), None);
    }
}
