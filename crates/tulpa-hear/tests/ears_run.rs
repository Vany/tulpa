//! [`ears::run`] with the live path's files, as `--ears` runs it: fake captures and recognizers, real threads.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test signal generation on small, fixed values"
)]

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::ears::{self, Ears, Outcome, Sink};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::recognize::{RecognizeError, Recognizer, Token, Transcript};
use tulpa_hear::ring::Ring;
use tulpa_stop::Stop;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tulpa-hear-run-{tag}-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&d);
    assert!(std::fs::create_dir_all(&d).is_ok());
    d
}

/// A 200 Hz tone, loud: speech to the energy gate.
fn speech(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n)
        .map(|i| ((i as f32 * 200.0 * std::f32::consts::TAU / 16_000.0).sin() * 8000.0) as i16)
        .collect()
}

/// A quiet room: low, non-zero noise.
fn quiet(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n).map(|i| if i % 2 == 0 { 40 } else { -40 }).collect()
}

/// Sends `audio` in 0.1 s chunks; `Err` once the side's pipeline is gone.
fn send(tx: &Sender<Event>, audio: &[i16]) -> Result<(), String> {
    for c in audio.chunks(1600) {
        tx.send(Event::Audio(c.to_vec()))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Waits for one message from a receiver the capture shares.
fn wait(rx: &Mutex<Receiver<()>>) -> Result<(), String> {
    rx.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recv()
        .map_err(|e| e.to_string())
}

/// The lines, kept.
#[derive(Default)]
struct Lines(Vec<String>);

impl Sink for Lines {
    fn line(&mut self, line: &str, _emit_ms: u64) -> std::io::Result<()> {
        self.0.push(line.to_owned());
        Ok(())
    }
}

/// Says when it is asked, then holds the recognition until released: a whisper pass that takes its time.
struct Held {
    entered: Sender<()>,
    release: Receiver<()>,
}

impl Recognizer for Held {
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        let _sent = self.entered.send(());
        let _released = self.release.recv();
        Ok(Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The heartbeat file's identity: each beat renames a new file into place.
fn beat_of(alive: &Path) -> Option<u64> {
    std::fs::metadata(alive).ok().map(|m| m.ino())
}

fn ears_with_files<R>(rec: R, state: &Path, tag: &str) -> std::io::Result<Ears<R>> {
    let ring = Ring::open(&temp(tag), 8 << 20, nix::unistd::getuid().as_raw())?;
    Ok(Ears::new(
        Arc::new(Mutex::new(rec)),
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    .with_files(state.to_path_buf(), ring))
}

/// While whisper holds a side's piece (longer than a beat), the side's audio still beats `.alive`: the tray must
/// not see a live side go stale because it is recognizing.
#[test]
fn a_side_beats_while_its_piece_is_recognized() -> std::io::Result<()> {
    let state = temp("beat-state");
    let alive = state.join("me").join(".alive");
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let (go_tx, go) = mpsc::channel();
    let (sent_tx, sent) = mpsc::channel();
    let (end_tx, end) = mpsc::channel();
    let (go, end) = (Mutex::new(go), Mutex::new(end));
    let sent_tx = Mutex::new(sent_tx);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        // Open, as the mic says once its stream runs: only an open stream's audio beats.
        let _sent = tx.send(Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        });
        // 5.9 s: one phrase, closed by its pause 5.5 s in, just past the stream's first beat (5 s of its audio).
        let mut first = quiet(1.0);
        first.extend(speech(2.5));
        first.extend(quiet(2.4));
        send(tx, &first)?;
        wait(&go)?;
        // Past the next beat (10 s), while whisper holds the piece.
        send(tx, &quiet(5.0))?;
        let _sent = sent_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(());
        wait(&end)
    });
    let ears = ears_with_files(
        Held {
            entered: entered_tx,
            release,
        },
        &state,
        "beat-ring",
    )?;
    let stop = Arc::new(Stop::new());
    let log = temp("beat-log");
    let running = std::thread::spawn(move || {
        let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
        let mut lines = Lines::default();
        let out = ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None);
        (out, lines.0)
    });
    // Each wait in this test is a guard against a wedge, not a timing claim.
    assert!(
        entered.recv_timeout(Duration::from_secs(60)).is_ok(),
        "the piece reached whisper"
    );
    let first = beat_of(&alive);
    assert!(first.is_some(), "the stream's first 5 s beat");
    assert!(go_tx.send(()).is_ok());
    assert!(sent.recv_timeout(Duration::from_secs(60)).is_ok());
    // A poll, in the test only: the beat is written on the run's own thread, which has no event to give the test.
    // Its limit is a guard against a wedge: the beat comes as the audio crosses 5 s of samples, at once.
    let deadline = Instant::now() + Duration::from_secs(60);
    while beat_of(&alive) == first && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let beat_while_held = beat_of(&alive) != first;
    assert!(release_tx.send(()).is_ok());
    assert!(end_tx.send(()).is_ok());
    let ran = running.join();
    assert!(beat_while_held, "no beat while whisper held the piece");
    let (out, lines) = ran.unwrap_or((Outcome::GaveUp, Vec::new()));
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    assert_eq!(lines.len(), 1, "{lines:?}");
    std::fs::remove_dir_all(&state)
}

/// Reads every piece as one word and counts the samples it is given; its first piece is held until released.
struct Slow {
    entered: Sender<()>,
    release: Receiver<()>,
    held: bool,
    given: Arc<AtomicU64>,
}

impl Recognizer for Slow {
    fn transcribe(&mut self, samples: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        if std::mem::replace(&mut self.held, true) {
            self.given.fetch_add(samples.len() as u64, Ordering::SeqCst);
        } else {
            let _sent = self.entered.send(());
            let _released = self.release.recv();
        }
        Ok(Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false))
    }
}

