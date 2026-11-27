//! #0423: the log window's own model — what the ears heard (and whether each
//! phrase reached the agent), what Tulpa said, and the lines the operator
//! types back.
//!
//! Pure and platform-free: the window ([`crate::macos`]) only renders
//! `render()`; every rule here is unit-tested on Linux.
//!
//! Marks are honest, never invented: a heard phrase the ears let through to
//! the link is `→ kira, queued`, and from then on its mark follows the wire's
//! word exactly as a typed line's does (below: on the wire, or lost with why);
//! one dropped before it left the Mac carries why, in `flow.log`'s own words
//! (`paused`, `no-capture`, `replaced-capture`, `echo`, `mode` — the mode
//! filter keeping the system audio on the Mac, a video or a song outside a
//! call — or the capture's own, `prompt-echo`, `own-voice`); and a transcript
//! line that is not a phrase at all (no side, no text) is not shown — noise
//! that was never heard. Both come from the one place the ears decide a
//! line's fate (`Ears::fate`). A line let through — heard or typed — is
//! `queued` the moment it is taken and stays so until the wire itself says
//! otherwise ([`Wire`]: `on the wire` when the link writes it — never `sent`:
//! the hub sends no word back — and `lost` with why when a full lane drops it
//! or the link breaks with it unwritten; the wire's word via
//! [`crate::link::WireNews`], by the entry's id, never a guess). A typed line
//! is `refused` with
//! why when it misses [`line_ok`] — the refusal shows in the window, so the
//! typist sees their line did not go.
//!
//! The outbound wire shape lives here too ([`typed_event`]): one id-less
//! event line on the control feed, `{"event":"text","state":"said","text",
//! "ts"}`, the same door the ears' and files' events use — text and ts only,
//! never a name the payload could choose. The door at the agent's side
//! caps the same shapes again.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::json;

use crate::link::{Outbox, WireNews, held};
use crate::log::format_utc;

/// One typed line at most, in characters — the agent's door refuses the same.
pub const TEXT_CHARS: usize = 2000;
/// How many entries the window keeps; older ones fall off the top.
pub const CAP: usize = 500;

/// Where a line let through to the link stands, by the wire's word — the same for a heard phrase and a
/// typed line (#0423 r2): one mark, one way it changes ([`SayLog::wire`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Wire {
    /// Waiting in the outbox for the link to write it.
    Queued,
    /// Written to the link's pipe (#0423, akira T2): "on the wire", never "sent". The hub sends no
    /// word back (guild `tulpa-text.ts` for a typed line, the ears' event door for a phrase), so a
    /// link that dies after the write, or a hub that drops the line, is past what the tray can know.
    OnWire,
    /// Never written, and why — the wire's own word: a full lane dropped it, or the link broke with it
    /// still unwritten; shown, never silent.
    Lost(&'static str),
}

impl Wire {
    /// The mark's words after `→ kira, `.
    fn words(self) -> String {
        match self {
            Self::Queued => "queued".to_owned(),
            Self::OnWire => "on the wire".to_owned(),
            Self::Lost(why) => format!("lost: {why}"),
        }
    }
}

/// What happened to a heard phrase.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Heard {
    /// Let through to the link — the phrase passed every gate on this Mac — and where it stands since.
    Wire(Wire),
    /// Dropped on this Mac before it left, and why: the tray's own word (`paused`, `echo`, `mode`…) or
    /// the capture's (`prompt-echo`, `own-voice`).
    Dropped(String),
}

impl Heard {
    /// The fate as `flow.log` words it, at the funnel: `sent` (flow.log's word for "let through to the
    /// outbox", unchanged for its readers; the window's mark is the wire's) or `dropped:<why>`.
    #[must_use]
    pub fn outcome(&self) -> String {
        match self {
            Self::Wire(_) => "sent".to_owned(),
            Self::Dropped(why) => format!("dropped:{why}"),
        }
    }
}

