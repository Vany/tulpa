//! Port forwards on request: a short-lived tunnel between a loopback port on the Mac and the same port on the
//! server's loopback, in either direction, carried by `ssh` on the link's own route.
//!
//! - **out** (`N →`): Tulpa listens on the Mac's `127.0.0.1:N`; each connection there dials `ssh -W 127.0.0.1:N`
//!   to the server. For an OAuth redirect that lands on the Mac's browser and must reach a listener on the server.
//!   By default it closes after its first connection has ended (`once`).
//! - **in** (`N ←`): Tulpa runs `ssh -N -R 127.0.0.1:N:127.0.0.1:N` for the forward's life: the server's
//!   `127.0.0.1:N` reaches the Mac's.
//!
//! Limits, all local: a port must be on the Mac's own allow-list (`[forward] ports`), whatever the hub granted; one
//! forward per port and direction; a time limit (out: 5 min by default, 15 at most; in: 15 and 60); Pause all and
//! Quit close every forward. Every ssh child runs under a watchdog that kills it once Tulpa is gone (a crash, a
//! SIGKILL), and its pid is written down so the next start kills any that outlived both. Opens, connections and closes
//! go to the activity log, with byte counts, never a byte of what was carried.
//!
//! Verbs (the hub's argument is one text line):
//! - `forward.open`, `"<port> <out|in> [ttl=<seconds>] [once|many]"` → `"<port> → open for 300 s, once"`
//! - `forward.close`, `"<port> <out|in>"` → `"<port> → closed"`
//! - `forward.list`, no argument → one line per open forward, or `"none"`

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tulpa_hands::Journal;
use tulpa_stop::Stop;

use crate::link::{CurrentRoute, DialSpec, LineHandler, Route, refuse_line, ssh_base};

/// An out forward's time limit when none is asked: an OAuth redirect needs a minute or two.
pub const OUT_TTL_DEFAULT: Duration = Duration::from_secs(5 * 60);
/// The longest an out forward may stay open: while it is, any page or process on the Mac can reach the server.
pub const OUT_TTL_MAX: Duration = Duration::from_secs(15 * 60);
/// An in forward's time limit when none is asked.
pub const IN_TTL_DEFAULT: Duration = Duration::from_secs(15 * 60);
/// The longest an in forward may stay open.
pub const IN_TTL_MAX: Duration = Duration::from_secs(60 * 60);
/// How long an in forward waits for the server's word on its `-R` before the open is answered.
///
/// ssh's own "remote forward success" line makes it open; a refusal, or no word in time (two TCP setups on the
/// jump route, `ConnectTimeout` 15), makes it not open.
pub const IN_START_WAIT: Duration = Duration::from_secs(15);
/// The line ssh prints (at `LogLevel=DEBUG1`) once the server has accepted a remote forward.
const REMOTE_FORWARD_OK: &str = "remote forward success for: listen";
/// How often the forward threads look at their stop flag and deadline.
const TICK: Duration = Duration::from_millis(100);
/// The watchdog's poll of its parent (Tulpa), in seconds: an orphaned ssh lives at most this long.
const WATCH_EVERY_S: u32 = 2;

/// Which way a forward carries connections.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Dir {
    /// The Mac's loopback port reaches the server's (`N →`).
    Out,
    /// The server's loopback port reaches the Mac's (`N ←`).
    In,
}

impl Dir {
    /// The menu's arrow.
    #[must_use]
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Out => "→",
            Self::In => "←",
        }
    }

    /// The protocol's word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "out" => Some(Self::Out),
            "in" => Some(Self::In),
            _ => None,
        }
    }

    const fn limits(self) -> (Duration, Duration) {
        match self {
            Self::Out => (OUT_TTL_DEFAULT, OUT_TTL_MAX),
            Self::In => (IN_TTL_DEFAULT, IN_TTL_MAX),
        }
    }
}

/// A checked `forward.open`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Plan {
    /// The loopback port, the same on both ends.
    pub port: u16,
    /// Which way.
    pub dir: Dir,
    /// How long it stays open at most.
    pub ttl: Duration,
    /// Out only: it closes once its first connection has ended.
    pub once: bool,
}

