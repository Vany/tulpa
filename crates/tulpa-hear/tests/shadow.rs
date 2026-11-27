//! `--shadow` end to end with fake captures and a fake recognizer: the shared pipeline into the shadow dir, the
//! tray's `EARS_STATE` only looked at, the stop record at the grace after a signal, and the binary's refusal without
//! the `whisper` feature (stdout empty, no dir made).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tulpa_hear::assemble::Assembler;
use tulpa_hear::ears::{self, Ears, GiveUp, Outcome};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::recognize::{RecognizeError, Recognizer, Token, Transcript};
use tulpa_hear::shadow::{self, Caught, CaughtSlot, LINES_LOG, Shadow, Slot};
use tulpa_stop::Stop;

/// How long a test waits for the recognizer to be entered: past it the test fails, it never hangs.
///
/// A guard against a wedge, not a timing claim: no test asserts how soon an entry comes. It is far longer than
/// segmenting a minute of talk takes on a loaded debug build.
const ENTRY_LIMIT: Duration = Duration::from_secs(60);

/// Says one word for every utterance, after `delay`; each entry is told first, by the utterance's length in samples.
struct Fake {
    delay: Duration,
    entered: Sender<usize>,
}

impl Recognizer for Fake {
    fn transcribe(&mut self, samples: &[f32], _lang: &str) -> Result<Transcript, RecognizeError> {
        let _told = self.entered.send(samples.len());
        std::thread::sleep(self.delay);
        Ok(Transcript::new(
            vec![Token::new(" alpha", 0, 400, 0.9)],
            false,
        ))
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test signal generation on small, fixed values"
)]
fn speech(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n)
        .map(|i| ((i as f32 * 200.0 * std::f32::consts::TAU / 16_000.0).sin() * 8000.0) as i16)
        .collect()
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test signal generation on small, fixed values"
)]
fn quiet(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n).map(|i| if i % 2 == 0 { 40 } else { -40 }).collect()
}

fn temp(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tulpa-hear-shadow-it-{tag}-{}", std::process::id()));
    let _gone = fs::remove_dir_all(&dir);
    dir
}

/// Every path under `dir` with its length and mtime, sorted.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            let meta = fs::symlink_metadata(&path).ok();
            if meta.as_ref().is_some_and(fs::Metadata::is_dir) {
                todo.push(path.clone());
            }
            let len = meta.as_ref().map_or(0, fs::Metadata::len);
            out.push((path, len, meta.and_then(|m| m.modified().ok())));
        }
    }
    out.sort();
    out
}

fn mode(p: &Path) -> Option<u32> {
    fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn chunks(audio: &[i16]) -> Vec<Vec<i16>> {
    audio.chunks(1600).map(<[i16]>::to_vec).collect()
}

/// About `secs` of talk: speech in 0.9 s bursts with 0.3 s gaps, far shorter than the pause that closes a phrase.
/// The gaps keep the noise floor at the room's: a tone held on is taken for the floor in time, and ends the phrase.
fn talk(secs: f32) -> Vec<i16> {
    let mut out = Vec::new();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test signal generation on small, fixed values"
    )]
    let want = (secs * 16_000.0) as usize;
    while out.len() < want {
        out.extend(speech(0.9));
        out.extend(quiet(0.3));
    }
    out
}

/// A [`Fake`] taking `delay` per utterance, and where its entries are told.
fn recognizer(delay: Duration) -> (Arc<Mutex<Fake>>, Receiver<usize>) {
    let (entered, entries) = mpsc::channel();
    (Arc::new(Mutex::new(Fake { delay, entered })), entries)
}

