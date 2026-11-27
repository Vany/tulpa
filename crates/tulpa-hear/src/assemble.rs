//! The line assembler: [`Heard`] utterances to the tray's `TULPA-EARS-LINE`s, in memory.
//!
//! The lines are byte-compatible with those `ears-capture.sh` prints (its awk assembler and its per-utterance loop,
//! ported one to one).
//!
//! - [`piece`]: one utterance's tokens to its text and word-timing payload. whisper's own marker tokens
//!   (`[_BEG_]`, `[_TT_39]`, `[_EOT_]`) are dropped first; subword pieces merge into words (a piece starting with a
//!   space, or the first, begins one); a gap of [`PAUSE_MS`] or more between pieces is `<pause N.Ns>` in the text
//!   and closes the word; a piece carrying `;` or a TAB (the payload's own separators) keeps the text and loses the
//!   whole payload. Each of whisper's lines that is a credit leak ([`credit_leak`]) is dropped by itself, from the
//!   text and the payload alike (a silence filler only with a piece of nothing else); then whisper's segment loop at the utterance's tail is cut ([`unlooped`]: from the
//!   loop's zone on, each segment text publishes once). The repeat-run bound counts before both. An utterance with no text, one with nothing left once its leak lines are dropped,
//!   or one with a token repeated [`REPEAT_BOUND`] times in a row is `<voice N.Ns>`: heard, not read, and the rule
//!   that refused it is kept ([`Unread`]). One with no token at all (whisper's VAD found no speech) is
//!   `<no speech N.Ns>` instead: nothing says it was voice. Here alone the script differs: it prints `<voice N.Ns>`.
//! - [`Assembler`]: per side, the turn hold — an utterance the segmenter cut at its maximum length (not at a
//!   pause) is held and merged with the next until [`HOLD_MAX_MS`] of held speech — then the prompt-pair echo
//!   filter ([`prompt_pairs`]), the intonation tag, and the line. Every phrase it lets go (emitted or dropped) is one
//!   line of `<EARS_STATE>/heard.log`, created `0600`; so is every unread piece of a real utterance (at least
//!   [`RETRY_MIN_MS`]), named with its rule and counts (`no-speech` and counts for no token at all). That log is
//!   the only file written. Per side it also keeps the deaf-suspect streak: [`DEAF_S`] seconds of consecutive
//!   pieces unread as `markers-only` or `empty-text` give one `<deaf-suspect <rule> <N>s>` line; a piece read as
//!   text re-arms it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tulpa_segment::Contour;

use crate::hear::{Heard, Side};
use crate::log::{BOUND, append, rotate_past};
use crate::recognize::{RETRY_MIN_MS, Token, Transcript};

/// A gap between two pieces of at least this (ms) is a `<pause N.Ns>`.
pub const PAUSE_MS: u64 = 500;
/// A token repeated this many times in a row is a decode loop, not speech; five is still emphatic speech.
pub const REPEAT_BOUND: usize = 6;
/// A segment text with this many copies in one utterance, the last of them in its last [`LOOP_TAIL`] segments, is
/// whisper's decoder looping on its own context, not speech. See [`unlooped`].
///
/// One live line (2026-11-25) ended in a loop: one 4-word segment 13 times and another 3 times, interleaved, at
/// rigid 1 s / 2 s spans, up to the line's end. Twice is real speech (a repeated phrase); so is a triple in the
/// middle of a line, which the loop's tail anchor keeps.
pub const SEGMENT_LOOP: usize = 3;
/// A loop reaches the utterance's tail: its last copy is among this many last segments (those with text).
pub const LOOP_TAIL: usize = 2;
/// A loop's zone starts at its first copy whose next copy is at most this many segments (with text) further on.
///
/// The live loop's copies were 1 to 5 apart; copies further apart, before that, are speech and stay.
pub const LOOP_SPACING: usize = 6;
/// The default bound (ms) on held speech (`EARS_HOLD_MAX_MS`): a turn that never pauses ships here, mid-phrase.
pub const HOLD_MAX_MS: u64 = 300_000;
/// The default deaf-suspect streak (`EARS_DEAF_S`), seconds.
///
/// This much consecutive audio unread as [`Why::MarkersOnly`] or [`Why::EmptyText`] on one side is one
/// `<deaf-suspect …>` line (2026-11-22: whisper went deaf for an hour while the pipe looked healthy, every piece
/// `<voice>`). Room noise (`no-tokens`) and music (`credit-leak`, `maxrun`) do not count.
pub const DEAF_S: u64 = 300;
/// The log's name in the state dir.
pub const HEARD_LOG: &str = "heard.log";
/// The cut mode heard.log names: the in-process segmenter always cuts on pauses.
const CUT_MODE: &str = "pauses";

/// One utterance assembled, as the script's awk prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Piece {
    /// The text, with `<pause N.Ns>` tags; or `<voice N.Ns>` / `<no speech N.Ns>` alone ([`Why`] picks which).
    pub text: String,
    /// The word timings, `t0,t1,word` records joined by `;` (epoch ms); empty for an unread piece and for a text
    /// with a token carrying the wire's own separators.
    pub payload: String,
    /// Why the text is a placeholder of unread audio; `None` when it was read.
    pub unread: Option<Unread>,
    /// How many of whisper's lines were credit leaks ([`credit_leak`]), dropped from the text and the payload.
    pub leaks: usize,
    /// The family of each dropped credit whose recital went on in words the list doesn't cover, those words kept as
    /// speech (#0454 B1 P6): how the list grows. In order, one per such line.
    pub flagged: Vec<&'static str>,
    /// The text the piece would have, placeholder included, with no credit dropped: what the prompt-echo rule reads,
    /// before the credit rule (#0454 B1 D7: it compares against the prompt itself, the more specific rule). The same
    /// as `text` when no line was dropped.
    pub heard: String,
}

/// The rule that left a piece unread, as the capture script's heard.log names it: `<voice N.Ns>`, or
/// `<no speech N.Ns>` for [`Why::NoTokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Why {
    /// whisper gave no token at all (its VAD found no speech): mostly room noise.
    NoTokens,
    /// Only whisper's own markers, no text token: the deaf signature.
    MarkersOnly,
    /// Text tokens that trim to nothing.
    EmptyText,
    /// A whisper credit leak ([`credit_leak`]): music.
    CreditLeak,
    /// A token repeated [`REPEAT_BOUND`] times in a row: a decode loop.
    MaxRun,
}

impl Why {
    /// The name heard.log and the deaf-suspect line use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoTokens => "no-tokens",
            Self::MarkersOnly => "markers-only",
            Self::EmptyText => "empty-text",
            Self::CreditLeak => "credit-leak",
            Self::MaxRun => "maxrun",
        }
    }

    /// Whether it counts towards the deaf-suspect streak.
    const fn deaf(self) -> bool {
        matches!(self, Self::MarkersOnly | Self::EmptyText)
    }

    /// Whether anything says the piece was voice: every rule but [`Why::NoTokens`], whose VAD found no speech.
    pub(crate) const fn heard_voice(self) -> bool {
        !matches!(self, Self::NoTokens)
    }

    /// The piece's text for `ms` of unread audio: `<voice N.Ns>`, or `<no speech N.Ns>` when nothing says it was
    /// voice (2026-11-24: room noise as `<voice>` read as deaf ears to the human and the agent alike).
    fn placeholder(self, ms: u64) -> String {
        let tag = if self.heard_voice() {
            VOICE_TAG
        } else {
            NO_SPEECH_TAG
        };
        format!("{tag}{}s>", tenths(ms))
    }

    /// What heard.log calls audio this rule left unread: `voice-unread`, or `no-speech` when nothing says it was
    /// voice.
    const fn label(self) -> &'static str {
        if self.heard_voice() {
            "voice-unread"
        } else {
            "no-speech"
        }
    }

    /// heard.log's outcome for a phrase that starts with a piece this rule left unread.
    fn outcome(self) -> String {
        format!("emitted:{}", self.label())
    }
}

/// How the placeholders of unread audio open ([`Why`] picks one): heard but not read, and no speech found.
const VOICE_TAG: &str = "<voice ";
/// See [`VOICE_TAG`].
const NO_SPEECH_TAG: &str = "<no speech ";
/// Both, for a reader that tells a placeholder from words.
pub(crate) const UNREAD_TAGS: [&str; 2] = [VOICE_TAG, NO_SPEECH_TAG];

/// An unread piece: the rule, and the counts the script logs with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unread {
    /// The rule.
    pub why: Why,
    /// Tokens whisper gave, its markers included.
    pub nt: usize,
    /// Of them, the text tokens (markers dropped).
    pub m: usize,
    /// The longest run of one token repeated.
    pub max_run: usize,
}

impl Unread {
    /// `nt=<n> m=<n> maxrun=<n>`.
    fn counts(&self) -> String {
        format!("nt={} m={} maxrun={}", self.nt, self.m, self.max_run)
    }

    /// heard.log's outcome for the piece itself: `voice-unread:<rule> <counts>`; `no-speech <counts>` for
    /// [`Why::NoTokens`] (the rule is the label: nothing says the piece was voice).
    fn heard_outcome(&self) -> String {
        if self.why.heard_voice() {
            format!("{}:{self}", self.why.label())
        } else {
            format!("{} {}", self.why.label(), self.counts())
        }
    }
}

impl fmt::Display for Unread {
    /// `<rule> nt=<n> m=<n> maxrun=<n>`, the script's.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.why.as_str(), self.counts())
    }
}

/// Seconds with one decimal, as awk's `printf "%.1f"` of `ms / 1000`.
pub(crate) fn tenths(ms: u64) -> String {
    let ms = u32::try_from(ms).unwrap_or(u32::MAX);
    format!("{:.1}", f64::from(ms) / 1000.0)
}

/// whisper's own markers — `[_BEG_]`, `[_TT_39]`, `[_EOT_]` — are not speech (the awk's `/^\[_.*\]$/`).
fn is_marker(bytes: &[u8]) -> bool {
    bytes.starts_with(b"[_") && bytes.ends_with(b"]")
}

/// How the lines whisper recites out of its training data on music begin, with each one's family label.
///
/// Subtitle credits and calls to action: the script's closed list, each entry seen live. Matched on [`normal`]
/// text, so case, punctuation and spacing never decide. A line that begins with one is a credit; its own words are
/// [`own_words`]', and the words after them are speech.
pub const LEAK_STARTS: [(&str, &str); 10] = [
    ("subtitles-created", "Субтитры созд"),
    ("subtitles-made", "Субтитры сдел"),
    ("subtitle-editor", "Редактор субтитров"),
    ("subscribe", "ПОДПИШИСЬ Н"),
    ("dynamic-music", "ДИНАМИЧНАЯ МУЗЫКА"),
    ("thanks-for-viewing-ru", "Спасибо за просмотр"),
    ("enjoy-viewing-ru", "Приятного просмотра"),
    ("to-be-continued-ru", "Продолжение следует"),
    ("thank-you-for-watching", "Thank you for watching"),
    ("thanks-for-watching", "Thanks for watching"),
];

/// The whole text of the credits in [`LEAK_STARTS`] as whisper recites them, word for word.
///
/// The subtitle editor's as in all three real captures of it; the short one and the longer call to subscribe, #0454 B1 P8). A line that opens
/// with one has the longest such text for its own words (#0454 B1 D4, P6).
pub const WHOLE_CREDITS: [&str; 10] = [
    "Редактор субтитров А.Семкин Корректор А.Егорова",
    "Редактор субтитров А.Семкин",
    "ПОДПИШИСЬ НА КАНАЛ",
    "ПОДПИШИСЬ НА НАШ КАНАЛ",
    "ДИНАМИЧНАЯ МУЗЫКА",
    "Спасибо за просмотр",
    "Приятного просмотра",
    "Продолжение следует",
    "Thank you for watching",
    "Thanks for watching",
];

/// Whether one line of whisper's text is a credit leak: not speech in the room ([`credit_family`] names one).
#[must_use]
pub fn credit_leak(line: &str) -> bool {
    credit_family(line).is_some()
}

/// The family of the credit one line of whisper's text is, or `None` for speech.
///
/// The line begins with one of [`LEAK_STARTS`] (its label), is a whole silence filler (`FILLERS`,
/// `silence-filler`), or carries the subtitler's name whisper learned with the credits (`SIGNATURE`,
/// `credit-name`), each compared on [`normal`] text. A line, not a whole utterance: [`piece`] cuts the text into
/// lines (`lines`) and drops only the lines this names, a filler only when the piece holds nothing else.
#[must_use]
pub fn credit_family(line: &str) -> Option<&'static str> {
    let n = normal(line);
    LEAK_STARTS
        .iter()
        .find(|(_, s)| n.starts_with(&normal(s)))
        .map(|(f, _)| *f)
        .or_else(|| {
            FILLERS
                .iter()
                .any(|f| normal(f) == n)
                .then_some("silence-filler")
        })
        .or_else(|| n.contains(&normal(SIGNATURE)).then_some("credit-name"))
}

/// The whole lines whisper says over silence. Said beside a real word they are speech (people say them), so a
/// filler line is a credit only in a piece of nothing but fillers and credits (#0454 B1 F2, the call logs' rule).
const FILLERS: [&str; 2] = ["Спасибо", "Thank you"];

/// The subtitler's name whisper learned with the credits: a word holding it is never speech in the room.
const SIGNATURE: &str = "DimaTorzok";

/// How a credit signed with [`SIGNATURE`] begins, when it says who did the subtitles (`Субтитры делал
/// DimaTorzok`): this word, one or two words before the name.
const SIGNED_BY: &str = "Субтитр";

/// The characters outside ASCII that are punctuation, not letters (#0454 B1 P7).
///
/// The ellipsis, guillemets and
/// curly quotes, the en and em dashes and the no-break space. The script's awk holds the same list.
pub const PUNCTUATION: [char; 11] = ['…', '«', '»', '„', '“', '”', '‘', '’', '–', '—', '\u{a0}'];