/// Reads `"<port> <out|in> [ttl=<seconds>] [once|many]"` against the Mac's allow-list.
///
/// # Errors
///
/// A refusal fit for the reply: a malformed argument, a port not on the allow-list, a time limit past its maximum,
/// `once` asked for an in forward.
pub fn parse_open(arg: &str, allowed: &[u16]) -> Result<Plan, String> {
    let mut words = arg.split_whitespace();
    let (port, dir) = port_and_dir(&mut words, allowed)?;
    let (default_ttl, max_ttl) = dir.limits();
    let mut ttl = default_ttl;
    let mut once = None;
    for word in words {
        if let Some(seconds) = word.strip_prefix("ttl=") {
            let seconds: u64 =
                seconds.parse().ok().filter(|s| *s > 0).ok_or_else(|| {
                    format!("arg: ttl={seconds} is not a whole number of seconds")
                })?;
            ttl = Duration::from_secs(seconds);
            if ttl > max_ttl {
                return Err(format!(
                    "arg: ttl {seconds} s is past the {} forward's {} s",
                    dir.word(),
                    max_ttl.as_secs()
                ));
            }
        } else if word == "once" || word == "many" {
            if once.is_some() {
                return Err("arg: once or many, not both".to_owned());
            }
            once = Some(word == "once");
        } else {
            return Err(format!(
                "arg: {word:?} — <port> <out|in> [ttl=<seconds>] [once|many]"
            ));
        }
    }
    if dir == Dir::In && once == Some(true) {
        return Err(
            "arg: once is for an out forward (an in forward can't see a connection end)".to_owned(),
        );
    }
    Ok(Plan {
        port,
        dir,
        ttl,
        once: dir == Dir::Out && once.unwrap_or(true),
    })
}

/// Reads `"<port> <out|in>"`.
///
/// # Errors
///
/// A refusal fit for the reply, as [`parse_open`].
pub fn parse_close(arg: &str) -> Result<(u16, Dir), String> {
    let mut words = arg.split_whitespace();
    let (port, dir) = port_and_dir(&mut words, &[])?;
    if words.next().is_some() {
        return Err("arg: <port> <out|in>".to_owned());
    }
    Ok((port, dir))
}

/// The first two words; `allowed` empty skips the allow-list (closing is never refused for it).
fn port_and_dir<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    allowed: &[u16],
) -> Result<(u16, Dir), String> {
    let port: u16 = words
        .next()
        .and_then(|p| p.parse().ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| "arg: <port> <out|in> — a port 1-65535 first".to_owned())?;
    let dir = words.next().and_then(Dir::parse).ok_or_else(|| {
        "arg: <port> <out|in> — out (Mac → server) or in (server → Mac)".to_owned()
    })?;
    if !allowed.is_empty() && !allowed.contains(&port) {
        return Err(format!(
            "port {port} is not on this Mac's forward list ([forward] ports)"
        ));
    }
    Ok((port, dir))
}

/// An ssh child as the forwards see it.
pub trait SshChild: Send + Sync {
    /// Still running.
    fn alive(&self) -> bool;
    /// Ends it (no-op once it ended).
    fn kill(&self);
    /// Its last stderr lines, for a refusal or an end.
    fn why(&self) -> String;
    /// The server confirmed what it was started for: an in forward's `-R` is listening on the server.
    fn ready(&self) -> bool;
}

/// One out connection's tunnel: the server end's bytes and the child carrying them.
#[non_exhaustive]
pub struct Tunnel {
    /// Bytes from the server's port.
    pub from_server: Box<dyn Read + Send>,
    /// Bytes to the server's port; dropped, the server end sees EOF.
    pub to_server: Box<dyn Write + Send>,
    /// The ssh child.
    pub child: Arc<dyn SshChild>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Tunnel").finish_non_exhaustive()
    }
}

/// Starts the ssh children; the real one is [`SshSpawn`], tests use fakes.
pub trait Spawn: Send + Sync {
    /// `ssh -W 127.0.0.1:<port>` for one out connection.
    ///
    /// # Errors
    ///
    /// When ssh can't be started.
    fn tunnel(&self, port: u16) -> io::Result<Tunnel>;
    /// `ssh -N -R 127.0.0.1:<port>:127.0.0.1:<port>` for an in forward's life.
    ///
    /// # Errors
    ///
    /// When ssh can't be started.
    fn reverse(&self, port: u16) -> io::Result<Arc<dyn SshChild>>;
}

/// One open forward, as `forward.list` and the menu show it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct View {
    /// The port.
    pub port: u16,
    /// Which way.
    pub dir: Dir,
    /// Seconds left before its time limit.
    pub left_s: u64,
    /// Out: it closes after its first connection.
    pub once: bool,
    /// Connections carried so far (out).
    pub conns: u64,
}

impl View {
    /// `53682 → once — click to close`: the menu row (no countdown: the menu is drawn on events, not every second).
    #[must_use]
    pub fn menu_line(&self) -> String {
        let once = if self.once { " once" } else { "" };
        format!("{} {}{once} — click to close", self.port, self.dir.arrow())
    }

    /// `53682 → (4:12 left, once)`: `forward.list`'s line.
    #[must_use]
    pub fn line(&self) -> String {
        let left = format!("{}:{:02}", self.left_s / 60, self.left_s % 60);
        let mut extra = String::new();
        if self.once {
            extra.push_str(", once");
        }
        if self.conns > 0 {
            extra = format!("{extra}, {} conn", self.conns);
        }
        format!("{} {} ({left} left{extra})", self.port, self.dir.arrow())
    }
}

struct Entry {
    until: Instant,
    once: bool,
    stop: Arc<AtomicBool>,
    conns: Arc<AtomicU64>,
}

