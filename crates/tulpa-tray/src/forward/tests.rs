use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tulpa_hands::Journal;

use super::*;
use crate::testutil::Killed;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

impl Journal for Notes {
    fn record(&self, line: &str) {
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line.to_owned());
        }
    }
}

impl Notes {
    fn all(&self) -> Vec<String> {
        self.0.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// A child that is alive until killed, or (refused) never was; `ready` once the server "confirmed" it.
struct FakeChild {
    alive: AtomicBool,
    ready: AtomicBool,
    why: String,
    stream: Option<TcpStream>,
}

impl SshChild for FakeChild {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(stream) = &self.stream {
            let _shut: std::io::Result<()> = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn why(&self) -> String {
        self.why.clone()
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

/// How the fake server answers an in forward's `-R`.
#[derive(Clone, Copy)]
enum Answer {
    /// Confirms at once.
    Accept,
    /// Refuses at once: ssh has exited.
    Refuse,
    /// Refuses after this long, never confirming first (the jump route's slow refusal).
    RefuseAfter(Duration),
    /// Never answers.
    Silent,
}

/// The server's end: a local echo server stands in for `127.0.0.1:N` on the server.
struct FakeSpawn {
    server: std::net::SocketAddr,
    answer: Mutex<Answer>,
    reverses: Mutex<Vec<Arc<FakeChild>>>,
}

impl FakeSpawn {
    fn new(refuse_reverse: bool) -> Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let server = listener.local_addr()?;
        thread::spawn(move || {
            for stream in listener.incoming().map_while(std::result::Result::ok) {
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = [0_u8; 1024];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let echoed: Vec<u8> = buf.get(..n).unwrap_or_default().to_ascii_uppercase();
                        if stream.write_all(&echoed).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Ok(Arc::new(Self {
            server,
            answer: Mutex::new(if refuse_reverse {
                Answer::Refuse
            } else {
                Answer::Accept
            }),
            reverses: Mutex::new(Vec::new()),
        }))
    }
}

impl Spawn for FakeSpawn {
    fn tunnel(&self, _port: u16) -> std::io::Result<Tunnel> {
        let stream = TcpStream::connect(self.server)?;
        let child = Arc::new(FakeChild {
            alive: AtomicBool::new(true),
            ready: AtomicBool::new(true),
            why: String::new(),
            stream: Some(stream.try_clone()?),
        });
        Ok(Tunnel {
            from_server: Box::new(stream.try_clone()?),
            to_server: Box::new(HalfCloser(stream)),
            child,
        })
    }

    fn reverse(&self, _port: u16) -> std::io::Result<Arc<dyn SshChild>> {
        let answer = self.answer.lock().map_or(Answer::Accept, |a| *a);
        let child = Arc::new(FakeChild {
            alive: AtomicBool::new(!matches!(answer, Answer::Refuse)),
            ready: AtomicBool::new(matches!(answer, Answer::Accept)),
            why: "remote port forwarding failed for listen port 53682".to_owned(),
            stream: None,
        });
        if let Answer::RefuseAfter(after) = answer {
            let late = Arc::clone(&child);
            thread::spawn(move || {
                thread::sleep(after);
                late.alive.store(false, Ordering::SeqCst);
            });
        }
        if let Ok(mut all) = self.reverses.lock() {
            all.push(Arc::clone(&child));
        }
        Ok(child)
    }
}

/// The write half: dropped, the echo server sees EOF (as ssh's stdin closing does).
struct HalfCloser(TcpStream);

impl Write for HalfCloser {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Drop for HalfCloser {
    fn drop(&mut self) {
        let _shut: std::io::Result<()> = self.0.shutdown(std::net::Shutdown::Write);
    }
}

/// A loopback port free now.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map_or(0, |a| a.port())
}

struct Rig {
    forwards: Arc<Forwards>,
    notes: Arc<Notes>,
    paused: Arc<Stop>,
    spawn: Arc<FakeSpawn>,
    changes: Arc<AtomicU64>,
}

fn rig(allowed: Vec<u16>, refuse_reverse: bool) -> Result<Rig> {
    let notes = Arc::new(Notes::default());
    let paused = Arc::new(Stop::new());
    let spawn = FakeSpawn::new(refuse_reverse)?;
    let changes = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&changes);
    let journal: Arc<dyn Journal> = Arc::clone(&notes) as Arc<dyn Journal>;
    let spawner: Arc<dyn Spawn> = Arc::clone(&spawn) as Arc<dyn Spawn>;
    let forwards = Arc::new(
        Forwards::new(
            allowed,
            spawner,
            Arc::clone(&paused),
            journal,
            Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .with_start_wait(Duration::from_millis(150)),
    );
    Ok(Rig {
        forwards,
        notes,
        paused,
        spawn,
        changes,
    })
}

fn until(wait: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < wait {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn line(id: &str, verb: &str, arg: Option<&str>) -> Vec<u8> {
    let value = arg.map_or_else(
        || serde_json::json!({"id": id, "verb": verb}),
        |arg| serde_json::json!({"id": id, "verb": verb, "arg": arg}),
    );
    value.to_string().into_bytes()
}

fn answer(reply: &[u8]) -> (bool, String) {
    let value: serde_json::Value = serde_json::from_slice(reply).unwrap_or_default();
    let ok = value
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let text = value
        .get(if ok { "result" } else { "err" })
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (ok, text)
}

#[test]
fn open_args_default_by_direction_and_keep_to_their_limits() {
    let allowed = [53682_u16, 8080];
    assert_eq!(
        parse_open("53682 out", &allowed),
        Ok(Plan {
            port: 53682,
            dir: Dir::Out,
            ttl: OUT_TTL_DEFAULT,
            once: true
        }),
        "out: 5 min, once"
    );
    assert_eq!(
        parse_open("8080 in", &allowed),
        Ok(Plan {
            port: 8080,
            dir: Dir::In,
            ttl: IN_TTL_DEFAULT,
            once: false
        }),
        "in: 15 min"
    );
    assert_eq!(
        parse_open("53682 out ttl=900 many", &allowed).map(|p| (p.ttl.as_secs(), p.once)),
        Ok((900, false))
    );
    assert!(
        parse_open("53682 out ttl=901", &allowed)
            .is_err_and(|e| e.contains("past the out forward's 900 s"))
    );
    assert_eq!(
        parse_open("8080 in ttl=3600", &allowed).map(|p| p.ttl.as_secs()),
        Ok(3600)
    );
    assert!(parse_open("8080 in ttl=3601", &allowed).is_err());
    assert!(
        parse_open("8080 in once", &allowed)
            .is_err_and(|e| e.contains("once is for an out forward"))
    );
    assert!(parse_open("53682 out once many", &allowed).is_err());
    for bad in [
        "",
        "53682",
        "0 out",
        "70000 out",
        "53682 sideways",
        "53682 out ttl=0",
        "53682 out ttl=x",
        "53682 out please",
    ] {
        assert!(parse_open(bad, &allowed).is_err(), "{bad:?}");
    }
}

#[test]
fn a_port_off_the_macs_own_list_is_refused_whatever_the_hub_granted() -> Result<()> {
    let r = rig(vec![53682], false)?;
    let (ok, why) = answer(
        &r.forwards
            .handle(&line("1", "forward.open", Some("9222 out"))),
    );
    assert!(!ok);
    assert_eq!(
        why,
        "port 9222 is not on this Mac's forward list ([forward] ports)"
    );
    assert!(r.forwards.list().is_empty());
    assert!(
        r.notes
            .all()
            .iter()
            .any(|l| l.contains("open refused") && l.contains("9222"))
    );
    let off = rig(Vec::new(), false)?;
    let (ok, why) = answer(
        &off.forwards
            .handle(&line("2", "forward.open", Some("53682 out"))),
    );
    assert!(!ok && why.starts_with("forward: off on this Mac"), "{why}");
    Ok(())
}

#[test]
fn an_out_forward_carries_one_connection_then_closes_and_refuses_a_second() -> Result<()> {
    let port = free_port();
    let r = rig(vec![port], false)?;
    let (ok, said) = answer(&r.forwards.handle(&line(
        "1",
        "forward.open",
        Some(&format!("{port} out")),
    )));
    assert!(ok, "{said}");
    assert_eq!(said, format!("{port} → open for 300 s, once"));
    assert!(
        r.forwards
            .list()
            .first()
            .map(View::line)
            .is_some_and(|l| l == format!("{port} → (4:59 left, once)")
                || l == format!("{port} → (5:00 left, once)"))
    );
    let mut client = TcpStream::connect(("127.0.0.1", port))?;
    // Once: the port closes at this first accept (the count moves after the listener is let go); a second
    // connection is refused by the Mac, not queued. No connect checks it: once freed, the port is anyone's.
    assert!(until(Duration::from_secs(2), || r
        .forwards
        .list()
        .first()
        .is_some_and(|v| v.conns == 1)));
    client.write_all(b"code=abc").ok();
    let mut back = [0_u8; 8];
    client.read_exact(&mut back).ok();
    assert_eq!(&back, b"CODE=ABC", "carried both ways");
    drop(client);
    assert!(
        until(Duration::from_secs(5), || r.forwards.list().is_empty()),
        "{:?}",
        r.notes.all()
    );
    let notes = r.notes.all();
    assert!(
        notes
            .iter()
            .any(|l| l.ends_with(&format!("{port} → closed: its one connection ended (once)"))),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|l| l.contains("connection 1 ended (8 bytes to the server, 8 back)")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|l| l.contains("abc") || l.contains("ABC")),
        "no payload in the log: {notes:?}"
    );
    assert!(
        r.changes.load(Ordering::SeqCst) >= 2,
        "the menu heard the open and the close"
    );
    Ok(())
}

#[test]
fn a_many_out_forward_ends_at_its_time_limit() -> Result<()> {
    let port = free_port();
    let r = rig(vec![port], false)?;
    let plan = Plan {
        port,
        dir: Dir::Out,
        // Far past the two carries below (milliseconds, unloaded): a loaded box once took longer than the 400 ms
        // this was, and the forward ended under the second.
        ttl: Duration::from_secs(5),
        once: false,
    };
    assert!(r.forwards.open(plan).is_ok());
    for _ in 0..2 {
        let mut c = TcpStream::connect(("127.0.0.1", port))?;
        c.write_all(b"x").ok();
        let mut b = [0_u8; 1];
        c.read_exact(&mut b).ok();
        assert_eq!(&b, b"X");
    }
    assert!(until(Duration::from_secs(30), || r
        .forwards
        .list()
        .is_empty()));
    assert!(
        r.notes
            .all()
            .iter()
            .any(|l| l.ends_with(&format!("{port} → closed: its time limit")))
    );
    // The listener is let go before that line is written; no connect checks it: once freed, the port is
    // anyone's (another process may listen on it by now).
    Ok(())
}

#[test]
fn an_in_forward_holds_its_ssh_until_closed_and_a_refused_one_says_why() -> Result<()> {
    let r = rig(vec![53682], false)?;
    let (ok, said) = answer(&r.forwards.handle(&line(
        "1",
        "forward.open",
        Some("53682 in ttl=60"),
    )));
    assert!(ok, "{said}");
    assert_eq!(said, "53682 ← open for 60 s");
    let (ok, busy) = answer(
        &r.forwards
            .handle(&line("2", "forward.open", Some("53682 in"))),
    );
    assert!(!ok && busy == "busy: 53682 ← is open", "{busy}");
    let (ok, listed) = answer(&r.forwards.handle(&line("3", "forward.list", None)));
    assert!(
        ok && listed.starts_with("53682 ← (0:5") || listed.starts_with("53682 ← (1:00"),
        "{listed}"
    );
    let (ok, closed) = answer(
        &r.forwards
            .handle(&line("4", "forward.close", Some("53682 in"))),
    );
    assert!(ok && closed == "53682 ← closed", "{closed}");
    assert!(until(Duration::from_secs(2), || r
        .forwards
        .list()
        .is_empty()));
    let child = r
        .spawn
        .reverses
        .lock()
        .ok()
        .and_then(|all| all.first().cloned());
    assert!(child.is_some_and(|c| !c.alive()), "its ssh killed");
    let (ok, again) = answer(
        &r.forwards
            .handle(&line("5", "forward.close", Some("53682 in"))),
    );
    assert!(!ok && again == "53682 ← is not open");

    let refused = rig(vec![53682], true)?;
    let (ok, why) = answer(
        &refused
            .forwards
            .handle(&line("6", "forward.open", Some("53682 in"))),
    );
    assert!(!ok);
    assert_eq!(
        why,
        "53682 ←: the server refused it (remote port forwarding failed for listen port 53682)"
    );
    assert!(refused.forwards.list().is_empty());
    Ok(())
}

#[test]
fn an_in_forward_whose_ssh_dies_leaves_the_list() -> Result<()> {
    let r = rig(vec![8080], false)?;
    assert!(
        r.forwards
            .open(Plan {
                port: 8080,
                dir: Dir::In,
                ttl: IN_TTL_DEFAULT,
                once: false
            })
            .is_ok()
    );
    if let Some(child) = r
        .spawn
        .reverses
        .lock()
        .ok()
        .and_then(|all| all.first().cloned())
    {
        child.alive.store(false, Ordering::SeqCst);
    }
    assert!(until(Duration::from_secs(2), || r
        .forwards
        .list()
        .is_empty()));
    assert!(
        r.notes
            .all()
            .iter()
            .any(|l| l.contains("8080 ← closed: ssh ended"))
    );
    Ok(())
}

#[test]
fn pause_refuses_new_forwards_and_close_all_ends_every_one() -> Result<()> {
    let port = free_port();
    let r = rig(vec![port, 8080], false)?;
    assert!(
        r.forwards
            .open(Plan {
                port,
                dir: Dir::Out,
                ttl: OUT_TTL_DEFAULT,
                once: true
            })
            .is_ok()
    );
    assert!(
        r.forwards
            .open(Plan {
                port: 8080,
                dir: Dir::In,
                ttl: IN_TTL_DEFAULT,
                once: false
            })
            .is_ok()
    );
    assert_eq!(r.forwards.list().len(), 2);
    r.paused.set();
    assert_eq!(r.forwards.close_all("Pause all"), 2);
    r.forwards.drain(Duration::from_secs(3));
    assert!(r.forwards.list().is_empty());
    // Each listener is let go before its forward's closed line; once freed, the port is anyone's, so no connect.
    let notes = r.notes.all();
    for closed in [
        format!("{port} → closed: closed"),
        "8080 ← closed: closed".to_owned(),
    ] {
        assert!(
            notes.iter().any(|l| l.ends_with(&closed)),
            "{closed}: {notes:?}"
        );
    }
    let (ok, why) = answer(
        &r.forwards
            .handle(&line("1", "forward.open", Some("8080 in"))),
    );
    assert!(!ok && why == "paused");
    assert_eq!(
        r.notes
            .all()
            .iter()
            .filter(|l| l.contains("closing: Pause all"))
            .count(),
        2
    );
    Ok(())
}

#[test]
fn malformed_lines_are_refused_with_their_id() -> Result<()> {
    let r = rig(vec![53682], false)?;
    for (verb, arg, want) in [
        ("forward.open", None, "arg"),
        ("forward.list", Some("x"), "arg"),
        ("forward.close", None, "arg"),
        ("forward.jump", Some("53682 out"), "verb"),
    ] {
        let (ok, why) = answer(&r.forwards.handle(&line("9", verb, arg)));
        assert!(!ok && why == want, "{verb}: {why}");
    }
    let object = br#"{"id":"9","verb":"forward.open","arg":{"port":53682}}"#;
    assert_eq!(
        answer(&r.forwards.handle(object)),
        (false, "arg".to_owned())
    );
    Ok(())
}

#[test]
fn the_pid_file_names_only_forward_sshes_and_the_sweep_kills_no_stranger() {
    assert_eq!(
        read_pids("123 53682\nbad\n456 8080 extra\n"),
        vec![(123, 53682), (456, 8080)]
    );
    assert!(is_forward_ssh(
        "ssh -T -o BatchMode=yes -N -n -R 127.0.0.1:53682:127.0.0.1:53682 box",
        53682
    ));
    assert!(is_forward_ssh(
        "/usr/bin/ssh -T -W 127.0.0.1:53682 box",
        53682
    ));
    assert!(
        !is_forward_ssh("ssh -T -W 127.0.0.1:53682 box", 8080),
        "another port"
    );
    assert!(!is_forward_ssh("vim 127.0.0.1:53682", 53682), "not ssh");
    assert!(
        !is_forward_ssh("ssh -T -W localhost:22 box", 53682),
        "the link's own ssh"
    );
    let dir = std::env::temp_dir().join(format!("tulpa-forward-pids-{}", std::process::id()));
    let _made: std::io::Result<()> = std::fs::create_dir_all(&dir);
    let file = dir.join("forward.pids");
    // This test's own pid: alive, but not a forward's ssh — never killed.
    let _written: std::io::Result<()> =
        std::fs::write(&file, format!("{} 53682\n", std::process::id()));
    let notes = Notes::default();
    assert_eq!(sweep_leftovers(&file, &notes), 0);
    assert_eq!(
        std::fs::read_to_string(&file).ok().as_deref(),
        Some(""),
        "emptied"
    );
    let _removed: std::io::Result<()> = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_watchdog_kills_the_ssh_once_its_parent_is_gone() -> Result<()> {
    // A stand-in parent that ends at once, and a stand-in ssh that would sleep for a minute.
    let mut parent = Killed::spawn(Command::new("sleep").arg("0.2"))?;
    let mut long = Command::new("sleep");
    long.arg("60");
    let mut child = Killed::spawn(&mut under_watchdog(&long, parent.id()))?;
    let _reaped: std::io::Result<std::process::ExitStatus> = parent.wait();
    let gone = until(Duration::from_secs(6), || {
        matches!(child.try_wait(), Ok(Some(_)))
    });
    assert!(
        gone,
        "the child outlived its parent past two watchdog polls"
    );
    Ok(())
}

#[test]
fn the_watchdog_leaves_a_child_alone_while_its_parent_lives() -> Result<()> {
    let mut long = Command::new("sleep");
    long.arg("60");
    let mut child = Killed::spawn(&mut under_watchdog(&long, std::process::id()))?;
    thread::sleep(Duration::from_millis(2500));
    let alive = matches!(child.try_wait(), Ok(None));
    let _killed: std::io::Result<()> = child.kill();
    let _reaped: std::io::Result<std::process::ExitStatus> = child.wait();
    assert!(alive);
    Ok(())
}

#[test]
fn the_route_sends_forward_verbs_to_the_forwards_and_the_rest_on() -> Result<()> {
    let r = rig(vec![53682], false)?;
    let others: LineHandler = Arc::new(|_line: &[u8]| b"others\n".to_vec());
    let with = route(Arc::clone(&others), Some(Arc::clone(&r.forwards)));
    assert_eq!(with(&line("1", "ping", None)), b"others\n".to_vec());
    let (ok, listed) = answer(&with(&line("2", "forward.list", None)));
    assert!(ok && listed == "none", "{listed}");
    let without = route(others, None);
    assert_eq!(
        answer(&without(&line("3", "forward.open", Some("53682 out")))),
        (false, "verb".to_owned()),
        "no forwards here: an unknown verb"
    );
    Ok(())
}

#[test]
fn a_close_item_names_its_forward_and_nothing_else() {
    assert_eq!(close_item(53682, Dir::Out), "forward-close:53682:out");
    assert_eq!(
        close_item_of(&close_item(8080, Dir::In)),
        Some((8080, Dir::In))
    );
    for other in [
        "forward-close:x:out",
        "forward-close:53682:up",
        "pause",
        "forward-close:53682",
    ] {
        assert_eq!(close_item_of(other), None, "{other}");
    }
}

/// akira: on the jump route a refusal can come after the old 3 s; the open must wait for the server's own word.
#[test]
fn an_in_forward_is_open_only_on_the_servers_word() -> Result<()> {
    // Refused late (at 400 ms, the wait is 1.5 s): never answered "open".
    let late = rig(vec![53682], false)?;
    let late_forwards = Arc::new(
        Arc::try_unwrap(late.forwards)
            .map_err(|_| "forwards still shared")?
            .with_start_wait(Duration::from_millis(1500)),
    );
    if let Ok(mut a) = late.spawn.answer.lock() {
        *a = Answer::RefuseAfter(Duration::from_millis(400));
    }
    let (ok, why) = answer(&late_forwards.handle(&line("1", "forward.open", Some("53682 in"))));
    assert!(!ok, "a late refusal is no open: {why}");
    assert!(why.contains("the server refused it"), "{why}");
    assert!(late_forwards.list().is_empty());

    // No word at all: not open once the wait runs out, and its ssh killed.
    let silent = rig(vec![53682], false)?;
    let silent_forwards = Arc::new(
        Arc::try_unwrap(silent.forwards)
            .map_err(|_| "forwards still shared")?
            .with_start_wait(Duration::from_millis(300)),
    );
    if let Ok(mut a) = silent.spawn.answer.lock() {
        *a = Answer::Silent;
    }
    let (ok, why) = answer(&silent_forwards.handle(&line("2", "forward.open", Some("53682 in"))));
    assert!(!ok);
    assert_eq!(why, "53682 ←: not open — the server gave no word in 0 s");
    let child = silent
        .spawn
        .reverses
        .lock()
        .ok()
        .and_then(|all| all.first().cloned());
    assert!(child.is_some_and(|c| !c.alive()), "its ssh killed");
    assert!(
        silent
            .notes
            .all()
            .iter()
            .any(|l| l.contains("not open: no word from the server"))
    );
    Ok(())
}

/// The #0362 rehearsal's failure: an `-R` ssh outlived a `kill -9` of Tulpa. The in forward's watchdog ends its ssh
/// the moment the pipe's write end closes (Tulpa gone, however), with no poll of a pid.
#[test]
fn the_reverse_watchdog_ends_the_ssh_when_its_pipe_closes() -> Result<()> {
    let mut long = Command::new("sleep");
    long.arg("60");
    // The parent is this test process (alive): only the pipe can end it here.
    let mut child = Killed::spawn(
        under_reverse_watchdog(&long, std::process::id()).stdin(std::process::Stdio::piped()),
    )?;
    thread::sleep(Duration::from_millis(500));
    assert!(
        matches!(child.try_wait(), Ok(None)),
        "alive while the pipe is open"
    );
    drop(child.stdin.take());
    let gone = until(Duration::from_secs(3), || {
        matches!(child.try_wait(), Ok(Some(_)))
    });
    assert!(gone, "the ssh outlived its pipe");
    Ok(())
}

#[test]
fn the_reverse_watchdog_still_polls_the_parent() -> Result<()> {
    let mut parent = Killed::spawn(Command::new("sleep").arg("0.2"))?;
    let mut long = Command::new("sleep");
    long.arg("60");
    let mut child = Killed::spawn(
        under_reverse_watchdog(&long, parent.id()).stdin(std::process::Stdio::piped()),
    )?;
    let _reaped: std::io::Result<std::process::ExitStatus> = parent.wait();
    let gone = until(Duration::from_secs(6), || {
        matches!(child.try_wait(), Ok(Some(_)))
    });
    assert!(gone, "the pid net, with the pipe still open");
    Ok(())
}

/// akira: after a clean close the guards may run `stop` on a pid the ssh left — by then possibly another process's.
/// `stop` ends only a pid that is still the guarded program; anything else is left alone.
#[test]
fn stop_leaves_a_pid_that_is_no_longer_the_program() -> Result<()> {
    // A stand-in for "a new process that took the ssh's old pid": a cat, not the guarded program (sleep).
    let mut other = Killed::spawn(Command::new("cat").stdin(std::process::Stdio::piped()))?;
    let script = format!("{STOP_FN} me={}; stop", other.id());
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .arg(GUARD_NAME)
        .arg("1")
        .arg("sleep")
        .status()?;
    assert!(status.success());
    let alive = matches!(other.try_wait(), Ok(None));
    let _killed: std::io::Result<()> = other.kill();
    let _reaped: std::io::Result<std::process::ExitStatus> = other.wait();
    assert!(
        alive,
        "a process that isn't the guarded program survives stop"
    );
    // The guarded program itself is ended.
    let mut own = Killed::spawn(Command::new("sleep").arg("60"))?;
    let script = format!("{STOP_FN} me={}; stop", own.id());
    let _s = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .arg(GUARD_NAME)
        .arg("1")
        .arg("sleep")
        .status()?;
    let gone = until(Duration::from_secs(4), || {
        matches!(own.try_wait(), Ok(Some(_)))
    });
    assert!(gone, "the guarded program is ended");
    Ok(())
}

/// The rehearsal's kill -9 leftover, at the wiring: `reverse()` itself must run its ssh under the pipe guard and keep
/// the pipe. Dropping the kept write end (as Tulpa's death does) ends the stand-in ssh at once.
#[test]
fn reverse_runs_its_ssh_under_the_pipe_guard() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("tulpa-reverse-guard-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let fake = dir.join("ssh");
    // A stand-in ssh: says the remote forward succeeded, then waits a minute — as itself (no exec), so its name stays
    // `ssh` and the guard's `stop` (which ends only the program it guards) recognises it.
    crate::testutil::write_script(
        &fake,
        "#!/bin/sh\necho 'debug1: remote forward success for: listen 127.0.0.1:53682' >&2\nn=0; while [ $n -lt 60 ]; do sleep 1; n=$((n+1)); done\n",
    )?;
    let spec =
        crate::link::DialSpec::new("work-box", "127.0.0.1", 18030, "dev-1", dir.join("token"))?;
    let spawn = SshSpawn::new(
        spec,
        Arc::new(Mutex::new(None)),
        dir.join("forward.pids"),
        Arc::new(Notes::default()),
    )
    .with_program(&fake);
    let child = spawn.reverse_real(53682)?;
    assert!(
        child.keep.lock().is_ok_and(|k| k.is_some()),
        "reverse keeps the guard's pipe"
    );
    assert!(
        until(Duration::from_secs(3), || child.ready()),
        "the stand-in's success line is read"
    );
    let pid = child.pid;
    // Tulpa's death, as the pipe sees it: its write end closes. No kill() is called.
    if let Ok(mut keep) = child.keep.lock() {
        drop(keep.take());
    }
    let gone = until(Duration::from_secs(4), || !child.alive());
    let _left = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    let _removed: std::io::Result<()> = std::fs::remove_dir_all(&dir);
    assert!(gone, "the ssh outlived its pipe");
    Ok(())
}

/// #0530 (the gate's F5 family): no lock is held across a wait. A forward's ssh child is killed and reaped (a wait)
/// with its lock let go: a child whose reap does not come (in uninterruptible sleep) would otherwise hold every look
/// at whether it is alive with it. Afterwards it is not alive, and a second kill does nothing.
#[test]
fn a_killed_ssh_child_is_reaped_outside_its_lock() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("tulpa-forward-reap-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let child = Command::new("/bin/sleep").arg("30").spawn()?;
    let real = Arc::new(RealChild {
        pid: child.id(),
        child: Mutex::new(Some(child)),
        pids: PidFile::new(dir.join("pids")),
        tail: Arc::default(),
        ready: Arc::default(),
        keep: Mutex::new(None),
    });
    assert!(real.alive());
    let seen = std::rc::Rc::new(std::cell::Cell::new(None));
    let (told, looked) = (std::rc::Rc::clone(&seen), Arc::downgrade(&real));
    crate::ears::BEFORE_REAP.set(Some(Box::new(move || {
        told.set(looked.upgrade().map(|r| r.child.try_lock().is_ok()));
    })));
    real.kill();
    crate::ears::BEFORE_REAP.set(None);
    assert_eq!(seen.get(), Some(true), "reaped under the child's lock");
    assert!(!real.alive());
    real.kill();
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