/// Text as the credit rules compare it (#0454 B1 P7), the script's `norm()`: letters kept, punctuation folded to a
/// word break, each run of breaks one space, none at either end.
///
/// A letter is an ASCII letter or digit, or any character outside ASCII but [`PUNCTUATION`]; everything else is
/// punctuation (ASCII punctuation, spaces, controls). Letters fold case only from A–Z and from the Russian alphabet
/// (А–Я, Ё): the awk folds these by an explicit table, never `tolower()`, which no awk on the Mac or c folds
/// Cyrillic with. No letter is ever dropped and no word merged.
#[must_use]
pub fn normal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars() {
        let letter = c.is_ascii_alphanumeric() || !(c.is_ascii() || PUNCTUATION.contains(&c));
        if !letter {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        if upper(c) {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Where a token ends a sentence: its text ends in one of these.
const SENTENCE_ENDS: [&str; 4] = [".", "!", "?", "…"];

/// One of whisper's lines: its tokens; whether it opens where a credit may (a whisper segment start: credit words
/// after any other word of their segment are speech, #0454 B1 D8'); and whether it is a credit whose recital went
/// on past its own words, the words after them being the next line (#0454 B1 P6, flagged in the piece's record).
struct Line {
    tokens: Range<usize>,
    opens: bool,
    flagged: bool,
}

/// The text tokens cut into whisper's lines.
///
/// A line ends at each of whisper's segment bounds (every credit whisper recited in the 12 real captures is a
/// whole segment, none ending a sentence, #0454 B1 F1), at a gap of [`PAUSE_MS`] or
/// more, after a token that ends a sentence when the next begins a word (an initial's dot ends none, [`line_ends`]), and after a credit's
/// own words ([`own_words`]) when a word with a letter follows them. A credit signed with [`SIGNATURE`] is a line of
/// its own however it sits in the text: from its [`SIGNED_BY`] word (or the name, without one) to the end of the
/// name's word, so the speech glued before or after it is a line of its own too. A line opens where a credit may
/// ([`Line::opens`]) only at the piece's start, a segment bound and the signed credit's bounds (the signature is a
/// hallucination token wherever it sits, #0454 B1 D12); after a sentence end, a gap or a credit's own words inside
/// a segment it holds speech, as the base matched a credit only where its text began (#0454 B1 D8': every real
/// credit is a whole segment).
fn lines(toks: &[&Token]) -> Vec<Line> {
    let mut cuts: Vec<(usize, bool)> = (1..toks.len())
        .filter_map(|i| {
            line_ends(
                toks.get(..i).unwrap_or_default(),
                toks.get(i..).unwrap_or_default(),
            )
            .map(|opens| (i, opens))
        })
        .collect();
    let words = word_spans(toks);
    let text = |w: &Range<usize>| normal(&joined(toks.get(w.clone()).unwrap_or_default()));
    let (name, by) = (normal(SIGNATURE), normal(SIGNED_BY));
    for (k, w) in words.iter().enumerate() {
        if !text(w).contains(&name) {
            continue;
        }
        let first = (k.saturating_sub(2)..k)
            .find(|&j| words.get(j).is_some_and(|o| text(o).starts_with(&by)))
            .unwrap_or(k);
        cuts.extend(words.get(first).map(|o| (o.start, true)));
        cuts.push((w.end, true));
    }
    // An opening cut first, so it is the one kept where a gap falls on the same token.
    cuts.sort_unstable_by_key(|&(i, opens)| (i, !opens));
    cuts.dedup_by_key(|c| c.0);
    let mut out = Vec::new();
    let (mut from, mut opens) = (0, true);
    for (cut, next) in cuts.into_iter().filter(|&(c, _)| c > 0 && c < toks.len()) {
        out.push((from..cut, opens));
        (from, opens) = (cut, next);
    }
    if from < toks.len() {
        out.push((from..toks.len(), opens));
    }
    out.into_iter()
        .flat_map(|(r, opens)| {
            let line = toks.get(r.clone()).unwrap_or_default();
            // The line ends where its segment does (the piece's end or a segment bound), not at a stop or a gap.
            let segment_end = toks.get(r.end).is_none_or(|next| {
                toks.get(r.end.saturating_sub(1))
                    .is_none_or(|last| last.segment != next.segment)
            });
            match own_words(line, segment_end).filter(|_| opens) {
                Some(k) if k < line.len() => {
                    let at = r.start.saturating_add(k);
                    vec![
                        Line {
                            tokens: r.start..at,
                            opens,
                            flagged: true,
                        },
                        Line {
                            tokens: at..r.end,
                            opens: false,
                            flagged: false,
                        },
                    ]
                }
                _ => vec![Line {
                    tokens: r,
                    opens,
                    flagged: false,
                }],
            }
        })
        .collect()
}

/// A credit's own words in a line that begins with one of [`LEAK_STARTS`] (#0454 B1 P6), as a count of the line's
/// tokens; `None` for a line that begins with none, or is signed with [`SIGNATURE`] (cut to its credit already).
///
/// They are the longest of [`WHOLE_CREDITS`] the line opens with, word for word on [`normal`] text; with none, the
/// words the start covers (a start ending inside a word covers that whole word). An [`EDITOR`] credit's are also its
/// names after the start ([`named`]; `segment_end`: the line ends where its whisper segment does), if they reach
/// further. Words with no letter right after them are theirs too: they are its punctuation.
fn own_words(line: &[&Token], segment_end: bool) -> Option<usize> {
    let words = word_spans(line);
    let upto = |k: usize| normal(&joined(line.get(..k).unwrap_or_default()));
    let all = upto(line.len());
    if all.contains(&normal(SIGNATURE)) {
        return None;
    }
    let (family, start) = LEAK_STARTS
        .iter()
        .find(|(_, s)| all.starts_with(&normal(s)))?;
    let ends: Vec<usize> = words.iter().map(|w| w.end).collect();
    let whole = ends
        .iter()
        .rev()
        .find(|&&e| WHOLE_CREDITS.iter().any(|w| normal(w) == upto(e)));
    let covered = *ends
        .iter()
        .find(|&&e| upto(e).starts_with(&normal(start)))?;
    let mut end = whole.copied().unwrap_or(covered);
    if *family == EDITOR {
        end = end.max(named(line, &words, covered, segment_end));
    }
    while let Some(w) = words.iter().find(|w| w.start == end) {
        if !normal(&joined(line.get(w.clone()).unwrap_or_default())).is_empty() {
            break;
        }
        end = w.end;
    }
    Some(end)
}

/// The family whose credit names who did the work after its start (#0454 B1 P6a): [`named`].
const EDITOR: &str = "subtitle-editor";

/// The word in an [`EDITOR`] credit after which a second name follows.
const PROOFREADER: &str = "Корректор";

/// Where the names of an [`EDITOR`] credit end, its start ending at token `at` (#0454 B1 D16, D19, D22): a name
/// ([`Name`]), then optionally [`PROOFREADER`] and a name of its own. A word is judged on its case as written, never
/// on [`normal`] text (which folds every word to lowercase); all-caps words count only in a credit whose start is all
/// caps. [`PROOFREADER`] is never a name. `segment_end`: the line ends where its whisper segment does (D20).
fn named(line: &[&Token], words: &[Range<usize>], at: usize, segment_end: bool) -> usize {
    let text = |w: &Range<usize>| joined(line.get(w.clone()).unwrap_or_default());
    let Some(k) = words.iter().position(|w| w.start == at) else {
        return at;
    };
    let caps = !words
        .get(..k)
        .unwrap_or_default()
        .iter()
        .any(|w| text(w).chars().any(lower));
    let proofreader = |k: usize| {
        words
            .get(k)
            .is_some_and(|w| normal(&text(w)) == normal(PROOFREADER))
    };
    let shape = |k: usize| {
        words
            .get(k)
            .filter(|_| !proofreader(k))
            .and_then(|w| name_shape(&text(w), caps))
    };
    // A word after which a capitalised one may still be a name: only the proofreader's word or the segment's end.
    let closes = |k: usize| (k == words.len() && segment_end) || proofreader(k);
    // ONE name from word k (default-to-real: a capitalised word before speech may be speech). A glued initial carries
    // its surname and closes the slot; initials alone are always taken, and the capitalised word after them only
    // where it closes; any other name is one capitalised word, taking a second only where that one closes.
    let name = |k: usize| match shape(k) {
        None => k,
        Some(Name::Initial) => {
            let mut k = k;
            while shape(k) == Some(Name::Initial) {
                k = k.saturating_add(1);
            }
            let last = shape(k);
            if last == Some(Name::Glued)
                || (last == Some(Name::Word) && closes(k.saturating_add(1)))
            {
                k.saturating_add(1)
            } else {
                k
            }
        }
        Some(Name::Word)
            if shape(k.saturating_add(1)) == Some(Name::Word) && closes(k.saturating_add(2)) =>
        {
            k.saturating_add(2)
        }
        Some(Name::Glued | Name::Word) => k.saturating_add(1),
    };
    let mut k = name(k);
    if proofreader(k) {
        k = name(k.saturating_add(1));
    }
    words.get(k).map_or(line.len(), |w| w.start)
}

/// The shapes of a name's words, as written (#0454 B1 D16).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Name {
    /// An initial: `И.`.
    Initial,
    /// An initial glued to a surname: `И.Петрова`.
    Glued,
    /// A capitalised word: `Ирина`.
    Word,
}

/// A word's [`Name`] shape as written, if it has one: in an all-caps credit (`caps`), the surname of a glued initial
/// and a word are all caps of two letters or more. Punctuation after the word is no part of its shape. Letters are
/// A–Z and the Russian alphabet, cased as [`normal`] folds them.
fn name_shape(word: &str, caps: bool) -> Option<Name> {
    let shape = |w: &str| -> String {
        w.chars()
            .map(|c| match c {
                c if upper(c) => 'U',
                c if lower(c) => 'l',
                '.' => '.',
                _ => '?',
            })
            .collect()
    };
    let rest = if caps { 'U' } else { 'l' };
    let tail = |t: &str| !t.is_empty() && t.chars().all(|c| c == rest);
    let of = |s: String| {
        if s == "U." {
            Some(Name::Initial)
        } else if s.strip_prefix("U.U").is_some_and(tail) {
            Some(Name::Glued)
        } else if s.strip_prefix('U').is_some_and(tail) {
            Some(Name::Word)
        } else {
            None
        }
    };
    of(shape(word)).or_else(|| of(shape(word.trim_end_matches(['.', ',', '!', '?', '…']))))
}

/// An uppercase letter of A–Z or the Russian alphabet: the letters [`normal`] folds.
fn upper(c: char) -> bool {
    c.is_ascii_uppercase() || ('А'..='Я').contains(&c) || c == 'Ё'
}

/// A lowercase letter of a–z or the Russian alphabet.
fn lower(c: char) -> bool {
    c.is_ascii_lowercase() || ('а'..='я').contains(&c) || c == 'ё'
}

/// The text tokens grouped into words, as index ranges: a token that begins with a space begins one.
fn word_spans(toks: &[&Token]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match out.last_mut() {
            Some(w) if !t.bytes.starts_with(b" ") => w.end = i.saturating_add(1),
            _ => out.push(i..i.saturating_add(1)),
        }
    }
    out
}

/// Whether a line ends between the last of `before` and the first of `rest` ([`lines`]' rule), and if so whether the
/// next one opens where a credit may (`Some(true)`: a segment bound only; `Some(false)`: a sentence end or a gap
/// inside a segment). An initial's dot ends no sentence (#0454 B1 D17: "Редактор субтитров И. Петрова" is one credit).
/// The rule is general, not scoped to a credit's name (D23): with every stop cut removed, 3000 pieces without a credit
/// came out the same (#0454 B1 r2h), so a stop cut reaches only a credit's own words.
fn line_ends(before: &[&Token], rest: &[&Token]) -> Option<bool> {
    let (a, b) = (before.last()?, rest.first()?);
    if a.segment != b.segment {
        return Some(true);
    }
    let word = before
        .iter()
        .rposition(|t| t.bytes.starts_with(b" "))
        .and_then(|w| before.get(w..))
        .unwrap_or(before);
    let stop = name_shape(&joined(word), false) != Some(Name::Initial)
        && b.bytes.starts_with(b" ")
        && SENTENCE_ENDS
            .iter()
            .any(|e| a.bytes.trim_ascii_end().ends_with(e.as_bytes()));
    let gap = b
        .start_ms
        .checked_sub(a.end_ms)
        .is_some_and(|gap| gap >= PAUSE_MS);
    (stop || gap).then_some(false)
}

/// The tokens' bytes joined, decoded and trimmed of spaces (as the line is).
fn joined(toks: &[&Token]) -> String {
    let bytes: Vec<u8> = toks.iter().flat_map(|t| t.bytes.iter().copied()).collect();
    String::from_utf8_lossy(&bytes).trim_matches(' ').to_owned()
}

/// A word in progress: its first piece's start, its last piece's end, its pieces' bytes (decoded when it closes).
struct Word {
    t0: u64,
    t1: u64,
    bytes: Vec<u8>,
}

/// Closes the open word into the payload (decoded, leading spaces trimmed, epoch added).
fn close_word(word: &mut Option<Word>, pay: &mut String, epoch_ms: u64) {
    let Some(w) = word.take() else {
        return;
    };
    if !pay.is_empty() {
        pay.push(';');
    }
    let _w: fmt::Result = write!(
        pay,
        "{},{},{}",
        epoch_ms.saturating_add(w.t0),
        epoch_ms.saturating_add(w.t1),
        String::from_utf8_lossy(&w.bytes).trim_start_matches(' ')
    );
}

/// Assembles one utterance's transcript into its text and payload.
///
/// Token offsets are ms from `epoch_ms`, the utterance's start; `ms` is the utterance's length, for an unread
/// piece's `<voice N.Ns>` / `<no speech N.Ns>`.
/// A token with no text or ending before it starts is ignored, as the script's parser does. Pieces join as bytes,
/// as the awk joins them: a letter whose bytes whisper split across two tokens decodes whole. The transcript's
/// special tokens count among the markers of an unread piece's [`Unread`].
///
/// Each of whisper's lines that is a credit leak ([`credit_leak`]; a silence filler only when every line is one) is
/// dropped by itself, and the text and the payload are both made from the tokens left: real words beside a credit
/// are published (2026-11-24, E2: a whole minute of speech was lost to a credit line whisper put at its end). Only
/// when nothing is left is the piece unread as `credit-leak`.
#[must_use]
pub fn piece(transcript: &Transcript, epoch_ms: u64, ms: u64) -> Piece {
    let parsed: Vec<&Token> = transcript
        .tokens
        .iter()
        .filter(|t| !t.bytes.is_empty() && t.start_ms <= t.end_ms)
        .collect();
    let nt = parsed.len().saturating_add(transcript.specials);
    let toks: Vec<&Token> = parsed
        .into_iter()
        .filter(|t| !is_marker(&t.bytes))
        .collect();
    let m = toks.len();
    let (mut run, mut max_run) = (0_usize, 0_usize);
    let mut prev: Option<&[u8]> = None;
    for t in &toks {
        if prev == Some(t.bytes.as_slice()) {
            run = run.saturating_add(1);
        } else {
            run = 1;
            prev = Some(&t.bytes);
        }
        max_run = max_run.max(run);
    }
    // A line opening inside a segment holds speech whatever its words (#0454 B1 D8').
    let lines: Vec<(&[&Token], String, Option<&str>, bool)> = lines(&toks)
        .into_iter()
        .map(|l| {
            let line = toks.get(l.tokens).unwrap_or_default();
            let s = joined(line);
            let family = if l.opens { credit_family(&s) } else { None };
            (line, s, family, l.flagged)
        })
        .collect();
    // A filler drops only with the whole piece: when every line with text is a credit or a filler.
    let only_leaks = lines.iter().all(|(_, s, f, _)| s.is_empty() || f.is_some());
    let mut kept: Vec<&Token> = Vec::with_capacity(toks.len());
    let mut leaks = 0_usize;
    let mut flagged = Vec::new();
    for (line, _, family, flag) in &lines {
        match *family {
            Some(f) if only_leaks || f != "silence-filler" => {
                leaks = leaks.saturating_add(1);
                if *flag {
                    flagged.push(f);
                }
            }
            _ => kept.extend_from_slice(line),
        }
    }
    let said = |kept: &[&Token]| {
        // After the leaks: a leak line holding a loop text's first copy would otherwise take its only kept copy along.
        let kept = unlooped(kept);
        let (text, payload) = words(&kept, epoch_ms);
        let why = if joined(&toks).is_empty() {
            Some(match (nt, m) {
                (0, _) => Why::NoTokens,
                (_, 0) => Why::MarkersOnly,
                _ => Why::EmptyText,
            })
        } else if joined(&kept).is_empty() {
            Some(Why::CreditLeak)
        } else if max_run >= REPEAT_BOUND {
            Some(Why::MaxRun)
        } else {
            None
        };
        (why.map_or(text, |w| w.placeholder(ms)), payload, why)
    };
    let (text, payload, why) = said(&kept);
    let heard = if leaks == 0 {
        text.clone()
    } else {
        said(&toks).0
    };
    match why {
        None => Piece {
            text,
            payload,
            unread: None,
            leaks,
            flagged,
            heard,
        },
        Some(why) => Piece {
            text,
            payload: String::new(),
            unread: Some(Unread {
                why,
                nt,
                m,
                max_run,
            }),
            leaks,
            flagged,
            heard,
        },
    }
}

/// The record's fields for a piece's flagged credits ([`Piece::flagged`]).
///
/// ` flagged=<n> flagged-family=<each family once, in order>`, or nothing when there is none, so a record without one reads as before (the script's verdict
/// line ends in the same fields).
#[must_use]
pub fn flag_fields(flagged: &[&str]) -> String {
    if flagged.is_empty() {
        return String::new();
    }
    let mut fams: Vec<&str> = Vec::new();
    for f in flagged {
        if !fams.contains(f) {
            fams.push(f);
        }
    }
    format!(
        " flagged={} flagged-family={}",
        flagged.len(),
        fams.join(",")
    )
}

/// These text tokens (markers already dropped) without whisper's segment loop at the utterance's tail.
///
/// A segment's text is its tokens' bytes, spaces and TABs trimmed at both ends, compared exactly; a segment with no
/// text is no copy and takes no position. A loop text has [`SEGMENT_LOOP`] or more copies, the last in the last
/// [`LOOP_TAIL`] segments. Its zone starts at its first copy whose next copy is at most [`LOOP_SPACING`] segments on;
/// with several loop texts, at the earliest such start. From there on each text keeps its first copy and every later
/// copy is dropped; before it, nothing is. No loop text, nothing dropped.
fn unlooped<'a>(toks: &[&'a Token]) -> Vec<&'a Token> {
    let mut texts: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for t in toks {
        texts
            .entry(t.segment)
            .or_default()
            .extend_from_slice(&t.bytes);
    }
    let trim = |b: &[u8]| -> Vec<u8> {
        let ws = |c: &u8| *c == b' ' || *c == b'\t';
        let from = b.iter().position(|c| !ws(c)).unwrap_or(b.len());
        let to = b
            .iter()
            .rposition(|c| !ws(c))
            .map_or(from, |i| i.saturating_add(1));
        b.get(from..to).unwrap_or_default().to_vec()
    };
    let segs: Vec<(usize, Vec<u8>)> = texts
        .into_iter()
        .map(|(n, b)| (n, trim(&b)))
        .filter(|(_, b)| !b.is_empty())
        .collect();
    let mut copies: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (p, (_, text)) in segs.iter().enumerate() {
        copies.entry(text.as_slice()).or_default().push(p);
    }
    let zone = copies
        .values()
        .filter(|at| {
            at.len() >= SEGMENT_LOOP
                && at
                    .last()
                    .is_some_and(|&p| p.saturating_add(LOOP_TAIL) >= segs.len())
        })
        .filter_map(|at| {
            at.windows(2).find_map(|w| match *w {
                [p, next] if next.saturating_sub(p) <= LOOP_SPACING => Some(p),
                _ => None,
            })
        })
        .min();
    let Some(zone) = zone else {
        return toks.to_vec();
    };
    let mut first: HashSet<&[u8]> = HashSet::new();
    let dropped: HashSet<usize> = segs
        .iter()
        .skip(zone)
        .filter(|(_, text)| !first.insert(text.as_slice()))
        .map(|&(n, _)| n)
        .collect();
    toks.iter()
        .copied()
        .filter(|t| !dropped.contains(&t.segment))
        .collect()
}

