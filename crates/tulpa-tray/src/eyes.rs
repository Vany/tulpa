//! The eyes organ behind the tray: `eyes.photo`, one still from the Mac's
//! default camera, taken only when the agent asks for one.
//!
//! The camera is the app bundle's own helper, [`HELPER_NAME`] beside the
//! app's executable, run as the tray's own child. macOS attributes the camera
//! to Tulpa: its prompt names Tulpa, the grant is Tulpa's (Privacy & Security
//! → Camera), and the camera light is macOS's. The helper takes no arguments,
//! prints one JPEG on stdout and exits 0, or exits nonzero with a
//! `camera: …` line on stderr. Before the macOS prompt it says
//! [`ASKING_LINE`]. It keeps nothing itself.
//!
//! The tray keeps the JPEG in the private photo store ([`tulpa_eyes::photo`]:
//! a `0700` directory, a `0600` file named by its UTC second) and answers
//! `{"path":…,"size":…,"sha256":…}`. One request, one still: nothing streams
//! and nothing repeats. The agent fetches the file with the data stream's
//! `xfer.get`. The store's directory is that stream's shelf ([`Eyes::shelf`]):
//! a photo leaves past the hands' deny list and is removed from the Mac once
//! the receiver's verdict names its size and sha256.
//!
//! The photo just answered is the eyes' to hand over ([`route`]): the next
//! `xfer.get` naming it, within [`FETCH_WINDOW`], is served without the hands,
//! whatever their mode (off too), policy or `read` posture, since taking the
//! photo was the decision. Pause still refuses it. Any later get of a photo
//! goes to the hands like any get. A photo not fetched is removed once it is
//! 24 h old ([`Eyes::sweep`]: at Tulpa's start and with each `eyes.photo`),
//! and the menu's eyes line counts the photos still waiting.
//!
//! The gate, in this order: the verb (only `eyes.photo`) → `[eyes] mode`
//! (`off` refuses with `eyes: off in config`) → Pause → no argument → one
//! photo at a time → for `ask`, a click in the menu (Pause while it waits
//! refuses) → Pause again → the helper, whose process group dies on Pause, a
//! dropped link or [`CAPTURE_LIMIT`]. Every request is one line in the
//! activity log, with the photo's name and size, never its bytes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};
use tulpa_eyes::{PHOTO_MAX_BYTES, Photo, PhotoStore, is_photo_name};
use tulpa_hands::exec::{self, Ended};
use tulpa_hands::{AskAnswer, Asker, Journal, Shelf, Xfer, shelf_get};
use tulpa_stop::{Attempts, Stop};

use crate::config::{TrayAppConfig, expand_tilde};
use crate::link::{LineHandler, refuse_line};
use crate::log::format_utc;
use crate::state::OrganMode;

/// The one verb the eyes answer.
pub const PHOTO_VERB: &str = "eyes.photo";
/// The camera helper's file name, beside the app's executable in the bundle.
pub const HELPER_NAME: &str = "tulpa-camera";
/// Where the photos are kept until the agent fetches them.
pub const PHOTO_DIR: &str = "~/Library/Application Support/tulpa/photos";
/// How long the helper may take, the first macOS camera prompt included.
pub const CAPTURE_LIMIT: Duration = Duration::from_secs(60);
/// How long an `ask` waits for a click.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after its reply the photo's own fetch may come; the hub asks at once.
pub const FETCH_WINDOW: Duration = Duration::from_secs(60);
/// The line the helper prints before macOS asks its owner about the camera.
pub const ASKING_LINE: &str = "camera: asking macOS for access";
/// The question the menu shows when `[eyes] mode = "ask"`.
const QUESTION: &str = "take one photo with the camera";
/// The longest helper line passed on in a refusal.
const SAID_MAX: usize = 200;

/// One request line as it arrives; `arg` must be absent, null or empty.
#[derive(Deserialize)]
struct Request {
    id: String,
    verb: String,
    #[serde(default)]
    arg: Option<Value>,
}

/// What the menu shows besides the mode.
#[derive(Debug, Default)]
struct Status {
    taking: bool,
    last: Option<String>,
}

