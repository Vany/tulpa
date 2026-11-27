//! The real whisper output fixtures (`tests/fixtures/whisper/*.json`, written by the `transcribe_wav` example from
//! real whisper.cpp runs and never hand-edited) through the crate's text paths: the raw reading
//! ([`Transcript::text`]) and the line assembler's [`piece`]. No `whisper` feature needed: only the JSON is read.
//!
//! `*.vad` is the live path (Silero spans joined as whisper.cpp joins them, one pass, token times mapped back);
//! `*.novad` is one pass over the whole audio. The samples (not in the repo): `jfk` and `ru` as they are;
//! `jfk_gap` = 3 s + jfk[..2.70 s] + 4 s + jfk[2.70 s..], the inserted stretches being jfk's own room tone (its
//! quietest 0.4 s, looped, natural level); `ru_gap` = the same cut of `ru` at 2.65 s with jfk's room tone under the
//! whole file; `jfk_gap_zeros` = `jfk_gap` with digital zeros instead of room tone (a known edge, below). True times
//! are from the waveforms: the first (last) 10 ms frame above -30 dBFS around the inserted stretch.

use std::path::Path;

use tulpa_hear::assemble::piece;
use tulpa_hear::recognize::{Token, Transcript};

/// Every fixture the example wrote.
const ALL: [&str; 10] = [
    "jfk.vad",
    "jfk.novad",
    "jfk_gap.vad",
    "jfk_gap.novad",
    "jfk_gap_zeros.vad",
    "jfk_gap_zeros.novad",
    "ru.vad",
    "ru.novad",
    "ru_gap.vad",
    "ru_gap.novad",
];

/// How far a word or a pause may be from its true time (s).
const TOLERANCE_S: f64 = 0.2;

/// The text after `"key":` (and any white space) up to the next `,`, `}` or line end (a number or bool), or between
/// the quotes (`hex`, which is never escaped; whisper-cli's `text`, checked unescaped by [`load_cli`]).
fn field<'a>(obj: &'a str, key: &str) -> Result<&'a str, String> {
    let pat = format!("\"{key}\":");
    let (_, rest) = obj
        .split_once(&pat)
        .ok_or_else(|| format!("no {key} in {obj}"))?;
    let rest = rest.trim_start();
    let value = rest.strip_prefix('"').map_or_else(
        || rest.split([',', '}', '\n']).next(),
        |q| q.split_once('"').map(|(v, _)| v),
    );
    value.ok_or_else(|| format!("no end of {key}"))
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let (pairs, odd) = s.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return Err(format!("odd hex {s}"));
    }
    pairs
        .iter()
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("bad hex in {s}"))
        })
        .collect()
}

fn num<T: std::str::FromStr>(obj: &str, key: &str) -> Result<T, String> {
    let v = field(obj, key)?;
    v.parse().map_err(|_| format!("bad {key} {v}"))
}

