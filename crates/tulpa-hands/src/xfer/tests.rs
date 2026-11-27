//! The data stream against a fake hub on localhost: the Mac's side runs for
//! real (engine, policy, files, threads); the hub is a `TcpListener` each test
//! drives by hand.

use std::error::Error;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::*;
use crate::engine::{AskAnswer, Asker, Engine, shelf_get};

const TICKET: &str = "T1cket-0123456789abcdefABCDEF_xy";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Allow;

impl Asker for Allow {
    fn ask(&self, _what: &str, _timeout: Duration) -> AskAnswer {
        AskAnswer::Allow
    }
}

#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

impl Journal for Lines {
    fn record(&self, line: &str) {
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line.to_owned());
        }
    }
}

impl Lines {
    fn text(&self) -> String {
        self.0.lock().map(|l| l.join("\n")).unwrap_or_default()
    }
}

/// The writing half of a localhost connection: a half-close is `shutdown(Write)`.
struct TcpHalf(TcpStream);

impl Write for TcpHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl HalfWrite for TcpHalf {
    fn half_close(&mut self) -> io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
}

struct TcpClose(TcpStream);

impl Closer for TcpClose {
    fn close(&self) {
        let _closed: io::Result<()> = self.0.shutdown(Shutdown::Both);
    }
}

/// The Mac's data dial, to the fake hub: the hello as the tray says it, less
/// the device token.
struct LocalDial(SocketAddr);

impl DataDial for LocalDial {
    fn dial(&self, ticket: &Ticket, offset: Option<u64>) -> io::Result<DataConn> {
        let stream = TcpStream::connect(self.0)?;
        let hello = offset.map_or_else(
            || format!("{{\"hello\":\"mac\",\"data\":\"{}\"}}\n", ticket.reveal()),
            |n| {
                format!(
                    "{{\"hello\":\"mac\",\"data\":\"{}\",\"offset\":{n}}}\n",
                    ticket.reveal()
                )
            },
        );
        (&stream).write_all(hello.as_bytes())?;
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(DataConn::new(
            Box::new(reader),
            Box::new(TcpHalf(stream)),
            Arc::new(TcpClose(closer)),
        ))
    }
}

struct Rig {
    home: tempfile::TempDir,
    root: PathBuf,
    hub: TcpListener,
    engine: Engine,
    xfer: Arc<Xfer>,
    journal: Arc<Lines>,
    paused: Arc<tulpa_stop::Stop>,
}

fn rig_with(extra: &str, idle: Duration, full: bool) -> Result<Rig> {
    rig_on(extra, idle, full, false)
}

/// Where the shelved rig keeps its photos: under the deny list's `~/Library`.
const SHELF_DIR: &str = "Library/Application Support/tulpa/photos";

/// The shelf's names in these tests: a stand-in for the eyes' own shape.
fn photo_shaped(name: &str) -> bool {
    name.strip_prefix("photo-")
        .and_then(|rest| rest.strip_suffix(".jpg"))
        .is_some_and(|core| !core.is_empty() && !core.contains('/'))
}

fn rig_on(extra: &str, idle: Duration, full: bool, shelved: bool) -> Result<Rig> {
    let home = tempfile::tempdir()?;
    std::fs::create_dir_all(home.path().join("proj"))?;
    std::fs::write(home.path().join("outside.txt"), "outside\n")?;
    let policy = Policy::from_toml(&format!("roots = [\"~/proj\"]\n{extra}"), home.path(), None)?;
    let hub = TcpListener::bind("127.0.0.1:0")?;
    let dial: Arc<dyn DataDial> = Arc::new(LocalDial(hub.local_addr()?));
    let mut stream = Xfer::new(dial, Box::new(|| {})).with_idle(idle);
    if shelved {
        let shelf = home.path().join(SHELF_DIR);
        std::fs::create_dir_all(&shelf)?;
        stream = stream.with_shelf(Shelf::new(shelf, photo_shaped));
    }
    let xfer = Arc::new(stream);
    let journal = Arc::new(Lines::default());
    let journal_dyn: Arc<dyn Journal> = Arc::<Lines>::clone(&journal);
    let asker: Arc<dyn Asker> = Arc::new(Allow);
    let paused = Arc::new(tulpa_stop::Stop::new());
    let engine = Engine::new(
        policy,
        asker,
        journal_dyn,
        Arc::clone(&paused),
        Arc::default(),
    )
    .with_full(Arc::new(AtomicBool::new(full)))
    .with_xfer(Arc::clone(&xfer));
    let root = std::fs::canonicalize(home.path().join("proj"))?;
    Ok(Rig {
        home,
        root,
        hub,
        engine,
        xfer,
        journal,
        paused,
    })
}

/// An idle limit past every 60 s guard: a transfer that ends before the guard was ended by something else (Pause).
const PAST_GUARD: Duration = Duration::from_secs(3600);

fn rig() -> Result<Rig> {
    rig_with("", IDLE, false)
}

/// `value[key]` without the panicking `Index`.
fn field<'v>(value: &'v Value, key: &str) -> &'v Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn control(engine: &Engine, verb: &str, arg: &Value) -> Result<Value> {
    let line = json!({"id": "x1", "verb": verb, "arg": arg}).to_string();
    Ok(serde_json::from_slice(&engine.handle(line.as_bytes()))?)
}

fn sha_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn put_arg(path: &Path, data: &[u8]) -> Value {
    json!({"ticket": TICKET, "path": path, "size": data.len(), "sha256": sha_hex(data)})
}

fn get_arg(path: &Path, offset: u64) -> Value {
    json!({"ticket": TICKET, "path": path, "offset": offset})
}

/// Bytes that never repeat with a short period, so an offset error shows.
fn sample(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut n: u64 = 0;
    while out.len() < len {
        out.extend_from_slice(&Sha256::digest(n.to_le_bytes()));
        n = n.saturating_add(1);
    }
    out.truncate(len);
    out
}

fn part_of(path: &Path, data: &[u8]) -> PathBuf {
    let sha = sha_hex(data);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.part", sha.get(..12).unwrap_or_default()));
    path.with_file_name(name)
}

