//! `--watchdog`: synthetic live logs judged (a latency jump triggers, a steady log with a drifted live clock does
//! not, too few phrases do not), the pid check (a `sleep` is refused and left running, a shadow-like command line is
//! signalled), and the binary on a gone pid (exit 0), a foreign pid (exit 1, nothing sent) and bad options (exit 2).

#![expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "small fixed test times around a fixed start; byte cuts within the log they cut"
)]

use std::io::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tulpa_hear::watchdog::{self, LiveTail, PidIs, Settings, Stat, Verdict, Watch, live_latencies};

/// The watchdog's start: a whole second, as the log's clock is.
const S: u64 = 1_700_000_000_000;
/// The live clock's drift once the shadow runs: t1 a minute behind the wall clock.
const DRIFT: u64 = 60_000;

/// One live line emitted in the second starting at `emit_ms` (read back as `emit_ms` + 500) whose `t1` is
/// `latency_ms` + `drift_ms` before that.
fn line(emit_ms: u64, latency_ms: u64, drift_ms: u64) -> String {
    let t1 = emit_ms - latency_ms - drift_ms;
    let secs = (emit_ms / 1000) % 86_400;
    format!(
        "TULPA-EARS-LINE {} {t1} {:02}:{:02}:{:02} [them] some words\n",
        t1 - 1_500,
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// `n` lines every `step_ms` from `from_ms`, latencies cycling through `lat`.
fn lines(from_ms: u64, step_ms: u64, n: u64, lat: &[u64], drift_ms: u64) -> String {
    (0..n)
        .map(|i| {
            let l = lat
                .get(usize::try_from(i).unwrap_or(0) % lat.len())
                .copied();
            line(from_ms + i * step_ms, l.unwrap_or(0), drift_ms)
        })
        .collect()
}

const STEADY: [u64; 5] = [1_000, 1_500, 2_000, 1_200, 1_800];
const JUMPED: [u64; 5] = [1_000, 4_000, 6_000, 3_000, 5_000];

/// Twenty steady phrases in the ten minutes before the start.
fn baseline_log() -> String {
    lines(S - 590_000, 29_000, 20, &STEADY, 0)
}

#[test]
fn a_latency_jump_triggers() {
    let log = baseline_log() + &lines(S + 10_000, 10_000, 10, &JUMPED, DRIFT);
    let mut w = Watch::new(Settings::default(), S);
    let v = w.judge(&live_latencies(&log), S + 120_000);
    assert_eq!(
        v,
        Verdict::Rise {
            baseline: Stat::new(1_000, 20),
            window: Stat::new(5_000, 10)
        }
    );
}

/// The same latencies after the start as before it, on a live clock a minute off: (emit − t1) is a minute larger,
/// its change is not, and nothing triggers.
#[test]
fn a_steady_log_does_not_trigger() {
    let log = baseline_log() + &lines(S + 10_000, 10_000, 10, &STEADY, DRIFT);
    let points = live_latencies(&log);
    let mut w = Watch::new(Settings::default(), S);
    for now in [S + 20_000, S + 60_000, S + 120_000] {
        assert!(
            !matches!(w.judge(&points, now), Verdict::Rise { .. }),
            "at +{} s",
            (now - S) / 1000
        );
    }
    assert_eq!(
        w.judge(&points, S + 120_000),
        Verdict::Within {
            baseline: Stat::new(1_000, 20),
            window: Some(Stat::new(1_000, 10)),
            n: 10
        }
    );
}

/// A rise of exactly the bound is not past it; one ms more is.
#[test]
fn the_bound_is_baseline_times_one_plus_the_rise() {
    let log = baseline_log() + &lines(S + 10_000, 10_000, 10, &[1_000, 2_300], DRIFT);
    let points = live_latencies(&log);
    let judge = |pct| Watch::new(Settings::new(pct, 120, 600), S).judge(&points, S + 120_000);
    assert!(
        matches!(judge(30), Verdict::Within { .. }),
        "1300 is not past 1000 + 30%"
    );
    assert!(
        matches!(judge(29), Verdict::Rise { .. }),
        "1300 is past 1000 + 29%"
    );
}

/// Fewer than five phrases in the window: not judged, however slow.
#[test]
fn four_phrases_in_the_window_are_not_judged() {
    let log = baseline_log() + &lines(S + 10_000, 10_000, 4, &[1_000, 9_000], DRIFT);
    let mut w = Watch::new(Settings::default(), S);
    assert_eq!(
        w.judge(&live_latencies(&log), S + 120_000),
        Verdict::Within {
            baseline: Stat::new(1_000, 20),
            window: None,
            n: 4
        }
    );
}

/// Three phrases before the start: no baseline, no judgement; it forms from the phrases before the window once
/// there are five, and is kept.
#[test]
fn too_few_baseline_phrases_wait_for_one() {
    let early = lines(S - 300_000, 60_000, 3, &STEADY, 0);
    let after = lines(S + 10_000, 20_000, 30, &STEADY, 0);
    let points = live_latencies(&(early + &after));
    let mut w = Watch::new(Settings::default(), S);
    // At +60 s: the window holds after-start phrases, the baseline span (up to the window) only the three.
    assert_eq!(w.judge(&points, S + 60_000), Verdict::NoBaseline { n: 3 });
    assert_eq!(w.baseline(), None);
    // At +200 s the span before the window (+80 s) holds 3 + 4 phrases: the baseline forms.
    assert!(!matches!(
        w.judge(&points, S + 200_000),
        Verdict::NoBaseline { .. }
    ));
    let b = w.baseline();
    assert_eq!(b.map(|b| b.n), Some(7));
    w.judge(&points, S + 500_000);
    assert_eq!(w.baseline(), b, "kept");
}

fn spawn(cmd: &str, args: &[&str]) -> Option<Child> {
    let c = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    assert!(c.is_ok(), "{c:?}");
    c.ok()
}

/// A pid whose process has exited and been reaped.
fn gone_pid() -> Option<u32> {
    let mut c = spawn("true", &[])?;
    assert!(c.wait().is_ok());
    Some(c.id())
}

/// A `sleep` is not a shadow: refused, nothing sent, still running.
#[test]
fn a_pid_that_is_not_a_shadow_is_refused() {
    let Some(mut child) = spawn("sleep", &["30"]) else {
        return;
    };
    assert_eq!(watchdog::check_pid(child.id()).ok(), Some(PidIs::Other));
    assert_eq!(watchdog::terminate(child.id()), Ok(PidIs::Other));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(matches!(child.try_wait(), Ok(None)), "still running");
    // This test's own process is never one either.
    assert_eq!(
        watchdog::check_pid(std::process::id()).ok(),
        Some(PidIs::Other)
    );
    child.kill().ok();
    child.wait().ok();
}

/// The live ears run the same binary: a `tulpa-hear --ears` (or `--probe-mic`) command line is refused, nothing
/// sent, still running. Only `--shadow` makes a tulpa-hear one to signal.
#[test]
fn a_tulpa_hear_that_is_not_a_shadow_is_refused() {
    for args in [
        &["tulpa-hear", "--ears"][..],
        &["tulpa-hear", "--probe-mic", "5"][..],
    ] {
        let mut argv = vec!["-c", "sleep 5; exit 0"];
        argv.extend_from_slice(args);
        let Some(mut child) = spawn("sh", &argv) else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            watchdog::check_pid(child.id()).ok(),
            Some(PidIs::Other),
            "{args:?}"
        );
        assert_eq!(
            watchdog::terminate(child.id()),
            Ok(PidIs::Other),
            "{args:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "{args:?} still running"
        );
        child.kill().ok();
        child.wait().ok();
    }
}

/// A command line holding `tulpa-hear` and `--shadow` is verified and gets SIGTERM.
#[test]
fn a_verified_shadow_gets_sigterm() {
    // sh's own argv is `sh -c <script> tulpa-hear --shadow`; the script's last command is a builtin, so sh stays.
    let Some(mut child) = spawn("sh", &["-c", "sleep 5; exit 0", "tulpa-hear", "--shadow"]) else {
        return;
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(watchdog::check_pid(child.id()).ok(), Some(PidIs::Shadow));
    assert_eq!(watchdog::terminate(child.id()), Ok(PidIs::Shadow));
    let status = child.wait();
    assert_eq!(status.ok().and_then(|s| s.signal()), Some(15));
}

#[test]
fn a_gone_pid_is_gone_and_pids_0_and_1_are_never_signalled() {
    let Some(pid) = gone_pid() else { return };
    assert_eq!(watchdog::check_pid(pid).ok(), Some(PidIs::Gone));
    assert_eq!(watchdog::terminate(pid), Ok(PidIs::Gone));
    assert!(watchdog::terminate(0).is_err());
    assert!(watchdog::terminate(1).is_err());
}

/// The binary's exit code and stdout.
fn bin(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
        .args(args)
        .output();
    assert!(out.is_ok(), "{out:?}");
    out.map_or((None, String::new()), |o| {
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        )
    })
}

/// The binary exits 0 at once when the pid is gone, saying so.
#[test]
fn the_binary_exits_0_on_a_gone_pid() {
    let Some(pid) = gone_pid().map(|p| p.to_string()) else {
        return;
    };
    let out = bin(&[
        "--watchdog",
        "--live",
        "/nonexistent/ears.log",
        "--pid",
        &pid,
    ]);
    assert_eq!(out, (Some(0), format!("watchdog: pid {pid} gone\n")));
}

/// A pid that is not a shadow's: exit 1, nothing sent.
#[test]
fn the_binary_refuses_a_foreign_pid() {
    let Some(mut child) = spawn("sleep", &["30"]) else {
        return;
    };
    let out = bin(&[
        "--watchdog",
        "--live",
        "/nonexistent/ears.log",
        "--pid",
        &child.id().to_string(),
    ]);
    assert_eq!(out, (Some(1), String::new()));
    assert!(matches!(child.try_wait(), Ok(None)), "still running");
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn the_binary_refuses_bad_options() {
    for bad in [
        &["--watchdog"][..],
        &["--watchdog", "--live", "x"],
        &["--watchdog", "--pid", "1234"],
        &["--watchdog", "--live", "x", "--pid", "1"],
        &["--watchdog", "--live", "x", "--pid", "0"],
        &["--watchdog", "--live", "x", "--pid", "-5"],
        &[
            "--watchdog",
            "--live",
            "x",
            "--pid",
            "1234",
            "--pid",
            "1235",
        ],
        &[
            "--watchdog",
            "--live",
            "x",
            "--pid",
            "1234",
            "--rise-pct",
            "1001",
        ],
        &[
            "--watchdog",
            "--live",
            "x",
            "--pid",
            "1234",
            "--window-s",
            "0",
        ],
        &["--watchdog", "--live", "x", "--pid", "1234", "--baseline-s"],
        &["--watchdog", "--live", "x", "--pid", "1234", "--flow", "y"],
    ] {
        assert_eq!(bin(bad).0, Some(2), "{bad:?}");
    }
}

/// A real-shaped live log: the tray's `ears.log` fixture.
const EARS_LOG: &[u8] = include_bytes!("data/compare/ears.log");

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tulpa-hear-tail-{tag}-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&d);
    assert!(std::fs::create_dir_all(&d).is_ok(), "scratch dir");
    d.join("ears.log")
}

