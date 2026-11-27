//! Serving one side of a link (#0375): the hub asks a device over the link to be one side of a path
//! between two devices' localhosts.
//!
//! The ask comes from a steward's or an operator's `link.open`; the streams of the two sides meet in
//! the hub, which splices them without reading them.
//!
//! - **client** (`link.serve-client`): this Mac listens on its own `127.0.0.1:<port>`, tcp or udp. Every
//!   connection (tcp) or source peer (udp) dials its own stream to the hub — on its own thread, so no
//!   dial's handshake ever blocks the listening loop; a new udp peer's first datagrams hold for its
//!   stream, bounded and drop-oldest, and ride in order once it is up. Stream 0, dialed first, is
//!   this side's control stream, held for the link's life and carrying nothing.
//! - **host** (`link.serve-host`): this Mac delivers to its own `127.0.0.1:<port>`. The hub says
//!   `open <n>` on the control stream for every client stream; the host dials its stream n and joins it
//!   to the local port — a tcp connection, or for udp a bound socket of its own, so replies find the
//!   peer they belong to — beside the control reader, so one stream's setup never delays the next
//!   `open`.
//! - A udp datagram travels a stream framed: a big-endian `u16` length, then that many bytes — the same
//!   frame the hub passes through unread.
//!
//! Limits, all local: a port must be on this Mac's own `[linkserve]` list, whatever the hub granted; a
//! client side carries at most [`PEERS_MAX`] concurrent peers, the rest dropped and logged (the hub
//! drops past 16 too, akira 18:05Z); every serve ends at its time limit, when the control stream ends
//! (the link ended — the hub ends the link when either device goes offline, and that closes every
//! stream), at Pause all and Quit. Client udp peers quiet for [`PEER_QUIET`] are dropped so the link's
//! streams come free. Serves, drops and closes go to the activity log with byte counts — never a
//! secret, never a byte carried.
//!
//! The streams are `ssh -W` children like the data stream's ([`crate::link::SshLinkDial`]): a Tulpa
//! killed hard leaves them behind briefly, but the hub ends the link when the device's own link drops,
//! and the streams' end closes the children — they need no pid file of their own.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tulpa_hands::{Closer, Journal};
use tulpa_stop::Stop;

use crate::link::{LineHandler, refuse_line};

/// How long a client udp peer may stay quiet before its stream is dropped: replies flow hub→peer, so
/// quiet is measured from the last datagram either way.
pub const PEER_QUIET: Duration = Duration::from_secs(60);
/// Peers one client side carries at once — the same cap the hub holds per link (akira 18:05Z): the
/// next peer is dropped and logged, not queued.
pub const PEERS_MAX: usize = 16;
/// How often the serve loops look at their stop flag and deadline.
const TICK: Duration = Duration::from_millis(100);
/// Datagrams one new peer may hold while its stream dials, and the bytes they may total: past
/// either, the oldest goes — a burst that outruns the dial keeps its newest packets (a game's
/// latest state), never a growing hold.
const PEER_PENDING_MAX: usize = 64;
const PEER_PENDING_BYTES: usize = 256 * 1024;
/// How often the client's udp read wakes when quiet, to sweep its quiet peers and see the stop
/// flag. A datagram wakes it at once: the read blocks, so the wait a sleeping poll put before
/// every datagram is gone.
const CLIENT_WAKE: Duration = Duration::from_millis(100);
/// The longest a serve may last (the hub's own link limit).
const TTL_MAX_S: u64 = 6 * 3_600;
/// How long a serve waits for its streams to close before it ends anyway.
const STREAMS_DRAIN: Duration = Duration::from_secs(3);
/// The longest a udp datagram can be on the wire.
const DATAGRAM_MAX: usize = 65_507;

/// Which protocol a link carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Proto {
    /// Connections, spliced byte for byte.
    Tcp,
    /// Datagrams, framed one frame per datagram.
    Udp,
}

impl Proto {
    /// The word the hub's argument carries.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }
}

/// Which side of a link this Mac serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Side {
    /// This Mac listens; its peers' streams start here.
    Client,
    /// This Mac delivers; the hub says when a stream starts.
    Host,
}

impl Side {
    /// The word the hub's verb carries.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Host => "host",
        }
    }
}

/// One stream to the hub, its hello (link id, the side's secret, the stream number) already said by
/// the dialer. Stream 0 is a side's control stream; the hub closes it when the link ends.
#[non_exhaustive]
pub struct StreamConn {
    /// Bytes the hub sent; EOF means the stream (usually the link) ended.
    pub from_hub: Box<dyn Read + Send>,
    /// Bytes to the hub; dropped, the hub sees EOF.
    pub to_hub: Box<dyn Write + Send>,
    /// Ends the stream from any thread.
    pub child: Arc<dyn Closer>,
}

impl std::fmt::Debug for StreamConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The halves are trait objects with nothing to say; the stream's identity is its dial.
        f.debug_struct("StreamConn").finish_non_exhaustive()
    }
}

/// Dials a link's streams on the link's own route ([`crate::link::SshLinkDial`]; tests use fakes).
pub trait LinkDial: Send + Sync {
    /// One stream to the hub, its hello said.
    ///
    /// # Errors
    ///
    /// When the stream's child could not be started or its hello not written; the caller drops what
    /// asked for the stream (a connection, a peer's datagram) and carries on.
    fn dial(&self, link: &str, secret: &str, stream: u32) -> io::Result<StreamConn>;
}