/// The hub's side: accepts the Mac's data connection and reads its hello.
fn accept(hub: &TcpListener) -> Result<(TcpStream, Value)> {
    hub.set_nonblocking(true)?;
    for _ in 0..400 {
        match hub.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                let mut line = Vec::new();
                let mut byte = [0_u8; 1];
                while (&stream).read(&mut byte)? == 1 && byte != *b"\n" {
                    line.extend_from_slice(&byte);
                }
                return Ok((stream, serde_json::from_slice(&line)?));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err("the Mac never dialled".into())
}

/// True when no data connection is waiting at the hub.
fn nobody_dialled(hub: &TcpListener) -> Result<bool> {
    hub.set_nonblocking(true)?;
    thread::sleep(Duration::from_millis(100));
    Ok(matches!(hub.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock))
}

/// Everything until the Mac closes (a reset counts as the end).
fn read_all(stream: &TcpStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match (&*stream).read_to_end(&mut out) {
        Ok(_) => Ok(out),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(out),
        Err(e) => Err(e.into()),
    }
}

fn verdict_of(bytes: &[u8]) -> Result<Value> {
    let line = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    Ok(serde_json::from_slice(line)?)
}

/// Waits until the transfer has let its slot go (its log line is written by then).
fn settle(xfer: &Xfer) -> bool {
    for _ in 0..400 {
        if !xfer.is_busy() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Waits until `path` holds `len` bytes (the Mac has taken what was sent).
fn holds(path: &Path, len: u64) -> bool {
    for _ in 0..400 {
        if std::fs::metadata(path).is_ok_and(|m| m.len() == len) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Sends `bytes` as the client would through the hub, half-closes, returns the verdict.
fn send_and_hear(stream: &TcpStream, bytes: &[u8]) -> Result<Value> {
    (&*stream).write_all(bytes)?;
    stream.shutdown(Shutdown::Write)?;
    verdict_of(&read_all(stream)?)
}

#[test]
fn put_lands_the_exact_bytes_and_says_so() -> Result<()> {
    let rig = rig()?;
    let data = sample(700_000);
    let path = rig.root.join("data.bin");
    let ack = control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    assert_eq!(field(&ack, "result"), &json!({"offset": 0}));
    let (stream, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "data"), &json!(TICKET));
    assert_eq!(field(&hello, "offset"), &json!(0));
    let verdict = send_and_hear(&stream, &data)?;
    assert_eq!(
        verdict,
        json!({"ok": true, "size": data.len(), "sha256": sha_hex(&data)})
    );
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&path)?, data);
    assert!(!part_of(&path, &data).exists(), "the .part was renamed");
    let mode = std::fs::metadata(&path)?.permissions().mode();
    assert_eq!(
        mode & 0o7111,
        0,
        "no exec or set-id bits on a new file: {mode:o}"
    );
    assert_eq!(mode & 0o600, 0o600, "0666 less the umask: {mode:o}");
    let log = rig.journal.text();
    assert!(
        log.contains("verb=xfer.put") && log.contains("result=ok offset=0"),
        "{log}"
    );
    assert!(
        log.contains("xfer: put") && log.contains(": ok, 700000 bytes"),
        "{log}"
    );
    assert!(!log.contains(TICKET), "the ticket is never logged: {log}");
    assert_eq!(
        rig.xfer.line().as_deref(),
        Some("transfer: put data.bin: ok")
    );
    Ok(())
}

/// Through `ssh -W` nothing gets back after the far end's EOF, so the Mac
/// answers at exactly `size` bytes; a hub that never half-closes still hears.
#[test]
fn put_answers_at_size_without_waiting_for_eof() -> Result<()> {
    let rig = rig()?;
    let data = sample(300_000);
    let path = rig.root.join("noeof.bin");
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    (&stream).write_all(&data)?;
    let started = Instant::now();
    let verdict = verdict_of(&read_all(&stream)?)?;
    assert!(started.elapsed() < Duration::from_secs(8), "no idle wait");
    assert_eq!(field(&verdict, "ok"), &json!(true), "{verdict}");
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&path)?, data);
    Ok(())
}

/// The same on resume: the `.part` already holds `offset` bytes, the hub sends
/// exactly the `size - offset` still missing and neither half-closes nor
/// closes; the verdict comes and the file is renamed into place.
#[test]
fn a_resumed_put_answers_at_size_without_waiting_for_eof() -> Result<()> {
    let rig = rig()?;
    let data = sample(300_000);
    let path = rig.root.join("resumed-noeof.bin");
    let part = part_of(&path, &data);
    std::fs::write(&part, data.get(..120_000).unwrap_or_default())?;
    let ack = control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    assert_eq!(field(&ack, "result"), &json!({"offset": 120_000}), "{ack}");
    let (stream, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "offset"), &json!(120_000));
    (&stream).write_all(data.get(120_000..).unwrap_or_default())?;
    let started = Instant::now();
    let verdict = verdict_of(&read_all(&stream)?)?;
    assert!(started.elapsed() < Duration::from_secs(8), "no idle wait");
    assert_eq!(
        verdict,
        json!({"ok": true, "size": data.len(), "sha256": sha_hex(&data)})
    );
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&path)?, data);
    assert!(!part.exists(), "the .part was renamed");
    assert!(
        rig.journal.text().contains("180000 from offset 120000"),
        "{}",
        rig.journal.text()
    );
    Ok(())
}

/// A ticket the hub refuses (the connection closes before a byte): no `.part`
/// is left behind; the `.part` is only made by the first byte.
#[test]
fn no_byte_no_part() -> Result<()> {
    let rig = rig()?;
    let data = sample(5000);
    let path = rig.root.join("never.bin");
    let ack = control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    assert_eq!(field(&ack, "result"), &json!({"offset": 0}), "{ack}");
    assert!(!part_of(&path, &data).exists(), "nothing made at the ack");
    let (stream, _) = accept(&rig.hub)?;
    stream.shutdown(Shutdown::Both)?;
    assert!(settle(&rig.xfer));
    assert!(!part_of(&path, &data).exists(), "no byte, no .part");
    assert!(rig.journal.text().contains("short: 0 of 5000"));
    Ok(())
}

/// A name with a newline cannot forge a log line; nor can an id.
#[test]
fn names_and_ids_cannot_forge_log_lines() -> Result<()> {
    let rig = rig()?;
    let data = sample(10);
    let path = rig.root.join("a\n2026-01-01T00:00:00Z note forged");
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    assert_eq!(field(&send_and_hear(&stream, &data)?, "ok"), &json!(true));
    assert!(settle(&rig.xfer));
    let reply = rig.engine.handle(
        json!({"id": "x\nforged", "verb": "xfer.get", "arg": get_arg(&path, 0)})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(
        reply,
        b"{\"err\":\"verb\"}\n".to_vec(),
        "a control character in the id"
    );
    let lines = rig.journal.0.lock().map(|l| l.clone()).unwrap_or_default();
    assert!(lines.iter().all(|l| !l.contains('\n')), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("\\n2026-01-01T00:00:00Z note forged")),
        "{lines:?}"
    );
    assert!(
        rig.xfer.line().is_some_and(|l| !l.contains('\n')),
        "the menu line stays one line"
    );
    Ok(())
}

/// The security review's checks 2 and 3: cut midway, rerun with a new ticket, only the rest
/// flows; the receiver's own `.part` decides the offset, whatever is claimed.
#[test]
fn put_resumes_from_its_own_part_whatever_offset_is_claimed() -> Result<()> {
    let rig = rig()?;
    let data = sample(600_000);
    let path = rig.root.join("big.bin");
    let part = part_of(&path, &data);
    let first = data.get(..300_000).unwrap_or_default();
    let second = data.get(300_000..400_000).unwrap_or_default();
    let rest = data.get(400_000..).unwrap_or_default();

    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    (&stream).write_all(first)?;
    stream.shutdown(Shutdown::Both)?;
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&part)?, first, "the .part keeps what came");
    assert!(!path.exists());
    assert!(rig.journal.text().contains("short: 300000 of 600000"));

    let mut claimed = put_arg(&path, &data);
    if let Some(arg) = claimed.as_object_mut() {
        arg.insert("offset".to_owned(), json!(0));
    }
    let ack = control(&rig.engine, "xfer.put", &claimed)?;
    assert_eq!(field(&ack, "result"), &json!({"offset": 300_000}), "{ack}");
    let (stream, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "offset"), &json!(300_000));
    (&stream).write_all(second)?;
    stream.shutdown(Shutdown::Both)?;
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::metadata(&part)?.len(), 400_000);

    if let Some(arg) = claimed.as_object_mut() {
        arg.insert("offset".to_owned(), json!(5_000_000));
    }
    let ack = control(&rig.engine, "xfer.put", &claimed)?;
    assert_eq!(field(&ack, "result"), &json!({"offset": 400_000}), "{ack}");
    let (stream, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "offset"), &json!(400_000));
    let verdict = send_and_hear(&stream, rest)?;
    assert_eq!(field(&verdict, "ok"), &json!(true), "{verdict}");
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&path)?, data);
    assert!(!part.exists());
    assert!(rig.journal.text().contains("200000 from offset 400000"));
    Ok(())
}