/// The eyes: the posture from the config, the camera helper, the photo store,
/// the flags the tray and the link own, a way to ask, and the log.
pub struct Eyes {
    posture: Result<OrganMode, String>,
    helper: Option<(PathBuf, Vec<String>)>,
    home: PathBuf,
    store: PhotoStore,
    limit: Duration,
    ask_timeout: Duration,
    paused: Arc<Stop>,
    link: Arc<Attempts>,
    asker: Arc<dyn Asker>,
    journal: Arc<dyn Journal>,
    busy: AtomicBool,
    status: Mutex<Status>,
    /// The photo just answered and when: the next `xfer.get` naming it within
    /// the fetch window is the eyes' to serve.
    offered: Mutex<Option<(PathBuf, Instant)>>,
    fetch_window: Duration,
    changed: Box<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for Eyes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Eyes")
            .field("posture", &self.posture)
            .field("helper", &self.helper)
            .field("store", &self.store)
            .field("busy", &self.busy.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// The one-photo slot; dropping it frees the slot however the photo ends.
struct Slot<'a>(&'a AtomicBool);

impl<'a> Slot<'a> {
    fn take(busy: &'a AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            .then_some(Self(busy))
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Eyes {
    /// The eyes as `[eyes]` sets them: `mode = "on"` takes a photo when asked,
    /// `"ask"` after a click in the menu, anything else refuses. The helper
    /// is [`HELPER_NAME`] beside this executable; the photos go to
    /// [`PHOTO_DIR`]. `paused` is the tray's flag, `link` the link's
    /// attempts (a request is killed by the death of the one it came on); `changed` re-renders the menu's eyes line.
    #[must_use]
    pub fn build(
        config: &TrayAppConfig,
        paused: Arc<Stop>,
        link: Arc<Attempts>,
        asker: Arc<dyn Asker>,
        journal: Arc<dyn Journal>,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let posture = match config.eyes.posture() {
            Ok(mode @ (OrganMode::On | OrganMode::Ask)) => Ok(mode),
            Ok(_) | Err(_) => Err("eyes: off in config".to_owned()),
        };
        let helper = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(HELPER_NAME)))
            .map(|program| (program, Vec::new()));
        Self {
            posture,
            helper,
            home: expand_tilde("~"),
            store: PhotoStore::new(expand_tilde(PHOTO_DIR)),
            limit: CAPTURE_LIMIT,
            ask_timeout: ASK_TIMEOUT,
            paused,
            link,
            asker,
            journal,
            busy: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            offered: Mutex::new(None),
            fetch_window: FETCH_WINDOW,
            changed,
        }
    }

    /// Another camera helper: `program` with `args` (tests).
    #[must_use]
    pub fn with_helper(mut self, program: PathBuf, args: Vec<String>) -> Self {
        self.helper = Some((program, args));
        self
    }

    /// Another photo directory (tests).
    #[must_use]
    pub fn with_photo_dir(mut self, dir: PathBuf) -> Self {
        self.store = PhotoStore::new(dir);
        self
    }

    /// Other limits for the helper and for a click (tests).
    #[must_use]
    pub const fn with_limits(mut self, capture: Duration, ask: Duration) -> Self {
        self.limit = capture;
        self.ask_timeout = ask;
        self
    }

    /// Another fetch window than [`FETCH_WINDOW`] (tests).
    #[must_use]
    pub const fn with_fetch_window(mut self, window: Duration) -> Self {
        self.fetch_window = window;
        self
    }

    /// Whether `[eyes]` lets the agent ask for photos (`on` or `ask`).
    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.posture.is_ok()
    }

    /// The data stream's shelf for the photos while the eyes are on: `xfer.get`
    /// hands a photo over from [`PHOTO_DIR`] once. `None` while they are off,
    /// so a photo kept from before stays on the Mac.
    #[must_use]
    pub fn shelf(&self) -> Option<Shelf> {
        self.is_on()
            .then(|| Shelf::new(self.store.dir().to_path_buf(), is_photo_name))
    }

    /// The menu's eyes line, with the photos still waiting to be fetched;
    /// `None` while the eyes are off in config and none wait.
    #[must_use]
    pub fn line(&self, paused: bool) -> Option<String> {
        let waiting = match self.store.waiting() {
            0 => String::new(),
            n => format!(" · {} waiting", photos(n)),
        };
        let Ok(mode) = self.posture.as_ref() else {
            // Photos kept before the eyes were turned off still show.
            return (!waiting.is_empty()).then(|| format!("eyes: off{waiting}"));
        };
        let (taking, last) = self
            .status
            .lock()
            .map(|status| (status.taking, status.last.clone()))
            .unwrap_or_default();
        let line = if paused {
            "eyes: paused".to_owned()
        } else if taking {
            "eyes: taking a photo".to_owned()
        } else if *mode == OrganMode::Ask {
            "eyes: camera on request, asks first".to_owned()
        } else {
            "eyes: camera on request".to_owned()
        };
        let line = match last {
            Some(at) if !taking => format!("{line} · last photo {at}"),
            _ => line,
        };
        Some(format!("{line}{waiting}"))
    }

