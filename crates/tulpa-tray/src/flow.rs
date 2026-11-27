//! `flow.log`: one `tail -f`-able file of everything that crosses the link.
//!
//! One JSON object per line, written and flushed per frame:
//! `{"ts":…,"dir":"in"|"out"|"note","id":…,"kind":…,"frame":…}` plus, for some kinds, more fields
//! (`outcome` for an ears line, `stage` for a say). `frame` is the whole frame — no trimming, no
//! redaction — embedded as JSON when it is a JSON value, else as a string. Two exceptions: the
//! link's own credentials — a hello's `token`, `data` (the ticket) and `secret` are logged as
//! `"<redacted>"` ([`redact_hello`]) — and the text of a line the operator typed into the log window
//! (#0423): its frame is logged with `text` as `"<redacted>"` ([`redact_typed`]), its fate beside it.
//! The operator's words stay in memory only.
//!
//! The command channel's frames are logged at its one choke point, [`crate::link::serve_attempt`],
//! which wraps the pipe in a [`LoggedPipe`]: no verb can answer around it.
//!
//! The logger is process-wide and set once ([`init`]); before that every call is a no-op. Writing
//! never panics and never blocks for long: one `write_all` of one line under a mutex. The first I/O
//! error is noted once to the tray's activity log; the rest are ignored. Past [`ROTATE_AT`] bytes the
//! file rotates to `.1` … `.5`, the oldest deleted.
//!
//! A fresh file — new, empty, or the live file after a rotation — starts with one header line,
//! `{"ts","dir":"note","id":null,"kind":"header","text":HEADER_TEXT}`, saying what the file holds
//! and what it hides. Appending to a file that already has lines adds none.
//!
//! Keepalives are summarised, not logged per frame: the hub's `ping` requests and the Mac's replies
//! to them become one `keepalive` note every [`KEEPALIVE_SUMMARY`] (`pings`, `pongs`,
//! `last_reply_ms`, `since`), plus one at once with `gap_s` when a ping comes more than
//! [`KEEPALIVE_GAP`] after the one before. `TULPA_FLOW_KEEPALIVE=all` logs every frame instead.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::link::{LinkError, Pipe, Recv};
use crate::log::{ActivityLog, format_utc};

/// The file's name, beside the tray's activity log.
pub const FILE_NAME: &str = "flow.log";

/// Rotation threshold: past this many bytes `flow.log` becomes `flow.log.1`.
pub const ROTATE_AT: u64 = 50 * 1024 * 1024;

/// Rotated files kept (`.1` … `.5`); the oldest beyond is deleted.
pub const KEEP: u32 = 5;

/// What a credential in a hello is logged as.
pub const REDACTED: &str = "<redacted>";

/// The `text` of the header line that starts every fresh or rotated `flow.log`.
pub const HEADER_TEXT: &str = "flow.log: every frame over the Tulpa link and every tray decision, verbatim. \
     Two things are hidden, each replaced by \"<redacted>\": the link's own credentials (the hello's token, \
     ticket, secret) and the text of a line typed into the log window. Nothing else is trimmed or redacted. Keepalives are summarised \
     (every 10 min, gaps at once; TULPA_FLOW_KEEPALIVE=all for every frame).";

/// How often the keepalive summary is written, at the first ping past it.
pub const KEEPALIVE_SUMMARY: Duration = Duration::from_secs(10 * 60);

/// A ping this long after the one before is logged at once, with `gap_s`.
pub const KEEPALIVE_GAP: Duration = Duration::from_secs(150);

/// The environment variable that, set to `all`, logs every keepalive frame instead of summaries.
pub const KEEPALIVE_ENV: &str = "TULPA_FLOW_KEEPALIVE";

/// In-flight ping ids remembered to match their replies; the oldest is forgotten past this.
const KEEPALIVE_IN_FLIGHT: usize = 16;

/// Which way a line went: from the hub (`in`), to it (`out`), or a tray decision (`note`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dir {
    /// Read from the hub.
    In,
    /// Written to the hub.
    Out,
    /// The tray's own decision about a line (an ears line, a say, a dropped event).
    Note,
}

impl Dir {
    const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Note => "note",
        }
    }
}

/// What a logged line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// A command from the hub.
    Request,
    /// The Mac's answer to a request (same `id`).
    Reply,
    /// An unsolicited line the Mac sends.
    Event,
    /// A hello, credentials redacted.
    Hello,
    /// A line the operator typed into the log window (#0423), its text redacted ([`redact_typed`]):
    /// written to the hub, or (a note, `outcome`) evicted from a full typed lane.
    Typed,
    /// A transcript line and whether it went (`outcome`).
    Ears,
    /// A `voice.say` through its lifecycle (`stage`).
    Voice,
    /// A fate frame from the hub; reserved for a later change on the hub's side.
    Fate,
    /// An event dropped from a full outbox before it could go.
    Drop,
    /// The file's first line, saying what it holds ([`HEADER_TEXT`]).
    Header,
    /// The link itself, when it did not carry something: down, a failed attempt, the retry and its
    /// delay, a stop, an event queued while offline (`why`, `detail`).
    Link,
    /// Keepalive pings and their replies, summarised (`pings`, `pongs`, `last_reply_ms`, `since`)
    /// or a gap before a ping (`gap_s`).
    Keepalive,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Reply => "reply",
            Self::Event => "event",
            Self::Hello => "hello",
            Self::Typed => "typed",
            Self::Ears => "ears",
            Self::Voice => "voice",
            Self::Fate => "fate",
            Self::Drop => "drop",
            Self::Header => "header",
            Self::Link => "link",
            Self::Keepalive => "keepalive",
        }
    }
}

