//! The Rust line assembler against the capture script's own, on the same input.
//!
//! The script's awk assembler (`ASSEMBLE='…'` in `packaging/macos/ears-capture.sh`, read from the file, not copied)
//! runs over each raw `whisper-cli -ojf` capture in `scripts/fixtures/0393-ojf` and prints one piece: its text and,
//! after a TAB, its word timings, or `<voice N.Ns>`. The same JSON, read into a [`Transcript`] the way the awk reads
//! it (the `text` and the offsets of each token, whisper's own markers kept as tokens), goes through [`piece`]; the
//! two must agree byte for byte, with one known difference: a piece with no token at all (whisper's VAD found no
//! speech) is `<no speech N.Ns>` in Rust and `<voice N.Ns>` in the script, which does not tell that rule apart
//! (the script is being retired, so it stays as it is). A few made-up transcripts cover what the captures do not
//! (markers only, no tokens, a decode loop, a separator in a token, a pause).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tulpa_hear::assemble::{
    LEAK_STARTS, PUNCTUATION, WHOLE_CREDITS, Why, flag_fields, normal, piece,
};
use tulpa_hear::recognize::{Token, Transcript};

/// The epoch the pieces start at, and their length (the `<voice N.Ns>`'s).
const EPOCH: u64 = 1_795_069_373_000;
const MS: u64 = 3_400;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

type Res<T> = Result<T, String>;

/// The script's awk program, as the script holds it: inside single quotes, so one with an apostrophe (in a comment,
/// say) is not the program the shell passes, and the script fails to parse (it did from db1fa146 to 046aeb45 of
/// #0454 B1, with every test here green).
fn script_awk() -> Res<String> {
    let script = std::fs::read_to_string(root().join("packaging/macos/ears-capture.sh"))
        .map_err(|e| format!("the capture script: {e}"))?;
    let (_, rest) = script.split_once("\nASSEMBLE='\n").ok_or("no ASSEMBLE='")?;
    let (awk, _) = rest.split_once("\n'\n").ok_or("no end of ASSEMBLE")?;
    if let Some(at) = awk.lines().position(|l| l.contains('\'')) {
        return Err(format!(
            "an apostrophe ends ASSEMBLE's quote at its line {}",
            at.saturating_add(1)
        ));
    }
    Ok(awk.to_owned())
}

/// The capture script parses as shell (#0454 B1 D18): every other test here cuts the awk out of the text, so a parse
/// break elsewhere (an apostrophe in an awk comment ended ASSEMBLE's quote from db1fa146 to 046aeb45) went unseen
/// without the rig.
#[test]
fn the_capture_script_parses_as_shell() -> Res<()> {
    let out = Command::new("sh")
        .arg("-n")
        .arg(root().join("packaging/macos/ears-capture.sh"))
        .output()
        .map_err(|e| format!("sh -n: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("sh -n: {}", String::from_utf8_lossy(&out.stderr)))
    }
}

/// The awk's piece for one JSON file (its first line; the second is its verdict, [`awk_out`]).
fn awk_piece(awk: &str, json: &Path) -> Res<Vec<u8>> {
    Ok(awk_out(awk, json)?.0)
}

/// The awk's two lines for one JSON file: the piece, and its verdict (`read`, or `voice-unread:<rule> …`).
fn awk_out(awk: &str, json: &Path) -> Res<(Vec<u8>, String)> {
    awk_out_at(awk, json, EPOCH)
}

/// [`awk_out`] with the utterance starting at `epoch`. A piece that dropped a credit has a third line, its text with no
/// credit dropped (#0454 B1 D7): checked here against the Rust port's [`tulpa_hear::assemble::Piece::heard`], on every
/// call, and present exactly when the Rust port dropped a line too.
fn awk_out_at(awk: &str, json: &Path, epoch: u64) -> Res<(Vec<u8>, String)> {
    let out = Command::new("awk")
        .env("LC_ALL", "C")
        .arg("-v")
        .arg(format!("epoch={epoch}"))
        .arg("-v")
        .arg(format!("ms={MS}"))
        .arg(awk)
        .arg(json)
        .output()
        .map_err(|e| format!("awk: {e}"))?;
    assert!(out.status.success(), "awk on {}", json.display());
    let mut s = out.stdout;
    assert_eq!(s.pop(), Some(b'\n'), "ends in a newline");
    let src = std::fs::read(json).map_err(|e| e.to_string())?;
    let rust = piece(&Transcript::new(ojf_tokens(&src), false), epoch, MS);
    if rust.leaks > 0 {
        let at = s
            .iter()
            .rposition(|&b| b == b'\n')
            .ok_or_else(|| format!("no heard line: {}", json.display()))?;
        let heard = s.split_off(at);
        assert_eq!(
            String::from_utf8_lossy(heard.get(1..).unwrap_or_default()),
            rust.heard,
            "the heard line, the awk vs the Rust port: {}",
            json.display()
        );
    }
    let at = s
        .iter()
        .rposition(|&b| b == b'\n')
        .ok_or_else(|| format!("no verdict line: {}", json.display()))?;
    let verdict = String::from_utf8(s.split_off(at).split_off(1)).map_err(|e| e.to_string())?;
    assert!(!s.contains(&b'\n'), "one piece line: {}", json.display());
    Ok((s, verdict))
}

/// The tokens of whisper.cpp 1.9.5/1.9.6's `-ojf` layout as the awk parses them: TAB depth 3 opens and closes a segment's
/// `"tokens"` (each opening is the next segment), depth 4 closes a token, its `"text"` at depth 5 (`\"` and `\\` unescaped), its offsets at depth 6.
fn ojf_tokens(json: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut segment = None::<usize>;
    let (mut inside, mut text, mut from, mut to) = (false, None::<Vec<u8>>, None, None);
    for line in json.split(|&b| b == b'\n') {
        let depth = line.iter().take_while(|&&b| b == b'\t').count();
        let line = line.get(depth..).unwrap_or_default();
        let num = |key: &[u8]| -> Option<u64> {
            let digits: Vec<u8> = line
                .strip_prefix(key)?
                .iter()
                .copied()
                .take_while(u8::is_ascii_digit)
                .collect();
            std::str::from_utf8(&digits).ok()?.parse().ok()
        };
        match depth {
            3 if line.starts_with(b"\"tokens\": [") => {
                inside = true;
                segment = Some(segment.map_or(0, |n: usize| n.saturating_add(1)));
                (text, from, to) = (None, None, None);
            }
            3 if inside && line.starts_with(b"]") => inside = false,
            4 if inside && (line == b"}" || line == b"},") => {
                if let (Some(t), Some(a), Some(b)) = (text.take(), from.take(), to.take())
                    && !t.is_empty()
                    && a <= b
                {
                    tokens.push(Token::new(t, a, b, 1.0).in_segment(segment.unwrap_or(0)));
                }
                (text, from, to) = (None, None, None);
            }
            5 if inside && line.starts_with(b"\"text\": \"") => {
                let raw = line.get(9..).unwrap_or_default();
                let raw = raw
                    .strip_suffix(b"\",")
                    .or_else(|| raw.strip_suffix(b"\""))
                    .unwrap_or(raw);
                let mut t = Vec::new();
                let mut i = raw.iter().copied().peekable();
                while let Some(b) = i.next() {
                    match (b, i.peek()) {
                        (b'\\', Some(&c)) if c == b'"' || c == b'\\' => {
                            t.push(c);
                            i.next();
                        }
                        _ => t.push(b),
                    }
                }
                text = Some(t);
            }
            6 if inside => {
                if let Some(n) = num(b"\"from\": ") {
                    from = Some(n);
                } else if let Some(n) = num(b"\"to\": ") {
                    to = Some(n);
                }
            }
            _ => {}
        }
    }
    tokens
}

/// The Rust piece in the awk's print form, and whether it is the no-tokens one (the one known difference).
fn rust_piece(json: &[u8]) -> (Vec<u8>, bool) {
    rust_piece_at(json, EPOCH)
}

/// [`rust_piece`] with the utterance starting at `epoch`.
fn rust_piece_at(json: &[u8], epoch: u64) -> (Vec<u8>, bool) {
    let p = piece(&Transcript::new(ojf_tokens(json), false), epoch, MS);
    let none = p.unread.is_some_and(|u| u.why == Why::NoTokens);
    let mut s = p.text.into_bytes();
    if !p.payload.is_empty() {
        s.push(b'\t');
        s.extend_from_slice(p.payload.as_bytes());
    }
    (s, none)
}

/// One made-up `-ojf` file: one segment of these tokens (text, from, to).
fn ojf(tokens: &[(&str, u64, u64)]) -> String {
    ojf_segments(&[tokens])
}

/// One made-up `-ojf` file of these segments, each of its tokens (text, from, to).
fn ojf_segments(segments: &[&[(&str, u64, u64)]]) -> String {
    let mut s = String::from("{\n\t\"transcription\": [\n");
    for (k, tokens) in segments.iter().enumerate() {
        s.push_str("\t\t{\n\t\t\t\"tokens\": [\n");
        ojf_tokens_into(&mut s, tokens);
        s.push_str(if k.saturating_add(1) < segments.len() {
            "\t\t\t]\n\t\t},\n"
        } else {
            "\t\t\t]\n\t\t}\n"
        });
    }
    s.push_str("\t]\n}\n");
    s
}

/// One segment's tokens, in the layout [`ojf_tokens`] reads.
fn ojf_tokens_into(s: &mut String, tokens: &[(&str, u64, u64)]) {
    for (i, (text, from, to)) in tokens.iter().enumerate() {
        let text = text.replace('\\', "\\\\").replace('"', "\\\"");
        let more = if i.saturating_add(1) < tokens.len() {
            ","
        } else {
            ""
        };
        let _w: std::fmt::Result = write!(
            s,
            "\t\t\t\t{{\n\t\t\t\t\t\"text\": \"{text}\",\n\t\t\t\t\t\"offsets\": {{\n\t\t\t\t\t\t\"from\": {from},\n\t\t\t\t\t\t\"to\": {to}\n\t\t\t\t\t}}\n\t\t\t\t}}{more}\n"
        );
    }
}

/// A made-up case: its name and its tokens (text, from, to).
type Case<'a> = (&'a str, &'a [(&'a str, u64, u64)]);