/// What happened to a typed line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Typed {
    /// Taken into the outbox's typed lane, and where it stands since.
    Wire(Wire),
    /// Refused here, and why; it never left the Mac.
    Refused(&'static str),
}

/// One line of the log. Its Debug shows the text's length, never the text (#0423 r3 S3, P2: the
/// text stays in the window and in memory, never in a log line, a panic message or an assert).
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub struct Entry {
    /// When it happened.
    pub at: SystemTime,
    /// Which stream it belongs to.
    pub kind: Kind,
    /// The phrase, the said text, or the typed line.
    pub text: String,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Entry")
            .field("at", &self.at)
            .field("kind", &self.kind)
            .field("text_chars", &self.text.chars().count())
            .finish()
    }
}

/// Which stream a line belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Kind {
    /// A phrase the ears transcribed.
    Heard {
        /// Which side of the capture it came from: `me` or `them`.
        side: &'static str,
        /// The line's own number in this log ([`SayLog::heard`]): the wire's word finds it by it.
        id: u64,
        /// Whether it reached the link, and if not, why not.
        fate: Heard,
    },
    /// Something Tulpa spoke (`voice.say`).
    Said,
    /// A line the operator typed into the window, and its fate.
    Typed {
        /// The line's own number in this log ([`SayLog::typed`]): the wire's word finds its line by
        /// it, never by the text.
        id: u64,
        /// What became of it so far.
        fate: Typed,
    },
}

/// The log: a bounded ring of entries. Shared as `Arc<Mutex<SayLog>>`. Its Debug is [`Entry`]'s,
/// entry by entry: never a text.
#[derive(Debug, Default)]
pub struct SayLog {
    entries: VecDeque<Entry>,
    /// The id the next heard or typed line gets: one count, so the wire's word names one entry.
    next_id: u64,
}

impl SayLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 0,
        }
    }

    /// Appends one entry, dropping the oldest past [`CAP`].
    pub fn push(&mut self, entry: Entry) {
        if self.entries.len() >= CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// A heard phrase with its fate; returns the phrase's id, which the wire's word for it carries
    /// ([`SayLog::wire`]). Logged before the phrase is queued, so the word can never come first.
    pub fn heard(
        &mut self,
        at: SystemTime,
        side: &'static str,
        mode: &str,
        text: &str,
        fate: Heard,
    ) -> u64 {
        let id = self.take_id();
        self.push(Entry {
            at,
            kind: Kind::Heard { side, id, fate },
            text: format!("({mode}) {}", capped(text)),
        });
        id
    }

    const fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Something Tulpa said.
    pub fn said(&mut self, at: SystemTime, text: &str) {
        self.push(Entry {
            at,
            kind: Kind::Said,
            text: capped(text),
        });
    }

    /// A typed line with its fate; returns the line's id, which the wire's word for it carries
    /// ([`SayLog::wire`]). Logged before the line is queued, so the word can never come first.
    pub fn typed(&mut self, at: SystemTime, text: &str, fate: Typed) -> u64 {
        let id = self.take_id();
        self.push(Entry {
            at,
            kind: Kind::Typed { id, fate },
            text: capped(text),
        });
        id
    }

    /// The wire's word for the queued line `id` (#0423, akira F2; heard lines too since r2), heard or
    /// typed. Only a line still queued flips; a line the ring already let go is not found and nothing
    /// changes. The text is never read: two equal lines are two lines.
    pub fn rewire(&mut self, id: u64, now: Wire) {
        let queued = self.entries.iter_mut().find_map(|e| match &mut e.kind {
            Kind::Heard {
                id: of,
                fate: Heard::Wire(wire),
                ..
            }
            | Kind::Typed {
                id: of,
                fate: Typed::Wire(wire),
            } if *of == id && *wire == Wire::Queued => Some(wire),
            _ => None,
        });
        if let Some(wire) = queued {
            *wire = now;
        }
    }

    /// The wire's word for a queued line ([`WireNews`], the outbox's callback): written → on the wire,
    /// evicted from a full lane or broken with the link before its write → lost. The one place a
    /// queued line's mark changes after it was taken.
    pub fn wire(&mut self, news: WireNews) {
        match news {
            WireNews::Written(id) => self.rewire(id, Wire::OnWire),
            WireNews::Evicted(id) => self.rewire(id, Wire::Lost("the queue was full")),
            WireNews::Broken(id) => {
                self.rewire(id, Wire::Lost("the link broke before it was written"));
            }
        }
    }

    /// The window's whole text, oldest first, one entry per visual line.
    ///
    /// Every character that would end a line or turn the direction of what follows (Cc, U+2028/U+2029,
    /// Unicode's `Bidi_Control`) shows as a space, in the text and in every word around it, so no text
    /// can start a line of its own or forge a mark.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            let mark = match &e.kind {
                Kind::Heard { side, fate, .. } => match fate {
                    Heard::Wire(wire) => format!("[{side} → kira, {}] {}", wire.words(), e.text),
                    Heard::Dropped(why) => format!("[{side} dropped: {why}] {}", e.text),
                },
                Kind::Said => format!("[tulpa] {}", e.text),
                Kind::Typed { fate, .. } => match fate {
                    Typed::Wire(wire) => format!("[you → kira, {}] {}", wire.words(), e.text),
                    Typed::Refused(why) => format!("[refused: {why}] {}", e.text),
                },
            };
            let line = format!("{} {mark}", clock(e.at));
            out.extend(
                line.chars()
                    .map(|c| if breaks_the_window(c) { ' ' } else { c }),
            );
            out.push('\n');
        }
        out
    }
}