/// A checked `link.serve-*` argument: the hub's JSON, against this Mac's own list.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Ask {
    /// The link's id, `l-` and eight hex digits.
    pub link: String,
    /// Which protocol the link carries.
    pub proto: Proto,
    /// The loopback port, on both the Mac's own list and the hub's.
    pub port: u16,
    /// How long the serve stays, the hub's ttl.
    pub ttl: Duration,
    /// This side's secret, said to the hub on every stream and never logged.
    pub secret: String,
}

fn hex_lower(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reads the hub's `{"link":…,"proto":…,"port":…,"ttl":…,"secret":…}` against `allowed`.
///
/// # Errors
///
/// A refusal fit for the reply: a malformed argument, a shape the hub never sends, a port not on this
/// Mac's `[linkserve]` list, a time limit past the hub's own.
pub fn parse_ask(arg: &str, allowed: &[u16]) -> Result<Ask, String> {
    #[derive(Deserialize)]
    struct Wire {
        link: String,
        proto: String,
        port: u16,
        ttl: u64,
        secret: String,
    }
    let wire =
        serde_json::from_str::<Wire>(arg).map_err(|_| "link: not the hub's JSON".to_owned())?;
    let id_ok = wire.link.len() == 10
        && wire.link.as_bytes().get(..2) == Some(b"l-")
        && hex_lower(wire.link.as_bytes().get(2..).unwrap_or(&[]));
    if !id_ok {
        return Err("link: the link id".to_owned());
    }
    if wire.secret.len() != 64 || !hex_lower(wire.secret.as_bytes()) {
        return Err("link: the side's secret".to_owned());
    }
    if !allowed.contains(&wire.port) {
        return Err(format!(
            "{} is not on this Mac's [linkserve] ports",
            wire.port
        ));
    }
    if wire.ttl == 0 || wire.ttl > TTL_MAX_S {
        return Err(format!("link: the time limit, 1..{TTL_MAX_S} s"));
    }
    let proto = Proto::parse(&wire.proto).ok_or_else(|| "link: the protocol".to_owned())?;
    Ok(Ask {
        link: wire.link,
        proto,
        port: wire.port,
        ttl: Duration::from_secs(wire.ttl),
        secret: wire.secret,
    })
}

/// One served side, as [`LinkServe::list`] and the menu show it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct View {
    /// The link's id.
    pub link: String,
    /// Which protocol it carries.
    pub proto: Proto,
    /// The loopback port.
    pub port: u16,
    /// Which side this Mac serves.
    pub side: Side,
    /// Seconds left before its time limit.
    pub left_s: u64,
    /// Peers carried: now, and of all time.
    pub peers: (u64, u64),
}

impl View {
    /// `l-1a2b3c4d tcp 25565 client — click to close`: the menu row.
    #[must_use]
    pub fn menu_line(&self) -> String {
        format!(
            "{} {} {} {} — click to close",
            self.link,
            self.proto.word(),
            self.port,
            self.side.word()
        )
    }

    /// `l-1a2b3c4d tcp 25565 client (4:12 left, 3 peer)`: the log's line.
    #[must_use]
    pub fn line(&self) -> String {
        let left = format!("{}:{:02}", self.left_s / 60, self.left_s % 60);
        let (now, all) = self.peers;
        let peers = if all > 0 {
            format!("{now} of {all} peer")
        } else {
            "0 peer".to_owned()
        };
        format!(
            "{} {} {} {} ({left} left, {peers})",
            self.link,
            self.proto.word(),
            self.port,
            self.side.word()
        )
    }
}

/// One serve's shared state: its stop flag, its end-once claim, the reason it ended, the control
/// stream's child, its counters, and the side's secret — in memory only, never logged.
struct Shared {
    link: String,
    secret: String,
    stop: AtomicBool,
    done: AtomicBool,
    why: Mutex<String>,
    control: Mutex<Option<Arc<dyn Closer>>>,
    side: Side,
    proto: Proto,
    port: u16,
    until: Instant,
    peers_now: AtomicUsize,
    /// Tcp dials in flight (their streams not yet counted in `peers_now`), so the cap holds
    /// while connections wait on their dials.
    dialing: AtomicUsize,
    peers_all: AtomicU64,
}

impl Shared {
    fn new(
        link: String,
        secret: String,
        side: Side,
        proto: Proto,
        port: u16,
        ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            link,
            secret,
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            why: Mutex::new(String::new()),
            control: Mutex::new(None),
            side,
            proto,
            port,
            until: Instant::now().checked_add(ttl).unwrap_or_else(Instant::now),
            peers_now: AtomicUsize::new(0),
            dialing: AtomicUsize::new(0),
            peers_all: AtomicU64::new(0),
        })
    }

    /// The control stream's child, closed when the serve ends from anywhere.
    fn set_control(&self, child: Arc<dyn Closer>) {
        if let Ok(mut held) = self.control.lock() {
            *held = Some(child);
        }
    }

    fn close_control(&self) {
        if let Ok(mut held) = self.control.lock()
            && let Some(child) = held.take()
        {
            child.close();
        }
    }

    /// One stream ended for good: its place in the concurrent count is freed. Without this the
    /// client's tcp cap ratcheted — 16 cumulative connections, each long closed, and the side
    /// refused every new one until the ttl (akira 04:4xZ B1).
    fn stream_done(&self) {
        self.peers_now.fetch_sub(1, Ordering::SeqCst);
    }

    /// Names the serve's end before its threads notice it, so the log says the true cause.
    fn name_why(&self, why: &str) {
        if let Ok(mut held) = self.why.lock()
            && held.is_empty()
        {
            why.clone_into(&mut held);
        }
    }

    fn reason(&self, fallback: &str) -> String {
        self.why.lock().map_or_else(
            |_| fallback.to_owned(),
            |held| {
                if held.is_empty() {
                    fallback.to_owned()
                } else {
                    held.clone()
                }
            },
        )
    }
}

