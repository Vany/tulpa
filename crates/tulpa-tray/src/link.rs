//! The dial-out link: one OpenSSH child carrying the body's line protocol to the
//! broker, reconnected with backoff, honest about its state.
//!
//! The Mac listens on nothing (`AGENTS.md`): `ssh -W` forwards the child's stdio
//! to one TCP endpoint on the SSH server side — the same shape as the phone's
//! dial-out. The first line is the token hello; the broker closes the connection
//! during the grace window when the token is wrong, which is how «connected»
//! stays truthful. Every command line goes to the line handler: the hands
//! organ when a policy is loaded, otherwise a refusal with `{"err":"verb"}`.
//!
//! Events: the body may also speak unasked. An organ pushes a complete line
//! into the shared [`Outbox`]; the serving side writes queued lines after the
//! hello is accepted and between replies, at most one serve tick (500 ms) after
//! the push. An event line carries no `id` and names its organ, e.g.
//! `{"event":"ears","state":"deaf","reason":"…"}`. Lines queued while the link
//! is down wait for the next accepted hello; the queue keeps the newest
//! [`OUTBOX_MAX`] lines.
//!
//! The data stream: a transfer the hands accepted dials its own connection
//! ([`SshDataDial`]): a second `ssh -W` child, the same command on the route the
//! link is on, whose first line is the data hello
//! `{"hello":"<device>","token":"<token>","data":"<ticket>","offset":n}`.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tulpa_hands::{Closer, DataConn, DataDial, HalfWrite, Ticket};
use tulpa_stop::{Attempts, Stop};

use crate::checkin::CheckIn;
use crate::flow::{Kind, LoggedPipe};
use crate::log::ActivityLog;
use crate::state::LinkState;

/// The token alphabet, mirroring the phone's token check: a token with
/// anything outside this set is refused before it reaches the wire.
const TOKEN_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=_-";
/// The broker refuses tokens shorter than this; the same floor applies locally.
const TOKEN_MIN_LEN: usize = 16;
/// The longest command line answered (a 4 KiB cap made a hands `write` of a few KB end the
/// attempt, and the broker's redial resent it — the Mac flapped every ~16 s). Above the broker's own cap on a hands
/// line (12 MiB) plus framing; a longer one is refused, not dropped.
const LINE_MAX_BYTES: usize = 16 * 1024 * 1024;
/// How long the pipe may stay silent and open after the hello before the broker
/// is considered to have accepted it.
const HELLO_GRACE: Duration = Duration::from_secs(3);
/// Why an [`SshPipe`] breaks when its stdout reader ends without the end-of-stream marker.
const READER_LOST: &str = "the stdout reader thread ended without an end-of-stream marker";
/// How long one frame may take from hand-off to the writer's ack before the
/// send counts as stalled. The broker's own contract answers a ping within
/// 15 s, so a healthy link never trips this; a child that stopped reading
/// backs up the pipe and would otherwise park the caller forever — the
/// worker-loop hang of 2026-11-18.
const SEND_STALL: Duration = Duration::from_secs(30);
/// How many frames may wait for the writer thread. More than one outstanding
/// only happens when the writer already stalled, and the stall bound ends the
/// wait; the bound exists so a stalled writer cannot move the wait into
/// memory as an endless queue.
const FRAME_QUEUE: usize = 8;
/// How long a send waits between tries on a full queue.
const FRAME_WAIT: Duration = Duration::from_millis(10);
/// How often [`DataChild::linger`] looks whether the ssh child exited within its grace. Still a
/// poll: the child is shared behind a lock with `close`, so no thread can block in its `wait`
/// (a later step of the poll inventory).
const REST_CHUNK: Duration = Duration::from_millis(100);
/// One line of ssh's stderr is logged up to this size, then truncated.
const STDERR_NOTE_MAX: usize = 300;
/// The reply to any command line while no organ owns the protocol.
const REFUSAL: &[u8] = b"{\"err\":\"verb\"}\n";
/// How many unsolicited ears/`files` event lines the [`Outbox`] keeps; older ones are dropped.
pub const OUTBOX_MAX: usize = 16;
/// #0423 (akira F2): how many typed lines the [`Outbox`]'s own lane keeps — the operator's mail, never
/// evicted by ears traffic, dropped oldest-first among typed lines only, each drop said.
pub const TYPED_MAX: usize = 32;
/// #0423 (akira F2): at most this many typed lines on the wire per [`TYPED_RATE_WINDOW`].
///
/// The agent door's own rate (guild `src/dispatcher/tulpa-text.ts` `RATE_MAX`/`RATE_WINDOW_MS`,
/// mirrored here so the coupling stays greppable across the repos). Lines past it stay queued until
/// the window lets them go, never dropped, never lied about.
pub const TYPED_RATE_MAX: usize = 12;
/// The window [`TYPED_RATE_MAX`] is counted over (guild `RATE_WINDOW_MS`, 60 s).
pub const TYPED_RATE_WINDOW: Duration = Duration::from_secs(60);
/// #0423 (akira T3): how many typed lines the tray writes per [`TYPED_RATE_WINDOW`] — one under the
/// hub's [`TYPED_RATE_MAX`] of 12.
///
/// The tray counts its own send instants, the hub counts arrivals: a line the network delays past the
/// window's edge (the first after a reconnect, a retransmit) lands in the hub's count beside the next
/// window's lines. One line of margin keeps such a line inside the hub's 12; a 12th written on the
/// window's edge would be the hub's 13th, dropped there and logged only, while the window said it went.
pub const TYPED_PACE_MAX: usize = TYPED_RATE_MAX - 1;

/// A connection, configuration, or protocol failure. Reasons are safe for the
/// local activity log: the token value itself is never part of them.
#[derive(Debug)]
#[non_exhaustive]
pub enum LinkError {
    /// Local configuration is unusable; retrying will not fix it.
    Config(String),
    /// The token file is missing, unreadable, or holds an unusable token.
    Token(String),
    /// The `ssh` child could not be started.
    Spawn(io::Error),
    /// Writing to the pipe failed.
    Send(io::Error),
    /// The pipe broke while reading.
    Recv(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(reason) => write!(formatter, "link configuration: {reason}"),
            Self::Token(reason) => write!(formatter, "link token: {reason}"),
            Self::Spawn(error) => write!(formatter, "ssh could not start: {error}"),
            Self::Send(error) => write!(formatter, "writing to the link failed: {error}"),
            Self::Recv(reason) => write!(formatter, "reading the link failed: {reason}"),
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Send(error) => Some(error),
            Self::Config(_) | Self::Token(_) | Self::Recv(_) => None,
        }
    }
}

/// Everything needed to keep one dialled link alive.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DialSpec {
    alias: String,
    target_host: String,
    target_port: u16,
    device: String,
    token_file: PathBuf,
    office_ssids: Vec<String>,
    direct_hosts: Vec<String>,
}

/// Which way an attempt dials: straight to the server on the office LAN, or through the jump host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Route {
    /// `ssh` to this LAN address, no jump.
    Direct(String),
    /// `ssh` through the alias as configured (its `ProxyJump`).
    Jump,
}

/// The route the link is on now, shared with the data dial so a transfer
/// goes the same way; `None` before the first connection.
pub type CurrentRoute = Arc<Mutex<Option<Route>>>;

/// The routes to try, in order: on an office Wi-Fi each direct address then the jump; elsewhere the jump only.
#[must_use]
pub fn routes(ssid: Option<&str>, office_ssids: &[String], direct_hosts: &[String]) -> Vec<Route> {
    let office = ssid.is_some_and(|s| office_ssids.iter().any(|o| o == s));
    let mut out: Vec<Route> = if office {
        direct_hosts.iter().cloned().map(Route::Direct).collect()
    } else {
        Vec::new()
    };
    out.push(Route::Jump);
    out
}

/// The current Wi-Fi network name (macOS `ipconfig getsummary en0`), if any.
#[must_use]
pub fn current_ssid() -> Option<String> {
    let out = Command::new("/usr/sbin/ipconfig")
        .args(["getsummary", "en0"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_ssid(&String::from_utf8_lossy(&out.stdout))
}

/// The ` SSID : <name>` line of `ipconfig getsummary`.
#[must_use]
pub fn parse_ssid(summary: &str) -> Option<String> {
    summary.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "SSID")
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

impl DialSpec {
    /// Validates the dial settings from local configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty alias or target host, a zero port, a device
    /// name outside `[A-Za-z0-9_-]` (the hello is built without JSON escaping, so
    /// the charset is the invariant), or an empty token path.
    pub fn new(
        alias: &str,
        target_host: &str,
        target_port: u16,
        device: &str,
        token_file: PathBuf,
    ) -> Result<Self, LinkError> {
        if alias.is_empty() {
            return Err(LinkError::Config(
                "link.host is empty; set the OpenSSH host alias".to_owned(),
            ));
        }
        if target_host.is_empty()
            || target_host
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(LinkError::Config(
                "link target host is empty or holds whitespace/control bytes".to_owned(),
            ));
        }
        if target_port == 0 {
            return Err(LinkError::Config(
                "link target port is 0; set the broker port".to_owned(),
            ));
        }
        let device_ok = !device.is_empty()
            && device.len() <= 32
            && device
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if !device_ok {
            return Err(LinkError::Config(
                "link device must be 1-32 bytes of [A-Za-z0-9_-]".to_owned(),
            ));
        }
        if token_file.as_os_str().is_empty() {
            return Err(LinkError::Config(
                "link token file path is empty".to_owned(),
            ));
        }
        Ok(Self {
            alias: alias.to_owned(),
            target_host: target_host.to_owned(),
            target_port,
            device: device.to_owned(),
            token_file,
            office_ssids: Vec::new(),
            direct_hosts: Vec::new(),
        })
    }

    /// The office Wi-Fi names and the server's LAN addresses for the direct route.
    #[must_use]
    pub fn with_direct(mut self, office_ssids: Vec<String>, direct_hosts: Vec<String>) -> Self {
        self.office_ssids = office_ssids;
        self.direct_hosts = direct_hosts;
        self
    }

    /// OpenSSH host alias, resolved by the user's own `ssh` configuration.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Endpoint dialed on the server side; localhost there is the broker.
    #[must_use]
    pub fn target(&self) -> (&str, u16) {
        (&self.target_host, self.target_port)
    }

    /// Device name carried by the hello line.
    #[must_use]
    pub fn device(&self) -> &str {
        &self.device
    }

    /// File holding the shared token, read fresh before every attempt.
    #[must_use]
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }
}

/// Reads the shared token: the first line of the file, trimmed, with the
/// protocol's charset and length floor enforced before anything reaches the wire.
///
/// # Errors
///
/// Returns an error when the file cannot be read or the token fails validation.
pub fn read_token(path: &Path) -> Result<String, LinkError> {
    let raw = fs::read_to_string(path)
        .map_err(|error| LinkError::Token(format!("{} is unreadable: {error}", path.display())))?;
    let token = raw.lines().next().map(str::trim).unwrap_or_default();
    if token.len() < TOKEN_MIN_LEN {
        return Err(LinkError::Token(format!(
            "token in {} is shorter than {TOKEN_MIN_LEN} bytes",
            path.display()
        )));
    }
    if !token.bytes().all(|byte| TOKEN_CHARS.contains(&byte)) {
        return Err(LinkError::Token(format!(
            "token in {} has bytes outside the protocol alphabet",
            path.display()
        )));
    }
    Ok(token.to_owned())
}

/// Builds the first line on the wire: `{"hello":"<device>","token":"<token>"}`.
/// Both values are pre-validated to alphabets without quotes or backslashes, so
/// no JSON escaping is applied or needed.
#[must_use]
pub fn hello_line(device: &str, token: &str) -> Vec<u8> {
    let mut line =
        String::with_capacity(device.len().saturating_add(token.len()).saturating_add(24));
    line.push_str("{\"hello\":\"");
    line.push_str(device);
    line.push_str("\",\"token\":\"");
    line.push_str(token);
    line.push_str("\"}\n");
    line.into_bytes()
}

/// Builds the data stream's hello line.
///
/// It carries the control hello's device and token, the hub's ticket and, for
/// a put, the Mac's own `.part` size. Device and token are validated as for
/// [`hello_line`] and a [`Ticket`] has a quote-free alphabet, so no escaping
/// is needed.
#[must_use]
pub fn data_hello_line(device: &str, token: &str, ticket: &Ticket, offset: Option<u64>) -> Vec<u8> {
    let mut line = hello_line(device, token);
    // `{"hello":…,"token":"…"}\n` loses its `}\n` and takes the data fields.
    line.truncate(line.len().saturating_sub(2));
    line.extend_from_slice(b",\"data\":\"");
    line.extend_from_slice(ticket.reveal().as_bytes());
    line.push(b'"');
    if let Some(offset) = offset {
        line.extend_from_slice(format!(",\"offset\":{offset}").as_bytes());
    }
    line.extend_from_slice(b"}\n");
    line
}

/// Builds a link stream's hello line (#0375).
///
/// The control hello's device and token, the link's id, the side's secret and the stream number (0:
/// the side's control stream). The id and the secret are the hub's own hex alphabet, so no escaping
/// is needed; the secret is said on the wire and never logged.
#[must_use]
pub fn link_hello_line(
    device: &str,
    token: &str,
    link: &str,
    secret: &str,
    stream: u32,
) -> Vec<u8> {
    let mut hello = hello_line(device, token);
    hello.truncate(hello.len().saturating_sub(2));
    hello.extend_from_slice(b",\"link\":\"");
    hello.extend_from_slice(link.as_bytes());
    hello.extend_from_slice(b"\",\"secret\":\"");
    hello.extend_from_slice(secret.as_bytes());
    hello.extend_from_slice(format!("\",\"stream\":{stream}}}\n").as_bytes());
    hello
}

/// Retry delays after failed attempts: 5 s doubling to a 5 min cap, the phone's
/// ladder. An attempt that reached «connected» starts the ladder over.
#[derive(Debug)]
pub struct Backoff {
    next_secs: u64,
}

const BACKOFF_START_SECS: u64 = 5;
const BACKOFF_CAP_SECS: u64 = 300;

impl Backoff {
    /// The first wait after the first failure.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_secs: BACKOFF_START_SECS,
        }
    }

    /// The wait for the failure just seen; advances the ladder to its cap.
    pub fn advance(&mut self) -> Duration {
        let secs = self.next_secs;
        self.next_secs = self.next_secs.saturating_mul(2).min(BACKOFF_CAP_SECS);
        Duration::from_secs(secs)
    }

    /// A connection reached «connected»: the next failure waits 5 s again.
    pub const fn reset(&mut self) {
        self.next_secs = BACKOFF_START_SECS;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// One received frame from the broker side of the pipe.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Recv {
    /// One complete line without its newline.
    Line(Vec<u8>),
    /// The peer closed the pipe cleanly.
    Eof,
    /// Nothing arrived within the requested window.
    Timeout,
}

/// How one dial attempt ended; the reason is fit for the local log.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum End {
    /// The broker closed the pipe during the hello grace: the token is unknown
    /// or wrong, or the server refused the forward.
    HelloRefused,
    /// The pipe ended after the hello was accepted.
    Dropped,
    /// The operator asked for shutdown.
    Stopped,
    /// The pipe broke.
    Io(String),
}

impl fmt::Display for End {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelloRefused => formatter.write_str(
                "the broker closed the connection during the hello (unknown or wrong token)",
            ),
            Self::Dropped => formatter.write_str("the connection ended"),
            Self::Stopped => formatter.write_str("stopped by the operator"),
            Self::Io(reason) => write!(formatter, "the link pipe broke: {reason}"),
        }
    }
}

/// One end of a dialled attempt: lines in, lines out.
pub trait Pipe: Send {
    /// Writes one complete frame; the caller includes the trailing newline.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError>;

    /// Receives one frame. `None` waits until a line, the pipe's end, or the
    /// [`Pipe::waker`] (an outbox push, or the operator's stop), which returns
    /// [`Recv::Timeout`]: a chance to re-check the stop and flush the outbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the pipe breaks.
    fn recv(&mut self, timeout: Option<Duration>) -> Result<Recv, LinkError>;

    /// A handle that ends the `recv` waiting now (or the next one) early with
    /// [`Recv::Timeout`]; [`serve_attempt`] gives it to the [`Outbox`] and to the stop,
    /// so a pushed line goes out at once and a stop ends the attempt at once.
    fn waker(&self) -> Waker;
}

/// Wakes a [`Pipe`]'s `recv`; see [`Pipe::waker`]. The same kind a [`Stop`] runs on its set.
pub type Waker = tulpa_stop::Waker;

