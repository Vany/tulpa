//! The ears' sources, moving out of `packaging/macos/ears-capture.sh` into Rust: the microphone ("me") and the
//! system audio ("them"). Audio stays in memory on its way to the lines; the one place it is written is the raw
//! ring (`--ears` only, as the script keeps it): each recognized piece, local, `0600`, byte-bounded.
//!
//! - [`dsp`]: downmix and one band-limited resample from the device's native rate to 16 kHz, `i16` little-endian;
//!   no gain, no limiter; pure, every platform.
//! - [`follow`]: which device to listen to — the macOS default input or output, followed as it changes, or a mic
//!   pinned by `EARS_MIC` — and the loop that rebuilds the capture on each device event (`tulpa-audio-events`); pure
//!   over [`follow::DeviceHost`] and [`follow::Source`], every platform.
//! - [`osver`]: whether this macOS (14.6 or later) can use a process tap, from `SystemVersion.plist`; pure.
//! - `mic` (macOS only): the `CoreAudio` capture through `cpal`, switching streams as the follower says.
//! - `aec` (macOS only): the optional `VoiceProcessingIO` capture `mic` uses when `EARS_MIC_AEC=1` (Apple's noise
//!   suppression and gain control; not an echo canceller for our own speech).
//! - `stream` (macOS only): what the mic and the tap share: the `CoreAudio` host as the follower sees it, its device
//!   events, and the `cpal` input stream both open.
//! - `tap` (macOS only): the system output through `cpal`'s `CoreAudio` process tap, following the default output.
//!
//! - [`check`]: the `--check` preflight (macOS version, model files, the `whisper` feature) that `--ears` also
//!   passes before opening any device; pure, every platform.
//! - [`event`]: what a capture reports on its channel; every platform.
//! - [`start`]: the order of a capture's start — its `Device` only once it runs, no audio before that
//!   `Device` — shared by the mic's and the tap's streams and the voice unit; every platform, wired on macOS.
//! - [`recognize`]: the [`recognize::Recognizer`] seam and the empty-transcript retry; every platform.
//! - [`span`]: VAD speech spans and putting per-span token times back on the utterance's timeline; pure, every
//!   platform.
//! - `whisper` (the `whisper` feature only): the real recognizer, whisper.cpp in process through `whisper-rs`, run
//!   on the speech spans its own Silero VAD finds.
//! - [`hear`]: one side's pipeline — events, the `tulpa-segment` cutter (and the tray's `.close` on "me"), the
//!   recognizer — to [`hear::Heard`]; every platform.
//! - [`beat`]: one side's heartbeat (`.alive`), withheld from a stream that keeps failing, and the tap's silent gaps
//!   filled with zeros, between a capture and its pipeline; every platform.
//! - [`close`]: the tray's `.close` marker on "me", taken (`--ears`) or watched (`--shadow`), and the asked closes
//!   waiting for their point in the stream; every platform.
//! - [`queue`]: a side's events on their way to its pipeline, holding at most 30 s of audio and dropping the oldest
//!   past it; every platform.
//! - [`pcm_out`]: `--pcm-out`'s pump, capture events to raw 16 kHz s16le on stdout and a line per other event on
//!   stderr; every platform.
//! - [`assemble`]: [`hear::Heard`] to the tray's `TULPA-EARS-LINE`, byte-compatible with the capture script's (marker
//!   drop, subword merge, pauses, credit leaks and decode loops as `<voice N.Ns>`, no speech found as
//!   `<no speech N.Ns>`, the turn hold, the prompt-echo filter, intonation), the deaf-suspect streak, and
//!   `<EARS_STATE>/heard.log` (with why each real unread piece was unread); every platform.
//! - [`log`]: the ears' own logs: appended `0600`, rotated once past 20 MB (`heard.log`, the shadow's `lines.log`,
//!   the mic's `mic-switch.log`); unix.
//! - [`ring`]: the raw ring, each piece's WAV, segmenter JSON and transcript, byte-bounded; unix.
//!
//! - [`ears`]: both sides from the captures to the lines, shared by `--ears` (stdout, the `.close` taken) and
//!   `--shadow` (the shadow dir, the `.close` only watched); every platform, the captures injected.
//! - [`live`]: `--ears`' one way to stop, the end of its stdin or a termination signal, with the phrase in progress
//!   and a held turn written; unix.
//! - [`shadow`]: the shadow rehearsal's dir, `lines.log` records, rotation and signal watch; unix.
//! - [`compare`]: the shadow rehearsal's verdict from the live and shadow logs, counts only; pure, every platform.
//! - [`replay`]: the live ring's recorded pieces, offline, through the shadow's recognizer and assembler into a
//!   shadow dir (`--replay`); the ring only read; unix.
//! - [`watchdog`]: the live emit latency (its change, the live clock's drift removed) judged against a baseline;
//!   SIGTERM to a verified shadow pid on a rise (`--watchdog`); unix.
//!
//! The binary: `tulpa-hear --probe-mic`, `--probe-tap`, `--pcm-out`, `--check`, `--ears` and `--shadow [dir]`
//! (both sides; need the `whisper` feature), `--replay` (needs it too), `--compare` and `--watchdog`.

