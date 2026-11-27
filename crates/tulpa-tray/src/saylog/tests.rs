//! saylog tests: the marks, the caps, the wire shape.

use std::time::Duration;

use super::*;

fn t(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(secs))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[test]
fn line_ok_refuses_empty_control_and_overlong_whole() {
    assert!(line_ok("стой").is_ok());
    assert_eq!(line_ok("   "), Err("an empty line goes nowhere"));
    assert_eq!(
        line_ok("a\u{0}b"),
        Err("a line with control or direction characters goes nowhere")
    );
    let long = "x".repeat(TEXT_CHARS.saturating_add(1));
    assert_eq!(line_ok(&long), Err("a line over the cap goes nowhere"));
    assert!(
        line_ok(&"x".repeat(TEXT_CHARS)).is_ok(),
        "exactly the cap passes"
    );
}

/// #0423 akira T2: the hub sends no word back for a typed line, so the window may say "on the wire"
/// at most, never "sent". The link dies right after the write, before any read: the line shows on the
/// wire, and no mark the window can show for a typed line says "sent".
#[test]
fn a_typed_line_is_on_the_wire_never_sent() {
    use crate::link::{End, LinkError, Pipe, Recv, Waker, serve_attempt};
    use tulpa_stop::Stop;
    /// Up after the hello, then dead at the first read after the typed line went out.
    struct DiesAfterWrite(Vec<Vec<u8>>, usize);
    impl Pipe for DiesAfterWrite {
        fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            self.0.push(bytes.to_vec());
            Ok(())
        }
        fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
            self.1 = self.1.saturating_add(1);
            Ok(if self.1 == 1 {
                Recv::Timeout
            } else {
                Recv::Eof
            })
        }
        fn waker(&self) -> Waker {
            std::sync::Arc::new(|| {})
        }
    }
    let saylog = std::sync::Arc::new(Mutex::new(SayLog::new()));
    let outbox = Outbox::default();
    attach(&saylog, &outbox);
    assert_eq!(
        take_typed(&saylog, &outbox, "ты тут?", t(0)),
        Typed::Wire(Wire::Queued)
    );
    let mut pipe = DiesAfterWrite(Vec::new(), 0);
    let end = serve_attempt(
        &mut pipe,
        b"hello\n",
        Duration::from_millis(1),
        &Stop::new(),
        &mut || {},
        &|_| Vec::new(),
        &outbox,
    );
    assert!(matches!(end, End::Dropped), "{end:?}");
    assert_eq!(pipe.0.len(), 2, "the hello and the typed line were written");
    let rendered = held(&saylog).render();
    assert!(
        rendered.contains("[you → kira, on the wire] ты тут?"),
        "{rendered}"
    );
    // Every mark a typed line can carry, none of them "sent".
    let mut every = SayLog::new();
    for fate in [
        Typed::Wire(Wire::Queued),
        Typed::Wire(Wire::OnWire),
        Typed::Refused("an empty line goes nowhere"),
        Typed::Wire(Wire::Lost("the queue was full")),
    ] {
        let _id = every.typed(t(0), "x", fate);
    }
    for shown in [rendered, every.render()] {
        assert!(!shown.to_lowercase().contains("sent"), "{shown}");
    }
}