/// The text (with `<pause N.Ns>` tags) and the payload of these text tokens; the payload is empty when a token
/// carries the payload's own separators.
fn words(toks: &[&Token], epoch_ms: u64) -> (String, String) {
    let mut text: Vec<u8> = Vec::new();
    let mut pay = String::new();
    let mut bad = false;
    let mut word: Option<Word> = None;
    let mut before: Option<&Token> = None;
    for &t in toks {
        if let Some(gap) = before.and_then(|b| t.start_ms.checked_sub(b.end_ms))
            && gap >= PAUSE_MS
        {
            text.extend_from_slice(format!("<pause {}s>", tenths(gap)).as_bytes());
            // A word cannot span the gap the text itself names.
            close_word(&mut word, &mut pay, epoch_ms);
        }
        match &mut word {
            Some(w) if !t.bytes.starts_with(b" ") => w.bytes.extend_from_slice(&t.bytes),
            _ => {
                close_word(&mut word, &mut pay, epoch_ms);
                word = Some(Word {
                    t0: t.start_ms,
                    t1: t.end_ms,
                    bytes: t.bytes.clone(),
                });
            }
        }
        if let Some(w) = &mut word {
            w.t1 = t.end_ms;
        }
        text.extend_from_slice(&t.bytes);
        if t.bytes.iter().any(|&b| b == b';' || b == b'\t') {
            bad = true;
        }
        before = Some(t);
    }
    close_word(&mut word, &mut pay, epoch_ms);
    let text = String::from_utf8_lossy(&text).trim_matches(' ').to_owned();
    (text, if bad { String::new() } else { pay })
}

/// The prompt's adjacent word pairs (`EARS_PROMPT`, its vocabulary separated by `, `).
///
/// whisper echoes its prompt back on silence, and a phrase carrying a pair is that echo. The script's rule: runs of spaces squeezed to one, then
/// per line every two neighbouring `, `-separated fields as `a, b`. No prompt, or one without a `, `: no pairs.
#[must_use]
pub fn prompt_pairs(prompt: &str) -> Vec<String> {
    let mut squeezed = String::with_capacity(prompt.len());
    for c in prompt.chars() {
        if !(c == ' ' && squeezed.ends_with(' ')) {
            squeezed.push(c);
        }
    }
    let mut pairs = Vec::new();
    for line in squeezed.split('\n') {
        let fields: Vec<&str> = line.split(", ").collect();
        for w in fields.windows(2) {
            if let [a, b] = w {
                pairs.push(format!("{a}, {b}"));
            }
        }
    }
    pairs
}

/// `EARS_HOLD_MAX_MS`: unset or empty is [`HOLD_MAX_MS`].
///
/// # Errors
///
/// A value that is not a whole number of milliseconds, named.
pub fn hold_max_from(v: Option<&str>) -> Result<u64, String> {
    v.filter(|s| !s.is_empty()).map_or(Ok(HOLD_MAX_MS), |s| {
        s.parse()
            .map_err(|_| format!("EARS_HOLD_MAX_MS is not a number of ms: {s:?}"))
    })
}

/// `EARS_DEAF_S`: unset or empty is [`DEAF_S`].
///
/// # Errors
///
/// A value that is not whole seconds from 1 to 86400 written without a leading zero, named (the script's rule).
pub fn deaf_s_from(v: Option<&str>) -> Result<u64, String> {
    v.filter(|s| !s.is_empty()).map_or(Ok(DEAF_S), |s| {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then_some(s)
            .filter(|s| !s.starts_with('0'))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| (1..=86_400).contains(n))
            .ok_or_else(|| {
                format!("EARS_DEAF_S must be whole seconds, 1..86400, no leading zero: {s:?}")
            })
    })
}

/// `HH:MM:SS` and `YYYY-MM-DDTHH:MM:SSZ` of `ms` since the epoch, UTC (the script's `date -u`).
fn utc(ms: u64) -> (String, String) {
    let secs = ms / 1000;
    let (days, day) = (secs / 86_400, secs % 86_400);
    let clock = format!("{:02}:{:02}:{:02}", day / 3600, (day / 60) % 60, day % 60);
    // Days to the civil date (H. Hinnant's algorithm), on the non-negative epoch only.
    let z = days.saturating_add(719_468);
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe
        .saturating_sub(doe / 1460)
        .saturating_add(doe / 36_524)
        .saturating_sub(doe / 146_096))
        / 365;
    let doy = doe.saturating_sub(
        yoe.saturating_mul(365)
            .saturating_add(yoe / 4)
            .saturating_sub(yoe / 100),
    );
    let mp = (doy.saturating_mul(5).saturating_add(2)) / 153;
    let d = doy
        .saturating_sub((mp.saturating_mul(153).saturating_add(2)) / 5)
        .saturating_add(1);
    let m = if mp < 10 {
        mp.saturating_add(3)
    } else {
        mp.saturating_sub(9)
    };
    let y = yoe
        .saturating_add(era.saturating_mul(400))
        .saturating_add(u64::from(m <= 2));
    let stamp = format!("{y:04}-{m:02}-{d:02}T{clock}Z");
    (clock, stamp)
}

/// One side's turn: what is held while the segmenter's cuts say the speech goes on; and its deaf-suspect streak.
#[derive(Debug, Default)]
struct Turn {
    hold: bool,
    t0: u64,
    t1: u64,
    text: String,
    pay: String,
    /// The text with no credit dropped ([`Piece::heard`]), joined as `text` is: the prompt-echo rule's.
    heard: String,
    /// The streak's unread ms so far (0: none), and its first piece's `t0`.
    deaf_ms: u64,
    deaf_t0: u64,
    /// The streak's line was given; only a piece read as text re-arms it.
    deaf_said: bool,
    /// Why the turn's first piece was unread (`None`: read): the phrase's heard.log outcome.
    lead: Option<Why>,
}

impl Turn {
    /// Adds one piece to the deaf-suspect streak: `Some((t0, ms))` of the streak when it reaches `after_ms` while
    /// armed.
    const fn deaf(&mut self, why: Option<Why>, h: &Heard, after_ms: u64) -> Option<(u64, u64)> {
        match why {
            Some(w) if w.deaf() => {
                if self.deaf_ms == 0 {
                    self.deaf_t0 = h.t0_ms;
                }
                self.deaf_ms = self.deaf_ms.saturating_add(h.ms);
            }
            Some(_) => self.deaf_ms = 0,
            None => {
                self.deaf_ms = 0;
                self.deaf_said = false;
            }
        }
        if self.deaf_said || self.deaf_ms < after_ms {
            return None;
        }
        self.deaf_said = true;
        Some((self.deaf_t0, self.deaf_ms))
    }
}

/// Both sides' turns, the prompt pairs and heard.log: [`Heard`] in, `TULPA-EARS-LINE`s out.
#[derive(Debug)]
pub struct Assembler {
    pairs: Vec<String>,
    hold_max_ms: u64,
    log: PathBuf,
    log_failed: bool,
    /// Rotate heard.log to `heard.log.1` once it passes this many bytes ([`BOUND`] unless
    /// [`Assembler::with_log_bound`] says otherwise): the live path's as the shadow's.
    log_bound: u64,
    /// Whether heard.log is rotated past its bound; `false` once the run is told to stop
    /// ([`Assembler::append_only`]).
    rotates: bool,
    /// The deaf-suspect streak's length, ms.
    deaf_after_ms: u64,
    me: Turn,
    them: Turn,
}

