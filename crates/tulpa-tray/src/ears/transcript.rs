//! The capture's transcripts, and which of their lines leave the Mac.
//!
//! The capture's transcriber appends one line per utterance to its transcript
//! files, `HH:MM:SS [me|them] [tag] text` (UTC time, the optional one-word
//! speaker tag on the microphone's lines). [`Tail`] reads only what was
//! appended since it last looked, whole lines only, and copes with a file
//! that is truncated, rotated, or replaced: when the bytes just before its
//! read position are no longer the ones it read there, the file is new and is
//! read from its start. [`line_event`] turns a line that may go into the one
//! event that leaves the Mac; every other line stays here: the microphone's
//! lines always go, the system audio's only while a call runs (the capture's
//! mode is `call`; any other mode, or no capture, keeps them on the Mac).
//!
//! A capture told to (the tray sets `TULPA_LINES_STDOUT=1`) may also put its
//! lines on its stdout, one [`LINE_MARKER`] record per phrase, and write no
//! transcript files at all: [`marked_line`] takes such a record apart into
//! the phrase's own utterance span and a line in the same grammar, which goes
//! through the same [`line_event`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{EarsMode, Side};
use crate::log::format_utc;

/// The most characters of a line's text sent: the receiving side keeps no more.
pub const LINE_TEXT_MAX: usize = 4000;
/// How many of the last bytes read are kept to recognize a file as the same.
const SEEN_KEEP: u64 = 256;
/// The most bytes read from one file per look; the rest waits for the next.
const READ_MAX: u64 = 1 << 20;
/// A line longer than this without its newline is taken as it is.
const LINE_MAX_BYTES: usize = 64 * 1024;
/// The longest speaker tag, in characters.
const SPEAKER_MAX: usize = 32;
/// Seconds in a day.
const DAY: u64 = 86_400;
/// A line's own `HH:MM:SS` is its time only this close to now, in seconds:
/// the capture writes UTC, and a clock in any other zone is further off.
const LINE_TIME_SLACK: u64 = 300;
/// The prefix of a phrase record on the capture's stdout.
///
/// `TULPA-EARS-LINE <t0> <t1> <line>` (vany 2026-11-18, #0393: no files in
/// the ears' path): the utterance's own span in epoch ms first (the
/// segmenter's `t0`/`t1`; `0 0` when the capture could not say), then a line
/// in the transcript grammar. The tray tees every stdout line to its side
/// log and hands only these records to [`line_event`].
///
/// The line may carry one TAB and, after it, the phrase's word timings —
/// the owner, 2026-11-18, tags inside the stream: `<t0>,<t1>,<word>` records
/// separated by `;`, epoch ms like the span itself. [`MarkedLine::words`]
/// takes them apart; the text before the TAB is the whole of what is said.
pub const LINE_MARKER: &str = "TULPA-EARS-LINE ";
/// The prefix of a capture's stdout record for a phrase it heard and will not publish:
/// `TULPA-EARS-DROP <why> [<side>] <text, possibly empty>`. `why` is `prompt-echo` (whisper
/// echoing its vocabulary prompt on silence), `own-voice` (the speaker tagger heard our own
/// voice) or `no-transcript` (whisper left nothing to read). The tray only logs it to `flow.log`
/// ([`dropped_line`]); nothing of it crosses the link.
pub(super) const DROP_MARKER: &str = "TULPA-EARS-DROP ";

/// The most characters of a [`DROP_MARKER`] record's `why`.
const DROP_WHY_MAX: usize = 32;

/// Takes one stdout line of the capture apart as a [`DROP_MARKER`] record: the reason (one word of
/// lowercase letters and `-`, at most [`DROP_WHY_MAX`] characters) and the rest, `[<side>] <text>`.
/// Anything else is `None`.
#[must_use]
pub(super) fn dropped_line(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix(DROP_MARKER)?
        .trim_end_matches(['\n', '\r']);
    let (why, line) = rest.split_once(' ')?;
    let word = !why.is_empty()
        && why.chars().count() <= DROP_WHY_MAX
        && why.chars().all(|c| c.is_ascii_lowercase() || c == '-');
    word.then_some((why, line.trim()))
}

/// The most word records one phrase may carry; past it the timings are not
/// a phrase's and none of them go.
const WORDS_MAX: usize = 1024;
/// The most characters of one timed word: a token, never a sentence.
const WORD_MAX: usize = 64;
/// How far outside the utterance's own span a word's timing may lie, in ms:
/// the span and the words come from the same clock, but the whisper window
/// is padded a little.
const WORD_SPAN_SLACK: u64 = 2_000;
/// How much earlier than the call capture's start a system-audio line's own
/// time may read and still be the call's, in seconds: the capture's clock and
/// ours differ by a tick. A line from before that is a video's or a song's —
/// not the call's — and stays on the Mac.
const CALL_SINCE_SLACK: Duration = Duration::from_secs(1);

/// A transcript line taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLine<'a> {
    /// The line's `HH:MM:SS` as seconds of the day, when it starts with one.
    at: Option<u64>,
    side: Side,
    /// The speaker tag right after the side, when the line carries one.
    speaker: Option<&'a str>,
    /// What was said: the rest after the time, the side and a speaker tag.
    text: &'a str,
}