/// P4: the tray's verdict is the door's (guild `src/lib/typed-line.ts` `typedLine`), both halves:
/// every line the door refuses the tray refuses, every line it takes the tray takes — as the same
/// trimmed text. The expected column is written by hand from the door's rules (JS `trim()`, then
/// empty / over 2000 code points / any of Cc, Zl, Zp, U+202A-202E, U+2066-2069 → null), not computed.
#[test]
fn line_ok_gives_the_doors_verdict_on_every_row() {
    let cap = "x".repeat(TEXT_CHARS);
    let over = "x".repeat(TEXT_CHARS.saturating_add(1));
    let flowers = "🌸".repeat(TEXT_CHARS);
    let flowers_over = "🌸".repeat(TEXT_CHARS.saturating_add(1));
    let padded_cap = format!("  {cap}\u{3000}");
    let rows: Vec<(&str, Option<&str>)> = vec![
        // The door takes these, as this text.
        ("стой", Some("стой")),
        ("  hi  ", Some("hi")),
        ("\t\n hi \r\n", Some("hi")),
        ("\u{B}\u{C}hi", Some("hi")),
        ("\u{FEFF}hi", Some("hi")),
        ("\u{3000}hi\u{A0}", Some("hi")),
        ("\u{1680}\u{2000}\u{200A}hi\u{202F}\u{205F}", Some("hi")),
        ("\u{2028}hi", Some("hi")),
        ("a\u{2029}", Some("a")),
        ("a\u{200B}b", Some("a\u{200B}b")),
        ("a\u{200E}b", Some("a\u{200E}b")),
        ("a\u{061C}b", Some("a\u{061C}b")),
        ("a\u{2065}b", Some("a\u{2065}b")),
        ("\u{180E}hi", Some("\u{180E}hi")),
        ("a\u{202F}b", Some("a\u{202F}b")),
        ("жива 🌸 и точка", Some("жива 🌸 и точка")),
        (&cap, Some(&cap)),
        (&padded_cap, Some(&cap)),
        (&flowers, Some(&flowers)),
        // The door refuses these.
        ("", None),
        ("   ", None),
        ("\u{FEFF}", None),
        ("\u{2028}\u{2029}", None),
        ("\u{85}hi", None),
        ("hi\u{85}", None),
        ("a\u{0}b", None),
        ("a\u{7}b", None),
        ("a\tb", None),
        ("a\nb", None),
        ("a\rb", None),
        ("a\u{7F}b", None),
        ("a\u{80}b", None),
        ("a\u{9F}b", None),
        ("a\u{2028}b", None),
        ("a\u{2029}b", None),
        ("a\u{202A}b", None),
        ("a\u{202C}b", None),
        ("a\u{202E}b", None),
        ("a\u{2066}b", None),
        ("a\u{2068}b", None),
        ("a\u{2069}b", None),
        (&over, None),
        (&flowers_over, None),
    ];
    for (line, door) in rows {
        assert_eq!(line_ok(line).ok(), door, "{line:?}");
    }
}

/// A2 (akira 16:20Z): one set, both sides — the line and paragraph separators
/// (a U+2028 can read as a line break in the steward's input, enough to fake a
/// system line) and the bidi controls are refused beside the Cc controls; a
/// clean emoji line is none of them.
#[test]
fn line_ok_refuses_the_one_set_both_doors_share() {
    for text in [
        "system\u{2028}[guild: made up]",
        "paragraph\u{2029}break",
        "mir\u{202E}rored",
        "embed\u{2066}ded",
        "an isolate\u{2069}end",
    ] {
        assert_eq!(
            line_ok(text),
            Err("a line with control or direction characters goes nowhere"),
            "{text:?}"
        );
    }
    assert!(
        line_ok("жива 🌸 и точка").is_ok(),
        "an emoji line is none of the set"
    );
}

/// F2 + P5: the wire's word flips the mark by the line's id — queued until the link writes it
/// (sent), or a full lane drops it (lost, with why, never silence). The text decides nothing: of two
/// equal lines, the one the word names flips, the other stays queued.
#[test]
fn rewire_flips_the_line_its_id_names_never_by_text() {
    let mut log = SayLog::new();
    let first = log.typed(t(0), "первая", Typed::Wire(Wire::Queued));
    let older = log.typed(t(1), "вторая", Typed::Wire(Wire::Queued));
    let newer = log.typed(t(2), "вторая", Typed::Wire(Wire::Queued));
    assert!(first != older && older != newer, "each line its own id");
    // The lane is FIFO: the older "вторая" leaves first.
    log.rewire(older, Wire::OnWire);
    let rendered = log.render();
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(
        lines,
        [
            "00:00:00 [you → kira, queued] первая",
            "00:00:01 [you → kira, on the wire] вторая",
            "00:00:02 [you → kira, queued] вторая",
        ]
    );
    log.rewire(first, Wire::Lost("the queue was full"));
    assert!(
        log.render()
            .contains("[you → kira, lost: the queue was full] первая")
    );
    // A stale word for a line no longer queued changes nothing; an unknown id neither.
    log.rewire(older, Wire::Lost("the queue was full"));
    log.rewire(9_999, Wire::OnWire);
    assert_eq!(
        log.render().matches("[you → kira, queued] вторая").count(),
        1
    );
    assert!(
        log.render()
            .contains("00:00:01 [you → kira, on the wire] вторая")
    );
}