/// Every open forward, and the rules they open by.
pub struct Forwards {
    allowed: Vec<u16>,
    spawn: Arc<dyn Spawn>,
    paused: Arc<Stop>,
    journal: Arc<dyn Journal>,
    changed: Arc<dyn Fn() + Send + Sync>,
    open: Mutex<HashMap<(u16, Dir), Entry>>,
    start_wait: Duration,
}

impl fmt::Debug for Forwards {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Forwards")
            .field("allowed", &self.allowed)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Request {
    id: String,
    verb: String,
    #[serde(default)]
    arg: Option<serde_json::Value>,
}

impl Forwards {
    /// Forwards on `allowed` ports (empty: none at all), started by `spawn`, refused while `paused`, logged to
    /// `journal`; `changed` is called whenever the open set changes (the menu).
    #[must_use]
    pub fn new(
        allowed: Vec<u16>,
        spawn: Arc<dyn Spawn>,
        paused: Arc<Stop>,
        journal: Arc<dyn Journal>,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            allowed,
            spawn,
            paused,
            journal,
            changed,
            open: Mutex::new(HashMap::new()),
            start_wait: IN_START_WAIT,
        }
    }

    /// Watches an in forward's start for `wait` before answering (tests: short).
    #[must_use]
    pub const fn with_start_wait(mut self, wait: Duration) -> Self {
        self.start_wait = wait;
        self
    }

    /// Whether a line's verb is one of the forwards'.
    #[must_use]
    pub fn owns(verb: &str) -> bool {
        verb.starts_with("forward.")
    }