/// Takes `HH:MM:SS [me|them] [speaker] text` apart; the time and the speaker
/// tag may be missing. The speaker tag is one word in brackets right after the
/// side ([`is_speaker`]). Anything else in brackets there is said, not a tag,
/// and stays in the text: words with a space inside (`[laughs softly]`), any
/// other character, more than [`SPEAKER_MAX`] characters, `[]` or no `]`.
/// `None` without a `[me]` or `[them]` side.
fn parse_line(line: &str) -> Option<ParsedLine<'_>> {
    let line = line.trim_start();
    let (at, rest) = match line.get(..8).and_then(time_of_day) {
        Some(at) => (Some(at), line.get(8..)?.trim_start()),
        None => (None, line),
    };
    let (side, rest) = match rest.strip_prefix("[me]") {
        Some(rest) => (Side::Me, rest),
        None => (Side::Them, rest.strip_prefix("[them]")?),
    };
    let rest = rest.trim_start();
    let (speaker, text) = rest
        .strip_prefix('[')
        .and_then(|tagged| tagged.split_once(']'))
        .filter(|(tag, _)| is_speaker(tag))
        .map_or((None, rest), |(tag, after)| (Some(tag), after));
    Some(ParsedLine {
        at,
        side,
        speaker,
        text: text.trim(),
    })
}

/// Whether `tag` is a speaker tag: 1 to [`SPEAKER_MAX`] ASCII letters, digits,
/// `_` or `-`, such as `owner` or `other`.
fn is_speaker(tag: &str) -> bool {
    (1..=SPEAKER_MAX).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `HH:MM:SS` as seconds of the day; `None` for anything else.
fn time_of_day(stamp: &str) -> Option<u64> {
    let bytes = stamp.as_bytes();
    if bytes.len() != 8 || bytes.get(2) != Some(&b':') || bytes.get(5) != Some(&b':') {
        return None;
    }
    let field = |from: usize, below: u64| -> Option<u64> {
        stamp
            .get(from..from.checked_add(2)?)
            .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))?
            .parse::<u64>()
            .ok()
            .filter(|value| *value < below)
    };
    let (hours, minutes, seconds) = (field(0, 24)?, field(3, 60)?, field(6, 60)?);
    hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

/// The moment a line stamped `seconds_of_day` (UTC) was written: that time
/// of day yesterday, today or tomorrow, whichever lies within
/// [`LINE_TIME_SLACK`] of `now`; `None` when none does (a clock in another
/// zone, or an old line), and the caller takes `now`.
fn line_moment(seconds_of_day: u64, now: SystemTime) -> Option<SystemTime> {
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let today = now
        .checked_sub(now.rem_euclid(DAY))?
        .checked_add(seconds_of_day)?;
    [today.checked_sub(DAY), Some(today), today.checked_add(DAY)]
        .into_iter()
        .flatten()
        .find(|at| at.abs_diff(now) <= LINE_TIME_SLACK)
        .and_then(|at| UNIX_EPOCH.checked_add(Duration::from_secs(at)))
}

/// The event for one transcript line, or `None` when the line stays on the Mac.
///
/// The event is `{"event":"ears","state":"line","side":"me"|"them","mode":…,
/// "text":…,"ts":…}`, with `"speaker":…` too when the line carries a speaker
/// tag: `side` from the line's `[me]`/`[them]`; `speaker` the tag, one word of
/// 1 to 32 ASCII letters, digits, `_` or `-` in brackets right after the side
/// (anything else there is no tag: it stays in the text, and no `speaker`
/// goes); `text` what was said (without the time, side and speaker tag,
/// control characters turned into spaces, trimmed, cut to [`LINE_TEXT_MAX`]
/// characters); `mode` the ears mode's id; `ts` the line's own UTC time when
/// it lies within minutes of `now`, else `now`, as ISO-8601 UTC.
///
/// Which lines go is the owner's word of 2026-11-18: every line of the
/// microphone (`[me]`, his own speech) goes in any mode; a line of the system
/// audio (`[them]`) goes only while a call runs — the capture's mode is
/// `call`, whatever else was being heard, and the line was said no earlier
/// than that capture began (`call_since`), less [`CALL_SINCE_SLACK`]: a
/// `[them]` line the last mode read before the switch is a video's or a
/// song's, not the call's — and in any other mode it stays on the Mac: a
/// video or a song is nobody's words here. A line without a side or text
/// never goes.
#[must_use]
pub fn line_event(
    line: &str,
    mode: EarsMode,
    now: SystemTime,
    call_since: SystemTime,
) -> Option<Value> {
    line_verdict(line, mode, now, call_since).ok()
}

/// [`line_event`] with the reason a line stays on the Mac.
///
/// The reasons: `unparsed` (not in the transcript grammar, or no text left
/// after cleaning), `mode` (a `[them]` line outside a call, or from before it
/// began), `unshaped` (no event could be built from it).
///
/// # Errors
///
/// Returns the reason when the line does not go.
pub fn line_verdict(
    line: &str,
    mode: EarsMode,
    now: SystemTime,
    call_since: SystemTime,
) -> Result<Value, &'static str> {
    let heard = Heard::of(line, now).ok_or("unparsed")?;
    if !heard.goes(mode, call_since) {
        return Err("mode");
    }
    heard.event(mode).ok_or("unshaped")
}