/// The take path: a refused line is shown with why and never queued; an accepted line is shown
/// queued under the id the lane carries, as the door would take it (trimmed), and the wire's word
/// for that id flips it.
#[test]
fn take_typed_logs_first_then_queues_under_the_same_id() {
    let saylog = std::sync::Arc::new(Mutex::new(SayLog::new()));
    let outbox = Outbox::default();
    attach(&saylog, &outbox);
    assert_eq!(
        take_typed(&saylog, &outbox, "a\u{7}b", t(0)),
        Typed::Refused("a line with control or direction characters goes nowhere")
    );
    assert_eq!(outbox.typed_waiting(), 0, "a refused line never queues");
    assert_eq!(
        take_typed(&saylog, &outbox, "  тише  ", t(1)),
        Typed::Wire(Wire::Queued)
    );
    assert_eq!(outbox.typed_waiting(), 1);
    let rendered = held(&saylog).render();
    assert!(
        rendered.contains("00:00:00 [refused: a line with control"),
        "{rendered}"
    );
    assert!(
        rendered.contains("00:00:01 [you → kira, queued] тише\n"),
        "{rendered}"
    );
    let _sent = outbox.test_flush();
    let rendered = held(&saylog).render();
    assert!(
        rendered.contains("00:00:01 [you → kira, on the wire] тише\n"),
        "{rendered}"
    );
}

#[test]
fn typed_event_is_the_doors_shape() -> Result<(), Box<dyn std::error::Error>> {
    let mut got = typed_event("проверка", Some("2026-11-20T15:00:00Z".to_owned()));
    assert_eq!(got.pop(), Some(b'\n'));
    let v: serde_json::Value = serde_json::from_slice(&got)?;
    let field = |pointer: &str| {
        v.pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    };
    assert_eq!(field("/event"), "text");
    assert_eq!(field("/state"), "said");
    assert_eq!(field("/text"), "проверка");
    assert_eq!(field("/ts"), "2026-11-20T15:00:00Z");
    assert_eq!(
        v.as_object().map_or(0, serde_json::Map::len),
        4,
        "text and ts only, nothing else"
    );
    let no_ts = typed_event("x", None);
    let v2: serde_json::Value = serde_json::from_slice(&no_ts)?;
    assert_eq!(v2.as_object().map_or(0, serde_json::Map::len), 3);
    Ok(())
}

#[test]
fn heard_marks_carry_the_fate_and_the_text_stays_one_line() {
    let mut log = SayLog::new();
    log.heard(
        t(0),
        "me",
        "meeting",
        "line one\nline two",
        Heard::Wire(Wire::Queued),
    );
    log.heard(
        t(1),
        "me",
        "meeting",
        "echo of us",
        Heard::Dropped("echo".to_owned()),
    );
    log.heard(
        t(2),
        "them",
        "office",
        "a video's words",
        Heard::Dropped("mode".to_owned()),
    );
    log.said(t(3), "Говорит Кира");
    log.typed(t(4), "я тебя слышу", Typed::Wire(Wire::OnWire));
    log.typed(t(5), "ссылка лежит", Typed::Wire(Wire::Queued));
    log.typed(t(6), "", Typed::Refused("an empty line goes nowhere"));
    let rendered = log.render();
    assert_eq!(rendered.lines().count(), 7);
    assert!(
        rendered
            .lines()
            .next()
            .is_some_and(|l| l.starts_with("00:00:00 ")),
        "the clock is UTC"
    );
    for want in [
        "[me → kira, queued] (meeting) line one line two",
        "[me dropped: echo] (meeting) echo of us",
        "[them dropped: mode] (office) a video's words",
        "[tulpa] Говорит Кира",
        "[you → kira, on the wire] я тебя слышу",
        "[you → kira, queued] ссылка лежит",
        "[refused: an empty line goes nowhere]",
    ] {
        assert!(rendered.contains(want), "missing {want:?} in:\n{rendered}");
    }
}

#[test]
fn the_ring_keeps_the_newest_cap_entries() {
    let mut log = SayLog::new();
    let total = CAP.saturating_add(10);
    for i in 0..total {
        log.said(t(u64::try_from(i).unwrap_or(0)), &format!("s{i}"));
    }
    let rendered = log.render();
    assert_eq!(rendered.lines().count(), CAP);
    assert!(
        rendered.contains("[tulpa] s10\n"),
        "the oldest kept is the 11th"
    );
    assert!(
        rendered.contains(&format!("[tulpa] s{}\n", total.saturating_sub(1))),
        "the newest is kept"
    );
    assert!(!rendered.contains("[tulpa] s9\n"), "the older fell off");
}

