//! `--replay` with a temp ring of synthetic pieces and a fake recognizer: the mtime order, the side from the name,
//! `t0`/`t1` from the mtime and the length, the emit time after the recognizer's own time, the ring left
//! byte-identical, a piece gone mid-run counted, a bad wav refused, `--since` skipping older pieces unread; and the
//! binary's refusal without `whisper` and of a bad `--since`.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "small fixed test times and a fixed-size test buffer"
)]

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File, FileTimes, Permissions};
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::recognize::{RecognizeError, Recognizer, Token, Transcript};
use tulpa_hear::replay::{self, Counts};
use tulpa_hear::shadow::{BOUND, LINES_LOG, Shadow};

/// A fixed wall-clock moment in the past: the pieces' mtimes are set around it.
const T: u64 = 1_700_000_000_000;

/// Says one word per piece after `delay`; on its first call removes `vanish` (the live ring rotating) and puts a
/// directory where `break_log` is (a log that can no longer be written).
#[derive(Default)]
struct Fake {
    delay: Duration,
    vanish: Option<PathBuf>,
    break_log: Option<PathBuf>,
    calls: usize,
}

impl Recognizer for Fake {
    fn transcribe(&mut self, _samples: &[f32], _lang: &str) -> Result<Transcript, RecognizeError> {
        self.calls += 1;
        if let Some(p) = self.vanish.take() {
            fs::remove_file(p).ok();
        }
        if let Some(log) = self.break_log.take() {
            break_log(log.parent().unwrap_or(&log));
        }
        std::thread::sleep(self.delay);
        Ok(Transcript::new(
            vec![Token::new(" alpha", 0, 300, 0.9)],
            false,
        ))
    }
}

fn temp(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tulpa-hear-replay-it-{tag}-{}", std::process::id()));
    let _gone = fs::remove_dir_all(&dir);
    assert!(fs::create_dir_all(dir.join("ring")).is_ok());
    dir
}

fn set_mtime(path: &Path, ms: u64) {
    let f = File::options().write(true).open(path);
    assert!(f.is_ok(), "{f:?}");
    let at = UNIX_EPOCH + Duration::from_millis(ms);
    assert!(f.is_ok_and(|f| f.set_times(FileTimes::new().set_modified(at)).is_ok()));
}

/// A piece of `ms` of 16 kHz samples, written as the segmenter writes it, ending at `end_ms`.
fn piece(ring: &Path, name: &str, ms: u64, end_ms: u64) -> PathBuf {
    let n = usize::try_from(ms * 16).unwrap_or(0);
    let path = ring.join(name);
    assert!(fs::write(&path, tulpa_segment::wav(&vec![100_i16; n])).is_ok());
    set_mtime(&path, end_ms);
    path
}

/// `lines.log`'s records: `(emit_ms, record)`.
fn records(dir: &Path) -> Vec<(u64, String)> {
    fs::read_to_string(dir.join(LINES_LOG))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (ms, rec) = l.split_once('\t')?;
            Some((ms.parse().ok()?, rec.to_owned()))
        })
        .collect()
}

/// A line record's `(side, t0, t1)`.
fn span(rec: &str) -> Option<(String, u64, u64)> {
    let mut f = rec.strip_prefix("TULPA-EARS-LINE ")?.split(' ');
    let (t0, t1) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
    let _clock = f.next()?;
    Some((f.next()?.to_owned(), t0, t1))
}

fn run(dir: &Path, fake: Fake) -> (Counts, Fake) {
    run_since(dir, fake, None)
}

/// Replays `dir`'s ring into `dir/out` after `prepare` (given the out dir, once it is opened), asking `stopped`
/// between pieces; the run's result.
fn run_raw(
    dir: &Path,
    fake: Fake,
    prepare: impl FnOnce(&Path),
    stopped: &dyn Fn() -> Option<&'static str>,
) -> std::io::Result<Counts> {
    let out = dir.join("out");
    let sink = Shadow::open(&out, BOUND)?;
    prepare(&out);
    let mut asm = Assembler::new(&out, None, HOLD_MAX_MS).with_log_bound(BOUND);
    let rec = Mutex::new(fake);
    replay::run(
        &dir.join("ring"),
        None,
        &rec,
        "en",
        &mut asm,
        &sink,
        stopped,
    )
}