#[cfg(target_os = "macos")]
pub mod aec;
pub mod assemble;
pub mod beat;
pub mod check;
pub mod close;
pub mod compare;
pub mod dsp;
pub mod ears;
pub mod event;
pub mod follow;
pub mod hear;
pub mod live;
pub mod log;
#[cfg(target_os = "macos")]
pub mod mic;
pub mod osver;
pub mod pcm_out;
pub mod queue;
pub mod recognize;
pub mod replay;
pub mod ring;
pub mod shadow;
pub mod span;
// Only `mic` (macOS) calls it; elsewhere only its tests do.
#[cfg_attr(
    not(any(target_os = "macos", test)),
    expect(dead_code, reason = "wired by the macOS-only mic; tested everywhere")
)]
pub mod start;
#[cfg(target_os = "macos")]
pub mod stream;
#[cfg(target_os = "macos")]
pub mod tap;
#[cfg(test)]
pub mod testkit;
pub mod watchdog;
#[cfg(feature = "whisper")]
pub mod whisper;

/// `t` as epoch ms: 0 before 1970, saturating.
#[must_use]
pub fn epoch_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Epoch ms now: the one wall clock of the lines, the logs and the sides' anchors.
#[must_use]
pub fn now_ms() -> u64 {
    epoch_ms(std::time::SystemTime::now())
}

/// Writes `line` and its newline to `w` as ONE write.
///
/// The capture's stderr is the tray's `ears.log`, the file the tray also appends the capture's lines to, and the
/// next capture's too: every writer appends, so a line that goes out in one write lands whole. A `writeln!` on
/// the unbuffered stderr is a write per formatted piece, and another writer's line could land between two of them.
///
/// # Errors
///
/// The write's.
pub fn line_to(w: &mut impl std::io::Write, line: &str) -> std::io::Result<()> {
    let mut whole = String::with_capacity(line.len().saturating_add(1));
    whole.push_str(line);
    whole.push('\n');
    w.write_all(whole.as_bytes())
}

/// One line on stderr, in one write ([`line_to`]); a stderr that is gone is not an error here.
pub fn warn(line: &str) {
    let _said: std::io::Result<()> = line_to(&mut std::io::stderr(), line);
}

/// [`warn`]s `line` unless `said` holds it already: a failure met again (a retry, a reopen) is said once. Whether it
/// was said now.
pub fn warn_once(said: &std::sync::Mutex<std::collections::BTreeSet<String>>, line: &str) -> bool {
    let new = said
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(line.to_owned());
    if new {
        warn(line);
    }
    new
}

/// Writes `text` to stdout as is and flushes it: every text this crate prints goes through here.
///
/// Std's stdout already flushes at each newline, on a pipe too; the flush says so where it matters, since the tray
/// reads `--ears` through a pipe and a kill (the tray's Pause, or a capture that ignored its stop) flushes nothing.
///
/// # Errors
///
/// The write's or the flush's.
pub fn print(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    /// Counts the writes it is given.
    #[derive(Default)]
    struct Counts(usize, Vec<u8>);

    impl Write for Counts {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(1);
            self.1.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A stderr line is one write, newline included: two processes appending to one log cannot land inside each
    /// other's lines. (A `writeln!` with arguments is several.)
    #[test]
    fn a_line_goes_out_in_one_write() {
        let mut w = Counts::default();
        assert!(super::line_to(&mut w, &format!("tulpa-hear: {}: {}", "me", "device X")).is_ok());
        assert_eq!(w.0, 1, "one write");
        assert_eq!(w.1, b"tulpa-hear: me: device X\n");
    }

    /// Each distinct line is said once, however often it is met. Kills: saying every time, or only the first line.
    #[test]
    fn warn_once_says_each_distinct_line_once() {
        let said = std::sync::Mutex::default();
        assert!(super::warn_once(&said, "test: warning: a"));
        assert!(!super::warn_once(&said, "test: warning: a"), "a retry");
        assert!(
            super::warn_once(&said, "test: warning: b"),
            "another failure"
        );
        assert!(!super::warn_once(&said, "test: warning: b"));
    }
}
