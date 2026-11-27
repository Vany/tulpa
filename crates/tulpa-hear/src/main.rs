//! `tulpa-hear --probe-mic <seconds>` and `tulpa-hear --probe-tap <seconds>`: run the macOS mic capture ("me") or
//! the system-audio tap ("them") for that long and print counts only — the device, its native rate and channels, the
//! 16 kHz samples produced, how many 100 ms blocks fell in each 10 dB RMS band, the switches seen and the warnings
//! (and for the mic whether `EARS_MIC_AEC` put it through Apple's voice processing). It never writes or prints audio.
//!
//! Exit 0 after the probe. Exit 1 when the capture fails: for the mic a pinned `EARS_MIC` that is missing, a bad
//! `EARS_MIC_AEC`, or `EARS_MIC_AEC=1` with a `VoiceProcessingIO` unit that cannot be opened (never the plain path
//! instead); for the tap a macOS older than 14.6 or a `SystemVersion.plist` that cannot be read. Exit 2 on a usage
//! error or on a platform other than macOS.
//!
//! `tulpa-hear --pcm-out`: the mic capture exactly as `--probe-mic` runs it (`EARS_MIC*`, the same device choice and
//! following of the default input), written to stdout as raw 16 kHz mono s16le, flushed per block, and nothing else
//! ever on stdout. stderr gets one line per device, switch, missing input and warning; the first, once a device
//! opens, names it, its native rate and channels and the output format. With no input device it keeps running and
//! writes nothing (no silence fill). Exit 0 when stdout's reader goes away (`EPIPE`); exit 1 on another write error
//! or when the capture fails as `--probe-mic`'s does. SIGTERM and SIGINT keep their default action (the process
//! ends). Exit 2 on a platform other than macOS.
//!
//! `tulpa-hear --check`: the preflight (the `check` module): the macOS version (14.6 or later), the model and VAD
//! files in `EARS_MODELS` (exist, readable, not empty) and whether this build has the `whisper` feature, one
//! `<name>: ok` / `<name>: FAIL <why>` line each on stdout. It opens no device and writes no file. Exit 0 when all
//! pass, else 1.
//!
//! `tulpa-hear --ears`: the ears' run mode, which the tray starts. Both sides in one process — "me" from the mic,
//! "them" from the tap — each on its own thread with its own segmenter, sharing one whisper model (loaded once at
//! start from `EARS_MODELS`, default `~/.cache/calltx`), in memory up to the ring (below). Each
//! phrase becomes one `TULPA-EARS-LINE` on stdout, byte-compatible with the capture script's (the line assembler,
//! with the turn hold up to `EARS_HOLD_MAX_MS` of held speech, default 300000), and one line of
//! `<EARS_STATE>/heard.log` (0600), plus one there for every unread piece of 2.5 s or more, naming the rule
//! (`voice-unread:<rule> nt=.. m=.. maxrun=..`); a piece with no token at all (no speech found) is
//! `<no speech N.Ns>`, not `<voice N.Ns>`, its own line `no-speech nt=.. m=.. maxrun=..`, its phrase's outcome
//! `emitted:no-speech`. `EARS_DEAF_S` (default 300)
//! seconds of consecutive pieces on one side unread as `markers-only` or `empty-text` give one
//! `<deaf-suspect <rule> <N>s>` line; a piece read as text re-arms it. As the script's segmenter did, each side
//! rewrites `<EARS_STATE>/me/.alive` / `them/.alive` every 5 s
//! of audio (the tray's self-check watches those directories; the tap, which hears nothing while nothing plays, also
//! every 5 s while its device is open), and each recognized piece parks in the raw ring
//! (`EARS_RING_DIR`, default `/tmp/tulpa-ears-ring-<uid>`; `EARS_RING_MAX_MB`, default 8: its WAV, its segmenter
//! JSON and `.whisper.json`, all 0600, the oldest dropped past the bound). `EARS_LANG` (default `ru`),
//! `EARS_PROMPT` (whisper's initial prompt, none when unset; its adjacent word pairs are the echo filter),
//! `EARS_STATE` (the tray's `me/.close`, heard.log, the heartbeats), and the mic's `EARS_MIC*` settings apply.
//! `TULPA_DEBUG=1` (the tray's top-level `debug = true`; off by default) adds one `TULPA-EARS-DEBUG` line on stdout
//! per recognized piece: whisper's raw text, each pass's language and the filter's verdict. Exit 1
//! on a missing model file (named), a model that will not load, a bad `EARS_HOLD_MAX_MS`, `EARS_DEAF_S`,
//! `TULPA_DEBUG` or `EARS_RING_MAX_MB`, a ring dir that is not a plain directory of ours, or a capture that stops with an error;
//! exit 2 when built without the `whisper` feature. In a
//! build with the feature `--ears` first runs `--check`'s checks and refuses with exit 1, the failing lines on
//! stderr, before it opens any device (off macOS the `macos` check always fails); without the feature it exits 2
//! before checking anything.
//!
//! `--ears` stops on SIGTERM, SIGINT or SIGHUP, and on the end of its stdin when `TULPA_EARS_STDIN_STOP` is set
//! (the tray holds that pipe; its close, or the tray's death, is the stop): the captures end, each side's phrase in
//! progress and a turn held at a 60 s cut are written, and it exits 0; a side that does not end is given up after
//! 5 s. SIGKILL still writes nothing.
//!
//! `tulpa-hear --shadow [dir]`: the rehearsal beside the live path — exactly `--ears`' pipeline and environment,
//! but nothing on stdout, nothing written under `EARS_STATE` (its `me/.close` is only looked at, never removed, and
//! no heartbeat) and no ring.
//! Each line goes to `<dir>/lines.log` as `<emit_ms>\t<line>`, between a `SHADOW start <pid> <version>` and a
//! `SHADOW stop <reason>` record; heard.log and mic-switch.log go to `<dir>` too (default
//! `$HOME/Library/Logs/tulpa/shadow`, made `0700`, files `0600`, each rotated to `.1` past 20 MB). SIGTERM, SIGINT
//! and SIGHUP finish the phrase in progress if that takes under 1.5 s, else drop it, write the stop record and exit
//! 0. Exit codes otherwise as `--ears` (a failure after the start record is `SHADOW stop error`).
//!
//! `tulpa-hear --compare <live ears.log> <shadow lines.log> [--flow <flow.log>] [--pair emit|span]
//! [--emit-tol-ms <ms>]`: the rehearsal's verdict (the `compare` module), counts only, never a phrase's text, on
//! stdout. Phrases pair by emit time within `--emit-tol-ms` (default 4000) or, with `--pair span`, by span overlap.
//! Exit 0 when every switch criterion that applies passes, 1 when one fails, 2 when a log cannot be read.
//!
//! `tulpa-hear --replay <ring-dir> [--shadow <dir>] [--since <unix seconds>]`: the live capture's recorded pieces
//! offline (the `replay` module). Each `me-<piece>.wav` / `them-<piece>.wav` in the ring (16 kHz mono s16le WAV, one
//! utterance as the live segmenter cut it; others refused on stderr; `.json`s ignored; another side skipped), oldest
//! mtime first, goes as one utterance through `--shadow`'s recognizer and assembler (no segmenter), with `t1` its mtime
//! and `t0` = `t1` − its length, into the dir exactly what `--shadow` writes there (default `<the --shadow
//! default>/replay`); a line's emit time is its `t1` plus the recognizer's time on it, so `--compare --pair emit` pairs
//! it with the live logs of the same minutes. With `--since`, a piece whose mtime is before that moment is skipped
//! without being opened and counted as `before --since` (an equal mtime is kept), so that only what was recorded after
//! a capture fix is compared; the options after the ring dir come in any order, each at most once. The ring is only
//! read: nothing there is written, moved or removed, and a piece gone mid-run is counted. At the end, counts only on
//! stdout. The `EARS_*` settings apply as for `--shadow`; a lower priority is the caller's (`taskpolicy`). Exit 0 after
//! the run (also when it was stopped by SIGTERM, SIGINT or SIGHUP, which end it between pieces with the stop record), 1
//! when it cannot start or `lines.log` cannot be written, 2 on a usage error or without the `whisper` feature.
//!
//! `tulpa-hear --watchdog --live <live log> --pid <shadow pid> [--rise-pct 30] [--window-s 120] [--baseline-s 600]`:
//! for a live shadow run (the `watchdog` module). Every 10 s it reads the live log with `--compare`'s reader and
//! judges the live emit latency — the change in (emit − `t1`) within a set of phrases, since the live clock drifts —
//! as its p90 over the last `--window-s` against the same over the `--baseline-s` before it started. Past baseline ×
//! (1 + rise/100) with at least 5 phrases in the window, it sends SIGTERM to `--pid`, but only once `ps -p <pid> -o
//! command=` shows `tulpa-hear` and `--shadow`, prints one line of counts and exits 0. Too few phrases for a
//! baseline: it says so and does not judge until one forms. Exit 0 also when the pid is gone; 1 when the pid is not
//! a shadow's (nothing sent) or `ps` cannot run; 2 on a usage error. Counts only, never text.

