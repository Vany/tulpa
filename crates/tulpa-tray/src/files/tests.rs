//! The files organ served as the link serves it: a line in, the reply out,
//! the bytes on a real data connection, the bundle in staging at the end.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use tulpa_hands::xfer::Xfer;
use tulpa_hands::{Closer, DataConn, DataDial, HalfWrite, Journal, Ticket};

use super::{Files, route};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const TICKET: &str = "T1cket-0123456789abcdefABCDEF_xy";

/// A journal that keeps its lines for the assertions.
#[derive(Default)]
struct Notes(std::sync::Mutex<Vec<String>>);

impl Journal for Notes {
    fn record(&self, line: &str) {
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line.to_owned());
        }
    }
}

impl Notes {
    fn text(&self) -> String {
        self.0.lock().map(|l| l.join("\n")).unwrap_or_default()
    }
}

struct TcpHalf(TcpStream);

impl Write for TcpHalf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl HalfWrite for TcpHalf {
    fn half_close(&mut self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
}

struct TcpClose(TcpStream);

impl Closer for TcpClose {
    fn close(&self) {
        let _closed: std::io::Result<()> = self.0.shutdown(Shutdown::Both);
    }
}

/// The Mac's data dial, to the fake hub standing in for the server side.
struct LocalDial(std::net::SocketAddr);

impl DataDial for LocalDial {
    fn dial(&self, ticket: &Ticket, offset: Option<u64>) -> std::io::Result<DataConn> {
        let mut stream = TcpStream::connect(self.0)?;
        let hello = offset.map_or_else(
            || format!("{{\"hello\":\"mac\",\"data\":\"{}\"}}\n", ticket.reveal()),
            |n| {
                format!(
                    "{{\"hello\":\"mac\",\"data\":\"{}\",\"offset\":{n}}}\n",
                    ticket.reveal()
                )
            },
        );
        stream.write_all(hello.as_bytes())?;
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(DataConn::new(
            Box::new(reader),
            Box::new(TcpHalf(stream)),
            Arc::new(TcpClose(closer)),
        ))
    }
}

/// Waits until the transfer has let its slot go — by then the landed
/// callback, which runs before the slot goes, has finished too.
fn settle(stream: &Xfer) -> bool {
    for _ in 0..400 {
        if !stream.is_busy() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn sha_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut out = String::new();
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _w: std::fmt::Result = write!(out, "{byte:02x}");
    }
    out
}

/// One field of a reply, Null when absent (indexing could panic).
fn field<'v>(value: &'v Value, key: &str) -> &'v Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// Makes a tiny but complete Tulpa.app archive, as the server would.
fn bundle(dir: &Path, commit: &str) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let made = dir.join("made");
    let app = made.join("Tulpa.app");
    fs::create_dir_all(app.join("Contents/MacOS"))?;
    fs::create_dir_all(app.join("Contents/Resources"))?;
    let exe = app.join("Contents/MacOS/tulpa");
    fs::write(&exe, "#!/bin/sh\nsleep 1\n")?;
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
    fs::write(
        app.join("Contents/Resources/build"),
        format!("version=1.0.0\ncommit={commit}\nutc=2026-01-01T00:00:00Z\n"),
    )?;
    let archive = made.join("Tulpa.tgz");
    let packed = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&made)
        .arg("Tulpa.app")
        .output()?;
    assert!(packed.status.success(), "the test bundle was made");
    Ok(fs::read(&archive)?)
}

/// The organ over a real dial, its support directory in a temp dir, routed
/// as macos.rs routes it with the hands absent (their refusal at the end).
struct Rig {
    home: tempfile::TempDir,
    journal: Arc<Notes>,
    outbox: Arc<crate::link::Outbox>,
    stream: Arc<Xfer>,
    hub: TcpListener,
    handler: crate::link::LineHandler,
}

fn rig() -> Result<Rig> {
    let home = tempfile::tempdir()?;
    let hub = TcpListener::bind("127.0.0.1:0")?;
    let dial: Arc<dyn DataDial> = Arc::new(LocalDial(hub.local_addr()?));
    let stream = Arc::new(Xfer::new(dial, Box::new(|| {})));
    let journal = Arc::new(Notes::default());
    let lent: Arc<dyn Journal> = Arc::<Notes>::clone(&journal);
    let outbox = Arc::new(crate::link::Outbox::default());
    let files = Files::new(
        home.path(),
        Arc::clone(&stream),
        lent,
        None,
        Arc::clone(&outbox),
    );
    let handler = route(
        Arc::new(crate::link::refuse_line) as crate::link::LineHandler,
        Some(files),
    );
    Ok(Rig {
        home,
        journal,
        outbox,
        stream,
        hub,
        handler,
    })
}