/// Runs one full dial attempt: read the token, open the pipe, say the hello,
/// serve command lines until the pipe ends.
pub trait Dialer {
    /// Performs one attempt, calling `on_connected` exactly when the hello grace
    /// passes with the pipe still open.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt cannot even start (token, spawn); a
    /// started attempt reports its end through [`End`] instead.
    fn attempt(&self, on_connected: &mut dyn FnMut(), stop: &Stop) -> Result<End, LinkError>;
}

/// Answers one command line with one reply line; owned by the hands organ.
pub type LineHandler = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Unsolicited lines (events) waiting for the serving side to write them.
///
/// Two lanes (#0423, akira F2): the ears/`files` lane, bounded at [`OUTBOX_MAX`] drop-oldest, and the
/// typed lane — the operator's mail — bounded at [`TYPED_MAX`] among typed lines only, so ears traffic
/// can never evict a typed line. The typed lane drains first, paced one under the agent door's own
/// rate ([`TYPED_PACE_MAX`] per [`TYPED_RATE_WINDOW`]); a line the window does not allow yet stays
/// queued, and the serving loop wakes for it when the window lets it go ([`Outbox::typed_due`]). The wire's word for
/// each line the window knows by id — a typed line, a heard phrase — written, or lost to a full lane or
/// a broken attempt, reaches the window through the [`WireNews`] callback, so it never says more than
/// the wire did.
pub struct Outbox {
    /// The ears/`files` lane: each line with the window's id for it, when the window shows it.
    lines: Mutex<VecDeque<(Option<u64>, Vec<u8>)>>,
    typed: Mutex<TypedLane>,
    on_wire: Mutex<Option<WireSink>>,
    /// The serving attempt's pipe waker, set while an attempt is serving (hello accepted, not yet
    /// ended); a line queued while it is `None` is noted to `flow.log` as queued offline.
    waker: Mutex<Option<Waker>>,
}

/// Where the wire's word for a line goes: the window's callback ([`Outbox::set_on_wire`]).
pub(crate) type WireSink = Box<dyn Fn(WireNews) + Send + Sync>;

impl Default for Outbox {
    fn default() -> Self {
        Self {
            lines: Mutex::new(VecDeque::new()),
            typed: Mutex::new(TypedLane::new()),
            on_wire: Mutex::new(None),
            waker: Mutex::new(None),
        }
    }
}

/// Lane depths only: the callback holds nothing printable, and a typed line's text is nobody's log line.
impl fmt::Debug for Outbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines = held(&self.lines).len();
        let typed = self.lane().queue.len();
        formatter
            .debug_struct("Outbox")
            .field("lines", &lines)
            .field("typed", &typed)
            .finish_non_exhaustive()
    }
}

/// The typed lane's own state: its queue — each line with the id the window knows it by — and the
/// pacer's send instants. Its Debug says how many lines wait, never their bytes (#0423 r3 S3, P2: a
/// poisoned-lock message, an assert or a `dbg!` must not print the operator's line).
struct TypedLane {
    queue: VecDeque<(u64, Vec<u8>)>,
    sends: VecDeque<Instant>,
    /// The production numbers are the consts; tests shrink the window to milliseconds so the pacing is
    /// exercised without a minute of wall time.
    rate_max: usize,
    rate_window: Duration,
}

impl fmt::Debug for TypedLane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypedLane")
            .field("queued", &self.queue.len())
            .field("sends", &self.sends)
            .field("rate_max", &self.rate_max)
            .field("rate_window", &self.rate_window)
            .finish()
    }
}

impl TypedLane {
    const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            sends: VecDeque::new(),
            rate_max: TYPED_PACE_MAX,
            rate_window: TYPED_RATE_WINDOW,
        }
    }

    /// Forgets the send instants that left the window at `now`, so the window slides.
    fn slide(&mut self, now: Instant) {
        while self
            .sends
            .front()
            .is_some_and(|front| now.duration_since(*front) >= self.rate_window)
        {
            self.sends.pop_front();
        }
    }

    /// The next line, taken off the lane when the window lets it go at `now`; `None` when the lane
    /// is empty or the window is full. Its send instant is recorded by [`TypedLane::sent`] once
    /// written; a line not written goes back ([`TypedLane::unsent`]).
    fn next(&mut self, now: Instant) -> Option<(u64, Vec<u8>)> {
        if self.wait(now) > Duration::ZERO {
            return None;
        }
        self.queue.pop_front()
    }

    /// A line was written at `at`: the window counts it.
    fn sent(&mut self, at: Instant) {
        self.sends.push_back(at);
    }

    /// A line taken by [`TypedLane::next`] was not written: it goes back to the front, uncounted.
    fn unsent(&mut self, line: (u64, Vec<u8>)) {
        self.queue.push_front(line);
    }

    /// How long until the window lets the next line go: zero when it does now.
    fn wait(&mut self, now: Instant) -> Duration {
        self.slide(now);
        if self.sends.len() < self.rate_max {
            return Duration::ZERO;
        }
        self.sends.front().map_or(Duration::ZERO, |oldest| {
            self.rate_window.saturating_sub(now.duration_since(*oldest))
        })
    }
}

/// What the wire said about one line the window knows by id (#0423, akira F2; heard phrases since r2):
/// written, or lost.
///
/// By the id it was queued with ([`Outbox::push_typed`], [`Outbox::push_heard`]); the window (saylog)
/// turns this into its mark, and the text is never read back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WireNews {
    /// The line was written to this connection's pipe — all the tray can know: the hub sends no word
    /// back (guild `tulpa-text.ts` for a typed line).
    Written(u64),
    /// Its lane was full; this oldest line was dropped and never left.
    Evicted(u64),
    /// The pipe broke with this ears-lane line taken for the attempt and not written: it went with the
    /// attempt. (A typed line goes back to its lane instead and stays queued.)
    Broken(u64),
}

/// A lock of the operator's lines, poisoned or not (#0423 r2; the window's log too since r3): the
/// outbox's lanes and the window's [`crate::saylog::SayLog`]. Nothing panics while holding one, and
/// one that refused its own lock after a panic elsewhere would stop or blank its lines for good with
/// no word. Every lock of them goes through here.
pub(crate) fn held<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Marks the outbox online, its pushes waking `pipe`, for as long as it lives.
struct Online<'a>(&'a Outbox);

impl<'a> Online<'a> {
    fn start(outbox: &'a Outbox, pipe: &dyn Pipe) -> Self {
        if let Ok(mut waker) = outbox.waker.lock() {
            *waker = Some(pipe.waker());
        }
        Self(outbox)
    }
}

impl Drop for Online<'_> {
    fn drop(&mut self) {
        if let Ok(mut waker) = self.0.waker.lock() {
            *waker = None;
        }
    }
}

impl Outbox {
    /// Queues one ears/`files` line, a missing trailing newline added, and wakes the serving
    /// attempt (if any) to write it.
    pub fn push(&self, line: Vec<u8>) {
        self.queue(None, line);
    }

    /// Queues one heard phrase's event as [`Outbox::push`] does, under the window's `id` for it
    /// ([`crate::saylog::SayLog::heard`]): the wire's word for it — written, evicted, broken — reaches
    /// the window as [`WireNews`].
    pub fn push_heard(&self, id: u64, line: Vec<u8>) {
        self.queue(Some(id), line);
    }

    fn queue(&self, id: Option<u64>, mut line: Vec<u8>) {
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        let wake = self.wake();
        if wake.is_none() {
            crate::flow::link(
                "queued-offline",
                "the link is not up: the event waits in the outbox",
                Some(&line),
                &[],
            );
        }
        let mut evicted = Vec::new();
        {
            let mut lines = held(&self.lines);
            while lines.len() >= OUTBOX_MAX {
                match lines.pop_front() {
                    Some(oldest) => evicted.push(oldest),
                    None => break,
                }
            }
            lines.push_back((id, line));
        }
        for (oldest, dropped) in evicted {
            crate::flow::frame(crate::flow::Dir::Note, crate::flow::Kind::Drop, &dropped);
            if let Some(oldest) = oldest {
                self.news(WireNews::Evicted(oldest));
            }
        }
        if let Some(wake) = wake {
            wake();
        }
    }

    /// The serving attempt's waker, if one is serving.
    fn wake(&self) -> Option<Waker> {
        self.waker.lock().ok().and_then(|waker| waker.clone())
    }

    /// Tests only: shrink the rate window to milliseconds — the pacing is exercised without a minute of
    /// wall time; the production numbers are [`TYPED_PACE_MAX`]/[`TYPED_RATE_WINDOW`].
    #[cfg(test)]
    pub(crate) fn set_rate_for_test(&self, max: usize, window: Duration) {
        let mut lane = self.lane();
        lane.rate_max = max;
        lane.rate_window = window;
    }

    /// The typed lane, poisoned or not ([`held`]): every lock of it goes through here.
    fn lane(&self) -> MutexGuard<'_, TypedLane> {
        held(&self.typed)
    }

    /// Queues one typed line — the operator's mail, its own lane (#0423, akira F2) — as `id`, and wakes
    /// the serving attempt (if any). The lane is bounded among typed lines only: an evicted oldest line
    /// is said as [`WireNews::Evicted`] and noted to `flow.log` without its text; the queuer never
    /// decides a fate, the wire does.
    pub fn push_typed(&self, id: u64, mut event: Vec<u8>) {
        if event.last() != Some(&b'\n') {
            event.push(b'\n');
        }
        let mut evicted = Vec::new();
        {
            let mut lane = self.lane();
            while lane.queue.len() >= TYPED_MAX {
                match lane.queue.pop_front() {
                    Some(oldest) => evicted.push(oldest),
                    None => break,
                }
            }
            lane.queue.push_back((id, event));
        }
        for (oldest, line) in evicted {
            crate::flow::record(
                crate::flow::Dir::Note,
                None,
                Kind::Typed,
                &crate::flow::redact_typed(&line),
                &[("outcome", "evicted")],
            );
            self.news(WireNews::Evicted(oldest));
        }
        if let Some(wake) = self.wake() {
            wake();
        }
    }

    /// Where the wire's word for a line the window knows by id goes ([`crate::saylog::attach`]). Set
    /// once, at startup, where the saylog exists; it is called with no lane lock of the outbox's held.
    pub fn set_on_wire(&self, news: WireSink) {
        if let Ok(mut slot) = self.on_wire.lock() {
            *slot = Some(news);
        }
    }

    fn news(&self, what: WireNews) {
        let Ok(slot) = self.on_wire.lock() else {
            return;
        };
        if let Some(tell) = slot.as_ref() {
            tell(what);
        }
    }

    /// Takes every queued ears/`files` line, oldest first, without their window ids: a reader that is
    /// not the wire (tests, mostly) says nothing to the window.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        self.take_lines()
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    fn take_lines(&self) -> Vec<(Option<u64>, Vec<u8>)> {
        held(&self.lines).drain(..).collect()
    }

    /// Tests only: one flush into a pipe that takes every line; the lines it took.
    #[cfg(test)]
    pub(crate) fn test_flush(&self) -> Vec<Vec<u8>> {
        struct Sink(Vec<Vec<u8>>);
        impl Pipe for Sink {
            fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                self.0.push(bytes.to_vec());
                Ok(())
            }
            fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
                Ok(Recv::Eof)
            }
            fn waker(&self) -> Waker {
                Arc::new(|| {})
            }
        }
        let mut sink = Sink(Vec::new());
        let _end = self.flush(&mut LoggedPipe::new(&mut sink));
        sink.0
    }

    /// How many typed lines wait in the lane.
    #[must_use]
    pub fn typed_waiting(&self) -> usize {
        self.lane().queue.len()
    }

    /// How long the serving loop may wait before a held typed line is due: `None` while the typed lane
    /// is empty (nothing to time — a push wakes the loop). No event says "the window slid", so this
    /// deadline is the one timed wake the loop takes.
    fn typed_due(&self) -> Option<Duration> {
        let mut lane = self.lane();
        if lane.queue.is_empty() {
            return None;
        }
        Some(lane.wait(Instant::now()))
    }

    /// Writes both lanes to `pipe`: the typed lane first, under its rate window — what the window does
    /// not allow yet stays queued ([`Outbox::typed_due`]) — then every ears/`files` line, never held
    /// behind typed pacing. `Some` means the pipe broke and the attempt is over: a typed line that did
    /// not leave goes back to the front of its lane for the next connection, unsaid; the unsent
    /// ears/`files` lines go with the attempt, each one the window knows said as [`WireNews::Broken`].
    fn flush(&self, pipe: &mut LoggedPipe<'_>) -> Option<End> {
        pipe.next_out(Kind::Typed);
        loop {
            let next = self.lane().next(Instant::now());
            let Some((id, line)) = next else { break };
            if let Err(error) = pipe.send(&line) {
                self.lane().unsent((id, line));
                return Some(End::Io(error.to_string()));
            }
            self.lane().sent(Instant::now());
            self.news(WireNews::Written(id));
        }
        pipe.next_out(Kind::Event);
        let mut lines = self.take_lines().into_iter();
        while let Some((id, line)) = lines.next() {
            if let Err(error) = pipe.send(&line) {
                // This line and every one still behind it go with the attempt.
                let gone = std::iter::once(id).chain(lines.map(|(id, _)| id));
                for gone in gone.flatten() {
                    self.news(WireNews::Broken(gone));
                }
                return Some(End::Io(error.to_string()));
            }
            if let Some(id) = id {
                self.news(WireNews::Written(id));
            }
        }
        None
    }
}

/// The reply to a command line while no organ owns the protocol: a refusal
/// that echoes the request's id.
#[must_use]
pub fn refuse_line(line: &[u8]) -> Vec<u8> {
    refusal_for(line)
}

/// Serves one attempt over a pipe: says the hello, then answers command lines
/// until the pipe ends or the operator stops. `on_connected` fires exactly once,
/// when the hello is considered accepted.
///
/// Lines queued in `outbox` go out after the hello is accepted, then as soon
/// as they are pushed (the push wakes the wait on the pipe).
///
/// This is the command channel's one choke point: the pipe is wrapped in a
/// [`LoggedPipe`] here, so the hello (credentials redacted), every line read,
/// every reply and every event land in `flow.log` ([`crate::flow`]).
pub fn serve_attempt(
    pipe: &mut dyn Pipe,
    hello: &[u8],
    grace: Duration,
    stop: &Stop,
    on_connected: &mut dyn FnMut(),
    serve: &dyn Fn(&[u8]) -> Vec<u8>,
    outbox: &Outbox,
) -> End {
    let pipe = &mut LoggedPipe::new(pipe);
    if let Err(error) = pipe.send(hello) {
        return End::Io(error.to_string());
    }
    // Stop wakes the wait on the pipe as an outbox push does, the hello's grace included; nothing else needs a
    // tick: lines, the pipe's end and a lost reader each arrive on the pipe itself.
    let _stop_wakes = stop.on_set(&pipe.waker());
    let _online;
    match pipe.recv(Some(grace)) {
        Ok(Recv::Eof) => return End::HelloRefused,
        // A stop's wake and the grace's end are both a timeout: the flag tells them apart. A stopped attempt is
        // never taken as connected, and a line in that window is not served.
        Ok(Recv::Timeout | Recv::Line(_)) if stop.is_set() => return End::Stopped,
        Ok(Recv::Timeout) => {
            _online = Online::start(outbox, pipe);
            on_connected();
        }
        Ok(Recv::Line(line)) => {
            _online = Online::start(outbox, pipe);
            on_connected();
            if let Some(end) = serve_line(pipe, &line, serve) {
                return end;
            }
        }
        Err(error) => return End::Io(error.to_string()),
    }
    loop {
        if let Some(end) = outbox.flush(pipe) {
            return end;
        }
        if stop.is_set() {
            return End::Stopped;
        }
        // A typed line the rate window holds is the one thing due without a push: the wait ends when
        // the window lets it go (#0423).
        match pipe.recv(outbox.typed_due()) {
            Ok(Recv::Timeout) => {}
            Ok(Recv::Eof) => return End::Dropped,
            Ok(Recv::Line(line)) => {
                if let Some(end) = serve_line(pipe, &line, serve) {
                    return end;
                }
            }
            Err(error) => return End::Io(error.to_string()),
        }
    }
}

/// The broker matches replies to in-flight requests BY ID, and pings every
/// 60 s expecting an id-carrying reply within 15 s — an id-less refusal is
/// counted as noise and the ping never clears, so the broker drops the link
/// (~75 s) and the supervisor flaps forever. The refusal therefore echoes the
/// request's id whenever the line carried one; a line without an id still gets
/// the bare refusal (from a server-side read of the phone broker).
/// The refusal of a line over [`LINE_MAX_BYTES`], echoing its id when the line carried one.
fn too_long_for(line: &[u8]) -> Vec<u8> {
    let err = format!("line over {LINE_MAX_BYTES} bytes");
    extract_id(line).map_or_else(
        || format!("{{\"err\":\"{err}\"}}\n").into_bytes(),
        |id| {
            let mut reply = b"{\"id\":\"".to_vec();
            reply.extend_from_slice(&id);
            reply.extend_from_slice(format!("\",\"ok\":false,\"err\":\"{err}\"}}\n").as_bytes());
            reply
        },
    )
}

