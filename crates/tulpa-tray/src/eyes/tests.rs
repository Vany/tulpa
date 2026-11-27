//! `eyes.photo` with a stand-in helper: a `/bin/sh` script plays the camera,
//! so no camera is ever opened and nothing is heard.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::thread;

use tulpa_eyes::PHOTO_MAX_AGE;
use tulpa_hands::{DataConn, DataDial, Ticket};

use super::*;
use crate::link::refuse_line;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// What the stand-in camera prints: a few bytes that start and end like a JPEG.
const STILL: &str = r"printf '\377\330\377\340fake still\377\331'";
/// Their size and sha256, computed apart from this code (Python's hashlib and sha256sum).
const STILL_SIZE: u64 = 16;
const STILL_SHA256: &str = "aeb010278047ae0ef39987cd1dcff3dfc33b42033b99053f984641a1a6b185fb";

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
    fn text(&self) -> String {
        self.0.lock().map(|l| l.join("\n")).unwrap_or_default()
    }
}

/// Answers every question the same way, and counts them.
struct Clicks {
    answer: AskAnswer,
    asked: AtomicUsize,
    question: Mutex<String>,
}

impl Asker for Clicks {
    fn ask(&self, what: &str, _timeout: Duration) -> AskAnswer {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut question) = self.question.lock() {
            what.clone_into(&mut question);
        }
        self.answer
    }
}

/// A directory of its own under the system's temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        static MADE: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tulpa-eyes-test-{}-{}-{}",
            std::process::id(),
            MADE.fetch_add(1, Ordering::SeqCst),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _gone: std::io::Result<()> = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    eyes: Eyes,
    dir: Scratch,
    notes: Arc<Notes>,
    clicks: Arc<Clicks>,
    paused: Arc<Stop>,
}

/// The eyes in `[eyes] mode = "<mode>"`, with `script` as the camera; the
/// script first leaves a mark, so a test can tell whether it ran.
fn rig(mode: &str, answer: AskAnswer, script: &str) -> Result<Rig> {
    let dir = Scratch::new()?;
    let config = TrayAppConfig::from_toml(&format!("[eyes]\nmode = \"{mode}\"\n"))?;
    let notes = Arc::new(Notes::default());
    let clicks = Arc::new(Clicks {
        answer,
        asked: AtomicUsize::new(0),
        question: Mutex::new(String::new()),
    });
    let paused = Arc::new(Stop::new());
    let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
    let asker: Arc<dyn Asker> = Arc::<Clicks>::clone(&clicks);
    let marked = format!("touch '{}'; {script}", dir.path().join("ran").display());
    let eyes = Eyes::build(
        &config,
        Arc::clone(&paused),
        Arc::default(),
        asker,
        journal,
        Box::new(|| {}),
    )
    .with_helper(PathBuf::from("/bin/sh"), vec!["-c".to_owned(), marked])
    .with_photo_dir(dir.path().join("photos"))
    .with_limits(Duration::from_secs(10), Duration::from_secs(1));
    Ok(Rig {
        eyes,
        dir,
        notes,
        clicks,
        paused,
    })
}

impl Rig {
    fn send(&self, line: &Value) -> Result<Value> {
        Ok(serde_json::from_slice(
            &self.eyes.handle(line.to_string().as_bytes()),
        )?)
    }

    fn photo(&self) -> Result<Value> {
        self.send(&json!({"id": "e1", "verb": PHOTO_VERB}))
    }

    /// Whether the camera script ran at all.
    fn ran(&self) -> bool {
        self.dir.path().join("ran").exists()
    }

    fn photos(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("photos")).map_or(0, Iterator::count)
    }

    fn asked(&self) -> usize {
        self.clicks.asked.load(Ordering::SeqCst)
    }
}

/// `value` at a JSON pointer, without the panicking `Index`.
fn at<'v>(value: &'v Value, pointer: &str) -> &'v Value {
    value.pointer(pointer).unwrap_or(&Value::Null)
}

fn mode_of(path: &Path) -> Result<u32> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o7777)
}