use std::io::{self, Write};
use std::process::ExitCode;

/// What the command line asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    /// `--probe-mic` / `--probe-tap` for that many seconds.
    Probe(Probe, u64),
    /// `--pcm-out`.
    PcmOut,
    /// `--check`.
    Check,
    /// `--ears`.
    Ears,
}

/// Which source to probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// The microphone.
    Mic,
    /// The system-audio tap.
    Tap,
}

fn usage() -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "usage: tulpa-hear --probe-mic <seconds 1..3600>\n       tulpa-hear --probe-tap <seconds 1..3600>\n       tulpa-hear --pcm-out\n       tulpa-hear --check\n       tulpa-hear --ears\n       tulpa-hear --shadow [dir]\n       tulpa-hear --replay <ring-dir> [--shadow <dir>] [--since <unix seconds>]\n       tulpa-hear --compare <live ears.log> <shadow lines.log> [--flow <flow.log>] [--pair emit|span] [--emit-tol-ms <ms>]\n       tulpa-hear --watchdog --live <live log> --pid <shadow pid> [--rise-pct 30] [--window-s 120] [--baseline-s 600]"
    );
    ExitCode::from(2)
}

/// The mode, or `None` for a usage error.
fn parse(args: &[String]) -> Option<Cmd> {
    if let [flag] = args {
        return match flag.as_str() {
            "--ears" => Some(Cmd::Ears),
            "--check" => Some(Cmd::Check),
            "--pcm-out" => Some(Cmd::PcmOut),
            _ => None,
        };
    }
    let [flag, n] = args else {
        return None;
    };
    let probe = match flag.as_str() {
        "--probe-mic" => Probe::Mic,
        "--probe-tap" => Probe::Tap,
        _ => return None,
    };
    let secs = n.parse::<u64>().ok().filter(|s| (1..=3600).contains(s))?;
    Some(Cmd::Probe(probe, secs))
}