fn append(path: &Path, bytes: &[u8]) {
    let done = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(bytes));
    assert!(done.is_ok(), "append: {done:?}");
}

/// The tail's points now, `None` on a read error.
fn read(tail: &mut LiveTail, path: &Path) -> Option<Vec<(u64, i64)>> {
    tail.read(path).map(<[_]>::to_vec).ok()
}

/// What a whole re-read gives.
fn whole(bytes: &[u8]) -> Vec<(u64, i64)> {
    live_latencies(&String::from_utf8_lossy(bytes))
}

/// Grows the file at `path` by `bytes` cut at `cuts`, checking after each piece that the tail's points are a whole
/// re-read's.
fn grow_and_check(path: &Path, bytes: &[u8], cuts: &[usize]) {
    let _gone = std::fs::remove_file(path);
    append(path, b"");
    let mut tail = LiveTail::default();
    let mut from = 0;
    for &to in cuts.iter().chain([&bytes.len()]) {
        append(path, &bytes[from..to]);
        assert_eq!(
            read(&mut tail, path),
            Some(whole(&bytes[..to])),
            "grown to byte {to} of {}, cuts {cuts:?}",
            bytes.len()
        );
        from = to;
    }
}

/// A long log: the fixture, synthetic phrases over a midnight (the `HH:MM:SS` day rollover), CRLF lines, a line
/// with bytes that are not UTF-8, lines without the marker and a last line without its newline.
fn long_log() -> Vec<u8> {
    let midnight = S - S % 86_400_000 + 86_400_000;
    let mut log = EARS_LOG.to_vec();
    log.extend(baseline_log().bytes());
    log.extend(lines(midnight - 60_000, 7_000, 20, &JUMPED, DRIFT).bytes());
    log.extend(lines(S, 3_000, 5, &STEADY, 0).replace('\n', "\r\n").bytes());
    log.extend(b"TULPA-EARS-LINE 1700000000000 1700000001000 22:13:22 [me] \xff\xfe caf\xc3\n");
    log.extend(b"ears: capture stopped\n");
    log.extend(lines(S + 600_000, 11_000, 40, &STEADY, DRIFT).bytes());
    log.extend(EARS_LOG);
    log.extend(line(S + 900_000, 1_234, 0).trim_end().bytes());
    log
}