#[test]
fn a_photo_comes_back_with_its_private_path_size_and_sha256() -> Result<()> {
    let rig = rig("on", AskAnswer::Deny, STILL)?;
    let reply = rig.photo()?;
    assert_eq!(at(&reply, "/ok"), &json!(true), "{reply}");
    assert_eq!(at(&reply, "/id"), &json!("e1"));
    let path = PathBuf::from(at(&reply, "/result/path").as_str().ok_or("no path")?);
    let photos = rig.dir.path().join("photos");
    assert_eq!(path.parent(), Some(photos.as_path()));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    assert!(is_photo_name(name), "{name}");
    assert_eq!(at(&reply, "/result/size"), &json!(STILL_SIZE));
    assert_eq!(at(&reply, "/result/sha256"), &json!(STILL_SHA256));
    assert_eq!(std::fs::read(&path)?.len(), 16);
    assert_eq!(mode_of(&path)?, 0o600, "the photo is the owner's alone");
    assert_eq!(mode_of(&photos)?, 0o700, "so is its directory");
    assert_eq!(rig.asked(), 0, "mode on asks nobody");
    let log = rig.notes.text();
    assert!(
        log.contains("eyes: id=e1 verb=\"eyes.photo\" decider=policy result=ok photo-")
            && log.contains("(16 bytes)"),
        "{log}"
    );
    assert!(!log.contains("fake still"), "the log never holds the photo");

    let empty = rig.send(&json!({"id": "e2", "verb": PHOTO_VERB, "arg": ""}))?;
    assert_eq!(
        at(&empty, "/ok"),
        &json!(true),
        "an empty argument is none: {empty}"
    );
    assert_eq!(rig.photos(), 2, "one photo per request");
    Ok(())
}

#[test]
fn paused_eyes_refuse_without_starting_the_camera() -> Result<()> {
    let rig = rig("on", AskAnswer::Allow, STILL)?;
    rig.paused.set();
    let reply = rig.photo()?;
    assert_eq!(at(&reply, "/err"), &json!("paused"), "{reply}");
    assert!(!rig.ran(), "the camera was never started");
    assert_eq!(rig.photos(), 0);
    assert!(rig.notes.text().contains("decider=pause"));
    Ok(())
}

#[test]
fn off_other_verbs_and_arguments_are_refused_before_the_camera() -> Result<()> {
    let off = rig("off", AskAnswer::Allow, STILL)?;
    assert_eq!(at(&off.photo()?, "/err"), &json!("eyes: off in config"));
    assert!(!off.ran());

    let rig = rig("on", AskAnswer::Allow, STILL)?;
    let other = rig.send(&json!({"id": "e1", "verb": "eyes.screen"}))?;
    assert_eq!(at(&other, "/err"), &json!("verb"), "{other}");
    for arg in [json!("front"), json!({"camera": 1}), json!(2)] {
        let reply = rig.send(&json!({"id": "e1", "verb": PHOTO_VERB, "arg": arg}))?;
        assert_eq!(at(&reply, "/err"), &json!("arg"), "{reply}");
    }
    assert!(!rig.ran());
    let forged = rig
        .eyes
        .handle(b"{\"id\":\"e1\\nok\",\"verb\":\"eyes.photo\"}");
    assert!(String::from_utf8_lossy(&forged).contains("\"err\":\"verb\""));
    assert!(rig.notes.text().contains("eyes: bad id refused"));
    assert!(!rig.ran());
    Ok(())
}

#[test]
fn ask_takes_a_photo_only_after_a_click() -> Result<()> {
    let denied = rig("ask", AskAnswer::Deny, STILL)?;
    assert_eq!(at(&denied.photo()?, "/err"), &json!("denied by the owner"));
    assert_eq!(denied.asked(), 1);
    assert!(!denied.ran());
    let question = denied
        .clicks
        .question
        .lock()
        .map(|q| q.clone())
        .unwrap_or_default();
    assert!(
        question.contains("photo") && question.contains("camera"),
        "{question}"
    );

    let late = rig("ask", AskAnswer::Expired, STILL)?;
    assert_eq!(at(&late.photo()?, "/err"), &json!("no click in time"));
    assert!(!late.ran());

    let allowed = rig("ask", AskAnswer::Allow, STILL)?;
    let reply = allowed.photo()?;
    assert_eq!(at(&reply, "/ok"), &json!(true), "{reply}");
    assert_eq!(allowed.asked(), 1);
    assert!(allowed.notes.text().contains("decider=click"));
    Ok(())
}

