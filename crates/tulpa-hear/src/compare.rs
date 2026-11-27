//! `tulpa-hear --compare`: the shadow rehearsal's verdict, in counts only. Pure; every platform.
//!
//! In: the live path's side log (the tray's `ears.log`, only its `TULPA-EARS-LINE ` lines), the shadow's
//! `lines.log` (`<emit_ms>\t<record>`, see [`shadow`](crate::shadow)), and optionally the tray's `flow.log` (JSON
//! lines), which gives a live line's emit time to the ms; without it a live line's emit time is its `HH:MM:SS` plus
//! 500 ms.
//!
//! The window is the shadow's first start to the earlier of the two last records; only phrases emitted in it
//! count. Pairing is per side and one to one ([`Pairing`]): by default a live and a shadow phrase pair when their
//! emit times are within a tolerance, greedily by the smallest difference — the live path's `t0`/`t1` are stamped by
//! its sample count, and a capture that drops samples puts them minutes behind the wall clock; with
//! [`Pairing::Span`] they pair when their `[t0, t1]` overlap, greedily by the largest overlap. Either way a phrase
//! with `t0 = t1 = 0` has no span and pairs with nothing. Words for the word error rate are lowercased, without
//! `<pause …>`/`<voice …>`/`<no speech …>` tags, `[intonation: …]`, the speaker tag and punctuation.
//!
//! The live clock is the median of (live emit − live `t1`); over 30 s the live spans are off the wall clock, and the
//! two span-based criteria (`|Δt0|`/`|Δt1|`, emit latency) are N/A: they neither pass nor fail.
//!
//! Out ([`Report::render`]): numbers, `HH:MM:SS side path` for the unpaired, and a PASS/FAIL/N/A per switch
//! criterion — never a phrase's text or a word of it.

use std::fmt::Write as _;

use crate::assemble::UNREAD_TAGS;
use crate::hear::Side;

/// What a `TULPA-EARS-LINE` starts with.
const MARKER: &str = "TULPA-EARS-LINE ";
/// Milliseconds in a day.
const DAY_MS: u64 = 86_400_000;
/// A live line's emit time without `flow.log`: its `HH:MM:SS` plus this.
const CLOCK_HALF_MS: u64 = 500;
/// A flow record further than this from a line's `HH:MM:SS` is not its emit.
const FLOW_NEAR_MS: u64 = 60_000;

/// The switch criteria's thresholds.
const COVERAGE_MIN_PCT: f64 = 95.0;
const DELTA_MAX_MS: u64 = 250;
const P90_SLACK_MS: i64 = 500;
const VOICE_POINTS_MAX: f64 = 5.0;
const WER_MAX_PCT: f64 = 10.0;
/// A live clock (median emit − `t1`) above this: the live spans are off the wall clock.
const LIVE_CLOCK_MAX_MS: i64 = 30_000;

/// `--emit-tol-ms`'s default.
pub const DEFAULT_EMIT_TOL_MS: u64 = 4_000;

/// How a live and a shadow phrase (same side, both with a span) are paired, one to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pairing {
    /// Emit times within `tol_ms` (inclusive), greedily by the smallest difference.
    Emit {
        /// The largest `|Δemit|` that pairs, ms.
        tol_ms: u64,
    },
    /// `[t0, t1]` overlap (touching counts), greedily by the largest overlap.
    Span,
}

impl Default for Pairing {
    fn default() -> Self {
        Self::Emit {
            tol_ms: DEFAULT_EMIT_TOL_MS,
        }
    }
}

/// Which path a phrase came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Path {
    /// The live capture script, through the tray.
    Live,
    /// `tulpa-hear --shadow`.
    Shadow,
}

impl Path {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Shadow => "shadow",
        }
    }
}

/// One phrase, its words kept only for the error rate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Phrase {
    side: Side,
    /// `(t0, t1)`, `None` for `0 0`.
    span: Option<(u64, u64)>,
    emit_ms: u64,
    words: Vec<String>,
    /// Only `<voice …>` / `<no speech …>`: heard, not read.
    voice_only: bool,
    /// A live phrase `flow.log` was given for but had no record of (its emit time is the clock's).
    flow_missing: bool,
}

/// A `TULPA-EARS-LINE`'s fields, after the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Line<'a> {
    t0: u64,
    t1: u64,
    /// Seconds of the UTC day, when the line has its `HH:MM:SS`.
    clock: Option<u64>,
    side: Side,
    /// The text, without the speaker tag; up to the TAB.
    text: &'a str,
}

/// `<t0> <t1> [HH:MM:SS] [side] [tag] text…<TAB>words`, after the marker.
fn parse_line(rest: &str) -> Option<Line<'_>> {
    let rest = rest.trim_end_matches(['\n', '\r']);
    let rest = rest.split_once('\t').map_or(rest, |(l, _)| l);
    let (t0, rest) = rest.trim_start().split_once(' ')?;
    let (t1, rest) = rest.trim_start().split_once(' ')?;
    let (t0, t1) = (t0.parse().ok()?, t1.parse().ok()?);
    let mut rest = rest.trim_start();
    let mut clock = None;
    if let Some((c, after)) = rest.split_once(' ')
        && let Some(secs) = parse_clock(c)
    {
        clock = Some(secs);
        rest = after.trim_start();
    }
    let (side, rest) = match rest.strip_prefix("[me]") {
        Some(r) => (Side::Me, r),
        None => (Side::Them, rest.strip_prefix("[them]")?),
    };
    let mut text = rest.trim_start();
    // A speaker tag right after the side; `[intonation: …]` is not one.
    if text.starts_with('[')
        && !text.starts_with("[intonation:")
        && let Some(end) = text.find(']')
    {
        text = text.get(end.saturating_add(1)..).unwrap_or("").trim_start();
    }
    Some(Line {
        t0,
        t1,
        clock,
        side,
        text,
    })
}