/// `--ears` (the live path, lines on stdout) or `--shadow [dir]` (the rehearsal beside it, lines to the dir).
#[derive(Debug)]
enum Mode {
    Ears,
    Shadow(
        #[cfg_attr(
            not(all(feature = "whisper", target_os = "macos")),
            expect(dead_code, reason = "the dir is used only where the pipeline runs")
        )]
        Option<std::path::PathBuf>,
    ),
}

impl Mode {
    const fn flag(&self) -> &'static str {
        match self {
            Self::Ears => "--ears",
            Self::Shadow(_) => "--shadow",
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--shadow"] => return listen(&Mode::Shadow(None)),
        ["--shadow", dir] if !dir.starts_with('-') => {
            return listen(&Mode::Shadow(Some(dir.into())));
        }
        ["--replay", ring, rest @ ..] if !ring.starts_with('-') => {
            return match replay_opts(rest) {
                Some((dir, since_ms)) => replay(ring, dir, since_ms),
                None => usage(),
            };
        }
        ["--watchdog", rest @ ..] => {
            return match watchdog_opts(rest) {
                Some((live, pid, settings)) => watchdog(live, pid, settings),
                None => usage(),
            };
        }
        ["--compare", live, shadow, rest @ ..] => {
            return match compare_opts(rest) {
                Some((flow, how)) => compare(live, shadow, flow, how),
                None => usage(),
            };
        }
        _ => {}
    }
    match parse(&args) {
        Some(Cmd::Probe(what, secs)) => probe(what, secs),
        Some(Cmd::PcmOut) => pcm_out(),
        Some(Cmd::Check) => check(),
        Some(Cmd::Ears) => listen(&Mode::Ears),
        None => usage(),
    }
}

#[cfg(not(target_os = "macos"))]
fn pcm_out() -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "tulpa-hear: the mic and the tap are macOS-only"
    );
    ExitCode::from(2)
}

/// `--pcm-out`: the mic to stdout until the reader goes away or the capture fails.
#[cfg(target_os = "macos")]
fn pcm_out() -> ExitCode {
    use std::sync::{Arc, mpsc};

    use tulpa_stop::Stop;

    use tulpa_hear::mic::{self, Settings};
    use tulpa_hear::pcm_out::{Outcome, pump};

    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(Stop::new());
    let worker = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || mic::run(&settings, &tx, &stop).map_err(|e| e.to_string()))
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let pumped = pump(&rx, &mut out, &mut io::stderr());
    stop.set();
    drop(rx);
    let result = worker.join();
    match pumped {
        Ok(Outcome::Closed) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("stdout: {e}")),
        Ok(_) => match result {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(e)) => fail(&e),
            Err(_) => fail(&"the capture thread panicked"),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn probe(_what: Probe, _secs: u64) -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "tulpa-hear: the mic and the tap are macOS-only"
    );
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn probe(what: Probe, secs: u64) -> ExitCode {
    use std::sync::{Arc, mpsc};

    use std::time::{Duration, Instant};
    use tulpa_stop::Stop;

    use tulpa_hear::mic::{self, Settings};
    use tulpa_hear::tap;

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(Stop::new());
    let (mut tally, worker) = match what {
        Probe::Mic => {
            let settings = match Settings::from_env() {
                Ok(s) => s,
                Err(e) => return fail(&e),
            };
            let tally = Tally {
                aec: Some(settings.aec.as_str()),
                ..Tally::default()
            };
            let stop = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                mic::run(&settings, &tx, &stop).map_err(|e| e.to_string())
            });
            (tally, worker)
        }
        Probe::Tap => {
            let stop = Arc::clone(&stop);
            let worker =
                std::thread::spawn(move || tap::run(&tx, &stop).map_err(|e| e.to_string()));
            (Tally::default(), worker)
        }
    };
    let deadline = Instant::now().checked_add(Duration::from_secs(secs));
    while let Some(left) = deadline.and_then(|d| d.checked_duration_since(Instant::now())) {
        match rx.recv_timeout(left) {
            Ok(ev) => tally.add(ev),
            Err(_) => break,
        }
    }
    stop.set();
    let result = worker.join();
    while let Ok(ev) = rx.try_recv() {
        tally.add(ev);
    }
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return fail(&e),
        Err(_) => return fail(&"the capture thread panicked"),
    }
    let _said: io::Result<()> = tulpa_hear::print(&tally.report());
    ExitCode::SUCCESS
}