/// Clicks Allow after the link attempt the request came on dies and the next one opens (the link redials while
/// the question waits).
struct RedialWhileAsked(Arc<Attempts>);

impl Asker for RedialWhileAsked {
    fn ask(&self, _what: &str, _timeout: Duration) -> AskAnswer {
        self.0.current().set();
        let _next: Arc<Stop> = self.0.begin();
        AskAnswer::Allow
    }
}

/// A capture asked for on a link attempt that dies is killed as the link's, though the link redialled before the
/// camera started: the request keeps the attempt it came on.
#[test]
fn a_capture_dies_with_the_link_attempt_it_was_asked_on() -> Result<()> {
    let dir = Scratch::new()?;
    let config = TrayAppConfig::from_toml("[eyes]\nmode = \"ask\"\n")?;
    let attempts = Arc::new(Attempts::default());
    let _first: Arc<Stop> = attempts.begin();
    let asker: Arc<dyn Asker> = Arc::new(RedialWhileAsked(Arc::clone(&attempts)));
    let journal: Arc<dyn Journal> = Arc::new(Notes::default());
    let script = format!("sleep 30; {STILL}");
    let eyes = Eyes::build(
        &config,
        Arc::new(Stop::new()),
        Arc::clone(&attempts),
        asker,
        journal,
        Box::new(|| {}),
    )
    .with_helper(PathBuf::from("/bin/sh"), vec!["-c".to_owned(), script])
    .with_photo_dir(dir.path().join("photos"))
    .with_limits(Duration::from_secs(60), Duration::from_secs(1));
    let started = Instant::now();
    let reply: Value = serde_json::from_slice(
        &eyes.handle(
            json!({"id": "1", "verb": PHOTO_VERB})
                .to_string()
                .as_bytes(),
        ),
    )?;
    assert_eq!(at(&reply, "/err"), &json!("link down (killed)"), "{reply}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    Ok(())
}

#[test]
fn pause_kills_a_running_capture_and_keeps_nothing() -> Result<()> {
    let rig = rig("on", AskAnswer::Allow, &format!("sleep 30; {STILL}"))?;
    let flag = Arc::clone(&rig.paused);
    let pauser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        flag.set();
    });
    let started = Instant::now();
    let reply = rig.photo()?;
    let _joined = pauser.join();
    assert_eq!(at(&reply, "/err"), &json!("paused (killed)"), "{reply}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.photos(), 0, "nothing kept");
    Ok(())
}

#[test]
fn what_the_camera_says_comes_back() -> Result<()> {
    let denied = rig(
        "on",
        AskAnswer::Allow,
        "echo 'camera: denied (System Settings > Privacy & Security > Camera)' >&2; exit 3",
    )?;
    assert_eq!(
        at(&denied.photo()?, "/err"),
        &json!("camera: denied (System Settings > Privacy & Security > Camera)")
    );

    let junk = rig("on", AskAnswer::Allow, "printf 'not a jpeg'")?;
    assert_eq!(at(&junk.photo()?, "/err"), &json!("eyes: not a JPEG"));
    assert_eq!(junk.photos(), 0, "nothing kept");

    let silent = rig("on", AskAnswer::Allow, "exit 4")?;
    assert_eq!(
        at(&silent.photo()?, "/err"),
        &json!("camera: the helper ended with exit 4")
    );

    let noisy = rig(
        "on",
        AskAnswer::Allow,
        r"printf 'odd\033[31m line\n' >&2; exit 5",
    )?;
    let err = at(&noisy.photo()?, "/err")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(err, "camera: odd[31m line", "control characters dropped");

    let prompt = rig(
        "on",
        AskAnswer::Allow,
        &format!("echo '{ASKING_LINE}' >&2; sleep 30"),
    )?;
    let prompt = Rig {
        eyes: prompt
            .eyes
            .with_limits(Duration::from_millis(300), Duration::from_secs(1)),
        ..prompt
    };
    let err = at(&prompt.photo()?, "/err")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        err.starts_with("camera: no photo in 300 ms (the macOS camera prompt was not answered"),
        "{err}"
    );
    assert_eq!(prompt.photos(), 0);
    Ok(())
}