/// One transcript line's side and cleaned text (#0423): the same reading
/// [`line_event`] does, without the mode filter.
///
/// A phrase that never left the Mac (paused, no capture, a replaced capture,
/// our own echo, the mode filter) is still something the ears heard, and the
/// log window shows it with its fate. `None` when the line is not a phrase
/// (no side or text): that is the filter's noise, not a dropped line, and
/// nothing is shown.
#[must_use]
pub(super) fn phrase(line: &str, now: SystemTime) -> Option<(Side, String)> {
    let heard = Heard::of(line, now)?;
    Some((heard.side, heard.text))
}

/// One [`LINE_MARKER`] record taken apart. The span is the utterance's own,
/// epoch ms — what the self-echo gate checks against the speaking windows,
/// not the moment the line was transcribed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkedLine<'a> {
    /// The utterance's start, epoch ms; 0 with `t1` when the capture could
    /// not say (then the tray falls back to the moment it read the line).
    pub t0: u64,
    /// The utterance's end, epoch ms.
    pub t1: u64,
    /// The line's side: only the microphone's can be the agent's own echo.
    pub side: Side,
    /// The line itself, after the two numbers, in the transcript grammar,
    /// without the word timings.
    pub line: &'a str,
    /// The word timings after the line's one TAB, when it carries them.
    words: Option<&'a str>,
}

impl MarkedLine<'_> {
    /// The phrase's word timings as the event's `words` array —
    /// `[{"t0":…,"t1":…,"w":…}, …]`, epoch ms like the span — or `None`
    /// when the record carries none, or they are not one phrase's: every
    /// record must be three fields (`t0,t1,word`, the word one token of at
    /// most [`WORD_MAX`] characters, its timing inside the utterance's span
    /// within [`WORD_SPAN_SLACK`]), and a phrase carries at most
    /// [`WORDS_MAX`] of them. Anything else loses the whole payload, never
    /// part of it: half the timings would tell the receiver where nothing
    /// was said.
    #[must_use]
    pub fn words(&self) -> Option<Vec<Value>> {
        let payload = self.words?;
        let mut out = Vec::new();
        for record in payload.split(';') {
            let (t0, rest) = record.split_once(',')?;
            let (t1, word) = rest.split_once(',')?;
            if word.contains(';') || word.contains('\t') {
                return None; // a record is exactly three fields on this wire
            }
            let t0: u64 = t0.parse().ok()?;
            let t1: u64 = t1.parse().ok()?;
            if t1 < t0
                || word.is_empty()
                || word.chars().count() > WORD_MAX
                || word.chars().any(char::is_control)
            {
                return None;
            }
            if self.t0 != 0
                && self.t1 != 0
                && (t0.saturating_add(1) < self.t0.saturating_sub(WORD_SPAN_SLACK)
                    || t0 > self.t1.saturating_add(WORD_SPAN_SLACK))
            {
                return None; // not this utterance's clock
            }
            out.push(json!({"t0": t0, "t1": t1, "w": word}));
        }
        if out.is_empty() || out.len() > WORDS_MAX {
            return None;
        }
        Some(out)
    }
}

/// Takes one stdout line of the capture apart as a [`LINE_MARKER`] record.
///
/// Any other stdout line, and a record whose line is not in the transcript
/// grammar, is `None` (it still went to the side log, whoever wrote it).
/// What follows the line's one TAB is the word timings ([`MarkedLine::words`]),
/// no part of the line itself.
#[must_use]
pub fn marked_line(text: &str) -> Option<MarkedLine<'_>> {
    let rest = text
        .strip_prefix(LINE_MARKER)?
        .trim_end_matches(['\n', '\r'])
        .trim_start();
    let (t0, rest) = rest.split_once(char::is_whitespace)?;
    let t1 = rest.trim_start();
    let (t1, line) = t1.split_once(char::is_whitespace)?;
    let t0: u64 = t0.parse().ok()?;
    let t1: u64 = t1.parse().ok()?;
    let (line, words) = match line.split_once('\t') {
        Some((line, words)) => (line, Some(words.trim())),
        None => (line, None),
    };
    let line = line.trim();
    let side = parse_line(line)?.side;
    Some(MarkedLine {
        t0,
        t1,
        side,
        line,
        words,
    })
}

/// A transcript line with a side and text, taken apart and cleaned.
#[derive(Debug)]
struct Heard<'a> {
    side: Side,
    /// The speaker tag, when the line carries one.
    speaker: Option<&'a str>,
    /// What was said: control characters turned into spaces, trimmed; never empty.
    text: String,
    /// When it was said: the line's own UTC time when that lies near the
    /// time it was read ([`line_moment`]), else the time it was read. The
    /// event's `ts`.
    at: SystemTime,
}

impl<'a> Heard<'a> {
    /// `line`, read at `now`; `None` without a side or text.
    fn of(line: &'a str, now: SystemTime) -> Option<Self> {
        let parsed = parse_line(line)?;
        let clean: String = parsed
            .text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let text = clean.trim();
        if text.is_empty() {
            return None;
        }
        Some(Self {
            side: parsed.side,
            speaker: parsed.speaker,
            text: text.to_owned(),
            at: parsed.at.and_then(|at| line_moment(at, now)).unwrap_or(now),
        })
    }