/// `HH:MM:SS` to seconds of the day.
fn parse_clock(c: &str) -> Option<u64> {
    let mut it = c.split(':');
    let (h, m, s) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || [h, m, s].iter().any(|p| p.len() != 2) {
        return None;
    }
    let (h, m, s): (u64, u64, u64) = (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?);
    (h < 24 && m < 60 && s < 60).then(|| {
        h.saturating_mul(3600)
            .saturating_add(m.saturating_mul(60))
            .saturating_add(s)
    })
}

/// The words of a text, and whether it held a placeholder of unread audio (`<voice …>`, `<no speech …>`): the
/// capture script on the live side cannot tell no speech from unread voice, so the two count as one.
fn words(text: &str) -> (Vec<String>, bool) {
    let mut kept = String::with_capacity(text.len());
    let mut voice = false;
    let mut rest = text;
    while !rest.is_empty() {
        let cut = rest.find(['<', '[']).unwrap_or(rest.len());
        kept.push_str(rest.get(..cut).unwrap_or(""));
        rest = rest.get(cut..).unwrap_or("");
        let tag = |open: &str, close: char, rest: &str| -> Option<usize> {
            rest.starts_with(open)
                .then(|| rest.find(close))
                .flatten()
                .map(|i| i.saturating_add(1))
        };
        let voice_tag = UNREAD_TAGS.iter().find_map(|open| tag(open, '>', rest));
        voice |= voice_tag.is_some();
        let skip = voice_tag
            .or_else(|| tag("<pause", '>', rest))
            .or_else(|| tag("[intonation:", ']', rest));
        if let Some(n) = skip {
            kept.push(' ');
            rest = rest.get(n..).unwrap_or("");
        } else {
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                kept.push(c);
            }
            rest = chars.as_str();
        }
    }
    let words = kept
        .to_lowercase()
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect();
    (words, voice)
}

/// Word-level Levenshtein distance.
fn distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, wa) in a.iter().enumerate() {
        let mut row = Vec::with_capacity(prev.len());
        row.push(i.saturating_add(1));
        for (j, wb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(usize::MAX);
            let sub = sub.saturating_add(usize::from(wa != wb));
            let del = prev.get(j.saturating_add(1)).copied().unwrap_or(usize::MAX);
            let ins = row.get(j).copied().unwrap_or(usize::MAX);
            row.push(sub.min(del.saturating_add(1)).min(ins.saturating_add(1)));
        }
        prev = row;
    }
    prev.last().copied().unwrap_or(0)
}

/// A pair's word error rate, %: the distance over the live phrase's words (at least one). Two unread-only
/// phrases (`<voice>` / `<no speech>`) agree.
fn wer(live: &Phrase, shadow: &Phrase) -> f64 {
    if live.voice_only && shadow.voice_only {
        return 0.0;
    }
    let d = distance(&live.words, &shadow.words);
    pct(d, live.words.len().max(1))
}

/// `a / b` in %.
#[expect(
    clippy::cast_precision_loss,
    reason = "phrase and word counts, far below 2^52"
)]
fn pct(a: usize, b: usize) -> f64 {
    a as f64 * 100.0 / b as f64
}

/// The nearest-rank percentile `p` (0..=100) of `v`, sorted here; `None` when empty.
pub(crate) fn rank<T: Copy + PartialOrd>(v: &mut [T], p: usize) -> Option<T> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let i = n.saturating_mul(p).div_ceil(100).max(1);
    v.get(i.saturating_sub(1)).copied()
}