/// The next entry whose length `pick` takes, waited for at most [`ENTRY_LIMIT`] in all; `None` past it.
fn entered(entries: &Receiver<usize>, pick: impl Fn(usize) -> bool) -> Option<usize> {
    let until = Instant::now().checked_add(ENTRY_LIMIT)?;
    loop {
        let left = until.checked_duration_since(Instant::now())?;
        match entries.recv_timeout(left) {
            Ok(n) if pick(n) => return Some(n),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// The shadow's lines go to its `lines.log` with the emit ms, heard.log goes to its dir, and `EARS_STATE` holds
/// exactly what the tray put there: the `.close` marker (which still closes "me"'s utterance) is left in place.
#[test]
fn shadow_writes_only_its_dir_and_leaves_ears_state_alone() {
    let base = temp("only");
    let (state, dir) = (base.join("state"), base.join("shadow"));
    assert!(fs::create_dir_all(state.join("me")).is_ok());
    let marker = state.join("me").join(".close");
    // "me" talks past the segmenter's 60 s cap, so its phrase is cut there and goes on. The tray asks for the close
    // once the cut piece is in the recognizer: the pipeline is then inside the phrase's rest, however far behind the
    // capture it runs, so the marker always lands mid-utterance (a marker seen before the phrase began closes
    // nothing). "me" is heard three times: the cut piece, the rest up to the ask, and the tail after it; the first two
    // are one line (the assembler joins a cut phrase to its rest).
    let mut me = quiet(2.0);
    me.extend(talk(64.0));
    let ask_at = me.len() / 1600;
    me.extend(talk(2.0));
    me.extend(quiet(3.0));
    let mut them = quiet(2.0);
    them.extend(speech(1.5));
    them.extend(quiet(3.0));
    let tray_marker = marker.clone();
    let (rec, entries) = recognizer(Duration::ZERO);
    let entries = Mutex::new(entries);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _stop: &Stop| {
        let audio = if side == Side::Me { &me } else { &them };
        for (i, c) in chunks(audio).into_iter().enumerate() {
            if side == Side::Me && i == ask_at {
                let cut = entries
                    .lock()
                    .map_err(|e| e.to_string())
                    .map(|e| entered(&e, |n| n > 20 * 16_000))?;
                cut.ok_or("the cut piece never reached the recognizer")?;
                fs::write(&tray_marker, b"").map_err(|e| e.to_string())?;
            }
            tx.send(Event::Audio(c)).map_err(|e| e.to_string())?;
        }
        Ok(())
    });
    let sink = Shadow::open(&dir, shadow::BOUND);
    assert!(sink.is_ok(), "{sink:?}");
    let Ok(mut sink) = sink else { return };
    assert!(sink.start(now_ms(), 7, "0.1.0").is_ok());
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        marker.clone(),
        CloseMode::Watch,
        now_ms,
    )
    // "me"'s 71 s come at once, not as they are heard: nothing of them is to be dropped as a backlog.
    .with_backlog(Duration::from_secs(600));
    let stop = Arc::new(Stop::new());
    let reason = shadow::run(&ears, &capture, &mut assembler, &mut sink, &stop, None);
    assert_eq!(reason, "end");

    // EARS_STATE: only what the tray made, untouched.
    let after = snapshot(&state);
    let paths: Vec<&PathBuf> = after.iter().map(|(p, _, _)| p).collect();
    assert_eq!(
        paths,
        [&state.join("me"), &marker],
        "nothing written to EARS_STATE"
    );
    assert_eq!(fs::metadata(&marker).map(|m| m.len()).ok(), Some(0));

    let text = fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "start, 2 me, 1 them, stop: {lines:?}");
    for l in &lines {
        let (ms, rec) = l.split_once('\t').unwrap_or(("", ""));
        assert!(
            ms.parse::<u64>().is_ok_and(|ms| ms > 1_700_000_000_000),
            "{l}"
        );
        assert!(
            rec.starts_with("TULPA-EARS-LINE ") || rec.starts_with("SHADOW "),
            "{l}"
        );
    }
    assert!(
        lines
            .first()
            .is_some_and(|l| l.ends_with("\tSHADOW start 7 0.1.0"))
    );
    assert!(
        lines
            .last()
            .is_some_and(|l| l.ends_with("\tSHADOW stop end"))
    );
    let count = |side: &str| {
        lines
            .iter()
            .filter(|l| l.contains(&format!(" [{side}] alpha")))
            .count()
    };
    assert_eq!(
        (count("me"), count("them")),
        (2, 1),
        "two for me only because the watched marker closed the rest at the ask: without it, the cut piece and all \
         its rest are one line"
    );
    let heard = fs::read_to_string(dir.join("heard.log")).unwrap_or_default();
    assert_eq!(heard.lines().count(), 3, "{heard}");
    for f in [LINES_LOG, "heard.log", "mic-switch.log"] {
        assert_eq!(mode(&dir.join(f)), Some(0o600), "{f}");
    }
    assert_eq!(mode(&dir), Some(0o700));
    fs::remove_dir_all(&base).ok();
}