/// `--watchdog`'s options, each at most once: `--live <file>` and `--pid <2..>` required; `--rise-pct <0..=1000>`,
/// `--window-s <1..=86400>`, `--baseline-s <1..=86400>`; `None` for anything else.
fn watchdog_opts<'a>(
    mut rest: &[&'a str],
) -> Option<(&'a str, u32, tulpa_hear::watchdog::Settings)> {
    use tulpa_hear::watchdog::{DEFAULT_BASELINE_S, DEFAULT_RISE_PCT, DEFAULT_WINDOW_S, Settings};
    let (mut live, mut pid, mut rise, mut window, mut baseline) = (None, None, None, None, None);
    let num = |v: &str, lo: u64, hi: u64| v.parse::<u64>().ok().filter(|n| (lo..=hi).contains(n));
    while let [opt, value, more @ ..] = rest {
        let slot_free = match *opt {
            "--live" if !value.starts_with('-') => live.replace(*value).is_none(),
            "--pid" => pid
                .replace(u32::try_from(num(value, 2, u64::from(u32::MAX))?).ok()?)
                .is_none(),
            "--rise-pct" => rise.replace(num(value, 0, 1000)?).is_none(),
            "--window-s" => window.replace(num(value, 1, 86_400)?).is_none(),
            "--baseline-s" => baseline.replace(num(value, 1, 86_400)?).is_none(),
            _ => return None,
        };
        if !slot_free {
            return None;
        }
        rest = more;
    }
    if !rest.is_empty() {
        return None;
    }
    Some((
        live?,
        pid?,
        Settings::new(
            rise.unwrap_or(DEFAULT_RISE_PCT),
            window.unwrap_or(DEFAULT_WINDOW_S),
            baseline.unwrap_or(DEFAULT_BASELINE_S),
        ),
    ))
}

/// `--watchdog`: judges the live log every tick until a rise (SIGTERM to the verified shadow pid, one line, exit 0)
/// or the pid is gone (exit 0); exit 1 when the pid is not a shadow's or `ps` cannot run.
fn watchdog(live: &str, pid: u32, settings: tulpa_hear::watchdog::Settings) -> ExitCode {
    use tulpa_hear::now_ms as now;
    use tulpa_hear::watchdog::{self, PidIs, TICK, Verdict, Watch};

    let say = |line: &str| {
        let _said: io::Result<()> = tulpa_hear::print(&format!("watchdog: {line}\n"));
    };
    let refuse = |what: &str| {
        let _said: io::Result<()> = writeln!(io::stderr(), "tulpa-hear: --watchdog: {what}");
        ExitCode::from(1)
    };
    let mut watch = Watch::new(settings, now());
    let mut tail = watchdog::LiveTail::default();
    let (mut said_none, mut said_base, mut said_unread) = (false, false, false);
    loop {
        match watchdog::check_pid(pid) {
            Ok(PidIs::Shadow) => {}
            Ok(PidIs::Gone) => {
                say(&format!("pid {pid} gone"));
                return ExitCode::SUCCESS;
            }
            Ok(_) => {
                return refuse(&format!(
                    "pid {pid} is not a tulpa-hear --shadow: nothing sent"
                ));
            }
            Err(e) => return refuse(&format!("ps: {e}")),
        }
        let points = match tail.read(std::path::Path::new(live)) {
            Ok(p) => {
                said_unread = false;
                p
            }
            Err(e) => {
                if !said_unread {
                    said_unread = true;
                    let _said: io::Result<()> =
                        writeln!(io::stderr(), "tulpa-hear: --watchdog: {live}: {e}");
                }
                &[]
            }
        };
        match watch.judge(points, now()) {
            Verdict::NoBaseline { n } if !said_none => {
                said_none = true;
                say(&format!(
                    "baseline: {n} phrases, {} needed: not judging until it forms",
                    watchdog::MIN_PHRASES
                ));
            }
            Verdict::Rise { baseline, window } => {
                return match watchdog::terminate(pid) {
                    Ok(PidIs::Shadow) => {
                        say(&format!(
                            "SIGTERM {pid}: window p90 {} ms (n {}) past baseline p90 {} ms (n {}) + {}%",
                            window.p90_ms, window.n, baseline.p90_ms, baseline.n, settings.rise_pct
                        ));
                        ExitCode::SUCCESS
                    }
                    Ok(PidIs::Gone) => {
                        say(&format!("pid {pid} gone"));
                        ExitCode::SUCCESS
                    }
                    Ok(_) => refuse(&format!(
                        "pid {pid} is not a tulpa-hear --shadow: nothing sent"
                    )),
                    Err(e) => refuse(&e),
                };
            }
            Verdict::Within { baseline, .. } if !said_base => {
                said_base = true;
                say(&format!(
                    "baseline: p90 {} ms over {} phrases",
                    baseline.p90_ms, baseline.n
                ));
            }
            _ => {}
        }
        // A poll: std has no portable event for a file's growth (nor for another process's exit), and the tick
        // reads only what was appended since the last one.
        std::thread::sleep(TICK);
    }
}

/// `--compare`'s options after the two logs, each at most once: `--flow <file>`, `--pair emit|span`,
/// `--emit-tol-ms <ms>` (emit pairing only); `None` for anything else.
fn compare_opts<'a>(
    mut rest: &[&'a str],
) -> Option<(Option<&'a str>, tulpa_hear::compare::Pairing)> {
    use tulpa_hear::compare::{DEFAULT_EMIT_TOL_MS, Pairing};
    let (mut flow, mut span, mut tol) = (None, None, None);
    while let [opt, value, more @ ..] = rest {
        let slot_free = match *opt {
            "--flow" => flow.replace(*value).is_none(),
            "--pair" => span
                .replace(match *value {
                    "emit" => false,
                    "span" => true,
                    _ => return None,
                })
                .is_none(),
            "--emit-tol-ms" => tol.replace(value.parse::<u64>().ok()?).is_none(),
            _ => return None,
        };
        if !slot_free {
            return None;
        }
        rest = more;
    }
    if !rest.is_empty() {
        return None;
    }
    match (span.unwrap_or(false), tol) {
        (true, Some(_)) => None,
        (true, None) => Some((flow, Pairing::Span)),
        (false, tol) => Some((
            flow,
            Pairing::Emit {
                tol_ms: tol.unwrap_or(DEFAULT_EMIT_TOL_MS),
            },
        )),
    }
}