/// Every served link side, and the rules they serve by.
pub struct LinkServe {
    allowed: Vec<u16>,
    dial: Arc<dyn LinkDial>,
    paused: Arc<Stop>,
    journal: Arc<dyn Journal>,
    changed: Arc<dyn Fn() + Send + Sync>,
    serving: Mutex<HashMap<String, Arc<Shared>>>,
    peer_quiet: Duration,
}

impl std::fmt::Debug for LinkServe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinkServe")
            .field("allowed", &self.allowed)
            .finish_non_exhaustive()
    }
}

impl LinkServe {
    /// Serves on `allowed` ports (empty: none at all), dialing with `dial`, refused while `paused`,
    /// logged to `journal`; `changed` is called whenever the served set changes (the menu).
    #[must_use]
    pub fn new(
        allowed: Vec<u16>,
        dial: Arc<dyn LinkDial>,
        paused: Arc<Stop>,
        journal: Arc<dyn Journal>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            allowed,
            dial,
            paused,
            journal,
            changed,
            serving: Mutex::new(HashMap::new()),
            peer_quiet: PEER_QUIET,
        }
    }

    /// Tests: a client udp peer's quiet time before its stream is dropped.
    #[must_use]
    pub const fn with_peer_quiet(mut self, quiet: Duration) -> Self {
        self.peer_quiet = quiet;
        self
    }

    /// Whether a line's verb is one of the serves'.
    #[must_use]
    pub fn owns(verb: &str) -> bool {
        verb == "link.serve-client" || verb == "link.serve-host"
    }

    /// Answers one `link.serve-*` line. The hub is the only asker (the dispatcher's own door refuses
    /// these verbs), and its lines arrive on the one link thread, one at a time.
    pub fn handle(self: &Arc<Self>, line: &[u8]) -> Vec<u8> {
        #[derive(Deserialize)]
        struct Request {
            id: String,
            verb: String,
            #[serde(default)]
            arg: Option<serde_json::Value>,
        }
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            self.journal.record("link serve: unparseable line refused");
            return refuse_line(line);
        };
        if request.id.is_empty()
            || request.id.len() > 64
            || request.id.chars().any(char::is_control)
        {
            self.journal.record("link serve: bad id refused");
            return refuse_line(line);
        }
        let Some(serde_json::Value::String(arg)) = request.arg.as_ref() else {
            return reply(&request.id, Err("arg".to_owned()));
        };
        let side = match request.verb.as_str() {
            "link.serve-client" => Side::Client,
            "link.serve-host" => Side::Host,
            _ => return reply(&request.id, Err("verb".to_owned())),
        };
        reply(&request.id, self.serve(side, arg))
    }

    /// Serves `side` of a link: everything local is up before the ok, so a refusal is the truth.
    fn serve(self: &Arc<Self>, side: Side, arg: &str) -> Result<String, String> {
        if self.paused.is_set() {
            return Err("paused".to_owned());
        }
        if self.allowed.is_empty() {
            return Err("link: off on this Mac (no [linkserve] ports)".to_owned());
        }
        let ask = parse_ask(arg, &self.allowed)
            .inspect_err(|why| self.journal.record(&format!("link serve: refused: {why}")))?;
        let busy = self
            .serving
            .lock()
            .is_ok_and(|held| held.contains_key(&ask.link));
        if busy {
            return Err(format!("busy: {} is already served", ask.link));
        }
        let shared = Shared::new(
            ask.link.clone(),
            ask.secret.clone(),
            side,
            ask.proto,
            ask.port,
            ask.ttl,
        );
        let started = match side {
            Side::Client if ask.proto == Proto::Tcp => self.client_tcp(&ask, &shared),
            Side::Client => self.client_udp(&ask, &shared),
            Side::Host if ask.proto == Proto::Tcp => self.host_side(&ask, &shared, HostJoin::Tcp),
            Side::Host => self.host_side(&ask, &shared, HostJoin::Udp),
        };
        started?;
        if let Ok(mut held) = self.serving.lock() {
            // A serve that ended between its setup and now (its control stream went at once) is not
            // left behind as a row that never goes.
            if shared.done.load(Ordering::SeqCst) {
                held.remove(&ask.link);
            } else {
                held.insert(ask.link.clone(), shared);
            }
        }
        self.journal.record(&format!(
            "link serve: {} {} {} {} opened",
            ask.link,
            ask.proto.word(),
            ask.port,
            side.word()
        ));
        (self.changed)();
        Ok(format!(
            "{} {} {} served",
            ask.link,
            ask.proto.word(),
            ask.port
        ))
    }

    /// Ends every served side (`why` names the cause in the log): Pause all, Quit, the loop's end.
    pub fn close_all(&self, why: &str) {
        if let Ok(held) = self.serving.lock() {
            for shared in held.values() {
                shared.name_why(why);
                shared.stop.store(true, Ordering::SeqCst);
                shared.close_control();
            }
        }
    }

    /// Waits up to `for_at_most` for every serve to be down and gone (the loops poll at `TICK`).
    pub fn drain(&self, for_at_most: Duration) {
        let deadline = Instant::now()
            .checked_add(for_at_most)
            .unwrap_or_else(Instant::now);
        while Instant::now() < deadline {
            if let Ok(held) = self.serving.lock()
                && held.is_empty()
            {
                return;
            }
            thread::sleep(TICK);
        }
    }

    /// Ends one serve by its link id (the menu's row); `false`: no such serve.
    pub fn close(&self, link: &str, why: &str) -> bool {
        let Some(shared) = self
            .serving
            .lock()
            .ok()
            .and_then(|held| held.get(link).cloned())
        else {
            return false;
        };
        shared.name_why(why);
        shared.stop.store(true, Ordering::SeqCst);
        shared.close_control();
        true
    }

    /// The served sides, for the menu.
    #[must_use]
    pub fn list(&self) -> Vec<View> {
        let Ok(held) = self.serving.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut views: Vec<View> = held
            .values()
            .map(|s| View {
                link: s.link.clone(),
                proto: s.proto,
                port: s.port,
                side: s.side,
                left_s: s
                    .until
                    .checked_duration_since(now)
                    .map_or(0, |left| left.as_secs()),
                peers: (
                    u64::try_from(s.peers_now.load(Ordering::SeqCst)).unwrap_or(0),
                    s.peers_all.load(Ordering::SeqCst),
                ),
            })
            .collect();
        views.sort_by(|a, b| a.link.cmp(&b.link));
        views
    }

    /// The one end: claimed once, everything closed, the row gone, the log saying the true cause.
    fn end(self: &Arc<Self>, shared: &Arc<Shared>, fallback: &str) {
        if shared.done.swap(true, Ordering::SeqCst) {
            return;
        }
        shared.stop.store(true, Ordering::SeqCst);
        shared.close_control();
        let reason = shared.reason(fallback);
        if let Ok(mut held) = self.serving.lock() {
            held.remove(&shared.link);
        }
        self.journal
            .record(&format!("link serve: {} ended ({reason})", shared.link));
        (self.changed)();
    }

    /// The control stream: held for the link's life, its end read here (a client side's carries
    /// nothing; a host side's `open n` lines are read by whoever holds the reader).
    fn hold_control(
        self: &Arc<Self>,
        shared: &Arc<Shared>,
        conn: StreamConn,
        lines: bool,
        join: Option<HostJoin>,
    ) {
        let StreamConn {
            from_hub,
            to_hub,
            child,
        } = conn;
        shared.set_control(Arc::clone(&child));
        let me = Arc::clone(self);
        let shared = Arc::clone(shared);
        thread::spawn(move || {
            // Held open for the link's life: dropping it would tell the hub this side went away.
            let _hold_write = to_hub;
            let mut reader = BufReader::new(from_hub);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if shared.stop.load(Ordering::SeqCst) {
                            return;
                        }
                        if lines && let Some(join) = join {
                            host_open(&me, &shared, &line, join);
                        }
                    }
                }
            }
            me.end(&shared, "the control stream ended");
            child.close();
        });
    }

    /// The client side of a tcp link: a listener on the Mac's own port; each connection dials one
    /// stream, at most [`PEERS_MAX`] at once.
    fn client_tcp(self: &Arc<Self>, ask: &Ask, shared: &Arc<Shared>) -> Result<(), String> {
        let listener = TcpListener::bind(("127.0.0.1", ask.port))
            .map_err(|e| format!("127.0.0.1:{} is in use ({e})", ask.port))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("the listener would not take peers ({e})"))?;
        let control = self
            .dial
            .dial(&ask.link, &ask.secret, 0)
            .map_err(|e| format!("the hub's stream would not open ({e})"))?;
        self.hold_control(shared, control, false, None);
        let me = Arc::clone(self);
        let shared = Arc::clone(shared);
        let dial = Arc::clone(&self.dial);
        thread::spawn(move || {
            let mut next: u32 = 0;
            loop {
                if shared.stop.load(Ordering::SeqCst) || Instant::now() >= shared.until {
                    break;
                }
                match listener.accept() {
                    Ok((local, peer)) => {
                        if shared
                            .peers_now
                            .load(Ordering::SeqCst)
                            .saturating_add(shared.dialing.load(Ordering::SeqCst))
                            >= PEERS_MAX
                        {
                            me.journal.record(&format!(
                                "link serve: {}: peer {peer} dropped: the link's {PEERS_MAX} streams are up",
                                shared.link
                            ));
                            continue;
                        }
                        let Some(n) = next.checked_add(1) else { break };
                        next = n;
                        shared.peers_all.fetch_add(1, Ordering::SeqCst);
                        shared.dialing.fetch_add(1, Ordering::SeqCst);
                        // The dial runs beside the loop (its handshake is seconds): the listener
                        // stays live for the next connection, and this one's bytes wait in its
                        // own kernel buffer until the stream is up.
                        let me = Arc::clone(&me);
                        let shared = Arc::clone(&shared);
                        let dial = Arc::clone(&dial);
                        thread::spawn(move || {
                            // The count spans the dial itself (its handshake is the seconds this
                            // cap exists for): it comes off only once the stream is up (counted
                            // as a peer first, so no window leaves it uncounted) or refused.
                            match dial.dial(&shared.link, &shared.secret, n) {
                                Ok(conn) => {
                                    shared.peers_now.fetch_add(1, Ordering::SeqCst);
                                    shared.dialing.fetch_sub(1, Ordering::SeqCst);
                                    carry_tcp(
                                        Arc::clone(&me.journal),
                                        Arc::clone(&shared),
                                        n,
                                        conn,
                                        local,
                                    );
                                }
                                Err(e) => {
                                    shared.dialing.fetch_sub(1, Ordering::SeqCst);
                                    me.journal.record(&format!(
                                        "link serve: {}: peer {peer} dropped: the stream would not open ({e})",
                                        shared.link
                                    ));
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                    Err(e) => {
                        shared.name_why(&format!("the listener broke ({e})"));
                        break;
                    }
                }
            }
            shared.name_why("its time limit");
            wait_streams(&shared);
            me.end(&shared, "its time limit");
        });
        Ok(())
    }

    /// The client side of a udp link: one bound socket; each source peer dials its own stream on
    /// its own thread and its datagrams travel framed. Peers quiet for `peer_quiet` are dropped.
    fn client_udp(self: &Arc<Self>, ask: &Ask, shared: &Arc<Shared>) -> Result<(), String> {
        let socket = UdpSocket::bind(("127.0.0.1", ask.port))
            .map_err(|e| format!("127.0.0.1:{} is in use ({e})", ask.port))?;
        socket
            .set_read_timeout(Some(CLIENT_WAKE))
            .map_err(|e| format!("the socket would not take peers ({e})"))?;
        let socket = Arc::new(socket);
        let control = self
            .dial
            .dial(&ask.link, &ask.secret, 0)
            .map_err(|e| format!("the hub's stream would not open ({e})"))?;
        self.hold_control(shared, control, false, None);
        let me = Arc::clone(self);
        let shared = Arc::clone(shared);
        let quiet = self.peer_quiet;
        thread::spawn(move || {
            let mut loop_ = UdpClientLoop {
                me: Arc::clone(&me),
                shared: Arc::clone(&shared),
                socket: Arc::clone(&socket),
                peers: Arc::new(Mutex::new(HashMap::new())),
                next: 0,
            };
            let mut buf = vec![0u8; DATAGRAM_MAX];
            let mut out = Vec::with_capacity(DATAGRAM_MAX + 2);
            loop {
                if shared.stop.load(Ordering::SeqCst) || Instant::now() >= shared.until {
                    break;
                }
                match socket.recv_from(&mut buf) {
                    Ok((len, peer)) => {
                        let datagram = buf.get(..len).unwrap_or(&[]);
                        if !loop_.carry(peer, datagram, &mut out) {
                            break;
                        }
                    }
                    // Quiet: the wake timeout fired (a quiet socket reads WouldBlock or
                    // TimedOut by platform) — sweep, then block again; no sleep sits before
                    // the next datagram.
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        loop_.sweep(quiet);
                    }
                    Err(e) => {
                        shared.name_why(&format!("the socket broke ({e})"));
                        break;
                    }
                }
            }
            loop_.close_peers();
            shared.name_why("its time limit");
            wait_streams(&shared);
            loop_.me.end(&loop_.shared, "its time limit");
        });
        Ok(())
    }

    /// The host side: no listener — the hub's `open n` lines on the control stream start each stream,
    /// joined to the local port (the hub already capped the client's peers at its own 16).
    fn host_side(
        self: &Arc<Self>,
        ask: &Ask,
        shared: &Arc<Shared>,
        join: HostJoin,
    ) -> Result<(), String> {
        let control = self
            .dial
            .dial(&ask.link, &ask.secret, 0)
            .map_err(|e| format!("the hub's stream would not open ({e})"))?;
        self.hold_control(shared, control, true, Some(join));
        Ok(())
    }
}

/// The host's joining of one `open n` stream: a tcp connection to the port, or a udp socket of its own.
#[derive(Clone, Copy)]
enum HostJoin {
    Tcp,
    Udp,
}

/// One `open n` line from the hub: dial this side's stream n and join it to the local port.
fn host_open(me: &Arc<LinkServe>, shared: &Arc<Shared>, line: &str, join: HostJoin) {
    let Some(n) = line
        .strip_prefix("open ")
        .and_then(|tail| tail.trim().parse::<u32>().ok())
    else {
        if !line.trim().is_empty() {
            me.journal.record(&format!(
                "link serve: {}: a line the hub never sends: {}",
                shared.link,
                line.trim()
            ));
        }
        return;
    };
    shared.peers_all.fetch_add(1, Ordering::SeqCst);
    // The dial runs beside the control reader (its handshake is seconds): the next `open m`
    // line is read at once, not behind this stream's setup.
    let me = Arc::clone(me);
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let Ok(conn) = me.dial.dial(&shared.link, &shared.secret, n) else {
            me.journal.record(&format!(
                "link serve: {}: stream {n} dropped: it would not open",
                shared.link
            ));
            return;
        };
        match join {
            HostJoin::Tcp => match TcpStream::connect(("127.0.0.1", shared.port)) {
                Ok(local) => {
                    shared.peers_now.fetch_add(1, Ordering::SeqCst);
                    carry_tcp(Arc::clone(&me.journal), Arc::clone(&shared), n, conn, local);
                }
                Err(e) => {
                    conn.child.close();
                    me.journal.record(&format!(
                        "link serve: {}: stream {n} dropped: 127.0.0.1:{} would not answer ({e})",
                        shared.link, shared.port
                    ));
                }
            },
            HostJoin::Udp => match UdpSocket::bind(("127.0.0.1", 0)) {
                Ok(reply) => {
                    shared.peers_now.fetch_add(1, Ordering::SeqCst);
                    carry_udp_host(
                        Arc::clone(&me.journal),
                        Arc::clone(&shared),
                        n,
                        conn,
                        reply,
                        shared.port,
                    );
                }
                Err(e) => {
                    conn.child.close();
                    me.journal.record(&format!(
                        "link serve: {}: stream {n} dropped: no reply socket ({e})",
                        shared.link
                    ));
                }
            },
        }
    });
}