/// Puts a directory where `out`'s `lines.log` is: every later append fails.
fn break_log(out: &Path) {
    let log = out.join(LINES_LOG);
    assert!(fs::remove_file(&log).is_ok() && fs::create_dir(&log).is_ok());
}

/// Sink path P3, the first `sink.start`: a start record that cannot be written is the run's error (`die`, exit 1).
#[test]
fn a_lost_start_record_is_an_error() {
    let dir = temp("start-lost");
    piece(&dir.join("ring"), "me-c000001.wav", 800, T);
    assert!(run_raw(&dir, Fake::default(), break_log, &|| None).is_err());
    fs::remove_dir_all(&dir).ok();
}

/// Sink path P4, `sink.record`: a line that cannot be written stops the run and fails it (exit 1).
#[test]
fn a_lost_line_fails_the_replay() {
    let dir = temp("line-lost");
    piece(&dir.join("ring"), "me-c000001.wav", 800, T);
    let fake = Fake {
        break_log: Some(dir.join("out").join(LINES_LOG)),
        ..Fake::default()
    };
    let counts = run_raw(&dir, fake, |_| {}, &|| None);
    assert!(
        counts.as_ref().is_ok_and(|c| c.lost && c.stopped),
        "{counts:?}"
    );
    assert_eq!(counts.map(|c| c.exit_code(false)).ok(), Some(1));
    fs::remove_dir_all(&dir).ok();
}

/// Sink path P5, the start record of a ring with no pieces: it cannot be written, the run's error (exit 1).
#[test]
fn a_lost_start_record_of_an_empty_ring_is_an_error() {
    let dir = temp("empty-start-lost");
    assert!(run_raw(&dir, Fake::default(), break_log, &|| None).is_err());
    fs::remove_dir_all(&dir).ok();
}

/// Sink path P6, `sink.stop`: a stop record that cannot be written fails the run (exit 1), though no line was lost,
/// and even when a signal is what stopped it. The signal comes before the second piece; the log breaks just then,
/// after the first piece's line.
#[test]
fn a_lost_stop_record_fails_the_replay() {
    let dir = temp("stop-lost");
    let ring = dir.join("ring");
    piece(&ring, "me-c000001.wav", 800, T);
    piece(&ring, "me-c000002.wav", 800, T + 2_000);
    let out = dir.join("out");
    let asked = std::cell::Cell::new(0_u32);
    let stopped = || {
        asked.set(asked.get().saturating_add(1));
        (asked.get() == 2).then(|| {
            break_log(&out);
            "SIGTERM"
        })
    };
    let counts = run_raw(&dir, Fake::default(), |_| {}, &stopped);
    assert!(
        counts
            .as_ref()
            .is_ok_and(|c| c.lines == 1 && c.stopped && c.lost),
        "{counts:?}"
    );
    assert_eq!(counts.map(|c| c.exit_code(true)).ok(), Some(1));
    fs::remove_dir_all(&dir).ok();
}

fn run_since(dir: &Path, fake: Fake, since_ms: Option<u64>) -> (Counts, Fake) {
    let out = dir.join("out");
    let sink = Shadow::open(&out, BOUND);
    assert!(sink.is_ok(), "{sink:?}");
    let Ok(sink) = sink else {
        return (Counts::default(), fake);
    };
    let mut asm = Assembler::new(&out, None, HOLD_MAX_MS).with_log_bound(BOUND);
    let rec = Mutex::new(fake);
    let counts = replay::run(
        &dir.join("ring"),
        since_ms,
        &rec,
        "en",
        &mut asm,
        &sink,
        &|| None,
    );
    assert!(counts.is_ok(), "{counts:?}");
    (
        counts.unwrap_or_default(),
        rec.into_inner().unwrap_or_default(),
    )
}

/// One ring file as seen: name, bytes, mtime, mode.
type Seen = (String, Vec<u8>, Option<SystemTime>, u32);