#[test]
fn the_rust_assembler_prints_what_the_scripts_prints() -> Res<()> {
    let awk = script_awk()?;
    let mut files = real_captures()?;
    assert_eq!(files.len(), 12, "{files:?}");
    let made = std::env::temp_dir().join(format!("tulpa-parity-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let cases: [Case<'_>; 7] = [
        (
            "speech",
            &[
                (" При", 0, 200),
                ("вет", 200, 400),
                (",", 400, 420),
                (" мир", 1100, 1500),
            ],
        ),
        ("markers", &[("[_BEG_]", 0, 0), ("[_TT_170]", 3400, 3400)]),
        ("none", &[]),
        (
            "loop",
            &[
                (" ла", 0, 1),
                (" ла", 1, 2),
                (" ла", 2, 3),
                (" ла", 3, 4),
                (" ла", 4, 5),
                (" ла", 5, 6),
            ],
        ),
        (
            "five",
            &[
                (" ла", 0, 1),
                (" ла", 1, 2),
                (" ла", 2, 3),
                (" ла", 3, 4),
                (" ла", 4, 5),
            ],
        ),
        ("separator", &[(" a;b", 0, 100), (" \"c\\d\"", 200, 300)]),
        ("blank", &[(" ", 0, 100), ("[_EOT_]", 100, 100)]),
    ];
    for (name, toks) in cases {
        let p = made.join(format!("{name}.json"));
        std::fs::write(&p, ojf(toks)).map_err(|e| e.to_string())?;
        files.push(p);
    }
    let mut nones = 0_usize;
    for f in &files {
        let json = std::fs::read(f).map_err(|e| e.to_string())?;
        let (script, (rust, none)) = (awk_piece(&awk, f)?, rust_piece(&json));
        if none {
            // The one known difference (see the top): both sides pinned, so neither drifts unseen.
            nones = nones.saturating_add(1);
            assert_eq!(rust, b"<no speech 3.4s>", "{}", f.display());
            assert_eq!(script, b"<voice 3.4s>", "{}", f.display());
            continue;
        }
        assert_eq!(
            String::from_utf8_lossy(&rust),
            String::from_utf8_lossy(&script),
            "{}",
            f.display()
        );
        assert_eq!(rust, script, "{} (bytes)", f.display());
    }
    assert_eq!(
        nones, 2,
        "no token: the capture me-1795069444 and the made-up `none`"
    );
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}

/// A real whisper.cpp 1.9.6 `-ojf` run (its public jfk sample): the awk keeps every token that is not one of
/// whisper's markers — its text, in order, and a timing for every word. The token count comes from the JSON's
/// per-token `"t_dtw"` key, not from TAB depths, so a re-indented layout the awk skips goes red here.
#[test]
fn the_awk_keeps_every_word_of_a_real_whisper_1_9_6_capture() -> Res<()> {
    let f = root().join("scripts/fixtures/0393-ojf/jfk.whisper-1.9.6.json");
    let json = std::fs::read(&f).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&json);
    let tokens = text.lines().filter(|l| l.contains("\"t_dtw\":")).count();
    let markers = text
        .lines()
        .filter(|l| l.trim_start().starts_with("\"text\": \"[_"))
        .count();
    assert_eq!(
        (tokens, markers),
        (28, 2),
        "the capture's tokens and markers"
    );
    let kept: Vec<String> = ojf_tokens(&json)
        .iter()
        .map(|t| t.text().into_owned())
        .filter(|t| !t.starts_with("[_"))
        .collect();
    assert_eq!(kept.len(), tokens.saturating_sub(markers), "{kept:?}");
    let out = awk_piece(&script_awk()?, &f)?;
    let out = String::from_utf8(out).map_err(|e| e.to_string())?;
    let (said, payload) = out.split_once('\t').ok_or("no timing payload")?;
    assert_eq!(said, kept.concat().trim(), "the text, every token in order");
    let words: Vec<&str> = payload
        .split(';')
        .filter_map(|r| r.splitn(3, ',').nth(2))
        .collect();
    assert_eq!(
        words.len(),
        payload.split(';').count(),
        "every record has a word: {payload}"
    );
    let squeeze = |s: &str| s.split_whitespace().collect::<String>();
    assert_eq!(
        words.concat(),
        squeeze(&kept.concat()),
        "a timing for every token's text"
    );
    Ok(())
}