/// The window as the menu's «Log window» item sees it ([`log_click`]); `AppKit`'s panel on the Mac.
pub trait Shown {
    /// Whether it is on screen (a hidden window is not).
    fn is_open(&self) -> bool;
    /// Puts it on screen, in front.
    fn raise(&self);
    /// Hides it; it stays built, with its typed line, for the next click.
    fn close(&self);
}

/// A click on the menu's «Log window» item (#0423 r3 S4); returns whether the window is open now.
///
/// The item owns the window's visibility (the window has no close button), so every click flips it:
/// the first builds it (`build` shows it), a shown one hides, a hidden one comes back. The result is
/// the item's tick.
pub fn log_click<W: Shown>(window: &mut Option<W>, build: impl FnOnce() -> W) -> bool {
    match window {
        None => *window = Some(build()),
        Some(shown) if shown.is_open() => shown.close(),
        Some(hidden) => hidden.raise(),
    }
    window.as_ref().is_some_and(W::is_open)
}

/// A typed line's one shape, the agent door's exactly (guild `src/lib/typed-line.ts` `typedLine`).
///
/// Trimmed as JavaScript's `trim()` trims ([`js_space`]), then 1..=[`TEXT_CHARS`] characters (Unicode
/// scalar values, the door's `[...text].length`), none of the door's refused set. Returns the trimmed
/// line — the text the door would take — or refuses the line whole, never mending it.
///
/// The set is ONE set on both sides (akira A2, 16:20Z): the Cc controls (`char::is_control`, the
/// door's `\p{Cc}`), the line and paragraph separators (U+2028/U+2029 — all of Zl and Zp; a U+2028
/// can read as a line break in the steward's input, enough to fake a system line), and the bidi
/// controls (U+202A-202E, U+2066-2069, which smuggle meaning).
///
/// # Errors
///
/// `Err(why)` with a line the window can show when the text is empty after trimming, carries one of
/// the refused characters, or is over the cap.
pub fn line_ok(text: &str) -> Result<&str, &'static str> {
    let trimmed = text.trim_matches(js_space);
    if trimmed.is_empty() {
        return Err("an empty line goes nowhere");
    }
    if trimmed.chars().any(door_refuses) {
        return Err("a line with control or direction characters goes nowhere");
    }
    if trimmed.chars().count() > TEXT_CHARS {
        return Err("a line over the cap goes nowhere");
    }
    Ok(trimmed)
}