/// Every file in the ring, and a hash over all of it.
fn snapshot(ring: &Path) -> (Vec<Seen>, u64) {
    let mut all: Vec<_> = fs::read_dir(ring)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let m = e.metadata().ok();
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap_or_default(),
                m.as_ref().and_then(|m| m.modified().ok()),
                m.map_or(0, |m| m.permissions().mode()),
            )
        })
        .collect();
    all.sort();
    let mut h = DefaultHasher::new();
    all.hash(&mut h);
    (all, h.finish())
}

/// Out of name order on purpose: the mtimes decide. Each line's span is `[mtime − length, mtime]`, its side the
/// name's prefix; a `.wav` with another prefix is skipped and counted, a `.json` and a stray file are ignored.
#[test]
fn pieces_replay_in_mtime_order_with_spans_from_their_length() {
    let dir = temp("order");
    let ring = dir.join("ring");
    piece(&ring, "me-c000003.wav", 1_000, T + 5_000);
    piece(&ring, "them-c000001.wav", 2_000, T + 1_000);
    piece(&ring, "me-c000002.wav", 500, T + 3_000);
    piece(&ring, "you-c000004.wav", 700, T + 2_000);
    assert!(fs::write(ring.join("me-c000002.json"), b"{}").is_ok());
    assert!(fs::write(ring.join("notes.txt"), b"x").is_ok());
    let (counts, fake) = run(&dir, Fake::default());
    assert_eq!(fake.calls, 3, "the skipped piece is not read");
    assert_eq!(counts.read, [2, 1]);
    assert_eq!(counts.audio_ms, [1_500, 2_000]);
    assert_eq!(counts.skipped, 1);
    assert_eq!(
        (counts.vanished, counts.refused, counts.lines),
        ([0, 0], [0, 0], 3)
    );
    let recs = records(&dir.join("out"));
    assert_eq!(recs.len(), 5, "{recs:?}");
    let spans: Vec<_> = recs.iter().filter_map(|(_, r)| span(r)).collect();
    assert_eq!(
        spans,
        vec![
            ("[them]".to_owned(), T - 1_000, T + 1_000),
            ("[me]".to_owned(), T + 2_500, T + 3_000),
            ("[me]".to_owned(), T + 4_000, T + 5_000),
        ],
        "mtime order, t1 = mtime, t0 = t1 − length"
    );
    // The window: the start at the first piece's t0, the stop at the last emit.
    let first = recs.first().cloned().unwrap_or_default();
    assert_eq!(first.0, T - 1_000);
    assert!(first.1.starts_with("SHADOW start "), "{first:?}");
    let last = recs.last().cloned().unwrap_or_default();
    assert_eq!(last.1, "SHADOW stop end");
    let emits: Vec<u64> = recs.iter().map(|(ms, _)| *ms).collect();
    assert_eq!(last.0, emits.iter().copied().max().unwrap_or(0));
    // heard.log beside it, one line per phrase.
    let heard = fs::read_to_string(dir.join("out/heard.log")).unwrap_or_default();
    assert_eq!(heard.lines().count(), 3, "{heard}");
    fs::remove_dir_all(&dir).ok();
}

/// The emit time is the piece's end plus the recognizer's own time on it.
#[test]
fn emit_is_t1_plus_the_recognizers_time() {
    let dir = temp("emit");
    piece(&dir.join("ring"), "them-c000001.wav", 1_000, T);
    let (counts, _) = run(
        &dir,
        Fake {
            delay: Duration::from_millis(300),
            ..Fake::default()
        },
    );
    assert_eq!(counts.lines, 1);
    let recs = records(&dir.join("out"));
    let line = recs.iter().find(|(_, r)| r.starts_with("TULPA-EARS-LINE"));
    let emit = line.map_or(0, |(ms, _)| *ms);
    assert!(
        (T + 300..T + 2_300).contains(&emit),
        "emit {} ms after t1",
        emit.saturating_sub(T)
    );
    fs::remove_dir_all(&dir).ok();
}

