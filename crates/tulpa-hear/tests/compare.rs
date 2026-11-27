//! `--compare` on hand-made fixtures (`tests/data/compare/`) and inline logs: exact counts, one-to-one pairing by
//! span overlap and by emit time (nearest first, tolerance edges), never across sides, `0 0` spans never paired,
//! the live-clock N/A switch, and no phrase text or word anywhere in the output.

#![expect(
    clippy::indexing_slicing,
    reason = "fixed criteria and unpaired lists; a short one fails the test"
)]

use std::fmt::Write as _;
use std::process::Command;

use tulpa_hear::compare::{EmitSource, Pairing, Report, Verdict, compare};
use tulpa_hear::hear::Side;

const LIVE: &str = include_str!("data/compare/ears.log");
const SHADOW: &str = include_str!("data/compare/lines.log");
const FLOW: &str = include_str!("data/compare/flow.log");

/// 2026-11-22T10:00:00Z, the shadow's start in the fixtures.
const S: u64 = 1_795_341_600_000;

fn data(name: &str) -> String {
    format!("{}/tests/data/compare/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Every word of every phrase in the fixtures (the text before the TAB, and the flow texts).
fn fixture_words() -> Vec<String> {
    let mut out = Vec::new();
    for text in [LIVE, SHADOW, FLOW] {
        for line in text.lines() {
            let line = line
                .split('\t')
                .find(|f| f.contains("TULPA-EARS-LINE"))
                .unwrap_or(line);
            let line = line.split("\"text\":").nth(1).unwrap_or(line);
            for w in line.split(|c: char| !c.is_alphabetic()) {
                let w = w.to_lowercase();
                if w.len() >= 4 && !out.contains(&w) {
                    out.push(w);
                }
            }
        }
    }
    out
}

/// Words the report itself uses, which the fixtures share only as markup (`TULPA-EARS-LINE`, `SHADOW`, JSON keys,
/// `voice`, `intonation`): the phrase words are what must never show.
const MARKUP: &[&str] = &[
    "tulpa",
    "them",
    "ears",
    "line",
    "shadow",
    "start",
    "stop",
    "sigterm",
    "voice",
    "intonation",
    "falling",
    "event",
    "frame",
    "state",
    "side",
    "mode",
    "office",
    "text",
    "hands",
    "done",
    "speaking",
    "capture",
    "started",
    "device",
    "switched",
    "partial",
    "without",
    "marker",
    "garbled",
    "json",
];

fn assert_no_text(out: &str) {
    let lower = out.to_lowercase();
    let words = fixture_words();
    assert!(
        words.iter().any(|w| w == "garden"),
        "the check sees phrase words: {words:?}"
    );
    for w in words.iter().filter(|w| !MARKUP.contains(&w.as_str())) {
        assert!(
            !lower.contains(w.as_str()),
            "the report shows {w:?}:\n{out}"
        );
    }
}

#[test]
fn fixture_counts_are_exact_without_flow() {
    let r = compare(LIVE, SHADOW, None, Pairing::Span);
    assert_eq!(r.window, Some((S, S + 70_000)));
    assert_eq!(r.emit_source, EmitSource::Clock);
    assert_eq!(r.live, [4, 2], "two lines outside the window");
    assert_eq!(r.shadow, [4, 2]);
    assert_eq!(r.live_no_span, [1, 0]);
    assert_eq!(r.shadow_no_span, [0, 0]);
    assert_eq!(r.paired, [2, 2]);
    assert_eq!(r.coverage_live.map(|c| (c * 10.0).round()), Some(800.0));
    assert_eq!(r.coverage_shadow.map(|c| (c * 10.0).round()), Some(667.0));
    assert_eq!((r.delta_t0, r.delta_t1), (Some(50), Some(50)));
    assert_eq!(r.latency_live, Some((500, 500)));
    assert_eq!(r.latency_shadow, Some((500, 800)));
    assert_eq!(r.voice_live.map(|v| (v * 10.0).round()), Some(167.0));
    assert_eq!(r.voice_shadow.map(|v| (v * 10.0).round()), Some(333.0));
    assert_eq!(r.wer, Some((0.0, 40.0)));
    assert_eq!(
        r.unpaired,
        vec![
            (S + 12_500, Side::Me, tulpa_hear::compare::Path::Shadow),
            (S + 50_000, Side::Me, tulpa_hear::compare::Path::Live),
            (S + 60_000, Side::Me, tulpa_hear::compare::Path::Shadow),
        ]
    );
    assert_eq!((r.starts, r.stops, r.crashes), (1, 1, 0));
    assert_eq!((r.live_unreadable, r.shadow_unreadable), (1, 0));
    let verdicts: Vec<Verdict> = r.criteria().iter().map(|c| c.verdict).collect();
    assert_eq!(
        verdicts,
        [
            Verdict::Fail,
            Verdict::Pass,
            Verdict::Pass,
            Verdict::Fail,
            Verdict::Pass,
            Verdict::Pass
        ]
    );
    assert!(!r.pass());
}

#[test]
fn flow_gives_live_emit_times_to_the_ms() {
    let r = compare(LIVE, SHADOW, Some(FLOW), Pairing::Span);
    assert_eq!(r.emit_source, EmitSource::Flow { missing: 1 });
    // 300 (flow) 800 (the nearer of two flow records; the "in" one ignored) 200, 300 (tag and intonation aside),
    // 500 (not in flow: the clock's).
    assert_eq!(r.latency_live, Some((300, 800)));
    assert_eq!(r.live, [4, 2]);
    let lat = &r.criteria()[2];
    assert_eq!(lat.verdict, Verdict::Fail, "shadow p50 500 > live p50 300");
    let out = r.render();
    assert!(out.contains("live emit time: flow.log ts"), "{out}");
    assert!(out.contains("HH:MM:SS + 500 ms: 1"), "{out}");
}

#[test]
fn the_report_is_counts_only() {
    for flow in [None, Some(FLOW)] {
        let out = compare(LIVE, SHADOW, flow, Pairing::Span).render();
        assert_no_text(&out);
        assert!(out.contains(
            "note: live-path word times are VAD-compressed (known defect); spans (t0/t1) are not"
        ));
        assert!(
            out.contains("window: 2026-11-22T10:00:00Z .. 2026-11-22T10:01:10Z (70 s)"),
            "{out}"
        );
        assert!(
            out.contains("\n  10:00:12 me shadow\n  10:00:50 me live\n  10:01:00 me shadow\n"),
            "{out}"
        );
        assert!(out.contains("result: FAIL"), "{out}");
    }
}

/// The binary: the same report on stdout, exit 1 on a failing criterion, 2 on a missing file.
#[test]
fn compare_binary_prints_the_report_and_exits_by_it() {
    let bin = env!("CARGO_BIN_EXE_tulpa-hear");
    let out = Command::new(bin)
        .args([
            "--compare",
            &data("ears.log"),
            &data("lines.log"),
            "--flow",
            &data("flow.log"),
            "--pair",
            "span",
        ])
        .output();
    let Ok(out) = out else {
        assert!(out.is_ok());
        return;
    };
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text,
        compare(LIVE, SHADOW, Some(FLOW), Pairing::Span).render()
    );
    assert_no_text(&text);
    assert_no_text(&String::from_utf8_lossy(&out.stderr));
    let missing = Command::new(bin)
        .args(["--compare", &data("nope.log"), &data("lines.log")])
        .output()
        .ok()
        .and_then(|o| o.status.code());
    assert_eq!(missing, Some(2));
}

/// A line emitted at `10:00:<sec>` (live: plus 500 ms).
fn line(t0: u64, t1: u64, sec: u64, side: &str, text: &str) -> String {
    format!("TULPA-EARS-LINE {t0} {t1} 10:00:{sec:02} [{side}] {text}")
}

/// One live phrase overlapped by two shadow phrases: one pair (the larger overlap), the other shadow phrase
/// unpaired; a phrase on the other side, however it overlaps, never pairs.
#[test]
fn pairing_is_one_to_one_by_largest_overlap() {
    let live = format!("{}\n", line(S + 1_000, S + 5_000, 5, "me", "one two three"));
    let shadow = format!(
        "{S}\tSHADOW start 1 0.1.0\n{}\t{}\n{}\t{}\n{}\t{}\n{}\tSHADOW stop end\n",
        S + 3_000,
        line(S + 1_200, S + 2_500, 3, "me", "one"),
        S + 5_000,
        line(S + 2_600, S + 5_100, 5, "me", "two three"),
        S + 5_100,
        line(S + 1_000, S + 5_000, 5, "them", "one two three"),
        S + 9_000,
    );
    let r = compare(&live, &shadow, None, Pairing::Span);
    assert_eq!(r.paired, [1, 0]);
    assert_eq!(
        (r.delta_t0, r.delta_t1),
        (Some(1_600), Some(100)),
        "paired with the 2.5 s overlap"
    );
    assert_eq!(r.unpaired.len(), 2, "{:?}", r.unpaired);
    assert_eq!(
        r.unpaired[0],
        (S + 1_000, Side::Them, tulpa_hear::compare::Path::Shadow)
    );
    assert_eq!(
        r.unpaired[1],
        (S + 1_200, Side::Me, tulpa_hear::compare::Path::Shadow)
    );
}

/// `0 0` is no span: counted, never paired, not in the coverage denominators.
#[test]
fn zero_spans_are_counted_but_never_paired() {
    let live = format!(
        "{}\n{}\n",
        line(0, 0, 0, "me", "same words"),
        line(S + 1_000, S + 2_000, 2, "me", "other")
    );
    let shadow = format!(
        "{S}\tSHADOW start 1 0.1.0\n{}\t{}\n{}\t{}\n",
        S + 1_000,
        line(0, 0, 1, "me", "same words"),
        S + 2_500,
        line(S + 1_000, S + 2_000, 2, "me", "other"),
    );
    let r = compare(&live, &shadow, None, Pairing::Span);
    assert_eq!((r.live_no_span, r.shadow_no_span), ([1, 0], [1, 0]));
    assert_eq!(r.paired, [1, 0]);
    assert_eq!(
        (r.coverage_live, r.coverage_shadow),
        (Some(100.0), Some(100.0))
    );
    assert_eq!(r.unpaired, []);
}

/// A start with no stop before it (after the first) is a crash; so is a `stop error`. Both are restarts too.
#[test]
fn restarts_and_crashes_are_counted() {
    let shadow = format!(
        "{S}\tSHADOW start 1 0.1.0\n{}\tSHADOW start 2 0.1.0\n{}\tSHADOW stop error\n{}\tSHADOW start 3 0.1.0\n{}\tSHADOW stop SIGINT\n",
        S + 10,
        S + 20,
        S + 30,
        S + 40
    );
    let r: Report = compare("", &shadow, None, Pairing::Span);
    assert_eq!((r.starts, r.stops, r.crashes), (3, 2, 2));
    let c = &r.criteria()[5];
    assert_eq!(c.verdict, Verdict::Fail);
    assert_eq!(c.seen, "crashes 2, restarts 2");
}

/// A shadow log: the start at `S`, then `(emit ms, line)` records, then a clean stop 30 s in.
fn shadow_log(records: &[(u64, String)]) -> String {
    let mut body = String::new();
    for (at, l) in records {
        let _w = writeln!(body, "{at}\t{l}");
    }
    format!(
        "{S}\tSHADOW start 1 0.1.0\n{body}{}\tSHADOW stop end\n",
        S + 30_000
    )
}

/// A live phrase with no span emitted at 10:00:29.5: it pairs with nothing and only keeps the window open.
fn late() -> String {
    line(0, 0, 29, "them", "late")
}

/// Live spans 300 s behind the wall clock: emit pairing still pairs both sides at their 500 ms emit difference,
/// span pairing pairs nothing, and the span-based criteria are N/A while the rest pass.
#[test]
fn emit_pairing_ignores_live_spans_300_s_behind() {
    const OFF: u64 = 300_000;
    let live = format!(
        "{}\n{}\n",
        line(S + 1_000 - OFF, S + 4_000 - OFF, 5, "me", "one two three"),
        line(S + 10_000 - OFF, S + 12_000 - OFF, 13, "them", "four five"),
    );
    let shadow = shadow_log(&[
        (
            S + 5_000,
            line(S + 1_000, S + 4_000, 5, "me", "one two three"),
        ),
        (
            S + 13_000,
            line(S + 10_000, S + 12_000, 13, "them", "four five"),
        ),
    ]);
    let r = compare(&live, &shadow, None, Pairing::default());
    assert_eq!(r.paired, [1, 1]);
    assert_eq!(r.delta_emit, Some(500));
    assert_eq!(r.delta_t0, Some(OFF));
    assert_eq!(r.latency_live, Some((301_500, 301_500)));
    assert_eq!(r.latency_shadow, Some((1_000, 1_000)));
    assert!(r.unpaired.is_empty(), "{:?}", r.unpaired);
    assert!(r.live_off_clock());
    let verdicts: Vec<Verdict> = r.criteria().iter().map(|c| c.verdict).collect();
    assert_eq!(
        verdicts,
        [
            Verdict::Pass,
            Verdict::NotApplicable,
            Verdict::NotApplicable,
            Verdict::Pass,
            Verdict::Pass,
            Verdict::Pass
        ]
    );
    assert!(r.pass());
    let out = r.render();
    assert!(out.contains("pairing: emit times within 4000 ms"), "{out}");
    assert!(
        out.contains(
            "live clock (emit - t1) ms: live median 301500 p90 301500, shadow median 1000 p90 1000"
        ),
        "{out}"
    );
    assert!(
        out.contains("live spans off the wall clock: span-based criteria n/a"),
        "{out}"
    );
    assert!(out.contains("median |demit| ms         500"), "{out}");
    assert!(out.contains("N/A (live clock) median |dt0|"), "{out}");
    assert!(out.contains("result: PASS"), "{out}");

    let span = compare(&live, &shadow, None, Pairing::Span);
    assert_eq!(span.paired, [0, 0], "the overlap pairing finds nothing");
    assert!(!span.pass());
}

/// One live phrase, two shadow phrases within the tolerance: it pairs with the nearer emit; the other is unpaired.
/// Two live, two shadow: greedy by the smallest difference overall, one to one.
#[test]
fn emit_pairing_is_one_to_one_by_nearest_emit() {
    let live = format!(
        "{}\n{}\n",
        line(S + 1_000, S + 4_000, 5, "me", "one"),
        late()
    );
    let shadow = shadow_log(&[
        (S + 3_000, line(S + 1_000, S + 2_000, 3, "me", "one")),
        (S + 6_000, line(S + 3_000, S + 4_000, 6, "me", "one")),
    ]);
    let r = compare(&live, &shadow, None, Pairing::default());
    assert_eq!(r.paired, [1, 0]);
    assert_eq!(r.delta_emit, Some(500), "paired with the 6.0 s one");
    assert_eq!(
        r.unpaired,
        [(S + 3_000, Side::Me, tulpa_hear::compare::Path::Shadow)]
    );

    // Live at 5.5 and 8.5 s; shadow at 6.0 (0.5 / 2.5 s away) and 9.5 (4.0 / 1.0 s away): 5.5-6.0 and 8.5-9.5.
    let live = format!(
        "{}\n{}\n{}\n",
        line(S + 1_000, S + 4_000, 5, "me", "one"),
        line(S + 6_000, S + 8_000, 8, "me", "two"),
        late()
    );
    let shadow = shadow_log(&[
        (S + 6_000, line(S + 1_000, S + 4_000, 6, "me", "one")),
        (S + 9_500, line(S + 6_000, S + 8_000, 9, "me", "two")),
    ]);
    let r = compare(&live, &shadow, None, Pairing::default());
    assert_eq!(r.paired, [2, 0]);
    assert_eq!(r.delta_t0, Some(0), "each with its own span");
    assert_eq!(r.wer, Some((0.0, 0.0)));
    assert!(r.unpaired.is_empty(), "{:?}", r.unpaired);
}

/// `|Δemit|` equal to the tolerance pairs; 1 ms more does not; `--emit-tol-ms` moves the edge.
#[test]
fn emit_tolerance_edges() {
    let live = format!(
        "{}\n{}\n",
        line(S + 1_000, S + 4_000, 5, "me", "one"),
        late()
    );
    let at = |emit: u64| shadow_log(&[(emit, line(S + 1_000, S + 4_000, 9, "me", "one"))]);
    let paired =
        |emit: u64, tol_ms: u64| compare(&live, &at(emit), None, Pairing::Emit { tol_ms }).paired;
    assert_eq!(paired(S + 9_500, 4_000), [1, 0], "4000 ms after: pairs");
    assert_eq!(paired(S + 1_500, 4_000), [1, 0], "4000 ms before: pairs");
    assert_eq!(paired(S + 9_501, 4_000), [0, 0], "4001 ms after: not");
    assert_eq!(paired(S + 1_499, 4_000), [0, 0], "4001 ms before: not");
    assert_eq!(paired(S + 9_500, 3_999), [0, 0]);
    assert_eq!(
        paired(S + 5_500, 0),
        [1, 0],
        "zero tolerance: the same ms only"
    );
    assert_eq!(paired(S + 5_501, 0), [0, 0]);
}

/// The live median emit − t1 at exactly 30 s keeps the span criteria; 1 ms more makes them N/A.
#[test]
fn live_clock_switch_at_30_s() {
    let shadow = shadow_log(&[(S + 5_000, line(S + 1_000, S + 4_000, 5, "me", "one"))]);
    let judge = |lag: u64| {
        // Live emit is 10:00:05.5.
        let t1 = S + 5_500 - lag;
        let live = format!("{}\n", line(t1 - 1_000, t1, 5, "me", "one"));
        let r = compare(&live, &shadow, None, Pairing::default());
        assert_eq!(r.paired, [1, 0]);
        let v: Vec<Verdict> = r.criteria().iter().map(|c| c.verdict).collect();
        (r.latency_live, v[1], v[2], r.pass(), r.render())
    };
    let (lat, dt, latency, pass, out) = judge(30_000);
    assert_eq!(lat, Some((30_000, 30_000)));
    assert_eq!((dt, latency, pass), (Verdict::Fail, Verdict::Pass, false));
    assert!(!out.contains("off the wall clock"), "{out}");
    assert!(!out.contains("N/A"), "{out}");
    let (lat, dt, latency, pass, out) = judge(30_001);
    assert_eq!(lat, Some((30_001, 30_001)));
    assert_eq!(
        (dt, latency, pass),
        (Verdict::NotApplicable, Verdict::NotApplicable, true)
    );
    assert!(out.contains("live spans off the wall clock"), "{out}");
    assert_eq!(out.matches("N/A (live clock)").count(), 2, "{out}");
}

/// A live `me` phrase and a shadow `them` phrase with the same span and 0.5 s apart: paired by neither mode.
fn assert_sides_never_cross(how: Pairing) {
    let live = format!(
        "{}\n{}\n",
        line(S + 1_000, S + 5_000, 5, "me", "one two"),
        late()
    );
    let shadow = shadow_log(&[(S + 5_000, line(S + 1_000, S + 5_000, 5, "them", "one two"))]);
    let r = compare(&live, &shadow, None, how);
    assert_eq!(r.paired, [0, 0], "{how:?}");
    assert_eq!(r.unpaired.len(), 2, "{how:?}: {:?}", r.unpaired);
    assert_eq!(r.delta_t0, None, "{how:?}");
}

#[test]
fn sides_never_cross_by_span() {
    assert_sides_never_cross(Pairing::Span);
}

#[test]
fn sides_never_cross_by_emit() {
    assert_sides_never_cross(Pairing::default());
}

/// The binary's pairing options: unknown values, repeats, and a tolerance with span pairing are usage errors.
#[test]
fn compare_binary_pair_options() {
    let bin = env!("CARGO_BIN_EXE_tulpa-hear");
    let code = |extra: &[&str]| {
        Command::new(bin)
            .args(["--compare", &data("ears.log"), &data("lines.log")])
            .args(extra)
            .output()
            .ok()
            .and_then(|o| o.status.code())
    };
    for bad in [
        &["--pair", "time"][..],
        &["--pair", "span", "--emit-tol-ms", "100"],
        &["--emit-tol-ms", "-1"],
        &["--pair", "emit", "--pair", "emit"],
        &["--pair"],
    ] {
        assert_eq!(code(bad), Some(2), "{bad:?}");
    }
    for good in [
        &[][..],
        &["--pair", "emit", "--emit-tol-ms", "100"],
        &["--emit-tol-ms", "100", "--flow", &data("flow.log")],
    ] {
        assert!(matches!(code(good), Some(0 | 1)), "{good:?}");
    }
}