#[test]
fn get_sends_from_the_receivers_offset_and_reads_its_verdict() -> Result<()> {
    let rig = rig()?;
    let data = sample(500_000);
    let path = rig.root.join("out.bin");
    std::fs::write(&path, &data)?;

    let ack = control(&rig.engine, "xfer.get", &get_arg(&path, 0))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    assert_eq!(field(&ack, "size"), &json!(500_000));
    assert_eq!(field(&ack, "sha256"), &json!(sha_hex(&data)));
    assert_eq!(
        field(&ack, "result"),
        &json!({"size": 500_000, "sha256": sha_hex(&data)})
    );
    let (stream, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "data"), &json!(TICKET));
    assert_eq!(
        field(&hello, "offset"),
        &Value::Null,
        "offset is for put only"
    );
    let mut got = Vec::new();
    (&stream).read_to_end(&mut got)?;
    assert_eq!(got, data, "everything, then a half-close");
    (&stream).write_all(b"{\"ok\":true,\"size\":500000}\n")?;
    drop(stream);
    assert!(settle(&rig.xfer));
    assert!(rig.journal.text().contains(": ok, 500000 bytes"));

    control(&rig.engine, "xfer.get", &get_arg(&path, 123_457))?;
    let (stream, _) = accept(&rig.hub)?;
    let mut got = Vec::new();
    (&stream).read_to_end(&mut got)?;
    assert_eq!(got, data.get(123_457..).unwrap_or_default());
    (&stream).write_all(b"{\"ok\":false,\"err\":\"sha256 mismatch\"}\n")?;
    drop(stream);
    assert!(settle(&rig.xfer));
    let log = rig.journal.text();
    assert!(
        log.contains("failed (the receiver said: sha256 mismatch)"),
        "{log}"
    );
    assert!(!log.contains(TICKET), "{log}");

    let past = control(&rig.engine, "xfer.get", &get_arg(&path, 500_001))?;
    assert_eq!(field(&past, "ok"), &json!(false), "{past}");
    assert!(nobody_dialled(&rig.hub)?);
    Ok(())
}

/// The security review's check 5: exactly `size` bytes; one extra aborts, fewer fail short.
#[test]
fn put_takes_exactly_size_bytes() -> Result<()> {
    let rig = rig()?;
    let data = sample(1000);
    let path = rig.root.join("exact.bin");
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    let mut over = data.clone();
    over.push(0);
    let verdict = send_and_hear(&stream, &over)?;
    assert_eq!(
        verdict,
        json!({"ok": false, "err": "over size: more than 1000 bytes"})
    );
    assert!(settle(&rig.xfer));
    assert!(!path.exists());
    let part = part_of(&path, &data);
    assert!(
        !part.exists() || std::fs::metadata(&part)?.len() <= 1000,
        "nothing past size is ever written"
    );

    let other = sample(2000);
    let short = rig.root.join("short.bin");
    control(&rig.engine, "xfer.put", &put_arg(&short, &other))?;
    let (stream, _) = accept(&rig.hub)?;
    let verdict = send_and_hear(&stream, other.get(..1500).unwrap_or_default())?;
    assert_eq!(
        verdict,
        json!({"ok": false, "err": "short: 1500 of 2000 bytes"})
    );
    assert!(settle(&rig.xfer));
    assert!(!short.exists());
    assert_eq!(std::fs::metadata(part_of(&short, &other))?.len(), 1500);
    Ok(())
}

#[test]
fn put_with_a_wrong_sha256_drops_its_part() -> Result<()> {
    let rig = rig()?;
    let data = sample(5000);
    let path = rig.root.join("wrong.bin");
    let mut arg = put_arg(&path, &data);
    if let Some(arg) = arg.as_object_mut() {
        arg.insert("sha256".to_owned(), json!(sha_hex(b"something else")));
    }
    control(&rig.engine, "xfer.put", &arg)?;
    let (stream, _) = accept(&rig.hub)?;
    let verdict = send_and_hear(&stream, &data)?;
    assert_eq!(verdict, json!({"ok": false, "err": "sha256 mismatch"}));
    assert!(settle(&rig.xfer));
    assert!(!path.exists());
    let leftovers = std::fs::read_dir(&rig.root)?.count();
    assert_eq!(leftovers, 0, "the .part is dropped");
    assert!(rig.journal.text().contains(".part dropped"));
    Ok(())
}

/// A replaced file keeps its permission bits (never set-id); a directory at
/// the final name is refused before any byte.
#[test]
fn a_replaced_file_keeps_its_mode() -> Result<()> {
    let rig = rig()?;
    let path = rig.root.join("tool.sh");
    std::fs::write(&path, b"old")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4750))?;
    let data = sample(64);
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    assert_eq!(field(&send_and_hear(&stream, &data)?, "ok"), &json!(true));
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&path)?, data);
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o7777,
        0o750
    );
    std::fs::create_dir(rig.root.join("adir"))?;
    let ack = control(
        &rig.engine,
        "xfer.put",
        &put_arg(&rig.root.join("adir"), &data),
    )?;
    assert_eq!(
        field(&ack, "err"),
        &json!("path: not a regular file"),
        "{ack}"
    );
    assert!(nobody_dialled(&rig.hub)?);
    Ok(())
}

/// The security review's check 4: a symlink at the `.part` name (and a hard link) is refused
/// before any byte moves.
#[test]
fn a_link_at_the_part_name_is_refused() -> Result<()> {
    let rig = rig()?;
    let data = sample(100);
    let path = rig.root.join("x.bin");
    let outside = rig.home.path().join("outside.txt");
    std::os::unix::fs::symlink(&outside, part_of(&path, &data))?;
    let ack = control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    assert_eq!(field(&ack, "ok"), &json!(false), "{ack}");
    assert!(
        field(&ack, "err")
            .as_str()
            .unwrap_or_default()
            .starts_with("open .part"),
        "{ack}"
    );
    assert_eq!(std::fs::read_to_string(&outside)?, "outside\n");
    assert!(nobody_dialled(&rig.hub)?);
    assert!(!rig.xfer.is_busy(), "a refusal frees the slot");

    let linked = rig.root.join("y.bin");
    std::fs::write(rig.root.join("keep.txt"), "keep\n")?;
    std::fs::hard_link(rig.root.join("keep.txt"), part_of(&linked, &data))?;
    let ack = control(&rig.engine, "xfer.put", &put_arg(&linked, &data))?;
    assert_eq!(
        field(&ack, "err"),
        &json!("open .part: it has other links"),
        "{ack}"
    );
    assert_eq!(
        std::fs::read_to_string(rig.root.join("keep.txt"))?,
        "keep\n"
    );
    Ok(())
}