/// One `flow.log` file, rotating by size.
pub struct FlowLog {
    path: PathBuf,
    rotate_at: u64,
    file: Mutex<Option<Sink>>,
    complain: Option<PathBuf>,
    complained: AtomicBool,
}

struct Sink {
    file: File,
    size: u64,
}

impl fmt::Debug for FlowLog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FlowLog")
            .field("path", &self.path)
            .field("rotate_at", &self.rotate_at)
            .finish_non_exhaustive()
    }
}

impl FlowLog {
    /// A log at `path` rotating past `rotate_at` bytes; the first I/O error is noted to the activity
    /// log at `complain`, when given. The file is opened on the first write.
    #[must_use]
    pub const fn new(path: PathBuf, rotate_at: u64, complain: Option<PathBuf>) -> Self {
        Self {
            path,
            rotate_at,
            file: Mutex::new(None),
            complain,
            complained: AtomicBool::new(false),
        }
    }

    /// The file written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one entry; a failure is noted once and otherwise ignored.
    pub fn write(
        &self,
        dir: Dir,
        id: Option<&str>,
        kind: Kind,
        frame: &[u8],
        extra: &[(&str, &str)],
    ) {
        self.write_at(SystemTime::now(), dir, id, kind, frame, extra);
    }

    /// [`FlowLog::write`], stamped `at`.
    fn write_at(
        &self,
        at: SystemTime,
        dir: Dir,
        id: Option<&str>,
        kind: Kind,
        frame: &[u8],
        extra: &[(&str, &str)],
    ) {
        let line = entry(at, dir, id, kind, frame, extra);
        if let Err(error) = self.append(line.as_bytes()) {
            self.complain(&error);
        }
    }

    fn append(&self, line: &[u8]) -> io::Result<()> {
        let mut slot = self
            .file
            .lock()
            .map_err(|_| io::Error::other("flow log lock poisoned"))?;
        if slot.is_none() {
            let mut sink = open(&self.path)?;
            if sink.size == 0 {
                // A new, empty or just-rotated file: it says what it is before anything else.
                let header = header(SystemTime::now());
                sink.file.write_all(header.as_bytes())?;
                sink.size = u64::try_from(header.len()).unwrap_or(u64::MAX);
            }
            *slot = Some(sink);
        }
        let Some(sink) = slot.as_mut() else {
            return Err(io::Error::other("flow log not open"));
        };
        let written = sink.file.write_all(line);
        sink.size = sink
            .size
            .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        let full = sink.size > self.rotate_at;
        if full {
            // Closed before the renames; the next write opens a fresh file.
            *slot = None;
        }
        let rotated = if full { rotate(&self.path) } else { Ok(()) };
        drop(slot);
        written.and(rotated)
    }

    fn complain(&self, error: &io::Error) {
        if self.complained.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(log) = &self.complain
            && let Ok(mut log) = ActivityLog::open(log)
        {
            let _noted = log.note(&format!(
                "flow: {} not written ({error}); further errors are not noted",
                self.path.display()
            ));
        }
    }
}

fn open(path: &Path) -> io::Result<Sink> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // An older file made with another mode is brought to 0600 too.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let size = file.metadata()?.len();
    Ok(Sink { file, size })
}