/// Fable F7: a side's audio waiting for whisper is bounded. While the recognizer holds one piece, a minute of
/// speech arrives; only the newest 30 s of it is still there when whisper comes back (the oldest went, as a break
/// in the stream), so the lines trail the room by a bounded time instead of by everything that ever queued.
#[test]
fn audio_waiting_for_whisper_is_bounded_and_the_oldest_goes() -> std::io::Result<()> {
    let state = temp("bound-state");
    let alive = state.join("me").join(".alive");
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let (go_tx, go) = mpsc::channel();
    let (sent_tx, sent) = mpsc::channel();
    let (end_tx, end) = mpsc::channel();
    let (go, end) = (Mutex::new(go), Mutex::new(end));
    let sent_tx = Mutex::new(sent_tx);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, _: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        let device = || Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        };
        let _sent = tx.send(device());
        // 4.9 s: under a beat (5 s of the stream's audio), so this stream never beats.
        let mut first = quiet(1.0);
        first.extend(speech(1.5));
        first.extend(quiet(2.4));
        send(tx, &first)?;
        wait(&go)?;
        // The stream fails: the minute that follows is not an open stream's, and does not beat either.
        let _sent = tx.send(Event::StreamError("gone".to_owned()));
        // A minute of speech while whisper holds the first piece. Words with short breaths between them: a steady
        // tone would become the segmenter's floor and stop counting as speech.
        let mut backlog = Vec::new();
        for _ in 0..60 {
            backlog.extend(speech(0.8));
            backlog.extend(quiet(0.2));
        }
        send(tx, &backlog)?;
        // Opened again, then the pause that closes the last phrase: a whole beat of it, the stream's one beat. The
        // heartbeat writes it as it reads the pause's last chunk, after it has passed on everything before.
        let _sent = tx.send(device());
        send(tx, &quiet(5.0))?;
        let _sent = sent_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(());
        wait(&end)
    });
    let given = Arc::new(AtomicU64::new(0));
    let ears = ears_with_files(
        Slow {
            entered: entered_tx,
            release,
            held: false,
            given: Arc::clone(&given),
        },
        &state,
        "bound-ring",
    )?;
    let stop = Arc::new(Stop::new());
    let log = temp("bound-log");
    let running = std::thread::spawn(move || {
        let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
        let mut lines = Lines::default();
        ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None)
    });
    // Guards against a wedge, not timing claims.
    assert!(
        entered.recv_timeout(Duration::from_secs(60)).is_ok(),
        "the first piece reached whisper"
    );
    assert!(go_tx.send(()).is_ok());
    assert!(sent.recv_timeout(Duration::from_secs(60)).is_ok());
    // The minute is in the side's queue once the stream's one beat is written. A poll, in the test only: the beat
    // is written on the side's own thread, which has no event to give the test. Whisper is still held meanwhile,
    // so nothing drains. Its limit is a guard against a wedge.
    let deadline = Instant::now() + Duration::from_secs(60);
    let queued = loop {
        if beat_of(&alive).is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(release_tx.send(()).is_ok());
    assert!(end_tx.send(()).is_ok());
    let out = running.join().unwrap_or(Outcome::GaveUp);
    assert!(queued, "the stream's beat, after the minute, never came");
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    let secs = given.load(Ordering::SeqCst) / 16_000;
    assert!(
        (20..=32).contains(&secs),
        "whisper was given {secs} s of the minute: the newest 30 s at most"
    );
    std::fs::remove_dir_all(&state)
}