    /// Answers one `forward.*` line.
    pub fn handle(self: &Arc<Self>, line: &[u8]) -> Vec<u8> {
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            self.journal.record("forward: unparseable line refused");
            return refuse_line(line);
        };
        if request.id.is_empty()
            || request.id.len() > 64
            || request.id.chars().any(char::is_control)
        {
            self.journal.record("forward: bad id refused");
            return refuse_line(line);
        }
        let arg = match &request.arg {
            None => None,
            Some(serde_json::Value::String(text)) => Some(text.as_str()),
            Some(_) => return reply(&request.id, Err("arg".to_owned())),
        };
        let done = match (request.verb.as_str(), arg) {
            ("forward.open", Some(arg)) => self.open_line(arg),
            ("forward.close", Some(arg)) => parse_close(arg).and_then(|(port, dir)| {
                if self.close(port, dir, "closed by request") {
                    Ok(format!("{port} {} closed", dir.arrow()))
                } else {
                    Err(format!("{port} {} is not open", dir.arrow()))
                }
            }),
            ("forward.list", None) => {
                let views = self.list();
                Ok(if views.is_empty() {
                    "none".to_owned()
                } else {
                    views.iter().map(View::line).collect::<Vec<_>>().join("\n")
                })
            }
            ("forward.open" | "forward.close", None) | ("forward.list", Some(_)) => {
                Err("arg".to_owned())
            }
            _ => Err("verb".to_owned()),
        };
        reply(&request.id, done)
    }

    fn open_line(self: &Arc<Self>, arg: &str) -> Result<String, String> {
        if self.paused.is_set() {
            return Err("paused".to_owned());
        }
        if self.allowed.is_empty() {
            return Err("forward: off on this Mac (no [forward] ports)".to_owned());
        }
        let plan = parse_open(arg, &self.allowed).inspect_err(|why| {
            self.journal
                .record(&format!("forward: open refused: {why}"));
        })?;
        self.open(plan)
    }

    /// Opens `plan`'s forward.
    ///
    /// # Errors
    ///
    /// Paused, already open, the port in use on the Mac (out), ssh refused or gone at once (in).
    pub fn open(self: &Arc<Self>, plan: Plan) -> Result<String, String> {
        if self.paused.is_set() {
            return Err("paused".to_owned());
        }
        let key = (plan.port, plan.dir);
        let label = format!("{} {}", plan.port, plan.dir.arrow());
        let stop = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(AtomicU64::new(0));
        {
            let Ok(open) = self.open.lock() else {
                return Err("forward: state lock poisoned".to_owned());
            };
            if open.contains_key(&key) {
                return Err(format!("busy: {label} is open"));
            }
        }
        let until = Instant::now()
            .checked_add(plan.ttl)
            .unwrap_or_else(Instant::now);
        match plan.dir {
            Dir::Out => {
                let listener = TcpListener::bind(("127.0.0.1", plan.port)).map_err(|e| {
                    format!(
                        "{label}: the Mac's 127.0.0.1:{} can't be listened on ({e})",
                        plan.port
                    )
                })?;
                listener
                    .set_nonblocking(true)
                    .map_err(|e| format!("{label}: {e}"))?;
                self.insert(key, &stop, &conns, until, plan.once)?;
                let me = Arc::clone(self);
                let (stop2, conns2) = (Arc::clone(&stop), Arc::clone(&conns));
                thread::spawn(move || me.serve_out(listener, plan, until, &stop2, &conns2));
            }
            Dir::In => {
                let child = self
                    .spawn
                    .reverse(plan.port)
                    .map_err(|e| format!("{label}: ssh not started ({e})"))?;
                // Open only on the server's own word: until ssh says the remote forward succeeded, it isn't.
                let started = Instant::now();
                loop {
                    if child.ready() {
                        break;
                    }
                    if !child.alive() {
                        let why = child.why();
                        self.journal.record(&format!(
                            "forward: {label} refused: ssh ended before the server confirmed it ({why})"
                        ));
                        return Err(format!("{label}: the server refused it ({why})"));
                    }
                    if started.elapsed() >= self.start_wait {
                        child.kill();
                        let secs = self.start_wait.as_secs();
                        self.journal.record(&format!(
                            "forward: {label} not open: no word from the server in {secs} s"
                        ));
                        return Err(format!(
                            "{label}: not open — the server gave no word in {secs} s"
                        ));
                    }
                    thread::sleep(TICK.min(self.start_wait));
                }
                if let Err(why) = self.insert(key, &stop, &conns, until, false) {
                    child.kill();
                    return Err(why);
                }
                let me = Arc::clone(self);
                let stop2 = Arc::clone(&stop);
                thread::spawn(move || me.watch_in(plan, &child, until, &stop2));
            }
        }
        let once = if plan.once { ", once" } else { "" };
        self.journal.record(&format!(
            "forward: {label} open for {} s{once}",
            plan.ttl.as_secs()
        ));
        (self.changed)();
        Ok(format!("{label} open for {} s{once}", plan.ttl.as_secs()))
    }

    fn insert(
        &self,
        key: (u16, Dir),
        stop: &Arc<AtomicBool>,
        conns: &Arc<AtomicU64>,
        until: Instant,
        once: bool,
    ) -> Result<(), String> {
        let Ok(mut open) = self.open.lock() else {
            return Err("forward: state lock poisoned".to_owned());
        };
        if open.contains_key(&key) {
            return Err(format!("busy: {} {} is open", key.0, key.1.arrow()));
        }
        open.insert(
            key,
            Entry {
                until,
                once,
                stop: Arc::clone(stop),
                conns: Arc::clone(conns),
            },
        );
        Ok(())
    }

    /// Takes the forward out of the open set if it is still this one (its stop flag), and logs why it ended.
    fn ended(&self, key: (u16, Dir), stop: &Arc<AtomicBool>, why: &str) {
        let removed = self.open.lock().is_ok_and(|mut open| {
            if open.get(&key).is_some_and(|e| Arc::ptr_eq(&e.stop, stop)) {
                open.remove(&key);
                true
            } else {
                false
            }
        });
        if removed {
            self.journal.record(&format!(
                "forward: {} {} closed: {why}",
                key.0,
                key.1.arrow()
            ));
            (self.changed)();
        }
    }

    fn serve_out(
        self: Arc<Self>,
        listener: TcpListener,
        plan: Plan,
        until: Instant,
        stop: &Arc<AtomicBool>,
        conns: &Arc<AtomicU64>,
    ) {
        let key = (plan.port, plan.dir);
        let label = format!("{} {}", plan.port, plan.dir.arrow());
        let running = Arc::new(AtomicUsize::new(0));
        let mut listener = Some(listener);
        let why = loop {
            if stop.load(Ordering::SeqCst) {
                break "closed".to_owned();
            }
            if Instant::now() >= until {
                break "its time limit".to_owned();
            }
            let Some(socket) = listener.as_ref() else {
                // Once: the port was closed at the first accept; the forward ends with that connection.
                if running.load(Ordering::SeqCst) == 0 {
                    break "its one connection ended (once)".to_owned();
                }
                thread::sleep(TICK);
                continue;
            };
            match socket.accept() {
                Ok((stream, _)) => {
                    if plan.once {
                        // Closed now: a second connection is refused by the Mac, not queued.
                        listener = None;
                    }
                    let n = conns.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                    running.fetch_add(1, Ordering::SeqCst);
                    self.journal
                        .record(&format!("forward: {label} connection {n} accepted"));
                    (self.changed)();
                    let me = Arc::clone(&self);
                    let (stop2, running2, label2) =
                        (Arc::clone(stop), Arc::clone(&running), label.clone());
                    thread::spawn(move || {
                        me.carry(&stream, plan.port, until, &stop2, &label2, n);
                        running2.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                Err(e) => break format!("the listener failed ({e})"),
            }
        };
        // The listener goes first: nothing new reaches the Mac's port once the forward is ending.
        drop(listener);
        stop.store(true, Ordering::SeqCst);
        while running.load(Ordering::SeqCst) > 0 {
            thread::sleep(TICK);
        }
        self.ended(key, stop, &why);
    }

    /// One out connection: the Mac's side and the server's, both ways, until either ends or the forward stops.
    fn carry(
        &self,
        stream: &TcpStream,
        port: u16,
        until: Instant,
        stop: &AtomicBool,
        label: &str,
        n: u64,
    ) {
        let _blocking: io::Result<()> = stream.set_nonblocking(false);
        let tunnel = match self.spawn.tunnel(port) {
            Ok(tunnel) => tunnel,
            Err(e) => {
                self.journal.record(&format!(
                    "forward: {label} connection {n}: ssh not started ({e})"
                ));
                let _shut: io::Result<()> = stream.shutdown(Shutdown::Both);
                return;
            }
        };
        let Tunnel {
            mut from_server,
            mut to_server,
            child,
        } = tunnel;
        let (Ok(mut mac_read), Ok(mut mac_write)) = (stream.try_clone(), stream.try_clone()) else {
            child.kill();
            return;
        };
        let up = Arc::new(AtomicU64::new(0));
        let down = Arc::new(AtomicU64::new(0));
        let (up2, down2) = (Arc::clone(&up), Arc::clone(&down));
        let sending = thread::spawn(move || {
            let copied = io::copy(&mut mac_read, &mut CountingWrite(&mut to_server, &up2));
            drop(to_server);
            copied.is_ok()
        });
        let receiving = thread::spawn(move || {
            let copied = io::copy(&mut from_server, &mut CountingWrite(&mut mac_write, &down2));
            let _shut: io::Result<()> = mac_write.shutdown(Shutdown::Write);
            copied.is_ok()
        });
        loop {
            if receiving.is_finished() && sending.is_finished() {
                break;
            }
            // The server side ended: the Mac's side gets its EOF (above) and a moment to finish, then is cut.
            if receiving.is_finished() || stop.load(Ordering::SeqCst) || Instant::now() >= until {
                let grace = Instant::now()
                    .checked_add(Duration::from_secs(2))
                    .unwrap_or_else(Instant::now);
                while !sending.is_finished()
                    && Instant::now() < grace
                    && !stop.load(Ordering::SeqCst)
                    && Instant::now() < until
                {
                    thread::sleep(TICK);
                }
                let _shut: io::Result<()> = stream.shutdown(Shutdown::Both);
                child.kill();
                break;
            }
            thread::sleep(TICK);
        }
        let _sent: thread::Result<bool> = sending.join();
        let _received: thread::Result<bool> = receiving.join();
        child.kill();
        self.journal.record(&format!(
            "forward: {label} connection {n} ended ({} bytes to the server, {} back)",
            up.load(Ordering::SeqCst),
            down.load(Ordering::SeqCst)
        ));
    }

    fn watch_in(
        self: Arc<Self>,
        plan: Plan,
        child: &Arc<dyn SshChild>,
        until: Instant,
        stop: &Arc<AtomicBool>,
    ) {
        let key = (plan.port, plan.dir);
        let why = loop {
            if stop.load(Ordering::SeqCst) {
                break "closed".to_owned();
            }
            if Instant::now() >= until {
                break "its time limit".to_owned();
            }
            if !child.alive() {
                break format!("ssh ended ({})", child.why());
            }
            thread::sleep(TICK);
        };
        child.kill();
        self.ended(key, stop, &why);
    }

    /// Closes one forward; false when it wasn't open.
    pub fn close(&self, port: u16, dir: Dir, why: &str) -> bool {
        let stop = self
            .open
            .lock()
            .ok()
            .and_then(|open| open.get(&(port, dir)).map(|e| Arc::clone(&e.stop)));
        let Some(stop) = stop else {
            return false;
        };
        self.journal
            .record(&format!("forward: {port} {} closing: {why}", dir.arrow()));
        stop.store(true, Ordering::SeqCst);
        true
    }

    /// Closes every forward (Pause all, Quit); returns how many were open.
    pub fn close_all(&self, why: &str) -> usize {
        let keys: Vec<(u16, Dir)> = self
            .open
            .lock()
            .map(|open| open.keys().copied().collect())
            .unwrap_or_default();
        for (port, dir) in &keys {
            self.close(*port, *dir, why);
        }
        keys.len()
    }

    /// Waits until no forward is open, at most `wait` (Quit: the ssh children are gone before the app exits).
    pub fn drain(&self, wait: Duration) {
        let started = Instant::now();
        while started.elapsed() < wait && self.open.lock().is_ok_and(|open| !open.is_empty()) {
            thread::sleep(TICK);
        }
    }

    /// The open forwards, by port then direction.
    #[must_use]
    pub fn list(&self) -> Vec<View> {
        let now = Instant::now();
        let mut views: Vec<View> = self
            .open
            .lock()
            .map(|open| {
                open.iter()
                    .map(|((port, dir), e)| View {
                        port: *port,
                        dir: *dir,
                        left_s: e.until.saturating_duration_since(now).as_secs(),
                        once: e.once,
                        conns: e.conns.load(Ordering::SeqCst),
                    })
                    .collect()
            })
            .unwrap_or_default();
        views.sort_by_key(|v| (v.port, v.dir == Dir::In));
        views
    }
}

/// The link's line handler with the forwards in front of `others`: `forward.*` verbs go to `forwards` (none
/// configured: refused as an unknown verb), every other line to `others`.
#[must_use]
pub fn route(others: LineHandler, forwards: Option<Arc<Forwards>>) -> LineHandler {
    #[derive(Deserialize)]
    struct Verb {
        verb: String,
    }
    Arc::new(move |line: &[u8]| {
        let verb = serde_json::from_slice::<Verb>(line)
            .map(|v| v.verb)
            .unwrap_or_default();
        if !Forwards::owns(&verb) {
            return others(line);
        }
        forwards.as_ref().map_or_else(
            || {
                serde_json::from_slice::<Request>(line).map_or_else(
                    |_| refuse_line(line),
                    |r| reply(&r.id, Err("verb".to_owned())),
                )
            },
            |forwards| forwards.handle(line),
        )
    })
}

/// The menu item that closes a forward: `forward-close:<port>:<out|in>`.
#[must_use]
pub fn close_item(port: u16, dir: Dir) -> String {
    format!("forward-close:{port}:{}", dir.word())
}

/// The forward a menu item closes, if it is one of [`close_item`]'s.
#[must_use]
pub fn close_item_of(id: &str) -> Option<(u16, Dir)> {
    let rest = id.strip_prefix("forward-close:")?;
    let (port, dir) = rest.split_once(':')?;
    Some((port.parse().ok()?, Dir::parse(dir)?))
}

struct CountingWrite<'a, W: Write>(&'a mut W, &'a AtomicU64);

impl<W: Write> Write for CountingWrite<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.write(buf)?;
        self.1
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::SeqCst);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn reply(id: &str, done: Result<String, String>) -> Vec<u8> {
    let value = match done {
        Ok(result) => json!({"id": id, "ok": true, "result": result}),
        Err(err) => json!({"id": id, "ok": false, "err": err}),
    };
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// The watchdog every forward's ssh runs under: `$1` is Tulpa's pid; a background loop kills the ssh (`exec`'d in
/// its place, so its pid is this shell's) once Tulpa is gone, and ends with it.
/// The guards' common head (akira): `p` Tulpa's pid, `me` the ssh's (the shell execs into it), `n` its program's name.
/// `stop` ends `me` only while it is still that program — a pid the ssh left behind may be another process's by then
/// — with SIGTERM, then SIGKILL 2 s later if it lingers. `ps -o comm=` is the name on Linux, the path on macOS.
const STOP_FN: &str = concat!(
    "p=$1; shift; me=$$; n=${1##*/}; ",
    "is_me() { kill -0 \"$me\" 2>/dev/null && case \"$(ps -o comm= -p \"$me\" 2>/dev/null)\" in *\"$n\") true;; *) false;; esac; }; ",
    "stop() { is_me || return 0; kill \"$me\" 2>/dev/null; sleep 2; is_me && kill -9 \"$me\" 2>/dev/null; return 0; }; "
);

/// The guards' `$0`: no "tulpa" in it, so a `pkill -f tulpa` (akira: the likely cause of the rehearsal's leftover)
/// can't take a guard down while leaving the ssh it watches.
const GUARD_NAME: &str = "fwd-guard";

fn watchdog_script() -> String {
    format!(
        "{STOP_FN} ( while kill -0 \"$p\" 2>/dev/null && kill -0 \"$me\" 2>/dev/null; do sleep {WATCH_EVERY_S}; done; kill -0 \"$p\" 2>/dev/null || stop ) </dev/null >/dev/null 2>&1 & exec \"$@\""
    )
}
/// The in forward's watchdog (#0362 rehearsal: an `-R` ssh outlived a `kill -9` of Tulpa on a Mac). Two nets: the
/// parent's pid, polled as above; and a pipe — the shell's stdin, whose write end only Tulpa holds. However Tulpa ends,
/// the kernel closes that end, `cat` sees EOF and the ssh is ended at once, with no poll and no pid to misread. The ssh
/// itself runs with `-n` and never reads stdin. The pipe is moved to fd 3 first: a background job's stdin is
/// `/dev/null` before its own redirects apply, so `<&0` there would read nothing. Only the `cat` keeps fd 3.
fn reverse_watchdog_script() -> String {
    format!(
        "{STOP_FN} exec 3<&0; ( cat <&3 >/dev/null; stop ) 3<&3 >/dev/null 2>&1 & \
         ( while kill -0 \"$p\" 2>/dev/null && kill -0 \"$me\" 2>/dev/null; do sleep {WATCH_EVERY_S}; done; kill -0 \"$p\" 2>/dev/null || stop ) </dev/null 3<&- >/dev/null 2>&1 & \
         exec \"$@\" </dev/null 3<&-"
    )
}
/// `ssh -R …` wrapped with both nets ([`reverse_watchdog_script`]); its stdin must be a pipe whose write end the
/// caller keeps for the forward's life.
fn under_reverse_watchdog(ssh: &Command, parent: u32) -> Command {
    let mut wrapped = Command::new("/bin/sh");
    wrapped
        .arg("-c")
        .arg(reverse_watchdog_script())
        .arg(GUARD_NAME)
        .arg(parent.to_string())
        .arg(ssh.get_program())
        .args(ssh.get_args());
    wrapped
}

/// `ssh …` wrapped: `/bin/sh -c <watchdog> fwd-guard <tulpa pid> ssh …`.
fn under_watchdog(ssh: &Command, parent: u32) -> Command {
    let mut wrapped = Command::new("/bin/sh");
    wrapped
        .arg("-c")
        .arg(watchdog_script())
        .arg(GUARD_NAME)
        .arg(parent.to_string())
        .arg(ssh.get_program())
        .args(ssh.get_args());
    wrapped
}

/// The real ssh children, on the link's route, under the watchdog, their pids written down.
pub struct SshSpawn {
    spec: DialSpec,
    route: CurrentRoute,
    pids: PidFile,
    journal: Arc<dyn Journal>,
    /// The ssh program: `ssh` from PATH; a test runs a stand-in.
    program: std::ffi::OsString,
}

impl fmt::Debug for SshSpawn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshSpawn")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl SshSpawn {
    /// Dials with `spec` on the route the link is on; pids written to `pid_file`.
    #[must_use]
    pub fn new(
        spec: DialSpec,
        route: CurrentRoute,
        pid_file: PathBuf,
        journal: Arc<dyn Journal>,
    ) -> Self {
        Self {
            spec,
            route,
            pids: PidFile::new(pid_file),
            journal,
            program: std::ffi::OsString::from("ssh"),
        }
    }

    /// A stand-in for ssh (tests: a script that behaves like it).
    #[cfg(test)]
    fn with_program(mut self, program: &Path) -> Self {
        self.program = program.as_os_str().to_owned();
        self
    }

    fn route(&self) -> Route {
        self.route
            .lock()
            .ok()
            .and_then(|current| current.clone())
            .unwrap_or(Route::Jump)
    }

    fn start(&self, ssh: &Command, port: u16, reverse: bool) -> io::Result<Arc<RealChild>> {
        let mut real = Command::new(&self.program);
        real.args(ssh.get_args());
        let ssh = &real;
        let mut wrapped = if reverse {
            under_reverse_watchdog(ssh, std::process::id())
        } else {
            under_watchdog(ssh, std::process::id())
        };
        let mut child = wrapped
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        self.pids.add(pid, port);
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let ready = Arc::new(AtomicBool::new(false));
        if let Some(stderr) = child.stderr.take() {
            let (tail2, journal, ready2) = (
                Arc::clone(&tail),
                Arc::clone(&self.journal),
                Arc::clone(&ready),
            );
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line: String = line.chars().filter(|c| !c.is_control()).take(200).collect();
                    // The in forward's ssh runs at DEBUG1 for its success line; the rest of its debug output
                    // (hosts, keys, users) stays out of the log and out of a refusal's words.
                    if line.contains(REMOTE_FORWARD_OK) {
                        ready2.store(true, Ordering::SeqCst);
                        journal.record("forward: ssh: the server accepted the remote forward");
                        continue;
                    }
                    if line.starts_with("debug") {
                        continue;
                    }
                    journal.record(&format!("forward: ssh: {line}"));
                    if let Ok(mut tail) = tail2.lock() {
                        tail.push(line);
                        if tail.len() > 3 {
                            tail.remove(0);
                        }
                    }
                }
            });
        }
        Ok(Arc::new(RealChild {
            child: Mutex::new(Some(child)),
            pid,
            pids: self.pids.clone(),
            tail,
            ready,
            keep: Mutex::new(None),
        }))
    }
}