/// The security review's check 4: a parent inside the roots that links out of them.
#[test]
fn a_parent_linking_out_of_the_roots_is_refused_both_ways() -> Result<()> {
    let rig = rig()?;
    let away = rig.home.path().join("away");
    std::fs::create_dir(&away)?;
    std::fs::write(away.join("secret.txt"), "secret\n")?;
    std::os::unix::fs::symlink(&away, rig.root.join("link"))?;
    let data = sample(10);
    let put = control(
        &rig.engine,
        "xfer.put",
        &put_arg(&rig.root.join("link/x.bin"), &data),
    )?;
    assert_eq!(
        field(&put, "err"),
        &json!("path: outside the roots"),
        "{put}"
    );
    assert_eq!(
        std::fs::read_dir(&away)?.count(),
        1,
        "nothing created there"
    );
    let get = control(
        &rig.engine,
        "xfer.get",
        &get_arg(&rig.root.join("link/secret.txt"), 0),
    )?;
    assert_eq!(
        field(&get, "err"),
        &json!("path: outside the roots"),
        "{get}"
    );
    let outside = control(
        &rig.engine,
        "xfer.put",
        &put_arg(&rig.home.path().join("x.bin"), &data),
    )?;
    assert_eq!(field(&outside, "err"), &json!("path: outside the roots"));
    // A link that stays inside the roots is the same directory, and fine.
    std::fs::create_dir(rig.root.join("sub"))?;
    std::os::unix::fs::symlink(rig.root.join("sub"), rig.root.join("inner"))?;
    let inside = control(
        &rig.engine,
        "xfer.put",
        &put_arg(&rig.root.join("inner/x.bin"), &data),
    )?;
    assert_eq!(field(&inside, "ok"), &json!(true), "{inside}");
    let (stream, _) = accept(&rig.hub)?;
    assert_eq!(field(&send_and_hear(&stream, &data)?, "ok"), &json!(true));
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(rig.root.join("sub/x.bin"))?, data);
    assert!(nobody_dialled(&rig.hub)?);
    Ok(())
}

/// The security review's check 4: a final path on the deny list, either way.
#[test]
fn deny_listed_paths_are_refused() -> Result<()> {
    let rig = rig()?;
    std::fs::write(rig.root.join(".env"), "TOKEN=x\n")?;
    let data = sample(10);
    for name in [".env", "server.pem", "id_ed25519", "aws-credentials.json"] {
        let ack = control(
            &rig.engine,
            "xfer.put",
            &put_arg(&rig.root.join(name), &data),
        )?;
        assert_eq!(field(&ack, "err"), &json!("path: denied"), "{name}: {ack}");
    }
    let get = control(&rig.engine, "xfer.get", &get_arg(&rig.root.join(".env"), 0))?;
    assert_eq!(field(&get, "err"), &json!("path: denied"), "{get}");
    assert_eq!(std::fs::read_to_string(rig.root.join(".env"))?, "TOKEN=x\n");
    assert!(nobody_dialled(&rig.hub)?);
    Ok(())
}

/// The design's get integrity: a file changed while it is sent fails short,
/// its last bytes never leave.
#[test]
fn a_file_changed_during_get_fails() -> Result<()> {
    let rig = rig()?;
    // Larger than the loopback buffers: the Mac blocks on backpressure while
    // the hub has not read yet.
    let data = sample(32 * 1024 * 1024);
    let path = rig.root.join("moving.bin");
    std::fs::write(&path, &data)?;
    let ack = control(&rig.engine, "xfer.get", &get_arg(&path, 0))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    (&file).write_all(b"changed")?;
    let later = SystemTime::now()
        .checked_add(Duration::from_secs(60))
        .ok_or("time")?;
    file.set_modified(later)?;
    let (stream, _) = accept(&rig.hub)?;
    let got = read_all(&stream)?;
    assert!(got.len() < data.len(), "{} bytes arrived", got.len());
    assert!(settle(&rig.xfer));
    let log = rig.journal.text();
    assert!(log.contains("failed (changed while sending"), "{log}");
    Ok(())
}

/// 60 s (here 1500 ms) without a byte closes the connection and keeps the
/// `.part`; the slot is free for the next transfer.
#[test]
fn an_idle_put_is_aborted_and_keeps_its_part() -> Result<()> {
    let rig = rig_with("", Duration::from_millis(1500), false)?;
    let data = sample(10_000);
    let path = rig.root.join("idle.bin");
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    (&stream).write_all(data.get(..4000).unwrap_or_default())?;
    let started = Instant::now();
    let heard = read_all(&stream)?;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        verdict_of(&heard)?,
        json!({"ok": false, "err": "idle 1500 ms"})
    );
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::metadata(part_of(&path, &data))?.len(), 4000);
    assert!(!path.exists());
    assert!(
        rig.journal
            .text()
            .contains("failed (idle 1500 ms); .part kept at 4000/10000")
    );
    let next = control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    assert_eq!(field(&next, "result"), &json!({"offset": 4000}), "{next}");
    drop(accept(&rig.hub)?);
    Ok(())
}

/// One transfer at a time; Pause closes the running one, keeps its `.part`,
/// refuses new ones while held, and the slot is free again after.
#[test]
fn one_at_a_time_and_pause_aborts() -> Result<()> {
    let rig = rig_with("", PAST_GUARD, false)?;
    let data = sample(10_000);
    let path = rig.root.join("paused.bin");
    std::fs::write(rig.root.join("other.bin"), b"other")?;
    control(&rig.engine, "xfer.put", &put_arg(&path, &data))?;
    let (stream, _) = accept(&rig.hub)?;
    (&stream).write_all(data.get(..3000).unwrap_or_default())?;
    let busy = control(
        &rig.engine,
        "xfer.get",
        &get_arg(&rig.root.join("other.bin"), 0),
    )?;
    assert_eq!(
        field(&busy, "err"),
        &json!("busy: a transfer is running"),
        "{busy}"
    );
    assert!(holds(&part_of(&path, &data), 3000));
    rig.paused.set();
    // The wait for the next chunk wakes on Pause itself: the idle limit is past the guard, so only Pause closes the
    // connection before the read's 60 s timeout fails the test.
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    drop(read_all(&stream)?);
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::metadata(part_of(&path, &data))?.len(), 3000);
    assert!(rig.journal.text().contains("failed (paused)"));
    let held = control(
        &rig.engine,
        "xfer.get",
        &get_arg(&rig.root.join("other.bin"), 0),
    )?;
    assert_eq!(field(&held, "err"), &json!("paused"));
    rig.paused.clear();
    let again = control(
        &rig.engine,
        "xfer.get",
        &get_arg(&rig.root.join("other.bin"), 0),
    )?;
    assert_eq!(field(&again, "ok"), &json!(true), "not busy: {again}");
    let (stream, _) = accept(&rig.hub)?;
    assert_eq!(read_all(&stream)?, b"other");
    (&stream).write_all(b"{\"ok\":true}\n")?;
    drop(stream);
    assert!(settle(&rig.xfer));
    Ok(())
}

