//! Speech to text for one closed utterance, in memory.
//!
//! [`Recognizer`] is the seam: the real one is `WhisperRecognizer` (whisper.cpp through `whisper-rs`, only with the
//! `whisper` feature, in [`whisper`](crate::whisper)); tests drive the pipeline with fakes. [`recognize`] adds the
//! live retry: an utterance of at least [`RETRY_MIN_MS`] whose transcript comes back with no tokens is transcribed
//! exactly once more, and one whose transcript is credit leaks only is transcribed once more with language detection.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use crate::assemble::{Why, piece};

/// An empty transcript of an utterance at least this long (ms) is transcribed once more.
pub const RETRY_MIN_MS: u64 = 2500;

/// One recognized token, as whisper gives it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Token {
    /// Its raw bytes, with whisper's own leading space when it starts a word. Not always UTF-8 on its own: whisper's
    /// byte-level BPE can split one letter's bytes across two tokens, so text is decoded only once pieces are joined.
    pub bytes: Vec<u8>,
    /// Where it starts, ms from the utterance's first sample.
    pub start_ms: u64,
    /// Where it ends, ms from the utterance's first sample.
    pub end_ms: u64,
    /// The decoder's probability for it, 0..=1.
    pub p: f32,
    /// Which of whisper's segments it came in, counted from 0 in the transcript's order: the line assembler drops a
    /// segment's text that whisper looped ([`crate::assemble::SEGMENT_LOOP`]).
    pub segment: usize,
}

impl Token {
    /// A token of these raw bytes (a `&str` or `String` gives its UTF-8).
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>, start_ms: u64, end_ms: u64, p: f32) -> Self {
        Self {
            bytes: bytes.into(),
            start_ms,
            end_ms,
            p,
            segment: 0,
        }
    }

    /// This token as one of whisper's segment `n` (0, the first, unless set).
    #[must_use]
    pub const fn in_segment(mut self, n: usize) -> Self {
        self.segment = n;
        self
    }

    /// Its bytes decoded on their own (a split letter's half is U+FFFD): for looking at, not for joining.
    #[must_use]
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// What the recognizer made of one utterance.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Transcript {
    /// The text tokens, in order (no special or timestamp tokens).
    pub tokens: Vec<Token>,
    /// The recognizer found no speech segment at all (whisper's VAD dropped everything), as opposed to segments
    /// that held no text tokens. Either way [`Transcript::is_empty`] holds.
    pub no_speech: bool,
    /// How many of whisper's own special and timestamp tokens (`[_BEG_]`, `[_TT_39]`, `[_EOT_]`) its segments held
    /// beside [`Transcript::tokens`]: `whisper-cli`'s JSON lists them among the tokens, and the line assembler's
    /// reason for an unread piece counts them (`markers-only`).
    pub specials: usize,
}

impl Transcript {
    /// A transcript of these tokens.
    #[must_use]
    pub const fn new(tokens: Vec<Token>, no_speech: bool) -> Self {
        Self {
            tokens,
            no_speech,
            specials: 0,
        }
    }

    /// This transcript with `n` special tokens beside its text tokens.
    #[must_use]
    pub const fn with_specials(mut self, n: usize) -> Self {
        self.specials = n;
        self
    }

    /// No tokens: heard, not read.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Nothing at all, not even whisper's own markers: the only pass worth a retry (the script retries only a JSON
    /// with no tokens). A markers-only pass is the deaf signature and stands as it is.
    #[must_use]
    pub const fn is_blank(&self) -> bool {
        self.tokens.is_empty() && self.specials == 0
    }

