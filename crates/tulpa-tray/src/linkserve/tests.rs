//! linkserve tests: a fake dialer stands for the hub's streams (socket pairs), real loopback
//! listeners and udp sockets stand for the Mac's side, and the wire contract — the `open n` lines,
//! the u16 frames — is spoken exactly as the hub speaks it.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tulpa_hands::{Closer, Journal};
use tulpa_stop::Stop;

use super::{LinkDial, LinkServe, PEER_PENDING_MAX, StreamConn};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// How long a wait for something the serve does may take before the test fails: its threads may wait their turn
/// on a loaded box (the 3 s these waits had failed there).
const GUARD: Duration = Duration::from_secs(30);

/// The journal the tests read back.
#[derive(Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    fn journal(&self) -> Arc<dyn Journal> {
        struct ToLines(Arc<Mutex<Vec<String>>>);
        impl Journal for ToLines {
            fn record(&self, line: &str) {
                if let Ok(mut held) = self.0.lock() {
                    held.push(line.to_owned());
                }
            }
        }
        Arc::new(ToLines(Arc::clone(&self.0)))
    }
}

fn wait_for(journal: &Arc<Mutex<Vec<String>>>, what: &str) -> bool {
    let deadline = Instant::now()
        .checked_add(GUARD)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if let Ok(held) = journal.lock()
            && held.iter().any(|l| l.contains(what))
        {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Waits for the served set to drain, as `LinkServe::drain` does.
fn gone(links: &LinkServe) -> bool {
    let deadline = Instant::now()
        .checked_add(GUARD)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if links.list().is_empty() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The hub as the tests play it: every dialed stream is a socket pair, and the test holds one
/// end. Keyed by stream number, not arrival order — the serve dials on its own threads now, so
/// two dials may land in either order.
struct FakeHub {
    conns: Mutex<HashMap<u32, Option<std::os::unix::net::UnixStream>>>,
}

impl FakeHub {
    fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// The test's end of stream n (0 = the control stream), if it came and was not taken.
    fn take(&self, n: u32) -> Option<std::os::unix::net::UnixStream> {
        let mut held = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.get_mut(&n).and_then(Option::take)
    }

    /// Whether stream n's dial has landed (taken or not).
    fn has(&self, n: u32) -> bool {
        self.conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&n)
    }

    fn count(&self) -> usize {
        self.conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl LinkDial for FakeHub {
    fn dial(&self, _link: &str, _secret: &str, stream: u32) -> std::io::Result<StreamConn> {
        let (mine, theirs) = std::os::unix::net::UnixStream::pair()?;
        let conn = StreamConn {
            from_hub: Box::new(mine.try_clone()?),
            to_hub: Box::new(mine),
            child: Arc::new(PairCloser(theirs.try_clone()?)),
        };
        {
            let mut held = self
                .conns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held.entry(stream).or_insert(Some(theirs));
        }
        Ok(conn)
    }
}

/// A hub whose dials can be slowed and made to fail, per stream number — the tests pin that a
/// slow dial never blocks the serve's loops, and what a peer's datagrams do while it runs.
struct SlowHub {
    hub: Arc<FakeHub>,
    delays: Mutex<HashMap<u32, Duration>>,
    fails: Mutex<HashSet<u32>>,
}

impl LinkDial for SlowHub {
    fn dial(&self, link: &str, secret: &str, stream: u32) -> std::io::Result<StreamConn> {
        // The delay is copied out before the sleep: a guard held across it would serialize the
        // very dials these tests mean to run beside each other.
        let delay = self
            .delays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&stream)
            .copied();
        if let Some(delay) = delay {
            thread::sleep(delay);
        }
        if self
            .fails
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&stream)
        {
            return Err(std::io::Error::other("the test fails this dial"));
        }
        self.hub.dial(link, secret, stream)
    }
}

/// Closing shuts the pair's other end down, as killing the ssh child ends the stream.
struct PairCloser(std::os::unix::net::UnixStream);

impl Closer for PairCloser {
    fn close(&self) {
        let _down: std::io::Result<()> = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// One served test: the organ, the fake hub, the pause flag, the journal's lines, and the free
/// ports the hub may ask for — one tcp, one udp, so parallel tests never share a listener.
struct Rig {
    links: Arc<LinkServe>,
    hub: Arc<FakeHub>,
    paused: Arc<tulpa_stop::Stop>,
    journal: Arc<Mutex<Vec<String>>>,
    tcp_port: u16,
    udp_port: u16,
}

impl Rig {
    /// The hub's ask for a tcp serve on this rig's tcp port.
    fn ask_tcp(&self) -> String {
        format!(
            r#"{{"link":"l-0123abcd","proto":"tcp","port":{},"ttl":3600,"secret":"{SECRET}"}}"#,
            self.tcp_port
        )
    }

    /// The hub's ask for a udp serve on this rig's udp port.
    fn ask_udp(&self) -> String {
        format!(
            r#"{{"link":"l-0123abce","proto":"udp","port":{},"ttl":3600,"secret":"{SECRET}"}}"#,
            self.udp_port
        )
    }
}

const SECRET: &str = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d4e5f6a7b8c9d0e1f2a3b4c5d4e5f6a7b8";

fn free_tcp_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn free_udp_port() -> Result<u16> {
    Ok(UdpSocket::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn rig_with(quiet: Option<Duration>) -> Result<Rig> {
    let hub = Arc::new(FakeHub::new());
    let dial: Arc<dyn LinkDial> = Arc::clone(&hub) as Arc<dyn LinkDial>;
    rig_from(dial, hub, quiet)
}

/// A rig on a dial the test shapes (a `SlowHub`) — its `hub` field is the shaped hub's inner one,
/// so the take/wait helpers work unchanged.
fn rig_from(dial: Arc<dyn LinkDial>, hub: Arc<FakeHub>, quiet: Option<Duration>) -> Result<Rig> {
    let records = Lines::default();
    let journal = records.journal();
    let paused = Arc::new(Stop::new());
    let changed: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let (tcp_port, udp_port) = (free_tcp_port()?, free_udp_port()?);
    let mut made = LinkServe::new(
        vec![tcp_port, udp_port],
        dial,
        Arc::clone(&paused),
        journal,
        changed,
    );
    if let Some(quiet) = quiet {
        made = made.with_peer_quiet(quiet);
    }
    Ok(Rig {
        links: Arc::new(made),
        hub,
        paused,
        journal: records.0,
        tcp_port,
        udp_port,
    })
}

fn rig() -> Result<Rig> {
    rig_with(None)
}

fn rig_quiet(quiet: Duration) -> Result<Rig> {
    rig_with(Some(quiet))
}

/// The test's end of the n-th dialed stream, once it comes (its dial follows a connection or an
/// `open` line the test sent, so it may lag; 0 = the control stream).
fn take_when(hub: &FakeHub, n: u32) -> Option<std::os::unix::net::UnixStream> {
    let deadline = Instant::now()
        .checked_add(GUARD)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if let Some(conn) = hub.take(n) {
            return Some(conn);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The line as the hub's broker sends it: the argument one JSON string holding the hub's JSON.
fn line(verb: &str, id: &str, arg: &str) -> Vec<u8> {
    serde_json::json!({"id": id, "verb": verb, "arg": arg})
        .to_string()
        .into_bytes()
}

fn ok_of(reply: &[u8]) -> Option<(bool, String)> {
    let parsed: serde_json::Value = serde_json::from_slice(reply.trim_ascii_end()).ok()?;
    let ok = parsed.get("ok")?.as_bool()?;
    let text = if ok {
        parsed.get("result").and_then(|v| v.as_str())
    } else {
        parsed.get("err").and_then(|v| v.as_str())
    }?;
    Some((ok, text.to_owned()))
}

#[test]
fn parse_ask_takes_the_hubs_shapes_and_this_macs_list() {
    use super::parse_ask;
    let allowed = [24642u16];
    let good = format!(
        r#"{{"link":"l-0123abce","proto":"udp","port":24642,"ttl":3600,"secret":"{SECRET}"}}"#
    );
    assert!(parse_ask(&good, &allowed).is_ok());
    let secret = SECRET;
    for bad in [
        format!(
            r#"{{"link":"l-0123abc","proto":"udp","port":24642,"ttl":60,"secret":"{secret}"}}"#
        ),
        format!(
            r#"{{"link":"l-0123ABCD","proto":"udp","port":24642,"ttl":60,"secret":"{secret}"}}"#
        ),
        format!(
            r#"{{"link":"l-0123abcd","proto":"sctp","port":24642,"ttl":60,"secret":"{secret}"}}"#
        ),
        format!(
            r#"{{"link":"l-0123abcd","proto":"udp","port":25565,"ttl":60,"secret":"{secret}"}}"#
        ),
        format!(
            r#"{{"link":"l-0123abcd","proto":"udp","port":24642,"ttl":0,"secret":"{secret}"}}"#
        ),
        r#"{"link":"l-0123abcd","proto":"udp","port":24642,"ttl":60,"secret":"short"}"#.to_string(),
    ] {
        assert!(parse_ask(&bad, &allowed).is_err(), "{bad}");
    }
}

#[test]
fn a_tcp_client_serve_splices_each_connection_to_its_own_stream() -> Result<()> {
    let r = rig()?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "1", &r.ask_tcp()));
    assert_eq!(
        ok_of(&reply),
        Some((true, format!("l-0123abcd tcp {} served", r.tcp_port)))
    );

    // The hub holds the control stream; a connection from the Mac's side dials stream 1.
    let control = take_when(&r.hub, 0).ok_or("the control stream did not come")?;
    let mut client = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    let mut stream1 = take_when(&r.hub, 1).ok_or("stream 1 did not come")?;
    client.write_all(b"to the hub")?;
    let mut got = [0u8; 10];
    stream1.read_exact(&mut got)?;
    assert_eq!(&got, b"to the hub");
    stream1.write_all(b"to the mac")?;
    client.read_exact(&mut got)?;
    assert_eq!(&got, b"to the mac");

    // The serve's row is up; the hub ending the control stream (the link ended) ends the serve —
    // a shutdown, not a drop: the fake's closer holds a dup of the hub's end.
    assert_eq!(r.links.list().len(), 1);
    let _shut: std::io::Result<()> = control.shutdown(std::net::Shutdown::Both);
    assert!(gone(&r.links));
    assert!(wait_for(&r.journal, "ended (the control stream ended)"));
    Ok(())
}

#[test]
fn a_tcp_host_serve_joins_the_hubs_open_lines_to_the_local_port() -> Result<()> {
    let r = rig()?;
    let echo = TcpListener::bind(("127.0.0.1", r.tcp_port))?;
    thread::spawn(move || {
        if let Ok((mut conn, _)) = echo.accept() {
            let mut got = [0u8; 15];
            if conn.read_exact(&mut got).is_ok() {
                let _wrote: std::io::Result<()> = conn.write_all(&got);
            }
        }
    });
    let reply = r.links.handle(&line("link.serve-host", "2", &r.ask_tcp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let mut control = take_when(&r.hub, 0).ok_or("the control stream did not come")?;
    control.write_all(b"open 1\n")?;
    let mut stream1 = take_when(&r.hub, 1).ok_or("stream 1 did not come")?;
    stream1.write_all(b"through the mac")?;
    let mut got = [0u8; 15];
    stream1.read_exact(&mut got)?;
    assert_eq!(&got, b"through the mac");
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

#[test]
fn a_udp_client_serve_frames_each_peer_on_its_own_stream_and_drops_the_quiet() -> Result<()> {
    let r = rig_quiet(Duration::from_millis(300))?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "3", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    peer.connect(("127.0.0.1", r.udp_port))?;
    peer.send(b"a datagram")?;
    let mut stream1 = take_when(&r.hub, 1).ok_or("stream 1 did not come")?;
    let mut head = [0u8; 2];
    stream1.read_exact(&mut head)?;
    assert_eq!(head, 10u16.to_be_bytes());
    let mut body = [0u8; 10];
    stream1.read_exact(&mut body)?;
    assert_eq!(&body, b"a datagram");
    // The hub's framed reply reaches the peer.
    let mut frame = Vec::new();
    frame.extend_from_slice(&9u16.to_be_bytes());
    frame.extend_from_slice(b"its reply");
    stream1.write_all(&frame)?;
    let mut buf = [0u8; 32];
    let (len, _) = peer.recv_from(&mut buf)?;
    assert_eq!(buf.get(..len).unwrap_or(&[]), b"its reply".as_slice());
    // A second peer is its own stream.
    let peer2 = UdpSocket::bind("127.0.0.1:0")?;
    peer2.connect(("127.0.0.1", r.udp_port))?;
    peer2.send(b"from two")?;
    let mut stream2 = take_when(&r.hub, 2).ok_or("stream 2 did not come")?;
    stream2.read_exact(&mut head)?;
    assert_eq!(head, 8u16.to_be_bytes());
    // Quiet peers are dropped: the journal says so, and a later datagram dials a fresh stream.
    assert!(wait_for(&r.journal, "quiet, its stream closed"));
    peer.send(b"again")?;
    assert!(wait_for_count(&r.hub, 4));
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

fn wait_for_count(hub: &FakeHub, at_least: usize) -> bool {
    let deadline = Instant::now()
        .checked_add(GUARD)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if hub.count() >= at_least {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn the_cap_drops_the_seventeenth_peer_and_names_it() -> Result<()> {
    let r = rig()?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "4", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let mut peers = Vec::new();
    for i in 0..17 {
        let peer = UdpSocket::bind("127.0.0.1:0")?;
        peer.connect(("127.0.0.1", r.udp_port))?;
        peer.send(format!("cap {i}").as_bytes())?;
        peers.push(peer.local_addr()?);
    }
    // Which peer the loop sees last is the load's to decide: sixteen ride a stream of their own, and the one
    // the cap dropped is the one its line names.
    let mut carried = HashSet::new();
    for n in 1..=16 {
        let mut stream = take_when(&r.hub, n).ok_or("a stream did not come")?;
        stream.set_read_timeout(Some(GUARD))?;
        carried.insert(String::from_utf8(read_frame(&mut stream)?)?);
    }
    assert!(wait_for(&r.journal, "streams are up"));
    let refused: Vec<String> = r
        .journal
        .lock()
        .map_err(|_| "poisoned")?
        .iter()
        .filter(|l| l.contains("streams are up"))
        .cloned()
        .collect();
    let named: Vec<usize> = (peers.iter().enumerate())
        .filter(|(_, peer)| {
            let dropped = format!("peer {peer} dropped:");
            refused.iter().any(|l| l.contains(&dropped))
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!((refused.len(), named.len()), (1, 1), "{refused:?}");
    let expected: HashSet<String> = (0..peers.len())
        .filter(|i| !named.contains(i))
        .map(|i| format!("cap {i}"))
        .collect();
    assert_eq!(carried, expected, "the named peer rode no stream");
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

/// Reads one u16-framed datagram off a stream the hub side holds.
fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Result<Vec<u8>> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let len = usize::from(u16::from_be_bytes(head));
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn slow_rig(delays: Vec<(u32, Duration)>, fails: Vec<u32>) -> Result<(Rig, Arc<FakeHub>)> {
    let hub = Arc::new(FakeHub::new());
    let slow = Arc::new(SlowHub {
        hub: Arc::clone(&hub),
        delays: Mutex::new(delays.into_iter().collect()),
        fails: Mutex::new(fails.into_iter().collect()),
    });
    let dial: Arc<dyn LinkDial> = Arc::clone(&slow) as Arc<dyn LinkDial>;
    let rig = rig_from(dial, Arc::clone(&hub), None)?;
    Ok((rig, hub))
}

// A new peer's first datagrams hold for its stream while the dial runs, and ride it in order
// the moment it is up — none lost to the setup itself.
#[test]
fn a_new_peers_first_datagrams_hold_and_ride_in_order() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(250))], vec![])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "20", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    peer.connect(("127.0.0.1", r.udp_port))?;
    peer.send(b"one")?;
    peer.send(b"two")?;
    peer.send(b"three")?;
    let mut stream1 = take_when(&hub, 1).ok_or("stream 1 did not come")?;
    assert_eq!(read_frame(&mut stream1)?, b"one".as_slice());
    assert_eq!(read_frame(&mut stream1)?, b"two".as_slice());
    assert_eq!(read_frame(&mut stream1)?, b"three".as_slice());
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// The dial runs beside the loop: a peer whose dial is slow does not delay another peer's —
// the second peer's stream comes up while the first is still dialing.
#[test]
fn a_slow_dial_does_not_block_the_loop_for_another_peer() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(600))], vec![])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "21", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let peer_a = UdpSocket::bind("127.0.0.1:0")?;
    peer_a.connect(("127.0.0.1", r.udp_port))?;
    peer_a.send(b"a first")?;
    let peer_b = UdpSocket::bind("127.0.0.1:0")?;
    peer_b.connect(("127.0.0.1", r.udp_port))?;
    peer_b.send(b"b first")?;
    let stream2 = take_when(&hub, 2).ok_or("stream 2 did not come")?;
    assert!(
        !hub.has(1),
        "the second peer's stream must not wait for the first's dial"
    );
    drop(stream2);
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// A dial that fails drops what it held, named with the count, and the peer is tried again —
// its next datagram dials a fresh stream.
#[test]
fn a_failed_dial_drops_what_it_held_and_is_tried_again() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(150))], vec![1])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "22", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    peer.connect(("127.0.0.1", r.udp_port))?;
    peer.send(b"first")?;
    thread::sleep(Duration::from_millis(20));
    peer.send(b"second")?;
    assert!(wait_for(&r.journal, "would not open (2 datagrams held)"));
    peer.send(b"third")?;
    let mut stream2 = take_when(&hub, 2).ok_or("the retry did not dial")?;
    assert_eq!(read_frame(&mut stream2)?, b"third".as_slice());
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// The hold is bounded: past its datagram bound the oldest go, named once in the journal, and
// what rides when the dial lands is the newest run, in order.
#[test]
fn the_hold_is_bounded_and_names_its_oldest_drop() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(400))], vec![])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "23", &r.ask_udp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    peer.connect(("127.0.0.1", r.udp_port))?;
    let burst = PEER_PENDING_MAX + 10;
    for i in 0..burst {
        peer.send(format!("d{i:03}").as_bytes())?;
    }
    assert!(wait_for(&r.journal, "oldest held datagram"));
    let mut stream1 = take_when(&hub, 1).ok_or("stream 1 did not come")?;
    for i in 10..burst {
        assert_eq!(
            read_frame(&mut stream1)?,
            format!("d{i:03}").as_bytes(),
            "the hold kept the wrong run"
        );
    }
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// The host's `open n` lines dial beside the control reader: the second line's stream comes up
// while the first's dial still runs.
#[test]
fn the_host_dials_two_open_lines_in_parallel() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(600))], vec![])?;
    let reply = r.links.handle(&line("link.serve-host", "24", &r.ask_tcp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let mut control = take_when(&hub, 0).ok_or("the control stream did not come")?;
    control.write_all(b"open 1\nopen 2\n")?;
    let stream2 = take_when(&hub, 2).ok_or("stream 2 did not come")?;
    assert!(
        !hub.has(1),
        "the second open must not wait for the first's dial"
    );
    drop(stream2);
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// The tcp twin: a connection whose dial is slow does not delay the next connection's.
#[test]
fn a_slow_tcp_dial_does_not_block_the_next_connection() -> Result<()> {
    let (r, hub) = slow_rig(vec![(1, Duration::from_millis(600))], vec![])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "25", &r.ask_tcp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let _control = take_when(&hub, 0).ok_or("the control stream did not come")?;
    let _conn1 = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    let _conn2 = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    let stream2 = take_when(&hub, 2).ok_or("stream 2 did not come")?;
    assert!(
        !hub.has(1),
        "the second connection must not wait for the first's dial"
    );
    drop(stream2);
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// akira 04:4xZ B1: the concurrent count must fall when streams end. It used to ratchet —
// incremented at every stream start, never decremented — so 16 cumulative tcp connections,
// each long closed, left the client side refusing every new one ("the link's 16 streams are
// up") until the link's ttl: about sixteen game reconnects and the side was dead. Sixteen
// connections, each closed before the next; the seventeenth must still dial a stream.
#[test]
fn the_cap_frees_streams_that_end_so_a_reconnect_is_not_refused() -> Result<()> {
    let r = rig()?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "11", &r.ask_tcp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let _control = take_when(&r.hub, 0).ok_or("the control stream did not come")?;
    let mut hub_sides = Vec::new();
    for n in 1..=16 {
        let conn = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
        hub_sides.push(take_when(&r.hub, n).ok_or("a stream did not come")?);
        drop(conn);
    }
    // Both ends of every pair down (a shutdown, not a drop: the fake's closer holds a dup of
    // the hub's end), then wait for the sixteen close lines: the count is freed by them.
    for side in &mut hub_sides {
        let _shut: std::io::Result<()> = side.shutdown(std::net::Shutdown::Both);
    }
    drop(hub_sides);
    let mut closed = 0;
    for _ in 0..250 {
        closed = r
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|l| l.contains(" closed ("))
            .count();
        if closed >= 16 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(closed >= 16, "{closed} of 16 close lines came");
    let _reconnect = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    assert!(
        take_when(&r.hub, 17).is_some(),
        "the 17th connection after 16 closed ones must dial a stream"
    );
    // The cap is checked before the dial: with the seventeenth stream up, its refusal can no longer come.
    assert!(
        !r.journal
            .lock()
            .map_err(|_| "poisoned")?
            .iter()
            .any(|l| l.contains("streams are up"))
    );
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

// The tcp cap counts dials while they are still opening: sixteen connections whose dials are
// slow hold all sixteen places, and the seventeenth is refused without dialing — the count
// comes off when a stream is up or refused, not when its thread first runs (akira's B1).
#[test]
fn the_tcp_cap_counts_dials_while_they_are_still_opening() -> Result<()> {
    let delays: Vec<(u32, Duration)> = (1..=16).map(|n| (n, Duration::from_millis(600))).collect();
    let (r, hub) = slow_rig(delays, vec![])?;
    let reply = r
        .links
        .handle(&line("link.serve-client", "21", &r.ask_tcp()));
    assert!(ok_of(&reply).is_some_and(|(ok, _)| ok));
    let _control = take_when(&r.hub, 0).ok_or("the control stream did not come")?;
    for _ in 0..16 {
        TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    }
    // The sixteen dials are held by their delays when the seventeenth connection arrives: the
    // cap must see them and refuse it without a dial.
    thread::sleep(Duration::from_millis(150));
    let _seventeenth = TcpStream::connect(("127.0.0.1", r.tcp_port))?;
    assert!(
        wait_for(&r.journal, "streams are up"),
        "the seventeenth connection must be refused while sixteen dials are opening"
    );
    // And it never dialed: no seventeenth stream exists once the delays have passed.
    thread::sleep(Duration::from_millis(700));
    assert!(hub.take(17).is_none(), "a refused connection must not dial");
    r.links.close_all("the test ends");
    r.links.drain(Duration::from_secs(3));
    Ok(())
}

#[test]
fn refusals_a_port_not_listed_paused_and_the_verb_gates() -> Result<()> {
    let r = rig()?;
    let off = format!(
        r#"{{"link":"l-0123abcf","proto":"tcp","port":31337,"ttl":60,"secret":"{SECRET}"}}"#
    );
    let (ok, why) = ok_of(&r.links.handle(&line("link.serve-client", "5", &off)))
        .unwrap_or((false, String::new()));
    assert!(
        (!ok) && why.contains("not on this Mac's [linkserve] ports"),
        "{why}"
    );

    r.paused.set();
    let (ok, why) = ok_of(
        &r.links
            .handle(&line("link.serve-client", "6", &r.ask_tcp())),
    )
    .unwrap_or((false, String::new()));
    assert!((!ok) && why == "paused");
    r.paused.clear();

    // The verb gate: an unknown verb and a missing argument are refused, never reaching a dial.
    let (ok, why) = ok_of(
        &r.links
            .handle(&line("link.serve-whatever", "7", &r.ask_tcp())),
    )
    .unwrap_or((false, String::new()));
    assert!((!ok) && why == "verb");
    let nolink = br#"{"id":"8","verb":"link.serve-host"}"#;
    let (ok, why) = ok_of(&r.links.handle(nolink)).unwrap_or((false, String::new()));
    assert!((!ok) && why == "arg");
    Ok(())
}

#[test]
fn route_refuses_the_verb_with_no_dialer_and_passes_others_through() {
    let passed = Arc::new(AtomicBool::new(false));
    let inner: crate::link::LineHandler = {
        let passed = Arc::clone(&passed);
        Arc::new(move |_line: &[u8]| {
            passed.store(true, Ordering::SeqCst);
            b"{\"id\":\"x\",\"ok\":true}\n".to_vec()
        })
    };
    let routed = super::route(inner, None);
    let reply = routed(br#"{"id":"9","verb":"link.serve-client","arg":"{}"}"#);
    assert!(ok_of(&reply).is_some_and(|(ok, _)| !ok));
    assert!(!passed.load(Ordering::SeqCst));
    routed(br#"{"id":"10","verb":"ping"}"#);
    assert!(passed.load(Ordering::SeqCst));
}

#[test]
fn close_items_round_trip() {
    assert_eq!(super::close_item("l-0123abcd"), "link-close:l-0123abcd");
    assert_eq!(
        super::close_item_of("link-close:l-0123abcd").as_deref(),
        Some("l-0123abcd")
    );
    assert!(super::close_item_of("forward-close:25565:out").is_none());
}