/// Every byte boundary of the fixture, alone and paired with a later one: at each read the points are exactly a
/// whole re-read's.
#[test]
fn the_tail_is_a_whole_reread_at_every_byte_boundary() {
    let path = scratch("bytes");
    assert!(
        whole(EARS_LOG).len() >= 5,
        "the fixture has spanned phrases"
    );
    for a in 0..=EARS_LOG.len() {
        grow_and_check(&path, EARS_LOG, &[a]);
        for b in (a..=EARS_LOG.len()).step_by(37) {
            grow_and_check(&path, EARS_LOG, &[a, b]);
        }
    }
    let _gone = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}

/// Random chunks (a fixed-seed generator) of a long log: at each read the points are exactly a whole re-read's.
#[test]
fn the_tail_is_a_whole_reread_in_random_chunks_of_a_long_log() {
    let path = scratch("chunks");
    let log = long_log();
    assert!(
        whole(&log).len() > 80,
        "the long log has many spanned phrases"
    );
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..200 {
        let mut cuts = Vec::new();
        let mut at = 0;
        loop {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            at += usize::try_from(seed >> 33).unwrap_or(0) % 300;
            if at >= log.len() {
                break;
            }
            cuts.push(at);
        }
        grow_and_check(&path, &log, &cuts);
    }
    let _gone = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}