/// `flow.log` → `.1`, `.1` → `.2` … `.4` → `.5`; the old `.5` is deleted.
fn rotate(path: &Path) -> io::Result<()> {
    let numbered = |n: u32| {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    };
    match fs::remove_file(numbered(KEEP)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    for n in (1..KEEP).rev() {
        match fs::rename(numbered(n), numbered(n.saturating_add(1))) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    fs::rename(path, numbered(1))
}

static FLOW: OnceLock<FlowLog> = OnceLock::new();

/// Sets the process-wide log, once; a second call changes nothing and returns `false`.
///
/// The log is the file at `path` (its directory created on the first write); its first I/O error is
/// noted to the activity log at `activity_log`.
pub fn init(path: &Path, activity_log: &Path) -> bool {
    FLOW.set(FlowLog::new(
        path.to_path_buf(),
        ROTATE_AT,
        Some(activity_log.to_path_buf()),
    ))
    .is_ok()
}

/// The process-wide log, when [`init`] ran.
#[must_use]
pub fn global() -> Option<&'static FlowLog> {
    FLOW.get()
}

/// Logs one entry to the process-wide log; a no-op before [`init`].
pub fn record(dir: Dir, id: Option<&str>, kind: Kind, frame: &[u8], extra: &[(&str, &str)]) {
    if let Some(log) = FLOW.get() {
        log.write(dir, id, kind, frame, extra);
    }
}

/// Logs a frame read from or written to the command channel, its id taken from the frame.
pub fn frame(dir: Dir, kind: Kind, frame: &[u8]) {
    if FLOW.get().is_some() {
        record(dir, frame_id(frame).as_deref(), kind, frame, &[]);
    }
}

/// Logs something the link did not carry, as a `link` note.
///
/// `why` is a short machine word (`down`, `connect-failed`, `hello-refused`, `retry`, `stopped`,
/// `queued-offline`), `detail` the rest in words, `frame` the event concerned (`null` when there is
/// none), `extra` any further fields.
pub fn link(why: &str, detail: &str, frame: Option<&[u8]>, extra: &[(&str, &str)]) {
    if FLOW.get().is_some() {
        let mut fields = vec![("why", why), ("detail", detail)];
        fields.extend_from_slice(extra);
        record(
            Dir::Note,
            frame.and_then(frame_id).as_deref(),
            Kind::Link,
            frame.unwrap_or(b"null"),
            &fields,
        );
    }
}

/// The typed line's event with its `text` replaced by [`REDACTED`], everything else as it was; an
/// event that is not a JSON object is replaced whole — it cannot be told apart from its text.
#[must_use]
pub fn redact_typed(frame: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(frame) else {
        return format!("\"{REDACTED}\"").into_bytes();
    };
    if let Some(value) = object.get_mut("text") {
        *value = Value::from(REDACTED);
    }
    Value::Object(object).to_string().into_bytes()
}

/// Logs a hello written to the hub, its credentials redacted.
pub fn hello(frame: &[u8]) {
    if FLOW.get().is_some() {
        record(Dir::Out, None, Kind::Hello, &redact_hello(frame), &[]);
    }
}

/// The hello with its credentials replaced by [`REDACTED`], everything else as it was.
///
/// The credentials are `token`, `data` (a data stream's ticket) and `secret` (a link stream's). A
/// hello that is not a JSON object is replaced whole: it cannot be told apart from its credentials.
#[must_use]
pub fn redact_hello(frame: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(frame) else {
        return format!("\"{REDACTED}\"").into_bytes();
    };
    for key in ["token", "data", "secret"] {
        if let Some(value) = object.get_mut(key) {
            *value = Value::from(REDACTED);
        }
    }
    Value::Object(object).to_string().into_bytes()
}

/// The frame's top-level `id` when it is a string or a number.
fn frame_id(frame: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Id {
        id: Option<Value>,
    }
    match serde_json::from_slice::<Id>(frame).ok()?.id? {
        Value::String(id) => Some(id),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// One entry, newline-terminated.
fn entry(
    at: SystemTime,
    dir: Dir,
    id: Option<&str>,
    kind: Kind,
    frame: &[u8],
    extra: &[(&str, &str)],
) -> String {
    let mut line = String::with_capacity(frame.len().saturating_add(128));
    line.push_str("{\"ts\":");
    line.push_str(&Value::from(timestamp(at)).to_string());
    line.push_str(",\"dir\":\"");
    line.push_str(dir.as_str());
    line.push_str("\",\"id\":");
    line.push_str(&id.map_or(Value::Null, Value::from).to_string());
    line.push_str(",\"kind\":\"");
    line.push_str(kind.as_str());
    line.push_str("\",\"frame\":");
    push_frame(&mut line, frame);
    for (key, value) in extra {
        line.push(',');
        line.push_str(&Value::from(*key).to_string());
        line.push(':');
        line.push_str(&Value::from(*value).to_string());
    }
    line.push_str("}\n");
    line
}

/// The header line, newline-terminated: `{"ts","dir":"note","id":null,"kind":"header","text"}`.
fn header(at: SystemTime) -> String {
    format!(
        "{{\"ts\":{},\"dir\":\"note\",\"id\":null,\"kind\":\"header\",\"text\":{}}}\n",
        Value::from(timestamp(at)),
        Value::from(HEADER_TEXT)
    )
}

/// The frame as JSON: verbatim when it is one JSON value on one line, re-serialised when it is JSON
/// spread over lines, else as a string.
fn push_frame(line: &mut String, frame: &[u8]) {
    let frame = frame
        .strip_suffix(b"\n")
        .map_or(frame, |f| f.strip_suffix(b"\r").unwrap_or(f));
    if let Ok(text) = std::str::from_utf8(frame)
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
    {
        if text.contains(['\n', '\r']) {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                line.push_str(&value.to_string());
                return;
            }
        } else {
            line.push_str(text.trim());
            return;
        }
    }
    line.push_str(&Value::from(String::from_utf8_lossy(frame)).to_string());
}

/// UTC RFC 3339 with milliseconds, e.g. `2026-11-22T10:00:00.123Z`.
fn timestamp(at: SystemTime) -> String {
    let millis = at
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .subsec_millis();
    format_utc(at).map_or_else(
        || "1970-01-01T00:00:00.000Z".to_owned(),
        |utc| format!("{}.{millis:03}Z", utc.trim_end_matches('Z')),
    )
}

/// What [`LoggedPipe`] stamps its lines with: the wall clock, or a test's.
pub type Clock<'c> = dyn Fn() -> SystemTime + Sync + 'c;

/// The wall clock.
const WALL_CLOCK: &Clock<'static> = &SystemTime::now;

/// The command channel's pipe with every frame through it logged.
///
/// What is sent is logged as `out`, as the kind [`LoggedPipe::next_out`] last set; what is read as
/// `in`, a request, or `fate` when the frame says so. Keepalive pings and their replies are
/// summarised ([`Keepalive`]) unless `TULPA_FLOW_KEEPALIVE=all`; a pending summary is written when
/// the pipe is dropped.
pub struct LoggedPipe<'a> {
    inner: &'a mut dyn Pipe,
    next_out: Kind,
    log: Option<&'a FlowLog>,
    clock: &'a Clock<'a>,
    keepalive: Option<Keepalive>,
}

impl fmt::Debug for LoggedPipe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoggedPipe")
            .field("next_out", &self.next_out)
            .field("keepalive", &self.keepalive)
            .finish_non_exhaustive()
    }
}

