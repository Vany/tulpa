//! `tulpa-hear --replay <ring>`: the live capture's recorded pieces, offline, into a shadow dir.
//!
//! They go through the shadow's recognizer and assembler, so that `--compare --pair emit` can set them beside the
//! live logs of the same minutes.
//!
//! - The ring ([`list`]): `<side>-<piece>.wav` files (`me` or `them`; a `.json` beside one and any other non-`.wav`
//!   name are ignored, a `.wav` with any other side is skipped and counted), each ONE utterance as the live
//!   segmenter cut it, in the order of their mtimes (then their names).
//! - `--since` ([`list`]'s `since_ms`): a piece whose mtime is before it is skipped and counted as `before --since`
//!   without being opened (only its metadata is looked at); an mtime equal to it is kept. Only what was recorded
//!   after a capture fix is then compared.
//! - Each piece ([`wav_samples`]): 16 kHz mono s16le PCM WAV only; anything else is refused (said on stderr by file
//!   name, counted), never guessed at.
//! - Its span: no start time is stored, and the file's mtime is about when the utterance ended on the wall clock,
//!   so `t1` = the mtime (epoch ms, the shadow log's clock) and `t0` = `t1` − its length ([`span`]). No live
//!   segmenter runs: the pieces are already cut. Neither is `cut` (at the maximum length) nor the intonation known
//!   without the json, so every piece is a whole phrase (`cut` false) with an unknown contour (no tag).
//! - Its emit time: `t1` plus the recognizer's own processing time for it — when the live path would have emitted
//!   it had it taken that long — so the records pair with the live ones by emit time.
//! - Into the shadow dir exactly what `--shadow` writes: `lines.log` records (`<emit_ms>\t<line>`) between a
//!   `SHADOW start` stamped at the first piece's `t0` and a `SHADOW stop` at the last emit (the comparator's window
//!   is the start to the last records), and `heard.log`.
//! - Read-only on the ring: files are opened read-only and only read; nothing there is written, moved or removed.
//!   The live ring rotates: a piece gone between the listing and its read is counted as vanished.
//!
//! [`Counts::render`] is all it says on stdout: counts, never a phrase's text.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tulpa_segment::Contour;

use crate::assemble::Assembler;
use crate::hear::{self, Heard, Side};
use crate::recognize::Recognizer;
use crate::shadow::Shadow;

/// The default replay dir under the shadow's default dir.
pub const SUBDIR: &str = "replay";
/// The only rate a piece may have.
const RATE: u32 = 16_000;

/// One piece in the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Piece {
    /// Its file.
    pub path: PathBuf,
    /// Whose, from the name's prefix.
    pub side: Side,
    /// Its mtime, epoch ms: about when the utterance ended.
    pub mtime_ms: u64,
}

/// What the ring held and what became of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Counts {
    /// Pieces read and recognized, `[me, them]`.
    pub read: [u64; 2],
    /// Their audio, ms, `[me, them]`.
    pub audio_ms: [u64; 2],
    /// `.wav`s whose name has no known side.
    pub skipped: u64,
    /// Pieces whose mtime is before `--since`, never read, `[me, them]`.
    pub before_since: [u64; 2],
    /// Pieces gone between the listing and their read, `[me, them]`.
    pub vanished: [u64; 2],
    /// Pieces that are not 16 kHz mono s16le PCM WAV (or not readable), `[me, them]`.
    pub refused: [u64; 2],
    /// Pieces whose recognition failed (said on stderr), `[me, them]`.
    pub failed: [u64; 2],
    /// Lines written to `lines.log`.
    pub lines: u64,
    /// The run stopped early (a signal, or `lines.log` could not be written).
    pub stopped: bool,
    /// `lines.log` lost a record (a line, or the stop record): the run failed, whatever stopped it.
    pub lost: bool,
}

/// A side's slot of a `[me, them]` count.
fn slot(v: &mut [u64; 2], side: Side) -> &mut u64 {
    let [me, them] = v;
    if side == Side::Me { me } else { them }
}

const fn bump(v: &mut u64) {
    *v = v.saturating_add(1);
}

impl Counts {
    /// The process's exit code for a finished `--replay`: 1 when a record was lost, or when the run stopped early
    /// without a signal (`signalled`); else 0.
    #[must_use]
    pub const fn exit_code(&self, signalled: bool) -> u8 {
        if self.lost || (self.stopped && !signalled) {
            1
        } else {
            0
        }
    }