/// The beat's gate does not lean on the segmenter's warmup: "them" has none (its first audio is classified at
/// once), and audio that comes with no `Device` is still heard but never beats, whichever side (FIX11's gate: only
/// an open stream's audio beats).
#[test]
fn audio_with_no_device_is_heard_but_never_beats() -> std::io::Result<()> {
    let state = temp("gate-state");
    let capture = Arc::new(|_: Side, tx: &Sender<Event>, _: &Stop| {
        // No `Device`: a buffer delivered while a start still blocked, or after a stream error.
        let mut audio = speech(6.0);
        audio.extend(quiet(3.0));
        send(tx, &audio)
    });
    let ears = ears_with_files(Says, &state, "gate-ring")?.with_backlog(Duration::from_secs(600));
    let mut asm = Assembler::new(&temp("gate-log"), None, HOLD_MAX_MS);
    let mut lines = Lines::default();
    let stop = Arc::new(Stop::new());
    let out = ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None);
    assert_eq!(out, Outcome::Ended);
    assert!(
        lines.0.iter().any(|l| l.contains(" [them] ")),
        "them: heard from its first frame: {:?}",
        lines.0
    );
    for side in ["me", "them"] {
        assert!(
            beat_of(&state.join(side).join(".alive")).is_none(),
            "{side}: a beat with no open stream"
        );
    }
    std::fs::remove_dir_all(&state)
}

/// A capture that has been told to stop never takes a marker meant for its successor. Once `stop` is set the run
/// only drains what it has: the tray may already have started the next capture, and a `.close` it writes now is
/// that one's ask. Here the marker appears after the stop, with audio still to drain; it is there when the run ends.
#[test]
fn a_stopped_run_leaves_the_close_marker_to_its_successor() -> std::io::Result<()> {
    let state = temp("stopped-close");
    let marker = state.join("me").join(".close");
    let asked = marker.clone();
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, stop: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        let _sent = tx.send(Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        });
        let mut before = quiet(1.0);
        before.extend(speech(1.0));
        send(tx, &before)?;
        let _stopped = stop.wait_until(None);
        // The stop is set: this ask is the next capture's. What this capture still delivers is only drained.
        std::fs::write(&asked, b"").map_err(|e| e.to_string())?;
        send(tx, &speech(2.0))
    });
    let ears = ears_with_files(Says, &state, "stopped-close-ring")?;
    let stop = Arc::new(Stop::new());
    let running = {
        let stop = Arc::clone(&stop);
        let log = temp("stopped-close-log");
        std::thread::spawn(move || {
            let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
            ears::run(
                &ears,
                &capture,
                &mut asm,
                &mut Lines::default(),
                &stop,
                None,
            )
        })
    };
    stop.set();
    let out = running.join().unwrap_or(Outcome::GaveUp);
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    assert!(
        marker.exists(),
        "the stopped run took a .close written after its stop"
    );
    std::fs::remove_dir_all(&state)
}