/// A client udp peer's half of its stream, once that stream is up: the writing end, the child to
/// close, and when it was last heard — read by the quiet sweep.
struct UdpPeer {
    to_hub: Box<dyn Write + Send>,
    child: Arc<dyn Closer>,
    last: Arc<Mutex<Instant>>,
}

/// The datagrams holding for a stream that is still opening: kept in arrival order, bounded by
/// count and bytes, drop-oldest past either.
struct PeerHold {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: u32,
    said_drop: bool,
}

/// A client udp peer's place in the serve: its stream once up, or — while that stream dials, on
/// its own thread — the datagrams holding for it, bounded and drop-oldest.
enum PeerEntry {
    /// The dial runs; `n` names it, so only its own thread may finish it.
    Opening {
        n: u32,
        hold: PeerHold,
    },
    Up(UdpPeer),
}

/// The client udp serve loop: the peers it carries, each on its own stream, and the next stream
/// number to dial. It never waits on a dial — a new peer's stream opens beside the loop, and the
/// datagrams that arrive first hold for it. The peers map is shared with those dials' threads
/// (each finishes its own entry), so it sits behind an `Arc`.
type Peers = Arc<Mutex<HashMap<SocketAddr, PeerEntry>>>;

struct UdpClientLoop {
    me: Arc<LinkServe>,
    shared: Arc<Shared>,
    socket: Arc<UdpSocket>,
    peers: Peers,
    next: u32,
}