/// A get whose receiver stops reading parks the sending thread in a write; Pause still ends it at once (the
/// supervisor's wait wakes on it), and the connection is closed.
#[test]
fn pause_ends_a_stuck_get_at_once() -> Result<()> {
    let rig = rig_with("", PAST_GUARD, false)?;
    let data = sample(16 * 1024 * 1024);
    let path = rig.root.join("big.bin");
    std::fs::write(&path, &data)?;
    let ack = control(&rig.engine, "xfer.get", &get_arg(&path, 0))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    let (stream, _) = accept(&rig.hub)?;
    // Nothing read: the socket buffers fill and the sender blocks.
    thread::sleep(Duration::from_millis(300));
    assert!(rig.xfer.is_busy());
    rig.paused.set();
    // The idle limit is past this 60 s guard: only Pause ends the transfer before it. `is_busy` has no event, so
    // the test looks every ms.
    let started = Instant::now();
    while rig.xfer.is_busy() && started.elapsed() < Duration::from_secs(60) {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!rig.xfer.is_busy(), "the transfer outlived Pause");
    let got = read_all(&stream)?;
    assert!(got.len() < data.len(), "cut short: {}", got.len());
    assert!(
        rig.journal.text().contains("failed (paused)"),
        "{}",
        rig.journal.text()
    );
    Ok(())
}

/// A data connection that never comes up fails the transfer and frees the slot.
#[test]
fn a_failed_dial_frees_the_slot() -> Result<()> {
    let rig = rig()?;
    let gone = TcpListener::bind("127.0.0.1:0")?;
    let addr = gone.local_addr()?;
    drop(gone);
    let dial: Arc<dyn DataDial> = Arc::new(LocalDial(addr));
    let xfer = Arc::new(Xfer::new(dial, Box::new(|| {})));
    let engine = rig.engine.with_xfer(Arc::clone(&xfer));
    std::fs::write(rig.root.join("f.txt"), b"f")?;
    let ack = control(&engine, "xfer.get", &get_arg(&rig.root.join("f.txt"), 0))?;
    assert_eq!(field(&ack, "ok"), &json!(true));
    assert!(settle(&xfer));
    assert!(rig.journal.text().contains("failed (data dial:"));
    Ok(())
}

/// The `.part` mark on `path`, read without following a link.
fn mark_on(path: &Path) -> rustix::io::Result<Vec<u8>> {
    let mut value = vec![0_u8; 1024];
    let len = rustix::fs::lgetxattr(path, PART_MARK, value.as_mut_slice())?;
    value.truncate(len);
    Ok(value)
}

fn set_mark(path: &Path, value: &[u8]) -> rustix::io::Result<()> {
    rustix::fs::setxattr(path, PART_MARK, value, XattrFlags::empty())
}

/// Only a `.part` a put made and left untouched for a day is swept: one cut
/// off midway goes on the next transfer; a user's own file of the same shape,
/// a young `.part`, a copy under another name, another name and a link stay.
/// The finished file carries no mark.
#[test]
fn only_our_stale_parts_are_swept_on_a_new_transfer() -> Result<()> {
    let rig = rig()?;
    let day_ago = SystemTime::now()
        .checked_sub(Duration::from_secs(90_000))
        .ok_or("time")?;

    let cut = sample(5000);
    let cut_path = rig.root.join("cut.bin");
    let ours = part_of(&cut_path, &cut);
    control(&rig.engine, "xfer.put", &put_arg(&cut_path, &cut))?;
    let (stream, _) = accept(&rig.hub)?;
    (&stream).write_all(cut.get(..1000).unwrap_or_default())?;
    stream.shutdown(Shutdown::Both)?;
    assert!(settle(&rig.xfer));
    let ours_name = ours.file_name().ok_or("name")?.as_bytes().to_vec();
    let marks = match mark_on(&ours) {
        Ok(value) => {
            assert_eq!(value, ours_name, "the mark is the .part's own name");
            true
        }
        Err(rustix::io::Errno::NOTSUP) => {
            // Straight to stderr, past the test harness's capture.
            writeln!(
                io::stderr().lock(),
                "NOT CHECKED: the temp filesystem keeps no extended attributes, \
                 so no .part is marked and the sweep's removal is not exercised"
            )?;
            false
        }
        Err(e) => return Err(format!("the cut .part carries no mark: {e}").into()),
    };

    let own = rig.root.join("mine.bin.0123456789ab.part");
    let young = rig.root.join("young.bin.0123456789ab.part");
    let copy = rig.root.join("copy.bin.0123456789ab.part");
    let foreign = rig.root.join("notes.part");
    for file in [&own, &young, &copy, &foreign] {
        std::fs::write(file, b"x")?;
    }
    let outside = rig.home.path().join("outside.txt");
    let link = rig.root.join("link.bin.0123456789ab.part");
    std::os::unix::fs::symlink(&outside, &link)?;
    if marks {
        set_mark(&young, b"young.bin.0123456789ab.part")?;
        set_mark(&copy, &ours_name)?;
        // Through the link it would look like ours.
        set_mark(&outside, b"link.bin.0123456789ab.part")?;
    }
    for file in [&ours, &own, &copy, &foreign, &outside] {
        std::fs::OpenOptions::new()
            .write(true)
            .open(file)?
            .set_modified(day_ago)?;
    }

    let data = sample(10);
    let new = rig.root.join("new.bin");
    control(&rig.engine, "xfer.put", &put_arg(&new, &data))?;
    if marks {
        assert!(!ours.exists(), "a day-old .part of ours is swept");
    } else {
        assert!(ours.exists(), "an unmarked .part is never swept");
    }
    assert!(own.exists(), "a user's own file of that shape is kept");
    assert!(
        young.exists() && copy.exists() && foreign.exists(),
        "young, a copy under another name, or another name: kept"
    );
    assert!(
        link.symlink_metadata().is_ok() && outside.exists(),
        "a link is never followed"
    );
    let (stream, _) = accept(&rig.hub)?;
    assert_eq!(field(&send_and_hear(&stream, &data)?, "ok"), &json!(true));
    assert!(settle(&rig.xfer));
    assert_eq!(std::fs::read(&new)?, data);
    if marks {
        assert_eq!(mark_on(&new), Err(NO_ATTR), "the finished file is unmarked");
    }
    Ok(())
}

#[test]
fn postures_caps_and_full_mode() -> Result<()> {
    let off = rig_with("put = \"off\"\nread = \"off\"", IDLE, false)?;
    let data = sample(10);
    let put = control(
        &off.engine,
        "xfer.put",
        &put_arg(&off.root.join("x.bin"), &data),
    )?;
    assert_eq!(field(&put, "err"), &json!("off"));
    std::fs::write(off.root.join("r.txt"), b"r")?;
    let get = control(
        &off.engine,
        "xfer.get",
        &get_arg(&off.root.join("r.txt"), 0),
    )?;
    assert_eq!(field(&get, "err"), &json!("off"), "get follows read");

    let rig = rig()?;
    let mut huge = put_arg(&rig.root.join("huge.bin"), &data);
    if let Some(arg) = huge.as_object_mut() {
        arg.insert("size".to_owned(), json!(SIZE_MAX.saturating_add(1)));
    }
    let refused = control(&rig.engine, "xfer.put", &huge)?;
    assert!(
        field(&refused, "err")
            .as_str()
            .unwrap_or_default()
            .starts_with("size: over"),
        "{refused}"
    );
    let sparse = rig.root.join("sparse.bin");
    std::fs::File::create(&sparse)?.set_len(SIZE_MAX.saturating_add(1))?;
    let refused = control(&rig.engine, "xfer.get", &get_arg(&sparse, 0))?;
    assert!(
        field(&refused, "err")
            .as_str()
            .unwrap_or_default()
            .starts_with("size: over"),
        "{refused}"
    );

    // Full mode: the user's own permissions, missing parents made (as `write`).
    let full = rig_with("", IDLE, true)?;
    let target = full.home.path().join("made/for/it.bin");
    let ack = control(&full.engine, "xfer.put", &put_arg(&target, &data))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    let (stream, _) = accept(&full.hub)?;
    assert_eq!(field(&send_and_hear(&stream, &data)?, "ok"), &json!(true));
    assert!(settle(&full.xfer));
    assert_eq!(std::fs::read(&target)?, data);
    assert!(full.journal.text().contains("decider=full"));
    Ok(())
}