    /// Removes the photos not fetched within 24 h
    /// ([`tulpa_eyes::PHOTO_MAX_AGE`]) and logs how many; Tulpa runs it at
    /// start, and the eyes with each `eyes.photo`.
    pub fn sweep(&self) {
        let swept = self.store.sweep(SystemTime::now());
        if swept > 0 {
            self.journal.record(&format!(
                "eyes: removed {} not fetched within 24 h",
                photos(swept)
            ));
            (self.changed)();
        }
    }

    /// Answers one request line with one reply line (newline-terminated).
    #[must_use]
    pub fn handle(&self, line: &[u8]) -> Vec<u8> {
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            self.journal.record("eyes: unparseable line refused");
            return refuse_line(line);
        };
        // A control character in the id could start a line of its own in the log.
        if request.id.is_empty()
            || request.id.len() > 64
            || request.id.chars().any(char::is_control)
        {
            self.journal.record("eyes: bad id refused");
            return refuse_line(line);
        }
        let started = Instant::now();
        let (decider, done) = self.decide(&request.verb, request.arg.as_ref());
        if let Ok(photo) = &done {
            self.offer(&photo.path);
        }
        let result = match &done {
            Ok(photo) => format!(
                "ok {} ({} bytes)",
                photo
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                photo.size
            ),
            Err(why) => format!("refused ({why})"),
        };
        self.journal.record(&format!(
            "eyes: id={} verb={:?} decider={decider} result={result} ms={}",
            request.id,
            request.verb,
            started.elapsed().as_millis()
        ));
        let value = match done {
            Ok(photo) => json!({"id": request.id, "ok": true, "result": {
                "path": photo.path.to_string_lossy(),
                "size": photo.size,
                "sha256": photo.sha256,
            }}),
            Err(err) => json!({"id": request.id, "ok": false, "err": err}),
        };
        let mut reply = value.to_string().into_bytes();
        reply.push(b'\n');
        reply
    }

    /// The gate, then the photo; the first part says who decided.
    fn decide(&self, verb: &str, arg: Option<&Value>) -> (&'static str, Result<Photo, String>) {
        // The attempt this request came on, taken before the question waits: its death, not a later attempt's,
        // kills the capture.
        let link = self.link.current();
        if verb != PHOTO_VERB {
            return ("policy", Err("verb".to_owned()));
        }
        self.sweep();
        let mode = match &self.posture {
            Ok(mode) => *mode,
            Err(why) => return ("policy", Err(why.clone())),
        };
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        let no_arg = match arg {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.trim().is_empty(),
            Some(_) => false,
        };
        if !no_arg {
            return ("policy", Err("arg".to_owned()));
        }
        let Some(_slot) = Slot::take(&self.busy) else {
            return ("policy", Err("busy: a photo is being taken".to_owned()));
        };
        let decider = if mode == OrganMode::Ask {
            let answer = self.asker.ask(QUESTION, self.ask_timeout);
            // Pause denies a waiting question; it says so as a pause.
            if self.paused.is_set() {
                return ("pause", Err("paused".to_owned()));
            }
            match answer {
                AskAnswer::Allow => "click",
                AskAnswer::Deny => return ("click", Err("denied by the owner".to_owned())),
                _ => return ("expired", Err("no click in time".to_owned())),
            }
        } else {
            "policy"
        };
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        self.set_status(true, None);
        let done = self.capture(&link);
        let kept_at = done
            .as_ref()
            .ok()
            .and_then(|_| format_utc(SystemTime::now()));
        self.set_status(false, kept_at);
        (decider, done)
    }

    /// Offers the photo at `path` to its fetch, from now.
    fn offer(&self, path: &Path) {
        if let Ok(mut offered) = self.offered.lock() {
            *offered = Some((path.to_path_buf(), Instant::now()));
        }
    }

    /// Serves `line` through `stream` when it is the `xfer.get` of the photo
    /// just answered, within the fetch window: once, without the hands.
    /// `None` for any other line.
    fn fetch(&self, line: &[u8], stream: &Arc<Xfer>) -> Option<Vec<u8>> {
        let (path, at) = self.offered.lock().ok()?.clone()?;
        if at.elapsed() > self.fetch_window {
            self.withdraw(&path);
            return None;
        }
        let reply = shelf_get(stream, line, &path, &self.paused, &self.journal)?;
        self.withdraw(&path);
        Some(reply)
    }

    /// Ends the offer of the photo at `path`, unless a newer photo took its place.
    fn withdraw(&self, path: &Path) {
        if let Ok(mut offered) = self.offered.lock()
            && offered.as_ref().is_some_and(|(offer, _)| offer == path)
        {
            *offered = None;
        }
    }

    fn set_status(&self, taking: bool, last: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.taking = taking;
            if last.is_some() {
                status.last = last;
            }
        }
        (self.changed)();
    }

    /// Runs the helper once and keeps what it printed.
    fn capture(&self, link: &Stop) -> Result<Photo, String> {
        let Some((program, args)) = &self.helper else {
            return Err("eyes: no camera helper (Tulpa runs outside its app bundle)".to_owned());
        };
        let mut command = exec::cleared(program, &self.home);
        command.args(args);
        let outcome =
            exec::run(command, &self.paused, link, self.limit, PHOTO_MAX_BYTES).map_err(|e| {
                format!(
                    "eyes: the camera helper did not start ({}: {e})",
                    program.display()
                )
            })?;
        match outcome.ended {
            Ended::Exit(Some(0)) => {}
            Ended::Paused => return Err("paused (killed)".to_owned()),
            Ended::Abandoned => return Err("link down (killed)".to_owned()),
            Ended::Timeout => return Err(no_photo_in(self.limit, &outcome.stderr)),
            Ended::Exit(code) => {
                return Err(helper_said(&outcome.stderr).unwrap_or_else(|| {
                    format!(
                        "camera: the helper ended with {}",
                        code.map_or_else(|| "a signal".to_owned(), |c| format!("exit {c}"))
                    )
                }));
            }
            _ => return Err("camera: the helper ended".to_owned()),
        }
        if outcome.cut {
            return Err(format!("camera: the photo is over {PHOTO_MAX_BYTES} bytes"));
        }
        let stamp = stamp(SystemTime::now())
            .ok_or_else(|| "eyes: the clock reads before 1970".to_owned())?;
        self.store
            .save(&outcome.stdout, &stamp)
            .map_err(|e| format!("eyes: {e}"))
    }
}