impl Rig {
    /// The support directory under the rig's home.
    fn support(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    /// Sends one line through the route as the link does, reads the reply.
    fn ask(&self, line: &Value) -> Result<Value> {
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        let reply = (self.handler)(&bytes);
        let reply = String::from_utf8_lossy(&reply).trim().to_owned();
        Ok(serde_json::from_str(&reply)?)
    }

    /// Accepts the dialed data connection, reads its hello line.
    fn accept(&self) -> Result<(TcpStream, Value)> {
        let (mut stream, _) = self.hub.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut hello = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            if stream.read(&mut byte)? == 0 {
                break;
            }
            hello.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        Ok((stream, serde_json::from_slice(&hello)?))
    }
}

/// Reads the hub side's verdict: everything until the Mac closes.
fn verdict_of(stream: &mut TcpStream) -> Result<Value> {
    let mut all = Vec::new();
    stream.read_to_end(&mut all)?;
    let text = String::from_utf8_lossy(&all).trim().to_owned();
    Ok(serde_json::from_str(&text)?)
}

#[test]
fn a_bundle_put_through_the_route_lands_and_is_staged() -> Result<()> {
    let rig = rig()?;
    let data = bundle(rig.home.path(), "1a2b3c4d5e6f7890aabbccddeeff001122334455")?;

    // The hands are absent and anything may be paused: files.put still serves.
    let reply = rig.ask(&json!({
        "id": "g1", "verb": "files.put",
        "arg": {"ticket": TICKET, "path": "Tulpa-1a2b3c4d.tgz",
                "size": data.len(), "sha256": sha_hex(&data)}
    }))?;
    assert_eq!(field(&reply, "ok"), &json!(true), "{reply}");
    assert_eq!(
        field(field(&reply, "result"), "offset"),
        &json!(0),
        "{reply}"
    );
    let (mut hub, hello) = rig.accept()?;
    assert_eq!(field(&hello, "data"), &json!(TICKET), "{hello}");
    assert_eq!(field(&hello, "offset"), &json!(0), "{hello}");
    hub.write_all(&data)?;
    hub.shutdown(Shutdown::Write)?;
    let verdict = verdict_of(&mut hub)?;
    assert_eq!(field(&verdict, "ok"), &json!(true), "{verdict}");
    assert!(settle(&rig.stream), "the transfer ended");

    // The staged bundle, where build-check.sh's Tulpa.app* glob looks.
    let staged = rig
        .support()
        .join("staging")
        .join("Tulpa.app-1a2b3c4d5e6f7890aabbccddeeff001122334455");
    assert!(staged.join("Contents/MacOS/tulpa").is_file());
    assert!(
        !rig.support()
            .join("inbox")
            .join("Tulpa-1a2b3c4d.tgz")
            .exists(),
        "the inbox copy was consumed by the staging"
    );

    // One activity line for the request, one for the delivery; no ticket.
    let log = rig.journal.text();
    assert!(
        log.contains("verb=files.put") && log.contains("staged"),
        "{log}"
    );
    assert!(!log.contains(TICKET), "{log}");
    let said: String = rig
        .outbox
        .drain()
        .iter()
        .map(|l| String::from_utf8_lossy(l).trim().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        said.contains("\"event\":\"files\"") && said.contains("Tulpa-1a2b3c4d.tgz"),
        "{said}"
    );
    Ok(())
}

#[test]
fn a_put_of_a_name_the_inbox_does_not_take_is_refused_before_dialing() -> Result<()> {
    let rig = rig()?;
    for bad in ["../out.tgz", "sub/x.tgz", ".hidden", "with space.tgz"] {
        let reply = rig.ask(&json!({
            "id": "g2", "verb": "files.put",
            "arg": {"ticket": TICKET, "path": bad, "size": 3, "sha256": sha_hex(b"abc")}
        }))?;
        assert_eq!(field(&reply, "ok"), &json!(false), "{bad}: {reply}");
        assert!(
            field(&reply, "err")
                .as_str()
                .unwrap_or_default()
                .contains("inbox"),
            "{bad}: {reply}"
        );
    }
    rig.hub.set_nonblocking(true)?;
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(rig.hub.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "a refused name never dials"
    );
    Ok(())
}

#[test]
fn files_state_lists_the_two_directories_and_other_verbs_fall_through() -> Result<()> {
    let rig = rig()?;
    fs::create_dir_all(rig.support().join("inbox"))?;
    fs::write(rig.support().join("inbox").join("notes.txt"), "hello")?;
    let reply = rig.ask(&json!({"id": "g3", "verb": "files.state"}))?;
    assert_eq!(field(&reply, "ok"), &json!(true), "{reply}");
    assert_eq!(
        field(field(&reply, "result"), "inbox"),
        &json!(["notes.txt"]),
        "{reply}"
    );
    assert_eq!(
        field(field(&reply, "result"), "staging"),
        &json!([]),
        "{reply}"
    );

    // Not this organ's verb: the inner handler's refusal, the id echoed.
    let reply = rig.ask(&json!({"id": "g4", "verb": "files.nonsense"}))?;
    assert_eq!(field(&reply, "err"), &json!("verb"), "{reply}");
    assert_eq!(field(&reply, "id"), &json!("g4"), "{reply}");
    Ok(())
}