    /// The end-of-run report, one `key: value` per line; `wall` is the run's wall time.
    #[must_use]
    pub fn render(&self, wall: Duration) -> String {
        let mut r = String::new();
        for (key, v) in [
            ("read", self.read),
            ("vanished", self.vanished),
            ("refused", self.refused),
            ("failed", self.failed),
        ] {
            let [me, them] = v;
            let _w = writeln!(
                r,
                "{key}: {} (me {me}, them {them})",
                me.saturating_add(them)
            );
        }
        let _w = writeln!(r, "skipped: {}", self.skipped);
        let [me, them] = self.before_since;
        let _w = writeln!(
            r,
            "before --since: {} (me {me}, them {them})",
            me.saturating_add(them)
        );
        let [me, them] = self.audio_ms;
        let _w = writeln!(
            r,
            "audio_s: {} (me {}, them {})",
            secs(me.saturating_add(them)),
            secs(me),
            secs(them)
        );
        let _w = writeln!(r, "lines: {}", self.lines);
        let _w = writeln!(r, "wall_s: {:.1}", wall.as_secs_f64());
        if self.lost {
            let _w = writeln!(r, "lost: a lines.log record");
        }
        if self.stopped {
            let _w = writeln!(r, "stopped: early");
        }
        r
    }
}

/// Seconds with one decimal.
fn secs(ms: u64) -> String {
    format!("{}.{}", ms / 1000, (ms % 1000) / 100)
}

/// The side a ring file name gives.
///
/// `Some(Some(side))` for `me-<piece>.wav` / `them-<piece>.wav`, `Some(None)` for any other `.wav` (skipped,
/// counted), `None` for a name that is not a `.wav` (ignored: the `.json`s).
#[must_use]
pub fn side_of(name: &str) -> Option<Option<Side>> {
    let stem = name.strip_suffix(".wav")?;
    let side = match stem.split_once('-') {
        Some(("me", piece)) if !piece.is_empty() => Some(Side::Me),
        Some(("them", piece)) if !piece.is_empty() => Some(Side::Them),
        _ => None,
    };
    Some(side)
}

/// The ring's pieces, oldest mtime first (ties by name), and how many `.wav`s were skipped for their name. A piece
/// gone before its mtime could be read is left out and counted as vanished in `counts`.
///
/// With `since_ms` (epoch ms), a piece whose mtime is before it is left out and counted as `before_since`; it is
/// never opened.
///
/// # Errors
///
/// The ring dir's `read_dir`.
pub fn list(ring: &Path, since_ms: Option<u64>, counts: &mut Counts) -> io::Result<Vec<Piece>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(ring)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(side) = name.to_str().and_then(side_of) else {
            continue;
        };
        let Some(side) = side else {
            bump(&mut counts.skipped);
            continue;
        };
        let path = entry.path();
        // `metadata` follows a link, as the read will; neither writes.
        match fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => {
                let mtime_ms = crate::epoch_ms(t);
                if since_ms.is_some_and(|since| mtime_ms < since) {
                    bump(slot(&mut counts.before_since, side));
                } else {
                    out.push(Piece {
                        path,
                        side,
                        mtime_ms,
                    });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                bump(slot(&mut counts.vanished, side));
            }
            Err(e) => {
                say(&path, &format!("refused: {e}"));
                bump(slot(&mut counts.refused, side));
            }
        }
    }
    out.sort_by(|a, b| (a.mtime_ms, &a.path).cmp(&(b.mtime_ms, &b.path)));
    Ok(out)
}