#[test]
fn requests_are_read_without_echoing_the_ticket() -> Result<()> {
    let object = json!({"ticket": TICKET, "path": "~/x", "size": 3, "sha256": "AB".repeat(32)});
    let request = XferRequest::parse(Direction::Put, Some(&object))?;
    assert_eq!(request.sha256, "ab".repeat(32), "hex is taken in lowercase");
    assert_eq!(request.logged(), "~/x (3 bytes)");
    let text = json!(object.to_string());
    assert!(
        XferRequest::parse(Direction::Put, Some(&text)).is_ok(),
        "a JSON string works too"
    );
    let bad_ticket = json!({"ticket": "short\"quote", "path": "~/x"});
    let err = XferRequest::parse(Direction::Get, Some(&bad_ticket))
        .err()
        .unwrap_or_default();
    assert_eq!(err, "arg: ticket missing or malformed");
    let no_sha = json!({"ticket": TICKET, "path": "~/x", "size": 3});
    assert!(XferRequest::parse(Direction::Put, Some(&no_sha)).is_err());
    let no_offset = json!({"ticket": TICKET, "path": "~/x"});
    let err = XferRequest::parse(Direction::Get, Some(&no_offset))
        .err()
        .unwrap_or_default();
    assert_eq!(err, "arg: get needs offset (the client's .part size)");
    let not_object = json!(17);
    let err = XferRequest::parse(Direction::Get, Some(&not_object))
        .err()
        .unwrap_or_default();
    assert!(!err.contains("17"), "{err}");
    let ticket = Ticket::parse(TICKET).ok_or("ticket")?;
    assert_eq!(format!("{ticket:?}"), "Ticket(32 bytes)");
    assert!(Ticket::parse("has space 0123456789").is_none());
    Ok(())
}

#[test]
fn helpers_name_and_measure() {
    assert!(is_part_name(b"a.bin.0123456789ab.part"));
    assert!(!is_part_name(b".0123456789ab.part"), "a name is needed");
    assert!(!is_part_name(b"a.bin.0123456789AB.part"), "lowercase only");
    assert!(!is_part_name(b"a.bin.0123456789a.part"));
    assert!(!is_part_name(b"notes.part"));
    assert!(space_ok(SPACE_MARGIN.saturating_add(10), 10));
    assert!(!space_ok(SPACE_MARGIN.saturating_add(9), 10));
    assert!(!space_ok(u64::MAX, u64::MAX), "no overflow into yes");
    assert_eq!(megabytes(12_345_678), "12.3");
    assert_eq!(hex(&[0, 15, 255]), "000fff");
}

/// Sends a get's bytes to the hub's side, which answers `verdict`; returns the bytes.
fn take_get(rig: &Rig, verdict: &str) -> Result<Vec<u8>> {
    let (stream, _) = accept(&rig.hub)?;
    let mut got = Vec::new();
    (&stream).read_to_end(&mut got)?;
    (&stream).write_all(verdict.as_bytes())?;
    drop(stream);
    assert!(settle(&rig.xfer));
    Ok(got)
}

/// The eyes' shelf: a photo leaves past the deny list (its directory is under
/// `~/Library`), and once the receiver's verdict names its size and sha256 it
/// is removed from the Mac. A verdict that does not name it, or a failed one,
/// keeps it.
#[test]
fn a_shelf_photo_leaves_past_the_deny_list_once() -> Result<()> {
    let rig = rig_on("", IDLE, false, true)?;
    let shelf = rig.home.path().join(SHELF_DIR);
    let data = sample(70_000);
    let photo = shelf.join("photo-20260115T093000Z.jpg");
    std::fs::write(&photo, &data)?;

    let ack = control(&rig.engine, "xfer.get", &get_arg(&photo, 0))?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    assert_eq!(field(&ack, "sha256"), &json!(sha_hex(&data)));
    let named = json!({"ok": true, "size": 70_000, "sha256": sha_hex(&data)});
    let got = take_get(&rig, &format!("{named}\n"))?;
    assert_eq!(got, data);
    assert!(!photo.exists(), "handed over: gone from the Mac");
    assert!(
        rig.journal.text().contains("; removed from the shelf"),
        "{}",
        rig.journal.text()
    );

    let second = shelf.join("photo-20260115T093001Z.jpg");
    std::fs::write(&second, &data)?;
    control(&rig.engine, "xfer.get", &get_arg(&second, 0))?;
    take_get(&rig, "{\"ok\":true,\"size\":70000}\n")?;
    assert!(second.exists(), "a verdict without the sha256 keeps it");
    let wrong = json!({"ok": true, "size": 70_000, "sha256": sha_hex(b"other")});
    control(&rig.engine, "xfer.get", &get_arg(&second, 0))?;
    take_get(&rig, &format!("{wrong}\n"))?;
    assert!(second.exists(), "a verdict naming another sha256 keeps it");
    control(&rig.engine, "xfer.get", &get_arg(&second, 0))?;
    take_get(&rig, "{\"ok\":false,\"err\":\"short\"}\n")?;
    assert!(second.exists(), "a failed verdict keeps it");
    control(&rig.engine, "xfer.get", &get_arg(&second, 0))?;
    take_get(&rig, "{\"ok\":true,\"size\":\"70000\"}\n")?;
    assert!(second.exists(), "an odd size names nothing");
    assert!(
        rig.journal.text().contains(": ok, 70000 bytes"),
        "and the get is still ok: {}",
        rig.journal.text()
    );
    assert!(
        rig.journal
            .text()
            .contains("kept on the shelf (the verdict did not name")
    );

    // Without a shelf the same photo is on the deny list like the rest of ~/Library.
    let bare = rig_on("", IDLE, false, false)?;
    let other = bare
        .home
        .path()
        .join(SHELF_DIR)
        .join("photo-20260115T093000Z.jpg");
    std::fs::create_dir_all(bare.home.path().join(SHELF_DIR))?;
    std::fs::write(&other, &data)?;
    let refused = control(&bare.engine, "xfer.get", &get_arg(&other, 0))?;
    assert_eq!(field(&refused, "err"), &json!("path: denied"), "{refused}");
    assert!(nobody_dialled(&bare.hub)?);
    Ok(())
}

