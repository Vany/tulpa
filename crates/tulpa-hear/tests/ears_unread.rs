//! E2: no piece is lost in silence, and a credit leak is heard again with language detection.
//!
//! [`ears::run`] with a recognizer double that, like whisper held to `ru` on English speech (2026-11-24), gives
//! credit lines for the words and the words under language detection. The test runs itself again as the child,
//! twice (the debug switch off, then on), with stdout and stderr pipes: each piece that ends as `<voice N.Ns>` says
//! one stderr line with its side, length and rule, the retry's outcome said with it, and never the text; heard.log
//! keeps its six columns and no raw text; the raw text goes only into the `TULPA-EARS-DEBUG` lines on stdout, one
//! per recognized piece, and only with the switch on.

#![expect(clippy::indexing_slicing, reason = "tests: lengths asserted first")]

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tulpa_hear::assemble::{Assembler, HOLD_MAX_MS};
use tulpa_hear::ears::{self, DEBUG_ENV, Ears, Outcome, Sink, debug_switch};
use tulpa_hear::event::Event;
use tulpa_hear::hear::{CloseMode, Side};
use tulpa_hear::recognize::{DETECT, RecognizeError, Recognizer, Token, Transcript};
use tulpa_stop::Stop;

/// Set in the child's environment: run the ears over the three pieces.
const CHILD: &str = "TULPA_HEAR_EARS_UNREAD_CHILD";

/// A 200 Hz tone, loud: speech to the energy gate.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test signal generation on small, fixed values"
)]
fn speech(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n)
        .map(|i| ((i as f32 * 200.0 * std::f32::consts::TAU / 16_000.0).sin() * 8000.0) as i16)
        .collect()
}

/// A quiet room: low, non-zero noise.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test signal generation on small, fixed values"
)]
fn quiet(secs: f32) -> Vec<i16> {
    let n = (secs * 16_000.0) as usize;
    (0..n).map(|i| if i % 2 == 0 { 40 } else { -40 }).collect()
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

/// Four pieces, told apart by length (3, 4, 5 and 6 s of speech, each with the segmenter's rolls): the first is
/// credits under `ru` and words under detection, the second credits under both, the third whisper's markers only,
/// the fourth Russian words with a credit after them under `ru` and Latin letters under detection.
#[derive(Default)]
struct Wrong {
    langs: Vec<String>,
}

impl Recognizer for Wrong {
    fn transcribe(&mut self, samples: &[f32], lang: &str) -> Result<Transcript, RecognizeError> {
        self.langs.push(lang.to_owned());
        let credits = || says(&[" Субтитры", " создавал", " DimaTorzok"]);
        Ok(match (samples.len() / 16_000, lang) {
            (..=3, DETECT) => says(&[" Hello", " world"]),
            (..=4, _) => credits(),
            (5, _) => Transcript::new(Vec::new(), false).with_specials(2),
            (_, DETECT) => says(&[" Da", " net", "."]),
            _ => says(&[" Да", " нет", ".", " Субтитры", " создавал", " DimaTorzok"]),
        })
    }
}

#[derive(Default)]
struct Lines(Vec<String>);

impl Sink for Lines {
    fn line(&mut self, line: &str, _: u64) -> std::io::Result<()> {
        self.0.push(line.to_owned());
        Ok(())
    }
}

const fn now_ms() -> u64 {
    1_795_550_000_000
}

/// The child: the four pieces on "me", nothing on "them", the debug switch as [`DEBUG_ENV`] says; panics (a failed
/// exit) when a line or the log is wrong.
fn child() {
    let dir = std::env::temp_dir().join(format!("tulpa-hear-unread-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&dir);
    let rec = Arc::new(Mutex::new(Wrong::default()));
    let ears = Ears::new(
        Arc::clone(&rec),
        "ru".to_owned(),
        dir.join("me/.close"),
        CloseMode::Take,
        now_ms,
    )
    // The whole run's audio comes at once: none of it may be dropped as a backlog a capture cannot make.
    .with_backlog(Duration::from_secs(600))
    .with_debug(debug_switch(std::env::var_os(DEBUG_ENV).as_deref()) == Ok(true));
    let capture = Arc::new(|side: Side, tx: &Sender<Event>, _: &Stop| {
        if side == Side::Them {
            return Ok(());
        }
        let mut audio = quiet(1.0);
        for secs in [3.0, 4.0, 5.0, 6.0] {
            audio.extend(speech(secs));
            audio.extend(quiet(3.0));
        }
        for c in audio.chunks(1600) {
            tx.send(Event::Audio(c.to_vec()))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    });
    let mut asm = Assembler::new(&dir, None, HOLD_MAX_MS);
    let mut lines = Lines::default();
    let stop = Arc::new(Stop::new());
    let out = ears::run(&ears, &capture, &mut asm, &mut lines, &stop, None);
    let heard = std::fs::read_to_string(dir.join("heard.log")).unwrap_or_default();
    let _gone = std::fs::remove_dir_all(&dir);
    assert_eq!(out, Outcome::Ended);
    let texts: Vec<&str> = lines
        .0
        .iter()
        .filter_map(|l| l.split_once(" [me] ").map(|(_, t)| t))
        .collect();
    assert_eq!(texts.len(), 4, "{:?}", lines.0);
    assert!(texts[0].starts_with("Hello world\t"), "{texts:?}");
    for t in &texts[1..3] {
        assert!(t.starts_with("<voice ") && !t.contains('\t'), "{texts:?}");
    }
    assert!(texts[3].starts_with("Да нет.\t"), "{texts:?}");
    let langs = rec
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .langs
        .clone();
    assert_eq!(
        langs,
        ["ru", DETECT, "ru", DETECT, "ru", "ru"],
        "one retry per piece that is credits only, none else"
    );
    let unread = heard
        .lines()
        .filter(|l| l.contains("\tvoice-unread:"))
        .count();
    assert_eq!(unread, 2, "{heard}");
    assert!(heard.lines().all(|l| l.split('\t').count() == 6), "{heard}");
    // heard.log has the published phrases, as before; never what the filter dropped.
    for word in ["Субтитры", "DimaTorzok"] {
        assert!(!heard.contains(word), "{word} in heard.log:\n{heard}");
    }
}

/// The child's stdout and stderr, and whether it exited 0, with the debug switch as given.
fn run_child(debug: Option<&str>) -> (String, String, bool) {
    let exe = std::env::current_exe();
    assert!(exe.is_ok(), "{exe:?}");
    let Ok(exe) = exe else {
        return (String::new(), String::new(), false);
    };
    let mut cmd = Command::new(exe);
    cmd.args([
        "every_voice_piece_lost_is_said_and_a_leak_is_heard_again",
        "--exact",
        "--nocapture",
    ])
    .env(CHILD, "1")
    .env_remove(DEBUG_ENV)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if let Some(d) = debug {
        cmd.env(DEBUG_ENV, d);
    }
    let spawned = cmd.spawn();
    assert!(spawned.is_ok(), "{spawned:?}");
    let Ok(mut spawned) = spawned else {
        return (String::new(), String::new(), false);
    };
    let (tx, rx) = mpsc::channel();
    let readers: Vec<_> = [
        spawned
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn std::io::Read + Send>),
        spawned
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .enumerate()
    .map(|(n, pipe)| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _read = pipe.read_to_string(&mut text);
            }
            let _sent = tx.send((n, text));
        })
    })
    .collect();
    // A guard against a wedge, not a timing claim: the pipes close when the child exits.
    let mut got = [None, None];
    for _ in 0..2 {
        if let Ok((n, text)) = rx.recv_timeout(Duration::from_secs(120))
            && let Some(slot) = got.get_mut(n)
        {
            *slot = Some(text);
        }
    }
    let _killed = spawned.kill();
    let status = spawned.wait();
    for r in readers {
        let _joined = r.join();
    }
    let [out, err] = got;
    assert!(
        out.is_some() && err.is_some(),
        "the child's pipes: {out:?} {err:?}"
    );
    (
        out.unwrap_or_default(),
        err.unwrap_or_default(),
        status.is_ok_and(|s| s.success()),
    )
}