/// `--replay`'s options after the ring dir, in any order, each at most once: `--shadow <dir>` and `--since <unix
/// seconds>` (digits only; returned as epoch ms). `None` is a usage error.
fn replay_opts<'a>(mut rest: &[&'a str]) -> Option<(Option<&'a str>, Option<u64>)> {
    let (mut dir, mut since_ms) = (None, None);
    while let [opt, value, more @ ..] = rest {
        let slot_free = match *opt {
            "--shadow" if !value.starts_with('-') => dir.replace(*value).is_none(),
            "--since" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => since_ms
                .replace(value.parse::<u64>().ok()?.checked_mul(1000)?)
                .is_none(),
            _ => return None,
        };
        if !slot_free {
            return None;
        }
        rest = more;
    }
    rest.is_empty().then_some((dir, since_ms))
}

/// `--compare`: the report on stdout; exit 0 when every criterion that applies passes, 1 when one fails, 2 when a
/// log cannot be read.
fn compare(
    live: &str,
    shadow: &str,
    flow: Option<&str>,
    how: tulpa_hear::compare::Pairing,
) -> ExitCode {
    let read = |p: &str| {
        std::fs::read(p)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| {
                let _said: io::Result<()> =
                    writeln!(io::stderr(), "tulpa-hear: --compare: {p}: {e}");
            })
    };
    let (Ok(live), Ok(shadow)) = (read(live), read(shadow)) else {
        return ExitCode::from(2);
    };
    let Ok(flow) = flow.map(read).transpose() else {
        return ExitCode::from(2);
    };
    let report = tulpa_hear::compare::compare(&live, &shadow, flow.as_deref(), how);
    let _said: io::Result<()> = tulpa_hear::print(&report.render());
    if report.pass() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(not(feature = "whisper"))]
fn replay(_ring: &str, _dir: Option<&str>, _since_ms: Option<u64>) -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "tulpa-hear: --replay: built without whisper (the `whisper` feature)"
    );
    ExitCode::from(2)
}

/// `--replay`: the ring's pieces through the shadow's recognizer and assembler into the replay dir; the counts on
/// stdout. Exit 0 after the run, 1 when it cannot start (the ring is not a readable dir, the models, the dir) or
/// `lines.log` cannot be written.
#[cfg(feature = "whisper")]
fn replay(ring: &str, dir: Option<&str>, since_ms: Option<u64>) -> ExitCode {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use tulpa_hear::assemble::Assembler;
    use tulpa_hear::replay;
    use tulpa_hear::shadow::{self, Shadow, Signals};
    use tulpa_hear::whisper::{WhisperRecognizer, models_dir};

    let began = Instant::now();
    let die = |e: &dyn std::fmt::Display| {
        let _said: io::Result<()> = writeln!(io::stderr(), "tulpa-hear: --replay: {e}");
        ExitCode::from(1)
    };
    // Before any thread starts: only the watch thread takes them, and the stop record names the signal.
    let signals = match Signals::block() {
        Ok(s) => s,
        Err(e) => return die(&format!("signals not blocked: {e}")),
    };
    let var = |k: &str| std::env::var(k).ok();
    let ring = Path::new(ring);
    if let Err(e) = std::fs::read_dir(ring) {
        return die(&format!("{}: {e}", ring.display()));
    }
    let Some(dir) = dir
        .map(PathBuf::from)
        .or_else(|| shadow::default_dir(var("HOME").as_deref()).map(|d| d.join(replay::SUBDIR)))
    else {
        return die(&"no --shadow dir given and HOME is not set");
    };
    let Some(models) = models_dir(var("EARS_MODELS").as_deref(), var("HOME").as_deref()) else {
        return die(&"neither EARS_MODELS nor HOME is set");
    };
    let prompt = var("EARS_PROMPT");
    let lang = var("EARS_LANG")
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "ru".to_owned());
    let recognizer = match WhisperRecognizer::load(&models, prompt.clone()) {
        Ok(r) => Mutex::new(r),
        Err(e) => return die(&e),
    };
    let sink = match Shadow::open(&dir, shadow::BOUND) {
        Ok(s) => s,
        Err(e) => return die(&format!("{}: {e}", dir.display())),
    };
    let mut assembler = match Assembler::from_env(&dir, prompt.as_deref(), &var) {
        Ok(a) => a.with_log_bound(shadow::BOUND),
        Err(e) => return die(&e),
    };
    let (caught, _watch) = signals.spawn(Arc::new(tulpa_stop::Stop::new()));
    let stopped = || caught.get().map(|c| c.name);
    match replay::run(
        ring,
        since_ms,
        &recognizer,
        &lang,
        &mut assembler,
        &sink,
        &stopped,
    ) {
        Ok(counts) => {
            let _said: io::Result<()> = tulpa_hear::print(&counts.render(began.elapsed()));
            ExitCode::from(counts.exit_code(stopped().is_some()))
        }
        Err(e) => die(&e),
    }
}