/// A little-endian `u16`/`u32` at `at`.
fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The samples of a 16 kHz mono 16-bit PCM WAV, from its header's chunks (`fmt ` before `data`; other chunks are
/// stepped over).
///
/// # Errors
///
/// What is wrong with it: not RIFF/WAVE, no `fmt `, a format other than PCM (1) 16 kHz mono 16-bit, no `data`, or
/// a `data` chunk longer than the file (cut short) or of an odd length.
pub fn wav_samples(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a RIFF/WAVE file".to_owned());
    }
    let mut at = 12_usize;
    let mut fmt_ok = false;
    loop {
        let id = bytes.get(at..at.saturating_add(4)).ok_or("no data chunk")?;
        let len = le32(bytes, at.saturating_add(4)).ok_or("a chunk header cut short")?;
        let len = usize::try_from(len).map_err(|_| "a chunk too long")?;
        let body = at.saturating_add(8);
        match id {
            b"fmt " => {
                let field = |off: usize| le16(bytes, body.saturating_add(off));
                let (tag, ch, bits) = (field(0), field(2), field(14));
                let rate = le32(bytes, body.saturating_add(4));
                if len < 16 || tag.is_none() || bits.is_none() {
                    return Err("a fmt chunk cut short".to_owned());
                }
                if (tag, ch, rate, bits) != (Some(1), Some(1), Some(RATE), Some(16)) {
                    return Err(format!(
                        "format {} with {} ch at {} Hz, {} bit: only PCM (1) mono 16000 Hz 16 bit",
                        tag.unwrap_or(0),
                        ch.unwrap_or(0),
                        rate.unwrap_or(0),
                        bits.unwrap_or(0)
                    ));
                }
                fmt_ok = true;
            }
            b"data" => {
                if !fmt_ok {
                    return Err("data before fmt".to_owned());
                }
                let data = bytes
                    .get(body..body.saturating_add(len))
                    .filter(|d| d.len() == len)
                    .ok_or("the data chunk is cut short")?;
                if len % 2 != 0 {
                    return Err("an odd data length".to_owned());
                }
                return Ok(data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| i16::from_le_bytes(c))
                    .collect());
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body.saturating_add(len).saturating_add(len % 2);
    }
}

/// Milliseconds of `n` samples at 16 kHz, as the segmenter counts an utterance's length.
#[must_use]
pub fn samples_ms(n: usize) -> u64 {
    u64::try_from(n)
        .unwrap_or(u64::MAX)
        .saturating_mul(1000)
        .checked_div(u64::from(RATE))
        .unwrap_or(0)
}

/// A piece's span on the wall clock, `(t0, t1)`: it ended at its mtime and lasted `ms`.
#[must_use]
pub const fn span(mtime_ms: u64, ms: u64) -> (u64, u64) {
    (mtime_ms.saturating_sub(ms), mtime_ms)
}

/// Replays the ring into the shadow `sink` (its `lines.log`) and `assembler` (its `heard.log`).
///
/// With the shared recognizer in `lang`, oldest piece first (those before `since_ms` left out, see [`list`]), until
/// the pieces run out or `stopped` names a reason (a signal's name; asked between pieces). Writes the `SHADOW start`
/// record (at the first piece's `t0`; at the wall clock's now for a ring with none) and the `SHADOW stop` record at
/// the last emit (`end`; the reason when stopped; `error` when `lines.log` could not be written), and returns the
/// counts.
///
/// # Errors
///
/// The ring's listing, or the start record's write. A later write failure stops the run (said on stderr).
pub fn run<R: Recognizer + ?Sized>(
    ring: &Path,
    since_ms: Option<u64>,
    recognizer: &Mutex<R>,
    lang: &str,
    assembler: &mut Assembler,
    sink: &Shadow,
    stopped: &dyn Fn() -> Option<&'static str>,
) -> io::Result<Counts> {
    let mut counts = Counts::default();
    let pieces = list(ring, since_ms, &mut counts)?;
    // The start record opens the comparator's window: it is stamped at the first piece read, once its length is
    // known.
    let mut started = false;
    let mut last = 0_u64;
    let mut reason = "end";
    for p in &pieces {
        if let Some(r) = stopped() {
            reason = r;
            counts.stopped = true;
            break;
        }
        let bytes = match read_only(&p.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                bump(slot(&mut counts.vanished, p.side));
                continue;
            }
            Err(e) => {
                say(&p.path, &format!("refused: {e}"));
                bump(slot(&mut counts.refused, p.side));
                continue;
            }
        };
        let samples = match wav_samples(&bytes) {
            Ok(s) => s,
            Err(e) => {
                say(&p.path, &format!("refused: {e}"));
                bump(slot(&mut counts.refused, p.side));
                continue;
            }
        };
        let ms = samples_ms(samples.len());
        let (t0, t1) = span(p.mtime_ms, ms);
        if !started {
            sink.start(t0, std::process::id(), env!("CARGO_PKG_VERSION"))?;
            started = true;
        }
        let began = Instant::now();
        // No debug line: `--replay` says only counts on stdout.
        let transcript = hear::transcribe(p.side, recognizer, lang, &samples, ms, false);
        let took = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        let Some(transcript) = transcript else {
            bump(slot(&mut counts.failed, p.side));
            continue;
        };
        bump(slot(&mut counts.read, p.side));
        let audio = slot(&mut counts.audio_ms, p.side);
        *audio = audio.saturating_add(ms);
        let emit = t1.saturating_add(took);
        last = last.max(emit);
        let h = Heard {
            side: p.side,
            t0_ms: t0,
            t1_ms: t1,
            ms,
            cut: false,
            contour: Contour::Unknown,
            transcript,
            // `me-c000003.wav` is piece `c000003`.
            piece: p
                .path
                .file_stem()
                .and_then(|f| f.to_str())
                .and_then(|f| f.split_once('-'))
                .map_or_else(String::new, |(_, stem)| stem.to_owned()),
        };
        for line in assembler.take(&h, emit) {
            if let Err(e) = sink.record(emit, &line) {
                let _said: io::Result<()> =
                    writeln!(io::stderr(), "tulpa-hear: --replay: lines.log: {e}");
                reason = "error";
                counts.stopped = true;
                counts.lost = true;
                break;
            }
            bump(&mut counts.lines);
        }
        if counts.stopped {
            break;
        }
    }
    if !started {
        last = crate::now_ms();
        sink.start(last, std::process::id(), env!("CARGO_PKG_VERSION"))?;
    }
    if let Err(e) = sink.stop(last, reason) {
        let _said: io::Result<()> =
            writeln!(io::stderr(), "tulpa-hear: --replay: stop record: {e}");
        counts.lost = true;
    }
    Ok(counts)
}