#[test]
fn one_photo_at_a_time() -> Result<()> {
    let rig = Arc::new(rig("on", AskAnswer::Allow, &format!("sleep 1; {STILL}"))?);
    let first = {
        let rig = Arc::clone(&rig);
        thread::spawn(move || rig.photo().map_err(|e| e.to_string()))
    };
    let waited = Instant::now();
    while !rig.ran() && waited.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(20));
    }
    let second = rig.photo()?;
    assert_eq!(
        at(&second, "/err"),
        &json!("busy: a photo is being taken"),
        "{second}"
    );
    let first = first.join().map_err(|_| "the first request panicked")??;
    assert_eq!(at(&first, "/ok"), &json!(true), "{first}");
    assert_eq!(rig.photos(), 1);
    Ok(())
}

#[test]
fn the_router_sends_eyes_verbs_to_the_eyes_only() -> Result<()> {
    let rig = rig("on", AskAnswer::Allow, STILL)?;
    let others: LineHandler = Arc::new(|_line: &[u8]| b"others\n".to_vec());
    let handler = route(others, Arc::new(rig.eyes), None);
    assert_eq!(handler(br#"{"id":"1","verb":"ping"}"#), b"others\n");
    assert_eq!(handler(br#"{"id":"1","verb":"ears.status"}"#), b"others\n");
    assert_eq!(
        handler(br#"{"id":"1","verb":"xfer.get","arg":{}}"#),
        b"others\n"
    );
    assert_eq!(handler(b"not json"), b"others\n");
    let reply: Value = serde_json::from_slice(&handler(br#"{"id":"1","verb":"eyes.look"}"#))?;
    assert_eq!(at(&reply, "/err"), &json!("verb"));
    let photo: Value = serde_json::from_slice(&handler(br#"{"id":"2","verb":"eyes.photo"}"#))?;
    assert_eq!(at(&photo, "/ok"), &json!(true), "{photo}");
    Ok(())
}

#[test]
fn the_menu_line_and_the_shelf_follow_the_mode() -> Result<()> {
    let off = rig("off", AskAnswer::Allow, STILL)?;
    assert!(!off.eyes.is_on());
    assert_eq!(off.eyes.line(false), None, "off: no eyes line");
    assert!(off.eyes.shelf().is_none(), "off: no photo leaves");

    let asks = rig("ask", AskAnswer::Allow, STILL)?;
    assert_eq!(
        asks.eyes.line(false).as_deref(),
        Some("eyes: camera on request, asks first")
    );

    let on = rig("on", AskAnswer::Allow, STILL)?;
    assert_eq!(
        on.eyes.line(false).as_deref(),
        Some("eyes: camera on request")
    );
    assert_eq!(on.eyes.line(true).as_deref(), Some("eyes: paused"));
    let shelf = on.eyes.shelf().ok_or("no shelf")?;
    assert_eq!(shelf.dir(), on.dir.path().join("photos"));
    on.photo()?;
    let line = on.eyes.line(false).unwrap_or_default();
    assert!(
        line.starts_with("eyes: camera on request · last photo 2"),
        "{line}"
    );
    assert!(line.ends_with("Z · 1 photo waiting"), "{line}");
    Ok(())
}

#[test]
fn the_stamp_is_the_stores_shape() {
    assert_eq!(
        stamp(SystemTime::UNIX_EPOCH).as_deref(),
        Some("19700101T000000Z")
    );
    let now = stamp(SystemTime::now()).unwrap_or_default();
    assert!(is_photo_name(&format!("photo-{now}.jpg")), "{now}");
}

/// The data stream's dial in these tests: no hub answers, so a get's ack
/// comes back and its transfer then fails, keeping the photo.
struct NoHub;

impl DataDial for NoHub {
    fn dial(&self, _ticket: &Ticket, _offset: Option<u64>) -> std::io::Result<DataConn> {
        Err(std::io::Error::other("no hub in this test"))
    }
}

const TICKET: &str = "T1cket-0123456789abcdefABCDEF_xy";

fn get_line(id: &str, path: &str) -> Vec<u8> {
    json!({"id": id, "verb": "xfer.get", "arg": {"ticket": TICKET, "path": path, "offset": 0}})
        .to_string()
        .into_bytes()
}

/// The data stream with the eyes' shelf.
fn stream_for(eyes: &Eyes) -> Result<Arc<Xfer>> {
    let shelf = eyes.shelf().ok_or("no shelf: the eyes are off")?;
    Ok(Arc::new(
        Xfer::new(Arc::new(NoHub), Box::new(|| {})).with_shelf(shelf),
    ))
}

/// Waits until the transfer has let its slot go.
fn settle(stream: &Xfer) -> bool {
    for _ in 0..400 {
        if !stream.is_busy() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// What the link answers for the hands while they are off: `verb` to every
/// line it is given; the count tells whether a line got there.
fn hands_off() -> (LineHandler, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    let handler: LineHandler = Arc::new(move |line: &[u8]| {
        counter.fetch_add(1, Ordering::SeqCst);
        refuse_line(line)
    });
    (handler, seen)
}

/// The photo just answered is fetched with the hands off: the eyes serve the
/// next get naming it through the data stream's shelf, once. A second get of
/// it, another photo's get, and every other line go on to the hands.
#[test]
fn the_photo_just_taken_is_fetched_with_the_hands_off() -> Result<()> {
    let rig = rig("on", AskAnswer::Allow, STILL)?;
    let stream = stream_for(&rig.eyes)?;
    let (hands, seen) = hands_off();
    let Rig {
        eyes, dir, notes, ..
    } = rig;
    let handler = route(hands, Arc::new(eyes), Some(Arc::clone(&stream)));

    let photo: Value = serde_json::from_slice(&handler(br#"{"id":"p1","verb":"eyes.photo"}"#))?;
    let path = at(&photo, "/result/path")
        .as_str()
        .ok_or("no path")?
        .to_owned();
    let ack: Value = serde_json::from_slice(&handler(&get_line("g1", &path)))?;
    assert_eq!(at(&ack, "/ok"), &json!(true), "{ack}");
    assert_eq!(at(&ack, "/size"), &json!(STILL_SIZE));
    assert_eq!(at(&ack, "/sha256"), &json!(STILL_SHA256));
    assert_eq!(seen.load(Ordering::SeqCst), 0, "the hands never saw it");
    assert!(settle(&stream), "the transfer ends");
    assert!(Path::new(&path).exists(), "no hub took it: the photo stays");

    let again: Value = serde_json::from_slice(&handler(&get_line("g2", &path)))?;
    assert_eq!(at(&again, "/err"), &json!("verb"), "{again}");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "a second get is the hands'");
    let other = dir.path().join("photos").join("photo-20260115T093000Z.jpg");
    std::fs::write(&other, b"\xFF\xD8\xFF older")?;
    handler(&get_line("g3", &other.to_string_lossy()));
    handler(br#"{"id":"r1","verb":"read","arg":"/tmp"}"#);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        3,
        "so are another photo and a read"
    );

    let log = notes.text();
    assert!(
        log.contains("shelf: id=g1 verb=xfer.get") && log.contains("result=ok size=16"),
        "{log}"
    );
    assert!(!log.contains(TICKET), "never the ticket: {log}");
    Ok(())
}

/// The fetch window: a get past it goes on to the hands. Pause: the eyes
/// refuse the get themselves, and the photo stays.
#[test]
fn the_fetch_window_and_pause() -> Result<()> {
    let late = rig("on", AskAnswer::Allow, STILL)?;
    let late = Rig {
        eyes: late.eyes.with_fetch_window(Duration::ZERO),
        ..late
    };
    let stream = stream_for(&late.eyes)?;
    let (hands, seen) = hands_off();
    let handler = route(hands, Arc::new(late.eyes), Some(Arc::clone(&stream)));
    let photo: Value = serde_json::from_slice(&handler(br#"{"id":"p1","verb":"eyes.photo"}"#))?;
    let path = at(&photo, "/result/path").as_str().ok_or("no path")?;
    thread::sleep(Duration::from_millis(5));
    let get: Value = serde_json::from_slice(&handler(&get_line("g1", path)))?;
    assert_eq!(at(&get, "/err"), &json!("verb"), "{get}");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "past the window: the hands'"
    );

    let paused = rig("on", AskAnswer::Allow, STILL)?;
    let stream = stream_for(&paused.eyes)?;
    let (hands, seen) = hands_off();
    let flag = Arc::clone(&paused.paused);
    let handler = route(hands, Arc::new(paused.eyes), Some(Arc::clone(&stream)));
    let photo: Value = serde_json::from_slice(&handler(br#"{"id":"p1","verb":"eyes.photo"}"#))?;
    let path = at(&photo, "/result/path").as_str().ok_or("no path")?;
    flag.set();
    let get: Value = serde_json::from_slice(&handler(&get_line("g1", path)))?;
    assert_eq!(at(&get, "/err"), &json!("paused"), "{get}");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "refused by the eyes, not the hands"
    );
    assert!(!stream.is_busy(), "no transfer started");
    assert!(Path::new(path).exists());
    Ok(())
}

/// A photo not fetched within a day is swept with the next `eyes.photo` (and
/// at start, `Eyes::sweep`); the menu line counts the photos still waiting,
/// also with the eyes off in config.
#[test]
fn unfetched_photos_are_counted_and_swept_after_a_day() -> Result<()> {
    let on = rig("on", AskAnswer::Allow, STILL)?;
    let photos = on.dir.path().join("photos");
    on.photo()?;
    let line = on.eyes.line(false).unwrap_or_default();
    assert!(line.ends_with(" · 1 photo waiting"), "{line}");
    let old = photos.join("photo-20260115T093000Z.jpg");
    std::fs::write(&old, b"\xFF\xD8\xFF old")?;
    let day_ago = SystemTime::now()
        .checked_sub(PHOTO_MAX_AGE.saturating_add(Duration::from_secs(60)))
        .ok_or("clock")?;
    std::fs::File::open(&old)?.set_modified(day_ago)?;
    let line = on.eyes.line(false).unwrap_or_default();
    assert!(line.ends_with(" · 2 photos waiting"), "{line}");

    on.photo()?;
    assert!(!old.exists(), "swept with the next eyes.photo");
    assert_eq!(on.photos(), 2, "the two of today stay");
    assert!(
        on.notes
            .text()
            .contains("eyes: removed 1 photo not fetched within 24 h"),
        "{}",
        on.notes.text()
    );
    let line = on.eyes.line(false).unwrap_or_default();
    assert!(line.ends_with(" · 2 photos waiting"), "{line}");

    let off = rig("off", AskAnswer::Allow, STILL)?;
    assert_eq!(off.eyes.line(false), None, "off, none waiting: no row");
    let kept = off.dir.path().join("photos");
    std::fs::create_dir(&kept)?;
    std::fs::write(
        kept.join("photo-20260115T093000Z.jpg"),
        b"\xFF\xD8\xFF kept",
    )?;
    assert_eq!(
        off.eyes.line(false).as_deref(),
        Some("eyes: off · 1 photo waiting")
    );
    std::fs::File::open(kept.join("photo-20260115T093000Z.jpg"))?.set_modified(day_ago)?;
    off.eyes.sweep();
    assert_eq!(off.eyes.line(false), None, "swept at start: no row");
    Ok(())
}

/// The rehearsal's first step, Deny from the menu: the question waits in the
/// menu's own `AskBox`, the menu's Deny answers it (`AskBox::answer` with its
/// number, as the Deny item does), and the request answers `denied by the
/// owner`: the camera never started, no photo is kept (nothing for the hub to
/// fetch), and the question leaves the menu.
#[test]
fn deny_from_the_menu_keeps_nothing() -> Result<()> {
    let dir = Scratch::new()?;
    let config = TrayAppConfig::from_toml("[eyes]\nmode = \"ask\"\n")?;
    let ask_box = Arc::new(crate::hands::AskBox::new(Box::new(|| {})));
    let asker: Arc<dyn Asker> = Arc::<crate::hands::AskBox>::clone(&ask_box);
    let notes = Arc::new(Notes::default());
    let journal: Arc<dyn Journal> = Arc::<Notes>::clone(&notes);
    let marked = format!("touch '{}'; {STILL}", dir.path().join("ran").display());
    let eyes = Eyes::build(
        &config,
        Arc::new(Stop::new()),
        Arc::default(),
        asker,
        journal,
        Box::new(|| {}),
    )
    .with_helper(PathBuf::from("/bin/sh"), vec!["-c".to_owned(), marked])
    .with_photo_dir(dir.path().join("photos"))
    .with_limits(Duration::from_secs(10), Duration::from_secs(10));
    let menu = Arc::clone(&ask_box);
    let clicker = thread::spawn(move || {
        for _ in 0..400 {
            if let Some(first) = menu.head() {
                menu.answer(first.id, AskAnswer::Deny);
                return Some(first.line());
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    });
    let reply: Value = serde_json::from_slice(&eyes.handle(br#"{"id":"e1","verb":"eyes.photo"}"#))?;
    let question = clicker
        .join()
        .map_err(|_| "the clicker panicked")?
        .ok_or("no question in the menu")?;
    assert!(
        question.ends_with("take one photo with the camera"),
        "{question}"
    );
    assert_eq!(at(&reply, "/err"), &json!("denied by the owner"), "{reply}");
    assert!(!dir.path().join("ran").exists(), "the camera never started");
    assert!(!dir.path().join("photos").exists(), "no photo kept");
    assert!(
        ask_box.pending_line().is_none(),
        "the question left the menu"
    );
    assert!(
        notes
            .text()
            .contains("decider=click result=refused (denied by the owner)"),
        "{}",
        notes.text()
    );
    Ok(())
}

/// Whether process `pid` is gone: `ps` knows it no more, or only as a zombie.
fn gone(pid: &str) -> bool {
    for _ in 0..200 {
        let seen = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
        if seen.is_ok_and(|stat| stat.is_empty() || stat.starts_with('Z')) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The rehearsal's second step, the macOS camera prompt left unanswered: at
/// the capture limit (60 s; here 300 ms) the request answers `camera: no
/// photo in …` with the prompt's note, nothing is kept, and the helper's
/// whole process group is gone, the stand-in's own child too (on the Mac:
/// `pgrep tulpa-camera` prints nothing).
#[test]
fn an_unanswered_prompt_ends_with_no_helper_left() -> Result<()> {
    let rig = rig("on", AskAnswer::Allow, STILL)?;
    let pids = rig.dir.path().to_path_buf();
    let script = format!(
        "echo $$ > '{helper}'; echo '{ASKING_LINE}' >&2; sleep 30 & echo $! > '{child}'; wait",
        helper = pids.join("helper.pid").display(),
        child = pids.join("child.pid").display(),
    );
    let rig = Rig {
        eyes: rig
            .eyes
            .with_helper(PathBuf::from("/bin/sh"), vec!["-c".to_owned(), script])
            .with_limits(Duration::from_millis(300), Duration::from_secs(1)),
        ..rig
    };
    let started = Instant::now();
    let err = at(&rig.photo()?, "/err")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        err.starts_with("camera: no photo in 300 ms (the macOS camera prompt was not answered"),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.photos(), 0, "nothing kept");
    for file in ["helper.pid", "child.pid"] {
        let pid = std::fs::read_to_string(pids.join(file))?;
        let pid = pid.trim();
        assert!(!pid.is_empty(), "{file}");
        assert!(gone(pid), "{file} {pid} still runs");
    }
    assert!(
        rig.notes
            .text()
            .contains("result=refused (camera: no photo in 300 ms"),
        "{}",
        rig.notes.text()
    );
    Ok(())
}