fn refusal_for(line: &[u8]) -> Vec<u8> {
    extract_id(line).map_or_else(
        || REFUSAL.to_vec(),
        |id| {
            let mut reply = Vec::with_capacity(id.len().saturating_add(24));
            reply.extend_from_slice(b"{\"id\":\"");
            reply.extend_from_slice(&id);
            reply.extend_from_slice(b"\",\"err\":\"verb\"}\n");
            reply
        },
    )
}

/// The value of a top-level `"id":"…"` pair, if the line carries exactly
/// parseable shape and a sane (printable, unescaped, ≤ 64-byte) value. This is
/// a scanner, not a JSON parser: it must never panic and never copy bytes out
/// of a hostile line beyond the id itself.
fn extract_id(line: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"\"id\"";
    let hit = line.windows(KEY.len()).position(|w| w == KEY)?;
    let at = hit.checked_add(KEY.len())?;
    let rest = line
        .get(at..)?
        .trim_ascii_start()
        .strip_prefix(b":")?
        .trim_ascii_start();
    let quoted = rest.strip_prefix(b"\"")?;
    let end = quoted.iter().position(|&b| b == b'"')?;
    let id = quoted.get(..end)?;
    if id.is_empty()
        || id.len() > 64
        || !id.iter().all(|b| (0x21..=0x7e).contains(b) && *b != b'\\')
    {
        return None;
    }
    Some(id.to_vec())
}

/// Answers one command line; `Some` means the attempt is over.
fn serve_line(
    pipe: &mut LoggedPipe<'_>,
    line: &[u8],
    serve: &dyn Fn(&[u8]) -> Vec<u8>,
) -> Option<End> {
    // An overlong line is refused with its id (the broker then answers its caller), never a reason to end the
    // attempt: a dropped link is redialled and the same line sent again, forever.
    let reply = if line.len() > LINE_MAX_BYTES {
        too_long_for(line)
    } else {
        serve(line)
    };
    pipe.next_out(Kind::Reply);
    match pipe.send(&reply) {
        Ok(()) => None,
        Err(error) => Some(End::Io(error.to_string())),
    }
}

/// One dialled attempt over the user's own OpenSSH.
///
/// `BatchMode` so a missing passphrase or host key fails the attempt instead
/// of prompting on the desktop, host keys checked strictly (unknown or changed
/// keys fail, never auto-accepted), keepalives so a dead route ends the child
/// instead of hanging, and `-W` so the child speaks to one endpoint on the
/// server side — no listening port on the Mac.
pub struct SshDialer {
    spec: DialSpec,
    log_path: PathBuf,
    handler: Option<LineHandler>,
    liveness: Arc<Attempts>,
    outbox: Arc<Outbox>,
    route: Arc<Mutex<String>>,
    current: CurrentRoute,
}

impl fmt::Debug for SshDialer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshDialer")
            .field("spec", &self.spec)
            .field("log_path", &self.log_path)
            .field("handler", &self.handler.is_some())
            .field("liveness", &self.liveness)
            .field("outbox", &self.outbox)
            .field("route", &self.route_line())
            .field("current", &self.current)
            .finish()
    }
}

impl SshDialer {
    /// Binds a dialer to a validated spec and the activity log the attempt's
    /// `ssh` stderr is noted to. Command lines are refused until a handler is
    /// attached.
    #[must_use]
    pub fn new(spec: DialSpec, log_path: PathBuf) -> Self {
        Self {
            spec,
            log_path,
            handler: None,
            liveness: Arc::default(),
            outbox: Arc::new(Outbox::default()),
            route: Arc::new(Mutex::new(String::new())),
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// Shares the route the link is on with the data dial ([`SshDataDial`]).
    #[must_use]
    pub fn with_current_route(mut self, current: CurrentRoute) -> Self {
        self.current = current;
        self
    }

    /// Shares the route line the menu shows («link: direct (…)» / «link: via …»).
    #[must_use]
    pub fn with_route_line(mut self, route: Arc<Mutex<String>>) -> Self {
        self.route = route;
        self
    }

    fn route_line(&self) -> String {
        self.route
            .lock()
            .map(|line| line.clone())
            .unwrap_or_default()
    }

    fn set_route_line(&self, line: String) {
        if let Ok(mut held) = self.route.lock() {
            *held = line;
        }
    }

    fn note(&self, message: &str) {
        if let Ok(mut log) = ActivityLog::open(&self.log_path) {
            let _noted: Result<(), crate::log::LogError> = log.note(message);
        }
    }

    /// Hands every command line to `handler` instead of refusing it.
    #[must_use]
    pub fn with_handler(mut self, handler: LineHandler) -> Self {
        self.handler = Some(handler);
        self
    }

    /// Shares the pipe's liveness through `attempts`: each pipe opened is a new
    /// attempt there, its stop set the moment that pipe's stdout ends (the ssh
    /// child died or the route dropped) and never cleared. A running hands
    /// child watches the stop of the attempt it started under and is killed
    /// when it is set, so a dead link never leaves an orphaned `run`, and a
    /// redial never makes that kill read as something else.
    #[must_use]
    pub fn with_liveness(mut self, attempts: Arc<Attempts>) -> Self {
        self.liveness = attempts;
        self
    }

    /// Writes the lines organs push into `outbox` while an attempt is served
    /// (see the module docs for the event shape).
    #[must_use]
    pub fn with_outbox(mut self, outbox: Arc<Outbox>) -> Self {
        self.outbox = outbox;
        self
    }
}

impl Dialer for SshDialer {
    /// On an office Wi-Fi the server is dialed directly (each LAN address in
    /// turn, 3 s each); if none comes up there the link WORRIES — a log line, an event to the agent, a
    /// warning in the menu — and goes through the jump host. Any other network goes through the jump.
    fn attempt(&self, on_connected: &mut dyn FnMut(), stop: &Stop) -> Result<End, LinkError> {
        let token = read_token(&self.spec.token_file)?;
        let hello = hello_line(self.spec.device(), &token);
        let ssid = current_ssid();
        let plan = routes(
            ssid.as_deref(),
            &self.spec.office_ssids,
            &self.spec.direct_hosts,
        );
        let office = plan.iter().any(|r| matches!(r, Route::Direct(_)));
        let mut last = End::Io("no route tried".to_owned());
        let mut direct_failed: Vec<String> = Vec::new();
        for route in plan {
            let mut connected = false;
            let label = match &route {
                Route::Direct(host) => format!("link: direct ({host})"),
                Route::Jump if !direct_failed.is_empty() => {
                    format!("link: via {} — ⚠ direct did not come up", self.spec.alias())
                }
                Route::Jump => format!("link: via {}", self.spec.alias()),
            };
            if !direct_failed.is_empty() && matches!(route, Route::Jump) {
                let why = format!(
                    "link: office network {} but the direct route failed ({}); going through {}",
                    ssid.as_deref().unwrap_or("?"),
                    direct_failed.join(", "),
                    self.spec.alias()
                );
                self.note(&why);
                let event = format!(
                    "{{\"event\":\"link\",\"state\":\"direct-failed\",\"tried\":{}}}",
                    serde_json::Value::from(direct_failed.clone())
                );
                self.outbox.push(event.into_bytes());
            }
            let end_mark = PipeEnd::open(&self.liveness);
            let mut pipe = match SshPipe::open(&self.spec, &self.log_path, end_mark, &route) {
                Ok(pipe) => pipe,
                Err(error) => {
                    if let Route::Direct(host) = &route {
                        direct_failed.push(host.clone());
                        continue;
                    }
                    return Err(error);
                }
            };
            let mut mark = || {
                connected = true;
                if let Ok(mut current) = self.current.lock() {
                    *current = Some(route.clone());
                }
                self.set_route_line(label.clone());
                self.note(&format!("link: on route {label}"));
                on_connected();
            };
            let end = match &self.handler {
                Some(handler) => serve_attempt(
                    &mut pipe,
                    &hello,
                    HELLO_GRACE,
                    stop,
                    &mut mark,
                    handler.as_ref(),
                    &self.outbox,
                ),
                None => serve_attempt(
                    &mut pipe,
                    &hello,
                    HELLO_GRACE,
                    stop,
                    &mut mark,
                    &refusal_for,
                    &self.outbox,
                ),
            };
            drop(pipe);
            match (&route, connected) {
                (Route::Direct(host), false) if office && end != End::Stopped => {
                    direct_failed.push(host.clone());
                    last = end;
                }
                _ => return Ok(end),
            }
        }
        Ok(last)
    }
}

/// `ssh -W target alias` on `route`, as every connection to the hub is
/// dialled: the link's own and each data connection.
fn ssh_command(spec: &DialSpec, route: &Route) -> Command {
    let (target_host, target_port) = spec.target();
    let mut ssh = ssh_base(route);
    ssh.arg("-W")
        .arg(format!("{target_host}:{target_port}"))
        .arg(spec.alias());
    ssh
}

/// `ssh` on `route` with the link's options (batch mode, strict host keys, keep-alives, forward failures fatal), the
/// alias and the request still to add: the link and the data stream add `-W`, a port forward its own (`forward.rs`).
pub(crate) fn ssh_base(route: &Route) -> Command {
    let mut ssh = Command::new("ssh");
    // The direct route: the same alias (user, key, host key) at the LAN address, no jump, 3 s to answer.
    if let Route::Direct(host) = route {
        ssh.arg("-o")
            .arg("ProxyJump=none")
            .arg("-o")
            .arg(format!("HostName={host}"))
            .arg("-o")
            .arg("ConnectTimeout=3");
    }
    ssh.arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=10")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("TCPKeepAlive=yes");
    ssh
}

/// The data dial: a second `ssh -W` child for each transfer.
///
/// The same dial spec and the route the link is on (the jump before the first
/// connection). The token is read fresh, as for the link; the hello is the
/// child's first line and is never logged; ssh's stderr is noted as the link's.
pub struct SshDataDial {
    spec: DialSpec,
    log_path: PathBuf,
    route: CurrentRoute,
}

impl fmt::Debug for SshDataDial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshDataDial")
            .field("spec", &self.spec)
            .field("log_path", &self.log_path)
            .finish_non_exhaustive()
    }
}

impl SshDataDial {
    /// Dials with `spec` on the route `route` holds (shared with the link's
    /// [`SshDialer::with_current_route`]).
    #[must_use]
    pub const fn new(spec: DialSpec, log_path: PathBuf, route: CurrentRoute) -> Self {
        Self {
            spec,
            log_path,
            route,
        }
    }
}

impl DataDial for SshDataDial {
    fn dial(&self, ticket: &Ticket, offset: Option<u64>) -> io::Result<DataConn> {
        let token =
            read_token(self.spec.token_file()).map_err(|e| io::Error::other(e.to_string()))?;
        let route = self
            .route
            .lock()
            .ok()
            .and_then(|current| current.clone())
            .unwrap_or(Route::Jump);
        let hello = data_hello_line(self.spec.device(), &token, ticket, offset);
        crate::flow::hello(&hello);
        spawn_data(ssh_command(&self.spec, &route), &self.log_path, &hello)
    }
}

/// A link stream's dialer (#0375).
///
/// A second `ssh -W` child per stream, like the data stream's, its hello naming the link, the
/// side's secret and the stream number (0: the side's control stream). The secret is said on the
/// wire and never logged.
pub struct SshLinkDial {
    spec: DialSpec,
    log_path: PathBuf,
    route: CurrentRoute,
}

impl fmt::Debug for SshLinkDial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshLinkDial")
            .field("spec", &self.spec)
            .field("log_path", &self.log_path)
            .finish_non_exhaustive()
    }
}

impl SshLinkDial {
    /// Dials with `spec` on the route `route` holds (shared with the link's
    /// [`SshDialer::with_current_route`]).
    #[must_use]
    pub const fn new(spec: DialSpec, log_path: PathBuf, route: CurrentRoute) -> Self {
        Self {
            spec,
            log_path,
            route,
        }
    }
}

impl crate::linkserve::LinkDial for SshLinkDial {
    fn dial(
        &self,
        link: &str,
        secret: &str,
        stream: u32,
    ) -> io::Result<crate::linkserve::StreamConn> {
        let token =
            read_token(self.spec.token_file()).map_err(|e| io::Error::other(e.to_string()))?;
        let route = self
            .route
            .lock()
            .ok()
            .and_then(|current| current.clone())
            .unwrap_or(Route::Jump);
        let hello = link_hello_line(self.spec.device(), &token, link, secret, stream);
        crate::flow::hello(&hello);
        let (writer, stdout, child) =
            spawn_stream_child(ssh_command(&self.spec, &route), &self.log_path, &hello)?;
        Ok(crate::linkserve::StreamConn {
            from_hub: Box::new(stdout),
            to_hub: Box::new(writer),
            child,
        })
    }
}

/// Starts `command` as a stream child: its stdin and stdout are the two
/// halves, `hello` its first line; closing it kills the child. The data and
/// link streams share it. The stdin belongs to a [`FrameWriter`] — the hello
/// is its first queued frame, so the same bound covers it as every later
/// frame (an unattended hello on a wedged server parked the whole worker
/// loop on 2026-11-18).
fn spawn_stream_child(
    mut command: Command,
    log_path: &Path,
    hello: &[u8],
) -> io::Result<(FrameWriter, std::process::ChildStdout, Arc<DataChild>)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _killed: io::Result<()> = child.kill();
        let _reaped: io::Result<std::process::ExitStatus> = child.wait();
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stream child has no stdio pipes",
        ));
    };
    let child = Arc::new(DataChild(Mutex::new(child)));
    let stderr_log = log_path.to_path_buf();
    thread::spawn(move || forward_stderr(stderr, &stderr_log));
    let kill = Arc::clone(&child);
    let mut writer = FrameWriter::new(Box::new(stdin), kill, SEND_STALL);
    writer.send(hello)?;
    Ok((writer, stdout, child))
}

/// Starts `command` as a data connection: its stdin and stdout are the two
/// halves, `hello` its first line; closing it kills the child. The data half
/// writes its stdin directly after the hello — unbounded, a carded advisory —
/// so the writer hands the target back once the hello is acked.
fn spawn_data(command: Command, log_path: &Path, hello: &[u8]) -> io::Result<DataConn> {
    let (writer, stdout, child) = spawn_stream_child(command, log_path, hello)?;
    let stdin = writer.into_inner()?;
    Ok(DataConn::new(
        Box::new(stdout),
        Box::new(DataStdin(Some(stdin))),
        child,
    ))
}

/// A data child's stdin; the half-close is closing it (ssh then sends EOF).
/// Writes go straight to the pipe — unbounded by design tonight; the same
/// stall medicine is carded for the data path.
struct DataStdin(Option<Box<dyn Write + Send>>);

impl Write for DataStdin {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.as_mut().map_or_else(
            || Err(io::Error::new(io::ErrorKind::BrokenPipe, "half-closed")),
            |stdin| stdin.write(buf),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl HalfWrite for DataStdin {
    fn half_close(&mut self) -> io::Result<()> {
        let result = self.flush();
        self.0 = None;
        result
    }
}

/// A data child: closing kills it; dropped, it is killed if still running and reaped.
struct DataChild(Mutex<Child>);

impl Closer for DataChild {
    fn close(&self) {
        if let Ok(mut child) = self.0.lock() {
            let _killed: io::Result<()> = child.kill();
        }
    }

    fn linger(&self, grace: Duration) {
        let started = std::time::Instant::now();
        while started.elapsed() < grace {
            let exited = self
                .0
                .lock()
                .map_or(true, |mut child| !matches!(child.try_wait(), Ok(None)));
            if exited {
                return;
            }
            thread::sleep(REST_CHUNK);
        }
        self.close();
    }
}

impl Drop for DataChild {
    fn drop(&mut self) {
        if let Ok(child) = self.0.get_mut() {
            SshPipe::teardown(child);
        }
    }
}

/// One frame bound for the child's stdin: the bytes to write whole, and the
/// channel the writer thread acks on once they have left (or failed).
enum Frame {
    /// Write the bytes, flush, ack.
    Write(Vec<u8>, mpsc::Sender<io::Result<()>>),
    /// Hand the target back to the caller and end the thread.
    Return(mpsc::Sender<Box<dyn Write + Send>>),
}

/// The writer thread's whole life: one frame at a time, whole, flushed,
/// acked. A write error ends it — the queue disconnects and later senders
/// fail fast instead of hanging on a dead child.
fn write_frames(mut target: Box<dyn Write + Send>, queue: &mpsc::Receiver<Frame>) {
    while let Ok(frame) = queue.recv() {
        match frame {
            Frame::Write(bytes, ack) => {
                let result = target.write_all(&bytes).and_then(|()| target.flush());
                let failed = result.is_err();
                let _ignored = ack.send(result);
                if failed {
                    return;
                }
            }
            Frame::Return(back) => {
                let _ignored = back.send(target);
                return;
            }
        }
    }
}

/// The error for a send that did not leave within the bound.
fn stalled_error(bound: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("send stalled over {bound:?}; the child was killed"),
    )
}

/// The owning writer for one child's stdin. Frames are handed to a dedicated
/// thread and acked when written, so every send is bounded: a child that
/// stops reading backs up the OS pipe and a plain `write_all` would park the
/// caller forever (every read was already timeout-capped; the writes were
/// not — that asymmetry was the worker-loop hang of 2026-11-18).
///
/// A stalled send does not merely fail: the frame may be half-written and the
/// channel has no resync, so the kill handle ends the child — SIGKILL, then
/// EPIPE unblocks the parked writer thread — and the connection dies loudly
/// for the caller to redial. Fail-and-continue on a torn frame is forbidden.
struct FrameWriter {
    queue: mpsc::SyncSender<Frame>,
    kill: Arc<dyn Closer>,
    bound: Duration,
    stalled: bool,
}

impl FrameWriter {
    /// Gives `target` to a writer thread; `kill` ends the child whose stdin
    /// `target` is when a send stalls.
    fn new(target: Box<dyn Write + Send>, kill: Arc<dyn Closer>, bound: Duration) -> Self {
        let (queue, incoming) = mpsc::sync_channel(FRAME_QUEUE);
        thread::spawn(move || write_frames(target, &incoming));
        Self {
            queue,
            kill,
            bound,
            stalled: false,
        }
    }