/// Once the stop is set, a run touches nothing it shares with its successor: no marker read (the test above), and
/// no heartbeat. A side's `.alive` says "this side hears" to the tray, which may already be judging the next
/// capture by it: a stopped run that still drains audio must not write it. Each side opens its stream only once the
/// stop is set and then delivers 12 s (two beats' worth and over): neither side has an `.alive` at the end.
///
/// Everything a side's heartbeat sees comes after the stop, so no beat can have passed its stop check before it: a
/// read of `.alive` taken at the stop could race a beat already past that check (#0530).
#[test]
fn a_stopped_run_does_not_beat_for_its_successor() -> std::io::Result<()> {
    let state = temp("stopped-beat");
    let (done_tx, done) = mpsc::channel();
    let done_tx = Mutex::new(done_tx);
    let capture = Arc::new(move |side: Side, tx: &Sender<Event>, stop: &Stop| {
        let _stopped = stop.wait_until(None);
        let _sent = tx.send(Event::Device {
            name: "Device".to_owned(),
            rate: 48_000,
            channels: 1,
        });
        send(tx, &quiet(12.0))?;
        let _told = done_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(side);
        Ok(())
    });
    let ears = ears_with_files(Says, &state, "stopped-beat-ring")?;
    let stop = Arc::new(Stop::new());
    let running = {
        let stop = Arc::clone(&stop);
        let log = temp("stopped-beat-log");
        std::thread::spawn(move || {
            let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
            ears::run(
                &ears,
                &capture,
                &mut asm,
                &mut Lines::default(),
                &stop,
                None,
            )
        })
    };
    stop.set();
    let out = running.join().unwrap_or(Outcome::GaveUp);
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    assert_eq!(
        done.try_iter().count(),
        2,
        "both sides delivered their 12 s"
    );
    for side in [Side::Me, Side::Them] {
        assert_eq!(
            beat_of(&state.join(side.as_str()).join(".alive")),
            None,
            "{}: the stopped run beat after its stop",
            side.as_str()
        );
    }
    std::fs::remove_dir_all(&state)
}

/// Runs "me" until the stop, then lets it deliver one more phrase (the drain), with a heard.log already past its
/// 20 MB bound and a ring already past its own, an old file in it. The dir the logs are in, and the ring's.
fn drained_past_the_bounds(tag: &str) -> std::io::Result<(PathBuf, PathBuf)> {
    let state = temp(&format!("{tag}-state"));
    let (log, ring_dir) = (temp(&format!("{tag}-log")), temp(&format!("{tag}-ring")));
    let capture = Arc::new(|side: Side, tx: &Sender<Event>, stop: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        let _sent = tx.send(Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        });
        send(tx, &quiet(1.0))?;
        let _stopped = stop.wait_until(None);
        // Told to stop: this phrase is only drained.
        let mut last = speech(1.5);
        last.extend(quiet(2.5));
        send(tx, &last)
    });
    // An old piece, twice the ring's bound: the next piece kept would push it out.
    std::fs::write(ring_dir.join("me-c000001.wav"), vec![0_u8; 2_000])?;
    let ring = Ring::open(&ring_dir, 1_000, nix::unistd::getuid().as_raw())?;
    let ears = Ears::new(
        Arc::new(Mutex::new(Says)),
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    .with_files(state, ring);
    let mut asm = Assembler::new(&log, None, HOLD_MAX_MS);
    // Past the bound already, without writing 20 MB: a sparse file.
    std::fs::OpenOptions::new()
        .write(true)
        .open(log.join("heard.log"))?
        .set_len(20_000_001)?;
    let stop = Arc::new(Stop::new());
    let running = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut lines = Lines::default();
            let out = ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None);
            (out, lines.0.len())
        })
    };
    stop.set();
    let (out, lines) = running.join().unwrap_or((Outcome::GaveUp, 0));
    assert_eq!(out, Outcome::Ended, "the run panicked, or did not end");
    assert_eq!(lines, 1, "the drained phrase was written");
    Ok((log, ring_dir))
}