    /// The transcript as JSON, one token a line: `{"no_speech":<bool>,"specials":<n>,"tokens":[` then per token
    /// `{"hex":"<its bytes>","text":"<decoded on its own>","start_ms":..,"end_ms":..,"p":..}` and `]}`. The
    /// recognizer's output kept as is (the raw ring's `.whisper.json`, the `tests/fixtures/whisper` files): `hex` is
    /// exact where `text` shows a letter whisper split across two tokens as U+FFFD in each.
    #[must_use]
    pub fn json(&self) -> String {
        let mut out = format!(
            "{{\"no_speech\":{},\"specials\":{},\"tokens\":[",
            self.no_speech, self.specials
        );
        for (n, tok) in self.tokens.iter().enumerate() {
            let hex = tok.bytes.iter().fold(String::new(), |mut h, b| {
                let _w: fmt::Result = write!(h, "{b:02x}");
                h
            });
            let _w: fmt::Result = write!(
                out,
                "{}\n{{\"hex\":\"{hex}\",\"text\":{},\"start_ms\":{},\"end_ms\":{},\"p\":{}}}",
                if n == 0 { "" } else { "," },
                json_str(&tok.text()),
                tok.start_ms,
                tok.end_ms,
                tok.p
            );
        }
        out.push_str("\n]}\n");
        out
    }

    /// The tokens' bytes joined as given, then decoded and trimmed: the raw reading. The line the tray gets is
    /// [`assemble::piece`](crate::assemble::piece)'s.
    #[must_use]
    pub fn text(&self) -> String {
        let joined: Vec<u8> = self
            .tokens
            .iter()
            .flat_map(|t| t.bytes.iter().copied())
            .collect();
        String::from_utf8_lossy(&joined).trim().to_owned()
    }
}

/// `s` as a JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _w: fmt::Result = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Why a recognizer could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecognizeError {
    /// A model file named at start is not there.
    MissingModel(String),
    /// The model could not be loaded, or a path cannot be handed to it.
    Load(String),
    /// One transcription failed.
    Run(String),
}

impl fmt::Display for RecognizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModel(what) => write!(f, "missing {what}"),
            Self::Load(e) => write!(f, "cannot load the model: {e}"),
            Self::Run(e) => write!(f, "transcription failed: {e}"),
        }
    }
}

impl std::error::Error for RecognizeError {}

/// Speech to text.
pub trait Recognizer {
    /// Transcribes 16 kHz mono samples (-1..=1) in language `lang` (`"ru"`, `"en"`, …).
    ///
    /// # Errors
    ///
    /// [`RecognizeError::Run`] when the recognizer fails on this audio.
    fn transcribe(&mut self, samples_16k: &[f32], lang: &str)
    -> Result<Transcript, RecognizeError>;
}

/// What the one retry gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Retry {
    /// The second pass read something.
    Read,
    /// Still empty.
    Empty,
    /// The second pass failed; the blank first result stands.
    Failed,
    /// The pass with language detection ([`DETECT`]) read words: its result is used.
    Detected,
    /// The pass with language detection read no words either: the first result stands.
    Undetected,
    /// The pass with language detection failed: the first result stands.
    DetectFailed,
}

impl Retry {
    /// The outcome, for the stderr line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Empty => "empty",
            Self::Failed => "failed",
            Self::Detected => "language detection recovered it",
            Self::Undetected => "language detection did not recover it",
            Self::DetectFailed => "language detection failed",
        }
    }

    /// Whether it is the retry with language detection (a credit leak's), not the blank pass's.
    #[must_use]
    pub const fn detects(self) -> bool {
        matches!(self, Self::Detected | Self::Undetected | Self::DetectFailed)
    }
}

/// whisper's language detection, as the language of a pass.
pub const DETECT: &str = "auto";

/// `i16` samples to whisper's `f32`.
#[must_use]
pub fn to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| f32::from(s) / 32768.0).collect()
}