    /// Writes one complete frame and waits for the writer's ack, at most
    /// `bound` in total — queue wait and write wait together, so the timer
    /// measures the writer, not the queue.
    ///
    /// # Errors
    ///
    /// `TimedOut` when the frame did not leave within the bound (the child
    /// was killed; the connection is over); the writer's own error when the
    /// write failed; `BrokenPipe` when the writer has already ended.
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.stalled {
            return Err(stalled_error(self.bound));
        }
        let deadline = Instant::now()
            .checked_add(self.bound)
            .unwrap_or_else(Instant::now);
        let left = || deadline.saturating_duration_since(Instant::now());
        let (ack_at, acked) = mpsc::channel();
        // The queue is bounded; a full one only means the writer is already
        // stalled, and the deadline ends that wait like any other.
        let mut queued = Frame::Write(bytes.to_vec(), ack_at);
        loop {
            if left().is_zero() {
                let _gone = queued;
                return Err(self.stall());
            }
            match self.queue.try_send(queued) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(back)) => {
                    queued = back;
                    thread::sleep(left().min(FRAME_WAIT));
                }
                Err(mpsc::TrySendError::Disconnected(back)) => {
                    let _gone = back;
                    return Err(broken());
                }
            }
        }
        match acked.recv_timeout(left()) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self.stall()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(broken()),
        }
    }

    /// Takes the target back for a caller that writes it directly (the data
    /// connection's raw half). Only honest with no frame outstanding — the
    /// last `send` returned — so the idle writer hands the target over at
    /// once and the thread ends.
    ///
    /// # Errors
    ///
    /// `BrokenPipe` when the writer thread has already ended; its target died
    /// with it.
    fn into_inner(self) -> io::Result<Box<dyn Write + Send>> {
        let (back_at, back) = mpsc::channel();
        match self.queue.send(Frame::Return(back_at)) {
            Ok(()) => back.recv().map_or_else(|_| Err(broken()), Ok),
            Err(_) => Err(broken()),
        }
    }

    /// The frame did not leave within the bound: the connection is corrupt
    /// (a frame may be half-written; the channel has no resync), so the child
    /// dies — kill, EPIPE unblocks the parked writer — and every later send
    /// on this writer fails fast.
    fn stall(&mut self) -> io::Error {
        self.stalled = true;
        self.kill.close();
        stalled_error(self.bound)
    }
}

/// The writer is gone: a child that died EPIPEs its writer, which acks the
/// one in-flight frame and ends; later senders see this instead of hanging.
fn broken() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the write side ended")
}

impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf).map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // every frame is written whole and flushed before its ack
    }
}

/// The ssh child and its two reader threads, seen as a [`Pipe`]. The child
/// itself lives in the writer's kill handle: dropping the pipe drops the
/// last handle, which kills and reaps it.
struct SshPipe {
    writer: FrameWriter,
    lines: mpsc::Receiver<PipeFrame>,
    /// The reader's channel, for [`Pipe::waker`].
    wake: mpsc::Sender<PipeFrame>,
    saw_eof: bool,
}

impl SshPipe {
    /// Spawns `ssh -W target alias` with piped stdio; the child's stdout becomes
    /// the line channel and its stderr is noted to the activity log (capped,
    /// one line each) so a refused key or host key has a local trace. `dead`
    /// is cleared before the spawn and set the moment the stdout reader sees
    /// the pipe end, whatever serves it watches the same flag. The stdin goes
    /// to a [`FrameWriter`] so every frame the pipe sends is bounded.
    ///
    /// # Errors
    ///
    /// Returns an error when `ssh` cannot be spawned or its pipes are missing.
    fn open(
        spec: &DialSpec,
        log_path: &Path,
        dead: PipeEnd,
        route: &Route,
    ) -> Result<Self, LinkError> {
        let mut child = ssh_command(spec, route)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(LinkError::Spawn)?;
        let stdin = child.stdin.take().ok_or_else(|| {
            LinkError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh child has no stdin pipe",
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LinkError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh child has no stdout pipe",
            ))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            LinkError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh child has no stderr pipe",
            ))
        })?;

        let child = Arc::new(DataChild(Mutex::new(child)));
        let writer = FrameWriter::new(Box::new(stdin), child, SEND_STALL);
        let stderr_log = log_path.to_path_buf();
        thread::spawn(move || forward_stderr(stderr, &stderr_log));
        Ok(Self::new(writer, stdout, dead))
    }

    /// The pipe over `writer`, its lines read from `stdout` on a reader thread.
    fn new(writer: FrameWriter, stdout: impl io::Read + Send + 'static, dead: PipeEnd) -> Self {
        let (line_tx, lines) = mpsc::channel();
        let reader_tx = line_tx.clone();
        thread::spawn(move || forward_lines(stdout, &reader_tx, &dead));
        Self {
            writer,
            lines,
            wake: line_tx,
            saw_eof: false,
        }
    }

    /// Kills and reaps the ssh child, best effort.
    fn teardown(child: &mut Child) {
        let _ignored: io::Result<()> = child.kill();
        let _ignored: io::Result<std::process::ExitStatus> = child.wait();
    }

    /// Maps one channel frame to the public vocabulary; the EOF marker latches.
    fn frame_to_recv(&mut self, frame: PipeFrame) -> Result<Recv, LinkError> {
        match frame {
            PipeFrame::Line(line) => Ok(Recv::Line(line)),
            PipeFrame::Eof => {
                self.saw_eof = true;
                Ok(Recv::Eof)
            }
            PipeFrame::Wake => Ok(Recv::Timeout),
            PipeFrame::ReaderLost => Err(LinkError::Recv(READER_LOST.to_owned())),
        }
    }
}

impl Pipe for SshPipe {
    fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.writer.send(bytes).map_err(LinkError::Send)
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<Recv, LinkError> {
        if self.saw_eof {
            return Ok(Recv::Eof);
        }
        // The pipe holds a sender (for the waker), so the channel never disconnects; the
        // reader announces its own end with `PipeFrame::Eof` or `PipeFrame::ReaderLost`.
        let frame = match timeout {
            None => self.lines.recv().ok(),
            Some(window) => match self.lines.recv_timeout(window) {
                Ok(frame) => Some(frame),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(Recv::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
        };
        self.frame_to_recv(frame.unwrap_or(PipeFrame::ReaderLost))
    }

    fn waker(&self) -> Waker {
        let wake = self.wake.clone();
        Arc::new(move || {
            let _sent: Result<(), mpsc::SendError<PipeFrame>> = wake.send(PipeFrame::Wake);
        })
    }
}

/// What an [`SshPipe`]'s channel carries: the stdout reader's lines and its end, and the waker's
/// wake.
#[derive(Debug, Eq, PartialEq)]
enum PipeFrame {
    Line(Vec<u8>),
    /// The child's stdout ended (or failed to read): [`Recv::Eof`].
    Eof,
    /// [`Pipe::waker`]: [`Recv::Timeout`].
    Wake,
    /// The reader thread ended without [`PipeFrame::Eof`] (it panicked): the pipe is broken.
    ReaderLost,
}

/// The reader thread's last frame, sent when it drops, so the end is announced however the
/// thread ends: [`PipeFrame::Eof`] once `clean` is set, else [`PipeFrame::ReaderLost`] (a panic).
struct ReaderEnd<'a> {
    tx: &'a mpsc::Sender<PipeFrame>,
    clean: bool,
}

impl Drop for ReaderEnd<'_> {
    fn drop(&mut self) {
        let last = if self.clean {
            PipeFrame::Eof
        } else {
            PipeFrame::ReaderLost
        };
        let _sent: Result<(), mpsc::SendError<PipeFrame>> = self.tx.send(last);
    }
}

/// One attempt's own liveness stop: opening begins a new attempt in the
/// link's [`Attempts`]; the end sets this attempt's stop only, so a late EOF
/// from an old pipe never reads a live link as dead, and a new pipe never
/// reads a dead attempt as alive.
#[derive(Debug)]
struct PipeEnd {
    dead: Arc<Stop>,
}

impl PipeEnd {
    fn open(attempts: &Attempts) -> Self {
        Self {
            dead: attempts.begin(),
        }
    }

    fn mark_dead(&self) {
        self.dead.set();
    }
}

/// Reads newline-delimited frames from the child's stdout into the channel;
/// sets `dead` and sends [`PipeFrame::Eof`] once at the end so EOF is distinguishable
/// from a live pipe and whatever watches the flag sees the death first. A thread that
/// ends any other way (a panic) sends [`PipeFrame::ReaderLost`] instead ([`ReaderEnd`]).
fn forward_lines(stdout: impl io::Read, tx: &mpsc::Sender<PipeFrame>, dead: &PipeEnd) {
    let mut end = ReaderEnd { tx, clean: false };
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if tx.send(PipeFrame::Line(line)).is_err() {
                    break;
                }
            }
        }
    }
    dead.mark_dead();
    end.clean = true;
}

/// Notes one capped line of ssh's stderr to the activity log. The token and
/// private material never pass through ssh's stderr, but the line is capped
/// all the same.
fn forward_stderr(stderr: std::process::ChildStderr, log_path: &Path) {
    let Ok(mut log) = ActivityLog::open(log_path) else {
        return;
    };
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let capped: String = line.chars().take(STDERR_NOTE_MAX).collect();
        let suffix = if line.chars().count() > STDERR_NOTE_MAX {
            "…"
        } else {
            ""
        };
        let _noted: Result<(), crate::log::LogError> = log.note(&format!("ssh: {capped}{suffix}"));
    }
}

/// Runs dial attempts until the operator stops.
///
/// Reports `LinkState` changes and notes every transition and attempt outcome
/// to the activity log. The backoff ladder advances on failures and resets
/// when an attempt reaches «connected». Each accepted hello rewrites
/// `checkin`, when given, with the current time — the marker an installer's
/// dead-man waits for; a marker that cannot be written is noted, never fatal.
pub fn supervise(
    dialer: &dyn Dialer,
    log_path: &Path,
    checkin: Option<&CheckIn>,
    stop: &Stop,
    report: &mut dyn FnMut(LinkState),
    rest: &mut dyn FnMut(Duration),
) {
    let Ok(mut log) = ActivityLog::open(log_path) else {
        return;
    };
    let mut backoff = Backoff::new();
    while !stop.is_set() {
        report(LinkState::Connecting);
        let _noted: Result<(), crate::log::LogError> = log.note("link: dialing");
        let mut reached_connected = false;
        let mut on_connected = || {
            reached_connected = true;
            report(LinkState::Connected);
            let _noted: Result<(), crate::log::LogError> =
                log.note("link: connected (hello accepted)");
            if let Some(checkin) = checkin
                && let Err(error) = checkin.write(SystemTime::now())
            {
                let _noted: Result<(), crate::log::LogError> = log.note(&format!(
                    "link: check-in marker {} not written: {error}",
                    checkin.path().display()
                ));
            }
        };
        let end = match dialer.attempt(&mut on_connected, stop) {
            Ok(end) => end,
            Err(error) => End::Io(error.to_string()),
        };
        if reached_connected {
            backoff.reset();
        }
        let _noted: Result<(), crate::log::LogError> =
            log.note(&format!("link: attempt ended: {end}"));
        let why = match end {
            End::Stopped => "stopped",
            _ if reached_connected => "down",
            End::HelloRefused => "hello-refused",
            _ => "connect-failed",
        };
        crate::flow::link(why, &end.to_string(), None, &[]);
        if stop.is_set() {
            if why != "stopped" {
                crate::flow::link(
                    "stopped",
                    &format!("stopped by the operator after: {end}"),
                    None,
                    &[],
                );
            }
            break;
        }
        report(LinkState::Connecting);
        let delay = backoff.advance();
        let secs = delay.as_secs().to_string();
        crate::flow::link(
            "retry",
            &format!("next attempt in {secs} s after: {end}"),
            None,
            &[("delay_s", &secs)],
        );
        rest(delay);
    }
}