/// The preflight's checks on this build, this machine and the environment's models directory.
fn preflight() -> Vec<tulpa_hear::check::Check> {
    use std::path::Path;

    use tulpa_hear::check::{self, models_dir};
    use tulpa_hear::osver::SYSTEM_VERSION_PLIST;

    let var = |k: &str| std::env::var(k).ok();
    let models = models_dir(var("EARS_MODELS").as_deref(), var("HOME").as_deref());
    check::run(
        cfg!(target_os = "macos"),
        Path::new(SYSTEM_VERSION_PLIST),
        models.as_deref(),
        cfg!(feature = "whisper"),
    )
}

/// `--check`: every check's line on stdout; exit 0 only when all pass.
fn check() -> ExitCode {
    let checks = preflight();
    for c in &checks {
        let _said: io::Result<()> = tulpa_hear::print(&format!("{c}\n"));
    }
    if checks.iter().all(tulpa_hear::check::Check::ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `--ears`' and `--shadow`'s gate: `Some(exit 1)` with the failing checks' lines on stderr, before any device is
/// opened.
#[cfg(feature = "whisper")]
fn refused(flag: &str) -> Option<ExitCode> {
    let failed: Vec<_> = preflight().into_iter().filter(|c| !c.ok()).collect();
    if failed.is_empty() {
        return None;
    }
    let mut err = io::stderr();
    for c in &failed {
        let _said: io::Result<()> = writeln!(err, "tulpa-hear: {flag}: {c}");
    }
    Some(ExitCode::from(1))
}

#[cfg(not(feature = "whisper"))]
fn listen(mode: &Mode) -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "tulpa-hear: {}: built without whisper (the `whisper` feature)",
        mode.flag()
    );
    ExitCode::from(2)
}

#[cfg(all(feature = "whisper", not(target_os = "macos")))]
fn listen(mode: &Mode) -> ExitCode {
    if let Some(code) = refused(mode.flag()) {
        return code; // always: the macos check fails here
    }
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "tulpa-hear: {}: the mic and the tap are macOS-only",
        mode.flag()
    );
    ExitCode::from(2)
}

#[cfg(all(feature = "whisper", target_os = "macos"))]
fn listen(mode: &Mode) -> ExitCode {
    use std::sync::{Arc, Mutex};

    use tulpa_hear::ears::{DEBUG_ENV, Ears, Stdout, debug_switch};
    use tulpa_hear::live;
    use tulpa_hear::mic::Settings;
    use tulpa_hear::now_ms;
    use tulpa_hear::shadow::{self, Signals};
    use tulpa_hear::whisper::{WhisperRecognizer, models_dir};
    use tulpa_stop::Stop;

    if let Some(code) = refused(mode.flag()) {
        return code;
    }
    // Before any thread starts, so that only the watch thread takes them. Both paths: a termination signal is a
    // stop that writes what is in progress (`live`, `shadow`), not the default death that writes nothing.
    let signals = match Signals::block() {
        Ok(s) => s,
        Err(e) => return fail(&format!("signals not blocked: {e}")),
    };
    let var = |k: &str| std::env::var(k).ok();
    let mut settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    // The tray's marker is in EARS_STATE for both; the shadow writes nothing there (not even mic-switch.log).
    let close = settings.state_dir.join("me").join(".close");
    let shadow = match mode {
        Mode::Ears => None,
        Mode::Shadow(dir) => match open_shadow(dir.as_deref()) {
            Ok((s, dir)) => {
                settings.state_dir.clone_from(&dir);
                Some((s, dir))
            }
            Err(code) => return code,
        },
    };
    // A setup failure after the start record is the shadow's `stop error`.
    let bail = |e: &dyn std::fmt::Display| {
        if let Some((s, _)) = &shadow {
            let _logged: std::io::Result<()> = s.stop(now_ms(), "error");
        }
        fail(e)
    };
    let Some(dir) = models_dir(var("EARS_MODELS").as_deref(), var("HOME").as_deref()) else {
        return bail(&"neither EARS_MODELS nor HOME is set");
    };
    let prompt = var("EARS_PROMPT");
    let shadow_dir = shadow.as_ref().map(|(_, dir)| dir.as_path());
    let (mut assembler, ring) =
        match assembler_and_ring(shadow_dir, &settings.state_dir, prompt.as_deref()) {
            Ok(both) => both,
            Err(e) => return bail(&e),
        };
    let recognizer = match WhisperRecognizer::load(&dir, prompt) {
        Ok(r) => Arc::new(Mutex::new(r)),
        Err(e) => return bail(&e),
    };
    let lang = var("EARS_LANG")
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "ru".to_owned());
    // The debug lines are `--ears`' alone: the shadow says nothing on stdout.
    let debug = match debug_switch(std::env::var_os(DEBUG_ENV).as_deref()) {
        Ok(on) => on && shadow.is_none(),
        Err(e) => return bail(&e),
    };
    let ears = Ears::new(
        recognizer,
        lang,
        close,
        close_mode(shadow.is_some()),
        now_ms,
    )
    .with_debug(debug);
    let ears = match ring {
        Some(ring) => ears.with_files(settings.state_dir.clone(), ring),
        None => ears,
    };
    let capture = Arc::new(capture(settings));
    let stop = Arc::new(Stop::new());
    let (caught, _watching) = signals.spawn(Arc::clone(&stop));
    let Some((mut sink, _)) = shadow else {
        // The live path's one way to stop (`live`): a termination signal, or the end of the stdin when the tray
        // says it holds that pipe. Either writes the held turn and the phrase in progress, then the run ends.
        if std::env::var_os(live::STDIN_STOP_ENV).is_some()
            && let Err(e) = live::watch_eof(io::stdin(), Arc::clone(&caught), Arc::clone(&stop))
        {
            return fail(&format!("stdin not watched: {e}"));
        }
        let outcome = live::run(
            &ears,
            &capture,
            &mut assembler,
            &mut Stdout::default(),
            &stop,
            &caught,
            live::GRACE,
        );
        return ExitCode::from(outcome.exit_code());
    };
    let reason = shadow::run(
        &ears,
        &capture,
        &mut assembler,
        &mut sink,
        &stop,
        Some(&caught),
    );
    ExitCode::from(shadow::exit_code(reason))
}