/// Fills the slot with a SIGTERM caught now and sets `stop`, as the signal thread does.
fn signal(slot: &CaughtSlot, stop: &Stop) -> Instant {
    let now = Instant::now();
    slot.fill(Caught::new("SIGTERM", now));
    stop.set();
    now
}

/// The signal tests' rig: "me" says one phrase and pauses (the pause closes it, so it reaches a recognizer 90 s slow,
/// past the grace and the 60 s guard: only the give-up ends the run before the guard), then stays quiet until `stop`;
/// "them" says nothing. The ears, their capture and the
/// recognizer's entries, and the shadow dir under `base`.
fn slow_phrase(
    base: &Path,
) -> (
    Ears<Fake>,
    Arc<impl tulpa_hear::ears::Capture + use<>>,
    Receiver<usize>,
    PathBuf,
) {
    let (state, dir) = (base.join("state"), base.join("shadow"));
    assert!(fs::create_dir_all(state.join("me")).is_ok());
    let capture = Arc::new(|side: Side, tx: &Sender<Event>, stop: &Stop| {
        let mut audio = quiet(2.0);
        audio.extend(speech(3.0));
        audio.extend(quiet(60.0));
        for c in chunks(&audio) {
            if stop.is_set() {
                break;
            }
            if side == Side::Me {
                tx.send(Event::Audio(c)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    });
    let (rec, entries) = recognizer(Duration::from_secs(90));
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Watch,
        now_ms,
    )
    // The rig sends its minute of audio at once, not as it is heard: nothing of it is to be dropped as a backlog.
    .with_backlog(Duration::from_secs(600));
    (ears, capture, entries, dir)
}

/// `took` (from the signal to the run's return) is the grace, not less, and under the 60 s guard. No tighter upper
/// bound: the run has no clock but the grace, and the recognizer's own end is past the guard.
fn at_the_grace(took: Duration) -> bool {
    took >= shadow::GRACE && took < Duration::from_secs(60)
}

/// A signal while the phrase is in the recognizer: the run gives the phrase up, writes `SHADOW stop SIGTERM`
/// and returns at the grace after the signal.
#[test]
fn a_signal_stops_the_run_with_its_record_at_the_grace() {
    let base = temp("signal");
    let (ears, capture, entries, dir) = slow_phrase(&base);
    let sink = Shadow::open(&dir, shadow::BOUND);
    let Ok(mut sink) = sink else {
        assert!(sink.is_ok(), "{sink:?}");
        return;
    };
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let stop = Arc::new(Stop::new());
    let slot: CaughtSlot = Arc::new(Slot::new());
    let signaller = {
        let (slot, stop) = (Arc::clone(&slot), Arc::clone(&stop));
        // Signalled whether or not the recognizer was entered, so a test that fails still ends.
        std::thread::spawn(move || (entered(&entries, |_| true), signal(&slot, &stop)))
    };
    let reason = shadow::run(
        &ears,
        &capture,
        &mut assembler,
        &mut sink,
        &stop,
        Some(&slot),
    );
    let returned = Instant::now();
    let joined = signaller.join();
    assert!(joined.is_ok());
    let Ok((phrase, at)) = joined else { return };
    assert!(phrase.is_some(), "the phrase never reached the recognizer");
    assert_eq!(reason, "SIGTERM");
    let took = returned.duration_since(at);
    assert!(at_the_grace(took), "{took:?}");
    let text = fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default();
    assert!(text.ends_with("\tSHADOW stop SIGTERM\n"), "{text}");
    assert!(
        !text.contains("TULPA-EARS-LINE"),
        "the slow phrase was dropped: {text}"
    );
    fs::remove_dir_all(&base).ok();
}

/// `stop` already set (as a capture or sink error sets it) and seen by the run while the phrase is still in the
/// recognizer, then a signal: the signal still bounds the run to the grace after it. Driven through `ears::run`,
/// whose give-up instant tells the test when the run has seen the stop.
#[test]
fn a_signal_after_an_error_stop_still_bounds_the_run() {
    let base = temp("signal-after-stop");
    let (ears, capture, entries, dir) = slow_phrase(&base);
    let sink = Shadow::open(&dir, shadow::BOUND);
    let Ok(mut sink) = sink else {
        assert!(sink.is_ok(), "{sink:?}");
        return;
    };
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let stop = Arc::new(Stop::new());
    let changed = Arc::new(Stop::new());
    let caught: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let (asked_tx, asked) = mpsc::channel();
    let at = {
        let caught = Arc::clone(&caught);
        move || {
            let _told = asked_tx.send(());
            caught
                .lock()
                .ok()
                .and_then(|c| *c)
                .and_then(|c| c.checked_add(shadow::GRACE))
        }
    };
    let signaller = {
        let (stop, changed, caught) =
            (Arc::clone(&stop), Arc::clone(&changed), Arc::clone(&caught));
        // Each wait is bounded and the signal comes whatever they saw, so a test that fails still ends.
        std::thread::spawn(move || {
            let phrase = entered(&entries, |_| true);
            stop.set();
            let seen = asked.recv_timeout(ENTRY_LIMIT).is_ok();
            let now = Instant::now();
            if let Ok(mut c) = caught.lock() {
                *c = Some(now);
            }
            changed.set();
            (phrase, seen, now)
        })
    };
    let outcome = ears::run(
        &ears,
        &capture,
        &mut assembler,
        &mut sink,
        &stop,
        Some(GiveUp::new(&at, &changed)),
    );
    let returned = Instant::now();
    let joined = signaller.join();
    assert!(joined.is_ok());
    let Ok((phrase, seen, signal_at)) = joined else {
        return;
    };
    assert!(phrase.is_some(), "the phrase never reached the recognizer");
    assert!(seen, "the run never asked when to give up after the stop");
    assert_eq!(outcome, Outcome::GaveUp);
    let took = returned.duration_since(signal_at);
    assert!(at_the_grace(took), "{took:?}");
    fs::remove_dir_all(&base).ok();
}

/// A capture that fails is the run's `error`, recorded as such.
#[test]
fn a_failed_capture_is_stop_error() {
    let dir = temp("error");
    let capture = Arc::new(|_: Side, _: &Sender<Event>, _: &Stop| Err("gone".to_owned()));
    let sink = Shadow::open(&dir, shadow::BOUND);
    let Ok(mut sink) = sink else {
        assert!(sink.is_ok(), "{sink:?}");
        return;
    };
    let mut assembler = Assembler::new(&dir, None, 300_000);
    let (rec, _entries) = recognizer(Duration::ZERO);
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        dir.join("none"),
        CloseMode::Watch,
        now_ms,
    );
    let stop = Arc::new(Stop::new());
    assert_eq!(
        shadow::run(&ears, &capture, &mut assembler, &mut sink, &stop, None),
        "error"
    );
    let text = fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default();
    assert!(text.ends_with("\tSHADOW stop error\n"), "{text}");
    fs::remove_dir_all(&dir).ok();
}

/// Without the feature `--shadow` refuses with 2 before anything: nothing on stdout, no dir made.
#[cfg(not(feature = "whisper"))]
#[test]
fn shadow_without_whisper_exits_2_and_makes_nothing() {
    use std::process::Command;

    let dir = temp("bin");
    let out = Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
        .args(["--shadow", &dir.to_string_lossy()])
        .output();
    assert!(out.is_ok());
    let Ok(out) = out else { return };
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--shadow: built without whisper"));
    assert!(!dir.exists());
}

/// A sink that holds its first line until told, and counts the lines it took.
struct Held {
    release: Receiver<()>,
    taken: usize,
}

impl tulpa_hear::ears::Sink for Held {
    fn line(&mut self, _line: &str, _emit_ms: u64) -> std::io::Result<()> {
        if self.taken == 0 {
            // Bounded, so a test that fails still ends.
            let _released = self.release.recv_timeout(ENTRY_LIMIT);
        }
        self.taken = self.taken.saturating_add(1);
        Ok(())
    }
}

/// Past the give-up instant, no more lines are written, a backlog included: "me" says four short phrases, the sink
/// holds the first line while the rest queue up, then `stop` comes with a give-up instant already past. The line in
/// hand is the last one written.
#[test]
fn past_the_grace_a_backlog_is_dropped() {
    let base = temp("backlog");
    let dir = base.join("shadow");
    assert!(fs::create_dir_all(&dir).is_ok());
    let mut me = quiet(2.0);
    for _ in 0..4 {
        me.extend(speech(1.0));
        me.extend(quiet(2.5));
    }
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _stop: &Stop| {
        if side == Side::Me {
            for c in chunks(&me) {
                tx.send(Event::Audio(c)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    });
    let (rec, entries) = recognizer(Duration::ZERO);
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        dir.join("none"),
        CloseMode::Watch,
        now_ms,
    );
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let (release, held) = mpsc::channel();
    let mut sink = Held {
        release: held,
        taken: 0,
    };
    let stop = Arc::new(Stop::new());
    let controller = {
        let stop = Arc::clone(&stop);
        // Four entries: the first three phrases are queued behind the held line (each is sent before the next
        // entry). Every wait is bounded and the release comes regardless, so a test that fails still ends.
        std::thread::spawn(move || {
            let all = (0..4).all(|_| entered(&entries, |_| true).is_some());
            stop.set();
            let _sent = release.send(());
            all
        })
    };
    let past = Instant::now();
    let at = move || Some(past);
    let never = Stop::new();
    let outcome = ears::run(
        &ears,
        &capture,
        &mut assembler,
        &mut sink,
        &stop,
        Some(GiveUp::new(&at, &never)),
    );
    assert_eq!(
        controller.join().ok(),
        Some(true),
        "four phrases recognized"
    );
    assert_eq!(outcome, Outcome::GaveUp);
    assert_eq!(sink.taken, 1, "only the line in hand at the stop");
    fs::remove_dir_all(&base).ok();
}

/// A sink whose first line fails (a full disk, then room again).
#[derive(Default)]
struct FailsOnce {
    calls: usize,
}

impl tulpa_hear::ears::Sink for FailsOnce {
    fn line(&mut self, _line: &str, _emit_ms: u64) -> std::io::Result<()> {
        self.calls = self.calls.saturating_add(1);
        if self.calls == 1 {
            Err(std::io::Error::other("no room (fake)"))
        } else {
            Ok(())
        }
    }
}

/// Sink path P1, `sink.line`: a line the sink lost fails the run (a non-zero exit), even when the sink works again
/// later, instead of a clean end.
#[test]
fn a_line_the_sink_loses_fails_the_run() {
    let base = temp("sink-fails");
    let dir = base.join("shadow");
    assert!(fs::create_dir_all(&dir).is_ok());
    let mut me = quiet(2.0);
    me.extend(speech(1.0));
    me.extend(quiet(2.5));
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _stop: &Stop| {
        if side == Side::Me {
            for c in chunks(&me) {
                tx.send(Event::Audio(c)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    });
    let (rec, _entries) = recognizer(Duration::ZERO);
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        dir.join("none"),
        CloseMode::Watch,
        now_ms,
    );
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let mut sink = FailsOnce::default();
    let stop = Arc::new(Stop::new());
    let outcome = ears::run(&ears, &capture, &mut assembler, &mut sink, &stop, None);
    assert_eq!(sink.calls, 1, "the line was offered and lost");
    assert_eq!(outcome, Outcome::Failed);
    assert_eq!(outcome.exit_code(), 1);
    fs::remove_dir_all(&base).ok();
}

/// Sink path P2, the shadow's `sink.stop`: a stop record that cannot be written fails the run (`error`, a non-zero
/// exit): the comparator reads the log, and a log without its stop record is no clean end.
#[test]
fn a_lost_stop_record_fails_the_shadow_run() {
    let dir = temp("stop-lost");
    let sink = Shadow::open(&dir, shadow::BOUND);
    let Ok(mut sink) = sink else {
        assert!(sink.is_ok(), "{sink:?}");
        return;
    };
    // A directory where the log is: every append fails.
    let log = dir.join(LINES_LOG);
    assert!(fs::remove_file(&log).is_ok() && fs::create_dir(&log).is_ok());
    let capture = Arc::new(|_: Side, _: &Sender<Event>, _: &Stop| Ok(()));
    let (rec, _entries) = recognizer(Duration::ZERO);
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        dir.join("none"),
        CloseMode::Watch,
        now_ms,
    );
    let mut assembler = Assembler::new(&dir, None, 300_000);
    let stop = Arc::new(Stop::new());
    let reason = shadow::run(&ears, &capture, &mut assembler, &mut sink, &stop, None);
    assert_eq!(reason, "error");
    assert_eq!(shadow::exit_code(reason), 1);
    fs::remove_dir_all(&dir).ok();
}

/// Runs `tulpa-hear --shadow <dir>` with a cleared environment, `EARS_STATE` under `base`, and stand-in models (one
/// byte each: they pass the preflight's look and fail to load); its exit code, stderr and the dir's `lines.log`.
#[cfg(all(feature = "whisper", target_os = "macos"))]
fn shadow_binary(base: &Path, dir: &Path) -> (Option<i32>, String, String) {
    use std::process::Command;

    let (state, models) = (base.join("state"), base.join("models"));
    assert!(fs::create_dir_all(state.join("me")).is_ok() && fs::create_dir_all(&models).is_ok());
    for name in ["ggml-large-v3-turbo-q5_0.bin", "ggml-silero-v5.1.2.bin"] {
        assert!(fs::write(models.join(name), b"x").is_ok());
    }
    let out = Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("EARS_STATE", &state)
        .env("EARS_MODELS", &models)
        .arg("--shadow")
        .arg(dir)
        .output();
    assert!(out.is_ok(), "{out:?}");
    let Ok(out) = out else {
        return (None, String::new(), String::new());
    };
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        fs::read_to_string(dir.join(LINES_LOG)).unwrap_or_default(),
    )
}

/// Sink path P7, the setup's stop record (`bail` in main): a setup failure after the start record (here the model
/// does not load) writes `SHADOW stop error` and exits 1.
#[cfg(all(feature = "whisper", target_os = "macos"))]
#[test]
fn a_setup_failure_after_the_start_record_is_stop_error_and_exit_1() {
    let base = temp("bin-bail");
    let dir = base.join("shadow");
    let (code, stderr, log) = shadow_binary(&base, &dir);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("cannot load the model"), "{stderr}");
    assert!(log.ends_with("\tSHADOW stop error\n"), "{log}");
    fs::remove_dir_all(&base).ok();
}