/// Read-only on the ring: with the ring and its files made unwritable, every piece is still read, and afterwards
/// every file there is byte for byte, mtime and mode what it was.
#[test]
fn the_ring_is_left_byte_identical() {
    let dir = temp("readonly");
    let ring = dir.join("ring");
    piece(&ring, "me-c000001.wav", 800, T);
    piece(&ring, "them-c000002.wav", 1_200, T + 2_000);
    assert!(fs::write(ring.join("me-c000001.json"), b"{\"ms\":800}").is_ok());
    piece(&ring, "x-c000003.wav", 300, T + 3_000);
    for e in fs::read_dir(&ring).into_iter().flatten().flatten() {
        assert!(fs::set_permissions(e.path(), Permissions::from_mode(0o400)).is_ok());
    }
    assert!(fs::set_permissions(&ring, Permissions::from_mode(0o500)).is_ok());
    let (before, hash_before) = snapshot(&ring);
    let (counts, _) = run(&dir, Fake::default());
    let (after, hash_after) = snapshot(&ring);
    assert!(fs::set_permissions(&ring, Permissions::from_mode(0o700)).is_ok());
    assert_eq!(counts.read, [1, 1]);
    assert_eq!(before.len(), 4);
    assert_eq!(hash_before, hash_after);
    assert_eq!(before, after);
    fs::remove_dir_all(&dir).ok();
}

/// A piece the live ring rotated away between the listing and its read is counted, and the run goes on.
#[test]
fn a_piece_gone_mid_run_is_counted_as_vanished() {
    let dir = temp("vanish");
    let ring = dir.join("ring");
    piece(&ring, "me-c000001.wav", 800, T);
    let gone = piece(&ring, "them-c000002.wav", 800, T + 2_000);
    piece(&ring, "me-c000003.wav", 800, T + 4_000);
    let (counts, fake) = run(
        &dir,
        Fake {
            vanish: Some(gone),
            ..Fake::default()
        },
    );
    assert_eq!(counts.vanished, [0, 1]);
    assert_eq!(counts.read, [2, 0]);
    assert_eq!(fake.calls, 2);
    assert_eq!(counts.lines, 2);
    fs::remove_dir_all(&dir).ok();
}

/// A piece that is not 16 kHz mono s16le WAV is refused (and counted), never fed in; the others still are.
#[test]
fn a_bad_wav_is_refused() {
    let dir = temp("bad");
    let ring = dir.join("ring");
    piece(&ring, "me-c000001.wav", 800, T);
    let mut stereo = tulpa_segment::wav(&[0_i16; 3_200]);
    stereo[22] = 2;
    assert!(fs::write(ring.join("them-c000002.wav"), &stereo).is_ok());
    set_mtime(&ring.join("them-c000002.wav"), T + 1_000);
    assert!(fs::write(ring.join("me-c000003.wav"), b"not a wav at all").is_ok());
    set_mtime(&ring.join("me-c000003.wav"), T + 2_000);
    let mut short = tulpa_segment::wav(&[0_i16; 3_200]);
    short.truncate(100);
    assert!(fs::write(ring.join("them-c000004.wav"), &short).is_ok());
    set_mtime(&ring.join("them-c000004.wav"), T + 3_000);
    let (counts, fake) = run(&dir, Fake::default());
    assert_eq!(counts.refused, [1, 2]);
    assert_eq!(counts.read, [1, 0]);
    assert_eq!(fake.calls, 1);
    assert!(
        counts
            .render(Duration::ZERO)
            .contains("refused: 3 (me 1, them 2)")
    );
    fs::remove_dir_all(&dir).ok();
}

/// The report is counts only.
#[test]
fn the_report_is_counts() {
    let dir = temp("report");
    piece(&dir.join("ring"), "them-c000001.wav", 1_500, T);
    let (counts, _) = run(&dir, Fake::default());
    let r = counts.render(Duration::from_millis(2_340));
    assert!(!r.contains("alpha"), "{r}");
    assert_eq!(
        r,
        "read: 1 (me 0, them 1)\nvanished: 0 (me 0, them 0)\nrefused: 0 (me 0, them 0)\n\
         failed: 0 (me 0, them 0)\nskipped: 0\nbefore --since: 0 (me 0, them 0)\naudio_s: 1.5 (me 0.0, them 1.5)\nlines: 1\nwall_s: 2.3\n"
    );
    fs::remove_dir_all(&dir).ok();
}