impl<'a> LoggedPipe<'a> {
    /// Wraps `inner`, logging to the process-wide log; its first frame out is the hello.
    /// `TULPA_FLOW_KEEPALIVE` is read here, once.
    pub fn new(inner: &'a mut dyn Pipe) -> Self {
        let all = std::env::var(KEEPALIVE_ENV).is_ok_and(|value| value == "all");
        Self::with(inner, global(), WALL_CLOCK, all)
    }

    /// Wraps `inner`, logging to `log` stamped by `clock`; `keepalive_all` logs every keepalive
    /// frame instead of summaries.
    pub(crate) fn with(
        inner: &'a mut dyn Pipe,
        log: Option<&'a FlowLog>,
        clock: &'a Clock<'a>,
        keepalive_all: bool,
    ) -> Self {
        Self {
            inner,
            next_out: Kind::Hello,
            log,
            clock,
            keepalive: (!keepalive_all).then(Keepalive::default),
        }
    }

    /// What the next frame sent is logged as.
    pub const fn next_out(&mut self, kind: Kind) {
        self.next_out = kind;
    }
}

impl Pipe for LoggedPipe<'_> {
    fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        if let Some(log) = self.log {
            let now = (self.clock)();
            match self.next_out {
                Kind::Hello => {
                    log.write_at(now, Dir::Out, None, Kind::Hello, &redact_hello(bytes), &[]);
                }
                Kind::Typed => {
                    log.write_at(now, Dir::Out, None, Kind::Typed, &redact_typed(bytes), &[]);
                }
                kind => {
                    let id = frame_id(bytes);
                    let pong = kind == Kind::Reply
                        && is_ok_reply(bytes)
                        && self
                            .keepalive
                            .as_mut()
                            .is_some_and(|keepalive| keepalive.reply(id.as_deref(), now));
                    if !pong {
                        log.write_at(now, Dir::Out, id.as_deref(), kind, bytes, &[]);
                    }
                }
            }
        }
        self.inner.send(bytes)
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<Recv, LinkError> {
        let got = self.inner.recv(timeout);
        if let (Some(log), Ok(Recv::Line(line))) = (self.log, &got) {
            let now = (self.clock)();
            let kind = incoming_kind(line);
            let id = frame_id(line);
            let ping = kind == Kind::Request && is_ping(line);
            match self.keepalive.as_mut() {
                Some(keepalive) if ping => {
                    for fields in keepalive.ping(id, now) {
                        write_keepalive(log, now, &fields);
                    }
                }
                _ => log.write_at(now, Dir::In, id.as_deref(), kind, line, &[]),
            }
        }
        got
    }

    fn waker(&self) -> crate::link::Waker {
        self.inner.waker()
    }
}

impl Drop for LoggedPipe<'_> {
    fn drop(&mut self) {
        if let (Some(log), Some(keepalive)) = (self.log, self.keepalive.as_mut())
            && let Some(fields) = keepalive.summary()
        {
            let now = (self.clock)();
            write_keepalive(log, now, &fields);
        }
    }
}

/// Writes one keepalive note, its fields as extra fields.
fn write_keepalive(log: &FlowLog, at: SystemTime, fields: &[(&str, String)]) {
    let extra: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    log.write_at(at, Dir::Note, None, Kind::Keepalive, b"null", &extra);
}

/// A reply carrying `"ok":true`: only those can be summarised as pongs, a failed answer to a ping is news.
fn is_ok_reply(line: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Ok {
        ok: Option<Value>,
    }
    serde_json::from_slice::<Ok>(line).is_ok_and(|v| v.ok.is_some_and(|ok| ok == true))
}

/// A request whose `verb` is exactly `ping`.
fn is_ping(line: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Verb {
        verb: Option<Value>,
    }
    serde_json::from_slice::<Verb>(line).is_ok_and(|v| v.verb.is_some_and(|verb| verb == "ping"))
}

/// One connection's keepalive tally: pings in, our replies to them out, since the last summary.
#[derive(Debug, Default)]
struct Keepalive {
    /// Pings not yet answered, oldest first: id and arrival.
    in_flight: VecDeque<(String, SystemTime)>,
    pings: u64,
    pongs: u64,
    last_reply_ms: Option<u128>,
    /// The first ping counted since the last summary.
    since: Option<SystemTime>,
    /// The last summary, or the first ping when there has been none.
    window: Option<SystemTime>,
    last_ping: Option<SystemTime>,
}