/// Every real whisper-cli capture: the twelve `-ojf` runs of `scripts/fixtures/0393-ojf` and the 59.8 s piece
/// lost live to one credit line (tulpa-hear's own fixture).
fn real_captures() -> Res<Vec<PathBuf>> {
    let mut real: Vec<PathBuf> = std::fs::read_dir(root().join("scripts/fixtures/0393-ojf"))
        .map_err(|e| format!("the -ojf captures: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    real.sort();
    real.push(root().join("crates/tulpa-hear/tests/fixtures/whisper-cli/w1-0-59.8.ru.json"));
    Ok(real)
}

/// The awk's piece and the Rust one for one JSON file, each named, so a test holds each to the line due by itself.
fn both(awk: &str, f: &Path) -> Res<[(&'static str, String); 2]> {
    let json = std::fs::read(f).map_err(|e| e.to_string())?;
    let (script, (rust, _)) = (awk_piece(awk, f)?, rust_piece(&json));
    let text = |b: Vec<u8>| String::from_utf8(b).map_err(|e| e.to_string());
    Ok([("awk", text(script)?), ("rust", text(rust)?)])
}

/// whisper looping whole segments (the 2026-11-25 live structure, words invented): segments 1-11 distinct, then A ×13
/// interleaved with B ×3 and four distinct ones. Each looped text publishes once, at its first occurrence, with that
/// occurrence's timings; every distinct segment stays, in order; the pauses come from the kept tokens' gaps.
#[test]
fn a_looped_capture_keeps_each_segment_text_once() -> Res<()> {
    let f = root().join("scripts/fixtures/0393-ojf/loop.whisper-1.9.6.json");
    for (side, out) in both(&script_awk()?, &f)? {
        loop_line(side, &out)?;
    }
    Ok(())
}

/// [`a_looped_capture_keeps_each_segment_text_once`]'s check of one side's line.
fn loop_line(side: &str, out: &str) -> Res<()> {
    let (said, payload) = out.split_once('\t').ok_or("no timing payload")?;
    assert_eq!(
        said,
        "Red boat sails. Old clock ticks. Tall pine sways. Cold wind blows. Small bird sings. Gray stone rests. \
         Warm bread bakes. Soft rain falls. Bright star glows. Quiet road bends. Wide river flows. \
         Blue lamp hums softly.<pause 3.1s> Brown fox naps. Thin cloud drifts.<pause 6.1s> Round moon rises. \
         Green door opens.<pause 13.1s> Late train waits.",
        "{side}"
    );
    let records: Vec<&str> = payload.split(';').collect();
    assert_eq!(records.len(), 15 * 3 + 4 + 3, "{side}: {payload}");
    let epoch = |ms: u64| EPOCH.saturating_add(ms);
    for (word, t0, t1) in [
        ("Blue", 25_600, 26_055),
        ("softly.", 27_025, 27_500),
        ("Green", 42_600, 43_213),
        ("opens.", 43_866, 44_499),
    ] {
        let rec = format!("{},{},{word}", epoch(t0), epoch(t1));
        assert_eq!(
            records
                .iter()
                .filter(|r| r.ends_with(&format!(",{word}")))
                .count(),
            1,
            "{side}: {word} once: {payload}"
        );
        assert!(
            records.contains(&rec.as_str()),
            "{side}: {rec} in {payload}"
        );
    }
    Ok(())
}

/// One made-up segment: its words from `t` on, 100 ms each, then a marker; owned, see [`segment_refs`].
fn seg(t: u64, words: &[&str]) -> Seg {
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    let mut at = t;
    for w in words {
        out.push(((*w).to_owned(), at, at.saturating_add(100)));
        at = at.saturating_add(100);
    }
    out.push(("[_TT_10]".to_owned(), at, at));
    out
}

/// Made-up segments borrowed in the shape [`ojf_segments`] takes.
fn segment_refs(segments: &[Seg]) -> Vec<Vec<(&str, u64, u64)>> {
    segments
        .iter()
        .map(|s| s.iter().map(|(w, a, b)| (w.as_str(), *a, *b)).collect())
        .collect()
}

/// One made-up segment, owned: its tokens (text, from, to).
type Seg = Vec<(String, u64, u64)>;

/// The line due for this text and these words (start ms, word), each word 100 ms long as [`seg`] makes it.
fn line(text: &str, recs: &[(u64, &str)]) -> String {
    let recs: Vec<String> = recs
        .iter()
        .map(|(t0, word)| {
            let t0 = EPOCH.saturating_add(*t0);
            format!("{t0},{},{word}", t0.saturating_add(100))
        })
        .collect();
    format!("{text}\t{}", recs.join(";"))
}

/// The words of the made-up segments.
const GO: [&str; 2] = [" Go", " home."];

/// The made-up count patterns of [`a_segment_text_three_times_keeps_the_first_and_twice_keeps_both`]: name, segments,
/// the line due.
fn count_cases() -> Vec<(&'static str, Vec<Seg>, String)> {
    let go = GO;
    vec![
        (
            "twice",
            vec![seg(0, &go), seg(300, &go)],
            line(
                "Go home. Go home.",
                &[(0, "Go"), (100, "home."), (300, "Go"), (400, "home.")],
            ),
        ),
        (
            "thrice",
            vec![
                seg(0, &go),
                seg(300, &go),
                seg(600, &go),
                seg(2000, &[" Done"]),
            ],
            line(
                "Go home.<pause 1.8s> Done",
                &[(0, "Go"), (100, "home."), (2000, "Done")],
            ),
        ),
        (
            "interleaved",
            vec![
                seg(0, &go),
                seg(200, &[" Wait"]),
                seg(300, &go),
                seg(500, &[" Wait"]),
                seg(600, &go),
                seg(800, &[" End"]),
            ],
            line(
                "Go home. Wait<pause 0.5s> End",
                &[(0, "Go"), (100, "home."), (200, "Wait"), (800, "End")],
            ),
        ),
    ]
}

/// The made-up exact-text patterns of [`a_segment_text_three_times_keeps_the_first_and_twice_keeps_both`]: name,
/// segments, the line due.
fn exact_cases() -> Vec<(&'static str, Vec<Seg>, String)> {
    let go = GO;
    vec![
        (
            "punctuation",
            vec![seg(0, &go), seg(300, &go), seg(600, &[" Go", " home!"])],
            line(
                "Go home. Go home. Go home!",
                &[
                    (0, "Go"),
                    (100, "home."),
                    (300, "Go"),
                    (400, "home."),
                    (600, "Go"),
                    (700, "home!"),
                ],
            ),
        ),
        (
            "prefix",
            vec![
                seg(0, &go),
                seg(300, &go),
                seg(600, &[" Go", " home.", " Now"]),
            ],
            line(
                "Go home. Go home. Go home. Now",
                &[
                    (0, "Go"),
                    (100, "home."),
                    (300, "Go"),
                    (400, "home."),
                    (600, "Go"),
                    (700, "home."),
                    (800, "Now"),
                ],
            ),
        ),
        (
            "trimmed",
            vec![
                seg(0, &go),
                seg(300, &go),
                seg(600, &[" Go", " home.", " "]),
                seg(2000, &[" Done"]),
            ],
            line(
                "Go home.<pause 1.8s> Done",
                &[(0, "Go"), (100, "home."), (2000, "Done")],
            ),
        ),
        (
            "markers apart",
            vec![
                seg(0, &go),
                seg(300, &["[_BEG_]", " Go", " home."]),
                seg(700, &go),
            ],
            line("Go home.", &[(0, "Go"), (100, "home.")]),
        ),
    ]
}

/// Segments of one word each, by name, 20 ms apart so no drop opens a pause; `""` is a segment of markers only. A
/// word is two pieces (`" A"`, `"."`, 50 ms each), so no single token repeats up to the repeat-run bound.
fn shaped(names: &[&str]) -> Vec<Seg> {
    (0_u64..)
        .zip(names)
        .map(|(i, w)| {
            let t = i.saturating_mul(20);
            let (half, end) = (t.saturating_add(50), t.saturating_add(100));
            let mark = ("[_TT_10]".to_owned(), end, end);
            if w.is_empty() {
                vec![mark]
            } else {
                vec![
                    (format!(" {w}"), t, half),
                    (".".to_owned(), half, end),
                    mark,
                ]
            }
        })
        .collect()
}

/// The line due for [`shaped`] segments when these positions (1-based) are kept.
fn kept_line(names: &[&str], keep: &[usize]) -> String {
    let recs: Vec<(u64, String)> = (0_u64..)
        .zip(names)
        .zip(1_usize..)
        .filter(|((_, w), p)| keep.contains(p) && !w.is_empty())
        .map(|((i, w), _)| (i.saturating_mul(20), format!("{w}.")))
        .collect();
    let text: Vec<&str> = recs.iter().map(|(_, w)| w.as_str()).collect();
    let recs: Vec<(u64, &str)> = recs.iter().map(|(t, w)| (*t, w.as_str())).collect();
    line(&text.join(" "), &recs)
}

/// One [`shaped`] case: its name, the segments by name, the positions kept.
fn zone(name: &'static str, names: &[&str], keep: &[usize]) -> (&'static str, Vec<Seg>, String) {
    (name, shaped(names), kept_line(names, keep))
}

/// The 2026-11-25 line's shape: 31 segments, 1-11 distinct, A at 12-14, 17-20, 25-30, B at 22-24, others at 15,
/// 16, 21, 31; `three` replaces segment 3.
fn live_shape(three: &'static str) -> Vec<&'static str> {
    vec![
        "s1", "s2", three, "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "A", "A", "A", "s15",
        "s16", "A", "A", "A", "A", "s21", "B", "B", "B", "A", "A", "A", "A", "A", "A", "s31",
    ]
}

/// The made-up loop-zone patterns (the r2 rule): only a loop that reaches the tail is cut, from its zone on.
fn zone_cases() -> Vec<(&'static str, Vec<Seg>, String)> {
    let all = |n: usize| (1..=n).collect::<Vec<_>>();
    let live_keep = [all(11), vec![12, 15, 16, 21, 22, 31]].concat();
    vec![
        // T1 (P1): a real triple mid-line, no loop after it: every copy stays.
        zone(
            "mid triple",
            &[
                "s1", "s2", "s3", "s4", "s5", "A", "A", "A", "s9", "s10", "s11", "s12", "s13",
                "s14",
            ],
            &all(14),
        ),
        // P1 tail bound: the last copy two before the end is not the tail.
        zone(
            "tail minus two",
            &["s1", "s2", "A", "A", "A", "s6", "s7"],
            &all(7),
        ),
        // T2 (P2/P3): the live shape.
        zone("live shape", &live_shape("s3"), &live_keep),
        // T3 (P2): a copy of A at 3, in real speech, 9 before the next: it stays, as does all before the zone.
        zone("live shape, A at 3", &live_shape("A"), &live_keep),
        // T4 (P1): twice at the tail is speech.
        zone(
            "twice at the tail",
            &["s1", "s2", "s3", "s4", "A", "A"],
            &all(6),
        ),
        // T5 (P4): B's zone starts at 2, A's at 5: the earlier wins.
        zone(
            "two loops",
            &["s1", "B", "s3", "B", "A", "A", "s7", "s8", "A", "B"],
            &[1, 2, 3, 5, 7, 8],
        ),
        // P2 spacing bound: A 7 apart (1 to 8) stays, 6 apart (8 to 14) starts the zone.
        zone(
            "spacing",
            &[
                "A", "s2", "s3", "s4", "s5", "s6", "s7", "A", "s9", "s10", "s11", "s12", "s13",
                "A", "s15",
            ],
            &[all(13), vec![15]].concat(),
        ),
        // P7: a markers-only segment takes no position: A at 2 and 9 are 6 apart, the zone starts at 2.
        zone(
            "markers-only between",
            &["s1", "A", "s3", "s4", "s5", "s6", "s7", "", "A", "A"],
            &all(8),
        ),
        // P7: a markers-only segment last is not the tail: A at 5 is.
        zone(
            "markers-only last",
            &["s1", "s2", "A", "A", "A", "s6", ""],
            &[1, 2, 3, 6],
        ),
        // P6: a leak line looped is unread on both sides (every copy a credit line, nothing left).
        (
            "leak loop",
            vec![
                seg(0, &[" Thanks", " for", " watching!"]),
                seg(400, &[" Thanks", " for", " watching!"]),
                seg(800, &[" Thanks", " for", " watching!"]),
            ],
            "<voice 3.4s>".to_owned(),
        ),
    ]
}

/// The repeat-run bound counts the tokens before the loop drop: six one-token segments stay a decode loop.
fn bound_case() -> (&'static str, Vec<Seg>, String) {
    let la: Vec<Seg> = [0, 100, 200, 300, 400, 500]
        .into_iter()
        .map(|t| seg(t, &[" la"]))
        .collect();
    ("maxrun first", la, "<voice 3.4s>".to_owned())
}

/// Made-up segment patterns, each through the awk and the Rust assembler, each side against the line due: a loop
/// text (3 copies, the last at the tail) is cut from its zone on, twice keeps both, a triple mid-line stays, compared
/// exactly on the trimmed text, markers apart.
#[test]
fn a_segment_text_three_times_keeps_the_first_and_twice_keeps_both() -> Res<()> {
    let awk = script_awk()?;
    let made = std::env::temp_dir().join(format!("tulpa-parity-loops-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    for (name, segments, due) in count_cases()
        .into_iter()
        .chain(exact_cases())
        .chain(zone_cases())
        .chain([bound_case()])
    {
        let p = made.join(format!("{}.json", name.replace(' ', "-")));
        let refs = segment_refs(&segments);
        let refs: Vec<&[(&str, u64, u64)]> = refs.iter().map(Vec::as_slice).collect();
        std::fs::write(&p, ojf_segments(&refs)).map_err(|e| e.to_string())?;
        for (side, line) in both(&awk, &p)? {
            assert_eq!(line, due, "{name}: {side}");
        }
    }
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}

/// The verdict line due for a piece Rust judged: `read`, or the rule in [`Why::as_str`]'s names with Rust's own
/// counts (the awk adds `ms` and, for a credit leak, the family — checked against the plan, not Rust, which has none).
fn rust_verdict(json: &[u8]) -> String {
    let p = piece(&Transcript::new(ojf_tokens(json), false), EPOCH, MS);
    p.unread.map_or_else(
        || {
            if p.leaks == 0 {
                "read".to_owned()
            } else {
                format!(
                    "credit-dropped n={} ms={MS}{}",
                    p.leaks,
                    flag_fields(&p.flagged)
                )
            }
        },
        |u| {
            format!(
                "voice-unread:{} nt={} m={} maxrun={} ms={MS}{}",
                u.why.as_str(),
                u.nt,
                u.m,
                u.max_run,
                flag_fields(&p.flagged)
            )
        },
    )
}

/// A verdict line without its `family=` field (the awk names a dropped credit's family, the Rust port has no such
/// field; every other field, the flagged ones included, is both copies').
fn no_family(verdict: &str) -> String {
    verdict
        .split(' ')
        .filter(|f| !f.starts_with("family="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A small deterministic generator (xorshift64, one fixed seed), so a red case replays.
struct Gen(u64);

impl Gen {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).unwrap_or(1);
        usize::try_from(self.next().checked_rem(n).unwrap_or(0)).unwrap_or(0)
    }
    /// From `lo` to `hi`, both included.
    fn within(&mut self, lo: usize, hi: usize) -> usize {
        lo.saturating_add(self.below(hi.saturating_sub(lo).saturating_add(1)))
    }
    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of.get(self.below(of.len())).copied().unwrap_or_default()
    }
}

/// Each credit family the script names, with a line whisper recites for it (whole, as its own piece).
const FAMILIES: [(&str, &str); 17] = [
    ("subtitles-created", "Субтитры создавал DimaTorzok"),
    ("subtitles-made", "Субтитры сделал DimaTorzok"),
    (
        "subtitle-editor",
        "Редактор субтитров А.Семкин Корректор А.Егорова",
    ),
    ("subtitle-editor", "Редактор субтитров А.Семкин"),
    ("subtitle-editor", "Редактор субтитров И.Петрова"),
    ("subscribe", "ПОДПИШИСЬ НА КАНАЛ"),
    ("subscribe", "ПОДПИШИСЬ НА НАШ КАНАЛ"),
    ("dynamic-music", "ДИНАМИЧНАЯ МУЗЫКА"),
    ("thanks-for-viewing-ru", "Спасибо за просмотр!"),
    ("enjoy-viewing-ru", "Приятного просмотра!"),
    ("to-be-continued-ru", "Продолжение следует..."),
    ("thank-you-for-watching", "Thank you for watching!"),
    ("thanks-for-watching", "Thanks for watching!"),
    ("silence-filler", "Спасибо."),
    ("silence-filler", "Спасибо!"),
    ("silence-filler", "Thank you."),
    ("credit-name", "DimaTorzok"),
];

/// Speech words: whole words with their leading space, and words whisper's turbo model splits into pieces.
const WORDS: [&str; 12] = [
    " да",
    " нет",
    " мы",
    " завтра",
    " встреча",
    " в",
    " офисе,",
    " ok",
    " ну",
    " так.",
    " Привет",
    " what?",
];
const PIECES: [[&str; 2]; 4] = [
    [" При", "вет"],
    [" сего", "дня"],
    [" ка", "нал"],
    [" sub", "titles"],
];
const MARKERS: [&str; 4] = ["[_BEG_]", "[_TT_42]", "[_TT_1500]", "[_EOT_]"];

/// Generated segments, owned: each a list of tokens (text, from, to).
type Segs = Vec<Vec<(String, u64, u64)>>;

/// One generated piece: the rule the plan says decides it (`read` for speech) with the family for a credit leak,
/// and its segments of tokens (text, from, to).
fn generated(g: &mut Gen) -> (String, Segs) {
    let mut at = 0_u64;
    let mut tok = |t: &str, gap: u64| {
        at = at.saturating_add(gap);
        let from = at;
        at = at.saturating_add(100);
        (t.to_owned(), from, at)
    };
    let marked = |g: &mut Gen, mut toks: Vec<(String, u64, u64)>| {
        if g.below(2) == 0 {
            toks.insert(0, ("[_BEG_]".to_owned(), 0, 0));
        }
        if g.below(2) == 0 {
            let end = toks.last().map_or(0, |t| t.2);
            toks.push(("[_TT_150]".to_owned(), end, end));
        }
        toks
    };
    match g.below(6) {
        0 => {
            let segs = g.within(1, 3);
            let mut out = Vec::new();
            let mut last = "";
            for _ in 0..segs {
                let mut toks = Vec::new();
                for _ in 0..g.within(1, 6) {
                    let gap = if g.below(5) == 0 { 600 } else { 0 };
                    if g.below(4) == 0 {
                        let [a, b] = PIECES
                            .get(g.below(PIECES.len()))
                            .copied()
                            .unwrap_or_default();
                        toks.push(tok(a, gap));
                        toks.push(tok(b, 0));
                    } else {
                        // Never the word just said: two runs of it back to back would add up to a decode loop.
                        let mut w = g.pick(&WORDS);
                        while w == last {
                            w = g.pick(&WORDS);
                        }
                        last = w;
                        for _ in 0..g.within(1, 5) {
                            toks.push(tok(w, gap));
                        }
                    }
                }
                out.push(marked(g, toks));
            }
            ("read".to_owned(), out)
        }
        1 => {
            let toks = (0..g.within(1, 4))
                .map(|_| tok(g.pick(&MARKERS), 0))
                .collect();
            ("markers-only".to_owned(), vec![toks])
        }
        2 => {
            let toks = if g.below(2) == 0 {
                Vec::new()
            } else {
                vec![("x".to_owned(), 500, 100)] // from > to: no token at all, as the awk and Rust read it
            };
            ("no-tokens".to_owned(), vec![toks])
        }
        3 => {
            let toks = (0..g.within(1, 3)).map(|_| tok(" ", 0)).collect();
            ("empty-text".to_owned(), vec![marked(g, toks)])
        }
        4 => {
            let (family, text) = FAMILIES
                .get(g.below(FAMILIES.len()))
                .copied()
                .unwrap_or_default();
            let toks = text
                .split(' ')
                .enumerate()
                .map(|(i, w)| {
                    tok(
                        &if i == 0 {
                            w.to_owned()
                        } else {
                            format!(" {w}")
                        },
                        0,
                    )
                })
                .collect();
            (
                format!("credit-leak family={family}"),
                vec![marked(g, toks)],
            )
        }
        _ => {
            let mut toks: Vec<_> = (0..g.below(3)).map(|_| tok(g.pick(&WORDS), 0)).collect();
            let w = g.pick(&[" ла", " ПОДПИШИСЬ", " да"]);
            let run = g.within(6, 12);
            toks.extend((0..run).map(|_| tok(w, 0)));
            ("maxrun".to_owned(), vec![marked(g, toks)])
        }
    }
}

/// #0454 B1: every piece the awk prints as `<voice N.Ns>` carries exactly one verdict naming the rule that decided it
/// and its inputs, and every piece it reads carries `read` — over generated pieces that hit each rule (and readable
/// ones), each checked against two oracles: the generator's plan (the rule it planted, the credit family) and the Rust
/// port (the same rule names, the same counts). The real whisper captures go through the same checks.
#[test]
fn every_unread_piece_names_its_rule_and_inputs() -> Res<()> {
    let awk = script_awk()?;
    let made = std::env::temp_dir().join(format!("tulpa-verdict-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let mut g = Gen(0x9e37_79b9_7f4a_7c15);
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut files: Vec<(PathBuf, Option<String>)> = Vec::new();
    for i in 0..600 {
        let (plan, segs) = generated(&mut g);
        let refs: Vec<Vec<(&str, u64, u64)>> = segs
            .iter()
            .map(|s| s.iter().map(|(w, a, b)| (w.as_str(), *a, *b)).collect())
            .collect();
        let slices: Vec<&[(&str, u64, u64)]> = refs.iter().map(Vec::as_slice).collect();
        let p = made.join(format!("g{i}.json"));
        std::fs::write(&p, ojf_segments(&slices)).map_err(|e| e.to_string())?;
        files.push((p, Some(plan)));
    }
    files.extend(real_captures()?.into_iter().map(|p| (p, None)));
    for (f, plan) in &files {
        let (line, verdict) = awk_out(&awk, f)?;
        let line = String::from_utf8(line).map_err(|e| e.to_string())?;
        let json = std::fs::read(f).map_err(|e| e.to_string())?;
        let name = f.display();
        assert_eq!(
            line == "<voice 3.4s>",
            verdict.starts_with("voice-unread:"),
            "{name}: a <voice> piece has a rule, a read one none: {line:?} / {verdict}"
        );
        assert!(
            !verdict.contains('\n')
                && (verdict == "read"
                    || verdict.starts_with("voice-unread:")
                    || verdict.starts_with("credit-dropped n=")),
            "{name}: one verdict line: {verdict:?}"
        );
        let (rule, family) = verdict
            .strip_prefix("voice-unread:")
            .map_or(("read", None), |v| {
                let rule = v.split(' ').next().unwrap_or_default();
                (rule, v.split(" family=").nth(1))
            });
        let rule = if verdict.starts_with("credit-dropped") {
            "credit-dropped"
        } else {
            rule
        };
        let n = seen.entry(rule.to_owned()).or_default();
        *n = n.saturating_add(1);
        if let Some(plan) = plan {
            let (want, want_family) = plan
                .split_once(" family=")
                .map_or((plan.as_str(), None), |(r, f)| (r, Some(f)));
            assert_eq!(
                (rule, family),
                (want, want_family),
                "{name}: the planted rule: {verdict}"
            );
        }
        let rust = rust_verdict(&json);
        assert_eq!(
            no_family(&verdict),
            rust,
            "{name}: the Rust port's rule and counts"
        );
    }
    for rule in [
        "read",
        "no-tokens",
        "markers-only",
        "empty-text",
        "credit-leak",
        "maxrun",
    ] {
        assert!(
            seen.get(rule).is_some_and(|&n| n >= 20),
            "{rule} too rare: {seen:?}"
        );
    }
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}

/// Writes generated segments as an `-ojf` file.
fn write_segs(p: &Path, segs: &Segs) -> Res<()> {
    let refs: Vec<Vec<(&str, u64, u64)>> = segs
        .iter()
        .map(|s| s.iter().map(|(w, a, b)| (w.as_str(), *a, *b)).collect())
        .collect();
    let slices: Vec<&[(&str, u64, u64)]> = refs.iter().map(Vec::as_slice).collect();
    std::fs::write(p, ojf_segments(&slices)).map_err(|e| e.to_string())
}

/// The case file's cases pinned by name (the generator's `NAMED`), so a change to one is deliberate: credit words after
/// speech in one segment are speech (#0454 B1 D11, D8'); a signed credit inside a segment is still dropped, one per
/// signed family (D12); a credit opening right after a signature is a credit (D14); and the editor's name slot (P6a,
/// D16, D19, D22).
const NAMED_CASES: [&str; 33] = [
    "d11-subscribe-after-speech",
    "d8x-subscribe-after-stop",
    "d12-subtitles-created-mid",
    "d12-subtitles-made-mid",
    "d12-credit-name-mid",
    "d14-editor-after-signature",
    "p6a-cyr-capitalised",
    "p6a-lat-capitalised",
    "p6a-cyr-initial-then-speech",
    "p6a-lat-initial-then-speech",
    "p6a-cyr-caps",
    "p6a-lat-caps",
    "p6a-caps-word-in-mixed",
    "p6a-verb-after-start",
    "p6a-three-names",
    "p6a-long",
    "p6a-short",
    "d16a-two-words-whole",
    "d16b-surname-then-speech",
    "d16c-two-words-then-speech",
    "d16d-glued-then-speech",
    "d16e-long-glued-then-speech",
    "d16f-long-two-words-whole",
    "d17g-initial-space-surname-whole",
    "d17h-initial-space-surname-then-speech",
    "d19i-initial-then-speech",
    "d19j-two-initials-surname-whole",
    "d19k-two-initials-surname-then-speech",
    "d19l-initial-at-segment-end",
    "d19m-two-words-stop-speech-one-segment",
    "d19n-two-words-stop-then-next-segment",
    "d22o-initial-then-glued-then-speech",
    "d16p-korrektor-right-after-start",
];

/// The case file of [`a_credit_costs_only_its_own_words`], written once by `fixtures/credit-cases.py`.
const CREDIT_CASES: &str = "tests/fixtures/credit-cases.txt";

/// One case of [`CREDIT_CASES`]: its name, its segments of tokens (text, from, to), the expected line and verdict.
struct CreditCase {
    name: String,
    segs: Segs,
    line: String,
    verdict: String,
}

/// Reads [`CREDIT_CASES`] (`case`, `seg`, `tok <from> <to> <text>`, `line`, `verdict` records).
fn credit_cases() -> Res<Vec<CreditCase>> {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(CREDIT_CASES);
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut out: Vec<CreditCase> = Vec::new();
    for rec in text.lines() {
        let (key, rest) = rec.split_once(' ').unwrap_or((rec, ""));
        let case = out.last_mut();
        match (key, case) {
            ("case", _) => out.push(CreditCase {
                name: rest.to_owned(),
                segs: Segs::new(),
                line: String::new(),
                verdict: String::new(),
            }),
            ("seg", Some(c)) => c.segs.push(Vec::new()),
            ("tok", Some(c)) => {
                let mut fields = rest.splitn(3, ' ');
                let num = |v: Option<&str>| -> Res<u64> {
                    v.and_then(|v| v.parse().ok())
                        .ok_or_else(|| format!("a bad tok: {rec}"))
                };
                let (from, to) = (num(fields.next())?, num(fields.next())?);
                let word = fields.next().ok_or_else(|| format!("a bad tok: {rec}"))?;
                c.segs
                    .last_mut()
                    .ok_or_else(|| format!("a tok before a seg: {rec}"))?
                    .push((word.to_owned(), from, to));
            }
            ("line", Some(c)) => rest.clone_into(&mut c.line),
            ("verdict", Some(c)) => rest.clone_into(&mut c.verdict),
            _ => return Err(format!("{CREDIT_CASES}: {rec}")),
        }
    }
    Ok(out)
}

/// #0454 B1 item 1b P1-P3: a credit removes only its own words. Over every case of [`CREDIT_CASES`] (each credit
/// family, the name and the silence fillers included, at every position, beside every neighbour, in its own whisper
/// segment and sharing one with the speech before it, after it or both, with and without a sentence end, at gaps of
/// 499 and 500 ms after it),
/// the script's awk and the Rust port each print the case's expected line and verdict. The expectations come from the
/// property, not from either copy: the base assembler's line for the same piece with each credit removed by hand (a
/// silence filler beside a real word stays), or `<voice>` and `credit-leak` naming the first family when no real word
/// is left. The Rust verdict has no family; the rest of it must match.
#[test]
fn a_credit_costs_only_its_own_words() -> Res<()> {
    let awk = script_awk()?;
    let cases = credit_cases()?;
    let made = std::env::temp_dir().join(format!("tulpa-credit-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let mut axes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut generated: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut bad: Vec<String> = Vec::new();
    for c in &cases {
        if NAMED_CASES.contains(&c.name.as_str()) {
            axes.insert(format!("named {}", c.name));
        } else if let Some(p9) = c.name.strip_prefix("p9-") {
            // p9-<f>-<family>-<tail>-<case>-<single|double>-<own|after>
            let parts: Vec<&str> = p9.rsplitn(5, '-').collect();
            let [share, _, case, tail, fam] = parts.as_slice() else {
                return Err(format!("a case name: {}", c.name));
            };
            let fam = fam.split_once('-').map_or(*fam, |(_, f)| f);
            axes.insert(format!("p9 {fam} {tail} {case} {share}"));
        } else if let Some(d8) = c.name.strip_prefix("d8-") {
            // d8-<f>-<family>-<segstart|stop|comma|plain|gap>-<case>-<none|dot|comma|dots>
            let parts: Vec<&str> = d8.rsplitn(4, '-').collect();
            let [punct, case, opener, fam] = parts.as_slice() else {
                return Err(format!("a case name: {}", c.name));
            };
            let fam = fam.split_once('-').map_or(*fam, |(_, f)| f);
            axes.insert(format!("d8 {fam} {opener} {case} {punct}"));
        } else {
            // <f>-<family>-<position>-<neighbour>-<own|before|after|both>-<stop|nostop>-gap<ms>
            let parts: Vec<&str> = c.name.rsplitn(6, '-').collect();
            let [gap, stop, share, nb, pos, fam] = parts.as_slice() else {
                return Err(format!("a case name: {}", c.name));
            };
            let fam = fam.split_once('-').map_or(*fam, |(_, f)| f);
            axes.insert(format!("{fam} {pos} {nb} {share} {gap}"));
            axes.insert(format!("{fam} {share} {stop} {gap}"));
            generated.insert((*fam).to_owned());
        }
        let p = made.join("case.json");
        write_segs(&p, &c.segs)?;
        // The case file's timings are from the utterance's start.
        let (line, verdict) = awk_out_at(&awk, &p, 0)?;
        let line = String::from_utf8(line).map_err(|e| e.to_string())?;
        if (line.as_str(), verdict.as_str()) != (c.line.as_str(), c.verdict.as_str()) {
            bad.push(format!("{}: the awk: {line:?} / {verdict}", c.name));
        }
        let json = std::fs::read(&p).map_err(|e| e.to_string())?;
        let rust = String::from_utf8_lossy(&rust_piece_at(&json, 0).0).into_owned();
        if rust != c.line {
            bad.push(format!("{}: the Rust port's piece: {rust:?}", c.name));
        }
        let rust = rust_verdict(&json);
        if rust != no_family(&c.verdict) {
            bad.push(format!("{}: the Rust port's verdict: {rust}", c.name));
        }
    }
    // All of them, so a red run says how far the copies are from the property, by which copy.
    assert!(
        bad.is_empty(),
        "{} of {} checks off the property; the first: {:#?}",
        bad.len(),
        cases.len().saturating_mul(3),
        bad.iter().take(8).collect::<Vec<_>>()
    );
    every_axis(&axes, &generated);
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}
/// Every case the generator promises is in the case file: each named case, the generator's families as [`FAMILIES`]
/// (an extra on either side is red), and every family on every axis (`axes`, as [`a_credit_costs_only_its_own_words`]
/// reads them off the case names).
fn every_axis(
    axes: &std::collections::BTreeSet<String>,
    generated: &std::collections::BTreeSet<String>,
) {
    for name in NAMED_CASES {
        assert!(axes.contains(&format!("named {name}")), "no case {name}");
    }
    // The generator's families and these are one set: an extra on either side is red.
    let want: std::collections::BTreeSet<String> =
        FAMILIES.iter().map(|(f, _)| (*f).to_owned()).collect();
    assert_eq!(*generated, want, "the case file's families vs FAMILIES");
    // Every family the generator plants, on every axis it promises.
    for fam in &want {
        // D8': each family but the signed ones (a line of their own wherever they sit, D12), at a segment start or
        // inside a segment.
        if !["subtitles-created", "subtitles-made", "credit-name"].contains(&fam.as_str()) {
            for opener in ["segstart", "stop", "comma", "plain", "gap"] {
                // Size: where nothing is recognised, two cases (the generator's D8_CASES).
                let cases: &[&str] = if ["segstart", "stop"].contains(&opener) {
                    &["as", "lower", "title", "upper"]
                } else {
                    &["as", "upper"]
                };
                for case in cases {
                    for punct in ["none", "dot", "comma", "dots"] {
                        let due = format!("d8 {fam} {opener} {case} {punct}");
                        assert!(axes.contains(&due), "no case for {due}");
                    }
                }
            }
        }
        for gap in ["gap499", "gap500"] {
            for pos in ["only", "first", "middle", "last", "two"] {
                for nb in ["speech", "credit", "empty", "markers"] {
                    // In its own segment a credit's line ends at the bound whatever the gap: one gap made (size).
                    let due = format!("{fam} {pos} {nb} own gap499");
                    assert!(axes.contains(&due), "no case for {due}");
                }
            }
            // A bare filler is speech: the fillers come with their stop only.
            let stops: &[&str] = if fam == "silence-filler" {
                &["stop"]
            } else {
                &["stop", "nostop"]
            };
            for share in ["before", "after", "both"] {
                for stop in stops {
                    let due = format!("{fam} {share} {stop} {gap}");
                    assert!(axes.contains(&due), "no case for {due}");
                }
            }
        }
        // P9: every spelling of the family's texts, alone and with speech glued after.
        for tail in ["none", "dot", "comma", "bang", "dots", "more"] {
            for case in ["as", "lower", "title", "upper"] {
                // An all-caps text's upper spelling is its own, made once.
                let caps = FAMILIES
                    .iter()
                    .filter(|(f, _)| f == fam)
                    .all(|(_, t)| t.to_uppercase() == *t);
                for share in ["own", "after"] {
                    let due = format!("p9 {fam} {tail} {case} {share}");
                    assert!(
                        axes.contains(&due) || (case == "upper" && caps),
                        "no case for {due}"
                    );
                }
            }
        }
    }
}
/// #0454 B1 item 1b P5 (review N1): an unread piece whose tail loop is cut reports `m` as every text token, before
/// the cut — the Rust port's `Unread.m` — not the tokens kept after it.
#[test]
fn a_looped_unread_piece_counts_its_tokens_before_the_cut() -> Res<()> {
    let awk = script_awk()?;
    let made = std::env::temp_dir().join(format!("tulpa-t1-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let p = made.join("t1.json");
    let la: Vec<(&str, u64, u64)> = (0..6).map(|i| (" ла", i * 100, i * 100 + 100)).collect();
    let tail = [(" да", 0, 0), (" нет", 0, 0)];
    let mut segs: Vec<Vec<(&str, u64, u64)>> = vec![la];
    for k in 1..=3_u64 {
        segs.push(
            tail.iter()
                .map(|&(w, _, _)| (w, 1000 * k, 1000 * k + 100))
                .collect(),
        );
    }
    let slices: Vec<&[(&str, u64, u64)]> = segs.iter().map(Vec::as_slice).collect();
    std::fs::write(&p, ojf_segments(&slices)).map_err(|e| e.to_string())?;
    let (line, verdict) = awk_out(&awk, &p)?;
    assert_eq!(line, b"<voice 3.4s>");
    // 12 text tokens; the loop cut would leave 8.
    assert_eq!(
        verdict,
        format!("voice-unread:maxrun nt=12 m=12 maxrun=6 ms={MS}")
    );
    let json = std::fs::read(&p).map_err(|e| e.to_string())?;
    assert_eq!(
        rust_verdict(&json),
        format!("voice-unread:maxrun nt=12 m=12 maxrun=6 ms={MS}")
    );
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}

/// The script's credit rules and the generator's families are one set: every label `credit_leak` and `leak_open`
/// can return is planted by [`FAMILIES`], and every family's line is a credit to the Rust port too. A rule added to the
/// awk alone, or a family dropped from it, goes red here (the awk and assemble.rs keep two copies of the list; this
/// and [`a_credit_costs_only_its_own_words`] keep them in lockstep).
#[test]
fn every_credit_rule_of_the_script_is_generated() -> Res<()> {
    let awk = script_awk()?;
    // The labels the awk returns as such, and its starts' (`ls("<label>", "<start>")` in its tables).
    let after = |pat: &str| -> Vec<&str> {
        awk.split(pat)
            .skip(1)
            .filter_map(|r| r.split('"').next())
            .collect()
    };
    let mut labels: Vec<&str> = after("return \"");
    labels.extend(after("ls(\""));
    labels.retain(|l| !l.is_empty());
    labels.sort_unstable();
    labels.dedup();
    let mut planted: Vec<&str> = FAMILIES.iter().map(|(f, _)| *f).collect();
    planted.sort_unstable();
    planted.dedup();
    assert_eq!(
        labels, planted,
        "the awk's credit labels vs the generator's families"
    );
    for (family, line) in FAMILIES {
        assert!(tulpa_hear::assemble::credit_leak(line), "{family}: {line}");
    }
    Ok(())
}

/// The awk's credit functions with a driver that prints `norm()` of each input line, under `locale`.
fn awk_norm(awk: &str, locale: &str, lines: &[String]) -> Res<Vec<String>> {
    let (_, fns) = awk
        .split_once("\nfunction credit_leak")
        .ok_or("no credit_leak in the awk")?;
    let prog = format!("function credit_leak{fns}\nBEGIN {{ tables() }} {{ print norm($0) }}\n");
    let mut child = Command::new("awk")
        .env("LC_ALL", locale)
        .arg(prog)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("awk: {e}"))?;
    let mut input = lines.join("\n");
    input.push('\n');
    std::io::Write::write_all(
        &mut child.stdin.take().ok_or("no awk stdin")?,
        input.as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    assert!(out.status.success(), "awk norm under {locale}");
    Ok(String::from_utf8(out.stdout)
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// #0454 B1 P7: the two copies compare text through one normaliser. Over every ASCII character, the Russian alphabet
/// both ways, the listed non-ASCII punctuation, letters outside both alphabets and folded sentences, the awk's
/// `norm()` (its explicit case table: no awk here or on the Mac folds Cyrillic with `tolower()`) prints what
/// `normal()` gives, in the C locale and in a UTF-8 one. And the table is Rust's own fold: every letter of A–Z, А–Я
/// and Ё folds to its `char::to_lowercase`, every other letter stays, every punctuation mark is a word break.
#[test]
fn the_awk_and_the_rust_port_fold_text_alike() -> Res<()> {
    let awk = script_awk()?;
    let upper: Vec<char> = ('A'..='Z').chain('А'..='Я').chain(['Ё']).collect();
    let mut inputs: Vec<String> = upper
        .iter()
        .flat_map(|c| c.to_lowercase().chain([*c]))
        .chain(('0'..='9').chain(['é', 'É', 'ß', 'ї', 'Ї', '中']))
        .map(|c| format!("x{c}y"))
        .collect();
    inputs.extend(
        (1_u8..128)
            .filter(|b| *b != b'\n')
            .map(|b| format!("a{}b", char::from(b))),
    );
    inputs.extend(PUNCTUATION.iter().map(|c| format!("a{c}b")));
    inputs.extend(
        [
            "  Thanks  For,watching!!  ",
            "Редактор субтитров А.Семкин",
            "…ПОДПИШИСЬ — НА «НАШ» КАНАЛ…",
            "ЁЖ ёж",
        ]
        .map(str::to_owned),
    );
    let want: Vec<String> = inputs.iter().map(|i| normal(i)).collect();
    for locale in ["C", "C.UTF-8"] {
        let got = awk_norm(&awk, locale, &inputs)?;
        for ((i, g), w) in inputs.iter().zip(&got).zip(&want) {
            assert_eq!(g, w, "{locale}: norm({i:?})");
        }
        assert_eq!(got.len(), want.len(), "{locale}: one line per input");
    }
    for c in &upper {
        let lower: String = c.to_lowercase().collect();
        assert_eq!(normal(&c.to_string()), lower, "the fold of {c}");
    }
    assert_eq!(normal("xéÉy"), "xéÉy", "a letter outside A–Z and А–Я stays");
    for c in PUNCTUATION.iter().copied().chain([',', '.', '\t', '-']) {
        assert_eq!(normal(&format!("a{c}b")), "a b", "{c:?} is a word break");
    }
    Ok(())
}

/// One P9 spelling of a credit list entry as whisper's tokens, one per word: `case` 0 as listed, 1 lower, 2 upper, 3
/// title; each word after the first with `sep` before it; `tail` appended to the last word, or its own words when it
/// begins with a space.
fn spelling(entry: &str, tail: &str, case: u8, sep: &str) -> Vec<String> {
    let text = match case {
        0 => entry.to_owned(),
        1 => entry.to_lowercase(),
        2 => entry.to_uppercase(),
        _ => entry
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                c.next().map_or_else(String::new, |f| {
                    f.to_uppercase()
                        .chain(c.flat_map(char::to_lowercase))
                        .collect()
                })
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    // One token per word, each after the first with `sep` before it; the tail on the last word, or
    // its own words.
    let mut words: Vec<String> = text
        .split(' ')
        .enumerate()
        .map(|(i, w)| {
            if i == 0 {
                w.to_owned()
            } else {
                format!("{sep}{w}")
            }
        })
        .collect();
    if tail.starts_with(' ') {
        words.extend(tail.split(' ').skip(1).map(|w| format!(" {w}")));
    } else if let Some(last) = words.last_mut() {
        last.push_str(tail);
    }
    words
}

/// #0454 B1 F-a: the awk and the Rust port hold one credit list. Its entries are equal as written (`ls()` and
/// `wc()` in the awk's tables against [`LEAK_STARTS`] and [`WHOLE_CREDITS`]), and each entry, in each P9 spelling
/// (punctuation after it, case, inner spacing, a longer recital), is judged alike by both: alone it is a credit
/// leak of its family; recited longer, its family's credit is dropped and flagged and the extra words read.
#[test]
fn the_awk_and_the_rust_port_hold_one_credit_list() -> Res<()> {
    let awk = script_awk()?;
    let quoted = |pat: &str| -> Vec<Vec<String>> {
        awk.split(pat)
            .skip(1)
            .filter_map(|r| r.split_once(')').map(|(args, _)| args))
            .map(|args| {
                args.split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    };
    let starts: Vec<Vec<String>> = LEAK_STARTS
        .iter()
        .map(|(f, s)| vec![(*f).to_owned(), (*s).to_owned()])
        .collect();
    assert_eq!(quoted("\n  ls("), starts, "the awk's starts vs LEAK_STARTS");
    let wholes: Vec<Vec<String>> = WHOLE_CREDITS
        .iter()
        .map(|w| vec![(*w).to_owned()])
        .collect();
    assert_eq!(
        quoted("\n  wc("),
        wholes,
        "the awk's whole texts vs WHOLE_CREDITS"
    );
    let made = std::env::temp_dir().join(format!("tulpa-fa-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let p = made.join("fa.json");
    let mut bad: Vec<String> = Vec::new();
    let mut n = 0_usize;
    let entries = LEAK_STARTS
        .iter()
        .copied()
        .chain(WHOLE_CREDITS.iter().map(|w| {
            let f = LEAK_STARTS
                .iter()
                .find(|(_, s)| w.starts_with(s))
                .map_or("", |(f, _)| *f);
            (f, *w)
        }));
    for (family, entry) in entries {
        for tail in ["", ".", ",", "!", "…", " завтра", " завтра встреча"] {
            for case in 0..4_u8 {
                for sep in [" ", "  "] {
                    let words = spelling(entry, tail, case, sep);
                    let spelled = words.concat();
                    let toks: Vec<(&str, u64, u64)> = (0_u64..)
                        .zip(&words)
                        .map(|(i, w)| (w.as_str(), i * 100, i * 100 + 100))
                        .collect();
                    std::fs::write(&p, ojf(&toks)).map_err(|e| e.to_string())?;
                    let (line, verdict) = awk_out_at(&awk, &p, 0)?;
                    let json = std::fs::read(&p).map_err(|e| e.to_string())?;
                    let rust = rust_piece_at(&json, 0).0;
                    let longer = tail.starts_with(' ');
                    let want = if longer {
                        format!(
                            "credit-dropped n=1 ms={MS} family={family} flagged=1 flagged-family={family}"
                        )
                    } else {
                        format!(
                            "voice-unread:credit-leak nt={0} m={0} maxrun=1 ms={MS} family={family}",
                            words.len()
                        )
                    };
                    let rust_v = rust_verdict(&json);
                    if verdict != want || line != rust || no_family(&verdict) != rust_v {
                        bad.push(format!(
                            "{spelled:?}: the awk {verdict} / the Rust port {rust_v}"
                        ));
                    }
                    n = n.saturating_add(1);
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {n} spellings: {:#?}",
        bad.len(),
        bad.iter().take(6).collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}

/// #0454 B1 P6a: the editor credit's name slot judges a word by its case as written, alike in both copies (the awk
/// under `LC_ALL=C`, where no class or `tolower()` knows a Cyrillic letter), letter by letter. Over A–Z, А–Я and Ё
/// with their lowercase, each letter as an initial (`Ж.`, `ж.`), opening a capitalised word, a lowercase word and an
/// all-caps one, after the start as written and after an all-caps start: a name-shaped word (an initial or a
/// capitalised word; in an all-caps credit an initial or an all-caps word) is the credit's own, so only the word
/// after it reads; any other word reads with it.
#[test]
fn the_awk_and_the_rust_port_judge_a_name_alike() -> Res<()> {
    let awk = script_awk()?;
    let made = std::env::temp_dir().join(format!("tulpa-p6a-{}", std::process::id()));
    std::fs::create_dir_all(&made).map_err(|e| e.to_string())?;
    let p = made.join("p6a.json");
    let mut bad: Vec<String> = Vec::new();
    let mut n = 0_usize;
    for up in ('A'..='Z').chain('А'..='Я').chain(['Ё']) {
        let low: String = up.to_lowercase().collect();
        let (tail_low, tail_up) = if up.is_ascii() {
            ("min", "MIN")
        } else {
            ("мин", "МИН")
        };
        let words = [
            (format!("{up}."), true, true),
            (format!("{low}."), false, false),
            (format!("{up}{tail_low}"), true, false),
            (format!("{low}{tail_low}"), false, false),
            (format!("{up}{tail_up}"), false, true),
        ];
        for (word, name_as_written, name_in_caps) in words {
            for (start, name) in [
                (["Редактор", " субтитров"], name_as_written),
                (["РЕДАКТОР", " СУБТИТРОВ"], name_in_caps),
            ] {
                let word = format!(" {word}");
                let toks = [
                    (start[0], 0, 100),
                    (start[1], 100, 200),
                    (word.as_str(), 200, 300),
                    (" завтра", 300, 400),
                ];
                std::fs::write(&p, ojf(&toks)).map_err(|e| e.to_string())?;
                let (line, verdict) = awk_out_at(&awk, &p, 0)?;
                let json = std::fs::read(&p).map_err(|e| e.to_string())?;
                let rust = rust_piece_at(&json, 0).0;
                let rust_v = rust_verdict(&json);
                let text = String::from_utf8_lossy(&line);
                let text = text.split('\t').next().unwrap_or_default();
                let want = if name {
                    "завтра".to_owned()
                } else {
                    format!("{} завтра", word.trim_start())
                };
                if line != rust || no_family(&verdict) != rust_v || text != want {
                    bad.push(format!(
                        "{start:?}{word:?}: the awk {text:?} {verdict} / the Rust port {:?} {rust_v} / want {want:?}",
                        String::from_utf8_lossy(&rust)
                    ));
                }
                n = n.saturating_add(1);
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {n} words: {:#?}",
        bad.len(),
        bad.iter().take(8).collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(&made).map_err(|e| e.to_string())
}