/// The epoch ms of the UTC `secs`-of-day nearest to `anchor_ms`.
fn nearest_clock(secs: u64, anchor_ms: u64) -> u64 {
    let day = anchor_ms.saturating_sub(anchor_ms % DAY_MS);
    let at = day.saturating_add(secs.saturating_mul(1000));
    [at.saturating_sub(DAY_MS), at, at.saturating_add(DAY_MS)]
        .into_iter()
        .min_by_key(|t| t.abs_diff(anchor_ms))
        .unwrap_or(at)
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
#[expect(
    clippy::arithmetic_side_effects,
    reason = "bounded calendar arithmetic on parsed years 0..=9999"
)]
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of days since 1970-01-01.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "bounded calendar arithmetic on epoch days of u64 ms"
)]
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.fff…]Z` to epoch ms.
fn parse_ts(ts: &str) -> Option<u64> {
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, dd) = (d.next()?, d.next()?, d.next()?);
    let (y, m, dd): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, dd.parse().ok()?);
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&dd) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let secs = parse_clock(hms)?;
    let ms = frac
        .chars()
        .chain(std::iter::repeat('0'))
        .take(3)
        .collect::<String>()
        .parse::<u64>()
        .ok()?;
    let days = u64::try_from(days_from_civil(y, m, dd)).ok()?;
    days.checked_mul(DAY_MS)?
        .checked_add(secs.checked_mul(1000)?)?
        .checked_add(ms)
}

/// Epoch ms to `(YYYY-MM-DDTHH:MM:SSZ, HH:MM:SS)`, UTC.
fn utc(ms: u64) -> (String, String) {
    let secs = ms / 1000;
    let tod = secs % 86_400;
    let clock = format!("{:02}:{:02}:{:02}", tod / 3600, tod / 60 % 60, tod % 60);
    let (y, m, d) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    (format!("{y:04}-{m:02}-{d:02}T{clock}Z"), clock)
}

/// One `ears` line event from `flow.log`: side, words, `ts`.
type FlowLine = (Side, Vec<String>, u64);

/// The `"dir":"out"`, `"kind":"event"`, `ears`/`line` records of `flow.log`.
fn flow_lines(flow: &str) -> Vec<FlowLine> {
    flow.lines()
        .filter(|l| l.contains("\"ears\""))
        .filter_map(|l| {
            let v = json::parse(l)?;
            let s = |path: &[&str]| v.at(path).and_then(json::Value::as_str);
            if s(&["dir"]) != Some("out")
                || s(&["kind"]) != Some("event")
                || s(&["frame", "event"]) != Some("ears")
                || s(&["frame", "state"]) != Some("line")
            {
                return None;
            }
            let side = match s(&["frame", "side"])? {
                "me" => Side::Me,
                "them" => Side::Them,
                _ => return None,
            };
            let ts = parse_ts(s(&["ts"])?)?;
            Some((side, words(s(&["frame", "text"])?).0, ts))
        })
        .collect()
}

/// The shadow's records: phrases, and the start/stop counts.
#[derive(Debug, Default)]
struct ShadowLog {
    phrases: Vec<Phrase>,
    first_start: Option<u64>,
    last_ms: Option<u64>,
    starts: usize,
    stops: usize,
    crashes: usize,
    ignored: usize,
}

fn read_shadow(text: &str) -> ShadowLog {
    let mut log = ShadowLog::default();
    let mut stopped = true; // before the first start
    for raw in text.lines() {
        let Some((ms, rec)) = raw.split_once('\t') else {
            log.ignored = log
                .ignored
                .saturating_add(usize::from(!raw.trim().is_empty()));
            continue;
        };
        let Ok(ms) = ms.parse::<u64>() else {
            log.ignored = log.ignored.saturating_add(1);
            continue;
        };
        if rec.starts_with("SHADOW start") {
            if log.starts > 0 && !stopped {
                log.crashes = log.crashes.saturating_add(1);
            }
            log.starts = log.starts.saturating_add(1);
            log.first_start.get_or_insert(ms);
            stopped = false;
        } else if let Some(reason) = rec.strip_prefix("SHADOW stop ") {
            log.stops = log.stops.saturating_add(1);
            log.crashes = log
                .crashes
                .saturating_add(usize::from(reason.trim() == "error"));
            stopped = true;
        } else if let Some(line) = rec.strip_prefix(MARKER).and_then(parse_line) {
            log.phrases.push(phrase(&line, ms));
        } else {
            log.ignored = log.ignored.saturating_add(1);
            continue;
        }
        log.last_ms = Some(log.last_ms.map_or(ms, |l| l.max(ms)));
    }
    log
}

fn phrase(line: &Line<'_>, emit_ms: u64) -> Phrase {
    let (words, voice) = words(line.text);
    Phrase {
        side: line.side,
        span: (line.t0 != 0 || line.t1 != 0).then_some((line.t0, line.t1)),
        emit_ms,
        voice_only: voice && words.is_empty(),
        flow_missing: false,
        words,
    }
}

/// How live emit times were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmitSource {
    /// `HH:MM:SS` + 500 ms.
    Clock,
    /// `flow.log`'s `ts`; this many lines (in the window) were not found there and fell back to the clock.
    Flow {
        /// Lines in the window with no flow record of the same side and text within a minute.
        missing: usize,
    },
}

/// The live log's phrases, with their emit times; and the lines that were not readable ones.
fn read_live(text: &str, flow: Option<&[FlowLine]>, anchor: u64) -> (Vec<Phrase>, usize) {
    let mut out = Vec::new();
    let mut bad = 0_usize;
    for raw in text.lines() {
        let Some(rest) = raw.strip_prefix(MARKER) else {
            continue;
        };
        let Some(line) = parse_line(rest) else {
            bad = bad.saturating_add(1);
            continue;
        };
        let Some(secs) = line.clock else {
            bad = bad.saturating_add(1);
            continue;
        };
        let at = nearest_clock(secs, if line.t1 == 0 { anchor } else { line.t1 });
        let p = phrase(&line, at.saturating_add(CLOCK_HALF_MS));
        let emit = flow.and_then(|f| {
            f.iter()
                .filter(|(side, w, ts)| {
                    *side == p.side && *w == p.words && ts.abs_diff(at) <= FLOW_NEAR_MS
                })
                .min_by_key(|(_, _, ts)| ts.abs_diff(at))
                .map(|(_, _, ts)| *ts)
        });
        out.push(Phrase {
            emit_ms: emit.unwrap_or(p.emit_ms),
            flow_missing: flow.is_some() && emit.is_none(),
            ..p
        });
    }
    (out, bad)
}

/// The live log's phrases that have a span, as `(emit_ms, emit_ms − t1)`, in the log's order: the watchdog's input.
///
/// They are read as `--compare` reads them without `flow.log` (emit time: the line's `HH:MM:SS` + 500 ms).
#[must_use]
pub fn live_latencies(live_log: &str) -> Vec<(u64, i64)> {
    read_live(live_log, None, 0)
        .0
        .iter()
        .filter_map(|p| {
            let (_, t1) = p.span?;
            let emit = i64::try_from(p.emit_ms).ok()?;
            Some((p.emit_ms, emit.saturating_sub(i64::try_from(t1).ok()?)))
        })
        .collect()
}

/// One to one, per side, both with a span: `(live index, shadow index)`.
fn pair(live: &[Phrase], shadow: &[Phrase], how: Pairing) -> Vec<(usize, usize)> {
    // (key, tie-break, i, j): the smallest key wins.
    let mut cands = Vec::new();
    for (i, l) in live.iter().enumerate() {
        for (j, s) in shadow.iter().enumerate() {
            let (Some((l0, l1)), Some((s0, s1))) = (l.span, s.span) else {
                continue;
            };
            if l.side != s.side {
                continue;
            }
            match how {
                Pairing::Emit { tol_ms } => {
                    let d = l.emit_ms.abs_diff(s.emit_ms);
                    if d <= tol_ms {
                        cands.push((d, 0, i, j));
                    }
                }
                Pairing::Span => {
                    if l0.max(s0) <= l1.min(s1) {
                        let overlap = l1.min(s1).saturating_sub(l0.max(s0));
                        let off = l0.abs_diff(s0).saturating_add(l1.abs_diff(s1));
                        cands.push((u64::MAX.saturating_sub(overlap), off, i, j));
                    }
                }
            }
        }
    }
    cands.sort_unstable();
    let (mut used_l, mut used_s) = (vec![false; live.len()], vec![false; shadow.len()]);
    let mut pairs = Vec::new();
    for (_, _, i, j) in cands {
        if let (Some(ul), Some(us)) = (used_l.get_mut(i), used_s.get_mut(j))
            && !*ul
            && !*us
        {
            *ul = true;
            *us = true;
            pairs.push((i, j));
        }
    }
    pairs.sort_unstable();
    pairs
}

/// Per side: `[me, them]`.
pub type PerSide = [usize; 2];

const fn idx(side: Side) -> usize {
    match side {
        Side::Me => 0,
        Side::Them => 1,
    }
}

/// The comparison's numbers; nothing in it is text from a phrase.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Report {
    /// The window, epoch ms; `None` when the logs share no time.
    pub window: Option<(u64, u64)>,
    /// How live emit times were found.
    pub emit_source: EmitSource,
    /// How the phrases were paired.
    pub pairing: Pairing,
    /// Phrases in the window, per side.
    pub live: PerSide,
    /// Phrases in the window, per side.
    pub shadow: PerSide,
    /// Of those, with `0 0` spans.
    pub live_no_span: PerSide,
    /// Of those, with `0 0` spans.
    pub shadow_no_span: PerSide,
    /// Pairs, per side.
    pub paired: PerSide,
    /// Paired live phrases over live phrases with a span, %.
    pub coverage_live: Option<f64>,
    /// Paired shadow phrases over shadow phrases with a span, %.
    pub coverage_shadow: Option<f64>,
    /// Median `|Δt0|`, `|Δt1|` over the pairs, ms.
    pub delta_t0: Option<u64>,
    /// See `delta_t0`.
    pub delta_t1: Option<u64>,
    /// Median `|Δemit|` over the pairs, ms.
    pub delta_emit: Option<u64>,
    /// Emit latency (`t1` → line out) p50, p90, ms, of the live phrases with a span: the live clock.
    pub latency_live: Option<(i64, i64)>,
    /// The same for the shadow.
    pub latency_shadow: Option<(i64, i64)>,
    /// Unread-only phrases (`<voice>` / `<no speech>`), %; the report still names it `<voice>`-share.
    pub voice_live: Option<f64>,
    /// See `voice_live`.
    pub voice_shadow: Option<f64>,
    /// Word error rate over the pairs, median and p90, %.
    pub wer: Option<(f64, f64)>,
    /// Unpaired phrases with a span: emit time ([`Pairing::Emit`]) or `t0` ([`Pairing::Span`]), epoch ms; side,
    /// path; by time.
    pub unpaired: Vec<(u64, Side, Path)>,
    /// `SHADOW start` records in the whole log.
    pub starts: usize,
    /// `SHADOW stop` records in the whole log.
    pub stops: usize,
    /// Starts not preceded by a stop (after the first), and `stop error`s.
    pub crashes: usize,
    /// `TULPA-EARS-LINE`s of the live log that could not be read (or had no `HH:MM:SS`).
    pub live_unreadable: usize,
    /// Shadow records that are neither a line nor a start/stop.
    pub shadow_unreadable: usize,
}

/// The phrases with a span that found no pair, by emit time or `t0`.
fn unpaired(paths: &[(&Vec<Phrase>, &Vec<bool>, Path)], how: Pairing) -> Vec<(u64, Side, Path)> {
    let mut out = Vec::new();
    for &(v, done, path) in paths {
        for (p, d) in v.iter().zip(done) {
            if let (Some((t0, _)), false) = (p.span, *d) {
                let at = match how {
                    Pairing::Emit { .. } => p.emit_ms,
                    Pairing::Span => t0,
                };
                out.push((at, p.side, path));
            }
        }
    }
    out.sort_by_key(|&(t, side, path)| (t, idx(side), path == Path::Shadow));
    out
}

/// Emit latency (emit − `t1`) p50 and p90, ms, of the phrases with a span.
fn latency(v: &[Phrase]) -> Option<(i64, i64)> {
    let mut l: Vec<i64> = v
        .iter()
        .filter_map(|p| {
            let (_, t1) = p.span?;
            Some(
                i64::try_from(p.emit_ms)
                    .ok()?
                    .saturating_sub(i64::try_from(t1).ok()?),
            )
        })
        .collect();
    Some((rank(&mut l, 50)?, rank(&mut l, 90)?))
}

/// How the live phrases in the window got their emit times.
fn emit_source(flow: bool, live: &[Phrase]) -> EmitSource {
    if flow {
        EmitSource::Flow {
            missing: live.iter().filter(|p| p.flow_missing).count(),
        }
    } else {
        EmitSource::Clock
    }
}

/// Compares the live log with the shadow's (and `flow.log`'s emit times when given), pairing phrases `how`.
#[must_use]
pub fn compare(live_log: &str, shadow_log: &str, flow_log: Option<&str>, how: Pairing) -> Report {
    let sh = read_shadow(shadow_log);
    let flow = flow_log.map(flow_lines);
    let anchor = sh.first_start.or(sh.last_ms).unwrap_or(0);
    let (live_all, live_unreadable) = read_live(live_log, flow.as_deref(), anchor);
    let live_last = live_all.iter().map(|p| p.emit_ms).max();
    let window = match (
        sh.first_start
            .or_else(|| sh.phrases.first().map(|p| p.emit_ms)),
        sh.last_ms,
        live_last,
    ) {
        (Some(a), Some(b), Some(c)) if a <= b.min(c) => Some((a, b.min(c))),
        _ => None,
    };
    let inside = |p: &&Phrase| window.is_some_and(|(a, b)| (a..=b).contains(&p.emit_ms));
    let live: Vec<Phrase> = live_all.iter().filter(inside).cloned().collect();
    let emit_source = emit_source(flow.is_some(), &live);
    let shadow: Vec<Phrase> = sh.phrases.iter().filter(inside).cloned().collect();
    let pairs = pair(&live, &shadow, how);

    let count = |v: &[Phrase], f: &dyn Fn(&Phrase) -> bool| {
        let mut c = [0_usize; 2];
        for p in v.iter().filter(|p| f(p)) {
            if let Some(n) = c.get_mut(idx(p.side)) {
                *n = n.saturating_add(1);
            }
        }
        c
    };
    let mut paired = [0_usize; 2];
    let (mut d0, mut d1, mut de, mut wers) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut paired_l = vec![false; live.len()];
    let mut paired_s = vec![false; shadow.len()];
    for &(i, j) in &pairs {
        let (Some(l), Some(s)) = (live.get(i), shadow.get(j)) else {
            continue;
        };
        if let Some(n) = paired.get_mut(idx(l.side)) {
            *n = n.saturating_add(1);
        }
        if let (Some((l0, l1)), Some((s0, s1))) = (l.span, s.span) {
            d0.push(l0.abs_diff(s0));
            d1.push(l1.abs_diff(s1));
        }
        de.push(l.emit_ms.abs_diff(s.emit_ms));
        wers.push(wer(l, s));
        if let Some(b) = paired_l.get_mut(i) {
            *b = true;
        }
        if let Some(b) = paired_s.get_mut(j) {
            *b = true;
        }
    }
    let spanned = |v: &[Phrase]| v.iter().filter(|p| p.span.is_some()).count();
    let n_pairs = pairs.len();
    let coverage = |n: usize| (n > 0).then(|| pct(n_pairs, n));
    let voice = |v: &[Phrase]| {
        (!v.is_empty()).then(|| pct(v.iter().filter(|p| p.voice_only).count(), v.len()))
    };
    let unpaired = unpaired(
        &[
            (&live, &paired_l, Path::Live),
            (&shadow, &paired_s, Path::Shadow),
        ],
        how,
    );
    Report {
        window,
        emit_source,
        pairing: how,
        live: count(&live, &|_| true),
        shadow: count(&shadow, &|_| true),
        live_no_span: count(&live, &|p| p.span.is_none()),
        shadow_no_span: count(&shadow, &|p| p.span.is_none()),
        paired,
        coverage_live: coverage(spanned(&live)),
        coverage_shadow: coverage(spanned(&shadow)),
        delta_t0: rank(&mut d0, 50),
        delta_t1: rank(&mut d1, 50),
        delta_emit: rank(&mut de, 50),
        latency_live: latency(&live),
        latency_shadow: latency(&shadow),
        voice_live: voice(&live),
        voice_shadow: voice(&shadow),
        wer: rank(&mut wers.clone(), 50).zip(rank(&mut wers, 90)),
        unpaired,
        starts: sh.starts,
        stops: sh.stops,
        crashes: sh.crashes,
        live_unreadable,
        shadow_unreadable: sh.ignored,
    }
}

/// A criterion's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// Passed.
    Pass,
    /// Failed.
    Fail,
    /// No data to judge: a fail.
    NoData,
    /// Does not apply (the live spans are off the wall clock): neither a pass nor a fail.
    NotApplicable,
}

impl Verdict {
    const fn of(pass: Option<bool>) -> Self {
        match pass {
            Some(true) => Self::Pass,
            Some(false) => Self::Fail,
            None => Self::NoData,
        }
    }
}

/// One switch criterion: its name, the numbers it saw, and its verdict.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Criterion {
    /// What it asks, with its threshold.
    pub name: &'static str,
    /// The numbers it judged.
    pub seen: String,
    /// Its verdict.
    pub verdict: Verdict,
}

fn opt_pct(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.1}"))
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| v.to_string())
}

impl Report {
    /// The live median emit − `t1` is over 30 s: the live spans are off the wall clock, and the span-based
    /// criteria don't apply.
    #[must_use]
    pub fn live_off_clock(&self) -> bool {
        self.latency_live
            .is_some_and(|(p50, _)| p50 > LIVE_CLOCK_MAX_MS)
    }

    /// The switch criteria, judged.
    #[must_use]
    pub fn criteria(&self) -> Vec<Criterion> {
        let cov = self.coverage_live.zip(self.coverage_shadow);
        let delta = self.delta_t0.zip(self.delta_t1);
        let lat = self.latency_live.zip(self.latency_shadow);
        let voice = self.voice_live.zip(self.voice_shadow);
        let span_based = |pass: Option<bool>| {
            if self.live_off_clock() {
                Verdict::NotApplicable
            } else {
                Verdict::of(pass)
            }
        };
        vec![
            Criterion {
                name: "paired coverage >= 95 % both directions",
                seen: format!(
                    "live->shadow {} %, shadow->live {} %",
                    opt_pct(self.coverage_live),
                    opt_pct(self.coverage_shadow)
                ),
                verdict: Verdict::of(
                    cov.map(|(a, b)| a >= COVERAGE_MIN_PCT && b >= COVERAGE_MIN_PCT),
                ),
            },
            Criterion {
                name: "median |dt0| and |dt1| <= 250 ms",
                seen: format!("{} ms, {} ms", opt(self.delta_t0), opt(self.delta_t1)),
                verdict: span_based(delta.map(|(a, b)| a <= DELTA_MAX_MS && b <= DELTA_MAX_MS)),
            },
            Criterion {
                name: "emit latency p50 <= live p50, p90 <= live p90 + 500 ms",
                seen: format!(
                    "p50 {} vs {} ms, p90 {} vs {} ms",
                    opt(self.latency_shadow.map(|l| l.0)),
                    opt(self.latency_live.map(|l| l.0)),
                    opt(self.latency_shadow.map(|l| l.1)),
                    opt(self.latency_live.map(|l| l.1)),
                ),
                verdict: span_based(lat.map(|((l50, l90), (s50, s90))| {
                    s50 <= l50 && s90 <= l90.saturating_add(P90_SLACK_MS)
                })),
            },
            Criterion {
                name: "<voice>-share within 5 points",
                seen: format!(
                    "live {} %, shadow {} %",
                    opt_pct(self.voice_live),
                    opt_pct(self.voice_shadow)
                ),
                verdict: Verdict::of(voice.map(|(l, s)| (l - s).abs() <= VOICE_POINTS_MAX)),
            },
            Criterion {
                name: "per-pair word error median <= 10 %",
                seen: format!("median {} %", opt_pct(self.wer.map(|w| w.0))),
                verdict: Verdict::of(self.wer.map(|(m, _)| m <= WER_MAX_PCT)),
            },
            Criterion {
                name: "zero crashes, zero restarts",
                seen: format!(
                    "crashes {}, restarts {}",
                    self.crashes,
                    self.starts.saturating_sub(1)
                ),
                verdict: Verdict::of(Some(self.crashes == 0 && self.starts == 1)),
            },
        ]
    }

    /// Every criterion that applies passed.
    #[must_use]
    pub fn pass(&self) -> bool {
        self.criteria()
            .iter()
            .all(|c| matches!(c.verdict, Verdict::Pass | Verdict::NotApplicable))
    }

    /// The report's head: how the numbers were found, the window, the pairing and the live clock.
    fn render_head(&self, r: &mut String) {
        let _w = writeln!(r, "tulpa-hear --compare: counts only, no phrase text");
        let _w = writeln!(
            r,
            "note: live-path word times are VAD-compressed (known defect); spans (t0/t1) are not"
        );
        let _w = match self.emit_source {
            EmitSource::Clock => writeln!(r, "live emit time: HH:MM:SS + 500 ms (no flow.log)"),
            EmitSource::Flow { missing } => writeln!(
                r,
                "live emit time: flow.log ts (same side and text, nearest HH:MM:SS); not found there, HH:MM:SS + 500 ms: {missing}"
            ),
        };
        let _w = match self.window {
            Some((a, b)) => writeln!(
                r,
                "window: {} .. {} ({} s)",
                utc(a).0,
                utc(b).0,
                b.saturating_sub(a) / 1000
            ),
            None => writeln!(r, "window: none (the logs share no time)"),
        };
        let _w = match self.pairing {
            Pairing::Emit { tol_ms } => writeln!(r, "pairing: emit times within {tol_ms} ms"),
            Pairing::Span => writeln!(r, "pairing: span overlap"),
        };
        let _w = writeln!(
            r,
            "live clock (emit - t1) ms: live median {} p90 {}, shadow median {} p90 {}",
            opt(self.latency_live.map(|l| l.0)),
            opt(self.latency_live.map(|l| l.1)),
            opt(self.latency_shadow.map(|l| l.0)),
            opt(self.latency_shadow.map(|l| l.1)),
        );
        if self.live_off_clock() {
            let _w = writeln!(r, "live spans off the wall clock: span-based criteria n/a");
        }
    }

    /// The plain-text table: numbers, times, sides and paths only.
    #[must_use]
    pub fn render(&self) -> String {
        let mut r = String::new();
        self.render_head(&mut r);
        let _w = writeln!(r, "{:<26}{:>8}{:>8}{:>8}", "", "me", "them", "total");
        let mut row = |name: &str, v: PerSide| {
            let [a, b] = v;
            let _w = writeln!(r, "{name:<26}{a:>8}{b:>8}{:>8}", a.saturating_add(b));
        };
        row("live phrases", self.live);
        row("shadow phrases", self.shadow);
        row("live no span (0 0)", self.live_no_span);
        row("shadow no span (0 0)", self.shadow_no_span);
        row("paired", self.paired);
        let _w = writeln!(
            r,
            "coverage live->shadow %   {}",
            opt_pct(self.coverage_live)
        );
        let _w = writeln!(
            r,
            "coverage shadow->live %   {}",
            opt_pct(self.coverage_shadow)
        );
        let _w = writeln!(r, "median |dt0| ms           {}", opt(self.delta_t0));
        let _w = writeln!(r, "median |dt1| ms           {}", opt(self.delta_t1));
        if matches!(self.pairing, Pairing::Emit { .. }) {
            let _w = writeln!(r, "median |demit| ms         {}", opt(self.delta_emit));
        }
        for (name, l) in [("live", self.latency_live), ("shadow", self.latency_shadow)] {
            let _w = writeln!(
                r,
                "emit latency {name:<7} ms    p50 {} p90 {}",
                opt(l.map(|l| l.0)),
                opt(l.map(|l| l.1))
            );
        }
        let _w = writeln!(r, "<voice>-share live %      {}", opt_pct(self.voice_live));
        let _w = writeln!(
            r,
            "<voice>-share shadow %    {}",
            opt_pct(self.voice_shadow)
        );
        let _w = writeln!(
            r,
            "word error %              median {} p90 {}",
            opt_pct(self.wer.map(|w| w.0)),
            opt_pct(self.wer.map(|w| w.1))
        );
        let _w = writeln!(
            r,
            "shadow starts {}, stops {}, crashes {}",
            self.starts, self.stops, self.crashes
        );
        let _w = writeln!(
            r,
            "unreadable lines: live {}, shadow {}",
            self.live_unreadable, self.shadow_unreadable
        );
        let at = match self.pairing {
            Pairing::Emit { .. } => "emit",
            Pairing::Span => "t0",
        };
        let _w = writeln!(
            r,
            "unpaired ({at} UTC, side, path): {}",
            self.unpaired.len()
        );
        for &(t0, side, path) in &self.unpaired {
            let _w = writeln!(r, "  {} {} {}", utc(t0).1, side.as_str(), path.as_str());
        }
        let _w = writeln!(r, "criteria:");
        for c in self.criteria() {
            let verdict = match c.verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
                Verdict::NoData => "FAIL (no data)",
                Verdict::NotApplicable => "N/A (live clock)",
            };
            let _w = writeln!(r, "  {verdict} {}: {}", c.name, c.seen);
        }
        let _w = writeln!(r, "result: {}", if self.pass() { "PASS" } else { "FAIL" });
        r
    }
}

/// Just enough JSON to read `flow.log`'s records.
mod json {
    /// A JSON value; numbers are kept as their text.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum Value {
        Null,
        Bool(bool),
        Num(String),
        Str(String),
        Arr(Vec<Self>),
        Obj(Vec<(String, Self)>),
    }

    impl Value {
        /// The value at a path of object keys.
        pub(super) fn at(&self, path: &[&str]) -> Option<&Self> {
            path.iter().try_fold(self, |v, k| match v {
                Self::Obj(kv) => kv.iter().find(|(key, _)| key == k).map(|(_, v)| v),
                _ => None,
            })
        }

        pub(super) const fn as_str(&self) -> Option<&str> {
            match self {
                Self::Str(s) => Some(s.as_str()),
                _ => None,
            }
        }
    }

    /// One whole value (whitespace around it allowed); `None` for anything else.
    pub(super) fn parse(s: &str) -> Option<Value> {
        let mut p = Parser {
            b: s.as_bytes(),
            i: 0,
        };
        let v = p.value(0)?;
        p.ws();
        (p.i == p.b.len()).then_some(v)
    }

    struct Parser<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }

        fn bump(&mut self) -> Option<u8> {
            let c = self.peek()?;
            self.i = self.i.saturating_add(1);
            Some(c)
        }

        fn ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.i = self.i.saturating_add(1);
            }
        }

        fn eat(&mut self, lit: &[u8]) -> Option<()> {
            let end = self.i.checked_add(lit.len())?;
            (self.b.get(self.i..end)? == lit).then(|| self.i = end)
        }

        fn value(&mut self, depth: usize) -> Option<Value> {
            if depth > 64 {
                return None;
            }
            self.ws();
            match self.peek()? {
                b'{' => {
                    self.i = self.i.saturating_add(1);
                    let mut kv = Vec::new();
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.i = self.i.saturating_add(1);
                        return Some(Value::Obj(kv));
                    }
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.ws();
                        self.eat(b":")?;
                        let v = self.value(depth.saturating_add(1))?;
                        kv.push((k, v));
                        self.ws();
                        match self.bump()? {
                            b',' => {}
                            b'}' => return Some(Value::Obj(kv)),
                            _ => return None,
                        }
                    }
                }
                b'[' => {
                    self.i = self.i.saturating_add(1);
                    let mut a = Vec::new();
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i = self.i.saturating_add(1);
                        return Some(Value::Arr(a));
                    }
                    loop {
                        a.push(self.value(depth.saturating_add(1))?);
                        self.ws();
                        match self.bump()? {
                            b',' => {}
                            b']' => return Some(Value::Arr(a)),
                            _ => return None,
                        }
                    }
                }
                b'"' => self.string().map(Value::Str),
                b't' => self.eat(b"true").map(|()| Value::Bool(true)),
                b'f' => self.eat(b"false").map(|()| Value::Bool(false)),
                b'n' => self.eat(b"null").map(|()| Value::Null),
                _ => {
                    let start = self.i;
                    while matches!(
                        self.peek(),
                        Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                    ) {
                        self.i = self.i.saturating_add(1);
                    }
                    let n = std::str::from_utf8(self.b.get(start..self.i)?).ok()?;
                    n.parse::<f64>().ok().map(|_| Value::Num(n.to_owned()))
                }
            }
        }

        fn hex4(&mut self) -> Option<u32> {
            let end = self.i.checked_add(4)?;
            let h = std::str::from_utf8(self.b.get(self.i..end)?).ok()?;
            let v = u32::from_str_radix(h, 16).ok()?;
            self.i = end;
            Some(v)
        }

        fn string(&mut self) -> Option<String> {
            self.eat(b"\"")?;
            let mut out: Vec<u8> = Vec::new();
            loop {
                match self.bump()? {
                    b'"' => return String::from_utf8(out).ok(),
                    b'\\' => {
                        let c = match self.bump()? {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{8}',
                            b'f' => '\u{c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            b'u' => {
                                let hi = self.hex4()?;
                                let code = if (0xD800..0xDC00).contains(&hi) {
                                    self.eat(b"\\u")?;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return None;
                                    }
                                    0x1_0000_u32
                                        .checked_add((hi.checked_sub(0xD800)?) << 10)?
                                        .checked_add(lo.checked_sub(0xDC00)?)?
                                } else {
                                    hi
                                };
                                char::from_u32(code)?
                            }
                            _ => return None,
                        };
                        let mut buf = [0_u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                    c if c < 0x20 => return None,
                    c => out.push(c),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_nested_objects_and_escapes() {
            let v = parse(r#" {"a":{"b":"xé😀\n","n":-1.5e3,"t":true,"z":null,"l":[1,"y"]}} "#);
            assert_eq!(
                v.as_ref()
                    .and_then(|v| v.at(&["a", "b"]))
                    .and_then(Value::as_str),
                Some("x\u{e9}\u{1f600}\n")
            );
            assert_eq!(parse(r#"{"a":1"#), None);
            assert_eq!(parse(r#"{"a":1} x"#), None);
            assert_eq!(parse(r#""\ud83d""#), None, "a lone high surrogate");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn words_drop_tags_intonation_and_punctuation() {
        assert_eq!(
            words("Well, <pause 1.2s>then: GO! [intonation: rising]"),
            (w("well then go"), false)
        );
        assert_eq!(words("<voice 3.0s>"), (vec![], true));
        assert_eq!(words("a <voice 1.0s> b"), (w("a b"), true));
        assert_eq!(words("<no speech 3.0s>"), (vec![], true));
        assert_eq!(words("кое-что, Да"), (w("коечто да"), false));
    }

    #[test]
    fn a_line_parses_with_or_without_clock_and_tag() {
        let l = parse_line("1 2 10:00:04 [me] [guest] Hi there [intonation: rising]\t1,2,hi");
        assert_eq!(
            l.map(|l| (l.t0, l.t1, l.clock, l.side, l.text)),
            Some((
                1,
                2,
                Some(36_004),
                Side::Me,
                "Hi there [intonation: rising]"
            ))
        );
        let l = parse_line("0 0 [them] [intonation: falling]");
        assert_eq!(
            l.map(|l| (l.clock, l.text)),
            Some((None, "[intonation: falling]"))
        );
        assert_eq!(parse_line("1 2 10:00:04 [we] x"), None);
        assert_eq!(parse_line("x 2 10:00:04 [me] x"), None);
    }

    #[test]
    fn distance_is_word_levenshtein() {
        assert_eq!(distance(&w("a b c"), &w("a b c")), 0);
        assert_eq!(distance(&w("a b c"), &w("a x c")), 1);
        assert_eq!(distance(&w("a b c d e"), &w("a b c")), 2);
        assert_eq!(distance(&[], &w("a b")), 2);
    }

    #[test]
    fn rank_is_nearest_rank() {
        assert_eq!(rank(&mut [4, 1, 3, 2], 50), Some(2));
        assert_eq!(rank(&mut [5, 1, 2, 3, 4], 90), Some(5));
        assert_eq!(rank(&mut [7], 90), Some(7));
        assert_eq!(rank::<u64>(&mut [], 50), None);
    }

    #[test]
    fn timestamps_round_trip() {
        let ms = parse_ts("2026-11-22T10:00:04.3Z");
        assert_eq!(ms, Some(1_795_341_604_300));
        assert_eq!(utc(1_795_341_604_300).0, "2026-11-22T10:00:04Z");
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-11-22 10:00:04Z"), None);
        // The clock nearest an anchor just after midnight is the previous day's 23:59:59.
        let midnight = parse_ts("2026-11-23T00:00:00.200Z").unwrap_or(0);
        assert_eq!(nearest_clock(86_399, midnight), midnight - 1_200);
    }
}