    /// Whether the line goes in `mode`: the microphone's always, the system
    /// audio's only in a call that had already begun (`call_since`) when the
    /// line was said. No other rule lets a line leave the Mac.
    fn goes(&self, mode: EarsMode, call_since: SystemTime) -> bool {
        match self.side {
            Side::Me => true,
            Side::Them => {
                mode == EarsMode::Call
                    && self.at
                        >= call_since
                            .checked_sub(CALL_SINCE_SLACK)
                            .unwrap_or(call_since)
            }
        }
    }

    /// The line's event in `mode`.
    fn event(&self, mode: EarsMode) -> Option<Value> {
        said_event(self.side, self.speaker, &self.text, mode, self.at)
    }
}

/// The event for a test line (`ears.test`), marked `"test":true`.
///
/// `text` is taken as the microphone's line in `mode` at `now`, shaped and
/// cleaned as [`line_event`] does; it goes whatever the mode filter. `None`
/// when no text is left after cleaning.
#[must_use]
pub fn test_line_event(text: &str, mode: EarsMode, now: SystemTime) -> Option<Value> {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = clean.trim();
    if text.is_empty() {
        return None;
    }
    let mut event = said_event(Side::Me, None, text, mode, now)?;
    event
        .as_object_mut()?
        .insert("test".to_owned(), Value::Bool(true));
    Some(event)
}

/// The line event for cleaned `text` said on `side` at `at`, by `speaker` when
/// the line was tagged, the text cut to [`LINE_TEXT_MAX`] characters.
fn said_event(
    side: Side,
    speaker: Option<&str>,
    text: &str,
    mode: EarsMode,
    at: SystemTime,
) -> Option<Value> {
    let text: String = text.chars().take(LINE_TEXT_MAX).collect();
    let ts = format_utc(at)?;
    let mut event = json!({
        "event": "ears",
        "state": "line",
        "side": side.id(),
        "mode": mode.id(),
        "text": text,
        "ts": ts,
    });
    if let Some(speaker) = speaker {
        event
            .as_object_mut()?
            .insert("speaker".to_owned(), Value::from(speaker));
    }
    Some(event)
}

/// One watched transcript file.
#[derive(Debug)]
struct Watched {
    path: PathBuf,
    /// The next unread byte: just past the last whole line read.
    pos: u64,
    /// The last bytes before `pos` (up to [`SEEN_KEEP`]), as they were read.
    seen: Vec<u8>,
}

/// Tails transcript files for the lines appended to them.
#[derive(Debug)]
pub(super) struct Tail {
    files: Vec<Watched>,
}

impl Tail {
    /// Starts watching `paths`. What a file holds now is old and never
    /// returned; a file that does not exist yet is new from its first byte.
    pub(super) fn new(paths: &[PathBuf]) -> Self {
        let files = paths
            .iter()
            .map(|path| {
                let (pos, seen) = File::open(path)
                    .ok()
                    .and_then(|mut file| {
                        let len = file.metadata().ok()?.len();
                        let seen = read_before(&mut file, len).ok()?;
                        Some((len, seen))
                    })
                    .unwrap_or_default();
                Watched {
                    path: path.clone(),
                    pos,
                    seen,
                }
            })
            .collect();
        Self { files }
    }

    /// The whole lines appended to the files since the last look, each file's
    /// in order, without their line ends. A line still being written (no
    /// newline yet) waits for the next look.
    pub(super) fn poll(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for watched in &mut self.files {
            watched.read_new(&mut lines);
        }
        lines
    }
}

impl Watched {
    fn read_new(&mut self, lines: &mut Vec<String>) {
        // A missing file (deleted, being rotated) keeps its place until one is back.
        let Ok(mut file) = File::open(&self.path) else {
            return;
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return;
        };
        let same = len >= self.pos
            && read_before(&mut file, self.pos).is_ok_and(|bytes| bytes == self.seen);
        if !same {
            // Truncated, rotated, or rewritten: everything in it is new.
            self.pos = 0;
            self.seen.clear();
        }
        let want = len.saturating_sub(self.pos).min(READ_MAX);
        if want == 0 {
            return;
        }
        let mut chunk = Vec::new();
        if file.seek(SeekFrom::Start(self.pos)).is_err()
            || file.by_ref().take(want).read_to_end(&mut chunk).is_err()
        {
            return;
        }
        let mut used: usize = 0;
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            if piece.last() != Some(&b'\n') && piece.len() <= LINE_MAX_BYTES {
                break;
            }
            used = used.saturating_add(piece.len());
            let text = String::from_utf8_lossy(piece);
            lines.push(text.trim_end_matches(['\n', '\r']).to_owned());
        }
        self.pos = self
            .pos
            .saturating_add(u64::try_from(used).unwrap_or(u64::MAX));
        self.seen
            .extend_from_slice(chunk.get(..used).unwrap_or_default());
        let keep = usize::try_from(SEEN_KEEP).unwrap_or(usize::MAX);
        let extra = self.seen.len().saturating_sub(keep);
        self.seen.drain(..extra);
    }
}

