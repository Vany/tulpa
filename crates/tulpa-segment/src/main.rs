//! `tulpa-segment --dir <dir> [--start <n>] [--close-ms <ms>] [--max-ms <ms>]`: 16 kHz mono s16le PCM on stdin → one WAV per utterance in `<dir>`.
//!
//! Each utterance is written as `c<n>.wav` (`<n>` from `--start`, zero-padded to six digits, counting up), with
//! `c<n>.json` beside it (its intonation and length), the JSON first. Both go to a hidden temporary name and are
//! renamed into place, so a reader listing `c*.wav` never sees half a file. A line per utterance goes to stderr.
//! At the first audio read and every 5 s of audio after it, `<dir>/.alive` is rewritten (and renamed into place): silence makes no utterance, and
//! the tray's self-check judges a side by its directory changing, so the heartbeat says the audio still flows.
//! The end of stdin closes an utterance in progress. So does `<dir>/.close`: at every read the file is looked for and,
//! if there, removed, and the utterance in progress (with the audio of that read) closes as at the end of stdin —
//! then the cutting goes on, the room's floor and the stream's clock kept. The tray drops it when its own
//! `voice.say` ends, so the owner's next words don't join the tail of our voice. A `.close` that can't be removed
//! is said once on stderr and ignored. Exit 0 at the end of stdin; 2 on a usage error; 1 on a write
//! error.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tulpa_segment::{ALIVE, Anchor, BEAT, Config, Segmenter, Utterance, put, stem, wav};

fn usage() -> ExitCode {
    let _said: io::Result<()> = writeln!(
        io::stderr(),
        "usage: tulpa-segment --dir <dir> [--start <n>] [--close-ms 200..30000] [--max-ms 5000..300000, above --close-ms]"
    );
    ExitCode::from(2)
}

fn emit(dir: &Path, n: u64, u: &Utterance, anchor_ms: u64) -> io::Result<()> {
    let base = stem(n);
    put(
        dir,
        &format!("{base}.json"),
        u.json_line(anchor_ms).as_bytes(),
    )?;
    put(dir, &format!("{base}.wav"), &wav(&u.samples))?;
    writeln!(
        io::stderr(),
        "tulpa-segment: {base} {} ms, {}{}",
        u.ms(),
        u.intonation.contour,
        if u.cut { ", cut at the max" } else { "" }
    )
}