/// The door's refused set ([`line_ok`]): Cc, Zl/Zp, and the bidi embeddings, overrides and isolates.
fn door_refuses(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// What the window never shows as itself (#0423 r3 S2): every character that ends a visual line — the
/// Cc controls (LF, CR, VT, FF, NEL U+0085 among them) and U+2028/U+2029, all of Unicode's mandatory
/// breaks — and every character that changes direction: Unicode's whole `Bidi_Control` set, which is
/// the door's (U+202A-202E, U+2066-2069) plus the marks U+200E, U+200F and U+061C the door lets through.
fn breaks_the_window(c: char) -> bool {
    door_refuses(c) || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}')
}

/// What JavaScript's `String.prototype.trim` strips, the door's trim: ECMAScript's `WhiteSpace` (TAB,
/// VT, FF, ZWNBSP U+FEFF, and every Zs space) and `LineTerminator` (LF, CR, U+2028, U+2029). Rust's
/// `str::trim` differs both ways — it strips U+0085 (a Cc the door refuses) and keeps U+FEFF (which
/// the door strips) — so it cannot stand in.
const fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// One line the operator typed (#0423), checked, logged and queued; returns the fate it logged.
///
/// A refused line ([`line_ok`]) is logged with why and never leaves the Mac; an accepted one is logged `queued` under a fresh id FIRST, then queued on the
/// outbox's typed lane under that id ([`Outbox::push_typed`]) — so the wire's word, which may come at
/// once, always finds its line. The window's lock is let go before the push: the wire's word takes it.
pub fn take_typed(saylog: &Mutex<SayLog>, outbox: &Outbox, line: &str, now: SystemTime) -> Typed {
    let mut log = held(saylog);
    match line_ok(line) {
        Err(why) => {
            log.typed(now, line, Typed::Refused(why));
            Typed::Refused(why)
        }
        Ok(text) => {
            let id = log.typed(now, text, Typed::Wire(Wire::Queued));
            drop(log);
            outbox.push_typed(id, typed_event(text, format_utc(now)));
            Typed::Wire(Wire::Queued)
        }
    }
}

/// Lets the wire's word for every queued line reach the window's log ([`SayLog::wire`]).
///
/// Registered once, where both exist. The callback takes no lock of the outbox's own, only the log's, briefly; a
/// poisoned log still takes the word — a mark must not stay `queued` for a line the wire settled.
pub fn attach(saylog: &std::sync::Arc<Mutex<SayLog>>, outbox: &Outbox) {
    let saylog = std::sync::Arc::clone(saylog);
    outbox.set_on_wire(Box::new(move |news| {
        held(&saylog).wire(news);
    }));
}

/// One typed line as the link's event, exactly the shape the agent's door
/// takes: text and ts only. The text arrives pre-checked ([`line_ok`]).
#[must_use]
pub fn typed_event(text: &str, ts: Option<String>) -> Vec<u8> {
    let mut line = json!({
        "event": "text",
        "state": "said",
        "text": text,
    })
    .to_string();
    if let Some(ts) = ts {
        // The ts is our own formatted UTC string (log::format_utc): no quotes
        // or backslashes to escape.
        line.pop();
        let tail = format!(",\"ts\":\"{ts}\"}}");
        line.push_str(&tail);
    }
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    bytes
}

/// The text capped at [`TEXT_CHARS`] characters (not bytes — a cut inside a character would show as
/// garbage). Kept as given otherwise: the window flattens it when it renders ([`SayLog::render`]).
fn capped(text: &str) -> String {
    text.chars().take(TEXT_CHARS).collect()
}

/// `HH:MM:SS` UTC of `at`, or `--:--:--` when it is before the epoch: a visible unknown, never a
/// wrong time.
fn clock(at: SystemTime) -> String {
    let Ok(since) = at.duration_since(SystemTime::UNIX_EPOCH) else {
        return "--:--:--".to_owned();
    };
    let secs = since.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests;