/// A fixture as the recognizer gave it. The format is the example's own: one token object per line, each starting
/// with `{"hex":`.
fn load(name: &str) -> Result<Transcript, String> {
    let json = read(name)?;
    let no_speech = match field(&json, "no_speech")? {
        "true" => true,
        "false" => false,
        other => return Err(format!("{name}: no_speech {other}")),
    };
    let tokens = json
        .split("{\"hex\":")
        .skip(1)
        .map(|obj| {
            let obj = format!("{{\"hex\":{obj}");
            Ok(Token::new(
                unhex(field(&obj, "hex")?)?,
                num(&obj, "start_ms")?,
                num(&obj, "end_ms")?,
                num(&obj, "p")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Transcript::new(tokens, no_speech))
}

/// A fixture's JSON text.
fn read(name: &str) -> Result<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/whisper")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The assembler's text without its `<pause N.Ns>` tags.
fn unpaused(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("<pause ") {
        out.push_str(before);
        rest = after.split_once('>').map_or("", |(_, r)| r);
    }
    out.push_str(rest);
    out
}

/// The assembled line text of a fixture, and its raw reading.
fn texts(name: &str) -> Result<(String, String), String> {
    let t = load(name)?;
    if t.no_speech || t.is_empty() {
        return Err(format!("{name}: no speech"));
    }
    let ms = t.tokens.last().map_or(0, |k| k.end_ms);
    Ok((piece(&t, 0, ms).text, t.text()))
}

#[test]
fn jfk_reads_through_the_assembler() -> Result<(), String> {
    let (line, raw) = texts("jfk.vad")?;
    let want = "ask not what your country can do for you";
    assert!(raw.contains(want), "{raw}");
    assert!(unpaused(&line).contains(want), "{line}");
    assert!(
        !line.starts_with("<voice") && !line.starts_with("<no speech"),
        "{line}"
    );
    Ok(())
}

#[test]
fn ru_decodes_whole_cyrillic() -> Result<(), String> {
    let (line, raw) = texts("ru.vad")?;
    for text in [&line, &raw] {
        assert!(!text.contains('\u{FFFD}'), "{text}");
        assert!(text.contains("проверка"), "{text}");
        assert!(
            text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
            "{text}"
        );
    }
    // The joined bytes are valid UTF-8, though a single token's need not be.
    let joined: Vec<u8> = load("ru.vad")?
        .tokens
        .iter()
        .flat_map(|t| t.bytes.iter().copied())
        .collect();
    assert!(String::from_utf8(joined).is_ok());
    Ok(())
}

#[test]
fn token_times_never_go_backwards() -> Result<(), String> {
    for name in ALL {
        let t = load(name)?;
        assert!(!t.tokens.is_empty(), "{name}");
        for k in &t.tokens {
            assert!(k.start_ms <= k.end_ms, "{name}: {k:?}");
        }
        for (a, b) in t.tokens.iter().zip(t.tokens.iter().skip(1)) {
            assert!(a.start_ms <= b.start_ms, "{name}: {a:?} then {b:?}");
            assert!(a.end_ms <= b.end_ms, "{name}: {a:?} then {b:?}");
        }
    }
    Ok(())
}

/// A fixture's words (a word starts at a token with a leading space), each with its start in seconds.
fn words(name: &str) -> Result<Vec<(String, f64)>, String> {
    let mut out: Vec<(Vec<u8>, u64)> = Vec::new();
    for tok in load(name)?.tokens {
        match out.last_mut() {
            Some((w, _)) if !tok.bytes.starts_with(b" ") => w.extend_from_slice(&tok.bytes),
            _ => out.push((tok.bytes.clone(), tok.start_ms)),
        }
    }
    Ok(out
        .into_iter()
        .map(|(w, ms)| {
            #[expect(clippy::cast_precision_loss, reason = "ms of a short test clip")]
            let s = ms as f64 / 1000.0;
            (String::from_utf8_lossy(&w).trim().to_owned(), s)
        })
        .collect())
}

/// Lower case, punctuation dropped: the words a reader would compare.
fn bare(text: &str) -> String {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The start (s) of the first word reading `word` (punctuation ignored).
fn start_of(name: &str, word: &str) -> Result<f64, String> {
    words(name)?
        .into_iter()
        .find(|(w, _)| bare(w) == bare(word))
        .map(|(_, s)| s)
        .ok_or_else(|| format!("{name}: no {word:?}"))
}

/// The assembled line's longest `<pause N.Ns>` (s) and the word right after it.
fn longest_pause(name: &str) -> Result<(f64, String), String> {
    let (line, _) = texts(name)?;
    let mut best: Option<(f64, String)> = None;
    for (_, rest) in line.match_indices("<pause ").map(|(i, _)| line.split_at(i)) {
        let body = rest.trim_start_matches("<pause ");
        let (n, after) = body
            .split_once("s>")
            .ok_or_else(|| format!("{name}: bad pause in {line}"))?;
        let secs: f64 = n.parse().map_err(|_| format!("{name}: bad pause {n}"))?;
        let next = after
            .split_whitespace()
            .map(bare)
            .find(|w| !w.is_empty())
            .unwrap_or_default();
        if best.as_ref().is_none_or(|(b, _)| secs > *b) {
            best = Some((secs, next));
        }
    }
    best.ok_or_else(|| format!("{name}: no pause in {line}"))
}

fn near(what: &str, got: f64, want: f64) {
    assert!(
        (got - want).abs() <= TOLERANCE_S,
        "{what}: {got:.2} s, true {want:.2} s ± {TOLERANCE_S}"
    );
}

/// The old live path (`whisper-cli --vad`, same model, VAD model and defaults, no prompt) on the same samples.
const LIVE: [(&str, &str); 4] = [
    (
        "jfk",
        "And so, my fellow Americans, ask not what your country can do for you, ask what you can do for your country.",
    ),
    (
        "jfk_gap",
        "And so, my fellow Americans, not what your country can do for you, ask what you can do for your country.",
    ),
    (
        "ru",
        "Привет, это проверка распознавания речи. Сегодня хорошая погода.",
    ),
    (
        "ru_gap",
        "Привет, это проверка распознавания речи. Сегодня хорошая погода.",
    ),
];

#[test]
fn gate_text_matches_the_live_path_word_for_word() -> Result<(), String> {
    for (name, live) in LIVE {
        let (line, raw) = texts(&format!("{name}.vad"))?;
        assert_eq!(bare(&raw), bare(live), "{name}");
        assert_eq!(bare(&unpaused(&line)), bare(live), "{name}");
        assert!(!raw.contains("Ask. not."), "{name}: {raw}");
        assert!(!raw.contains('\u{FFFD}'), "{name}: {raw}");
    }
    Ok(())
}

#[test]
fn gate_jfk_says_ask_not() -> Result<(), String> {
    let (line, raw) = texts("jfk.vad")?;
    let want = "ask not what your country can do for you";
    assert!(bare(&raw).contains(want), "{raw}");
    assert!(bare(&unpaused(&line)).contains(want), "{line}");
    Ok(())
}

#[test]
fn gate_ru_gap_words_sit_on_the_audio() -> Result<(), String> {
    // The waveform: the first 10 ms frame above -30 dBFS after the leading room tone, and after the inserted 4 s.
    near("ru_gap Привет", start_of("ru_gap.vad", "Привет")?, 3.01);
    // whisper gives this token t0 = the end of "речи." (before the cut) and t1 after it: the straddle rule
    // (`span::map_span_ms`) starts it at the later piece.
    near("ru_gap Сегодня", start_of("ru_gap.vad", "Сегодня")?, 9.83);
    Ok(())
}

#[test]
fn gate_ru_gap_inserted_pause_is_true() -> Result<(), String> {
    // True gap from the waveform: "речи." ends 5.51 s, "Сегодня" starts 9.83 s (frames above -30 dBFS).
    let (secs, next) = longest_pause("ru_gap.vad")?;
    assert_eq!(next, "сегодня", "the longest pause comes before {next:?}");
    near("ru_gap gap pause", secs, 4.32);
    Ok(())
}

/// DECISION PENDING, asserted as it is today: on `jfk_gap` (jfk's own room tone in the inserted stretches) Silero
/// with its untuned default parameters finds no speech in "ask" (true onset 10.29 s), so it is never transcribed.
/// That is the same miss as the live path, `whisper-cli --vad`, on the same sample (same Silero model and defaults).
/// The owner's decision on it is pending (#0454); when it lands, this test changes. The inserted-gap pause follows
/// it: the longest pause (true gap 5.11 → 10.29 s = 5.18 s to "ask") comes before "not" instead.
#[test]
fn jfk_gap_room_tone_ask_dropped_by_silero_same_as_live_path_decision_pending() -> Result<(), String>
{
    let (_, raw) = texts("jfk_gap.vad")?;
    let read = bare(&raw);
    assert!(read.contains("americans not what your country"), "{raw}");
    assert!(!read.contains("ask not"), "{raw}");
    let (secs, next) = longest_pause("jfk_gap.vad")?;
    assert_eq!(next, "not");
    assert!((secs - 5.9).abs() < 0.05, "{secs}");
    Ok(())
}

#[test]
fn gate_vad_differs_from_novad() -> Result<(), String> {
    for name in ["jfk", "jfk_gap", "jfk_gap_zeros", "ru", "ru_gap"] {
        assert_ne!(
            read(&format!("{name}.vad"))?,
            read(&format!("{name}.novad"))?,
            "{name}"
        );
    }
    Ok(())
}

/// KNOWN EDGE, asserted as it is: after 4 s of digital zeros Silero (default parameters) finds no speech in "ask
/// not", so the live path drops both words; the whole-audio pass keeps them. A real mic never sends digital zeros
/// (see `jfk_gap` for the same cut on room tone).
#[test]
fn known_edge_jfk_gap_zeros_drops_ask_not() -> Result<(), String> {
    let (_, raw) = texts("jfk_gap_zeros.vad")?;
    let read = bare(&raw);
    assert!(read.contains("americans what your country"), "{raw}");
    assert!(!read.contains("ask not"), "{raw}");
    let (_, whole) = texts("jfk_gap_zeros.novad")?;
    assert!(
        bare(&whole).contains("ask not what your country"),
        "{whole}"
    );
    Ok(())
}

/// whisper's end-of-text token id in the multilingual vocabulary: every id from it up is a special or timestamp token
/// (`[_BEG_]`, `[_TT_…]`), which the live recognizer counts as specials and never hands over as text
/// (`whisper.rs`, `data.id >= eot`).
const EOT_MULTILINGUAL: u32 = 50_257;

/// A whisper-cli full JSON fixture (`tests/fixtures/whisper-cli/`) in the recognizer's shape, and its segments' text.
///
/// No JSON crate in the dev-dependencies, so a reader for this one file shape: in `transcription`, every object holding
/// a `"text":` key is a token when an `"id":` follows before the next `"text":`, else a segment. Token times are whisper-cli's own
/// token offsets (ms), not synthesized. A text with a backslash (an escape this reader does not decode) fails.
fn load_cli(name: &str) -> Result<(Transcript, Vec<String>), String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/whisper-cli")
        .join(format!("{name}.json"));
    let json = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (_, transcription) = json
        .split_once("\"transcription\":")
        .ok_or_else(|| format!("{name}: no transcription"))?;
    let (mut tokens, mut segments, mut specials) = (Vec::new(), Vec::new(), 0_usize);
    for chunk in transcription.split("\"text\":").skip(1) {
        let chunk = format!("\"text\":{chunk}");
        let text = field(&chunk, "text")?;
        if text.contains('\\') {
            return Err(format!("{name}: escaped text {text}"));
        }
        if !chunk.contains("\"id\":") {
            segments.push(text.to_owned());
            continue;
        }
        let id: u32 = num(&chunk, "id")?;
        if (id >= EOT_MULTILINGUAL) != text.starts_with("[_") {
            return Err(format!("{name}: token {id} {text:?}"));
        }
        if id >= EOT_MULTILINGUAL {
            specials = specials.saturating_add(1);
            continue;
        }
        let (_, offsets) = chunk
            .split_once("\"offsets\":")
            .ok_or_else(|| format!("{name}: no offsets for {text:?}"))?;
        tokens.push(Token::new(
            text,
            num(offsets, "from")?,
            num(offsets, "to")?,
            num(&chunk, "p")?,
        ));
    }
    Ok((
        Transcript::new(tokens, false).with_specials(specials),
        segments,
    ))
}

/// E2 P1 on a real recognition: the 59.8 s piece lost live on 2026-11-24 (15 segments of English, then whisper's
/// invented credit «Субтитры создавал `DimaTorzok`») publishes all its words and drops only the credit.
#[test]
#[expect(
    clippy::print_stdout,
    reason = "expected vs found counts in the test output (--nocapture)"
)]
fn real_recognition_ending_in_a_signed_credit_publishes_every_real_word() -> Result<(), String> {
    let (t, segments) = load_cli("w1-0-59.8.ru")?;
    let (credit, real) = segments
        .split_last()
        .ok_or_else(|| "no segments".to_owned())?;
    assert_eq!(credit.trim(), "Субтитры создавал DimaTorzok");
    let p = piece(&t, 0, 59_800);
    let want = bare(&real.concat());
    let found = bare(&unpaused(&p.text));
    let found_real = real.iter().filter(|s| found.contains(&bare(s))).count();
    println!(
        "expected {} words in {} segments, found {} words, {} of the segments whole; leak lines {}",
        want.split(' ').count(),
        real.len(),
        found.split(' ').filter(|w| !w.is_empty()).count(),
        found_real,
        p.leaks
    );
    assert!(
        p.unread.is_none(),
        "unread as {}: {}",
        p.unread.map_or("-", |u| u.why.as_str()),
        p.text
    );
    assert_eq!(found, want, "{}", p.text);
    for word in credit.split_whitespace() {
        assert!(!p.text.contains(word), "{word} in {}", p.text);
        assert!(!p.payload.contains(word), "{word} in {}", p.payload);
    }
    assert_eq!(p.leaks, 1);
    Ok(())
}