impl Assembler {
    /// An assembler logging to `<state_dir>/heard.log`, which is created `0600` (or made `0600`) here. A log that
    /// cannot be created is said on stderr; the lines still go out.
    #[must_use]
    pub fn new(state_dir: &Path, prompt: Option<&str>, hold_max_ms: u64) -> Self {
        if let Err(e) = fs::create_dir_all(state_dir) {
            crate::warn(&format!(
                "tulpa-hear: {} not made: {e}",
                state_dir.display()
            ));
        }
        let mut a = Self {
            pairs: prompt.map(prompt_pairs).unwrap_or_default(),
            hold_max_ms,
            log: state_dir.join(HEARD_LOG),
            log_failed: false,
            log_bound: BOUND,
            rotates: true,
            deaf_after_ms: DEAF_S.saturating_mul(1000),
            me: Turn::default(),
            them: Turn::default(),
        };
        let made =
            append(&a.log).and_then(|_| fs::set_permissions(&a.log, Permissions::from_mode(0o600)));
        if let Err(e) = made {
            a.log_error(&e);
        }
        a
    }

    /// [`Assembler::new`] with `EARS_HOLD_MAX_MS` and `EARS_DEAF_S` as `var` gives them (the environment's).
    ///
    /// # Errors
    ///
    /// A bad value of either, named; nothing is created then.
    pub fn from_env(
        state_dir: &Path,
        prompt: Option<&str>,
        var: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let hold_max_ms = hold_max_from(var("EARS_HOLD_MAX_MS").as_deref())?;
        let deaf_s = deaf_s_from(var("EARS_DEAF_S").as_deref())?;
        Ok(Self::new(state_dir, prompt, hold_max_ms).with_deaf_s(deaf_s))
    }

    /// This assembler with heard.log bounded at `bytes` instead of [`BOUND`]: once a line takes it past that, it
    /// is renamed to `heard.log.1` (replacing an older one) and a new one is started on the next line.
    #[must_use]
    pub const fn with_log_bound(mut self, bytes: u64) -> Self {
        self.log_bound = bytes;
        self
    }

    /// From now on heard.log is only appended to, never rotated: for a run that has been told to stop.
    ///
    /// The capture that comes next shares the file and rotates it by the same rule. Two writers that both see it
    /// past its bound would both rename: the second rename puts the first's fresh log over the rotated one. After
    /// the stop, rotation is the next capture's alone.
    pub const fn append_only(&mut self) {
        self.rotates = false;
    }

    /// This assembler with the deaf-suspect streak at `secs` (`EARS_DEAF_S`) instead of [`DEAF_S`].
    #[must_use]
    pub const fn with_deaf_s(mut self, secs: u64) -> Self {
        self.deaf_after_ms = secs.saturating_mul(1000);
        self
    }

    /// `side`'s turn.
    const fn turn(&mut self, side: Side) -> &mut Turn {
        match side {
            Side::Me => &mut self.me,
            Side::Them => &mut self.them,
        }
    }

    /// Takes one utterance; returns the lines it lets go, in order: a `<deaf-suspect …>` line when this piece
    /// completes the side's streak, then the phrase's line when the phrase is let go and emitted — none while the
    /// turn is held or when the phrase is a prompt echo (dropped). `now_ms` is the clock column and heard.log's
    /// time.
    pub fn take(&mut self, h: &Heard, now_ms: u64) -> Vec<String> {
        let piece = piece(&h.transcript, h.t0_ms, h.ms);
        let (clock, stamp) = utc(now_ms);
        let side = h.side.as_str();
        let mut lines = Vec::new();
        if let Some(u) = piece.unread
            && h.ms >= RETRY_MIN_MS
        {
            self.heard(&stamp, h.side, h.ms, &h.piece, &u.heard_outcome());
        }
        let (deaf_after_ms, hold_max_ms) = (self.deaf_after_ms, self.hold_max_ms);
        let turn = self.turn(h.side);
        let why = piece.unread.map(|u| u.why);
        if let (Some((t0, ms)), Some(why)) = (turn.deaf(why, h, deaf_after_ms), why) {
            lines.push(format!(
                "TULPA-EARS-LINE {t0} {} {clock} [{side}] <deaf-suspect {} {}s>",
                h.t1_ms,
                why.as_str(),
                ms / 1000
            ));
        }
        if turn.hold {
            let gap = h.t0_ms.checked_sub(turn.t1);
            for (text, said) in [
                (&mut turn.text, &piece.text),
                (&mut turn.heard, &piece.heard),
            ] {
                match gap {
                    Some(gap) if gap >= PAUSE_MS => {
                        let _w: fmt::Result = write!(text, "<pause {}s>", tenths(gap));
                    }
                    // Pieces of one phrase with no pause between: a breath, not glue.
                    _ if !text.is_empty() => text.push(' '),
                    _ => {}
                }
                text.push_str(said);
            }
        } else {
            turn.t0 = h.t0_ms;
            turn.lead = why;
            turn.text.clone_from(&piece.text);
            turn.heard.clone_from(&piece.heard);
            turn.pay.clear();
        }
        turn.t1 = h.t1_ms;
        if !piece.payload.is_empty() {
            if !turn.pay.is_empty() {
                turn.pay.push(';');
            }
            turn.pay.push_str(&piece.payload);
        }
        // A max-length cut is not the end of the phrase: hold it, up to the bound of held speech.
        if h.cut
            && (turn.t0 == 0
                || turn
                    .t1
                    .checked_sub(turn.t0)
                    .is_none_or(|held| held < hold_max_ms))
        {
            turn.hold = true;
            return lines;
        }
        lines.extend(self.let_go(h.side, h.contour, now_ms));
        lines
    }

    /// Lets go of `side`'s held turn, if any (a max-length cut whose successor never came): its line, as when the
    /// next piece ends it. For the end of the run: a held phrase is written, not lost.
    pub fn flush(&mut self, side: Side, now_ms: u64) -> Option<String> {
        let turn = self.turn(side);
        if !turn.hold {
            return None;
        }
        self.let_go(side, Contour::Unknown, now_ms)
    }

    /// The line of `side`'s turn, which ends here with `contour` (its text and payload taken, its hold cleared), and
    /// its heard.log line; `None` when the phrase is a prompt echo (dropped).
    fn let_go(&mut self, side: Side, contour: Contour, now_ms: u64) -> Option<String> {
        let (clock, stamp) = utc(now_ms);
        let turn = self.turn(side);
        turn.hold = false;
        let (t0, t1) = (turn.t0, turn.t1);
        let mut text = std::mem::take(&mut turn.text);
        let mut pay = std::mem::take(&mut turn.pay);
        let mut lead = turn.lead.take();
        let heard = std::mem::take(&mut turn.heard);
        let span = t1.saturating_sub(t0);
        if self.pairs.iter().any(|p| heard.contains(p.as_str())) {
            self.heard(&stamp, side, span, &heard, "dropped:prompt-echo");
            return None;
        }
        if text.is_empty() {
            lead = Some(Why::EmptyText);
            text = Why::EmptyText.placeholder(span);
            pay.clear();
        }
        let outcome = lead.map_or_else(|| "emitted".to_owned(), Why::outcome);
        self.heard(&stamp, side, span, &text, &outcome);
        let tone = match contour {
            Contour::Rising => " [intonation: rising]",
            Contour::Falling => " [intonation: falling]",
            _ => "",
        };
        // One line on the wire whatever whisper said (a newline inside a token would split it).
        let one = |s: &str| s.replace(['\n', '\r'], " ");
        let text = one(&text);
        let side = side.as_str();
        Some(if pay.is_empty() {
            format!("TULPA-EARS-LINE {t0} {t1} {clock} [{side}] {text}{tone}")
        } else {
            let pay = one(&pay);
            format!("TULPA-EARS-LINE {t0} {t1} {clock} [{side}] {text}{tone}\t{pay}")
        })
    }

    /// One heard.log line: UTC time, side, audio seconds, cut mode, text (tabs and newlines as spaces), outcome.
    fn heard(&mut self, stamp: &str, side: Side, span_ms: u64, text: &str, outcome: &str) {
        let line = format!(
            "{stamp}\t{}\t{}\t{CUT_MODE}\t{}\t{outcome}\n",
            side.as_str(),
            tenths(span_ms),
            text.replace(['\t', '\n'], " ")
        );
        let written = append(&self.log)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .and_then(|()| {
                if self.rotates {
                    rotate_past(&self.log, self.log_bound).map(drop)
                } else {
                    Ok(())
                }
            });
        if let Err(e) = written {
            self.log_error(&e);
        }
    }

    /// Says a heard.log failure on stderr, once.
    fn log_error(&mut self, e: &io::Error) {
        if !self.log_failed {
            self.log_failed = true;
            crate::warn(&format!(
                "tulpa-hear: {} not written: {e}",
                self.log.display()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        clippy::unreadable_literal,
        reason = "tests: fixed lists, and whisper's own timings verbatim"
    )]

    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::recognize::Transcript;

    /// The python rig's `ojf()` writes no `p`: its synthetic tokens get this one (the assembler never reads it).
    const P: f32 = 1.0;
    /// 2026-11-22T14:56:07Z.
    const NOW: u64 = 1_795_359_367_000;

    fn fx(toks: &[(&str, u64, u64, f32)]) -> Vec<Token> {
        toks.iter()
            .map(|&(t, a, b, p)| Token::new(t, a, b, p))
            .collect()
    }

    /// One utterance as the python rig's `piece()` scripts it (`into` flat unless said).
    fn utt(side: Side, tokens: Vec<Token>, ms: u64, t0: u64, t1: u64, cut: bool) -> Heard {
        Heard {
            side,
            t0_ms: t0,
            t1_ms: t1,
            ms,
            cut,
            contour: Contour::Flat,
            transcript: Transcript::new(tokens, false),
            piece: format!("c{t0}"),
        }
    }

    struct Rig {
        dir: PathBuf,
        asm: Assembler,
    }

    impl Rig {
        fn new(tag: &str, prompt: Option<&str>, hold_max_ms: u64) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tulpa-assemble-{tag}-{}", std::process::id()));
            let _gone = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).ok();
            let asm = Assembler::new(&dir, prompt, hold_max_ms);
            Self { dir, asm }
        }

        fn lines(&mut self, utts: &[Heard]) -> Vec<String> {
            utts.iter().flat_map(|h| self.asm.take(h, NOW)).collect()
        }

        fn log(&self) -> Vec<Vec<String>> {
            fs::read_to_string(self.dir.join(HEARD_LOG))
                .unwrap_or_default()
                .lines()
                .map(|l| l.split('\t').map(str::to_owned).collect())
                .collect()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _gone = fs::remove_dir_all(&self.dir);
        }
    }

    /// The text after `[me] ` / `[them] ` up to the TAB, and the payload after it.
    fn body(line: &str) -> (&str, &str) {
        let (text, payload) = line.split_once('\t').unwrap_or((line, ""));
        let body = text.split_once("] ").map_or(text, |(_, b)| b);
        (body, payload)
    }

    // ---- the python rig's assembler cases (scripts/test-ears-capture.py), same tokens ----

    /// `check_held_turn_keeps_its_midhold_voice`
    #[test]
    fn held_turn_keeps_its_midhold_voice() {
        let mut rig = Rig::new("midhold", None, HOLD_MAX_MS);
        let lines = rig.lines(&[
            utt(
                Side::Me,
                rig_toks(&[("привет", 0, 500), (" как", 700, 1100)]),
                6000,
                1_000_000,
                1_006_000,
                true,
            ),
            utt(Side::Me, rig_toks(&[]), 4000, 1_012_400, 1_016_400, true),
            utt(
                Side::Me,
                rig_toks(&[(" мир", 200, 900)]),
                5000,
                1_017_000,
                1_022_000,
                false,
            ),
        ]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("TULPA-EARS-LINE 1000000 1022000 "),
            "{lines:?}"
        );
        let (text, payload) = body(&lines[0]);
        // The mid-hold piece has no token at all: `<no speech>` here, `<voice>` in the script (see the top).
        assert_eq!(
            text,
            "привет как<pause 6.4s><no speech 4.0s><pause 0.6s>мир"
        );
        assert_eq!(
            payload,
            "1000000,1000500,привет;1000700,1001100,как;1017200,1017900,мир"
        );
    }