/// Sink path P8, the start record (`open_shadow` in main): a shadow dir that cannot be made, so no start record,
/// exits 1 before anything runs.
#[cfg(all(feature = "whisper", target_os = "macos"))]
#[test]
fn a_shadow_dir_that_cannot_be_made_exits_1() {
    let base = temp("bin-open");
    assert!(fs::create_dir_all(&base).is_ok() && fs::write(base.join("a-file"), b"").is_ok());
    let dir = base.join("a-file").join("shadow");
    let (code, stderr, _) = shadow_binary(&base, &dir);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("--shadow: ") && stderr.contains("Not a directory"),
        "{stderr}"
    );
    fs::remove_dir_all(&base).ok();
}

/// Says one word for every utterance once it is let go; each entry is told first.
struct Gated {
    entered: Sender<()>,
    go: Receiver<()>,
}

impl Recognizer for Gated {
    fn transcribe(&mut self, _samples: &[f32], _lang: &str) -> Result<Transcript, RecognizeError> {
        let _told = self.entered.send(());
        // Bounded, so a test that fails still ends.
        let _let_go = self.go.recv_timeout(ENTRY_LIMIT);
        Ok(Transcript::new(
            vec![Token::new(" alpha", 0, 400, 0.9)],
            false,
        ))
    }
}