impl Keepalive {
    /// Counts a ping that arrived `at`; returns the lines due now (a summary, a gap), as fields.
    fn ping(&mut self, id: Option<String>, at: SystemTime) -> Vec<Vec<(&'static str, String)>> {
        let mut lines = Vec::new();
        let window = *self.window.get_or_insert(at);
        if at.duration_since(window).unwrap_or(Duration::ZERO) >= KEEPALIVE_SUMMARY {
            lines.extend(self.summary());
            self.window = Some(at);
        }
        if let Some(previous) = self.last_ping {
            let gap = at.duration_since(previous).unwrap_or(Duration::ZERO);
            if gap > KEEPALIVE_GAP {
                lines.push(vec![("gap_s", gap.as_secs().to_string())]);
            }
        }
        self.last_ping = Some(at);
        self.pings = self.pings.saturating_add(1);
        self.since.get_or_insert(at);
        if let Some(id) = id {
            if self.in_flight.len() >= KEEPALIVE_IN_FLIGHT {
                self.in_flight.pop_front();
            }
            self.in_flight.push_back((id, at));
        }
        lines
    }

    /// Whether a reply with `id`, sent `at`, answers a ping in flight; if so it is counted.
    fn reply(&mut self, id: Option<&str>, at: SystemTime) -> bool {
        let Some(index) = id.and_then(|id| self.in_flight.iter().position(|(ping, _)| ping == id))
        else {
            return false;
        };
        let Some((_, pinged)) = self.in_flight.remove(index) else {
            return false;
        };
        self.pongs = self.pongs.saturating_add(1);
        self.last_reply_ms = Some(
            at.duration_since(pinged)
                .unwrap_or(Duration::ZERO)
                .as_millis(),
        );
        true
    }

    /// The summary of what was counted since the last one, the counts reset; `None` when nothing was.
    fn summary(&mut self) -> Option<Vec<(&'static str, String)>> {
        if self.pings == 0 && self.pongs == 0 {
            return None;
        }
        let mut fields = vec![
            ("pings", self.pings.to_string()),
            ("pongs", self.pongs.to_string()),
        ];
        if let Some(ms) = self.last_reply_ms {
            fields.push(("last_reply_ms", ms.to_string()));
        }
        if let Some(since) = self.since {
            fields.push(("since", timestamp(since)));
        }
        self.pings = 0;
        self.pongs = 0;
        self.last_reply_ms = None;
        self.since = None;
        Some(fields)
    }
}

/// A frame from the hub is a request, unless it is a `fate` frame (`"type":"fate"`), reserved for a
/// later change on the hub's side and only logged.
fn incoming_kind(line: &[u8]) -> Kind {
    #[derive(serde::Deserialize)]
    struct Type {
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    match serde_json::from_slice::<Type>(line) {
        Ok(Type { kind: Some(kind) }) if kind == "fate" => Kind::Fate,
        _ => Kind::Request,
    }
}

/// Test support: the process-wide log, set once to a file under the temp directory for this test
/// process, so every test module's entries can be read back (filter by a unique id or text).
#[cfg(test)]
pub(crate) fn test_global() -> &'static FlowLog {
    FLOW.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tulpa-flow-test-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        FlowLog::new(dir.join(FILE_NAME), ROTATE_AT, None)
    })
}

/// Test support: the value at the JSON pointer `pointer` in `value`, null when there is none.
#[cfg(test)]
pub(crate) fn fl(value: &Value, pointer: &str) -> Value {
    value.pointer(pointer).cloned().unwrap_or(Value::Null)
}

/// Test support: the `n`th entry, null when there is none.
#[cfg(test)]
pub(crate) fn nth(entries: &[Value], n: usize) -> Value {
    entries.get(n).cloned().unwrap_or(Value::Null)
}