/// `1 photo`, `2 photos`.
fn photos(count: usize) -> String {
    if count == 1 {
        "1 photo".to_owned()
    } else {
        format!("{count} photos")
    }
}

/// `YYYYMMDDTHHMMSSZ` for `time`, the photo's name stamp.
fn stamp(time: SystemTime) -> Option<String> {
    format_utc(time).map(|utc| utc.chars().filter(|c| *c != '-' && *c != ':').collect())
}

/// The helper's last line, made safe for the wire and the log, as a
/// `camera: …` refusal.
fn helper_said(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let last = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let clean: String = last
        .chars()
        .filter(|c| !c.is_control())
        .take(SAID_MAX)
        .collect();
    Some(if clean.starts_with("camera:") {
        clean
    } else {
        format!("camera: {clean}")
    })
}

/// The refusal past the capture limit; it says when the macOS prompt was up.
fn no_photo_in(limit: Duration, stderr: &[u8]) -> String {
    let spoken = if limit.subsec_millis() == 0 {
        format!("{} s", limit.as_secs())
    } else {
        format!("{} ms", limit.as_millis())
    };
    if String::from_utf8_lossy(stderr).contains(ASKING_LINE) {
        format!(
            "camera: no photo in {spoken} (the macOS camera prompt was not answered; ask again once it is allowed)"
        )
    } else {
        format!("camera: no photo in {spoken}")
    }
}

/// The link's line handler with the eyes in front of `others`.
///
/// `eyes.*` verbs go to the eyes, and so does the `xfer.get` of the photo
/// just answered, served on `stream` (the data stream) whatever the hands'
/// state. Every other line goes to `others`.
#[must_use]
pub fn route(others: LineHandler, eyes: Arc<Eyes>, stream: Option<Arc<Xfer>>) -> LineHandler {
    #[derive(Deserialize)]
    struct Verb {
        verb: String,
    }
    Arc::new(move |line: &[u8]| {
        let verb = serde_json::from_slice::<Verb>(line)
            .map(|v| v.verb)
            .unwrap_or_default();
        if verb.starts_with("eyes.") {
            return eyes.handle(line);
        }
        if verb == "xfer.get"
            && let Some(stream) = &stream
            && let Some(reply) = eyes.fetch(line, stream)
        {
            return reply;
        }
        others(line)
    })
}

#[cfg(test)]
mod tests;