/// Once the stop is set a run only APPENDS to what it shares with its successor. heard.log is past its bound: the
/// drained phrase is appended to it, and it is not rotated (the next capture may have rotated it already, and a
/// second rename would put its fresh log over the rotated one).
#[test]
fn a_stopped_run_does_not_rotate_heard_log() -> std::io::Result<()> {
    let (log, ring) = drained_past_the_bounds("stopped-rotate")?;
    assert!(
        !log.join("heard.log.1").exists(),
        "the stopped run rotated heard.log"
    );
    let len = std::fs::metadata(log.join("heard.log"))?.len();
    assert!(len > 20_000_001, "the phrase was appended: {len} bytes");
    std::fs::remove_dir_all(&log)?;
    std::fs::remove_dir_all(&ring)
}

/// The same for the ring: the drained piece is parked, and nothing is deleted to make room, though the ring is
/// past its bound. Pruning is the next capture's alone.
#[test]
fn a_stopped_run_prunes_nothing_from_the_ring() -> std::io::Result<()> {
    let (log, ring) = drained_past_the_bounds("stopped-prune")?;
    assert!(
        ring.join("me-c000001.wav").exists(),
        "the stopped run deleted an old piece"
    );
    let parked = std::fs::read_dir(&ring)?.count();
    assert_eq!(parked, 4, "the old piece and the drained one's three files");
    std::fs::remove_dir_all(&log)?;
    std::fs::remove_dir_all(&ring)
}

/// The clock of [`a_late_stream_is_anchored_where_its_samples_begin`]: the capture moves it as it sends.
static CLOCK: AtomicU64 = AtomicU64::new(START);
const START: u64 = 1_800_000_000_000;

fn fake_now() -> u64 {
    CLOCK.load(Ordering::SeqCst)
}

/// Reads every piece as one word.
struct Says;

impl Recognizer for Says {
    fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
        Ok(Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false))
    }
}

/// The tap sends nothing until something plays: a side whose first samples arrive 2 s after the start has its
/// `t0`/`t1` where its samples are, not 2 s early.
#[test]
fn a_late_stream_is_anchored_where_its_samples_begin() -> std::io::Result<()> {
    let state = temp("anchor-state");
    let capture = Arc::new(|side: Side, tx: &Sender<Event>, _: &Stop| {
        if side == Side::Me {
            return Ok(());
        }
        // 1 s of room, 1.5 s of speech, 3 s of room, from 2 s after the start, each 0.1 s chunk sent when "heard".
        CLOCK.store(START + 2_000, Ordering::SeqCst);
        let mut audio = quiet(1.0);
        audio.extend(speech(1.5));
        audio.extend(quiet(3.0));
        for c in audio.chunks(1600) {
            CLOCK.fetch_add(100, Ordering::SeqCst);
            tx.send(Event::Audio(c.to_vec()))
                .map_err(|e| e.to_string())?;
            // Paced, so the pipeline reads each chunk near its time (a late read only makes an estimate late).
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    });
    let ears = Ears::new(
        Arc::new(Mutex::new(Says)),
        "ru".to_owned(),
        state.join("me/.close"),
        CloseMode::Take,
        fake_now,
    );
    let mut asm = Assembler::new(&state, None, HOLD_MAX_MS);
    let mut lines = Lines::default();
    let stop = Arc::new(Stop::new());
    let out = ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None);
    assert_eq!(out, Outcome::Ended);
    assert_eq!(lines.0.len(), 1, "{:?}", lines.0);
    let line = lines.0.first().map_or("", String::as_str);
    let t: Vec<u64> = line
        .split(' ')
        .skip(1)
        .take(2)
        .filter_map(|v| v.parse().ok())
        .collect();
    // Speech from 3.0 to 4.5 s after the start; the segmenter's pre- and post-roll and frames within 250 ms.
    assert_eq!(t.len(), 2, "{line}");
    assert!(
        t.first()
            .is_some_and(|t0| t0.abs_diff(START + 3_000) <= 250),
        "{line}"
    );
    assert!(
        t.get(1).is_some_and(|t1| t1.abs_diff(START + 4_500) <= 250),
        "{line}"
    );
    std::fs::remove_dir_all(&state)
}