/// The whole file, opened read-only.
fn read_only(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut f = fs::OpenOptions::new().read(true).open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)?;
    Ok(out)
}

/// One line to stderr naming the piece's file (its name only, which holds no content).
fn say(path: &Path, msg: &str) {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let _said: io::Result<()> = writeln!(io::stderr(), "tulpa-hear: --replay: {name}: {msg}");
}

#[cfg(test)]
mod tests {
    #![expect(clippy::indexing_slicing, reason = "fixed test buffers")]

    use super::*;

    #[test]
    fn names_give_the_side() {
        assert_eq!(side_of("me-c000123.wav"), Some(Some(Side::Me)));
        assert_eq!(side_of("them-c000123.wav"), Some(Some(Side::Them)));
        assert_eq!(side_of("you-c000123.wav"), Some(None));
        assert_eq!(side_of("me-.wav"), Some(None));
        assert_eq!(side_of("me.wav"), Some(None));
        assert_eq!(side_of("c000123.wav"), Some(None));
        assert_eq!(side_of("me-c000123.json"), None);
        assert_eq!(side_of("me-c000123.wav.tmp"), None);
    }

    #[test]
    fn the_segmenters_own_wav_reads_back() {
        let s: Vec<i16> = (0..1600)
            .map(|i| i16::try_from(i).unwrap_or(0) - 800)
            .collect();
        assert_eq!(wav_samples(&tulpa_segment::wav(&s)), Ok(s));
        assert_eq!(samples_ms(16_000), 1000);
        assert_eq!(samples_ms(1600), 100);
    }

    #[test]
    fn other_formats_are_refused() {
        let good = tulpa_segment::wav(&[1, 2, 3, 4]);
        let with = |at: usize, v: &[u8]| {
            let mut b = good.clone();
            b[at..at + v.len()].copy_from_slice(v);
            b
        };
        for (what, bad) in [
            ("stereo", with(22, &2u16.to_le_bytes())),
            ("44.1 kHz", with(24, &44_100u32.to_le_bytes())),
            ("8 bit", with(34, &8u16.to_le_bytes())),
            ("float", with(20, &3u16.to_le_bytes())),
            ("not RIFF", with(0, b"RIFX")),
            ("cut short", good[..good.len() - 1].to_vec()),
            ("empty", Vec::new()),
        ] {
            assert!(wav_samples(&bad).is_err(), "{what}");
        }
    }

    /// A LIST chunk between fmt and data is stepped over.
    #[test]
    fn other_chunks_are_stepped_over() {
        let good = tulpa_segment::wav(&[7, -7]);
        let mut b = good[..36].to_vec();
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(b"abc\0");
        b.extend_from_slice(&good[36..]);
        assert_eq!(wav_samples(&b), Ok(vec![7, -7]));
    }

    #[test]
    fn the_span_ends_at_the_mtime() {
        assert_eq!(span(10_000, 2_500), (7_500, 10_000));
        assert_eq!(span(100, 2_500), (0, 100));
    }
}
