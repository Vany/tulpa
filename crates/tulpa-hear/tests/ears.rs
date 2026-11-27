//! `--check` and `--ears` as a caller sees them: exit codes and lines.
//!
//! `--ears` in a build without the `whisper` feature refuses with exit 2 and says why, before any check; with the
//! feature a failing check refuses it with exit 1 (the failing lines on stderr) before any device is opened.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A temp models directory, removed on drop.
struct Models(PathBuf);

impl Models {
    fn new(tag: &str, files: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("tulpa-hear-ears-{tag}-{}", std::process::id()));
        let _gone = std::fs::remove_dir_all(&dir);
        assert!(std::fs::create_dir_all(&dir).is_ok());
        if files {
            for f in ["ggml-large-v3-turbo-q5_0.bin", "ggml-silero-v5.1.2.bin"] {
                assert!(std::fs::write(dir.join(f), b"ggml").is_ok());
            }
        }
        Self(dir)
    }

    fn run(&self, arg: &str) -> (Option<i32>, String, String) {
        let out: Option<Output> = Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
            .arg(arg)
            .env("EARS_MODELS", &self.0)
            .output()
            .ok();
        let text = |b: Option<&Vec<u8>>| b.map(|b| String::from_utf8_lossy(b).into_owned());
        (
            out.as_ref().and_then(|o| o.status.code()),
            text(out.as_ref().map(|o| &o.stdout)).unwrap_or_default(),
            text(out.as_ref().map(|o| &o.stderr)).unwrap_or_default(),
        )
    }
}

impl Drop for Models {
    fn drop(&mut self) {
        let _gone = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn check_prints_one_line_per_check_and_exits_by_them() {
    let m = Models::new("check", true);
    let (code, out, _) = m.run("--check");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    let names: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.split_once(": ").map(|(n, _)| n))
        .collect();
    assert_eq!(names, ["macos", "model", "vad", "whisper"]);
    assert!(
        lines.iter().skip(1).take(2).all(|l| l.contains(": ok ")),
        "{out}"
    );
    let all_ok = cfg!(target_os = "macos") && cfg!(feature = "whisper");
    if !cfg!(target_os = "macos") {
        assert_eq!(lines.first(), Some(&"macos: FAIL not macOS"));
    }
    let want = if all_ok { Some(0) } else { Some(1) };
    assert_eq!(code, want, "{out}");
}

#[test]
fn check_fails_on_missing_models() {
    let m = Models::new("check-empty", false);
    let (code, out, _) = m.run("--check");
    assert_eq!(code, Some(1));
    assert!(
        out.contains("model: FAIL ") && out.contains("vad: FAIL "),
        "{out}"
    );
}

#[cfg(not(feature = "whisper"))]
#[test]
fn ears_without_whisper_exits_2_before_any_check() {
    // The models are missing too: the feature is reported, with 2, not the check's 1.
    let m = Models::new("nowhisper", false);
    let (code, _, err) = m.run("--ears");
    assert_eq!(code, Some(2));
    assert!(err.contains("built without whisper"), "{err}");
    assert!(!err.contains("FAIL"), "{err}");
}

#[cfg(feature = "whisper")]
#[test]
fn ears_refuses_on_a_failing_check_before_any_device() {
    let m = Models::new("refuse", false);
    let (code, out, err) = m.run("--ears");
    assert_eq!(code, Some(1));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("model: FAIL "), "{err}");
}