/// A file that shrank in place, then a new file renamed over the path (larger than what was read): each read gives
/// that file's points only.
#[test]
fn a_shrunk_or_replaced_file_is_read_from_its_start() {
    let path = scratch("rotate");
    let first = long_log();
    append(&path, &first);
    let mut tail = LiveTail::default();
    assert_eq!(read(&mut tail, &path), Some(whole(&first)));

    let shrunk = lines(S + 2_000_000, 5_000, 6, &JUMPED, 0);
    assert!(std::fs::write(&path, &shrunk).is_ok(), "truncate and write");
    assert_eq!(read(&mut tail, &path), Some(whole(shrunk.as_bytes())));

    let new = lines(S + 3_000_000, 4_000, 200, &STEADY, DRIFT);
    assert!(
        new.len() > shrunk.len(),
        "the new file is past the old offset"
    );
    let next = path.with_extension("new");
    assert!(std::fs::write(&next, &new).is_ok(), "write the new file");
    assert!(std::fs::rename(&next, &path).is_ok(), "rename over");
    assert_eq!(read(&mut tail, &path), Some(whole(new.as_bytes())));

    assert!(std::fs::remove_file(&path).is_ok(), "remove");
    assert!(tail.read(&path).is_err(), "a missing file is an error");
    append(&path, shrunk.as_bytes());
    assert_eq!(read(&mut tail, &path), Some(whole(shrunk.as_bytes())));
    let _gone = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}