impl Spawn for SshSpawn {
    fn tunnel(&self, port: u16) -> io::Result<Tunnel> {
        let mut ssh = ssh_base(&self.route());
        ssh.arg("-W")
            .arg(format!("127.0.0.1:{port}"))
            .arg(self.spec.alias());
        let child = self.start(&ssh, port, false)?;
        let (stdin, stdout) = child.take_pipes()?;
        let child: Arc<dyn SshChild> = child;
        Ok(Tunnel {
            from_server: Box::new(stdout),
            to_server: Box::new(stdin),
            child,
        })
    }

    fn reverse(&self, port: u16) -> io::Result<Arc<dyn SshChild>> {
        let child: Arc<dyn SshChild> = self.reverse_real(port)?;
        Ok(child)
    }
}

impl SshSpawn {
    /// `ssh -N -R` under both guards, its watchdog pipe kept ([`Spawn::reverse`]).
    fn reverse_real(&self, port: u16) -> io::Result<Arc<RealChild>> {
        let mut ssh = ssh_base(&self.route());
        // DEBUG1 for ssh's "remote forward success" line: the open waits for it (IN_START_WAIT).
        ssh.arg("-o")
            .arg("LogLevel=DEBUG1")
            .arg("-N")
            .arg("-n")
            .arg("-R")
            .arg(format!("127.0.0.1:{port}:127.0.0.1:{port}"))
            .arg(self.spec.alias());
        // The in forward runs under both guards (reverse_watchdog_script): its stdin is the pipe whose write end only
        // Tulpa holds — kept by the child (RealChild.keep) for the forward's life, dropped at its kill, closed by the
        // kernel if Tulpa dies any way. Its stdout carries nothing.
        let child = self.start(&ssh, port, true)?;
        if let Ok((stdin, _stdout)) = child.take_pipes()
            && let Ok(mut keep) = child.keep.lock()
        {
            *keep = Some(stdin);
        }
        Ok(child)
    }
}