/// A sink that loses every line, and tells each one it lost.
struct Loses(Sender<()>);

impl tulpa_hear::ears::Sink for Loses {
    fn line(&mut self, _line: &str, _emit_ms: u64) -> std::io::Result<()> {
        let _told = self.0.send(());
        Err(std::io::Error::other("no room (fake)"))
    }
}

/// Sink path P1 inside the grace: the signal stops the run, the phrase then leaves the recognizer and the sink loses
/// its line, then the grace runs out. The run failed: `Failed`, the shadow's `error` (exit 1), not the signal's
/// name. On events only: the recognizer is let go after the signal, the grace's end is the test's instant, given
/// once the line is lost, and "me"'s capture holds its side open, so the give-up is what ends the run.
#[test]
fn a_line_lost_inside_the_grace_fails_the_run() {
    let base = temp("lost-in-grace");
    let dir = base.join("shadow");
    assert!(fs::create_dir_all(&dir).is_ok());
    let mut me = quiet(2.0);
    me.extend(speech(1.0));
    me.extend(quiet(2.5));
    let (hold_tx, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _stop: &Stop| {
        if side == Side::Me {
            for c in chunks(&me) {
                tx.send(Event::Audio(c)).map_err(|e| e.to_string())?;
            }
            // Held open until the test lets go (bounded, so a test that fails still ends).
            let _held = hold.lock().map(|h| h.recv_timeout(ENTRY_LIMIT));
        }
        Ok(())
    });
    let (entered_tx, entered_rx) = mpsc::channel();
    let (go_tx, go) = mpsc::channel();
    let rec = Arc::new(Mutex::new(Gated {
        entered: entered_tx,
        go,
    }));
    let ears = Ears::new(
        rec,
        "ru".to_owned(),
        dir.join("none"),
        CloseMode::Watch,
        now_ms,
    );
    let mut assembler = Assembler::new(&dir, None, 300_000).with_log_bound(shadow::BOUND);
    let (lost_tx, lost) = mpsc::channel();
    let mut sink = Loses(lost_tx);
    let stop = Arc::new(Stop::new());
    let changed = Arc::new(Stop::new());
    let grace_end: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let at = {
        let grace_end = Arc::clone(&grace_end);
        move || grace_end.lock().ok().and_then(|g| *g)
    };
    let controller = {
        let (stop, changed, grace_end) = (
            Arc::clone(&stop),
            Arc::clone(&changed),
            Arc::clone(&grace_end),
        );
        // Every wait is bounded and the grace ends regardless, so a test that fails still ends.
        std::thread::spawn(move || {
            let phrase = entered_rx.recv_timeout(ENTRY_LIMIT).is_ok();
            stop.set(); // the signal
            let _let_go = go_tx.send(());
            let line_lost = lost.recv_timeout(ENTRY_LIMIT).is_ok();
            if let Ok(mut g) = grace_end.lock() {
                *g = Some(Instant::now());
            }
            changed.set();
            (phrase, line_lost)
        })
    };
    let outcome = ears::run(
        &ears,
        &capture,
        &mut assembler,
        &mut sink,
        &stop,
        Some(GiveUp::new(&at, &changed)),
    );
    drop(hold_tx);
    assert_eq!(
        controller.join().ok(),
        Some((true, true)),
        "the phrase reached the recognizer and the sink"
    );
    assert_eq!(outcome, Outcome::Failed);
    assert_eq!(outcome.exit_code(), 1);
    let reason = shadow::reason(Some("SIGTERM"), outcome);
    assert_eq!(reason, "error");
    assert_eq!(shadow::exit_code(reason), 1);
    fs::remove_dir_all(&base).ok();
}