/// Now, epoch ms.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut dir: Option<PathBuf> = None;
    let mut n: u64 = 0;
    let mut close_ms: Option<u16> = None;
    let mut max_ms: Option<u32> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            "--start" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => n = v,
                None => return usage(),
            },
            // Milliseconds, so the capture script can pass EARS_CLOSE_MS / EARS_MAX_MS as they are.
            "--close-ms" => match args
                .next()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| (200..=30_000).contains(v))
                .map(u16::try_from)
                .and_then(Result::ok)
            {
                Some(v) => close_ms = Some(v),
                None => return usage(),
            },
            "--max-ms" => match args
                .next()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| (5_000..=300_000).contains(v))
            {
                Some(v) => max_ms = Some(v),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let Some(dir) = dir else { return usage() };
    let mut cfg = Config::default();
    if let Some(ms) = close_ms {
        cfg.close_silence = f32::from(ms) / 1000.0;
    }
    if let Some(ms) = max_ms {
        // In tenths of a second (≤ 3000): exact in a u16, and fine enough for a cap.
        cfg.max_len = f32::from(u16::try_from(ms / 100).unwrap_or(u16::MAX)) / 10.0;
    }
    if cfg.max_len <= cfg.close_silence {
        return usage();
    }
    let mut stream = Stream::new(dir, n, Segmenter::new(cfg));
    if stream.pump(&mut io::stdin()).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// The cutting loop over one input: the state `main` keeps between reads.
struct Stream {
    dir: PathBuf,
    /// The next utterance's number.
    n: u64,
    seg: Segmenter,
    /// A byte of a sample split across two reads.
    carry: Option<u8>,
    /// Samples since the last heartbeat.
    since_beat: usize,
    clock: Anchor,
    /// A `.close` that could not be removed was said once (stderr).
    stuck_said: bool,
}

impl Stream {
    fn new(dir: PathBuf, n: u64, seg: Segmenter) -> Self {
        Self {
            dir,
            n,
            seg,
            carry: None,
            since_beat: BEAT,
            clock: Anchor::default(),
            stuck_said: false,
        }
    }

    /// Reads `input` to its end, writing each closed utterance; the end closes the one in progress.
    ///
    /// # Errors
    ///
    /// A write error in `dir` (the heartbeat or an utterance).
    fn pump(&mut self, input: &mut impl Read) -> io::Result<()> {
        let mut buf = vec![0u8; 16_000];
        loop {
            let got = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut bytes: Vec<u8> = self.carry.take().into_iter().collect();
            bytes.extend_from_slice(buf.get(..got).unwrap_or(&[]));
            let (pairs, rest) = bytes.as_chunks::<2>();
            self.carry = rest.first().copied();
            let samples: Vec<i16> = pairs.iter().map(|p| i16::from_le_bytes(*p)).collect();
            let (anchor_ms, drift) = self.clock.read(samples.len(), now_ms());
            if let Some(drift) = drift {
                let _said: io::Result<()> = writeln!(
                    io::stderr(),
                    "tulpa-segment: drift {drift} ms (a backlog, or a gap in the input)"
                );
            }
            self.since_beat = self.since_beat.saturating_add(samples.len());
            if self.since_beat >= BEAT {
                self.since_beat = 0;
                put(&self.dir, ALIVE, b"")?;
            }
            for u in self.seg.push(&samples) {
                self.write(&u, anchor_ms)?;
            }
            // The samples just read were (mostly) heard before the `.close` was asked: they go in first.
            if self.close_asked()
                && let Some(u) = self.seg.finish()
            {
                self.write(&u, anchor_ms)?;
            }
        }
        if let Some(u) = self.seg.finish() {
            self.write(&u, self.clock.at())?;
        }
        Ok(())
    }

    fn write(&mut self, u: &Utterance, anchor_ms: u64) -> io::Result<()> {
        emit(&self.dir, self.n, u, anchor_ms)?;
        self.n = self.n.saturating_add(1);
        Ok(())
    }

    /// Whether `<dir>/.close` was there, removing it. Only a removed one counts (one close per ask); one that
    /// can't be removed is said once on stderr and otherwise ignored — it never stops the cutting.
    fn close_asked(&mut self) -> bool {
        match std::fs::remove_file(self.dir.join(".close")) {
            Ok(()) => true,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound && !self.stuck_said {
                    self.stuck_said = true;
                    let _said: io::Result<()> =
                        writeln!(io::stderr(), "tulpa-segment: .close not removed: {e}");
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "a test signal generator turns seconds and floats into samples"
    )]

    use super::*;

    /// Speech-like: a 150 Hz voice under a 4 Hz syllable envelope, `secs` long.
    fn speech(secs: f32) -> Vec<i16> {
        let n = (secs * 16_000.0) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let p = std::f32::consts::TAU * 150.0 * t;
                let env = 0.45f32.mul_add((std::f32::consts::TAU * 4.0 * t).sin(), 0.55);
                (0.5f32.mul_add((2.0 * p).sin(), p.sin()) / 1.5 * 6000.0 * env) as i16
            })
            .collect()
    }

    /// A quiet room: a little deterministic noise, `secs` long.
    fn quiet(secs: f32) -> Vec<i16> {
        let mut seed: u32 = 12_345;
        (0..(secs * 16_000.0) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((((seed >> 16) & 0x7fff) as f32 / 32_768.0 - 0.5) * 200.0) as i16
            })
            .collect()
    }

    /// stdin as the capture gives it, 0.5 s per read; before read `ask_at`, drops `.close` in `dir` (the tray),
    /// and records which utterances were on disk at each read.
    struct Mic {
        bytes: Vec<u8>,
        at: usize,
        reads: usize,
        ask_at: usize,
        dir: PathBuf,
        seen: Vec<usize>,
    }

    impl Read for Mic {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.seen.push(wavs(&self.dir).len());
            if self.reads == self.ask_at {
                std::fs::write(self.dir.join(".close"), b"")?;
            }
            self.reads = self.reads.saturating_add(1);
            let rest = self.bytes.get(self.at..).unwrap_or(&[]);
            let k = rest.len().min(buf.len()).min(16_000);
            buf.get_mut(..k)
                .unwrap_or(&mut [])
                .copy_from_slice(rest.get(..k).unwrap_or(&[]));
            self.at = self.at.saturating_add(k);
            Ok(k)
        }
    }

    fn wavs(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        n.starts_with('c') && Path::new(n).extension().is_some_and(|x| x == "wav")
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The tray's `.close` mid-utterance closes it at that read — long before the 2 s pause would — and is
    /// removed; the cutting goes on: the speech after the ask is an utterance of its own, and a later phrase is
    /// cut on its pause as usual.
    #[test]
    fn a_close_file_ends_the_utterance_in_progress_and_the_cutting_goes_on()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-segment-close-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir)?;
        // 2 s of room (warmup), then 6 s of speech with no pause; `.close` at read 10 (5 s in, 3 s of speech),
        // then 2.5 s of room, a 2 s phrase, and 3 s of room (its 2 s pause closes it).
        let audio: Vec<i16> =
            [quiet(2.0), speech(6.0), quiet(2.5), speech(2.0), quiet(3.0)].concat();
        let mut mic = Mic {
            bytes: audio.iter().flat_map(|s| s.to_le_bytes()).collect(),
            at: 0,
            reads: 0,
            ask_at: 10,
            dir: dir.clone(),
            seen: Vec::new(),
        };
        let mut stream = Stream::new(dir.clone(), 0, Segmenter::new(Config::default()));
        stream.pump(&mut mic)?;
        // Read 10 took the audio up to 5.5 s; the next read finds the first utterance on disk.
        assert_eq!(mic.seen.get(10), Some(&0), "{:?}", mic.seen);
        assert_eq!(
            mic.seen.get(11),
            Some(&1),
            "closed at the ask, mid-speech: {:?}",
            mic.seen
        );
        assert!(!dir.join(".close").exists(), "the ask is removed");
        assert_eq!(wavs(&dir), ["c000000.wav", "c000001.wav", "c000002.wav"]);
        let first = std::fs::read_to_string(dir.join("c000000.json"))?;
        assert!(
            !first.contains("\"cut\""),
            "a close, not a max cut: {first}"
        );
        // Speech went on past the ask (5.5–8 s): its own utterance, closed by the pause after it.
        let rest = std::fs::read_to_string(dir.join("c000001.json"))?;
        assert!(!rest.contains("\"cut\""), "{rest}");
        // The later phrase (10.5–12.5 s) closes on its pause, before the input ends (at 15.5 s, read 31).
        let third_at = mic.seen.iter().position(|&k| k == 3);
        assert!(
            third_at.is_some_and(|i| i < 31),
            "cut on the pause: {:?}",
            mic.seen
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