/// Transcribes `samples` (an utterance of `ms`), with at most one retry.
///
/// A blank result ([`Transcript::is_blank`]: no tokens, no markers) of an utterance of at least [`RETRY_MIN_MS`] is
/// transcribed exactly once more, in `lang`. A first result with nothing left once its credit-leak lines are dropped
/// ([`Why::CreditLeak`](crate::assemble::Why::CreditLeak)) is transcribed exactly once more with language detection
/// ([`DETECT`]): whisper held to the wrong language can give credits for every word (2026-11-24, E2: English speech
/// under `ru` came back as `Субтитры создавал DimaTorzok` only, and as the words under `auto`). That pass is used
/// when it reads words; otherwise the first stands. A first result that kept words is never re-heard: detection
/// can take a short Russian phrase for another language and give it back translated or in Latin letters, and the
/// words `lang` heard are not traded for that. Returns the transcript and, when a retry ran, its outcome.
///
/// # Errors
///
/// The first pass's error. (A failed retry keeps the first result: [`Retry::Failed`], [`Retry::DetectFailed`].)
pub fn recognize<R: Recognizer + ?Sized>(
    rec: &mut R,
    samples: &[f32],
    ms: u64,
    lang: &str,
) -> Result<(Transcript, Option<Retry>), RecognizeError> {
    let first = rec.transcribe(samples, lang)?;
    if first.is_blank() && ms >= RETRY_MIN_MS {
        return Ok(match rec.transcribe(samples, lang) {
            Ok(second) if !second.is_empty() => (second, Some(Retry::Read)),
            Ok(second) => (second, Some(Retry::Empty)),
            Err(_) => (first, Some(Retry::Failed)),
        });
    }
    if piece(&first, 0, ms).unread.map(|u| u.why) != Some(Why::CreditLeak) {
        return Ok((first, None));
    }
    Ok(match rec.transcribe(samples, DETECT) {
        Ok(second) if piece(&second, 0, ms).unread.is_none() => (second, Some(Retry::Detected)),
        Ok(_) => (first, Some(Retry::Undetected)),
        Err(_) => (first, Some(Retry::DetectFailed)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers from a script, one entry per call, and counts the calls.
    struct Scripted(Vec<Transcript>, usize);

    impl Recognizer for Scripted {
        fn transcribe(&mut self, _: &[f32], _: &str) -> Result<Transcript, RecognizeError> {
            self.1 = self.1.saturating_add(1);
            Ok(if self.0.is_empty() {
                Transcript::default()
            } else {
                self.0.remove(0)
            })
        }
    }

    #[test]
    fn a_markers_only_pass_is_not_retried_and_stands() {
        let markers = Transcript::new(Vec::new(), false).with_specials(3);
        let leak = Transcript::new(vec![Token::new(" Субтитры", 0, 900, 0.9)], false);
        let mut rec = Scripted(vec![markers.clone(), leak], 0);
        let got = recognize(&mut rec, &[0.0; 16], RETRY_MIN_MS, "ru");
        assert_eq!(got, Ok((markers, None)));
        assert_eq!(rec.1, 1, "one pass");
    }

    #[test]
    fn a_blank_pass_is_retried_once() {
        let read = Transcript::new(vec![Token::new(" да", 0, 300, 0.9)], false);
        let mut rec = Scripted(vec![Transcript::default(), read.clone()], 0);
        let got = recognize(&mut rec, &[0.0; 16], RETRY_MIN_MS, "ru");
        assert_eq!(got, Ok((read, Some(Retry::Read))));
        assert_eq!(rec.1, 2);
    }

    /// Answers by language from a script (`None`: the pass fails), and keeps the language of each call.
    struct ByLang {
        script: Vec<(&'static str, Option<Transcript>)>,
        langs: Vec<String>,
    }

    impl Recognizer for ByLang {
        fn transcribe(&mut self, _: &[f32], lang: &str) -> Result<Transcript, RecognizeError> {
            self.langs.push(lang.to_owned());
            self.script
                .iter()
                .find(|(l, _)| *l == lang)
                .and_then(|(_, t)| t.clone())
                .ok_or_else(|| RecognizeError::Run(format!("no {lang}")))
        }
    }

    fn says(words: &[&str]) -> Transcript {
        let toks = (0_u64..)
            .zip(words)
            .map(|(i, w)| {
                Token::new(
                    *w,
                    i.saturating_mul(400),
                    i.saturating_mul(400).saturating_add(380),
                    0.9,
                )
            })
            .collect();
        Transcript::new(toks, false)
    }

    /// E2 P4 and r2 R3: a first pass that is credit leaks only is transcribed exactly once more, with language
    /// detection; its result is used when it reads words, and the first stands otherwise (or when it fails). A first
    /// pass that kept words after the leak filter is never re-heard, so detection never trades them away (here it
    /// would give them back in Latin letters). No leak, no retry. Each case: what `ru` and `auto` give, the text
    /// published, the retry's outcome, the passes run.
    #[test]
    fn a_leak_is_reheard_once_and_never_takes_kept_words() {
        let credits = says(&[" Субтитры", " создавал", " DimaTorzok"]);
        let words = says(&[" Hello", " world"]);
        let short_ru = says(&[" Да", ",", " конечно", "."]);
        let mixed = says(&[" Да", " нет", ".", " Субтитры", " создавал", " DimaTorzok"]);
        let latin = says(&[" Da", " net", "."]);
        let cases = [
            (
                Some(&credits),
                Some(&words),
                "Hello world",
                Some(Retry::Detected),
                2,
            ),
            (
                Some(&credits),
                Some(&short_ru),
                "Да, конечно.",
                Some(Retry::Detected),
                2,
            ),
            (
                Some(&credits),
                Some(&credits),
                "<voice 1.0s>",
                Some(Retry::Undetected),
                2,
            ),
            (
                Some(&credits),
                None,
                "<voice 1.0s>",
                Some(Retry::DetectFailed),
                2,
            ),
            (Some(&mixed), Some(&latin), "Да нет.", None, 1),
            (Some(&words), Some(&credits), "Hello world", None, 1),
        ];
        for (n, (ru, auto, text, retry, calls)) in cases.into_iter().enumerate() {
            let mut rec = ByLang {
                script: vec![("ru", ru.cloned()), (DETECT, auto.cloned())],
                langs: Vec::new(),
            };
            let got = recognize(&mut rec, &[0.0; 16], 1000, "ru").ok();
            let said = got.as_ref().map(|(t, r)| (piece(t, 0, 1000).text, *r));
            assert_eq!(said, Some((text.to_owned(), retry)), "case {n}");
            let langs: Vec<&str> = rec.langs.iter().map(String::as_str).collect();
            assert_eq!(
                Some(langs.as_slice()),
                ["ru", DETECT].get(..calls),
                "case {n}"
            );
        }
        let mut failing = ByLang {
            script: vec![("ru", None), (DETECT, Some(words))],
            langs: Vec::new(),
        };
        assert!(recognize(&mut failing, &[0.0; 16], 1000, "ru").is_err());
        assert_eq!(failing.langs, ["ru"]);
    }

    #[test]
    fn the_json_keeps_bytes_exact_and_text_escaped() {
        let t = Transcript::new(
            vec![
                Token::new(" \"да\"\t\\", 10, 250, 0.5),
                Token::new(vec![0xD0], 250, 260, 0.25),
            ],
            false,
        )
        .with_specials(3);
        assert_eq!(
            t.json(),
            "{\"no_speech\":false,\"specials\":3,\"tokens\":[\n\
             {\"hex\":\"2022d0b4d0b022095c\",\"text\":\" \\\"да\\\"\\u0009\\\\\",\"start_ms\":10,\"end_ms\":250,\"p\":0.5},\n\
             {\"hex\":\"d0\",\"text\":\"\u{FFFD}\",\"start_ms\":250,\"end_ms\":260,\"p\":0.25}\n]}\n"
        );
        assert_eq!(
            Transcript::new(Vec::new(), true).json(),
            "{\"no_speech\":true,\"specials\":0,\"tokens\":[\n]}\n"
        );
    }
}