/// The bytes of `file` just before `pos`, up to [`SEEN_KEEP`] of them.
fn read_before(file: &mut File, pos: u64) -> std::io::Result<Vec<u8>> {
    let from = pos.saturating_sub(SEEN_KEEP);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(pos.saturating_sub(from))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "a record that fails to parse fails its own test right there, at the record"
    )]

    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use std::time::SystemTime;

    use super::*;

    fn temp_dir(name: &str) -> std::io::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "tulpa-transcript-test-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn append(path: &Path, text: &str) -> std::io::Result<()> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(text.as_bytes())
    }

    const fn parsed<'a>(
        at: Option<u64>,
        side: Side,
        speaker: Option<&'a str>,
        text: &'a str,
    ) -> ParsedLine<'a> {
        ParsedLine {
            at,
            side,
            speaker,
            text,
        }
    }

    #[test]
    fn a_drop_record_is_its_reason_and_the_rest() {
        assert_eq!(
            dropped_line("TULPA-EARS-DROP prompt-echo [me] спасибо, за просмотр\n"),
            Some(("prompt-echo", "[me] спасибо, за просмотр"))
        );
        assert_eq!(
            dropped_line("TULPA-EARS-DROP own-voice [me] \n"),
            Some(("own-voice", "[me]"))
        );
        for noise in [
            "TULPA-EARS-DROP Bad [me] x",
            "TULPA-EARS-DROP  [me] x",
            "TULPA-EARS-DROP no-space-after-why",
            "TULPA-EARS-LINE 1 2 12:00:00 [me] x",
            "ears-capture: started",
        ] {
            assert_eq!(dropped_line(noise), None, "{noise}");
        }
        let long = format!("TULPA-EARS-DROP {} [me] x", "a".repeat(DROP_WHY_MAX + 1));
        assert_eq!(dropped_line(&long), None);
    }

    /// 2026-11-15T12:00:10Z.
    fn noon() -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(1_794_744_010))
            .unwrap_or(UNIX_EPOCH)
    }

    /// A call these tests' lines (stamped 12:00:04) were said well into: the
    /// gate under test is the call's start time, so the default says the call
    /// has been running for an hour.
    fn earlier_call() -> SystemTime {
        noon()
            .checked_sub(Duration::from_secs(3_600))
            .unwrap_or_else(noon)
    }

    /// A stdout phrase record carries the utterance's span, and its line is
    /// the same grammar the transcript files hold; anything else a capture
    /// may print is not one.
    #[test]
    fn a_stdout_record_gives_the_span_and_the_line() {
        let marked = marked_line(
            "TULPA-EARS-LINE 1794744010000 1794744012500 12:00:10 [me] [vany] привет\r\n",
        );
        assert_eq!(
            marked,
            Some(MarkedLine {
                t0: 1_794_744_010_000,
                t1: 1_794_744_012_500,
                side: Side::Me,
                line: "12:00:10 [me] [vany] привет",
                words: None,
            })
        );
        let unknown = marked_line("TULPA-EARS-LINE 0 0 [them] from the system audio\n");
        assert_eq!(
            unknown,
            Some(MarkedLine {
                t0: 0,
                t1: 0,
                side: Side::Them,
                line: "[them] from the system audio",
                words: None,
            }),
            "no stamp and no span: the line itself is enough"
        );
        for other in [
            "12:00:10 [me] a transcript file's line, no marker\n",
            "TULPA-EARS-LINE not-a-number 5 12:00:10 [me] x\n",
            "TULPA-EARS-LINE 5\n",
            "TULPA-EARS-LINE 1 2 \n",
            "TULPA-EARS-LINE 1 2 [we] neither side\n",
            "ears-capture: them=sysaudio (self-restarting)\n",
        ] {
            assert_eq!(marked_line(other), None, "{other:?}");
        }
    }

    /// A record's word timings (the owner, 2026-11-18: tags inside the stream) sit
    /// after the line's one TAB, epoch ms like the span, and reach the event
    /// as one array — the text before the TAB is the whole of what was said.
    #[test]
    fn a_records_word_timings_leave_with_the_line() {
        let marked = marked_line(
            "TULPA-EARS-LINE 1794744010000 1794744012500 12:00:10 [me] привет\t\
             1794744010050,1794744010420,при;1794744010500,1794744010900,вет\n",
        )
        .expect("a record with timings");
        assert_eq!(marked.line, "12:00:10 [me] привет", "the TAB ends the line");
        assert_eq!(
            marked.words(),
            Some(vec![
                json!({"t0": 1_794_744_010_050_u64, "t1": 1_794_744_010_420_u64, "w": "при"}),
                json!({"t0": 1_794_744_010_500_u64, "t1": 1_794_744_010_900_u64, "w": "вет"}),
            ])
        );
        // Punctuation is a timed token too, commas included (a record is
        // three fields: the third is the rest).
        let comma = marked_line("TULPA-EARS-LINE 1000 3000 [me] да ,\t1100,1200,да;1500,1600,,\n")
            .expect("a record with punctuation");
        assert_eq!(
            comma.words(),
            Some(vec![
                json!({"t0": 1100, "t1": 1200, "w": "да"}),
                json!({"t0": 1500, "t1": 1600, "w": ","}),
            ])
        );
        // No span to check the timings against: they go as they are.
        let no_span = marked_line("TULPA-EARS-LINE 0 0 [me] x\t10,20,x\n").expect("no span");
        assert_eq!(
            no_span.words(),
            Some(vec![json!({"t0": 10, "t1": 20, "w": "x"})])
        );
    }

    /// Timings that are not one phrase's lose the whole payload, never part
    /// of it: half the timings would tell the receiver where nothing was
    /// said. The line itself is untouched — the words ride beside it, not in
    /// it.
    #[test]
    fn timings_that_are_not_one_phrases_lose_the_whole_payload() {
        let bad = |payload: &str| {
            marked_line(&format!("TULPA-EARS-LINE 1000 3000 [me] текст\t{payload}"))
                .expect("the record parses")
                .words()
        };
        assert_eq!(bad(""), None, "an empty payload is no timings");
        assert_eq!(bad("1100"), None, "one field");
        assert_eq!(bad("1100,1200"), None, "two fields");
        assert_eq!(bad("1100,1200,"), None, "no word");
        assert_eq!(bad("x100,1200,да"), None, "not epoch ms");
        assert_eq!(bad("1200,1100,да"), None, "ends before it starts");
        // The span check needs room to stand outside: a span away from the
        // clock's zero, where more than the slack out is another phrase.
        let far = |payload: &str| {
            marked_line(&format!(
                "TULPA-EARS-LINE 100000 101000 [me] текст\t{payload}"
            ))
            .expect("the record parses")
            .words()
        };
        assert_eq!(
            far("97000,97500,да"),
            None,
            "before the utterance's span by more than the slack"
        );
        assert_eq!(
            far("103100,103500,да"),
            None,
            "past the utterance's span by more than the slack"
        );
        assert_eq!(
            bad("1000,1100,a;b"),
            None,
            "a record separator inside a word"
        );
        assert_eq!(
            bad(&format!("1000,1100,{}", "д".repeat(65))),
            None,
            "not a token"
        );
        let many: Vec<String> = (0..1025)
            .map(|i| format!("{},1000,w{}", 1000 + i, i))
            .collect();
        assert_eq!(bad(&many.join(";")), None, "not one phrase's worth");
        // Within the slack of the span is the phrase's clock, whatever the
        // words: whisper's window is padded.
        assert_eq!(
            bad("950,1050,да").map_or(0, |w| w.len()),
            1,
            "inside the slack goes"
        );
    }

    #[test]
    fn only_new_whole_lines_are_read_and_each_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("append")?;
        let file = dir.join("me.test.txt");
        fs::write(&file, "12:00:00 [me] old line\n")?;
        let mut tail = Tail::new(std::slice::from_ref(&file));
        assert_eq!(tail.poll(), Vec::<String>::new(), "what was there is old");
        append(&file, "12:00:04 [me] first\r\n12:00:08 [me] sec")?;
        assert_eq!(tail.poll(), vec!["12:00:04 [me] first"]);
        assert_eq!(
            tail.poll(),
            Vec::<String>::new(),
            "a line is read once; the unfinished one waits"
        );
        append(&file, "ond\n")?;
        assert_eq!(tail.poll(), vec!["12:00:08 [me] second"]);
        assert_eq!(tail.poll(), Vec::<String>::new());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_file_born_later_is_new_from_its_first_byte() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("born")?;
        let (me, them) = (dir.join("me.txt"), dir.join("them.txt"));
        let mut tail = Tail::new(&[me.clone(), them.clone()]);
        assert_eq!(tail.poll(), Vec::<String>::new());
        append(&them, "a\nb\n")?;
        append(&me, "c\n")?;
        assert_eq!(
            tail.poll(),
            vec!["c", "a", "b"],
            "each file's lines in order"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn truncation_rotation_and_replacement_are_followed() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = temp_dir("rotate")?;
        let file = dir.join("me.txt");
        fs::write(&file, "one\ntwo\n")?;
        let mut tail = Tail::new(std::slice::from_ref(&file));
        // Truncated in place, then written again: all of it is new.
        fs::write(&file, "")?;
        assert_eq!(tail.poll(), Vec::<String>::new());
        append(&file, "three\n")?;
        assert_eq!(tail.poll(), vec!["three"]);
        // Truncated and rewritten past the old position between two looks.
        fs::write(&file, "four, a longer line than before\n")?;
        assert_eq!(tail.poll(), vec!["four, a longer line than before"]);
        // Rotated: moved away, a new file in its place.
        fs::rename(&file, dir.join("me.txt.1"))?;
        assert_eq!(tail.poll(), Vec::<String>::new(), "gone for a moment");
        append(&dir.join("me.txt.1"), "late line in the old file\n")?;
        fs::write(&file, "five\n")?;
        assert_eq!(tail.poll(), vec!["five"]);
        // Replaced by a copy with the same lines and a new one (an atomic rewrite): only the new one.
        let copy = dir.join("me.txt.tmp");
        fs::write(&copy, "five\nsix\n")?;
        fs::rename(&copy, &file)?;
        assert_eq!(tail.poll(), vec!["six"], "a line is never read twice");
        assert_eq!(tail.poll(), Vec::<String>::new());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_runaway_line_without_newline_is_taken_whole() -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_dir("runaway")?;
        let file = dir.join("me.txt");
        let mut tail = Tail::new(std::slice::from_ref(&file));
        let long = "x".repeat(LINE_MAX_BYTES.saturating_add(1));
        fs::write(&file, &long)?;
        assert_eq!(tail.poll(), vec![long]);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_line_is_taken_apart_into_time_side_speaker_and_text() {
        let at = Some(12 * 3_600 + 4);
        assert_eq!(
            parse_line("12:00:04 [me] [owner] hello there "),
            Some(parsed(at, Side::Me, Some("owner"), "hello there")),
            "the speaker tag is not part of the text"
        );
        assert_eq!(
            parse_line("12:00:04 [them] hello"),
            Some(parsed(at, Side::Them, None, "hello"))
        );
        assert_eq!(
            parse_line("12:00:04 [me][Other_2-b]hi"),
            Some(parsed(at, Side::Me, Some("Other_2-b"), "hi")),
            "taken as written"
        );
        assert_eq!(
            parse_line("[them] [other] no time given"),
            Some(parsed(None, Side::Them, Some("other"), "no time given"))
        );
        assert_eq!(
            parse_line("12:00:04 [me] [owner]"),
            Some(parsed(at, Side::Me, Some("owner"), ""))
        );
        let longest = "x".repeat(SPEAKER_MAX);
        assert_eq!(
            parse_line(&format!("12:00:04 [me] [{longest}] hi")),
            Some(parsed(at, Side::Me, Some(&longest), "hi"))
        );
        let too_long = format!("[{longest}x] hi");
        for said in [
            "[laughs softly] fine",
            "[own:er] hi",
            "[владелец] hi",
            "[] hi",
            "[owner hi",
            "[[owner]] hi",
            &too_long,
        ] {
            assert_eq!(
                parse_line(&format!("12:00:04 [me] {said}")),
                Some(parsed(at, Side::Me, None, said)),
                "{said:?} is no speaker tag: said, it stays in the text"
            );
        }
        for no_side in [
            "12:00:04 no side here",
            "12:00:04 [meow] hi",
            "25:00:00 [me] not a time, so no side first",
            "",
        ] {
            assert_eq!(parse_line(no_side), None, "{no_side:?}");
        }
    }

    #[test]
    fn the_line_time_is_taken_only_near_now() {
        let at = |stamp: &str, now: SystemTime| {
            time_of_day(stamp)
                .and_then(|s| line_moment(s, now))
                .and_then(format_utc)
        };
        assert_eq!(
            at("12:00:04", noon()).as_deref(),
            Some("2026-11-15T12:00:04Z")
        );
        let past_midnight = UNIX_EPOCH
            .checked_add(Duration::from_secs(1_794_787_201))
            .unwrap_or(UNIX_EPOCH);
        assert_eq!(
            at("23:59:58", past_midnight).as_deref(),
            Some("2026-11-15T23:59:58Z"),
            "a line from just before midnight is yesterday's"
        );
        assert_eq!(
            at("15:00:04", noon()),
            None,
            "hours off: not UTC, now is taken"
        );
        for bad in ["12:0:04", "12-00-04", "24:00:00", "12:60:00", "1200:04 "] {
            assert_eq!(time_of_day(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_test_line_event_is_the_microphone_line_marked_as_a_test()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            test_line_event(
                " 12:00:04 [them] [owner] hello\nthere ",
                EarsMode::Call,
                noon()
            ),
            Some(
                json!({"event": "ears", "state": "line", "side": "me", "mode": "call",
                "text": "12:00:04 [them] [owner] hello there", "ts": "2026-11-15T12:00:10Z",
                "test": true})
            ),
            "the text is taken as said: no time, side or tag is read off it"
        );
        let long = "я".repeat(LINE_TEXT_MAX.saturating_add(1));
        let sent = test_line_event(&long, EarsMode::Off, noon()).ok_or("a long test goes")?;
        assert_eq!(
            sent.pointer("/text")
                .and_then(Value::as_str)
                .map(|text| text.chars().count()),
            Some(LINE_TEXT_MAX),
            "capped"
        );
        assert_eq!(sent.pointer("/mode"), Some(&json!("off")));
        for empty in ["", " \t\n "] {
            assert_eq!(
                test_line_event(empty, EarsMode::Alone, noon()),
                None,
                "{empty:?}"
            );
        }
        Ok(())
    }

    /// The microphone's line goes in every mode, marked by nothing but its own
    /// side; the system audio's goes only in a call — in any other mode it
    /// stays on the Mac, whatever it says (vany's word of 2026-11-18).
    #[test]
    fn the_microphones_line_always_goes_and_the_system_audio_needs_a_call() {
        let event = |line: &str, mode: EarsMode| line_event(line, mode, noon(), earlier_call());
        for mode in EarsMode::ALL {
            assert_eq!(
                event("12:00:04 [me] [owner] anything said", mode)
                    .and_then(|e| e.pointer("/side").cloned()),
                Some(json!("me")),
                "{mode:?}: the microphone's line goes"
            );
        }
        assert_eq!(
            event("12:00:04 [them] the other side of the call", EarsMode::Call)
                .and_then(|e| e.pointer("/side").cloned()),
            Some(json!("them")),
            "in a call the system audio's line goes"
        );
        for mode in [
            EarsMode::Off,
            EarsMode::Office,
            EarsMode::Alone,
            EarsMode::Meeting,
            EarsMode::Stream,
        ] {
            assert!(
                event("12:00:04 [them] a video, a song, a meeting", mode).is_none(),
                "{mode:?}: outside a call the system audio stays"
            );
        }
    }

    /// The call's own only: a `[them]` line said before the call capture
    /// began stays local even when read after the mode turned to a call —
    /// the words the last mode already heard are a video's or a song's, not
    /// the call's — while a line said as the call began, inside the
    /// since-slack, goes.
    #[test]
    fn the_system_audios_line_needs_a_call_that_had_begun() {
        let said = "12:00:04 [them] words"; // said at 12:00:04, noon is 12:00:10
        assert_eq!(
            line_event(said, EarsMode::Call, noon(), noon()),
            None,
            "the call began six seconds after the words: they stay"
        );
        let begun = UNIX_EPOCH
            .checked_add(Duration::from_secs(1_794_744_004))
            .unwrap_or_else(noon); // 12:00:04: the call began as the words were said
        assert_eq!(
            line_event(said, EarsMode::Call, noon(), begun)
                .and_then(|e| e.pointer("/side").cloned()),
            Some(json!("them")),
            "said within the since-slack of the call's start: goes"
        );
        let just_before = UNIX_EPOCH
            .checked_add(Duration::from_secs(1_794_744_004))
            .unwrap_or_else(noon); // 12:00:04: the call began then
        assert!(
            line_event("12:00:03 [them] words", EarsMode::Call, noon(), just_before).is_some(),
            "a line's stamp may read a tick before the capture that heard it"
        );
    }

    /// The event carries the receiving shape — nothing of the old rule:
    /// `side`, `mode`, `text`, `ts`, and `speaker` when the line carries a tag.
    #[test]
    fn a_line_event_has_the_receiving_shape() -> Result<(), Box<dyn std::error::Error>> {
        let event = |line: &str, mode: EarsMode| line_event(line, mode, noon(), earlier_call());
        assert_eq!(
            event("12:00:04 [me] [owner] Тульпа, ты тут?", EarsMode::Call),
            Some(
                json!({"event": "ears", "state": "line", "side": "me", "speaker": "owner",
                "mode": "call", "text": "Тульпа, ты тут?", "ts": "2026-11-15T12:00:04Z"})
            )
        );
        assert_eq!(
            event("15:00:04 [me] nothing said to us", EarsMode::Stream),
            Some(
                json!({"event": "ears", "state": "line", "side": "me", "mode": "stream",
                "text": "nothing said to us", "ts": "2026-11-15T12:00:10Z"})
            ),
            "a time hours off is not the line's: now"
        );
        assert_eq!(
            event("12:00:04 [me] [owner]  ", EarsMode::Alone),
            None,
            "no text"
        );
        assert_eq!(
            event("12:00:04 tulpa without a side", EarsMode::Alone),
            None
        );
        let long = format!("12:00:04 [them] {}", "я".repeat(LINE_TEXT_MAX + 1));
        let sent = event(&long, EarsMode::Call).ok_or("a call line goes")?;
        let text = sent
            .pointer("/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(text.chars().count(), LINE_TEXT_MAX, "capped");
        Ok(())
    }

    /// A tagged line's event carries the tag as `speaker` and the text without it; an untagged one
    /// has no `speaker` and the text as before; a malformed tag is no tag: it stays in the text, its
    /// words count as said, and no `speaker` goes.
    #[test]
    fn a_speaker_tag_goes_as_the_speaker_and_a_malformed_one_stays_in_the_text() {
        let event = |line: &str, mode: EarsMode| line_event(line, mode, noon(), earlier_call());
        assert_eq!(
            event("12:00:04 [me] [owner] hello there", EarsMode::Alone),
            Some(
                json!({"event": "ears", "state": "line", "side": "me", "speaker": "owner",
                "mode": "alone", "text": "hello there", "ts": "2026-11-15T12:00:04Z"})
            ),
            "tagged"
        );
        assert_eq!(
            event("12:00:04 [them] [other] hello", EarsMode::Call),
            Some(
                json!({"event": "ears", "state": "line", "side": "them", "speaker": "other",
                "mode": "call", "text": "hello", "ts": "2026-11-15T12:00:04Z"})
            ),
            "tagged, on the other side, in a call"
        );
        assert_eq!(
            event("12:00:04 [them] [other] hello", EarsMode::Meeting),
            None,
            "the other side outside a call stays, tag or not"
        );
        assert_eq!(
            event("12:00:04 [me] [own:er] hello there", EarsMode::Office),
            Some(
                json!({"event": "ears", "state": "line", "side": "me", "mode": "office",
                "text": "[own:er] hello there", "ts": "2026-11-15T12:00:04Z"})
            ),
            "malformed: left in the text, no speaker"
        );
    }
}