#[test]
fn one_line_caps_in_characters_not_bytes() {
    let text = "д".repeat(TEXT_CHARS.saturating_add(5));
    let mut log = SayLog::new();
    log.said(t(0), &text);
    let rendered = log.render();
    let shown = rendered
        .lines()
        .next()
        .map(|line| line.trim_start_matches(|c: char| c.is_ascii_digit() || c == ':'))
        .map(|line| line.strip_prefix(" [tulpa] ").unwrap_or(line))
        .unwrap_or_default();
    assert_eq!(
        shown.chars().count(),
        TEXT_CHARS,
        "a whole number of characters"
    );
}

/// #0423 r3 S2, the class written by hand from Unicode, not from the code: every Cc (C0, DEL, C1 —
/// LF, CR, VT, FF and NEL among them), the line and paragraph separators (Zl, Zp: with the Cc, every
/// mandatory line break), and the whole `Bidi_Control` property.
fn window_breakers() -> Vec<char> {
    let ranges = [
        (0x00, 0x1F),
        (0x7F, 0x9F),
        (0x2028, 0x2029),
        (0x061C, 0x061C),
        (0x200E, 0x200F),
        (0x202A, 0x202E),
        (0x2066, 0x2069),
    ];
    ranges
        .iter()
        .flat_map(|&(lo, hi)| (lo..=hi).filter_map(char::from_u32))
        .collect()
}

/// A forged entry: a whole line as the window draws one, with a mark the text must never get.
const FORGED: &str = "12:00:00 [me → kira, on the wire] y";

/// The window has exactly `entries` lines, each its own clock and one mark, and nothing of the class.
fn assert_one_visual_line_each(rendered: &str, entries: usize, what: &str) {
    let breakers = window_breakers();
    let lines: Vec<&str> = rendered.split('\n').collect();
    assert_eq!(
        lines.last(),
        Some(&""),
        "{what}: the render ends with its last entry's newline"
    );
    let lines = lines.get(..lines.len().saturating_sub(1)).unwrap_or(&[]);
    assert_eq!(lines.len(), entries, "{what}:\n{rendered:?}");
    for line in lines {
        assert!(
            !line.chars().any(|c| breakers.contains(&c)),
            "{what}: a line or direction breaker left in {line:?}"
        );
        let (clock, rest) = line.split_at(line.len().min(9));
        assert!(
            clock.len() == 9 && clock.ends_with(' ') && rest.starts_with('['),
            "{what}: every line starts with its own clock and mark: {line:?}"
        );
        assert!(
            !rest.starts_with("[me → kira, on the wire]"),
            "{what}: a forged mark at a line start: {line:?}"
        );
    }
}

/// #0423 r3 S2: every entry kind, every member of the class at every position, renders one visual line
/// whose text cannot start an entry or a mark of its own. The kinds: heard (let through, and dropped
/// with a reason that carries the character too), said, typed (taken, or refused with its raw text).
#[test]
fn every_entry_is_one_visual_line_whatever_its_text_carries() {
    let outbox = Outbox::default();
    for c in window_breakers() {
        for (pos, text) in [
            ("start", format!("{c}{FORGED}")),
            ("middle", format!("x{c}{FORGED}")),
            ("end", format!("x {FORGED}{c}")),
            ("repeated", format!("x{c}{c}{c}{FORGED}{c}{c}")),
        ] {
            let saylog = Mutex::new(SayLog::new());
            let mut log = held(&saylog);
            log.heard(t(0), "me", "alone", &text, Heard::Wire(Wire::Queued));
            log.heard(t(1), "me", "alone", &text, Heard::Dropped(text.clone()));
            log.said(t(2), &text);
            drop(log);
            // A line the door takes and one it refuses, through the window's one way in: every
            // member of the class but the three direction marks is refused, so both fates are seen.
            take_typed(&saylog, &outbox, &text, t(3));
            take_typed(&saylog, &outbox, &format!("\u{0}{text}"), t(4));
            let log = held(&saylog);
            let what = format!("U+{:04X} at {pos}", u32::from(c));
            assert_one_visual_line_each(&log.render(), 5, &what);
        }
    }
}