/// heard.log's assembler — `state`'s for the live path, the shadow dir's (bounded) for the shadow — and the live
/// path's raw ring (the shadow keeps none), both from the environment, before any device opens.
#[cfg(all(feature = "whisper", target_os = "macos"))]
fn assembler_and_ring(
    shadow_dir: Option<&std::path::Path>,
    state: &std::path::Path,
    prompt: Option<&str>,
) -> Result<
    (
        tulpa_hear::assemble::Assembler,
        Option<tulpa_hear::ring::Ring>,
    ),
    String,
> {
    use tulpa_hear::assemble::Assembler;
    use tulpa_hear::ring::Ring;
    use tulpa_hear::shadow::BOUND;

    let var = |k: &str| std::env::var(k).ok();
    let ring = shadow_dir
        .is_none()
        .then(|| Ring::from_env(&var))
        .transpose()?;
    let a = Assembler::from_env(shadow_dir.unwrap_or(state), prompt, &var)?;
    Ok((
        if shadow_dir.is_some() {
            a.with_log_bound(BOUND)
        } else {
            a
        },
        ring,
    ))
}

/// The shadow dir (given, or the default under `HOME`), opened, with its start record written.
#[cfg(all(feature = "whisper", target_os = "macos"))]
fn open_shadow(
    dir: Option<&std::path::Path>,
) -> Result<(tulpa_hear::shadow::Shadow, std::path::PathBuf), ExitCode> {
    use tulpa_hear::shadow::{self, Shadow};

    let home = std::env::var("HOME").ok();
    let Some(dir) = dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| shadow::default_dir(home.as_deref()))
    else {
        return Err(fail(&"--shadow: no dir given and HOME is not set"));
    };
    let opened = Shadow::open(&dir, shadow::BOUND).and_then(|s| {
        s.start(
            tulpa_hear::now_ms(),
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
        )
        .map(|()| s)
    });
    match opened {
        Ok(s) => Ok((s, dir)),
        Err(e) => Err(fail(&format!("--shadow: {}: {e}", dir.display()))),
    }
}

/// The live path takes the tray's `.close`; the shadow only watches it.
#[cfg(all(feature = "whisper", target_os = "macos"))]
const fn close_mode(shadow: bool) -> tulpa_hear::hear::CloseMode {
    use tulpa_hear::hear::CloseMode;
    if shadow {
        CloseMode::Watch
    } else {
        CloseMode::Take
    }
}

/// Both sides' captures: the mic for "me", the system-audio tap for "them".
#[cfg(all(feature = "whisper", target_os = "macos"))]
fn capture(settings: tulpa_hear::mic::Settings) -> impl tulpa_hear::ears::Capture {
    use std::sync::mpsc::Sender;

    use tulpa_stop::Stop;

    use tulpa_hear::hear::Side;
    use tulpa_hear::{mic, tap};

    move |side: Side, tx: &Sender<mic::Event>, stop: &Stop| match side {
        Side::Me => mic::run(&settings, tx, stop).map_err(|e| e.to_string()),
        _ => tap::run(tx, stop).map_err(|e| e.to_string()),
    }
}

#[cfg(target_os = "macos")]
fn fail(e: &dyn std::fmt::Display) -> ExitCode {
    let _said: io::Result<()> = writeln!(io::stderr(), "tulpa-hear: {e}");
    ExitCode::from(1)
}

/// Samples per RMS block: 100 ms at 16 kHz.
#[cfg(target_os = "macos")]
const BLOCK: usize = 1600;
/// RMS bands, 10 dB each from 0 dBFS down to -90; then quieter, then digital silence.
#[cfg(target_os = "macos")]
const BANDS: usize = 9;

#[cfg(target_os = "macos")]
#[derive(Default)]
struct Tally {
    /// The mic's `EARS_MIC_AEC` setting; `None` for the tap.
    aec: Option<&'static str>,
    device: Option<(String, u32, u16)>,
    samples: u64,
    switches: u64,
    no_input: u64,
    warnings: u64,
    block: Vec<i16>,
    /// `BANDS` 10 dB bands, then below -90 dBFS, then all-zero blocks.
    buckets: [u64; BANDS + 2],
}