/// Test support: every entry in the test log that satisfies `keep`.
#[cfg(test)]
pub(crate) fn test_entries(keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    let text = fs::read_to_string(test_global().path()).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| keep(entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse(line: &str) -> Result<Value, serde_json::Error> {
        serde_json::from_str(line)
    }

    #[test]
    fn an_entry_carries_the_frame_whole_and_as_json() -> TestResult {
        let at = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let line = entry(
            at,
            Dir::In,
            Some("r1"),
            Kind::Request,
            b"{\"id\":\"r1\",\"verb\":\"x\",\"n\":1.50}\n",
            &[],
        );
        assert!(line.ends_with("}\n") && line.matches('\n').count() == 1);
        assert!(
            line.contains("\"frame\":{\"id\":\"r1\",\"verb\":\"x\",\"n\":1.50}"),
            "a JSON frame goes in verbatim: {line}"
        );
        let value = parse(&line)?;
        assert_eq!(fl(&value, "/ts"), "2023-11-14T22:13:20.123Z");
        assert_eq!(fl(&value, "/dir"), "in");
        assert_eq!(fl(&value, "/id"), "r1");
        assert_eq!(fl(&value, "/kind"), "request");
        assert_eq!(fl(&value, "/frame/verb"), "x");
        Ok(())
    }

    #[test]
    fn a_frame_that_is_not_json_is_a_string_and_no_id_is_null() -> TestResult {
        let line = entry(
            SystemTime::now(),
            Dir::Out,
            None,
            Kind::Event,
            b"not json {\n",
            &[("outcome", "sent")],
        );
        let value = parse(&line)?;
        assert_eq!(fl(&value, "/frame"), "not json {");
        assert_eq!(fl(&value, "/id"), Value::Null);
        assert_eq!(fl(&value, "/outcome"), "sent");
        Ok(())
    }

    #[test]
    fn a_hello_loses_only_its_credentials() -> TestResult {
        let control = redact_hello(b"{\"hello\":\"dev-1\",\"token\":\"sekrit\"}\n");
        assert_eq!(
            serde_json::from_slice::<Value>(&control)?,
            serde_json::json!({"hello": "dev-1", "token": REDACTED})
        );
        let link = redact_hello(
            b"{\"hello\":\"dev-1\",\"token\":\"t\",\"link\":\"ab\",\"secret\":\"s\",\"stream\":0}\n",
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&link)?,
            serde_json::json!({"hello": "dev-1", "token": REDACTED, "link": "ab", "secret": REDACTED, "stream": 0})
        );
        let data =
            redact_hello(b"{\"hello\":\"d\",\"token\":\"t\",\"data\":\"tk\",\"offset\":42}\n");
        assert_eq!(
            serde_json::from_slice::<Value>(&data)?,
            serde_json::json!({"hello": "d", "token": REDACTED, "data": REDACTED, "offset": 42})
        );
        assert!(!String::from_utf8_lossy(&redact_hello(b"token=sekrit")).contains("sekrit"));
        Ok(())
    }

    #[test]
    fn past_the_threshold_it_rotates_and_keeps_five() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("logs").join(FILE_NAME);
        // 1000 bytes past the header, so the header's length does not move the rotations.
        let rotate_at = u64::try_from(header(SystemTime::now()).len())? + 1_000;
        let log = FlowLog::new(path.clone(), rotate_at, None);
        log.write(Dir::Out, None, Kind::Event, b"{\"event\":\"first\"}", &[]);
        assert!(path.is_file() && !dir.path().join("logs/flow.log.1").exists());
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        for n in 0..40 {
            log.write(
                Dir::Out,
                None,
                Kind::Event,
                format!(
                    "{{\"event\":\"e\",\"n\":{n},\"pad\":\"{}\"}}",
                    "x".repeat(400)
                )
                .as_bytes(),
                &[],
            );
        }
        // One more after the last rotation: the live file starts again.
        log.write(Dir::Out, None, Kind::Event, b"{\"event\":\"last\"}", &[]);
        let names: std::collections::BTreeSet<String> = fs::read_dir(dir.path().join("logs"))?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            "flow.log",
            "flow.log.1",
            "flow.log.2",
            "flow.log.3",
            "flow.log.4",
            "flow.log.5",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(names, expected, "rotated to .1 … .5 and no further");
        assert!(fs::read_to_string(&path)?.contains("\"last\""));
        let last = fs::read_to_string(dir.path().join("logs/flow.log.1"))?;
        assert!(last.lines().all(|l| parse(l).is_ok()), "whole lines only");
        Ok(())
    }

    fn first_line(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
        let text = fs::read_to_string(path)?;
        Ok(parse(text.lines().next().unwrap_or_default())?)
    }

    fn is_header(value: &Value) -> bool {
        fl(value, "/kind") == "header"
            && fl(value, "/dir") == "note"
            && fl(value, "/id") == Value::Null
            && fl(value, "/text") == HEADER_TEXT
            && fl(value, "/ts")
                .as_str()
                .is_some_and(|ts| ts.ends_with('Z'))
    }

    #[test]
    fn a_fresh_file_starts_with_the_header() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(FILE_NAME);
        let log = FlowLog::new(path.clone(), ROTATE_AT, None);
        log.write(Dir::Out, None, Kind::Event, b"{\"event\":\"one\"}", &[]);
        let header = first_line(&path)?;
        assert!(is_header(&header), "first line is the header: {header}");
        assert!(HEADER_TEXT.contains("\"<redacted>\"") && HEADER_TEXT.contains("Nothing else"));
        let text = fs::read_to_string(&path)?;
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().nth(1).is_some_and(|l| l.contains("\"one\"")));
        // An empty file that already exists is fresh too.
        let empty = dir.path().join("empty.log");
        File::create(&empty)?;
        FlowLog::new(empty.clone(), ROTATE_AT, None).write(
            Dir::In,
            None,
            Kind::Request,
            b"{}",
            &[],
        );
        assert!(is_header(&first_line(&empty)?));
        Ok(())
    }

    #[test]
    fn reopening_a_file_with_lines_adds_no_header() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(FILE_NAME);
        FlowLog::new(path.clone(), ROTATE_AT, None).write(Dir::Out, None, Kind::Event, b"{}", &[]);
        // A second process (a restarted tray) appends to the same file.
        FlowLog::new(path.clone(), ROTATE_AT, None).write(Dir::Out, None, Kind::Event, b"{}", &[]);
        let text = fs::read_to_string(&path)?;
        let headers = text
            .lines()
            .filter_map(|l| parse(l).ok())
            .filter(is_header)
            .count();
        assert_eq!((headers, text.lines().count()), (1, 3), "{text}");
        Ok(())
    }

    #[test]
    fn after_a_rotation_the_new_file_starts_with_the_header() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(FILE_NAME);
        let log = FlowLog::new(path.clone(), 600, None);
        let big = format!("{{\"event\":\"big\",\"pad\":\"{}\"}}", "x".repeat(400));
        log.write(Dir::Out, None, Kind::Event, big.as_bytes(), &[]);
        assert!(dir.path().join("flow.log.1").is_file(), "rotated");
        log.write(Dir::Out, None, Kind::Event, b"{\"event\":\"after\"}", &[]);
        let header = first_line(&path)?;
        assert!(
            is_header(&header),
            "the new flow.log starts with the header: {header}"
        );
        assert!(fs::read_to_string(&path)?.contains("\"after\""));
        assert!(is_header(&first_line(&dir.path().join("flow.log.1"))?));
        Ok(())
    }

    #[test]
    fn an_unwritable_log_never_panics() {
        let log = FlowLog::new(PathBuf::from("/proc/no-such/flow.log"), ROTATE_AT, None);
        log.write(Dir::Out, None, Kind::Event, b"{}", &[]);
        log.write(Dir::Out, None, Kind::Event, b"{}", &[]);
        assert!(log.complained.load(Ordering::SeqCst));
    }

    /// A pipe that hands out `incoming` one line per `recv`, then `Eof`.
    struct FakePipe {
        incoming: VecDeque<Vec<u8>>,
    }

    impl Pipe for FakePipe {
        fn send(&mut self, _bytes: &[u8]) -> Result<(), LinkError> {
            Ok(())
        }

        fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
            Ok(self.incoming.pop_front().map_or(Recv::Eof, Recv::Line))
        }

        fn waker(&self) -> crate::link::Waker {
            std::sync::Arc::new(|| {}) // `recv` never waits
        }
    }

    /// A test clock: seconds past a fixed start, set by the test.
    struct TestClock(std::sync::atomic::AtomicU64);

    impl TestClock {
        const START: u64 = 1_800_000_000;

        fn now(&self) -> SystemTime {
            let seconds = Self::START.saturating_add(self.0.load(Ordering::SeqCst));
            UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .unwrap_or(UNIX_EPOCH)
        }

        fn set(&self, seconds: u64) {
            self.0.store(seconds, Ordering::SeqCst);
        }
    }

    /// One keepalive exchange at `at` seconds: the hub's ping `n` in, our pong out a moment later.
    fn exchange(pipe: &mut LoggedPipe<'_>, clock: &TestClock, at: u64, n: u64) -> TestResult {
        clock.set(at);
        let got = pipe.recv(None)?;
        assert_eq!(got, Recv::Line(ping(n)));
        clock.set(at.saturating_add(1));
        pipe.next_out(Kind::Reply);
        pipe.send(format!("{{\"id\":\"{n}\",\"ok\":true,\"result\":\"pong\"}}\n").as_bytes())?;
        Ok(())
    }

    fn ping(n: u64) -> Vec<u8> {
        format!("{{\"id\":\"{n}\",\"verb\":\"ping\"}}").into_bytes()
    }

    /// Every line of the log at `path` but the header.
    fn lines(path: &Path) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let text = fs::read_to_string(path).unwrap_or_default();
        Ok(text
            .lines()
            .map(parse)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|line| !is_header(line))
            .collect())
    }

    fn keepalives(lines: &[Value]) -> Vec<Value> {
        lines
            .iter()
            .filter(|line| fl(line, "/kind") == "keepalive")
            .cloned()
            .collect()
    }

    /// A log in a temp dir, a clock at 0 and a pipe that will read `incoming`.
    fn setup(
        incoming: Vec<Vec<u8>>,
    ) -> Result<(tempfile::TempDir, FlowLog, TestClock, FakePipe), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let log = FlowLog::new(dir.path().join(FILE_NAME), ROTATE_AT, None);
        let pipe = FakePipe {
            incoming: incoming.into(),
        };
        Ok((dir, log, TestClock(0.into()), pipe))
    }

    #[test]
    fn keepalives_within_ten_minutes_are_one_summary_line() -> TestResult {
        let (_dir, log, clock, mut inner) = setup((1..=11).map(ping).collect())?;
        let now = || clock.now();
        let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
        for n in 1..=10 {
            exchange(&mut pipe, &clock, (n - 1) * 60, n)?;
        }
        assert_eq!(
            lines(log.path())?,
            Vec::<Value>::new(),
            "no line per ping or pong"
        );
        // The 11th ping comes at 10 min: the summary of the ten before it.
        exchange(&mut pipe, &clock, 600, 11)?;
        let written = lines(log.path())?;
        assert_eq!(written.len(), 1, "{written:?}");
        let summary = nth(&written, 0);
        assert_eq!(fl(&summary, "/kind"), "keepalive");
        assert_eq!(fl(&summary, "/dir"), "note");
        assert_eq!(fl(&summary, "/id"), Value::Null);
        assert_eq!(fl(&summary, "/pings"), "10");
        assert_eq!(fl(&summary, "/pongs"), "10");
        assert_eq!(fl(&summary, "/last_reply_ms"), "1000");
        assert_eq!(
            fl(&summary, "/since"),
            timestamp(UNIX_EPOCH + Duration::from_secs(TestClock::START))
        );
        assert_eq!(
            fl(&summary, "/ts"),
            timestamp(clock.now() - Duration::from_secs(1))
        );
        Ok(())
    }

    #[test]
    fn a_ping_after_a_silence_is_a_gap_line_at_once() -> TestResult {
        let (_dir, log, clock, mut inner) = setup(vec![ping(1), ping(2), ping(3)])?;
        let now = || clock.now();
        let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
        exchange(&mut pipe, &clock, 0, 1)?;
        exchange(&mut pipe, &clock, 150, 2)?;
        assert_eq!(lines(log.path())?.len(), 0, "150 s is not a gap");
        exchange(&mut pipe, &clock, 350, 3)?;
        let written = keepalives(&lines(log.path())?);
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(fl(&nth(&written, 0), "/gap_s"), "200");
        assert_eq!(
            fl(&nth(&written, 0), "/pings"),
            Value::Null,
            "a gap is not a summary"
        );
        Ok(())
    }

    #[test]
    fn other_requests_and_replies_are_logged_in_full_between_pings() -> TestResult {
        let shell = b"{\"id\":\"s1\",\"verb\":\"shell\",\"args\":[\"ping\"]}".to_vec();
        let (_dir, log, clock, mut inner) = setup(vec![ping(1), shell, ping(2)])?;
        let now = || clock.now();
        let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
        exchange(&mut pipe, &clock, 0, 1)?;
        pipe.recv(None)?;
        pipe.send(b"{\"id\":\"s1\",\"ok\":true,\"result\":\"pong\"}\n")?;
        exchange(&mut pipe, &clock, 60, 2)?;
        let written = lines(log.path())?;
        let shape: Vec<(Value, Value, Value)> = written
            .iter()
            .map(|l| (fl(l, "/dir"), fl(l, "/id"), fl(l, "/kind")))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("in".into(), "s1".into(), "request".into()),
                ("out".into(), "s1".into(), "reply".into()),
            ]
        );
        assert_eq!(fl(&nth(&written, 0), "/frame/args/0"), "ping");
        Ok(())
    }

    #[test]
    fn a_pong_with_an_unknown_id_is_logged() -> TestResult {
        let (_dir, log, clock, mut inner) = setup(vec![ping(1)])?;
        let now = || clock.now();
        let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
        exchange(&mut pipe, &clock, 0, 1)?;
        // Answered already: a second pong for 1, and one for an id never pinged.
        pipe.send(b"{\"id\":\"1\",\"ok\":true,\"result\":\"pong\"}\n")?;
        pipe.send(b"{\"id\":\"99\",\"ok\":true,\"result\":\"pong\"}\n")?;
        let written = lines(log.path())?;
        let ids: Vec<Value> = written.iter().map(|l| fl(l, "/id")).collect();
        assert_eq!(ids, vec![Value::from("1"), Value::from("99")]);
        assert!(written.iter().all(|l| fl(l, "/kind") == "reply"));
        Ok(())
    }

    #[test]
    fn a_failed_answer_to_a_ping_is_logged() -> TestResult {
        let (_dir, log, clock, mut inner) = setup(vec![ping(1)])?;
        let now = || clock.now();
        let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
        let _got = pipe.recv(None)?;
        pipe.next_out(Kind::Reply);
        pipe.send(b"{\"id\":\"1\",\"ok\":false,\"err\":\"busy\"}\n")?;
        let written = lines(log.path())?;
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(fl(&nth(&written, 0), "/kind"), "reply");
        assert_eq!(fl(&nth(&written, 0), "/id"), "1");
        Ok(())
    }

    #[test]
    fn keepalive_all_logs_every_frame() -> TestResult {
        let (_dir, log, clock, mut inner) = setup((1..=3).map(ping).collect())?;
        let now = || clock.now();
        {
            let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, true);
            for n in 1..=3 {
                exchange(&mut pipe, &clock, n * 400, n)?;
            }
        }
        let written = lines(log.path())?;
        let shape: Vec<(Value, Value)> = written
            .iter()
            .map(|l| (fl(l, "/id"), fl(l, "/kind")))
            .collect();
        let expected: Vec<(Value, Value)> = (1..=3)
            .flat_map(|n| {
                [
                    (n.to_string().into(), "request".into()),
                    (n.to_string().into(), "reply".into()),
                ]
            })
            .collect();
        assert_eq!(shape, expected, "every frame, no summary, no gap");
        Ok(())
    }

    #[test]
    fn dropping_the_pipe_writes_the_pending_summary() -> TestResult {
        let (_dir, log, clock, mut inner) = setup(vec![ping(1), ping(2), ping(3)])?;
        let now = || clock.now();
        {
            let mut pipe = LoggedPipe::with(&mut inner, Some(&log), &now, false);
            exchange(&mut pipe, &clock, 0, 1)?;
            exchange(&mut pipe, &clock, 60, 2)?;
            clock.set(120);
            pipe.recv(None)?;
            assert_eq!(lines(log.path())?.len(), 0);
        }
        let written = lines(log.path())?;
        assert_eq!(written.len(), 1, "{written:?}");
        let summary = nth(&written, 0);
        assert_eq!(fl(&summary, "/kind"), "keepalive");
        assert_eq!(
            (fl(&summary, "/pings"), fl(&summary, "/pongs")),
            ("3".into(), "2".into())
        );
        // A pipe that saw no keepalive writes nothing when dropped.
        let mut idle = FakePipe {
            incoming: VecDeque::new(),
        };
        drop(LoggedPipe::with(&mut idle, Some(&log), &now, false));
        assert_eq!(lines(log.path())?.len(), 1);
        Ok(())
    }
}