/// Only a photo-shaped regular file with one name, directly on the shelf and
/// reached without a symlink, leaves it; the hands' read posture still decides,
/// and full mode hands a shelf photo over once too.
#[test]
fn nothing_else_leaves_the_shelf() -> Result<()> {
    let rig = rig_on("", IDLE, false, true)?;
    let shelf = rig.home.path().join(SHELF_DIR);
    let data = sample(1_000);
    std::fs::write(shelf.join("notes.txt"), &data)?;
    std::fs::write(rig.root.join("code.rs"), &data)?;
    std::os::unix::fs::symlink(rig.root.join("code.rs"), shelf.join("photo-link.jpg"))?;
    std::fs::hard_link(rig.root.join("code.rs"), shelf.join("photo-hard.jpg"))?;
    std::fs::create_dir(shelf.join("photo-dir.jpg"))?;
    std::fs::create_dir(shelf.join("sub"))?;
    std::fs::write(shelf.join("sub").join("photo-deep.jpg"), &data)?;
    for (name, err) in [
        ("notes.txt", "path: denied"),
        ("photo-hard.jpg", "path: denied"),
        ("photo-dir.jpg", "path: not a regular file"),
        ("sub/photo-deep.jpg", "path: denied"),
    ] {
        let ack = control(&rig.engine, "xfer.get", &get_arg(&shelf.join(name), 0))?;
        assert_eq!(field(&ack, "err"), &json!(err), "{name}: {ack}");
    }
    let linked = control(
        &rig.engine,
        "xfer.get",
        &get_arg(&shelf.join("photo-link.jpg"), 0),
    )?;
    let err = field(&linked, "err").as_str().unwrap_or_default();
    assert!(
        err.starts_with("open: "),
        "a symlink is never followed: {linked}"
    );
    assert!(nobody_dialled(&rig.hub)?);

    // A shelf whose directory is itself a link sends nothing.
    let moved = rig_on("", IDLE, false, true)?;
    let moved_shelf = moved.home.path().join(SHELF_DIR);
    std::fs::remove_dir(&moved_shelf)?;
    std::os::unix::fs::symlink(&moved.root, &moved_shelf)?;
    std::fs::write(moved.root.join("photo-20260115T093000Z.jpg"), &data)?;
    let through = control(
        &moved.engine,
        "xfer.get",
        &get_arg(&moved_shelf.join("photo-20260115T093000Z.jpg"), 0),
    )?;
    assert_eq!(field(&through, "ok"), &json!(false), "{through}");

    // The read posture still applies to the shelf.
    let closed = rig_on("read = \"off\"\n", IDLE, false, true)?;
    let shut = closed
        .home
        .path()
        .join(SHELF_DIR)
        .join("photo-20260115T093000Z.jpg");
    std::fs::write(&shut, &data)?;
    let off = control(&closed.engine, "xfer.get", &get_arg(&shut, 0))?;
    assert_eq!(field(&off, "err"), &json!("off"), "{off}");

    // Full mode reads any path, but a shelf photo still leaves once.
    let full = rig_on("", IDLE, true, true)?;
    let once = full
        .home
        .path()
        .join(SHELF_DIR)
        .join("photo-20260115T093000Z.jpg");
    std::fs::write(&once, &data)?;
    control(&full.engine, "xfer.get", &get_arg(&once, 0))?;
    let named = json!({"ok": true, "size": 1_000, "sha256": sha_hex(&data)});
    take_get(&full, &format!("{named}\n"))?;
    assert!(!once.exists(), "handed over in full mode too");
    Ok(())
}

/// The eyes' own fetch ([`shelf_get`]): the one named photo leaves with no
/// engine at all, whatever the hands' posture (here `read = "off"`, which the
/// engine itself refuses), and once. Any other line is not its to answer;
/// Pause refuses; the shelf's own rules still hold; without a shelf it
/// answers nothing.
#[test]
fn shelf_get_serves_the_named_photo_without_the_hands() -> Result<()> {
    let rig = rig_on("read = \"off\"\n", IDLE, false, true)?;
    let shelf = rig.home.path().join(SHELF_DIR);
    let data = sample(5_000);
    let photo = shelf.join("photo-20260115T093000Z.jpg");
    std::fs::write(&photo, &data)?;
    let journal: Arc<dyn Journal> = Arc::<Lines>::clone(&rig.journal);
    let line = |id: &str, verb: &str, path: &Path| {
        json!({"id": id, "verb": verb, "arg": get_arg(path, 0)})
            .to_string()
            .into_bytes()
    };

    let hands = control(&rig.engine, "xfer.get", &get_arg(&photo, 0))?;
    assert_eq!(field(&hands, "err"), &json!("off"), "the hands: {hands}");

    for other in [
        line("s0", "xfer.put", &photo),
        line("s0", "xfer.get", &shelf.join("photo-20260115T093001Z.jpg")),
        line("s\n0", "xfer.get", &photo),
        b"not json".to_vec(),
    ] {
        let answered = shelf_get(&rig.xfer, &other, &photo, &rig.paused, &journal);
        assert!(answered.is_none(), "{}", String::from_utf8_lossy(&other));
    }

    rig.paused.set();
    let paused = shelf_get(
        &rig.xfer,
        &line("s1", "xfer.get", &photo),
        &photo,
        &rig.paused,
        &journal,
    )
    .ok_or("paused: not answered")?;
    let paused: Value = serde_json::from_slice(&paused)?;
    assert_eq!(field(&paused, "err"), &json!("paused"), "{paused}");
    assert!(nobody_dialled(&rig.hub)?);
    rig.paused.clear();

    let ack = shelf_get(
        &rig.xfer,
        &line("s2", "xfer.get", &photo),
        &photo,
        &rig.paused,
        &journal,
    )
    .ok_or("not answered")?;
    let ack: Value = serde_json::from_slice(&ack)?;
    assert_eq!(field(&ack, "ok"), &json!(true), "{ack}");
    assert_eq!(field(&ack, "size"), &json!(5_000));
    assert_eq!(field(&ack, "sha256"), &json!(sha_hex(&data)));
    let named = json!({"ok": true, "size": 5_000, "sha256": sha_hex(&data)});
    assert_eq!(take_get(&rig, &format!("{named}\n"))?, data);
    assert!(!photo.exists(), "handed over once: gone from the Mac");
    let log = rig.journal.text();
    assert!(
        log.contains("shelf: id=s2 verb=xfer.get")
            && log.contains("decider=shelf result=ok size=5000"),
        "{log}"
    );
    assert!(log.contains("; removed from the shelf"), "{log}");
    assert!(!log.contains(TICKET), "never the ticket: {log}");

    let linked = shelf.join("photo-20260115T093002Z.jpg");
    std::fs::write(&linked, &data)?;
    std::fs::hard_link(&linked, rig.root.join("copy.jpg"))?;
    let refused = shelf_get(
        &rig.xfer,
        &line("s3", "xfer.get", &linked),
        &linked,
        &rig.paused,
        &journal,
    )
    .ok_or("a second name: not answered")?;
    let refused: Value = serde_json::from_slice(&refused)?;
    assert_eq!(field(&refused, "err"), &json!("path: denied"), "{refused}");
    assert!(nobody_dialled(&rig.hub)?);

    let bare = rig_on("", IDLE, false, false)?;
    let bare_journal: Arc<dyn Journal> = Arc::<Lines>::clone(&bare.journal);
    let unshelved = shelf_get(
        &bare.xfer,
        &line("s4", "xfer.get", &photo),
        &photo,
        &bare.paused,
        &bare_journal,
    );
    assert!(unshelved.is_none(), "no shelf: the hands answer");
    Ok(())
}