struct RealChild {
    /// The ssh child until its kill takes it out to reap it.
    child: Mutex<Option<Child>>,
    pid: u32,
    pids: PidFile,
    tail: Arc<Mutex<Vec<String>>>,
    ready: Arc<AtomicBool>,
    /// An in forward's watchdog pipe (its write end): held while the forward lives.
    keep: Mutex<Option<ChildStdin>>,
}

impl RealChild {
    fn take_pipes(&self) -> io::Result<(ChildStdin, ChildStdout)> {
        let pipes = self
            .child
            .lock()
            .map_err(|_| io::Error::other("ssh child lock poisoned"))?
            .as_mut()
            .map(|child| (child.stdin.take(), child.stdout.take()));
        match pipes {
            Some((Some(stdin), Some(stdout))) => Ok((stdin, stdout)),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh child killed",
            )),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh child has no stdio pipes",
            )),
        }
    }
}

impl SshChild for RealChild {
    fn alive(&self) -> bool {
        self.child.lock().is_ok_and(|mut child| {
            child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        })
    }

    fn kill(&self) {
        // The watchdog's pipe first: its own net ends the ssh too, whatever happens to the kill below.
        if let Ok(mut keep) = self.keep.lock() {
            drop(keep.take());
        }
        // Taken out under the lock, killed and reaped (a wait) with it let go.
        let taken = self.child.lock().ok().and_then(|mut child| child.take());
        if let Some(mut child) = taken {
            let _killed: io::Result<()> = child.kill();
            crate::ears::reap_killed(&mut child);
        }
        self.pids.remove(self.pid);
    }