impl UdpClientLoop {
    /// One datagram on its way to the hub: a known peer's stream takes it (or its opening hold
    /// keeps it, newest-kept); a new peer gets a stream of its own, dialed beside the loop.
    /// `false`: the stream numbers ran out — the serve ends. A peer past the cap is dropped and
    /// logged; a stream that will not write is closed and its peer forgotten.
    fn carry(&mut self, peer: SocketAddr, datagram: &[u8], out: &mut Vec<u8>) -> bool {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match peers.get_mut(&peer) {
            Some(PeerEntry::Up(p)) => {
                *p.last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
                out.clear();
                push_frame(datagram, out);
                if p.to_hub.write_all(out).is_err()
                    && let Some(PeerEntry::Up(gone)) = peers.remove(&peer)
                {
                    gone.child.close();
                    self.shared.stream_done();
                }
                return true;
            }
            Some(PeerEntry::Opening { hold, .. }) => {
                hold_datagram(hold, datagram, &self.me.journal, &self.shared.link, &peer);
                return true;
            }
            None => {}
        }
        if peers.len() >= PEERS_MAX {
            drop(peers);
            self.me.journal.record(&format!(
                "link serve: {}: peer {peer} dropped: the link's {PEERS_MAX} streams are up",
                self.shared.link
            ));
            return true;
        }
        let Some(n) = self.next.checked_add(1) else {
            return false;
        };
        self.next = n;
        self.shared.peers_all.fetch_add(1, Ordering::SeqCst);
        let mut hold = PeerHold {
            queue: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            said_drop: false,
        };
        hold_datagram(
            &mut hold,
            datagram,
            &self.me.journal,
            &self.shared.link,
            &peer,
        );
        peers.insert(peer, PeerEntry::Opening { n, hold });
        drop(peers);
        // The dial runs beside the loop: the loop stays live for every other peer, and this
        // peer's next datagrams join the hold.
        let me = Arc::clone(&self.me);
        let shared = Arc::clone(&self.shared);
        let socket = Arc::clone(&self.socket);
        let peers = Arc::clone(&self.peers);
        thread::spawn(move || open_peer(&me, &shared, &socket, &peers, peer, n));
        true
    }