/// With `--since`, a piece whose mtime is before it is counted as `before --since` and never read (it is not even a
/// wav: read, it would be refused), one at exactly that mtime is kept, and the start record is the first kept
/// piece's `t0`.
#[test]
fn since_skips_older_pieces_unread_and_keeps_an_equal_mtime() {
    let dir = temp("since");
    let ring = dir.join("ring");
    let since = T + 10_000;
    let old = ring.join("them-c000001.wav");
    assert!(fs::write(&old, b"not a wav").is_ok());
    set_mtime(&old, since - 1);
    piece(&ring, "me-c000002.wav", 2_000, since - 5_000);
    piece(&ring, "me-c000003.wav", 1_000, since);
    piece(&ring, "them-c000004.wav", 1_500, since + 3_000);
    let (counts, fake) = run_since(&dir, Fake::default(), Some(since));
    assert_eq!(counts.before_since, [1, 1]);
    assert_eq!(counts.refused, [0, 0]);
    assert_eq!(counts.read, [1, 1]);
    assert_eq!(fake.calls, 2);
    let recs = records(&dir.join("out"));
    assert!(
        recs.first()
            .is_some_and(|(ms, r)| *ms == since - 1_000 && r.starts_with("SHADOW start")),
        "{recs:?}"
    );
    let spans: Vec<_> = recs.iter().filter_map(|(_, r)| span(r)).collect();
    assert_eq!(
        spans,
        [
            ("[me]".to_owned(), since - 1_000, since),
            ("[them]".to_owned(), since + 1_500, since + 3_000)
        ]
    );
    assert!(
        counts
            .render(Duration::ZERO)
            .contains("\nbefore --since: 2 (me 1, them 1)\n")
    );
    fs::remove_dir_all(&dir).ok();
}

/// A bad `--since` is a usage error (exit 2, the usage on stderr) before anything else, with or without `whisper`.
#[test]
fn the_binary_refuses_a_bad_since() {
    let dir = temp("since-bin");
    let ring = dir.join("ring");
    let out = dir.join("out");
    let (r, o) = (ring.to_string_lossy(), out.to_string_lossy());
    for bad in [
        &[&*r, "--since"][..],
        &[&*r, "--since", ""],
        &[&*r, "--since", "-1"],
        &[&*r, "--since", "+5"],
        &[&*r, "--since", "1.5"],
        &[&*r, "--since", "soon"],
        &[&*r, "--since", "18446744073709552"],
        &[&*r, "--since", "5", "--since", "6"],
        &[&*r, "--since", "5", "--shadow", &*o, "--since", "5"],
        &[&*r, "--since", "5", "extra"],
        &["--since", "5", &*r],
    ] {
        let run = std::process::Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
            .arg("--replay")
            .args(bad)
            .output();
        assert!(run.is_ok(), "{run:?}");
        let Ok(run) = run else { return };
        assert_eq!(run.status.code(), Some(2), "{bad:?}");
        assert!(run.stdout.is_empty(), "{bad:?}");
        assert!(
            String::from_utf8_lossy(&run.stderr).starts_with("usage:"),
            "{bad:?}"
        );
    }
    assert!(!out.exists());
    fs::remove_dir_all(&dir).ok();
}

/// Without the `whisper` feature the binary refuses `--replay` with exit 2 before making any dir.
#[test]
#[cfg(not(feature = "whisper"))]
fn the_binary_refuses_replay_without_whisper() {
    let dir = temp("bin");
    let out = dir.join("out");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_tulpa-hear"))
        .arg("--replay")
        .arg(dir.join("ring"))
        .arg("--shadow")
        .arg(&out)
        .output();
    assert!(run.is_ok(), "{run:?}");
    let Ok(run) = run else { return };
    assert_eq!(run.status.code(), Some(2));
    assert!(run.stdout.is_empty());
    assert!(!out.exists());
    fs::remove_dir_all(&dir).ok();
}