    fn why(&self) -> String {
        let tail = self.tail.lock().map(|t| t.join(" / ")).unwrap_or_default();
        if tail.is_empty() {
            "no message".to_owned()
        } else {
            tail
        }
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

impl Drop for RealChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The forwards' ssh pids, one `<pid> <port>` per line, so the next start kills any that outlived Tulpa and its
/// watchdog (a SIGKILL to the whole group, a power cut).
#[derive(Clone, Debug)]
struct PidFile {
    path: PathBuf,
    live: Arc<Mutex<Vec<(u32, u16)>>>,
}

impl PidFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            live: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn add(&self, pid: u32, port: u16) {
        if let Ok(mut live) = self.live.lock() {
            live.push((pid, port));
            write_pids(&self.path, &live);
        }
    }

    fn remove(&self, pid: u32) {
        if let Ok(mut live) = self.live.lock() {
            let before = live.len();
            live.retain(|(p, _)| *p != pid);
            if live.len() != before {
                write_pids(&self.path, &live);
            }
        }
    }
}

fn write_pids(path: &Path, live: &[(u32, u16)]) {
    let text = live.iter().fold(String::new(), |mut text, (pid, port)| {
        let _written: fmt::Result = writeln!(text, "{pid} {port}");
        text
    });
    let _written: io::Result<()> = fs::write(path, text);
}

/// The pid lines of a pid file: `(pid, port)`, malformed lines skipped.
#[must_use]
pub fn read_pids(text: &str) -> Vec<(u32, u16)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.parse().ok()?, words.next()?.parse().ok()?))
        })
        .collect()
}