    /// Peers quiet past `quiet` are closed and forgotten, their streams freed. A peer whose
    /// stream is still opening is left alone: its dial ends by its own bounds (the ssh timeouts,
    /// the writer's stall bound), the sweep holds no child to close for it, and taking the entry
    /// would let a second dial start while the first still runs.
    fn sweep(&self, quiet: Duration) {
        let now = Instant::now();
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|peer, entry| {
                let PeerEntry::Up(p) = entry else {
                    return true;
                };
                let last = *p
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let keep = now.duration_since(last) < quiet;
                if !keep {
                    p.child.close();
                    self.shared.stream_done();
                    self.me.journal.record(&format!(
                        "link serve: {}: peer {peer} quiet, its stream closed",
                        self.shared.link
                    ));
                }
                keep
            });
        }
    }

    /// Every peer's stream closed — the serve is ending. Each freed place counts, so the drain
    /// after it sees an honest zero instead of burning its full wait. A still-opening peer has
    /// no stream here to close; its dial's thread closes what it dialed when it finds the entry
    /// gone.
    fn close_peers(&self) {
        if let Ok(peers) = self.peers.lock() {
            for entry in peers.values() {
                if let PeerEntry::Up(p) = entry {
                    p.child.close();
                    self.shared.stream_done();
                }
            }
        }
    }
}