#[test]
fn every_voice_piece_lost_is_said_and_a_leak_is_heard_again() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    for debug in [None, Some("0"), Some("1")] {
        let (out, err, ok) = run_child(debug);
        assert!(ok, "the child failed (debug {debug:?}):\n{out}\n{err}");
        said_on_stderr(&err);
        let marked: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("TULPA-EARS-DEBUG "))
            .collect();
        if debug == Some("1") {
            debug_lines(&marked, &out);
        } else {
            assert!(marked.is_empty(), "debug {debug:?}: {marked:?}");
            for word in ["Субтитры", "DimaTorzok", "Hello", "нет"] {
                assert!(
                    !out.contains(word),
                    "{word} on stdout, debug {debug:?}:\n{out}"
                );
            }
        }
    }
}

/// One stderr line per piece lost or re-heard, with no word of what was said.
fn said_on_stderr(text: &str) {
    let said: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.starts_with("tulpa-hear: me: unread ") || l.starts_with("tulpa-hear: me: retry ")
        })
        .collect();
    assert_eq!(said.len(), 3, "{text}");
    assert!(
        said[0].starts_with("tulpa-hear: me: retry ")
            && said[0].ends_with(" ms: language detection recovered it"),
        "{said:?}"
    );
    assert!(
        said[1].ends_with(" s: credit-leak, language detection did not recover it"),
        "{said:?}"
    );
    assert!(said[2].ends_with(" s: markers-only"), "{said:?}");
    for word in ["Субтитры", "DimaTorzok", "Hello", "нет"] {
        assert!(!text.contains(word), "{word} on stderr:\n{text}");
    }
}

/// One debug line per recognized piece, in order, each pass with its language, the filter's verdict and reason,
/// and the raw text before the filter: the re-heard piece shows both passes, the mixed one its one pass.
fn debug_lines(marked: &[&str], out: &str) {
    let tails: Vec<&str> = marked
        .iter()
        .filter_map(|l| l.strip_prefix("TULPA-EARS-DEBUG [me] "))
        .filter_map(|l| l.split_once(" s: ").map(|(_, t)| t))
        .collect();
    assert_eq!(
        tails,
        [
            "lang=ru verdict=unread:credit-leak leak-lines=1 raw=\"Субтитры создавал DimaTorzok\" \
             | lang=auto verdict=read leak-lines=0 raw=\"Hello world\"",
            "lang=ru verdict=unread:credit-leak leak-lines=1 raw=\"Субтитры создавал DimaTorzok\" \
             | lang=auto verdict=unread:credit-leak leak-lines=1 raw=\"Субтитры создавал DimaTorzok\"",
            "lang=ru verdict=unread:markers-only leak-lines=0 raw=\"\"",
            "lang=ru verdict=read leak-lines=1 raw=\"Да нет. Субтитры создавал DimaTorzok\"",
        ],
        "{out}"
    );
}