    /// `check_hold_bound_publishes_midphrase`
    #[test]
    fn hold_bound_publishes_midphrase() {
        let mut rig = Rig::new("bound", None, 10_000);
        let lines = rig.lines(&one_two_three());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("TULPA-EARS-LINE 2000000 2012000 "),
            "{lines:?}"
        );
        assert!(lines[0].contains("[me] один два\t"), "{lines:?}");
        assert!(
            lines[1].starts_with("TULPA-EARS-LINE 2012000 2015000 "),
            "{lines:?}"
        );
        assert!(lines[1].contains("[me] три\t"), "{lines:?}");
    }

    /// Fable F4: a cut whose successor never comes (the run ends first) is let go by `flush`, as the next piece would
    /// have let it go; a side with nothing held gives nothing, and a flushed turn is not given again.
    #[test]
    fn a_held_turn_is_flushed_at_the_end() {
        let mut rig = Rig::new("flush", None, HOLD_MAX_MS);
        let cut = utt(
            Side::Them,
            rig_toks(&[("привет", 0, 500)]),
            6000,
            1_000_000,
            1_006_000,
            true,
        );
        assert_eq!(rig.lines(&[cut]), Vec::<String>::new(), "held");
        assert_eq!(rig.asm.flush(Side::Me, NOW), None, "nothing held");
        let line = rig.asm.flush(Side::Them, NOW).unwrap_or_default();
        assert!(
            line.starts_with("TULPA-EARS-LINE 1000000 1006000 "),
            "{line}"
        );
        assert_eq!(body(&line), ("привет", "1000000,1000500,привет"));
        assert_eq!(rig.asm.flush(Side::Them, NOW), None, "once");
    }

    /// `check_hold_far_from_the_bound_merges`
    #[test]
    fn hold_far_from_the_bound_merges() {
        let mut rig = Rig::new("far", None, 3_600_000);
        let lines = rig.lines(&one_two_three());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("TULPA-EARS-LINE 2000000 2015000 "),
            "{lines:?}"
        );
        assert!(lines[0].contains("[me] один два три\t"), "{lines:?}");
    }

    fn one_two_three() -> Vec<Heard> {
        vec![
            utt(
                Side::Me,
                rig_toks(&[("один", 0, 400)]),
                6000,
                2_000_000,
                2_006_000,
                true,
            ),
            utt(
                Side::Me,
                rig_toks(&[(" два", 600, 900)]),
                6000,
                2_006_000,
                2_012_000,
                true,
            ),
            utt(
                Side::Me,
                rig_toks(&[(" три", 0, 300)]),
                3000,
                2_012_000,
                2_015_000,
                false,
            ),
        ]
    }

    /// `check_assembler_drops_whisper_markers_and_merges_pieces`
    #[test]
    fn assembler_drops_whisper_markers_and_merges_pieces() {
        let mut rig = Rig::new("markers", None, HOLD_MAX_MS);
        let toks = rig_toks(&[
            ("[_BEG_]", 0, 10),
            (" П", 40, 90),
            ("ров", 90, 140),
            ("ер", 140, 190),
            ("ка", 190, 240),
            (" уш", 250, 300),
            ("ей", 300, 350),
            (".", 350, 360),
            ("[_TT_39]", 360, 370),
            (" Раз", 1400, 1500),
            (",", 1500, 1510),
            (" два", 1550, 1650),
        ]);
        let lines = rig.lines(&[utt(Side::Me, toks, 6000, 3_000_000, 3_006_000, false)]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (text, payload) = body(&lines[0]);
        assert_eq!(text, "Проверка ушей.<pause 1.0s> Раз, два");
        assert_eq!(
            payload,
            "3000040,3000240,Проверка;3000250,3000360,ушей.;3001400,3001510,Раз,;3001550,3001650,два"
        );
    }

    /// whisper's byte-level BPE splits a letter's UTF-8 across tokens: here the two bytes of 'и' (D0 B8). Joined
    /// on bytes, as the script's awk joins them, the word comes out whole on the line and in the payload — decoded
    /// per token it would be "пр\u{FFFD}\u{FFFD}вет".
    #[test]
    fn a_letter_split_across_tokens_decodes_whole() {
        let mut rig = Rig::new("split", None, HOLD_MAX_MS);
        let toks = vec![
            Token::new(" пр", 0, 100, P),
            Token::new(vec![0xD0], 100, 150, P),
            Token::new(vec![0xB8], 150, 200, P),
            Token::new("вет", 200, 300, P),
            Token::new(" мир", 320, 500, P),
        ];
        assert_eq!(toks[1].text(), "\u{FFFD}", "one half alone is not a letter");
        let transcript = Transcript::new(toks.clone(), false);
        assert_eq!(transcript.text(), "привет мир");
        let lines = rig.lines(&[utt(Side::Me, toks, 1000, 5_000_000, 5_001_000, false)]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (text, payload) = body(&lines[0]);
        assert_eq!(text, "привет мир");
        assert_eq!(payload, "5000000,5000300,привет;5000320,5000500,мир");
    }

    /// A byte that is not UTF-8 at all, even joined, is U+FFFD in the text and the payload — no panic.
    #[test]
    fn a_lone_invalid_byte_is_a_replacement_char() {
        let toks = vec![
            Token::new(" a", 0, 100, P),
            Token::new(vec![0xFF], 100, 150, P),
            Token::new("b", 150, 200, P),
            Token::new(vec![b' ', 0xD0], 220, 300, P),
        ];
        let p = piece(&Transcript::new(toks, false), 0, 1000);
        assert_eq!(p.text, "a\u{FFFD}b \u{FFFD}");
        assert_eq!(p.payload, "0,200,a\u{FFFD}b;220,300,\u{FFFD}");
    }

    /// `check_a_phrase_of_only_markers_is_voice`
    #[test]
    fn a_phrase_of_only_markers_is_voice() {
        let mut rig = Rig::new("onlymarkers", None, HOLD_MAX_MS);
        let toks = rig_toks(&[("[_BEG_]", 0, 10), ("[_TT_5]", 20, 30), ("[_EOT_]", 40, 50)]);
        let lines = rig.lines(&[utt(Side::Me, toks, 4000, 3_000_000, 3_004_000, false)]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let fields: Vec<&str> = lines[0].split(' ').take(3).collect();
        assert_eq!(fields, ["TULPA-EARS-LINE", "3000000", "3004000"]);
        assert!(lines[0].ends_with(" [me] <voice 4.0s>"), "{lines:?}");
        assert!(
            !lines[0].contains('\t'),
            "an unread phrase carries no words payload"
        );
    }

    /// `check_credit_leaks_publish_as_voice`: the six real captures, each `<voice 5.0s>` with no payload.
    #[test]
    fn credit_leaks_publish_as_voice() {
        let mut rig = Rig::new("credits", None, HOLD_MAX_MS);
        let credits = [
            ME_1795069373,
            ME_1795069478,
            ME_1795069415,
            ME_1795069395,
            ME_1795069407,
            ME_1795069451,
        ];
        let utts: Vec<Heard> = (1_u64..)
            .zip(credits)
            .map(|(i, c)| {
                utt(
                    Side::Me,
                    fx(c),
                    5000,
                    1_000_000 * i,
                    1_000_000 * i + 5000,
                    false,
                )
            })
            .collect();
        let lines = rig.lines(&utts);
        assert_eq!(lines.len(), credits.len(), "{lines:?}");
        for line in &lines {
            assert!(line.ends_with(" [me] <voice 5.0s>"), "{line}");
            assert!(
                !line.contains('\t'),
                "a leaked credit carries no words: {line}"
            );
        }
    }

    /// One phrase through the assembler: its text and payload, and how many leak lines its piece dropped.
    fn said_as(tag: &str, toks: &[(&str, u64, u64)]) -> (String, String, usize) {
        let mut rig = Rig::new(tag, None, HOLD_MAX_MS);
        let h = utt(Side::Me, rig_toks(toks), 5000, 1_000_000, 1_005_000, false);
        let leaks = piece(&h.transcript, h.t0_ms, h.ms).leaks;
        let lines = rig.lines(&[h]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (text, payload) = body(&lines[0]);
        (text.to_owned(), payload.to_owned(), leaks)
    }

    /// Q2 P6: a leak line never takes a looped text's one kept copy along; the copies left decide the loop. A line
    /// ends at a segment bound (#0454 B1 P1), so a credit segment before the copies leaves the first copy kept, and
    /// a leak line opening the first copy's own segment drops before the loop is cut, leaving that copy kept.
    #[test]
    fn a_leak_line_never_takes_the_one_kept_copy_of_a_looped_text() {
        let seg = |n: usize, toks: &[(&str, u64, u64)]| -> Vec<Token> {
            toks.iter()
                .map(|&(t, a, b)| Token::new(t, a, b, P).in_segment(n))
                .collect()
        };
        let toks: Vec<Token> = [
            seg(
                0,
                &[
                    (" Thanks", 0, 100),
                    (" for", 100, 200),
                    (" watching", 200, 300),
                ],
            ),
            seg(1, &[(" Go", 300, 400), (" home.", 400, 500)]),
            seg(2, &[(" Go", 500, 600), (" home.", 600, 700)]),
            seg(3, &[(" Go", 700, 800), (" home.", 800, 900)]),
            seg(4, &[(" Go", 900, 1000), (" home.", 1000, 1100)]),
        ]
        .concat();
        let p = piece(&Transcript::new(toks, false), 0, 1100);
        assert_eq!(
            (p.text.as_str(), p.payload.as_str(), p.unread, p.leaks),
            ("Go home.", "300,400,Go;400,500,home.", None, 1)
        );
        // The leak line inside the segment that holds the first copy: at its start, the one place a credit is
        // recognised in a segment (#0454 B1 D8'; after the copy, as D1 first had it, the credit's words are speech).
        let toks: Vec<Token> = [
            seg(
                0,
                &[
                    (" Thanks", 0, 100),
                    (" for", 100, 200),
                    (" watching.", 200, 300),
                    (" Go", 300, 400),
                    (" home.", 400, 500),
                ],
            ),
            seg(1, &[(" Go", 500, 600), (" home.", 600, 700)]),
            seg(2, &[(" Go", 700, 800), (" home.", 800, 900)]),
            seg(3, &[(" Go", 900, 1000), (" home.", 1000, 1100)]),
        ]
        .concat();
        let p = piece(&Transcript::new(toks, false), 0, 1100);
        assert_eq!(
            (p.text.as_str(), p.payload.as_str(), p.unread, p.leaks),
            ("Go home.", "300,400,Go;400,500,home.", None, 1)
        );
    }

    /// #0454 B1 D4: a whole credit text only ever ends a line that a start already makes a credit, so the cut after
    /// it narrows what a credit takes and never widens what matches.
    #[test]
    fn every_whole_credit_text_opens_with_a_leak_start() {
        for w in WHOLE_CREDITS {
            let w = normal(w);
            assert!(
                LEAK_STARTS.iter().any(|(_, s)| w.starts_with(&normal(s))),
                "{w}"
            );
        }
    }

    /// E2 P1: a leak line is dropped by itself, wherever it sits; the words beside it are published, and the
    /// payload holds exactly those words. Not "a text that begins with a leak is all leak", not "everything from a
    /// leak on", not "a text that contains a leak is all leak".
    #[test]
    fn a_leak_line_is_dropped_alone_and_the_words_beside_it_are_published() {
        // Before the words, ended by its stop.
        assert_eq!(
            said_as(
                "leak-first",
                &[
                    (" Продолжение", 0, 400),
                    (" следует", 400, 800),
                    ("...", 800, 900),
                    (" Hello", 1000, 1300),
                    (" world", 1300, 1600),
                ]
            ),
            (
                "Hello world".to_owned(),
                "1001000,1001300,Hello;1001300,1001600,world".to_owned(),
                1
            )
        );
        // After the words, with a stop between.
        assert_eq!(
            said_as(
                "leak-last-stop",
                &[
                    (" Hello", 0, 300),
                    (" world", 300, 600),
                    (".", 600, 650),
                    (" Субтитры", 700, 1100),
                    (" создавал", 1100, 1500),
                    (" DimaTorzok", 1500, 2000),
                ]
            ),
            (
                "Hello world.".to_owned(),
                "1000000,1000300,Hello;1000300,1000650,world.".to_owned(),
                1
            )
        );
        // After the words with no stop between, in one segment: mid-sentence, so the credit's words are speech (#0454 B1
        // D8; the real 59.8 s piece's credit is a segment of its own and still drops).
        assert_eq!(
            said_as(
                "leak-last-nostop",
                &[
                    (" Hello", 0, 300),
                    (" world", 300, 600),
                    (" Редактор", 700, 1000),
                    (" субтитров", 1000, 1300),
                    (" А", 1300, 1350),
                    (".", 1350, 1400),
                    ("Семкин", 1400, 1700),
                    (" Корректор", 1700, 2000),
                    (" А", 2000, 2050),
                    (".", 2050, 2100),
                    ("Егорова", 2100, 2400),
                ]
            ),
            (
                "Hello world Редактор субтитров А.Семкин Корректор А.Егорова".to_owned(),
                "1000000,1000300,Hello;1000300,1000600,world;1000700,1001000,Редактор;1001000,1001300,субтитров;\
                 1001300,1001700,А.Семкин;1001700,1002000,Корректор;1002000,1002400,А.Егорова"
                    .to_owned(),
                0
            )
        );
    }

    /// #0454 B1 F2: a silence filler beside a real word is speech, wherever it sits: only a piece of nothing but
    /// fillers and credits drops it.
    #[test]
    fn a_silence_filler_beside_speech_is_published() {
        // After the words past a pause: a line of its own.
        assert_eq!(
            said_as(
                "leak-after-pause",
                &[
                    (" Hello", 0, 300),
                    (" world", 300, 600),
                    ("Thank", 1500, 1700),
                    (" you", 1700, 1900),
                    (".", 1900, 2000),
                ]
            ),
            (
                "Hello world<pause 0.9s>Thank you.".to_owned(),
                "1000000,1000300,Hello;1000300,1000600,world;1001500,1001700,Thank;1001700,1002000,you."
                    .to_owned(),
                0
            )
        );
        // A silence filler in the middle of speech: published as said, with its timings.
        assert_eq!(
            said_as(
                "leak-middle",
                &[
                    (" One", 0, 300),
                    (" two", 300, 600),
                    (".", 600, 650),
                    (" Thank", 700, 900),
                    (" you", 900, 1100),
                    (".", 1100, 1150),
                    (" Three", 1250, 1500),
                    (" four", 1500, 1800),
                    (".", 1800, 1850),
                ]
            ),
            (
                "One two. Thank you. Three four.".to_owned(),
                "1000000,1000300,One;1000300,1000650,two.;1000700,1000900,Thank;1000900,1001150,you.;\
                 1001250,1001500,Three;1001500,1001850,four."
                    .to_owned(),
                0
            )
        );
    }

    /// E2 r2 R4: a credit signed with the subtitler's name, glued to speech with no stop, pause or listed opening,
    /// is dropped alone: the words before it and after it are published, the credit's words are not (with its
    /// `Субтитры …` opening or without one); the gap it leaves is a pause, as any gap between words is.
    #[test]
    fn a_signed_credit_glued_to_speech_takes_no_words_with_it() {
        let words = [(" Hello", 0, 300), (" world", 300, 600)];
        let hello = || "1000000,1000300,Hello;1000300,1000600,world".to_owned();
        for (tag, credit) in [
            (
                "signed-opening",
                &[
                    (" Субтитры", 600, 900),
                    (" делал", 900, 1200),
                    (" Dima", 1200, 1400),
                    ("Tor", 1400, 1500),
                    ("zok", 1500, 1600),
                ][..],
            ),
            ("signed-name", &[(" DimaTorzok", 600, 1000)][..]),
        ] {
            let toks: Vec<_> = words.iter().chain(credit).copied().collect();
            assert_eq!(
                said_as(tag, &toks),
                ("Hello world".to_owned(), hello(), 1),
                "{tag}"
            );
        }
        assert_eq!(
            said_as(
                "signed-between",
                &[
                    (" Hello", 0, 300),
                    (" Субтитры", 300, 600),
                    (" сделал", 600, 900),
                    (" DimaTorzok", 900, 1200),
                    (" world", 1200, 1500),
                ]
            ),
            (
                "Hello<pause 0.9s> world".to_owned(),
                "1000000,1000300,Hello;1001200,1001500,world".to_owned(),
                1
            )
        );
    }

    /// #0454 B1 D11, D8': credit words after any other word of their whisper segment are speech, published as said
    /// and unflagged, with a stop before them or none: a credit is recognised only where a segment starts.
    #[test]
    fn credit_words_after_speech_in_one_segment_are_speech() {
        for (tag, toks, text) in [
            (
                "d11-no-stop",
                &[
                    (" мы", 0, 100),
                    (" обсудили", 100, 200),
                    (" Подпишись", 200, 300),
                    (" на", 300, 400),
                    (" канал", 400, 500),
                ][..],
                "мы обсудили Подпишись на канал",
            ),
            (
                "d8-after-stop",
                &[
                    (" Он", 0, 100),
                    (" ушёл.", 100, 200),
                    (" ПОДПИШИСЬ", 200, 300),
                    (" НА", 300, 400),
                    (" КАНАЛ", 400, 500),
                    (" Мы", 500, 600),
                    (" ждём.", 600, 700),
                ][..],
                "Он ушёл. ПОДПИШИСЬ НА КАНАЛ Мы ждём.",
            ),
        ] {
            let p = piece(&Transcript::new(rig_toks(toks), false), 0, 1000);
            assert_eq!(
                (p.text.as_str(), p.leaks, p.flagged.len()),
                (text, 0, 0),
                "{tag}"
            );
        }
    }

    /// #0454 B1 D12: a signed credit is a line of its own wherever it sits, inside a segment after speech too, and is
    /// dropped: the signature is a hallucination token, not speech. One per signed family.
    #[test]
    fn a_signed_credit_inside_a_segment_is_still_dropped() {
        let around = |credit: &[&str]| -> Vec<Token> {
            let words: Vec<&str> = [" Он", " сказал"]
                .into_iter()
                .chain(credit.iter().copied())
                .chain([" и", " ушёл"])
                .collect();
            words
                .iter()
                .zip((0_u64..).step_by(100))
                .map(|(w, at)| Token::new(*w, at, at.saturating_add(100), P))
                .collect()
        };
        for (family, credit) in [
            (
                "subtitles-created",
                &[" Субтитры", " создавал", " DimaTorzok"][..],
            ),
            (
                "subtitles-made",
                &[" Субтитры", " сделал", " DimaTorzok"][..],
            ),
            ("credit-name", &[" DimaTorzok"][..]),
        ] {
            let toks = around(credit);
            let p = piece(&Transcript::new(toks.clone(), false), 0, 1000);
            assert_eq!(
                (p.text.as_str(), p.leaks),
                ("Он сказал и ушёл", 1),
                "{family}"
            );
            let line = joined(&toks.iter().skip(2).take(credit.len()).collect::<Vec<_>>());
            assert_eq!(credit_family(&line), Some(family), "{family}");
        }
    }

    /// E2 P1: a sentence that merely holds a listed word is speech, a filler inside a longer line included; and a
    /// text of two leak lines and nothing else is unread as `credit-leak`, with no payload.
    #[test]
    fn a_list_word_inside_speech_is_speech_and_two_leak_lines_alone_are_unread() {
        for (tag, toks, text) in [
            (
                "thanks-inside",
                &[
                    (" I", 0, 100),
                    (" said", 100, 300),
                    (" thank", 300, 500),
                    (" you", 500, 700),
                    (" to", 700, 800),
                    (" him", 800, 1000),
                    (".", 1000, 1050),
                ][..],
                "I said thank you to him.",
            ),
            (
                "filler-inside",
                &[
                    (" I", 0, 100),
                    (" said", 100, 300),
                    (" Thank", 300, 500),
                    (" you", 500, 700),
                    (".", 700, 750),
                ][..],
                "I said Thank you.",
            ),
        ] {
            let (said, payload, leaks) = said_as(tag, toks);
            assert_eq!((said.as_str(), leaks), (text, 0), "{tag}");
            assert_eq!(
                payload.split(';').count(),
                toks.iter().filter(|t| t.0.starts_with(' ')).count(),
                "{tag}"
            );
        }
        let two = rig_toks(&[
            (" Субтитры", 0, 400),
            (" создавал", 400, 800),
            (" DimaTorzok", 800, 1200),
            (" Редактор", 1300, 1600),
            (" субтитров", 1600, 1900),
            (" А", 1900, 1950),
            (".", 1950, 2000),
            ("Семкин", 2000, 2300),
        ]);
        let p = piece(&Transcript::new(two.clone(), false), 0, 3000);
        assert_eq!(p.leaks, 2);
        assert_eq!(
            p.unread.map(|u| u.to_string()),
            Some("credit-leak nt=8 m=8 maxrun=1".to_owned())
        );
        let mut rig = Rig::new("two-leaks", None, HOLD_MAX_MS);
        let lines = rig.lines(&[utt(Side::Me, two, 3000, 1_000_000, 1_003_000, false)]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with(" [me] <voice 3.0s>"), "{lines:?}");
    }

    /// E2 r2 R1: heard.log is as it was before the leak filter went per line: six columns, and an unread piece's own
    /// line only from a real utterance's length. A credits-only piece of 1 s gets only its phrase's line, one of
    /// 3 s its rule's line too; a piece of words and a credit gets the words' phrase line. No raw text anywhere.
    #[test]
    fn heard_log_keeps_its_six_columns_and_its_length_gate() {
        let mut rig = Rig::new("r1", None, HOLD_MAX_MS);
        let mixed = rig_toks(&[
            (" Hello", 0, 300),
            (" world", 300, 600),
            (".", 600, 650),
            (" Субтитры", 700, 1100),
            (" создавал", 1100, 1500),
            (" DimaTorzok", 1500, 2000),
        ]);
        let lines = rig.lines(&[
            utt(
                Side::Me,
                fx(ME_1795069373),
                1000,
                1_000_000,
                1_001_000,
                false,
            ),
            utt(
                Side::Me,
                fx(ME_1795069373),
                3000,
                2_000_000,
                2_003_000,
                false,
            ),
            utt(Side::Me, mixed, 3000, 3_000_000, 3_003_000, false),
        ]);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let log = rig.log();
        let rows: Vec<Vec<&str>> = log
            .iter()
            .map(|l| l[1..].iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec![
                    "me",
                    "1.0",
                    "pauses",
                    "<voice 1.0s>",
                    "emitted:voice-unread"
                ],
                vec![
                    "me",
                    "3.0",
                    "pauses",
                    "c2000000",
                    "voice-unread:credit-leak nt=16 m=14 maxrun=1"
                ],
                vec![
                    "me",
                    "3.0",
                    "pauses",
                    "<voice 3.0s>",
                    "emitted:voice-unread"
                ],
                vec!["me", "3.0", "pauses", "Hello world.", "emitted"],
            ]
        );
        let all = fs::read_to_string(rig.dir.join(HEARD_LOG)).unwrap_or_default();
        assert!(
            !all.contains("DimaTorzok") && !all.contains("Субтитры"),
            "{all}"
        );
    }

    /// `check_real_speech_mentioning_subtitles_passes`
    #[test]
    fn real_speech_mentioning_subtitles_passes() {
        let mut rig = Rig::new("near-miss", None, HOLD_MAX_MS);
        let toks = rig_toks(&[
            (" Я", 0, 300),
            (" делаю", 310, 600),
            (" субтитры", 610, 900),
            (" к", 910, 1000),
            (" роли", 1010, 1200),
        ]);
        let lines = rig.lines(&[utt(Side::Me, toks, 5000, 1_000_000, 1_005_000, false)]);
        let (text, payload) = body(&lines[0]);
        assert!(
            text.contains("субтитры") && !text.contains("<voice"),
            "{text}"
        );
        assert_eq!(
            payload,
            "1000000,1000300,Я;1000310,1000600,делаю;1000610,1000900,субтитры;1000910,1001000,к;1001010,1001200,роли"
        );
    }

    /// `check_a_repeat_token_loop_is_voice_but_five_is_speech`
    #[test]
    fn a_repeat_token_loop_is_voice_but_five_is_speech() {
        let mut rig = Rig::new("repeat", None, HOLD_MAX_MS);
        let looped: Vec<Token> = (0..9)
            .map(|i| Token::new(" ПОДПИШИСЬ", i * 500 + 100, i * 500 + 400, P))
            .collect();
        let five: Vec<Token> = (0..5)
            .map(|i| Token::new(" ну", i * 300 + 100, i * 300 + 250, P))
            .collect();
        let lines = rig.lines(&[
            utt(Side::Me, looped, 8000, 1_000_000, 1_008_000, false),
            utt(Side::Me, five, 4000, 2_000_000, 2_004_000, false),
        ]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].ends_with(" [me] <voice 8.0s>"), "{lines:?}");
        assert!(!lines[0].contains('\t'), "{lines:?}");
        assert_eq!(
            lines[1].split_once("[me] ").map(|(_, b)| b),
            Some(
                "ну ну ну ну ну\t2000100,2000250,ну;2000400,2000550,ну;2000700,2000850,ну;\
                 2001000,2001150,ну;2001300,2001450,ну"
            )
        );
    }

    /// `check_real_and_madeup_speech_fixtures_pass_through`
    #[test]
    fn real_and_madeup_speech_fixtures_pass_through() {
        let mut rig = Rig::new("pass", None, HOLD_MAX_MS);
        let lines = rig.lines(&[
            utt(Side::Them, fx(THEM_LAST), 4000, 1_000_000, 1_004_000, false),
            utt(
                Side::Them,
                fx(ME_1795069355),
                6000,
                2_000_000,
                2_006_000,
                false,
            ),
            utt(
                Side::Me,
                fx(ME_1795069444),
                3000,
                3_000_000,
                3_003_000,
                false,
            ),
        ]);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let (door, music, empty) = (&lines[0], &lines[1], &lines[2]);
        assert!(door.contains("Проверка двери"), "{door}");
        assert!(
            door.starts_with("TULPA-EARS-LINE 1000000 1004000 ") && door.contains('\t'),
            "{door}"
        );
        assert!(music.contains("Чтобы приобретать"), "{music}");
        assert!(
            music.starts_with("TULPA-EARS-LINE 2000000 2006000 ") && music.contains('\t'),
            "{music}"
        );
        assert!(
            // `me-1795069444` has no token at all: `<no speech>` here, `<voice>` in the script (see the top).
            empty.ends_with(" [me] <no speech 3.0s>") && !empty.contains('\t'),
            "{empty}"
        );
    }

    // ---- the line, the hold, the echo filter and heard.log ----

    /// The whole line, byte for byte, as the script's two printf forms print it: with the payload after one TAB
    /// (and the intonation tag before it), and without (a token carrying the wire's `;`: text, no TAB).
    #[test]
    fn the_line_is_byte_exact_with_and_without_payload() {
        let mut rig = Rig::new("exact", None, HOLD_MAX_MS);
        let mut with = utt(
            Side::Them,
            rig_toks(&[(" Раз", 0, 300), (",", 300, 320), (" два", 900, 1200)]),
            2000,
            1_795_359_360_000,
            1_795_359_362_000,
            false,
        );
        with.contour = Contour::Rising;
        let without = utt(
            Side::Me,
            rig_toks(&[(" да", 0, 200), (";", 200, 210), (" нет", 300, 500)]),
            1000,
            1_795_359_363_000,
            1_795_359_364_000,
            false,
        );
        let lines = rig.lines(&[with, without]);
        assert_eq!(
            lines,
            [
                "TULPA-EARS-LINE 1795359360000 1795359362000 14:56:07 [them] Раз,<pause 0.6s> два \
                 [intonation: rising]\t1795359360000,1795359360320,Раз,;1795359360900,1795359361200,два",
                "TULPA-EARS-LINE 1795359363000 1795359364000 14:56:07 [me] да; нет",
            ]
        );
    }

    /// The tag only for rising and falling; flat and unknown say nothing.
    #[test]
    fn intonation_tag_only_rising_or_falling() {
        let mut rig = Rig::new("tone", None, HOLD_MAX_MS);
        for (c, tag) in [
            (Contour::Rising, " [intonation: rising]"),
            (Contour::Falling, " [intonation: falling]"),
            (Contour::Flat, ""),
            (Contour::Unknown, ""),
        ] {
            let mut h = utt(
                Side::Me,
                rig_toks(&[(" так", 0, 300)]),
                1000,
                5_000,
                6_000,
                false,
            );
            h.contour = c;
            let lines = rig.lines(&[h]);
            assert_eq!(
                lines,
                [format!(
                    "TULPA-EARS-LINE 5000 6000 14:56:07 [me] так{tag}\t5000,5300,так"
                )]
            );
        }
    }

    /// A held turn on one side is not touched by the other side's phrases.
    #[test]
    fn the_sides_hold_apart() {
        let mut rig = Rig::new("sides", None, HOLD_MAX_MS);
        let lines = rig.lines(&[
            utt(
                Side::Me,
                rig_toks(&[("один", 0, 400)]),
                6000,
                2_000_000,
                2_006_000,
                true,
            ),
            utt(
                Side::Them,
                rig_toks(&[(" алло", 0, 400)]),
                1000,
                2_003_000,
                2_004_000,
                false,
            ),
            utt(
                Side::Me,
                rig_toks(&[(" два", 0, 300)]),
                3000,
                2_006_000,
                2_009_000,
                false,
            ),
        ]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("[them] алло\t"), "{lines:?}");
        assert!(
            lines[1].starts_with("TULPA-EARS-LINE 2000000 2009000 "),
            "{lines:?}"
        );
        assert!(lines[1].contains("[me] один два\t"), "{lines:?}");
    }

    /// A phrase carrying one of the prompt's adjacent pairs is whisper echoing its prompt: no line, and heard.log
    /// says `dropped:prompt-echo`; the next phrase is emitted and logged as such; one with no token at all as
    /// no-speech, after its piece's own line.
    #[test]
    fn a_prompt_echo_that_is_also_a_credit_is_judged_an_echo() {
        // #0454 B1 D7: the prompt-echo rule reads the text before any credit is dropped. Folded, the pair is the
        // to-be-continued credit; it is still the prompt's own vocabulary, so it is an echo, as the script's rig says.
        let mut rig = Rig::new(
            "echo-credit",
            Some("Продолжение, следует, подписывайтесь"),
            HOLD_MAX_MS,
        );
        let lines = rig.lines(&[
            utt(
                Side::Me,
                rig_toks(&[(" Продолжение,", 0, 400), (" следует", 500, 900)]),
                3000,
                10_000,
                13_000,
                false,
            ),
            utt(
                Side::Me,
                rig_toks(&[(" настоящая", 0, 400), (" фраза", 500, 900)]),
                3000,
                20_000,
                23_000,
                false,
            ),
        ]);
        assert_eq!(lines.len(), 1, "the echo leaves no line: {lines:?}");
        assert!(lines[0].contains("[me] настоящая фраза\t"), "{lines:?}");
        // The phrase's own heard.log line (the piece's unread record comes first, as for any unread piece).
        let log = rig.log();
        assert_eq!(
            log.get(1).map(|l| (l[4].as_str(), l[5].as_str())),
            Some(("Продолжение, следует", "dropped:prompt-echo")),
            "{log:?}"
        );
    }

    #[test]
    fn prompt_echo_is_dropped_and_logged() {
        let mut rig = Rig::new("echo", Some("Tulpa,  tmux, whisper"), HOLD_MAX_MS);
        let lines = rig.lines(&[
            utt(
                Side::Me,
                rig_toks(&[(" Tulpa", 0, 400), (",", 400, 420), (" tmux", 500, 900)]),
                3000,
                10_000,
                13_000,
                false,
            ),
            utt(
                Side::Them,
                rig_toks(&[(" tmux", 0, 400), (" тут", 500, 900)]),
                2000,
                20_000,
                22_000,
                false,
            ),
            utt(Side::Them, rig_toks(&[]), 2500, 30_000, 32_500, false),
        ]);
        assert_eq!(lines.len(), 2, "the echo leaves no line: {lines:?}");
        assert!(lines[0].contains("[them] tmux тут\t"), "{lines:?}");
        let stamp = "2026-11-22T14:56:07Z";
        assert_eq!(
            rig.log(),
            [
                [
                    stamp,
                    "me",
                    "3.0",
                    "pauses",
                    "Tulpa, tmux",
                    "dropped:prompt-echo"
                ],
                [stamp, "them", "2.0", "pauses", "tmux тут", "emitted"],
                // A real utterance (2.5 s) unread: its piece, by name, with the rule, first.
                [
                    stamp,
                    "them",
                    "2.5",
                    "pauses",
                    "c30000",
                    "no-speech nt=0 m=0 maxrun=0"
                ],
                [
                    stamp,
                    "them",
                    "2.5",
                    "pauses",
                    "<no speech 2.5s>",
                    "emitted:no-speech"
                ],
            ]
        );
    }

    /// A piece is labelled voice only when something says it was: no token at all (the VAD found no speech) is
    /// `<no speech N.Ns>` on the line and in heard.log, its piece's own heard.log line `no-speech <counts>`, the
    /// phrase's outcome `emitted:no-speech`; each other unread rule keeps `<voice N.Ns>`,
    /// `voice-unread:<rule> <counts>` and `emitted:voice-unread`.
    #[test]
    fn only_a_piece_with_voice_in_it_is_labelled_voice() {
        let mut rig = Rig::new("no-speech", None, HOLD_MAX_MS);
        let looped: Vec<Token> = (0..6).map(|i| Token::new(" ла", i, i + 1, P)).collect();
        let cases: [(Why, Transcript, &str, &str, &str); 5] = [
            (
                Why::NoTokens,
                Transcript::new(Vec::new(), false),
                "no-speech nt=0 m=0 maxrun=0",
                "<no speech 3.0s>",
                "emitted:no-speech",
            ),
            (
                Why::MarkersOnly,
                Transcript::new(Vec::new(), false).with_specials(2),
                "voice-unread:markers-only nt=2 m=0 maxrun=0",
                "<voice 3.0s>",
                "emitted:voice-unread",
            ),
            (
                Why::EmptyText,
                Transcript::new(rig_toks(&[(" ", 0, 100)]), false),
                "voice-unread:empty-text nt=1 m=1 maxrun=1",
                "<voice 3.0s>",
                "emitted:voice-unread",
            ),
            (
                Why::CreditLeak,
                Transcript::new(fx(ME_1795069373), false),
                "voice-unread:credit-leak nt=16 m=14 maxrun=1",
                "<voice 3.0s>",
                "emitted:voice-unread",
            ),
            (
                Why::MaxRun,
                Transcript::new(looped, false),
                "voice-unread:maxrun nt=6 m=6 maxrun=6",
                "<voice 3.0s>",
                "emitted:voice-unread",
            ),
        ];
        for (i, (why, transcript, own, text, outcome)) in (1_u64..).zip(cases) {
            let t0 = 1_000_000 * i;
            let h = Heard {
                transcript,
                ..utt(Side::Me, Vec::new(), 3000, t0, t0 + 3000, false)
            };
            assert_eq!(
                piece(&h.transcript, h.t0_ms, h.ms).unread.map(|u| u.why),
                Some(why)
            );
            let lines = rig.lines(&[h]);
            assert_eq!(lines.len(), 1, "{why:?}: {lines:?}");
            assert!(
                lines[0].ends_with(&format!(" [me] {text}")),
                "{why:?}: {lines:?}"
            );
            let log = rig.log();
            let tail: Vec<(&str, &str)> = log[log.len() - 2..]
                .iter()
                .map(|l| (l[4].as_str(), l[5].as_str()))
                .collect();
            assert_eq!(
                tail,
                [(format!("c{t0}").as_str(), own), (text, outcome)],
                "{why:?}"
            );
        }
    }

    /// The outcome is the rule's, never the text's: a phrase whisper read whose words happen to begin with `<voice `
    /// is read text, `emitted`, not `emitted:voice-unread`.
    #[test]
    fn read_text_that_looks_like_a_placeholder_is_read_text() {
        let mut rig = Rig::new("voice-text", None, HOLD_MAX_MS);
        let lines = rig.lines(&[utt(
            Side::Me,
            rig_toks(&[("<voice", 0, 400), (" 2.0s>", 400, 800)]),
            3000,
            10_000,
            13_000,
            false,
        )]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains(" [me] <voice 2.0s>\t"), "{lines:?}");
        let log = rig.log();
        let all: Vec<(&str, &str)> = log.iter().map(|l| (l[4].as_str(), l[5].as_str())).collect();
        assert_eq!(all, [("<voice 2.0s>", "emitted")]);
    }

    /// Fable F10: the live path's heard.log is bounded too. An assembler built as `--ears` builds it (no bound asked
    /// for) rotates a heard.log that has passed 20 MB to `heard.log.1` and starts a new one.
    #[test]
    fn the_live_heard_log_rotates_past_its_bound() {
        let rig = Rig::new("live-bound", None, HOLD_MAX_MS);
        let log = rig.dir.join(HEARD_LOG);
        // Past the bound already, without writing 20 MB: a sparse file.
        let big = fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .and_then(|f| f.set_len(crate::log::BOUND.saturating_add(1)));
        assert!(big.is_ok(), "{big:?}");
        let mut asm = Assembler::new(&rig.dir, None, HOLD_MAX_MS);
        let h = utt(
            Side::Me,
            rig_toks(&[(" a", 0, 100)]),
            1000,
            1_000,
            2_000,
            false,
        );
        assert_eq!(asm.take(&h, NOW).len(), 1);
        let old = rig.dir.join(format!("{HEARD_LOG}.1"));
        assert!(old.exists(), "the full log was not rotated");
        let _line = asm.take(&h, NOW);
        let new = fs::metadata(&log).map_or(u64::MAX, |m| m.len());
        assert!(new < 1_000, "a new log was started: {new} bytes");
    }

    /// heard.log is created 0600, and an existing one is made 0600; tabs and newlines in a text are spaces there.
    #[test]
    fn heard_log_is_0600_and_one_line_per_phrase() {
        let rig = Rig::new("mode", None, HOLD_MAX_MS);
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
        let log = rig.dir.join(HEARD_LOG);
        assert_eq!(mode(&log), Some(0o600));
        fs::set_permissions(&log, Permissions::from_mode(0o644)).ok();
        let mut asm = Assembler::new(&rig.dir, None, HOLD_MAX_MS);
        assert_eq!(mode(&log), Some(0o600), "made 0600 at start");
        let h = utt(
            Side::Me,
            rig_toks(&[(" a", 0, 100), ("\tb", 100, 200), ("\nc", 200, 300)]),
            1000,
            1_000,
            2_000,
            false,
        );
        let line = asm.take(&h, NOW);
        assert_eq!(
            line,
            ["TULPA-EARS-LINE 1000 2000 14:56:07 [me] a\tb c"],
            "a TAB in a token: text only; a newline never splits the line"
        );
        assert_eq!(
            rig.log(),
            [[
                "2026-11-22T14:56:07Z",
                "me",
                "1.0",
                "pauses",
                "a b c",
                "emitted"
            ]]
        );
    }

    /// The prompt's pairs: spaces squeezed, per line, every two neighbours.
    #[test]
    fn prompt_pairs_follow_the_scripts_rule() {
        assert_eq!(prompt_pairs("один,  два, три"), ["один, два", "два, три"]);
        assert_eq!(prompt_pairs("a, b\nc, d, e"), ["a, b", "c, d", "d, e"]);
        assert_eq!(prompt_pairs(""), [] as [String; 0]);
        assert_eq!(prompt_pairs("один два"), [] as [String; 0]);
        assert!(
            prompt_pairs("a,b").is_empty(),
            "the separator is comma-space"
        );
    }

    /// `%.1f` of ms/1000 as awk prints it (C's round-half-even on the exact binary value).
    #[test]
    fn tenths_match_awk() {
        for (ms, s) in [
            (2250, "2.2"),
            (2350, "2.4"),
            (2450, "2.5"),
            (50, "0.1"),
            (150, "0.1"),
            (250, "0.2"),
            (6400, "6.4"),
            (1050, "1.1"),
            (2149, "2.1"),
            (0, "0.0"),
        ] {
            assert_eq!(tenths(ms), s, "{ms}");
        }
    }

    #[test]
    fn utc_dates() {
        assert_eq!(
            utc(NOW),
            ("14:56:07".to_owned(), "2026-11-22T14:56:07Z".to_owned())
        );
        assert_eq!(utc(1_709_251_199_999).1, "2024-02-29T23:59:59Z");
        assert_eq!(utc(951_868_800_000).1, "2000-03-01T00:00:00Z");
        assert_eq!(utc(0).1, "1970-01-01T00:00:00Z");
        assert_eq!(utc(4_133_937_600_000).1, "2100-12-31T12:00:00Z");
    }

    #[test]
    fn hold_max_from_env() {
        assert_eq!(hold_max_from(None), Ok(HOLD_MAX_MS));
        assert_eq!(hold_max_from(Some("")), Ok(HOLD_MAX_MS));
        assert_eq!(hold_max_from(Some("10000")), Ok(10_000));
        assert!(hold_max_from(Some("5m")).is_err());
    }

    #[test]
    fn deaf_s_from_env_follows_the_scripts_rule() {
        assert_eq!(deaf_s_from(None), Ok(DEAF_S));
        assert_eq!(deaf_s_from(Some("")), Ok(DEAF_S));
        assert_eq!(deaf_s_from(Some("1")), Ok(1));
        assert_eq!(deaf_s_from(Some("86400")), Ok(86_400));
        for bad in ["0", "030", "86401", "-5", "5s", " 5", "1e3"] {
            assert!(deaf_s_from(Some(bad)).is_err(), "{bad}");
        }
    }

    /// Each rule an unread piece can fall to, with the counts the script logs beside it.
    #[test]
    fn an_unread_piece_names_its_rule() {
        let why = |t: Transcript| piece(&t, 0, 3000).unread.map(|u| u.to_string());
        let toks = |t: &[(&str, u64, u64)]| Transcript::new(rig_toks(t), false);
        assert_eq!(
            why(Transcript::new(Vec::new(), true)),
            Some("no-tokens nt=0 m=0 maxrun=0".to_owned())
        );
        // whisper-rs drops the markers but counts them; whisper-cli's JSON lists them as tokens: both are markers.
        assert_eq!(
            why(Transcript::new(Vec::new(), false).with_specials(3)),
            Some("markers-only nt=3 m=0 maxrun=0".to_owned())
        );
        assert_eq!(
            why(toks(&[("[_BEG_]", 0, 0), ("[_TT_150]", 3000, 3000)]).with_specials(1)),
            Some("markers-only nt=3 m=0 maxrun=0".to_owned())
        );
        assert_eq!(
            why(toks(&[(" ", 0, 100), ("  ", 100, 200)]).with_specials(2)),
            Some("empty-text nt=4 m=2 maxrun=1".to_owned())
        );
        assert_eq!(
            why(toks(&[(" Спасибо", 0, 400), (".", 400, 500)])),
            Some("credit-leak nt=2 m=2 maxrun=1".to_owned())
        );
        let looped: Vec<(&str, u64, u64)> =
            (0..7).map(|i| (" ла", i * 100, i * 100 + 90)).collect();
        assert_eq!(
            why(toks(&looped)),
            Some("maxrun nt=7 m=7 maxrun=7".to_owned())
        );
        assert_eq!(why(toks(&[(" да", 0, 300)])), None);
        // A token the parser ignores (no text, or ending before it starts) is not counted.
        assert_eq!(
            why(toks(&[("", 0, 10), ("x", 50, 40)])),
            Some("no-tokens nt=0 m=0 maxrun=0".to_owned())
        );
    }

    /// heard.log gets one line per unread piece of a real utterance (2.5 s or more), before the phrase's own line;
    /// a shorter one (a click) only the phrase's.
    #[test]
    fn a_real_unread_piece_is_logged_by_name() {
        let mut rig = Rig::new("unread", None, HOLD_MAX_MS);
        let markers = |ms, t0| Heard {
            transcript: Transcript::new(Vec::new(), false).with_specials(2),
            ..utt(Side::Me, Vec::new(), ms, t0, t0 + ms, false)
        };
        let lines = rig.lines(&[markers(2499, 10_000), markers(2500, 20_000)]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        let log = rig.log();
        let outcomes: Vec<(&str, &str, &str)> = log
            .iter()
            .map(|l| (l[2].as_str(), l[4].as_str(), l[5].as_str()))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("2.5", "<voice 2.5s>", "emitted:voice-unread"),
                (
                    "2.5",
                    "c20000",
                    "voice-unread:markers-only nt=2 m=0 maxrun=0"
                ),
                ("2.5", "<voice 2.5s>", "emitted:voice-unread"),
            ]
        );
    }

    /// The deaf-suspect streak: per side, consecutive markers-only / empty-text pieces add their ms; at the bound
    /// one line, from the streak's first `t0` to this piece's `t1`, naming this piece's rule; no-tokens and music
    /// reset it without re-arming; only text re-arms it.
    #[test]
    fn the_deaf_streak_says_once_until_text_rearms_it() {
        let mut rig = Rig::new("deaf", None, HOLD_MAX_MS);
        rig.asm = Assembler::new(&rig.dir, None, HOLD_MAX_MS).with_deaf_s(10);
        let deaf = |side, t0: u64| Heard {
            transcript: Transcript::new(Vec::new(), false).with_specials(1),
            ..utt(side, Vec::new(), 4000, t0, t0 + 4000, false)
        };
        let blank = |side, t0: u64| Heard {
            transcript: Transcript::new(rig_toks(&[(" ", 0, 100)]), false),
            ..utt(side, Vec::new(), 4000, t0, t0 + 4000, false)
        };
        let quiet = |side, t0: u64| utt(side, Vec::new(), 4000, t0, t0 + 4000, false);
        let text = |side, t0: u64| {
            utt(
                side,
                rig_toks(&[(" да", 0, 300)]),
                4000,
                t0,
                t0 + 4000,
                false,
            )
        };
        let said = |rig: &mut Rig, hs: &[Heard]| -> Vec<String> {
            rig.lines(hs)
                .into_iter()
                .filter(|l| l.contains("<deaf-suspect"))
                .collect()
        };
        // 8 s, a room-noise piece resets, then 12 s from 100_000: said once, at the third piece.
        let first = said(
            &mut rig,
            &[
                deaf(Side::Me, 10_000),
                deaf(Side::Me, 20_000),
                quiet(Side::Me, 30_000),
                deaf(Side::Me, 100_000),
                blank(Side::Me, 110_000),
                deaf(Side::Them, 115_000),
                deaf(Side::Me, 120_000),
                deaf(Side::Me, 130_000),
            ],
        );
        assert_eq!(
            first,
            ["TULPA-EARS-LINE 100000 124000 14:56:07 [me] <deaf-suspect markers-only 12s>"]
        );
        // Still deaf, and a room-noise reset: not armed again, nothing said.
        assert_eq!(
            said(
                &mut rig,
                &[
                    quiet(Side::Me, 140_000),
                    deaf(Side::Me, 150_000),
                    blank(Side::Me, 160_000),
                    deaf(Side::Me, 170_000),
                ]
            ),
            Vec::<String>::new()
        );
        // Text re-arms; the next streak is said again, the other side's own count apart.
        assert_eq!(
            said(
                &mut rig,
                &[
                    text(Side::Me, 180_000),
                    blank(Side::Me, 190_000),
                    deaf(Side::Them, 195_000),
                    deaf(Side::Me, 200_000),
                    blank(Side::Them, 205_000),
                    deaf(Side::Me, 210_000),
                ]
            ),
            [
                "TULPA-EARS-LINE 115000 209000 14:56:07 [them] <deaf-suspect empty-text 12s>",
                "TULPA-EARS-LINE 190000 214000 14:56:07 [me] <deaf-suspect markers-only 12s>",
            ]
        );
    }

    /// The python rig's synthetic tokens (text, from, to).
    fn rig_toks(toks: &[(&str, u64, u64)]) -> Vec<Token> {
        toks.iter()
            .map(|&(t, a, b)| Token::new(t, a, b, P))
            .collect()
    }

    // ---- real whisper-cli -ojf captures (2026-11-19), tokens verbatim: text, from, to, p ----

    /// `me-1795069373`.
    const ME_1795069373: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.924848),
        (" С", 0, 250, 0.122305),
        ("уб", 250, 750, 0.839652),
        ("т", 750, 930, 0.998744),
        ("ит", 1040, 1490, 0.999743),
        ("ры", 1590, 1980, 0.999605),
        (" созд", 2080, 3000, 0.240102),
        ("ав", 3000, 3500, 0.902283),
        ("ал", 3500, 3990, 0.997722),
        (" D", 3990, 4250, 0.998889),
        ("ima", 4250, 4920, 0.998953),
        ("T", 5030, 5250, 0.999033),
        ("or", 5250, 5650, 0.999548),
        ("z", 5750, 6000, 0.999371),
        ("ok", 6000, 6540, 0.997169),
        ("[_TT_327]", 6540, 6540, 0.0591524),
    ];
    /// `me-1795069478`.
    const ME_1795069478: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.886325),
        (" С", 0, 460, 0.234898),
        ("уб", 460, 1380, 0.888738),
        ("т", 1380, 1840, 0.999222),
        ("ит", 1840, 2760, 0.999733),
        ("ры", 2760, 3680, 0.999651),
        (" созд", 3680, 5520, 0.151373),
        ("ав", 5520, 6350, 0.875475),
        ("ал", 6460, 7290, 0.996559),
        (" D", 7400, 7820, 0.998767),
        ("ima", 7820, 9200, 0.99964),
        ("T", 9200, 9660, 0.998908),
        ("or", 9660, 10570, 0.999846),
        ("z", 10680, 10980, 0.999805),
        ("ok", 11090, 11990, 0.99883),
        ("[_TT_600]", 12000, 12000, 0.201085),
    ];
    /// `me-1795069415`.
    const ME_1795069415: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.838465),
        (" С", 10, 170, 0.0791897),
        ("уб", 170, 500, 0.707473),
        ("т", 610, 640, 0.999445),
        ("ит", 730, 980, 0.999657),
        ("ры", 1080, 1390, 0.99962),
        (" сделал", 1390, 2440, 0.352447),
        (" D", 2440, 2610, 0.988797),
        ("ima", 2610, 3120, 0.999586),
        ("T", 3130, 3300, 0.999152),
        ("or", 3300, 3650, 0.999882),
        ("z", 3650, 3820, 0.999777),
        ("ok", 3820, 4210, 0.999053),
        ("[_TT_211]", 4220, 4220, 0.100743),
    ];
    /// `me-1795069395`.
    const ME_1795069395: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.900555),
        (" Р", 0, 0, 0.0218326),
        ("ед", 10, 30, 0.866003),
        ("ак", 40, 70, 0.999676),
        ("тор", 70, 120, 0.997661),
        (" с", 120, 130, 0.999164),
        ("уб", 130, 160, 0.999359),
        ("т", 160, 170, 0.999687),
        ("ит", 170, 200, 0.99956),
        ("ров", 200, 240, 0.999739),
        (" А", 260, 260, 0.96707),
        (".", 310, 310, 0.999971),
        ("С", 320, 320, 0.995915),
        ("ем", 350, 350, 0.952129),
        ("ки", 350, 380, 0.999187),
        ("н", 380, 460, 0.999914),
        (" Кор", 460, 460, 0.940895),
        ("р", 460, 460, 0.999518),
        ("ектор", 460, 460, 0.999405),
        (" А", 460, 460, 0.999335),
        (".", 460, 460, 0.999949),
        ("Е", 460, 460, 0.997215),
        ("го", 460, 460, 0.999651),
        ("ров", 460, 460, 0.999601),
        ("а", 460, 460, 0.999596),
        ("[_TT_23]", 460, 460, 0.267513),
    ];
    /// `me-1795069407`.
    const ME_1795069407: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.906204),
        (" Р", 0, 20, 0.0261587),
        ("ед", 20, 60, 0.558961),
        ("ак", 60, 100, 0.999317),
        ("тор", 100, 160, 0.996635),
        (" с", 160, 180, 0.998143),
        ("уб", 180, 220, 0.999291),
        ("т", 220, 240, 0.999352),
        ("ит", 240, 280, 0.999438),
        ("ров", 280, 340, 0.999802),
        (" А", 340, 360, 0.952518),
        (".", 360, 420, 0.999914),
        ("С", 420, 440, 0.991609),
        ("ем", 440, 480, 0.949757),
        ("ки", 480, 520, 0.998254),
        ("н", 520, 540, 0.999853),
        (" Кор", 540, 540, 0.85446),
        ("р", 620, 620, 0.999283),
        ("ектор", 640, 670, 0.99923),
        (" А", 750, 750, 0.999195),
        (".", 760, 810, 0.999945),
        ("Е", 810, 830, 0.991596),
        ("го", 830, 870, 0.999584),
        ("ров", 870, 930, 0.9993),
        ("а", 930, 1040, 0.99957),
        ("[_TT_52]", 1040, 1040, 0.0140787),
    ];
    /// `me-1795069451`.
    const ME_1795069451: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.939038),
        (" Р", 0, 0, 0.123538),
        ("ед", 0, 10, 0.98105),
        ("ак", 10, 20, 0.999748),
        ("тор", 20, 40, 0.99831),
        (" с", 40, 40, 0.999106),
        ("уб", 40, 50, 0.999399),
        ("т", 50, 50, 0.999786),
        ("ит", 50, 60, 0.99956),
        ("ров", 60, 80, 0.999779),
        (" А", 80, 80, 0.967208),
        (".", 80, 100, 0.999962),
        ("С", 100, 100, 0.996434),
        ("ем", 100, 110, 0.942274),
        ("ки", 110, 120, 0.999371),
        ("н", 120, 240, 0.999908),
        (" Кор", 240, 240, 0.959945),
        ("р", 240, 240, 0.999731),
        ("ектор", 240, 240, 0.999504),
        (" А", 240, 240, 0.999146),
        (".", 240, 240, 0.999929),
        ("Е", 240, 240, 0.998094),
        ("го", 240, 240, 0.999682),
        ("ров", 240, 240, 0.999641),
        ("а", 240, 240, 0.999603),
        ("[_TT_12]", 240, 240, 0.397751),
    ];
    /// `them-last`.
    const THEM_LAST: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 0.983046),
        (" П", 30, 30, 0.915854),
        ("ров", 30, 130, 0.999388),
        ("ер", 130, 190, 0.999838),
        ("ка", 190, 240, 0.999947),
        (" дв", 270, 320, 0.999853),
        ("ери", 320, 420, 0.99991),
        (".", 420, 560, 0.937442),
        (" 1", 560, 590, 0.683416),
        ("-", 850, 850, 0.64918),
        ("2", 930, 1070, 0.999024),
        (".", 1070, 1300, 0.956021),
        ("[_TT_65]", 1300, 1300, 0.465716),
    ];
    /// `me-1795069355`.
    const ME_1795069355: &[(&str, u64, u64, f32)] = &[
        ("[_BEG_]", 0, 0, 1.0),
        (" Чтобы", 0, 180, 0.568762),
        (" при", 180, 290, 0.956762),
        ("обр", 290, 320, 0.316365),
        ("ет", 430, 470, 0.835754),
        ("ать", 470, 580, 0.956532),
        (",", 580, 650, 0.999935),
        (" что", 650, 760, 0.913373),
        (" получится", 760, 1090, 0.999298),
        (" с", 1090, 1120, 0.968254),
        ("ход", 1120, 1230, 0.999641),
        ("ить", 1230, 1310, 1.0),
        (" в", 1370, 1370, 0.932604),
        (" под", 1400, 1480, 0.981171),
        ("ъ", 1480, 1510, 0.992903),
        ("ем", 1510, 1520, 0.999954),
        ("е", 1610, 1610, 0.877693),
        (",", 1630, 1680, 0.99771),
        (" что", 1680, 1790, 0.878882),
        (" вы", 1790, 1820, 0.979914),
        (" такие", 1910, 2040, 0.801615),
        (" про", 2040, 2090, 0.995232),
        ("щ", 2180, 2180, 0.999992),
        ("ад", 2200, 2250, 0.99701),
        ("ные", 2250, 2360, 0.998727),
        (" ч", 2360, 2390, 1.0),
        ("аты", 2390, 2500, 0.997295),
        (".", 2500, 2690, 0.999916),
        ("[_TT_135]", 2700, 2700, 1.0),
    ];
    /// `me-1795069444`.
    const ME_1795069444: &[(&str, u64, u64, f32)] = &[];
}