/// A datagram joining a hold: kept, unless the hold is at its bounds — then the oldest goes to
/// make room, named once per dial.
fn hold_datagram(
    hold: &mut PeerHold,
    datagram: &[u8],
    journal: &Arc<dyn Journal>,
    link: &str,
    peer: &SocketAddr,
) {
    let PeerHold {
        queue,
        bytes,
        dropped,
        said_drop,
    } = hold;
    while queue.len() >= PEER_PENDING_MAX
        || bytes.saturating_add(datagram.len()) > PEER_PENDING_BYTES
    {
        let Some(gone) = queue.pop_front() else {
            break;
        };
        *bytes = bytes.saturating_sub(gone.len());
        *dropped = (*dropped).saturating_add(1);
        if !*said_drop {
            *said_drop = true;
            journal.record(&format!(
                "link serve: {link}: peer {peer} dropped its oldest held datagram \
                 (the stream is still opening)"
            ));
        }
    }
    queue.push_back(datagram.to_vec());
    *bytes = (*bytes).saturating_add(datagram.len());
}

/// Finishes a new peer's dial, off the serve's loop: the datagrams that held for the stream ride
/// it in order the moment it is up, and a dial that fails drops them, named. Only the entry that
/// still names this dial's number is finished — an absent or renumbered one means the serve ended
/// or the peer was re-dialed after a failure, and the child this thread dialed is closed here.
fn open_peer(
    me: &Arc<LinkServe>,
    shared: &Arc<Shared>,
    socket: &Arc<UdpSocket>,
    peers: &Peers,
    peer: SocketAddr,
    n: u32,
) {
    let Ok(conn) = me.dial.dial(&shared.link, &shared.secret, n) else {
        let (held, dropped) = {
            let mut peers = peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match peers.get_mut(&peer) {
                Some(PeerEntry::Opening { n: m, hold, .. }) if *m == n => {
                    let gone = (hold.queue.len(), hold.dropped);
                    peers.remove(&peer);
                    gone
                }
                _ => (0, 0),
            }
        };
        let held_s = if held > 0 {
            let dropped_s = if dropped > 0 {
                format!(", {dropped} dropped")
            } else {
                String::new()
            };
            format!(" ({held} datagrams held{dropped_s})")
        } else {
            String::new()
        };
        me.journal.record(&format!(
            "link serve: {}: peer {peer} dropped: the stream would not open{held_s}",
            shared.link
        ));
        return;
    };
    let StreamConn {
        from_hub,
        mut to_hub,
        child,
    } = conn;
    let last = Arc::new(Mutex::new(Instant::now()));
    let reader = UdpPeerRead {
        socket: Arc::clone(socket),
        peer,
        last: Arc::clone(&last),
        journal: Arc::clone(&me.journal),
        link: shared.link.clone(),
        n,
    };
    let reader_child = Arc::clone(&child);
    thread::spawn(move || reader.run(from_hub, &reader_child));
    {
        let mut peers = peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only the entry that still names this dial's number is finished — an absent or
        // renumbered one means the serve ended or the peer was re-dialed after a failure.
        let mine = matches!(peers.get(&peer), Some(PeerEntry::Opening { n: m, .. }) if *m == n);
        if !mine {
            child.close();
            return;
        }
        if let Some(PeerEntry::Opening { hold, .. }) = peers.get_mut(&peer) {
            // The held frames ride first, under the same hold as the handover, so no later
            // datagram can write around them. Each write is the carded A3 shape (a stalled
            // writer stalls the loop), bounded here by the hold's own bound.
            let mut out = Vec::with_capacity(DATAGRAM_MAX + 2);
            for datagram in hold.queue.drain(..) {
                out.clear();
                push_frame(&datagram, &mut out);
                if to_hub.write_all(&out).is_err() {
                    child.close();
                    break;
                }
            }
        }
        peers.insert(
            peer,
            PeerEntry::Up(UdpPeer {
                to_hub,
                child,
                last,
            }),
        );
    }
    shared.peers_now.fetch_add(1, Ordering::SeqCst);
}

/// A tcp pair: the stream and the local connection, copied both ways until either ends (the hub tears
/// both down when either side goes), then the child is closed and the peer count freed.
fn carry_tcp(
    journal: Arc<dyn Journal>,
    shared: Arc<Shared>,
    n: u32,
    conn: StreamConn,
    local: TcpStream,
) {
    let link = shared.link.as_str();
    let StreamConn {
        from_hub,
        to_hub,
        child,
    } = conn;
    let Ok(mut to_local) = local.try_clone() else {
        child.close();
        shared.stream_done();
        journal.record(&format!("link serve: {link}: stream {n} closed"));
        return;
    };
    let mut from_local = local;
    let down = Arc::new(AtomicU64::new(0));
    let up = Arc::new(AtomicU64::new(0));
    let hub_to_local = {
        let child = Arc::clone(&child);
        let counted = Arc::clone(&down);
        thread::spawn(move || {
            let mut from_hub = from_hub;
            let count = io::copy(&mut from_hub, &mut to_local).unwrap_or_default();
            counted.store(count, Ordering::SeqCst);
            child.close();
        })
    };
    let local_to_hub = {
        let child = Arc::clone(&child);
        let counted = Arc::clone(&up);
        thread::spawn(move || {
            let mut to_hub = to_hub;
            let count = io::copy(&mut from_local, &mut to_hub).unwrap_or_default();
            counted.store(count, Ordering::SeqCst);
            drop(to_hub);
            child.close();
        })
    };
    let link = link.to_owned();
    thread::spawn(move || {
        let _down: thread::Result<()> = hub_to_local.join();
        let _up: thread::Result<()> = local_to_hub.join();
        // The stream is over for good on both sides: its place in the count is freed (akira B1).
        shared.stream_done();
        journal.record(&format!(
            "link serve: {link}: stream {n} closed ({} up, {} down)",
            human(up.load(Ordering::SeqCst)),
            human(down.load(Ordering::SeqCst))
        ));
    });
}