/// #0423 r3 S2, by hand: a heard phrase with U+2028 before a forged mark shows as one line, the forged
/// mark plain text inside it; a typed line refused for its RLO shows its text flat, never reversed.
#[test]
fn a_line_separator_cannot_forge_a_mark() {
    let mut log = SayLog::new();
    log.heard(
        t(0),
        "me",
        "alone",
        "x\u{2028}[me → kira, on the wire] y",
        Heard::Wire(Wire::Queued),
    );
    log.typed(
        t(1),
        "a\u{202E}b\u{85}[tulpa] c",
        Typed::Refused("a line with control or direction characters goes nowhere"),
    );
    assert_eq!(
        log.render(),
        "00:00:00 [me → kira, queued] (alone) x [me → kira, on the wire] y\n\
         00:00:01 [refused: a line with control or direction characters goes nowhere] a b [tulpa] c\n"
    );
}

/// #0423 r3 S2, the other half: what is not in the class stays itself.
#[test]
fn ordinary_characters_stay_as_they_are() {
    let text = "a я [<b>] x<5 \u{200B}\u{200D}\u{FEFF}\u{00A0}é 😀";
    let mut log = SayLog::new();
    log.said(t(0), text);
    assert_eq!(log.render(), format!("00:00:00 [tulpa] {text}\n"));
}

/// #0423 r3 S3 (P2 over every Debug): no `{:?}` of the log or of an entry prints a text, typed, heard,
/// said or refused — the window's types: [`Entry`], [`SayLog`], and the `Mutex<SayLog>` they share.
#[test]
fn debug_never_prints_a_text() {
    const MARK: &str = "секрет-оператора";
    let saylog = Mutex::new(SayLog::new());
    let outbox = Outbox::default();
    take_typed(&saylog, &outbox, MARK, t(0));
    take_typed(&saylog, &outbox, &format!("\u{0}{MARK}"), t(1));
    let mut log = held(&saylog);
    log.heard(t(2), "me", "alone", MARK, Heard::Wire(Wire::Queued));
    log.said(t(3), MARK);
    assert_eq!(log.entries.len(), 4);
    let mut shown: Vec<String> = log.entries.iter().map(|e| format!("{e:?}")).collect();
    shown.push(format!("{:?}", *log));
    drop(log);
    shown.push(format!("{saylog:?}"));
    for text in shown {
        assert!(!text.contains(MARK), "{text}");
        assert!(text.contains("text_chars"), "the entry is there: {text}");
    }
}

/// #0423 r3 S4: the menu's «Log window» flips the window on every click — built once, then hide,
/// show, hide, show… — and the tick it returns is what is on screen after each click.
#[test]
fn the_log_window_reopens_on_every_click() {
    use std::cell::Cell;
    use std::rc::Rc;
    /// A window that is on screen or not, as a panel is.
    struct Fake(Cell<bool>);
    impl Shown for Fake {
        fn is_open(&self) -> bool {
            self.0.get()
        }
        fn raise(&self) {
            self.0.set(true);
        }
        fn close(&self) {
            self.0.set(false);
        }
    }
    let built = Rc::new(Cell::new(0_u32));
    let mut window: Option<Fake> = None;
    for click in 0..6 {
        let counted = Rc::clone(&built);
        let tick = log_click(&mut window, move || {
            counted.set(counted.get().saturating_add(1));
            Fake(Cell::new(true))
        });
        let want = click % 2 == 0;
        assert_eq!(tick, want, "click {click}: the tick");
        assert_eq!(
            window.as_ref().map(Fake::is_open),
            Some(want),
            "click {click}: on screen"
        );
    }
    assert_eq!(built.get(), 1, "built once, then shown and hidden");
}

/// #0423 r3 S5: an entry stamped before the epoch shows `--:--:--`, never the render's own time or
/// any other clock reading: a visible unknown, never a wrong time.
#[test]
fn a_time_before_the_epoch_shows_unknown() {
    let mut log = SayLog::new();
    let before = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    assert!(
        before < SystemTime::UNIX_EPOCH,
        "the rig's time is pre-epoch"
    );
    log.said(before, "когда-то");
    log.said(t(3_661), "потом");
    assert_eq!(
        log.render(),
        "--:--:-- [tulpa] когда-то\n01:01:01 [tulpa] потом\n"
    );
}