/// Whether `command` (as `ps -o command=` shows it) is a forward's ssh for `port`: a pid reused by another program
/// is never killed.
#[must_use]
pub fn is_forward_ssh(command: &str, port: u16) -> bool {
    let mut words = command.split_whitespace();
    let first = words.next().unwrap_or_default();
    let ssh = first == "ssh" || first.ends_with("/ssh");
    let rest: Vec<&str> = words.collect();
    let out = format!("127.0.0.1:{port}");
    let reverse = format!("127.0.0.1:{port}:127.0.0.1:{port}");
    ssh && rest.windows(2).any(|w| matches!(w, [flag, target] if (*flag == "-W" && *target == out) || (*flag == "-R" && *target == reverse)))
}

/// At start: kills each ssh a pid file names that is still a forward's ssh, then empties the file; says how many.
pub fn sweep_leftovers(path: &Path, journal: &dyn Journal) -> usize {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    let mut killed = 0_usize;
    for (pid, port) in read_pids(&text) {
        let Ok(out) = Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
        else {
            continue;
        };
        let command = String::from_utf8_lossy(&out.stdout);
        if is_forward_ssh(command.trim(), port)
            && Command::new("/bin/kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success())
        {
            killed = killed.saturating_add(1);
            journal.record(&format!(
                "forward: a {port} ssh left by an earlier run killed (pid {pid})"
            ));
        }
    }
    let _emptied: io::Result<()> = fs::write(path, "");
    killed
}

#[cfg(test)]
mod tests;