#[cfg(target_os = "macos")]
impl Tally {
    fn add(&mut self, ev: tulpa_hear::mic::Event) {
        use tulpa_hear::mic::Event;
        match ev {
            Event::Device {
                name,
                rate,
                channels,
            } => self.device = Some((name, rate, channels)),
            Event::Switched { .. } => self.switches = self.switches.saturating_add(1),
            Event::NoInput => self.no_input = self.no_input.saturating_add(1),
            Event::StreamError(_) | Event::Warning(_) => {
                self.warnings = self.warnings.saturating_add(1);
            }
            Event::Audio(chunk) => {
                self.samples = self.samples.saturating_add(chunk.len() as u64);
                for s in chunk {
                    self.block.push(s);
                    if self.block.len() == BLOCK {
                        self.bucket();
                        self.block.clear();
                    }
                }
            }
            _ => {}
        }
    }

    fn bucket(&mut self) {
        let sum: f64 = self.block.iter().map(|&s| f64::from(s).powi(2)).sum();
        #[expect(clippy::cast_precision_loss, reason = "a block is 1600 samples")]
        let rms = (sum / self.block.len() as f64).sqrt() / 32768.0;
        let i = if rms <= 0.0 {
            BANDS + 1
        } else {
            let db = 20.0 * rms.log10();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to 0..=9 (BANDS) before the cast"
            )]
            let band = (-db / 10.0).floor().clamp(0.0, 9.0) as usize;
            band
        };
        if let Some(b) = self.buckets.get_mut(i) {
            *b = b.saturating_add(1);
        }
    }

    fn report(&self) -> String {
        use std::fmt::Write as _;
        let mut r = String::new();
        let (name, rate, channels) = self
            .device
            .as_ref()
            .map_or(("none", 0, 0), |(n, r, c)| (n.as_str(), *r, *c));
        let _w = writeln!(r, "device: {name}");
        let _w = writeln!(r, "device_rate: {rate}");
        let _w = writeln!(r, "device_channels: {channels}");
        let _w = writeln!(r, "output_samples: {}", self.samples);
        let _w = writeln!(r, "rms_blocks_100ms_dbfs:");
        for (i, n) in self.buckets.iter().enumerate() {
            let label = match i {
                i if i < BANDS => format!(
                    "-{}..-{}",
                    i.saturating_mul(10).saturating_add(10),
                    i.saturating_mul(10)
                ),
                BANDS => "below -90".to_owned(),
                _ => "silent".to_owned(),
            };
            let _w = writeln!(r, "  {label}: {n}");
        }
        let _w = writeln!(r, "switches: {}", self.switches);
        let _w = writeln!(r, "no_device: {}", self.no_input);
        let _w = writeln!(r, "warnings: {}", self.warnings);
        if let Some(aec) = self.aec {
            let _w = writeln!(r, "aec: {aec}");
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn replay_options_come_in_any_order_each_once() {
        assert_eq!(replay_opts(&[]), Some((None, None)));
        assert_eq!(replay_opts(&["--shadow", "d"]), Some((Some("d"), None)));
        assert_eq!(replay_opts(&["--since", "0"]), Some((None, Some(0))));
        assert_eq!(
            replay_opts(&["--since", "1700000000", "--shadow", "d"]),
            Some((Some("d"), Some(1_700_000_000_000)))
        );
        assert_eq!(
            replay_opts(&["--shadow", "d", "--since", "1700000000"]),
            Some((Some("d"), Some(1_700_000_000_000)))
        );
        // The largest whole second whose ms fit in a u64.
        assert_eq!(
            replay_opts(&["--since", "18446744073709551"]),
            Some((None, Some(18_446_744_073_709_551_000)))
        );
        for bad in [
            &["--since"][..],
            &["--since", ""],
            &["--since", "-1"],
            &["--since", "+5"],
            &["--since", " 5"],
            &["--since", "1.5"],
            &["--since", "1e9"],
            &["--since", "18446744073709552"],
            &["--since", "5", "--since", "5"],
            &["--shadow", "d", "--shadow", "e"],
            &["--shadow", "-d"],
            &["--since", "5", "--shadow"],
            &["--since", "5", "x"],
            &["--until", "5"],
        ] {
            assert_eq!(replay_opts(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn probe_tap_parses_like_probe_mic() {
        assert_eq!(
            parse(&args(&["--probe-mic", "5"])),
            Some(Cmd::Probe(Probe::Mic, 5))
        );
        assert_eq!(
            parse(&args(&["--probe-tap", "5"])),
            Some(Cmd::Probe(Probe::Tap, 5))
        );
        assert_eq!(
            parse(&args(&["--probe-tap", "3600"])),
            Some(Cmd::Probe(Probe::Tap, 3600))
        );
        for bad in [
            &["--probe-tap"][..],
            &["--probe-tap", "0"],
            &["--probe-tap", "3601"],
            &["--probe-tap", "-1"],
            &["--probe-tap", "soon"],
            &["--probe-tap", "5", "x"],
            &["--probe-them", "5"],
            &["--probe-mic"],
            &[],
        ] {
            assert_eq!(parse(&args(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn pcm_out_takes_no_argument() {
        assert_eq!(parse(&args(&["--pcm-out"])), Some(Cmd::PcmOut));
        assert_eq!(parse(&args(&["--check"])), Some(Cmd::Check));
        assert_eq!(parse(&args(&["--ears"])), Some(Cmd::Ears));
        for bad in [
            &["--pcm-out", "5"][..],
            &["--pcm-out", "--pcm-out"],
            &["--pcm-out", "5", "x"],
            &["--pcm"],
        ] {
            assert_eq!(parse(&args(bad)), None, "{bad:?}");
        }
    }
}