// The inbox puts of the files organ (#0395): the same wire and the same
// `.part` discipline as a policy put, against a directory the caller owns.

/// What a landed callback caught: (path, size, sha256) triples.
type LandedLog = Arc<Mutex<Vec<(PathBuf, u64, String)>>>;

struct InboxRig {
    home: tempfile::TempDir,
    xfer: Arc<Xfer>,
    journal: Arc<Lines>,
    hub: TcpListener,
}

fn inbox_rig(idle: Duration) -> Result<InboxRig> {
    let home = tempfile::tempdir()?;
    std::fs::create_dir_all(home.path().join("inbox"))?;
    let hub = TcpListener::bind("127.0.0.1:0")?;
    let dial: Arc<dyn DataDial> = Arc::new(LocalDial(hub.local_addr()?));
    let xfer = Arc::new(Xfer::new(dial, Box::new(|| {})).with_idle(idle));
    let journal = Arc::new(Lines::default());
    Ok(InboxRig {
        home,
        xfer,
        journal,
        hub,
    })
}

impl InboxRig {
    /// The inbox directory under the rig's home.
    fn inbox(&self) -> PathBuf {
        self.home.path().join("inbox")
    }

    /// The inbox directory opened as the files organ opens it.
    fn dir(&self) -> Result<OwnedFd> {
        use rustix::fs::{Mode, OFlags};
        Ok(rustix::fs::open(
            self.inbox(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    /// What the tray lends the files organ: no pause to answer.
    fn lent(&self) -> Lent {
        Lent::always_on(Arc::<Lines>::clone(&self.journal), None)
    }

    fn request(path: &str, data: &[u8]) -> Result<XferRequest> {
        let arg = json!({
            "ticket": TICKET,
            "path": path,
            "size": data.len(),
            "sha256": sha_hex(data),
        });
        Ok(XferRequest::parse(Direction::Put, Some(&arg))?)
    }

    fn catcher() -> (LandedLog, Landed) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            seen,
            Box::new(move |path: &Path, size: u64, sha256: &str| {
                if let Ok(mut seen) = sink.lock() {
                    seen.push((path.to_owned(), size, sha256.to_owned()));
                }
            }),
        )
    }
}

#[test]
fn an_inbox_put_lands_resumes_and_tells() -> Result<()> {
    let rig = inbox_rig(IDLE)?;
    let data = sample(2_600_000);
    let (cut_here, rest) = data.split_at(1_000_000);
    let request = InboxRig::request("Tulpa-1a2b3c4d.tgz", &data)?;
    let (seen, landed) = InboxRig::catcher();

    // The first leg is cut short: the hub sends half and hangs up.
    let ack = start_inbox(
        &rig.xfer,
        rig.dir()?,
        rig.inbox(),
        &request,
        Some(landed),
        rig.lent(),
    )?;
    assert_eq!(ack, Ack::Put { offset: 0 });
    let (mut hub, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "offset"), &json!(0), "{hello}");
    hub.write_all(cut_here)?;
    let half = hub.try_clone()?;
    half.shutdown(Shutdown::Write)?;
    let verdict: Value = serde_json::from_slice(&read_all(&hub)?)?;
    assert_eq!(field(&verdict, "ok"), &json!(false), "{verdict}");
    assert!(settle(&rig.xfer));
    let part = part_of(&rig.inbox().join("Tulpa-1a2b3c4d.tgz"), &data);
    assert_eq!(std::fs::metadata(&part)?.len(), 1_000_000);
    drop(half);

    // The second resumes from the Mac's own half.
    let (seen_again, landed_again) = InboxRig::catcher();
    let ack = start_inbox(
        &rig.xfer,
        rig.dir()?,
        rig.inbox(),
        &request,
        Some(landed_again),
        rig.lent(),
    )?;
    assert_eq!(ack, Ack::Put { offset: 1_000_000 });
    let (mut hub, hello) = accept(&rig.hub)?;
    assert_eq!(field(&hello, "offset"), &json!(1_000_000), "{hello}");
    hub.write_all(rest)?;
    let verdict: Value = serde_json::from_slice(&read_all(&hub)?)?;
    assert_eq!(field(&verdict, "ok"), &json!(true), "{verdict}");
    assert_eq!(field(&verdict, "sha256"), &json!(sha_hex(&data)));
    assert!(settle(&rig.xfer));

    let landed_file = rig.inbox().join("Tulpa-1a2b3c4d.tgz");
    assert_eq!(std::fs::read(&landed_file)?, data);
    assert!(!part.exists(), "the .part became the file");
    assert!(
        seen.lock().map_or(true, |s| s.is_empty()),
        "no landing on a cut leg"
    );
    let caught = seen_again.lock().map_or_else(|_| Vec::new(), |s| s.clone());
    assert_eq!(
        caught,
        vec![(landed_file, data.len() as u64, sha_hex(&data))],
        "{caught:?}"
    );
    let log = rig.journal.text();
    assert!(log.contains("files") || log.contains("put"), "{log}");
    assert!(!log.contains(TICKET), "never the ticket: {log}");
    Ok(())
}

#[test]
fn an_inbox_put_refuses_a_name_that_escapes_before_dialing() -> Result<()> {
    let rig = inbox_rig(IDLE)?;
    let data = sample(10);
    for escape in ["../outside.tgz", "sub/dir/name.tgz", "..", "."] {
        let request = InboxRig::request(escape, &data)?;
        let err = start_inbox(
            &rig.xfer,
            rig.dir()?,
            rig.inbox(),
            &request,
            None,
            rig.lent(),
        )
        .err()
        .unwrap_or_default();
        assert!(err.contains("one name"), "{escape}: {err}");
    }
    assert!(nobody_dialled(&rig.hub)?, "a refused name never dials");
    Ok(())
}

#[test]
fn an_inbox_put_of_wrong_bytes_drops_its_part_and_never_lands() -> Result<()> {
    let rig = inbox_rig(IDLE)?;
    let data = sample(400_000);
    let request = InboxRig::request("Tulpa-wrong.tgz", &data)?;
    let (seen, landed) = InboxRig::catcher();
    let ack = start_inbox(
        &rig.xfer,
        rig.dir()?,
        rig.inbox(),
        &request,
        Some(landed),
        rig.lent(),
    )?;
    assert_eq!(ack, Ack::Put { offset: 0 });
    let (mut hub, _hello) = accept(&rig.hub)?;
    let mut wrong = data.clone();
    if let Some(first) = wrong.first_mut() {
        *first ^= 0xff;
    }
    hub.write_all(&wrong)?;
    let half = hub.try_clone()?;
    half.shutdown(Shutdown::Write)?;
    let verdict: Value = serde_json::from_slice(&read_all(&hub)?)?;
    assert_eq!(field(&verdict, "ok"), &json!(false), "{verdict}");
    assert!(
        field(&verdict, "err")
            .as_str()
            .unwrap_or_default()
            .contains("sha256"),
        "{verdict}"
    );
    assert!(settle(&rig.xfer));
    drop(half);
    let part = part_of(&rig.inbox().join("Tulpa-wrong.tgz"), &data);
    assert!(
        !part.exists(),
        "bytes that hash wrong are no prefix to keep"
    );
    assert!(!rig.inbox().join("Tulpa-wrong.tgz").exists());
    assert!(seen.lock().map_or(true, |s| s.is_empty()), "nothing landed");
    Ok(())
}