/// Waits up to `wait`, returning the moment the operator stops.
pub fn rest_until(wait: Duration, stop: &Stop) {
    let _stopped: bool = stop.wait_until(Instant::now().checked_add(wait));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_path(name: &str) -> PathBuf {
        let unique = format!(
            "tulpa-link-test-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        std::env::temp_dir().join(unique)
    }

    fn write_token(name: &str, content: &str) -> io::Result<PathBuf> {
        let path = temp_path(name);
        fs::write(&path, content)?;
        Ok(path)
    }

    const GOOD_TOKEN: &str = "AbCdEfGh0123456789+/=_-xyz";

    #[test]
    fn token_reads_first_line_and_enforces_floor_and_alphabet()
    -> Result<(), Box<dyn std::error::Error>> {
        let missing = read_token(&temp_path("nope"));
        assert!(matches!(missing, Err(LinkError::Token(_))));

        let short = write_token("short", "tooshort\n")?;
        assert!(matches!(read_token(&short), Err(LinkError::Token(_))));

        let bad = write_token("bad", "has space and \"quotes\" 12345678\n")?;
        assert!(matches!(read_token(&bad), Err(LinkError::Token(_))));

        let with_newlines = write_token("good", &format!("{GOOD_TOKEN}\nsecond line\n"))?;
        let token = read_token(&with_newlines)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        assert_eq!(token, GOOD_TOKEN);
        fs::remove_file(with_newlines)?;
        Ok(())
    }

    #[test]
    fn hello_line_has_the_wire_shape() {
        assert_eq!(
            hello_line("dev-1", GOOD_TOKEN),
            format!("{{\"hello\":\"dev-1\",\"token\":\"{GOOD_TOKEN}\"}}\n").into_bytes()
        );
    }

    #[test]
    fn dial_spec_validates_every_field() -> Result<(), Box<dyn std::error::Error>> {
        let token = temp_path("token");
        let spec = DialSpec::new("work-box", "127.0.0.1", 18030, "dev-1", token.clone())?;
        assert_eq!(spec.alias(), "work-box");
        assert_eq!(spec.target(), ("127.0.0.1", 18030));
        assert_eq!(spec.device(), "dev-1");
        assert_eq!(spec.token_file(), token.as_path());

        assert!(DialSpec::new("", "127.0.0.1", 18030, "dev-1", token.clone()).is_err());
        assert!(DialSpec::new("work-box", "", 18030, "dev-1", token.clone()).is_err());
        assert!(DialSpec::new("work-box", "has space", 18030, "dev-1", token.clone()).is_err());
        assert!(DialSpec::new("work-box", "127.0.0.1", 0, "dev-1", token.clone()).is_err());
        assert!(DialSpec::new("work-box", "127.0.0.1", 18030, "", token.clone()).is_err());
        assert!(DialSpec::new("work-box", "127.0.0.1", 18030, "bad\"device", token).is_err());
        assert!(DialSpec::new("work-box", "127.0.0.1", 18030, "dev-1", PathBuf::new()).is_err());
        Ok(())
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets() {
        let mut backoff = Backoff::new();
        let ladder: Vec<u64> = (0..8).map(|_| backoff.advance().as_secs()).collect();
        assert_eq!(ladder, vec![5, 10, 20, 40, 80, 160, 300, 300]);
        backoff.reset();
        assert_eq!(backoff.advance().as_secs(), 5);
    }

    /// A scripted pipe: each `recv` pops the next scripted frame, records what
    /// was sent, and reports EOF once the script runs out. `fail_send`, when
    /// set, is what every `send` returns instead.
    #[derive(Debug, Default)]
    struct FakePipe {
        script: VecDeque<Recv>,
        sent: Vec<Vec<u8>>,
        fail_send: Option<LinkError>,
    }

    impl FakePipe {
        fn scripted(frames: &[Recv]) -> Self {
            Self {
                script: frames.iter().cloned().collect(),
                sent: Vec::new(),
                fail_send: None,
            }
        }
    }

    impl Pipe for FakePipe {
        fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            self.sent.push(bytes.to_vec());
            self.fail_send.as_ref().map_or(Ok(()), |error| {
                Err(LinkError::Send(io::Error::other(error.to_string())))
            })
        }

        fn recv(&mut self, _timeout: Option<Duration>) -> Result<Recv, LinkError> {
            Ok(self.script.pop_front().unwrap_or(Recv::Eof))
        }

        fn waker(&self) -> Waker {
            Arc::new(|| {}) // `recv` never waits
        }
    }

    static NO_STOP: Stop = Stop::new();

    /// A write that parks until the kill gate opens, then fails EPIPE — the
    /// same shape a real child's stdin takes when the child is killed.
    struct ParkedWrite {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    fn half_closed() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, "half-closed")
    }

    /// Waits while the gate is closed; the caller's write parks here exactly
    /// as a real `write_all` parks on a full pipe.
    fn wait_open(gate: &(Mutex<bool>, std::sync::Condvar)) -> io::Result<()> {
        while !*gate.0.lock().map_err(|_| half_closed())? {
            drop(
                gate.1
                    .wait(gate.0.lock().map_err(|_| half_closed())?)
                    .map_err(|_| half_closed())?,
            );
        }
        Ok(())
    }

    impl Write for ParkedWrite {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            wait_open(&self.gate)?;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the child was killed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A kill handle that opens the write's gate, as SIGKILL opens the real
    /// pipe's writer, and counts its own calls.
    #[derive(Default)]
    struct Kills(Mutex<u32>, Arc<(Mutex<bool>, std::sync::Condvar)>);

    impl Closer for Kills {
        fn close(&self) {
            if let Ok(mut count) = self.0.lock() {
                *count = (*count).saturating_add(1);
            }
            if let Ok(mut open) = self.1.0.lock() {
                *open = true;
                self.1.1.notify_all();
            }
        }
    }

    impl Kills {
        fn count(&self) -> u32 {
            self.0.lock().map_or(0, |count| *count)
        }
    }

    /// A write that records every frame whole, and the shared ledger it
    /// writes to (the raw half records there too once handed back).
    struct RecordedWrite(Arc<Mutex<Vec<Vec<u8>>>>);

    impl RecordedWrite {
        fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
            let frames = Arc::new(Mutex::new(Vec::new()));
            (Self(Arc::clone(&frames)), frames)
        }
    }

    impl Write for RecordedWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Ok(mut frames) = self.0.lock() {
                frames.push(buf.to_vec());
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_are_written_whole_and_acked_in_order() {
        let (write, frames) = RecordedWrite::new();
        let mut writer = FrameWriter::new(
            Box::new(write),
            Arc::new(Kills::default()),
            Duration::from_secs(30),
        );
        assert!(
            matches!(writer.send(b"hello\n"), Ok(())),
            "the hello leaves"
        );
        assert!(
            matches!(writer.send(b"{\"id\":\"1\"}\n"), Ok(())),
            "the reply leaves"
        );
        let seen = frames.lock().map(|seen| seen.clone()).unwrap_or_default();
        assert_eq!(
            seen,
            vec![b"hello\n".to_vec(), b"{\"id\":\"1\"}\n".to_vec()],
            "the hello is the writer's first frame, later frames keep their order"
        );
    }

    #[test]
    fn a_stalled_send_kills_the_child_and_later_sends_fail_fast() {
        let kills = Arc::new(Kills::default());
        let kill = Arc::clone(&kills);
        let mut writer = FrameWriter::new(
            Box::new(ParkedWrite {
                gate: Arc::clone(&kills.1),
            }),
            kill,
            Duration::from_millis(200),
        );
        let stalled = writer.send(b"hello\n").map_err(|error| error.to_string());
        assert_eq!(
            stalled,
            Err("send stalled over 200ms; the child was killed".to_owned()),
            "a parked write stalls after the bound"
        );
        assert_eq!(kills.count(), 1, "the child was killed once");
        let again = writer.send(b"again\n").map_err(|error| error.to_string());
        assert_eq!(
            again,
            Err("send stalled over 200ms; the child was killed".to_owned()),
            "a stalled writer never recovers"
        );
        assert_eq!(
            kills.count(),
            1,
            "the latch fails fast without killing twice"
        );
    }

    #[test]
    fn a_dead_writer_fails_its_send_with_its_error_and_the_next_fast() {
        struct DeadWrite;
        impl Write for DeadWrite {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let kills = Arc::new(Kills::default());
        let kill = Arc::clone(&kills);
        let mut writer = FrameWriter::new(Box::new(DeadWrite), kill, Duration::from_secs(30));
        let first = writer
            .send(b"hello\n")
            .map_err(|error| (error.kind(), error.to_string()));
        assert_eq!(
            first,
            Err((io::ErrorKind::BrokenPipe, "pipe closed".to_owned())),
            "the dead write fails with its own error"
        );
        let second = writer
            .send(b"again\n")
            .map_err(|error| (error.kind(), error.to_string()));
        assert_eq!(
            second,
            Err((io::ErrorKind::BrokenPipe, "the write side ended".to_owned())),
            "the ended writer fails fast"
        );
        assert_eq!(kills.count(), 0, "no stall: nothing to kill");
    }

    #[test]
    fn into_inner_hands_the_raw_target_back_after_the_hello() {
        let (write, frames) = RecordedWrite::new();
        let mut writer = FrameWriter::new(
            Box::new(write),
            Arc::new(Kills::default()),
            Duration::from_secs(30),
        );
        assert!(
            matches!(writer.send(b"hello\n"), Ok(())),
            "the hello leaves"
        );
        let raw_wrote = writer.into_inner().map(|mut raw| {
            let sent = raw.write_all(b"raw\n").is_ok();
            sent && raw.flush().is_ok()
        });
        assert!(
            matches!(raw_wrote, Ok(true)),
            "the raw half writes after its bounded hello"
        );
        let seen = frames.lock().map(|seen| seen.clone()).unwrap_or_default();
        assert_eq!(
            seen,
            vec![b"hello\n".to_vec(), b"raw\n".to_vec()],
            "the data half writes the same stdin after its bounded hello"
        );
    }

    #[test]
    fn a_stalled_send_ends_the_attempt_with_its_reason() {
        let mut pipe = FakePipe {
            fail_send: Some(LinkError::Send(io::Error::new(
                io::ErrorKind::TimedOut,
                "send stalled over 30s; the child was killed",
            ))),
            ..FakePipe::scripted(&[Recv::Timeout])
        };
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &refusal_for,
            &Outbox::default(),
        );
        let named = match &end {
            End::Io(reason) => reason.contains("send stalled"),
            _ => false,
        };
        assert!(
            named,
            "a stalled send ends the attempt with its reason, got {end:?}"
        );
    }

    #[test]
    fn serve_attempt_refused_hello_when_the_pipe_closes_in_grace() {
        let mut pipe = FakePipe::scripted(&[Recv::Eof]);
        let mut connected = false;
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {
                connected = true;
            },
            &refusal_for,
            &Outbox::default(),
        );
        assert_eq!(end, End::HelloRefused);
        assert!(!connected, "a refused hello never reports connected");
        assert_eq!(pipe.sent, vec![b"hello\n".to_vec()]);
    }

    #[test]
    fn serve_attempt_connects_then_serves_until_drop() {
        let mut pipe = FakePipe::scripted(&[
            Recv::Timeout,
            Recv::Line(b"{\"id\":\"1\",\"verb\":\"ping\"}".to_vec()),
            Recv::Line(b"garbage".to_vec()),
            Recv::Eof,
        ]);
        let mut connects = 0usize;
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {
                connects = connects.saturating_add(1);
            },
            &refusal_for,
            &Outbox::default(),
        );
        assert_eq!(end, End::Dropped);
        assert_eq!(connects, 1, "connected fires exactly once");
        assert_eq!(
            pipe.sent,
            vec![
                b"hello\n".to_vec(),
                b"{\"id\":\"1\",\"err\":\"verb\"}\n".to_vec(),
                REFUSAL.to_vec(),
            ],
            "every command line is refused while hands do not exist; the refusal echoes the request id so the broker's pending clears"
        );
    }

    #[test]
    fn serve_attempt_writes_events_after_the_hello_and_between_replies() {
        let outbox = Outbox::default();
        outbox.push(b"{\"event\":\"ears\",\"state\":\"deaf\"}".to_vec());
        let mut pipe = FakePipe::scripted(&[
            Recv::Timeout,
            Recv::Line(b"{\"id\":\"1\",\"verb\":\"ping\"}".to_vec()),
            Recv::Eof,
        ]);
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &refusal_for,
            &outbox,
        );
        assert_eq!(end, End::Dropped);
        assert_eq!(
            pipe.sent,
            vec![
                b"hello\n".to_vec(),
                b"{\"event\":\"ears\",\"state\":\"deaf\"}\n".to_vec(),
                b"{\"id\":\"1\",\"err\":\"verb\"}\n".to_vec(),
            ],
            "a queued event goes out once the hello is accepted, newline added"
        );
        assert!(outbox.drain().is_empty());
    }

    #[test]
    fn outbox_keeps_the_newest_lines_only() {
        let outbox = Outbox::default();
        for n in 0..OUTBOX_MAX.saturating_add(3) {
            outbox.push(format!("{n}\n").into_bytes());
        }
        let lines = outbox.drain();
        assert_eq!(lines.len(), OUTBOX_MAX);
        assert_eq!(lines.first().map(Vec::as_slice), Some(&b"3\n"[..]));
    }

    use crate::flow::{fl, nth};
    use std::fmt::Write as _;

    /// The flow log's entries whose frame mentions `needle` (each test's own unique text).
    fn flow_with(needle: &str) -> Vec<serde_json::Value> {
        crate::flow::test_entries(|entry| fl(entry, "/frame").to_string().contains(needle))
    }

    #[test]
    fn flow_logs_a_request_and_its_reply_under_one_id() {
        crate::flow::test_global();
        let outbox = Outbox::default();
        let mut pipe = FakePipe::scripted(&[
            Recv::Line(
                b"{\"id\":\"flow-req-1\",\"verb\":\"hands.run\",\"arg\":\"uptime\"}".to_vec(),
            ),
            Recv::Eof,
        ]);
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &|_line: &[u8]| b"{\"id\":\"flow-req-1\",\"ok\":true,\"result\":\"up\"}\n".to_vec(),
            &outbox,
        );
        assert_eq!(end, End::Dropped);
        let entries = flow_with("flow-req-1");
        assert_eq!(entries.len(), 2, "one line each: {entries:?}");
        let (request, reply) = (&nth(&entries, 0), &nth(&entries, 1));
        assert_eq!(
            (
                &fl(request, "/dir"),
                &fl(request, "/kind"),
                &fl(request, "/id")
            ),
            (&"in".into(), &"request".into(), &"flow-req-1".into())
        );
        assert_eq!(fl(request, "/frame/arg"), "uptime", "the full request");
        assert_eq!(
            (&fl(reply, "/dir"), &fl(reply, "/kind"), &fl(reply, "/id")),
            (&"out".into(), &"reply".into(), &"flow-req-1".into())
        );
        assert_eq!(fl(reply, "/frame/result"), "up", "the full reply");
    }

    #[test]
    fn flow_logs_an_outbox_event_as_it_goes_out() {
        crate::flow::test_global();
        let outbox = Outbox::default();
        outbox.push(b"{\"event\":\"ears\",\"state\":\"flow-event-2\"}".to_vec());
        let mut pipe = FakePipe::scripted(&[Recv::Timeout, Recv::Eof]);
        let _end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &refusal_for,
            &outbox,
        );
        let entries = flow_with("flow-event-2");
        assert_eq!(entries.len(), 2, "{entries:?}");
        // Pushed before any attempt served: noted as queued while the link was down.
        let queued = nth(&entries, 0);
        assert_eq!(
            (
                fl(&queued, "/dir"),
                fl(&queued, "/kind"),
                fl(&queued, "/why")
            ),
            ("note".into(), "link".into(), "queued-offline".into())
        );
        assert_eq!(fl(&queued, "/frame/state"), "flow-event-2");
        assert_eq!(fl(&nth(&entries, 1), "/dir"), "out");
        assert_eq!(fl(&nth(&entries, 1), "/kind"), "event");
        assert_eq!(fl(&nth(&entries, 1), "/id"), serde_json::Value::Null);
        assert_eq!(fl(&nth(&entries, 1), "/frame/state"), "flow-event-2");
    }

    #[test]
    fn flow_keeps_a_big_shell_reply_whole() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let mut stdout = String::new();
        for n in 0..3_000 {
            writeln!(stdout, "flow-out-3 line {n:05} of the build's output")?;
        }
        let stderr = "warning: one\nwarning: two\n".repeat(400);
        assert!(stdout.len() > 64 * 1024);
        let reply = serde_json::json!({
            "id": "flow-shell-3", "ok": true,
            "result": {"cmd": "make all", "code": 2, "stdout": stdout, "stderr": stderr},
        });
        let reply = format!("{reply}\n").into_bytes();
        let mut pipe = FakePipe::scripted(&[
            Recv::Line(
                b"{\"id\":\"flow-shell-3\",\"verb\":\"hands.run\",\"arg\":\"make all\"}".to_vec(),
            ),
            Recv::Eof,
        ]);
        let _end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &|_line: &[u8]| reply.clone(),
            &Outbox::default(),
        );
        let entries = crate::flow::test_entries(|e| {
            fl(e, "/id") == "flow-shell-3" && fl(e, "/kind") == "reply"
        });
        assert_eq!(entries.len(), 1);
        let result = &fl(&nth(&entries, 0), "/frame/result");
        assert_eq!(
            fl(result, "/stdout").as_str(),
            Some(stdout.as_str()),
            "every byte of stdout"
        );
        assert_eq!(
            fl(result, "/stderr").as_str(),
            Some(stderr.as_str()),
            "every byte of stderr"
        );
        assert_eq!(fl(result, "/code"), 2);
        assert_eq!(fl(result, "/cmd"), "make all");
        Ok(())
    }

    #[test]
    fn flow_logs_the_hello_with_its_token_redacted() -> Result<(), Box<dyn std::error::Error>> {
        crate::flow::test_global();
        let mut pipe = FakePipe::scripted(&[Recv::Eof]);
        let hello = hello_line("flow-dev-4", GOOD_TOKEN);
        let _end = serve_attempt(
            &mut pipe,
            &hello,
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &refusal_for,
            &Outbox::default(),
        );
        assert_eq!(
            pipe.sent.first(),
            Some(&hello),
            "the wire still carries the token"
        );
        let entries = flow_with("flow-dev-4");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(fl(&nth(&entries, 0), "/kind"), "hello");
        assert_eq!(fl(&nth(&entries, 0), "/dir"), "out");
        assert_eq!(
            fl(&nth(&entries, 0), "/frame"),
            serde_json::json!({"hello": "flow-dev-4", "token": crate::flow::REDACTED}),
            "the token redacted, the rest as it was"
        );
        let file = fs::read_to_string(crate::flow::test_global().path())?;
        assert!(!file.contains(GOOD_TOKEN), "no token anywhere in the log");
        Ok(())
    }

    #[test]
    fn flow_logs_an_outbox_overflow_drop() {
        crate::flow::test_global();
        let outbox = Outbox::default();
        for n in 0..=OUTBOX_MAX {
            outbox.push(format!("{{\"event\":\"flow-drop-6\",\"n\":{n}}}").into_bytes());
        }
        let drops = crate::flow::test_entries(|e| {
            fl(e, "/kind") == "drop" && fl(e, "/frame/event") == "flow-drop-6"
        });
        assert_eq!(drops.len(), 1, "{drops:?}");
        assert_eq!(fl(&nth(&drops, 0), "/frame/n"), 0, "the oldest went");
        assert_eq!(fl(&nth(&drops, 0), "/dir"), "note");
    }

    /// akira F2 rig check (a): link down, 20 ears lines and one typed line
    /// queued — on reconnect the typed line arrives, first, and the ears lane
    /// alone bore the eviction.
    #[test]
    fn a_typed_line_survives_an_ears_flood_and_drains_first() {
        let outbox = Outbox::default();
        let (news, sink) = news_sink();
        outbox.set_on_wire(sink);
        for n in 0..OUTBOX_MAX.saturating_add(4) {
            outbox
                .push(format!("{{\"event\":\"ears\",\"state\":\"line\",\"n\":{n}}}").into_bytes());
        }
        outbox.push_typed(
            7,
            br#"{"event":"text","state":"said","text":"telegram is down"}"#.to_vec(),
        );
        assert_eq!(outbox.typed_waiting(), 1, "the typed lane is its own");
        let mut pipe = FakePipe::scripted(&[]);
        assert!(outbox.flush(&mut LoggedPipe::new(&mut pipe)).is_none());
        assert!(
            pipe.sent
                .first()
                .is_some_and(|l| l.starts_with(b"{\"event\":\"text\"")),
            "the operator's mail goes first"
        );
        assert_eq!(
            pipe.sent.len(),
            OUTBOX_MAX.saturating_add(1),
            "the ears lane kept its own newest 16"
        );
        assert!(
            !pipe.sent.iter().any(|l| contains(l, b"\"n\":0}")),
            "the oldest ears line was the evicted one"
        );
        assert!(
            pipe.sent.iter().any(|l| contains(l, b"\"n\":19}")),
            "the newest ears line is kept"
        );
        assert_eq!(take_news(&news), vec![WireNews::Written(7)]);
        assert_eq!(outbox.typed_waiting(), 0);
    }

    /// #0423 r3 S3 (P2 over every Debug): a typed line waiting in its lane never shows in a `{:?}` —
    /// of the lane, of its lock, or of the outbox that holds it — as text or as its bytes.
    #[test]
    fn debug_never_prints_a_typed_line() {
        const MARK: &str = "секрет-оператора";
        let outbox = Outbox::default();
        outbox.push_typed(1, crate::saylog::typed_event(MARK, None));
        assert_eq!(outbox.typed_waiting(), 1);
        for (shown, counted) in [
            (format!("{:?}", *outbox.lane()), "queued: 1"),
            (format!("{:?}", outbox.typed), "queued: 1"),
            (format!("{outbox:?}"), "typed: 1"),
        ] {
            // The lane holds bytes: a derived Debug prints them as numbers, so both forms are looked for.
            let as_bytes = format!("{:?}", MARK.as_bytes());
            let as_bytes = as_bytes.trim_matches(['[', ']']);
            assert!(
                !shown.contains(MARK) && !shown.contains(as_bytes),
                "{shown}"
            );
            assert!(shown.contains(counted), "the line is counted: {shown}");
        }
    }

    /// #0423 r2 R2: a panic while the typed lane's lock was held (anywhere) poisons it; the lane still
    /// takes, times and writes its lines — a poisoned lock never stalls the operator's mail silently.
    #[test]
    fn a_poisoned_typed_lane_still_writes_its_line() {
        let outbox = Arc::new(Outbox::default());
        let (news, sink) = news_sink();
        outbox.set_on_wire(sink);
        let poisoner = Arc::clone(&outbox);
        #[expect(
            clippy::panic,
            reason = "a panic under the lane's lock is what is tested"
        )]
        let panicked = thread::spawn(move || {
            let _held = poisoner.typed.lock();
            panic!("rig: poison the typed lane");
        })
        .join();
        assert!(
            panicked.is_err(),
            "the rig thread panicked holding the lock"
        );
        assert!(outbox.typed.is_poisoned(), "the lane's lock is poisoned");
        outbox.push_typed(
            3,
            br#"{"event":"text","state":"said","text":"still here"}"#.to_vec(),
        );
        assert_eq!(outbox.typed_waiting(), 1);
        assert_eq!(
            outbox.typed_due(),
            Some(Duration::ZERO),
            "due now, not never"
        );
        let sent = outbox.test_flush();
        assert!(sent.iter().any(|l| contains(l, b"still here")), "{sent:?}");
        assert_eq!(take_news(&news), vec![WireNews::Written(3)]);
        assert_eq!(outbox.typed_waiting(), 0);
    }

    /// akira F2 rig check (b): 15 typed lines queued — no more than the rate
    /// window allows leave on the first flush, the rest stay queued for the
    /// next cycle, and all leave once the window slid; the wire's word is said
    /// for each, in order, none silently dropped. No wall-clock race (#0423 r3
    /// S6): the hold is checked under the production window, which no
    /// descheduled thread can outlast; the slide by shrinking the window to
    /// 1 ms. The timing of the slide itself is [`paced_sends`]' (virtual clock).
    #[test]
    fn the_typed_lane_drains_under_the_agent_doors_rate_window() {
        let outbox = Outbox::default();
        let (news, sink) = news_sink();
        outbox.set_on_wire(sink);
        for n in 0..15 {
            outbox.push_typed(
                n,
                format!("{{\"event\":\"text\",\"state\":\"said\",\"text\":\"line {n}\"}}")
                    .into_bytes(),
            );
        }
        let mut pipe = FakePipe::scripted(&[]);
        assert!(outbox.flush(&mut LoggedPipe::new(&mut pipe)).is_none());
        assert_eq!(
            pipe.sent.len(),
            TYPED_PACE_MAX,
            "the window's allowance, no more"
        );
        assert_eq!(
            outbox.typed_waiting(),
            15 - TYPED_PACE_MAX,
            "the rest stay queued, not dropped"
        );
        // The next flush cycle within the window: still held.
        let mut held = FakePipe::scripted(&[]);
        assert!(outbox.flush(&mut LoggedPipe::new(&mut held)).is_none());
        assert_eq!(held.sent.len(), 0, "inside the window nothing more leaves");
        outbox.set_rate_for_test(TYPED_PACE_MAX, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let mut later = FakePipe::scripted(&[]);
        assert!(outbox.flush(&mut LoggedPipe::new(&mut later)).is_none());
        assert_eq!(
            later.sent.len(),
            15 - TYPED_PACE_MAX,
            "the window slid: the rest leave"
        );
        assert_eq!(outbox.typed_waiting(), 0);
        let all: Vec<Vec<u8>> = pipe.sent.into_iter().chain(later.sent).collect();
        assert_eq!(
            all.len(),
            15,
            "all fifteen reached the wire within two windows"
        );
        for n in 0..15 {
            let want = format!("line {n}");
            assert!(
                all.iter().any(|l| contains(l, want.as_bytes())),
                "{want} went out"
            );
        }
        let wire_word = take_news(&news);
        assert_eq!(
            wire_word.len(),
            15,
            "the wire said a word for each, in order"
        );
        assert_eq!(
            wire_word,
            (0..15).map(WireNews::Written).collect::<Vec<_>>(),
            "each line's own id, in order"
        );
    }

    /// F2: a full typed lane drops its own oldest — said as an eviction, never
    /// silence; the ears lane is untouched.
    #[test]
    fn a_full_typed_lane_evicts_oldest_said_and_leaves_ears_alone() {
        let outbox = Outbox::default();
        outbox.set_rate_for_test(usize::MAX, Duration::from_millis(0));
        let (news, sink) = news_sink();
        outbox.set_on_wire(sink);
        outbox.push(b"ears line\n".to_vec());
        for n in 0..=TYPED_MAX {
            outbox.push_typed(
                u64::try_from(n).unwrap_or(u64::MAX),
                format!("typed {n}").into_bytes(),
            );
        }
        assert_eq!(outbox.typed_waiting(), TYPED_MAX);
        assert_eq!(
            take_news(&news),
            vec![WireNews::Evicted(0)],
            "the oldest typed line, said"
        );
        assert_eq!(
            outbox.drain(),
            vec![b"ears line\n".to_vec()],
            "the ears lane untouched"
        );
    }

    /// F2: a pipe that breaks mid-typed-send puts the line back at the front
    /// of its lane — it goes first on the next connection, and the window
    /// never heard "sent" for it.
    #[test]
    fn a_broken_pipe_puts_the_typed_line_back_unsaid() {
        let outbox = Outbox::default();
        let (news, sink) = news_sink();
        outbox.set_on_wire(sink);
        outbox.push_typed(1, b"first".to_vec());
        outbox.push_typed(2, b"second".to_vec());
        let mut pipe = FakePipe::scripted(&[]);
        pipe.fail_send = Some(LinkError::Config("broken".to_owned()));
        assert!(
            outbox.flush(&mut LoggedPipe::new(&mut pipe)).is_some(),
            "the pipe broke"
        );
        assert_eq!(outbox.typed_waiting(), 2, "both lines back in the lane");
        assert!(take_news(&news).is_empty(), "nothing was said to have gone");
        let mut ok = FakePipe::scripted(&[]);
        assert!(outbox.flush(&mut LoggedPipe::new(&mut ok)).is_none());
        assert_eq!(
            ok.sent.first().map(Vec::as_slice),
            Some(&b"first\n"[..]),
            "the unsent goes first"
        );
    }

    /// The hub's door as tulpa-text.ts keeps it: the instants (ms) it ACCEPTED; an arrival with
    /// [`TYPED_RATE_MAX`] accepted ones already inside the last [`TYPED_RATE_WINDOW`] (`now - t <
    /// window`) is dropped. Returns how many arrivals were dropped.
    fn hub_drops(arrivals: &[u64]) -> usize {
        let window = u64::try_from(TYPED_RATE_WINDOW.as_millis()).unwrap_or(u64::MAX);
        let mut sorted = arrivals.to_vec();
        sorted.sort_unstable();
        let mut kept: Vec<u64> = Vec::new();
        let mut dropped = 0_usize;
        for now in sorted {
            kept.retain(|t| now.saturating_sub(*t) < window);
            if kept.len() >= TYPED_RATE_MAX {
                dropped = dropped.saturating_add(1);
            } else {
                kept.push(now);
            }
        }
        dropped
    }

    /// The most arrivals inside any span of the hub's window, `(t - window, t]`.
    fn most_in_window(arrivals: &[u64]) -> usize {
        let window = u64::try_from(TYPED_RATE_WINDOW.as_millis()).unwrap_or(u64::MAX);
        arrivals
            .iter()
            .map(|end| {
                arrivals
                    .iter()
                    .filter(|t| *t <= end && end.saturating_sub(**t) < window)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    /// The production pacer ([`TypedLane::next`]/[`TypedLane::sent`]) driven on a virtual clock the
    /// way the serve loop drives it: flush what the window lets go, then wait [`TypedLane::wait`]
    /// (the loop's [`Outbox::typed_due`]) and flush again. `n` lines queued at once, at most `max` per
    /// window. Returns the send instants, ms from the start.
    fn paced_sends(n: u64, max: usize) -> Vec<u64> {
        let start = Instant::now();
        let mut lane = TypedLane::new();
        lane.rate_max = max;
        for id in 0..n {
            lane.queue.push_back((id, Vec::new()));
        }
        let mut now = start;
        let mut sends = Vec::new();
        while !lane.queue.is_empty() {
            while lane.next(now).is_some() {
                lane.sent(now);
                sends
                    .push(u64::try_from(now.duration_since(start).as_millis()).unwrap_or(u64::MAX));
            }
            let Some(later) = now.checked_add(lane.wait(now)) else {
                break;
            };
            now = later;
        }
        sends
    }

    /// The hub model is no blank: 13 arrivals inside one window lose one, 12 lose none.
    #[test]
    fn the_hub_model_drops_a_thirteenth_and_only_that() {
        assert_eq!(hub_drops(&[0; 13]), 1);
        assert_eq!(hub_drops(&[0; 12]), 0);
        assert_eq!(most_in_window(&[0; 13]), 13);
        assert_eq!(hub_drops(&[0, 60_000]), 0, "the window is half-open");
    }

    /// #0423 akira T3: 24 typed lines queued at once (a reconnect's drain), the pacer at
    /// [`TYPED_PACE_MAX`]: whatever delay the first arrival takes inside a window, the hub's sliding
    /// window never holds 13 arrivals and drops nothing; every line is written.
    #[test]
    fn a_delayed_first_arrival_never_makes_the_hubs_thirteenth() {
        let sends = paced_sends(24, TYPED_PACE_MAX);
        assert_eq!(sends.len(), 24, "every line is written: {sends:?}");
        for delay in [1, 250, 5_000, 30_000, 59_999] {
            let mut arrivals = sends.clone();
            if let Some(first) = arrivals.first_mut() {
                *first = first.saturating_add(delay);
            }
            assert!(
                most_in_window(&arrivals) <= TYPED_RATE_MAX,
                "the hub's window held {} with the first {delay} ms late: {arrivals:?}",
                most_in_window(&arrivals)
            );
            assert_eq!(
                hub_drops(&arrivals),
                0,
                "first {delay} ms late: {arrivals:?}"
            );
        }
    }

    /// A typed line the window holds leaves when its deadline comes, with no push to wake the loop:
    /// the serve loop's wait on the pipe is bounded by [`Outbox::typed_due`] while a line is held, and
    /// unbounded (`None`) once the lane is empty.
    #[test]
    fn a_held_typed_line_leaves_on_its_own_deadline() {
        struct Timed {
            sent: Vec<Vec<u8>>,
            waits: Vec<Option<Duration>>,
        }
        impl Pipe for Timed {
            fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                self.sent.push(bytes.to_vec());
                Ok(())
            }
            fn recv(&mut self, timeout: Option<Duration>) -> Result<Recv, LinkError> {
                self.waits.push(timeout);
                // Honours a bounded wait as a real pipe would; an unbounded one ends the attempt.
                match timeout {
                    Some(wait) if wait <= Duration::from_secs(1) => {
                        std::thread::sleep(wait);
                        Ok(Recv::Timeout)
                    }
                    _ => Ok(Recv::Eof),
                }
            }
            fn waker(&self) -> Waker {
                Arc::new(|| {})
            }
        }
        let outbox = Outbox::default();
        outbox.set_rate_for_test(1, Duration::from_millis(200));
        outbox.push_typed(1, b"first".to_vec());
        outbox.push_typed(2, b"second".to_vec());
        let mut pipe = Timed {
            sent: Vec::new(),
            waits: Vec::new(),
        };
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            Duration::from_millis(1),
            &NO_STOP,
            &mut || {},
            &|_| Vec::new(),
            &outbox,
        );
        assert!(matches!(end, End::Dropped), "{end:?}");
        assert_eq!(
            pipe.sent,
            [
                b"hello\n".to_vec(),
                b"first\n".to_vec(),
                b"second\n".to_vec()
            ],
            "the held line left with no push"
        );
        let held = pipe.waits.get(1).copied().flatten();
        assert!(
            held.is_some_and(|wait| wait > Duration::ZERO && wait <= Duration::from_millis(200)),
            "the wait while a line is held is its deadline: {:?}",
            pipe.waits
        );
        assert_eq!(
            pipe.waits.last(),
            Some(&None),
            "an empty lane waits for a push: {:?}",
            pipe.waits
        );
    }

    /// #0423 P2: a typed line's text never reaches the disk — `flow.log` holds its fate (written, or
    /// evicted) with the text `"<redacted>"`; the wire still carries the text.
    #[test]
    fn a_typed_lines_text_stays_off_flow_log() {
        crate::flow::test_global();
        let outbox = Outbox::default();
        outbox.set_rate_for_test(usize::MAX, Duration::ZERO);
        let event = |text: &str| crate::saylog::typed_event(text, None);
        // One more than the lane holds: the first is evicted, the rest are written.
        for n in 0..=TYPED_MAX {
            outbox.push_typed(
                u64::try_from(n).unwrap_or(u64::MAX),
                event(&format!("p2-тайна-{n}")),
            );
        }
        let sent = outbox.test_flush();
        assert_eq!(sent.len(), TYPED_MAX);
        assert!(
            sent.iter().all(|l| contains(l, "p2-тайна-".as_bytes())),
            "the wire has the text"
        );
        let leaked = crate::flow::test_entries(|e| e.to_string().contains("p2-тайна-"));
        assert!(leaked.is_empty(), "flow.log holds typed text: {leaked:?}");
        let typed = crate::flow::test_entries(|e| {
            fl(e, "/kind") == "typed" && fl(e, "/frame/text") == crate::flow::REDACTED
        });
        let outcome = |want: &str| typed.iter().filter(|e| fl(e, "/outcome") == want).count();
        assert!(outcome("evicted") >= 1, "the eviction is noted: {typed:?}");
        assert!(
            typed.iter().filter(|e| fl(e, "/dir") == "out").count() >= TYPED_MAX,
            "each written line is logged, redacted: {typed:?}"
        );
    }

    /// Whether `haystack` contains `needle`, plainly (tests only).
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len().max(1)).any(|w| w == needle)
    }

    /// A `set_on_wire` sink for tests: the callback must own its storage
    /// (`'static`), so the seen lines live behind an `Arc<Mutex<..>>` the test
    /// reads back through [`take_news`].
    fn news_sink() -> (Arc<Mutex<Vec<WireNews>>>, WireSink) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = Arc::clone(&seen);
            Box::new(move |what| {
                if let Ok(mut seen) = seen.lock() {
                    seen.push(what);
                }
            })
        };
        (seen, sink)
    }

    /// Drains a [`news_sink`]'s storage, poison-tolerant (nothing here panics
    /// while holding it).
    fn take_news(seen: &Arc<Mutex<Vec<WireNews>>>) -> Vec<WireNews> {
        seen.lock()
            .map(|mut seen| std::mem::take(&mut *seen))
            .unwrap_or_default()
    }

    #[test]
    fn refusal_echoes_the_request_id_only_when_present() {
        assert_eq!(
            refusal_for(b"{\"id\":\"ping-7\",\"verb\":\"ping\"}"),
            b"{\"id\":\"ping-7\",\"err\":\"verb\"}\n".to_vec()
        );
        assert_eq!(refusal_for(b"garbage"), REFUSAL.to_vec());
        assert_eq!(
            refusal_for(b"{\"id\":\"\"}"),
            REFUSAL.to_vec(),
            "an empty id is no id"
        );
        assert_eq!(
            refusal_for(b"{\"id\":\"bad\\\"esc\"}"),
            REFUSAL.to_vec(),
            "an escaped quote ends the value scan: the remainder is not a clean id"
        );
        let long = format!("{{\"id\":\"{}\"}}", "x".repeat(65));
        assert_eq!(
            refusal_for(long.as_bytes()),
            REFUSAL.to_vec(),
            "an overlong id is refused bare"
        );
    }

    /// An overlong line is refused with its id and the attempt goes on (it used to end the attempt,
    /// and the broker's redial resent the line forever).
    #[test]
    fn serve_attempt_refuses_an_overlong_line_and_goes_on() {
        let mut long = br#"{"id":"w1","verb":"write","arg":""#.to_vec();
        long.resize(LINE_MAX_BYTES.saturating_add(1), b'x');
        let mut pipe = FakePipe::scripted(&[
            Recv::Timeout,
            Recv::Line(long),
            Recv::Line(br#"{"id":"p2","verb":"ping"}"#.to_vec()),
            Recv::Eof,
        ]);
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &NO_STOP,
            &mut || {},
            &refusal_for,
            &Outbox::default(),
        );
        assert_eq!(end, End::Dropped, "the attempt ends only at the EOF");
        let refused =
            String::from_utf8_lossy(pipe.sent.get(1).map_or(&[][..], Vec::as_slice)).into_owned();
        assert_eq!(
            refused,
            format!(
                "{{\"id\":\"w1\",\"ok\":false,\"err\":\"line over {LINE_MAX_BYTES} bytes\"}}\n"
            )
        );
        assert_eq!(pipe.sent.len(), 3, "the next line was served too");
    }

    #[test]
    fn serve_attempt_stops_when_the_operator_stops() {
        let stop = Stop::new();
        stop.set();
        let mut pipe = FakePipe::scripted(&[Recv::Timeout]);
        let end = serve_attempt(
            &mut pipe,
            b"hello\n",
            HELLO_GRACE,
            &stop,
            &mut || {},
            &refusal_for,
            &Outbox::default(),
        );
        assert_eq!(end, End::Stopped);
    }

    /// A pipe that stays silent: `recv` waits out its whole window (or until woken), every
    /// frame sent goes to `sent` the moment it is written.
    struct IdlePipe {
        sent: mpsc::Sender<Vec<u8>>,
        wake: mpsc::Sender<()>,
        woken: mpsc::Receiver<()>,
    }

    impl Pipe for IdlePipe {
        fn send(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            let _sent: Result<(), mpsc::SendError<Vec<u8>>> = self.sent.send(bytes.to_vec());
            Ok(())
        }

        fn recv(&mut self, timeout: Option<Duration>) -> Result<Recv, LinkError> {
            let _woken = timeout.map_or_else(
                || {
                    self.woken
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                },
                |window| self.woken.recv_timeout(window),
            );
            Ok(Recv::Timeout)
        }

        fn waker(&self) -> Waker {
            let wake = self.wake.clone();
            Arc::new(move || {
                let _sent: Result<(), mpsc::SendError<()>> = wake.send(());
            })
        }
    }

    /// A line pushed while the serve loop waits on a silent pipe leaves at once, not on the loop's next stop check.
    /// Past the hello's grace the wait on the silent pipe has no clock, so only the push's wake can send the line
    /// before the 60 s guard; on a plain thread, so a wait it does not wake fails the test there.
    #[test]
    fn serve_attempt_writes_a_pushed_line_without_waiting_for_the_tick()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sent, frames) = mpsc::channel();
        let (wake, woken) = mpsc::channel();
        let mut pipe = IdlePipe { sent, wake, woken };
        let outbox = Arc::new(Outbox::default());
        let stop = Arc::new(Stop::new());
        let grace = Duration::from_millis(10);
        let (done, ended) = mpsc::channel();
        {
            let (outbox, stop) = (Arc::clone(&outbox), Arc::clone(&stop));
            thread::spawn(move || {
                let end = serve_attempt(
                    &mut pipe,
                    b"hello\n",
                    grace,
                    &stop,
                    &mut || {},
                    &refusal_for,
                    &outbox,
                );
                let _told = done.send(end);
            });
        }
        let hello = frames.recv_timeout(Duration::from_secs(60));
        // Past the grace and the first (empty) flush, so the loop is parked in `recv` (an order, not a bound).
        thread::sleep(grace.saturating_mul(6));
        outbox.push(b"{\"event\":\"ears\",\"state\":\"on\"}".to_vec());
        let line = frames.recv_timeout(Duration::from_secs(60));
        stop.set();
        assert_eq!(hello?, b"hello\n");
        assert_eq!(line?, b"{\"event\":\"ears\",\"state\":\"on\"}\n");
        assert_eq!(ended.recv_timeout(Duration::from_secs(60))?, End::Stopped);
        Ok(())
    }

    /// A stop while the serve loop waits on a silent pipe ends the attempt: the stop wakes the pipe's wait (the old
    /// loop looked every 500 ms). The silent pipe has no clock, so only the stop can end the wait before the 60 s
    /// guard; on a plain thread, so a wait it does not wake fails the test there instead of hanging a join.
    #[test]
    fn serve_attempt_ends_at_once_when_stopped_on_a_silent_pipe()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sent, frames) = mpsc::channel();
        let (wake, woken) = mpsc::channel();
        let mut pipe = IdlePipe { sent, wake, woken };
        let stop = Arc::new(Stop::new());
        let grace = Duration::from_millis(10);
        let (done, ended) = mpsc::channel();
        let serving = Arc::clone(&stop);
        thread::spawn(move || {
            let outbox = Outbox::default();
            let end = serve_attempt(
                &mut pipe,
                b"hello\n",
                grace,
                &serving,
                &mut || {},
                &refusal_for,
                &outbox,
            );
            let _told = done.send(end);
        });
        let hello = frames.recv_timeout(Duration::from_secs(60));
        thread::sleep(grace.saturating_mul(6));
        stop.set();
        assert_eq!(hello?, b"hello\n");
        assert_eq!(ended.recv_timeout(Duration::from_secs(60))?, End::Stopped);
        Ok(())
    }

    /// A stop during the hello's grace ends the attempt as an event, without taking the hello as accepted (no
    /// `on_connected`, so no Connected menu and no check-in marker). The grace is past the 60 s guard, so only the
    /// stop can end the wait before the guard; on a plain thread, so a wait it does not wake fails the test there.
    #[test]
    fn a_stop_during_the_hello_grace_ends_the_attempt_unconnected()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sent, frames) = mpsc::channel();
        let (wake, woken) = mpsc::channel();
        let mut pipe = IdlePipe { sent, wake, woken };
        let stop = Arc::new(Stop::new());
        let grace = Duration::from_secs(3600);
        let (done, ended) = mpsc::channel();
        {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut connected = 0_u32;
                let end = serve_attempt(
                    &mut pipe,
                    b"hello\n",
                    grace,
                    &stop,
                    &mut || connected = connected.saturating_add(1),
                    &refusal_for,
                    &Outbox::default(),
                );
                let _told = done.send((end, connected));
            });
        }
        // The hello is out: the attempt is in its grace.
        let hello = frames.recv_timeout(Duration::from_secs(60));
        stop.set();
        assert_eq!(hello?, b"hello\n");
        assert_eq!(
            ended.recv_timeout(Duration::from_secs(60))?,
            (End::Stopped, 0),
            "stopped, and not connected"
        );
        Ok(())
    }

    /// The reconnect backoff ends the moment the operator stops, not at its next 100 ms look. The rest is past the
    /// 60 s guard, so only the stop can end it before the guard; on a plain thread, so a rest it does not wake fails
    /// the test there.
    #[test]
    fn rest_until_ends_on_stop() -> Result<(), Box<dyn std::error::Error>> {
        let stop = Arc::new(Stop::new());
        let (done, rested) = mpsc::channel();
        {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                rest_until(Duration::from_secs(3600), &stop);
                let _told = done.send(());
            });
        }
        // So the stop lands inside the rest (an order, not a bound: a stop before it ends it at once too).
        thread::sleep(Duration::from_millis(50));
        stop.set();
        rested.recv_timeout(Duration::from_secs(60))?;
        let unset = Stop::new();
        let began = Instant::now();
        rest_until(Duration::from_millis(40), &unset);
        assert!(
            began.elapsed() >= Duration::from_millis(40),
            "a full rest when not stopped"
        );
        Ok(())
    }

    /// The stdout reader frames every line, then marks the pipe dead: a hands
    /// child blocked in a `run` watches the same flag and is killed.
    /// Office Wi-Fi → each LAN address, then the jump; any other network (or none) → the jump only.
    #[test]
    fn routes_follow_the_wifi_name() {
        let office = vec!["ExampleOfficeWifi".to_owned()];
        let hosts = vec!["hub.example".to_owned(), "192.0.2.10".to_owned()];
        assert_eq!(
            routes(Some("ExampleOfficeWifi"), &office, &hosts),
            vec![
                Route::Direct("hub.example".to_owned()),
                Route::Direct("192.0.2.10".to_owned()),
                Route::Jump
            ]
        );
        assert_eq!(routes(Some("Cafe"), &office, &hosts), vec![Route::Jump]);
        assert_eq!(routes(None, &office, &hosts), vec![Route::Jump]);
        assert_eq!(
            parse_ssid("<dictionary> {\n  SSID : ExampleOfficeWifi\n  BSSID : aa\n}"),
            Some("ExampleOfficeWifi".to_owned())
        );
        assert_eq!(parse_ssid("  BSSID : aa\n"), None);
    }

    #[test]
    fn forward_lines_frames_and_marks_the_pipe_dead_at_eof()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("echo one; echo two")
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "child has no stdout pipe"))?;
        let attempts = Attempts::default();
        let (tx, rx) = mpsc::channel();
        let end = PipeEnd::open(&attempts);
        let dead = attempts.current();
        let reader = thread::spawn(move || forward_lines(stdout, &tx, &end));
        let frames: Vec<PipeFrame> = rx.iter().collect();
        let _joined = reader.join();
        assert_eq!(
            frames,
            vec![
                PipeFrame::Line(b"one".to_vec()),
                PipeFrame::Line(b"two".to_vec()),
                PipeFrame::Eof
            ]
        );
        assert!(dead.is_set(), "EOF marks the pipe dead");
        Ok(())
    }

    /// A stdout reader that ends without the end-of-stream marker (here: it panics) breaks the
    /// pipe at once; `recv` must not read it as silence.
    #[test]
    fn ssh_pipe_recv_fails_when_the_reader_ends_without_the_marker() {
        struct PanickingRead;
        impl io::Read for PanickingRead {
            #[expect(clippy::panic, reason = "the reader thread's death is what is tested")]
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                panic!("the stdout reader dies (an expected panic in this test)");
            }
        }
        let kill: Arc<Kills> = Arc::new(Kills::default());
        let writer = FrameWriter::new(Box::new(io::sink()), kill, Duration::from_secs(30));
        let end = PipeEnd::open(&Attempts::default());
        let mut pipe = SshPipe::new(writer, PanickingRead, end);
        let asked = Instant::now();
        let got = pipe.recv(Some(Duration::from_secs(2)));
        let took = asked.elapsed();
        assert!(
            matches!(&got, Err(LinkError::Recv(why)) if why == READER_LOST),
            "{got:?}"
        );
        assert!(took < Duration::from_secs(1), "took {took:?}");
    }

    /// An old pipe's EOF that lands after the next attempt opened
    /// leaves the live link alive; the current pipe's EOF still marks it dead,
    /// and the old attempt stays dead.
    #[test]
    fn a_late_eof_from_an_old_attempt_never_marks_the_live_link_dead() {
        let attempts = Attempts::default();
        let old = PipeEnd::open(&attempts);
        let old_dead = attempts.current();
        let current = PipeEnd::open(&attempts);
        let dead = attempts.current();
        old.mark_dead();
        assert!(!dead.is_set(), "the old pipe's end is not this link's");
        current.mark_dead();
        assert!(dead.is_set());
        assert!(old_dead.is_set());
    }

    /// An asker that says it was asked, then answers what the test sends: the point where a request has taken its
    /// link attempt and waits, so the test orders the link's deaths and redials around it without sleeping.
    struct Gate {
        asked: Mutex<mpsc::Sender<()>>,
        answer: Mutex<mpsc::Receiver<tulpa_hands::AskAnswer>>,
    }

    impl tulpa_hands::Asker for Gate {
        fn ask(&self, _what: &str, _timeout: Duration) -> tulpa_hands::AskAnswer {
            if let Ok(asked) = self.asked.lock() {
                let _told: Result<(), mpsc::SendError<()>> = asked.send(());
            }
            self.answer
                .lock()
                .ok()
                .and_then(|answer| answer.recv_timeout(Duration::from_secs(60)).ok())
                .unwrap_or(tulpa_hands::AskAnswer::Expired)
        }
    }

    /// The hands over `attempts` with two asked commands, `sleepy` (30 s) and `hi`, behind a [`Gate`]: the engine
    /// on its own thread, the gate's two ends, and the reply.
    struct Pending {
        asked: mpsc::Receiver<()>,
        answer: mpsc::Sender<tulpa_hands::AskAnswer>,
        replied: mpsc::Receiver<Vec<u8>>,
    }

    fn ask_and_run(
        attempts: &Arc<Attempts>,
        dir: &Path,
        command: &str,
    ) -> Result<Pending, Box<dyn std::error::Error>> {
        fs::create_dir_all(dir.join("proj"))?;
        let policy = tulpa_hands::Policy::from_toml(
            "roots = [\"~/proj\"]\nask_timeout_secs = 60\n\
             [[command]]\nname = \"sleepy\"\nargv = [\"/bin/sleep\", \"30\"]\ncwd = \"~/proj\"\nmode = \"ask\"\n\
             [[command]]\nname = \"hi\"\nargv = [\"/bin/echo\", \"hi\"]\ncwd = \"~/proj\"\nmode = \"ask\"\n",
            dir,
            None,
        )?;
        let (on_ask, asked) = mpsc::channel();
        let (answer, answer_rx) = mpsc::channel();
        let gate: Arc<dyn tulpa_hands::Asker> = Arc::new(Gate {
            asked: Mutex::new(on_ask),
            answer: Mutex::new(answer_rx),
        });
        let journal: Arc<dyn tulpa_hands::Journal> = Arc::new(Notes::default());
        let engine = tulpa_hands::Engine::new(
            policy,
            gate,
            journal,
            Arc::new(Stop::new()),
            Arc::clone(attempts),
        );
        let line = serde_json::json!({"id": "1", "verb": "run", "arg": command}).to_string();
        let (reply, replied) = mpsc::channel();
        thread::spawn(move || {
            let _told = reply.send(engine.handle(line.as_bytes()));
        });
        asked.recv_timeout(Duration::from_secs(60))?;
        Ok(Pending {
            asked,
            answer,
            replied,
        })
    }

    impl Pending {
        /// Clicks Allow and reads the reply's `result` or `err`.
        fn allow(self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
            drop(self.asked);
            self.answer.send(tulpa_hands::AskAnswer::Allow)?;
            let reply: serde_json::Value =
                serde_json::from_slice(&self.replied.recv_timeout(Duration::from_secs(60))?)?;
            Ok(reply
                .get("result")
                .or_else(|| reply.get("err"))
                .cloned()
                .unwrap_or(reply))
        }
    }

    /// A request killed by the death of the link attempt it came on says the link went down, even when a redial
    /// opened the next attempt before the run read why it was killed: that attempt stays dead, and a redial is not
    /// a resume (`Paused` is the operator's, never the link's).
    #[test]
    fn a_link_death_kill_reads_as_link_down_across_a_redial()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_path("redial-kill");
        let attempts = Arc::new(Attempts::default());
        let first = PipeEnd::open(&attempts);
        let asked = ask_and_run(&attempts, &dir, "sleepy")?;
        // The request holds attempt 1 and waits for the click; attempt 1 dies, attempt 2 opens, then the run.
        first.mark_dead();
        let _second = PipeEnd::open(&attempts);
        let started = Instant::now();
        let said = asked.allow()?;
        assert_eq!(said, serde_json::json!("link down (killed)"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A request on attempt 2 is not killed by attempt 1's death landing late; attempt 2's own death still kills
    /// a request on it (the positive control: the same gate, the same command).
    #[test]
    fn an_old_attempts_late_death_spares_a_request_on_the_new_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_path("late-death");
        let attempts = Arc::new(Attempts::default());
        let first = PipeEnd::open(&attempts);
        let second = PipeEnd::open(&attempts);
        let asked = ask_and_run(&attempts, &dir, "hi")?;
        first.mark_dead();
        assert_eq!(asked.allow()?, serde_json::json!("exit 0\nhi\n"));
        let asked = ask_and_run(&attempts, &dir, "hi")?;
        second.mark_dead();
        assert_eq!(asked.allow()?, serde_json::json!("link down (killed)"));
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A dialer scripted with attempt outcomes; a `Dropped` outcome simulates a
    /// connection that reached «connected» first, and the script stops the
    /// supervisor once it runs out.
    #[derive(Debug)]
    struct ScriptedDialer<'a> {
        outcomes: RefCell<VecDeque<Result<End, LinkError>>>,
        stop: &'a Stop,
    }

    impl Dialer for ScriptedDialer<'_> {
        fn attempt(&self, on_connected: &mut dyn FnMut(), _stop: &Stop) -> Result<End, LinkError> {
            let outcome = self
                .outcomes
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(End::Dropped));
            if matches!(outcome, Ok(End::Dropped)) {
                on_connected();
            }
            if self.outcomes.borrow().is_empty() {
                self.stop.set();
            }
            outcome
        }
    }

    #[test]
    fn supervise_reports_states_resets_backoff_and_notes_everything()
    -> Result<(), Box<dyn std::error::Error>> {
        let log_path = temp_path("supervise.log");
        let stop = Stop::new();
        let outcomes: VecDeque<Result<End, LinkError>> = VecDeque::from([
            Err(LinkError::Config("synthetic".to_owned())),
            Ok(End::HelloRefused),
            Ok(End::Dropped),
            Ok(End::HelloRefused),
        ]);
        let dialer = ScriptedDialer {
            outcomes: RefCell::new(outcomes),
            stop: &stop,
        };
        let mut states: Vec<LinkState> = Vec::new();
        let mut rests: Vec<Duration> = Vec::new();
        supervise(
            &dialer,
            &log_path,
            None,
            &stop,
            &mut |state| states.push(state),
            &mut |wait: Duration| {
                rests.push(wait);
                if rests.len() >= 3 {
                    stop.set();
                }
            },
        );
        assert_eq!(
            rests.iter().map(Duration::as_secs).collect::<Vec<_>>(),
            vec![5, 10, 5],
            "the ladder advances on failures and resets after a connected attempt"
        );
        assert_eq!(
            states
                .iter()
                .filter(|state| **state == LinkState::Connected)
                .count(),
            1,
            "exactly one attempt reached connected"
        );
        assert_eq!(
            states.first().copied(),
            Some(LinkState::Connecting),
            "every attempt begins as connecting"
        );
        let log_text = fs::read_to_string(&log_path)?;
        assert!(log_text.contains("link: dialing"));
        assert!(log_text.contains("link configuration: synthetic"));
        assert!(log_text.contains("unknown or wrong token"));
        assert!(log_text.contains("link: connected (hello accepted)"));
        fs::remove_file(&log_path)?;
        Ok(())
    }

    /// Fails its first attempt before the hello, connects on its second and loses that link, then
    /// stops the supervisor.
    struct DownDialer<'a> {
        attempts: RefCell<u32>,
        stop: &'a Stop,
    }

    impl Dialer for DownDialer<'_> {
        fn attempt(&self, on_connected: &mut dyn FnMut(), _stop: &Stop) -> Result<End, LinkError> {
            let mut attempts = self.attempts.borrow_mut();
            *attempts = attempts.saturating_add(1);
            if *attempts == 1 {
                return Ok(End::Io("flow-link-1 no route".to_owned()));
            }
            on_connected();
            self.stop.set();
            Ok(End::Io("flow-link-1 reset by peer".to_owned()))
        }
    }

    #[test]
    fn flow_logs_link_down_connect_failed_retry_and_stop() {
        crate::flow::test_global();
        let stop = Stop::new();
        let dialer = DownDialer {
            attempts: RefCell::new(0),
            stop: &stop,
        };
        supervise(
            &dialer,
            &temp_path("flow-link.log"),
            None,
            &stop,
            &mut |_| {},
            &mut |_| {},
        );
        let lines: Vec<(String, String)> = crate::flow::test_entries(|e| {
            fl(e, "/kind") == "link" && fl(e, "/detail").to_string().contains("flow-link-1")
        })
        .iter()
        .map(|e| {
            assert_eq!(
                (fl(e, "/dir"), fl(e, "/frame")),
                ("note".into(), serde_json::Value::Null)
            );
            (
                fl(e, "/why").as_str().unwrap_or_default().to_owned(),
                fl(e, "/delay_s").as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
        let whys: Vec<&str> = lines.iter().map(|(why, _)| why.as_str()).collect();
        assert_eq!(
            whys,
            ["connect-failed", "retry", "down", "stopped"],
            "{lines:?}"
        );
        assert_eq!(lines.get(1).map(|(_, delay)| delay.as_str()), Some("5"));
        let down = crate::flow::test_entries(|e| {
            fl(e, "/why") == "down"
                && fl(e, "/detail")
                    .to_string()
                    .contains("flow-link-1 reset by peer")
        });
        assert_eq!(
            down.len(),
            1,
            "the down line carries the attempt's end: {down:?}"
        );
    }

    #[test]
    fn supervise_writes_the_check_in_only_once_the_hello_is_accepted()
    -> Result<(), Box<dyn std::error::Error>> {
        let log_path = temp_path("checkin.log");
        let marker = temp_path("support").join("checkin");
        let checkin = CheckIn::new(marker.clone(), "0.1.0", "abc", Some(7), 42);
        let stop = Stop::new();
        let refused = ScriptedDialer {
            outcomes: RefCell::new(VecDeque::from([Ok(End::HelloRefused)])),
            stop: &stop,
        };
        supervise(
            &refused,
            &log_path,
            Some(&checkin),
            &stop,
            &mut |_| {},
            &mut |_| {},
        );
        assert!(!marker.exists(), "a refused hello writes no check-in");

        stop.clear();
        let before = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let accepted = ScriptedDialer {
            outcomes: RefCell::new(VecDeque::from([Ok(End::Dropped)])),
            stop: &stop,
        };
        supervise(
            &accepted,
            &log_path,
            Some(&checkin),
            &stop,
            &mut |_| {},
            &mut |_| {},
        );
        let text = fs::read_to_string(&marker)?;
        let epoch: u64 = text
            .lines()
            .find_map(|line| line.strip_prefix("epoch="))
            .ok_or("no epoch line")?
            .parse()?;
        assert!(
            epoch >= before,
            "the check-in carries the time of the accepted hello"
        );
        assert!(text.contains("exe_inode=7\n"));
        fs::remove_file(&marker)?;
        fs::remove_file(&log_path)?;
        Ok(())
    }

    const TICKET: &str = "T1cket-0123456789abcdefABCDEF_xy";

    #[test]
    fn data_hello_line_is_the_control_hello_plus_ticket_and_offset()
    -> Result<(), Box<dyn std::error::Error>> {
        let ticket = Ticket::parse(TICKET).ok_or("ticket")?;
        assert_eq!(
            data_hello_line("dev-1", GOOD_TOKEN, &ticket, Some(42)),
            format!(
                "{{\"hello\":\"dev-1\",\"token\":\"{GOOD_TOKEN}\",\"data\":\"{TICKET}\",\"offset\":42}}\n"
            )
            .into_bytes()
        );
        let get = data_hello_line("dev-1", GOOD_TOKEN, &ticket, None);
        let value: serde_json::Value = serde_json::from_slice(&get)?;
        assert_eq!(
            value,
            serde_json::json!({"hello": "dev-1", "token": GOOD_TOKEN, "data": TICKET}),
            "offset is for put only"
        );
        Ok(())
    }

    /// The data dial runs the link's own command: the same options, target and
    /// alias, and on the direct route the same overrides.
    #[test]
    fn data_connections_dial_like_the_link() -> Result<(), Box<dyn std::error::Error>> {
        let spec = DialSpec::new("work-box", "127.0.0.1", 18030, "dev-1", temp_path("t"))?;
        let args = |route: &Route| -> Vec<String> {
            ssh_command(&spec, route)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let jump = args(&Route::Jump);
        assert_eq!(
            jump.get(jump.len().saturating_sub(3)..),
            Some(
                &[
                    "-W".to_owned(),
                    "127.0.0.1:18030".to_owned(),
                    "work-box".to_owned()
                ][..]
            )
        );
        assert!(jump.contains(&"StrictHostKeyChecking=yes".to_owned()));
        assert!(jump.contains(&"BatchMode=yes".to_owned()));
        let direct = args(&Route::Direct("192.0.2.9".to_owned()));
        assert!(direct.contains(&"ProxyJump=none".to_owned()));
        assert!(direct.contains(&"HostName=192.0.2.9".to_owned()));
        let missing = SshDataDial::new(spec, temp_path("log"), Arc::new(Mutex::new(None)));
        let ticket = Ticket::parse(TICKET).ok_or("ticket")?;
        assert!(
            missing.dial(&ticket, None).is_err(),
            "no token file, no child"
        );
        Ok(())
    }

    struct Allow;

    impl tulpa_hands::Asker for Allow {
        fn ask(&self, _what: &str, _timeout: Duration) -> tulpa_hands::AskAnswer {
            tulpa_hands::AskAnswer::Allow
        }
    }

    #[derive(Default)]
    struct Notes(Mutex<Vec<String>>);

    impl tulpa_hands::Journal for Notes {
        fn record(&self, line: &str) {
            if let Ok(mut lines) = self.0.lock() {
                lines.push(line.to_owned());
            }
        }
    }

    /// A child standing in for `ssh -W` with the hub at its far end: the same
    /// spawning, halves and closing as the real data dial.
    struct ScriptDial {
        script: &'static str,
        dir: PathBuf,
    }

    impl DataDial for ScriptDial {
        fn dial(&self, ticket: &Ticket, offset: Option<u64>) -> io::Result<DataConn> {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(self.script).env("DIR", &self.dir);
            let hello = data_hello_line("dev-1", GOOD_TOKEN, ticket, offset);
            spawn_data(command, &self.dir.join("log"), &hello)
        }
    }

    /// The hub's side of a put: the hello noted, the file's bytes, then the
    /// Mac's verdict until its half-close. No EOF toward the Mac first: through
    /// `ssh -W` nothing gets back after one, so the Mac must answer at `size`.
    const PUT_HUB: &str = "IFS= read -r hello; printf '%s\\n' \"$hello\" > \"$DIR/hello\"; cat \"$DIR/payload\"; cat > \"$DIR/verdict\"";
    /// The hub's side of a get: the hello noted, the bytes until the Mac's
    /// half-close, then a verdict.
    const GET_HUB: &str = "IFS= read -r hello; printf '%s\\n' \"$hello\" > \"$DIR/hello\"; cat > \"$DIR/got\"; printf '{\"ok\":true}\\n'";

    fn wait_idle(xfer: &tulpa_hands::Xfer) -> bool {
        for _ in 0..400 {
            if !xfer.is_busy() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// The hands in full mode with the data stream over `dial`, its slot and its log.
    type Served = (tulpa_hands::Engine, Arc<tulpa_hands::Xfer>, Arc<Notes>);

    fn engine_over(
        dial: Arc<dyn DataDial>,
        dir: &Path,
    ) -> Result<Served, Box<dyn std::error::Error>> {
        let policy = tulpa_hands::Policy::from_toml("roots = []", dir, None)?;
        let xfer = Arc::new(tulpa_hands::Xfer::new(dial, Box::new(|| {})));
        let notes = Arc::new(Notes::default());
        let journal: Arc<dyn tulpa_hands::Journal> = Arc::<Notes>::clone(&notes);
        let asker: Arc<dyn tulpa_hands::Asker> = Arc::new(Allow);
        let engine = tulpa_hands::Engine::new(
            policy,
            asker,
            journal,
            Arc::new(Stop::new()),
            Arc::default(),
        )
        .with_full(Arc::new(std::sync::atomic::AtomicBool::new(true)))
        .with_xfer(Arc::clone(&xfer));
        Ok((engine, xfer, notes))
    }

    /// End to end through a real child: the hands take a put and send a get over
    /// the data connection this module spawns; the half-close and the verdict
    /// cross the child's pipes.
    #[test]
    fn a_child_carries_a_put_and_a_get_with_their_verdicts()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = temp_path("xfer");
        fs::create_dir_all(&dir)?;
        let payload: Vec<u8> = (0_u32..200_000).flat_map(u32::to_le_bytes).collect();
        fs::write(dir.join("payload"), &payload)?;
        let sha = {
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg("(sha256sum \"$0\" 2>/dev/null || shasum -a 256 \"$0\") | cut -c1-64")
                .arg(dir.join("payload"))
                .output()?;
            String::from_utf8(out.stdout)?.trim().to_owned()
        };
        assert_eq!(sha.len(), 64, "{sha}");

        let put_dial: Arc<dyn DataDial> = Arc::new(ScriptDial {
            script: PUT_HUB,
            dir: dir.clone(),
        });
        let (engine, xfer, notes) = engine_over(put_dial, &dir)?;
        let target = dir.join("landed.bin");
        let line = serde_json::json!({"id": "p1", "verb": "xfer.put", "arg": {
            "ticket": TICKET, "path": target, "size": payload.len(), "sha256": sha}});
        let ack: serde_json::Value =
            serde_json::from_slice(&engine.handle(line.to_string().as_bytes()))?;
        assert_eq!(ack.get("ok"), Some(&serde_json::json!(true)), "{ack}");
        assert!(wait_idle(&xfer));
        assert_eq!(fs::read(&target)?, payload);
        let hello: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("hello"))?)?;
        assert_eq!(hello.get("data"), Some(&serde_json::json!(TICKET)));
        assert_eq!(hello.get("offset"), Some(&serde_json::json!(0)));
        let verdict: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("verdict"))?)?;
        assert_eq!(
            verdict,
            serde_json::json!({"ok": true, "size": payload.len(), "sha256": sha})
        );

        let get_dial: Arc<dyn DataDial> = Arc::new(ScriptDial {
            script: GET_HUB,
            dir: dir.clone(),
        });
        let (engine, xfer, get_notes) = engine_over(get_dial, &dir)?;
        let line = serde_json::json!({"id": "g1", "verb": "xfer.get", "arg": {
            "ticket": TICKET, "path": target, "offset": 1000}});
        let ack: serde_json::Value =
            serde_json::from_slice(&engine.handle(line.to_string().as_bytes()))?;
        assert_eq!(ack.get("sha256"), Some(&serde_json::json!(sha)), "{ack}");
        assert!(wait_idle(&xfer));
        assert_eq!(
            fs::read(dir.join("got"))?,
            payload.get(1000..).unwrap_or_default()
        );
        let log = get_notes.0.lock().map(|l| l.join("\n")).unwrap_or_default();
        assert!(log.contains("xfer: get") && log.contains(": ok,"), "{log}");
        let log = notes.0.lock().map(|l| l.join("\n")).unwrap_or_default();
        assert!(!log.contains(TICKET), "{log}");
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Lingering waits while the child runs, up to the grace, then kills it; a
    /// child that has exited is not waited for.
    #[test]
    fn a_data_child_is_waited_for_then_killed() -> Result<(), Box<dyn std::error::Error>> {
        let stalled = DataChild(Mutex::new(Command::new("/bin/sleep").arg("30").spawn()?));
        let started = std::time::Instant::now();
        stalled.linger(Duration::from_millis(300));
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "waited while it ran"
        );
        let mut ended = false;
        for _ in 0..100 {
            ended = stalled
                .0
                .lock()
                .is_ok_and(|mut child| matches!(child.try_wait(), Ok(Some(_))));
            if ended {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ended, "killed past the grace");
        let quick = DataChild(Mutex::new(
            Command::new("/bin/sh").arg("-c").arg("exit 0").spawn()?,
        ));
        let started = std::time::Instant::now();
        quick.linger(Duration::from_secs(10));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no wait once it exited"
        );
        Ok(())
    }
}