/// A host-side udp stream: framed datagrams from the hub go to the local port; its replies go back
/// framed. A read timeout keeps the replier aware of the stream's end while it is quiet.
fn carry_udp_host(
    journal: Arc<dyn Journal>,
    shared: Arc<Shared>,
    n: u32,
    conn: StreamConn,
    reply: UdpSocket,
    port: u16,
) {
    let StreamConn {
        mut from_hub,
        mut to_hub,
        child,
    } = conn;
    let _timeout: io::Result<()> = reply.set_read_timeout(Some(Duration::from_millis(200)));
    let over = Arc::new(AtomicBool::new(false));
    let link = shared.link.clone();
    let to = SocketAddr::from(([127, 0, 0, 1], port));
    let reply = Arc::new(reply);
    let replier = {
        let over = Arc::clone(&over);
        let reply = Arc::clone(&reply);
        thread::spawn(move || {
            let mut buf = vec![0u8; DATAGRAM_MAX];
            loop {
                match reply.recv_from(&mut buf) {
                    Ok((len, _)) => {
                        let mut out = Vec::with_capacity(len.saturating_add(2));
                        push_frame(buf.get(..len).unwrap_or(&[]), &mut out);
                        if to_hub.write_all(&out).is_err() {
                            break;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        if over.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };
    thread::spawn(move || {
        while let Ok(Some(datagram)) = unframe(&mut from_hub) {
            let _sent: io::Result<usize> = reply.send_to(&datagram, to);
        }
        over.store(true, Ordering::SeqCst);
        child.close();
        let _joined: thread::Result<()> = replier.join();
        // The stream is over for good: its place in the count is freed (akira B1).
        shared.stream_done();
        journal.record(&format!("link serve: {link}: stream {n} closed"));
    });
}

/// A client-side udp peer's stream read: framed datagrams from the hub go to the peer; the stream's
/// end closes the child and frees the peer count.
struct UdpPeerRead {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    last: Arc<Mutex<Instant>>,
    journal: Arc<dyn Journal>,
    link: String,
    n: u32,
}

impl UdpPeerRead {
    fn run(self, mut from_hub: Box<dyn Read + Send>, child: &Arc<dyn Closer>) {
        while let Ok(Some(datagram)) = unframe(&mut from_hub) {
            *self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
            let _sent: io::Result<usize> = self.socket.send_to(&datagram, self.peer);
        }
        child.close();
        self.journal.record(&format!(
            "link serve: {}: stream {} closed",
            self.link, self.n
        ));
    }
}

/// A datagram's frame on a stream: a big-endian `u16` length, then the bytes. A datagram too long
/// for the wire cannot arrive; one is clamped to nothing rather than framed wrong.
fn push_frame(datagram: &[u8], out: &mut Vec<u8>) {
    if let Ok(len) = u16::try_from(datagram.len()) {
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(datagram);
    }
}

/// Reads one frame: `None` on a clean end, an error on a torn one.
fn unframe<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut head = [0u8; 2];
    match reader.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = usize::from(u16::from_be_bytes(head));
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(io::ErrorKind::UnexpectedEof, "a torn frame")
        } else {
            e
        }
    })?;
    Ok(Some(buf))
}

/// How long a serve waits at its end for its streams to close before it ends anyway.
fn wait_streams(shared: &Arc<Shared>) {
    let deadline = Instant::now()
        .checked_add(STREAMS_DRAIN)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if shared.peers_now.load(Ordering::SeqCst) == 0 {
            return;
        }
        thread::sleep(TICK);
    }
}

fn reply(id: &str, done: Result<String, String>) -> Vec<u8> {
    let value = match done {
        Ok(result) => serde_json::json!({"id": id, "ok": true, "result": result}),
        Err(err) => serde_json::json!({"id": id, "ok": false, "err": err}),
    };
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

fn human(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// Answers `link.serve-*` lines before the hands' engine sees them (`crate::macos` composes this with
/// [`crate::forward::route`]); with no dialer there is nothing to serve and the verb is refused.
pub fn route(others: LineHandler, links: Option<Arc<LinkServe>>) -> LineHandler {
    #[derive(Deserialize)]
    struct Verb {
        verb: String,
    }
    Arc::new(move |line: &[u8]| {
        let verb = serde_json::from_slice::<Verb>(line)
            .map(|v| v.verb)
            .unwrap_or_default();
        if !LinkServe::owns(&verb) {
            return others(line);
        }
        links.as_ref().map_or_else(
            || {
                #[derive(Deserialize)]
                struct Id {
                    id: String,
                }
                serde_json::from_slice::<Id>(line).map_or_else(
                    |_| refuse_line(line),
                    |r| {
                        if r.id.is_empty() || r.id.len() > 64 || r.id.chars().any(char::is_control)
                        {
                            refuse_line(line)
                        } else {
                            // No dialer (no [linkserve] ports): the verb is Tulpa's to refuse, like any
                            // organ it does not have.
                            reply(&r.id, Err("verb".to_owned()))
                        }
                    },
                )
            },
            |links| links.handle(line),
        )
    })
}

/// The menu item that closes a served side: `link-close:<id>`.
#[must_use]
pub fn close_item(link: &str) -> String {
    format!("link-close:{link}")
}

/// The serve a menu item closes, if it is one of [`close_item`]'s.
#[must_use]
pub fn close_item_of(item: &str) -> Option<String> {
    item.strip_prefix("link-close:").map(str::to_owned)
}

#[cfg(test)]
mod tests;
