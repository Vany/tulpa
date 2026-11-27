//! The data stream: big files to and from the Mac, beside the control line.
//!
//! Control only asks and is answered at once: `xfer.put` with
//! `{ticket, path, size, sha256}`, `xfer.get` with `{ticket, path, offset}`.
//! The bytes travel on a second connection the Mac dials itself ([`DataDial`]:
//! the link's own route, a second `ssh -W` child), which carries the transfer
//! and its verdict. The receiver owns the `.part`, the offset, the sha256
//! check and the rename; the sender only seeks.
//!
//! **put** (the Mac receives). The ack is `{"ok":true,"result":{"offset":n}}`;
//! the data hello says the same offset, the size of the Mac's own
//! `<path>.<sha256[:12]>.part` (made at the first byte, so a ticket the hub
//! refuses leaves none). Exactly the `size - offset` bytes still missing are
//! taken (a read that runs past them aborts), fsynced, checked against size and
//! sha256 and renamed onto `<path>`; then one verdict line,
//! `{"ok":true,"size":n,"sha256":"…"}` or `{"ok":false,"err":"…"}`, and the
//! connection closes. The receiver answers as soon as it holds those bytes:
//! EOF is not the signal. On put the hub never half-closes toward the Mac and
//! closes both legs once the verdict has passed through: an `ssh -W` child
//! stops reading its stdin once the far end half-closes, so a line written
//! after that EOF would never leave the Mac.
//!
//! **get** (the Mac sends). Size and sha256 are computed before the ack,
//! `{"ok":true,"size":n,"sha256":"…"}` (also under `result`); the bytes start
//! at the request's `offset` (required), the client's own `.part` size. Size and mtime are
//! checked again before the last bytes leave: a file changed while sending
//! fails short. Then a half-close and the client's verdict line.
//!
//! The path policy is the hands' `write`/`read`: in full mode the user's own
//! permissions; otherwise the real path of the parent directory must lie in a
//! root and neither the requested nor the real names may be on the deny list.
//! The `.part` is opened under the parent's descriptor with `O_NOFOLLOW`
//! (`O_EXCL` when new). One transfer at a time; 60 s without a byte closes the
//! data connection and keeps the `.part`; Pause closes it too. The ticket is
//! never logged.
//!
//! **The sweep** removes only `.part` files this program made. A new `.part`
//! is marked right after its `O_EXCL` create with the extended attribute
//! `user.tulpa.part`, whose value is the `.part`'s own file name; the mark is
//! taken off just before the rename, so the finished file never carries it.
//! Each new transfer deletes, in the directories it knows, a regular file
//! (opened with `O_NOFOLLOW`) of the `<name>.<12 hex>.part` shape that is
//! untouched for 24 h and carries the mark naming it under its current name.
//! Anything else of that shape stays: the user's own file, a copy under
//! another name, a `.part` on a filesystem without extended attributes (it is
//! never marked, so never swept).
//!
//! **The shelf** ([`Shelf`], set by the tray, never by the policy file or the
//! link) is one directory whose files `get` hands over once: the eyes' photos.
//! A get of a path directly in it, with a name of the shelf's shape, is sent
//! outside the roots and past the deny list, in roots and full mode alike.
//! The directory and the file are opened with `O_NOFOLLOW`, and the file must
//! be a regular file with one name. Through the hands, the verb's posture
//! still applies. [`crate::engine::shelf_get`] serves one named file with no
//! hands at all: the eyes' fetch of the photo they just took. When the
//! receiver's verdict is ok and names the file's own size and sha256, the
//! file is removed, if its name still leads to the file that was sent. Any
//! other name in the directory follows the roots and the deny list as usual.

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::fs::{Access, AtFlags, Dir, FileType, Mode, OFlags, XattrFlags};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tulpa_stop::Got;

use crate::engine::{Journal, WorkHook};
use crate::fsx::{self, PathRefusal};
use crate::policy::Policy;
use crate::scrub::scrub_secrets;

/// 60 s with no byte on a data connection closes it; a put's `.part` stays.
pub const IDLE: Duration = Duration::from_secs(60);
/// The largest file either way (4 GiB, the server side's cap, held here too).
pub const SIZE_MAX: u64 = 4_294_967_296;
/// Free space a put needs beyond the bytes it still has to receive (64 MiB).
pub const SPACE_MARGIN: u64 = 67_108_864;
/// A `.part` this program made, untouched this long (24 h), is swept on the
/// next transfer.
pub const PART_MAX_AGE: Duration = Duration::from_secs(86_400);
/// The extended attribute that marks a `.part` as this program's own; its value
/// is the `.part`'s file name, so a copy under another name is not ours. Linux
/// lets an unprivileged process set only `user.` names; macOS takes any name.
const PART_MARK: &CStr = c"user.tulpa.part";
/// The error for an extended attribute that is not there.
#[cfg(target_os = "macos")]
const NO_ATTR: rustix::io::Errno = rustix::io::Errno::NOATTR;
/// The error for an extended attribute that is not there.
#[cfg(not(target_os = "macos"))]
const NO_ATTR: rustix::io::Errno = rustix::io::Errno::NODATA;
/// One read or write on the data connection.
const CHUNK: usize = 256 * 1024;
/// The longest verdict line read back from a get's receiver.
const VERDICT_MAX: usize = 4096;
/// How long a finished connection may take to deliver its last line before it is closed.
const LINGER: Duration = Duration::from_secs(10);
/// How many directories that held a `.part` are remembered for the sweep.
const SWEEP_DIRS_MAX: usize = 64;
/// The tray hears about progress at most this often.
const TELL_EVERY: Duration = Duration::from_secs(1);
/// Ticket length bounds: 32 random bytes as hex, base64 or base64url fit.
const TICKET_LEN: std::ops::RangeInclusive<usize> = 16..=256;
/// The ticket alphabet: no quote, backslash or control byte, so the data
/// hello needs no escaping.
const TICKET_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=_-";
/// A get writes in pieces this big, so the idle clock sees slow progress.
const WRITE_PIECE: usize = 16 * 1024;
/// How much of a receiver's error is kept for the log and the menu.
const ERR_SHOWN_MAX: usize = 200;
/// How much of a path the outcome line keeps.
const LOGGED_PATH_MAX: usize = 200;
/// The refusal while a transfer holds the slot.
const BUSY: &str = "busy: a transfer is running";

/// Which way the bytes go, seen from the Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Direction {
    /// The Mac receives (`xfer.put`).
    Put,
    /// The Mac sends (`xfer.get`).
    Get,
}

impl Direction {
    /// The direction a control verb names, if it is a transfer verb.
    #[must_use]
    pub fn of_verb(verb: &str) -> Option<Self> {
        match verb {
            "xfer.put" => Some(Self::Put),
            "xfer.get" => Some(Self::Get),
            _ => None,
        }
    }

    /// `put` or `get`, for the log and the menu.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Get => "get",
        }
    }
}

/// The hub's single-use ticket for the Mac's data leg.
///
/// It is said once, in the data hello, and never logged: `Debug` shows only
/// its length.
#[derive(Clone, PartialEq, Eq)]
pub struct Ticket(String);

impl fmt::Debug for Ticket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Ticket({} bytes)", self.0.len())
    }
}

impl Ticket {
    /// A ticket from a control request: 16-256 bytes of `[A-Za-z0-9+/=_-]`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        (TICKET_LEN.contains(&text.len()) && text.bytes().all(|b| TICKET_CHARS.contains(&b)))
            .then(|| Self(text.to_owned()))
    }

    /// The ticket's text, for the data hello and nothing else.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

/// The writing half of a data connection.
pub trait HalfWrite: Write + Send {
    /// Ends this side's bytes (a half-close): the peer reads EOF while the
    /// reading half stays open for its verdict.
    ///
    /// # Errors
    ///
    /// Returns the error of flushing or closing the writing half.
    fn half_close(&mut self) -> io::Result<()>;
}

/// Ends a data connection from any thread.
pub trait Closer: Send + Sync {
    /// Closes at once: a blocked read or write on either half returns.
    fn close(&self);

    /// Gives the peer up to `grace` to take what was written and close its
    /// side, then closes. The default closes at once.
    fn linger(&self, _grace: Duration) {
        self.close();
    }
}

/// One dialled data connection whose hello has been said.
pub struct DataConn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn HalfWrite>,
    closer: Arc<dyn Closer>,
}

impl fmt::Debug for DataConn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("DataConn").finish_non_exhaustive()
    }
}

impl DataConn {
    /// Joins the two halves and the way to close them.
    #[must_use]
    pub fn new(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn HalfWrite>,
        closer: Arc<dyn Closer>,
    ) -> Self {
        Self {
            reader,
            writer,
            closer,
        }
    }
}

/// Dials the data connection beside the link.
pub trait DataDial: Send + Sync {
    /// Opens one data connection and says its hello: the ticket and, for a
    /// put, the Mac's own `.part` size (`offset`).
    ///
    /// # Errors
    ///
    /// Returns why no connection could be opened; the transfer then fails and
    /// its slot is freed.
    fn dial(&self, ticket: &Ticket, offset: Option<u64>) -> io::Result<DataConn>;
}

/// The local side of the data stream: the dialer, the one-transfer slot (apart
/// from the hands' request slot) and what the menu shows.
pub struct Xfer {
    dial: Arc<dyn DataDial>,
    idle: Duration,
    busy: AtomicBool,
    status: Mutex<Status>,
    sweep_dirs: Mutex<BTreeSet<PathBuf>>,
    shelf: Option<Shelf>,
    changed: Box<dyn Fn() + Send + Sync>,
}

/// A directory whose own files `xfer.get` hands over once, outside the roots
/// and past the deny list: the eyes' photos. Only the tray makes one; the
/// policy file and the link cannot.
#[derive(Clone)]
pub struct Shelf {
    dir: PathBuf,
    names: fn(&str) -> bool,
}

impl fmt::Debug for Shelf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Shelf")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Shelf {
    /// The files directly in `dir`, an absolute path as the tray writes it,
    /// whose names pass `names`.
    #[must_use]
    pub const fn new(dir: PathBuf, names: fn(&str) -> bool) -> Self {
        Self { dir, names }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The name `path` has on the shelf: `None` unless it lies directly in the
    /// shelf's directory with a name of the shelf's shape.
    fn name_of<'p>(&self, path: &'p Path) -> Option<&'p str> {
        let name = path.file_name()?.to_str()?;
        (path.parent()? == self.dir.as_path() && (self.names)(name)).then_some(name)
    }

    /// Opens `name` on the shelf: neither the directory nor the file through a
    /// symlink, and only a regular file with one name.
    fn open(&self, name: &str) -> Result<(File, Shelved), String> {
        let refused = |e: rustix::io::Errno| PathRefusal::Open(e.to_string()).words();
        let dir = rustix::fs::open(
            &self.dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(refused)?;
        let file = File::from(
            rustix::fs::openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(refused)?,
        );
        let meta = file.metadata().map_err(|e| format!("stat: {e}"))?;
        if !meta.is_file() {
            return Err(PathRefusal::Kind("regular file").words());
        }
        // A second name for the file could have been given to anything: only
        // a file with one name leaves the shelf.
        if meta.nlink() != 1 {
            return Err(PathRefusal::Denied.words());
        }
        let shelved = Shelved {
            dir,
            name: name.to_owned(),
            dev: meta.dev(),
            ino: meta.ino(),
        };
        Ok((file, shelved))
    }
}

/// A file a get took from the shelf: its directory, its name, and which file
/// it was.
struct Shelved {
    dir: OwnedFd,
    name: String,
    dev: u64,
    ino: u64,
}

impl Shelved {
    /// Removes the file now that the receiver holds it, if its name still
    /// leads to the file that was sent; says what happened, for the log.
    fn hand_over(&self) -> String {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let same = rustix::fs::openat(&self.dir, self.name.as_str(), flags, Mode::empty())
            .ok()
            .map(File::from)
            .and_then(|file| file.metadata().ok())
            .is_some_and(|meta| meta.dev() == self.dev && meta.ino() == self.ino);
        if !same {
            return "kept on the shelf (its name leads to another file now)".to_owned();
        }
        match rustix::fs::unlinkat(&self.dir, self.name.as_str(), AtFlags::empty()) {
            Ok(()) => "removed from the shelf".to_owned(),
            Err(e) => format!("kept on the shelf (remove: {e})"),
        }
    }
}

impl fmt::Debug for Xfer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Xfer")
            .field("idle", &self.idle)
            .field("busy", &self.is_busy())
            .finish_non_exhaustive()
    }
}

/// What the menu shows: the running transfer, else the last one's outcome.
#[derive(Debug, Default)]
struct Status {
    running: Option<Running>,
    last: Option<String>,
    told: Option<Instant>,
}

#[derive(Debug)]
struct Running {
    direction: Direction,
    name: String,
    done: u64,
    size: u64,
}

impl Xfer {
    /// The data stream over `dial`; `changed` runs when the menu line changes
    /// (at most once a second while bytes flow).
    #[must_use]
    pub fn new(dial: Arc<dyn DataDial>, changed: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            dial,
            idle: IDLE,
            busy: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            sweep_dirs: Mutex::new(BTreeSet::new()),
            shelf: None,
            changed,
        }
    }

    /// Another idle limit than [`IDLE`] (tests).
    #[must_use]
    pub const fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// Hands over the files of a shelf on `get` (see the module's notes).
    #[must_use]
    pub fn with_shelf(mut self, files: Shelf) -> Self {
        self.shelf = Some(files);
        self
    }

    /// True while a transfer holds the slot.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Whether `path` names a file the shelf would hand over: directly in its
    /// directory, with a name of its shape.
    pub(crate) fn on_shelf(&self, path: &Path) -> bool {
        self.shelf
            .as_ref()
            .is_some_and(|shelf| shelf.name_of(path).is_some())
    }

    /// The menu's line: the running transfer with the Mac's byte count, else
    /// the last outcome; `None` before the first transfer.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        let status = self.status.lock().ok()?;
        if let Some(run) = &status.running {
            return Some(format!(
                "transfer: {} {} {}/{} MB",
                run.direction.word(),
                run.name,
                megabytes(run.done),
                megabytes(run.size)
            ));
        }
        status.last.as_ref().map(|last| format!("transfer: {last}"))
    }

    fn begin(&self, direction: Direction, name: String, done: u64, size: u64) {
        if let Ok(mut status) = self.status.lock() {
            status.running = Some(Running {
                direction,
                name,
                done,
                size,
            });
            status.told = Some(Instant::now());
        }
        (self.changed)();
    }

    fn advance(&self, done: u64) {
        let tell = self.status.lock().is_ok_and(|mut status| {
            if let Some(run) = status.running.as_mut() {
                run.done = done;
            }
            let due = status.told.is_none_or(|told| told.elapsed() >= TELL_EVERY);
            if due {
                status.told = Some(Instant::now());
            }
            due
        });
        if tell {
            (self.changed)();
        }
    }

    fn end(&self, last: String) {
        if let Ok(mut status) = self.status.lock() {
            status.running = None;
            status.last = Some(last);
        }
        (self.changed)();
    }

    fn remember(&self, dir: PathBuf) {
        if let Ok(mut dirs) = self.sweep_dirs.lock()
            && (dirs.len() < SWEEP_DIRS_MAX || dirs.contains(&dir))
        {
            dirs.insert(dir);
        }
    }

    /// Sweeps the remembered directories (each still inside the roots unless
    /// in full mode).
    fn sweep_known(&self, policy: &Policy, full: bool) {
        let dirs: Vec<PathBuf> = self
            .sweep_dirs
            .lock()
            .map(|dirs| dirs.iter().cloned().collect())
            .unwrap_or_default();
        for dir in dirs {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let Ok(fd) = rustix::fs::open(&dir, flags, Mode::empty()) else {
                continue;
            };
            let allowed = full
                || fsx::fd_path(&fd)
                    .is_ok_and(|real| policy.inside_roots(&real) && !policy.denied(&real));
            if allowed {
                sweep_dir(&fd, SystemTime::now());
            }
        }
    }
}

/// The slot one transfer holds; dropping it frees the slot on every exit.
struct Slot(Arc<Xfer>);

impl Slot {
    fn take(xfer: &Arc<Xfer>) -> Option<Self> {
        xfer.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            .then(|| Self(Arc::clone(xfer)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

/// The keep-awake hold of a running transfer, let go however it ends.
struct Working(Option<WorkHook>);

impl Working {
    fn hold(hook: Option<WorkHook>) -> Self {
        if let Some(hook) = &hook {
            hook(true);
        }
        Self(hook)
    }
}

impl Drop for Working {
    fn drop(&mut self) {
        if let Some(hook) = &self.0 {
            hook(false);
        }
    }
}

/// The control request's argument, as the hub sends it.
#[derive(Deserialize)]
struct RawArg {
    #[serde(default)]
    ticket: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    offset: Option<u64>,
}

/// One checked `xfer.put` / `xfer.get` request.
#[derive(Debug)]
pub struct XferRequest {
    pub(crate) direction: Direction,
    ticket: Ticket,
    path: String,
    size: u64,
    sha256: String,
    offset: u64,
}

impl XferRequest {
    /// Reads the argument: an object, or a string holding one.
    ///
    /// # Errors
    ///
    /// Why the request is refused, in words safe for the log; the values are
    /// never echoed (one of them is the ticket).
    pub fn parse(direction: Direction, arg: Option<&serde_json::Value>) -> Result<Self, String> {
        let raw: RawArg = match arg {
            Some(serde_json::Value::String(text)) => serde_json::from_str(text).ok(),
            Some(value @ serde_json::Value::Object(_)) => {
                serde_json::from_value(value.clone()).ok()
            }
            _ => None,
        }
        .ok_or_else(|| "arg: an object {ticket, path, …}".to_owned())?;
        let ticket = raw
            .ticket
            .as_deref()
            .and_then(Ticket::parse)
            .ok_or_else(|| "arg: ticket missing or malformed".to_owned())?;
        let path = raw
            .path
            .filter(|p| !p.is_empty() && p.len() <= 4096)
            .ok_or_else(|| "arg: path missing".to_owned())?;
        let (size, sha256) = match direction {
            Direction::Put => (
                raw.size.ok_or_else(|| "arg: put needs size".to_owned())?,
                raw.sha256
                    .as_deref()
                    .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_ascii_lowercase)
                    .ok_or_else(|| "arg: put needs sha256 (64 hex)".to_owned())?,
            ),
            Direction::Get => (0, String::new()),
        };
        // A put's offset is the receiver's own: a claimed one is ignored. A get's
        // is the receiver's, required: guessing 0 would append to its `.part`.
        let offset = match direction {
            Direction::Put => 0,
            Direction::Get => raw
                .offset
                .ok_or_else(|| "arg: get needs offset (the client's .part size)".to_owned())?,
        };
        Ok(Self {
            direction,
            ticket,
            path,
            size,
            sha256,
            offset,
        })
    }

    /// The request's path: a put's name inside the caller's directory, a
    /// get's file. Never the ticket.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the request names exactly `path`, as written.
    pub(crate) fn names(&self, path: &Path) -> bool {
        Path::new(&self.path) == path
    }

    /// The request as the activity log shows it: never the ticket.
    pub(crate) fn logged(&self) -> String {
        match self.direction {
            Direction::Put => format!("{} ({} bytes)", self.path, self.size),
            Direction::Get => format!("{} from {}", self.path, self.offset),
        }
    }

    /// The question a click answers when the posture is `ask`.
    pub(crate) fn question(&self) -> String {
        match self.direction {
            Direction::Put => format!("receive a file into {} ({} bytes)", self.path, self.size),
            Direction::Get => format!("send the file {}", self.path),
        }
    }
}

/// The control ack of an accepted transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ack {
    /// put: the Mac's own `.part` size, where the bytes resume.
    Put {
        /// The `.part`'s size in bytes, where the sender resumes.
        offset: u64,
    },
    /// get: the file's size and sha256, computed before any byte.
    Get {
        /// The whole file's size in bytes.
        size: u64,
        /// The whole file's sha256, hex.
        sha256: String,
    },
}

/// What the engine lends the transfer it starts.
#[non_exhaustive]
pub struct Lent {
    /// The flag that stops the transfer; for the files organ's puts (#0395) a
    /// flag nothing ever sets: the channel rides the link, not Pause.
    pub paused: Arc<tulpa_stop::Stop>,
    /// Where the transfer's outcome line goes.
    pub journal: Arc<dyn Journal>,
    /// The keep-awake hold of a running transfer, if the tray gave one.
    pub working: Option<WorkHook>,
}

/// A lent slot says nothing about itself beyond its shape; the parts (a
/// journal) are not for debug output.
impl fmt::Debug for Lent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lent").finish_non_exhaustive()
    }
}

impl Lent {
    /// What an always-on organ lends its transfers (#0395): a fresh pause
    /// flag nothing ever sets, so the transfer answers no Pause at all, the
    /// organ's own journal, and the keep-awake hold of a running transfer, if
    /// the tray gave one.
    #[must_use]
    pub fn always_on(journal: Arc<dyn Journal>, working: Option<WorkHook>) -> Self {
        Self {
            paused: Arc::new(tulpa_stop::Stop::new()),
            journal,
            working,
        }
    }
}

/// Told once a put into a directory the caller owns has landed (#0395).
///
/// The file's path, size and sha256, on the transfer's own thread before its
/// slot goes: the delivery step of the files organ.
pub type Landed = Box<dyn FnOnce(&Path, u64, &str) + Send>;

/// Checks and prepares one transfer, starts it on its own thread, and returns
/// the control ack at once (or why not: busy, policy, no space…).
pub(crate) fn start(
    xfer: &Arc<Xfer>,
    policy: &Policy,
    full: bool,
    request: &XferRequest,
    lent: Lent,
) -> Result<Ack, String> {
    let slot = Slot::take(xfer).ok_or_else(|| BUSY.to_owned())?;
    xfer.sweep_known(policy, full);
    let plan = match request.direction {
        Direction::Put => {
            let plan = prepare_put(policy, full, request)?;
            xfer.remember(plan.real_dir.clone());
            Plan::Put(plan)
        }
        Direction::Get => Plan::Get(prepare_get(
            policy,
            full,
            request,
            &lent.paused,
            xfer.shelf.as_ref(),
        )?),
    };
    launch(slot, plan, &request.ticket, lent)
}

/// A get of a file on the shelf with no policy at all
/// ([`crate::engine::shelf_get`]): only what the shelf would hand over
/// leaves, opened as the shelf opens it, and it leaves once as any shelf file.
pub(crate) fn start_shelf(
    xfer: &Arc<Xfer>,
    request: &XferRequest,
    lent: Lent,
) -> Result<Ack, String> {
    if request.direction != Direction::Get {
        return Err("verb".to_owned());
    }
    let path = Path::new(&request.path);
    let (shelf, name) = xfer
        .shelf
        .as_ref()
        .and_then(|shelf| shelf.name_of(path).map(|name| (shelf, name)))
        .ok_or_else(|| PathRefusal::Denied.words())?;
    let slot = Slot::take(xfer).ok_or_else(|| BUSY.to_owned())?;
    let (file, shelved) = shelf.open(name)?;
    let plan = plan_get(file, Some(shelved), request, &lent.paused)?;
    launch(slot, Plan::Get(plan), &request.ticket, lent)
}

/// Runs a prepared transfer on its own thread, which holds `slot` until the
/// transfer ends; the control ack at once.
fn launch(slot: Slot, plan: Plan, ticket: &Ticket, lent: Lent) -> Result<Ack, String> {
    let ack = match &plan {
        Plan::Put(put) => Ack::Put { offset: put.offset },
        Plan::Get(get) => Ack::Get {
            size: get.size,
            sha256: get.sha256.clone(),
        },
    };
    let ticket = ticket.clone();
    thread::Builder::new()
        .name("tulpa-xfer".to_owned())
        .spawn(move || {
            let working = Working::hold(lent.working.clone());
            let xfer = Arc::clone(&slot.0);
            let (line, last) = run(&xfer, &lent, &ticket, plan);
            lent.journal.record(&line);
            xfer.end(last);
            drop(slot);
            drop(working);
        })
        .map(drop)
        .map_err(|e| format!("xfer: no thread: {e}"))?;
    Ok(ack)
}

enum Plan {
    Put(PutPlan),
    Get(GetPlan),
}

/// A put into a directory the caller owns outright (the files organ's inbox,
/// #0395): the request's path is only the final name inside it — a single
/// name, never a path (`..` and `/` are refused here too, whatever the
/// caller checked) — and no policy, posture or click applies. The plan is
/// [`plan_put`]'s, against the caller's opened directory.
fn prepare_inbox_put(
    dir: OwnedFd,
    real_dir: PathBuf,
    request: &XferRequest,
    landed: Option<Landed>,
) -> Result<PutPlan, String> {
    if request.size > SIZE_MAX {
        return Err(format!("size: over the {SIZE_MAX}-byte cap"));
    }
    let name = OsString::from(&request.path);
    if Path::new(&name).file_name() != Some(name.as_os_str()) {
        return Err("arg: the inbox takes one name, not a path".to_owned());
    }
    let path = real_dir.join(&name);
    plan_put(
        Target {
            dir,
            real_dir,
            path,
            name,
        },
        request,
        None,
        landed,
    )
}

/// Starts an inbound put into `dir`, a directory the caller opened and owns.
///
/// This is the files organ's inbox (#0395): no policy, no posture, no click.
/// The ack at once says the `.part`'s size, where the bytes resume; the bytes
/// move on the data connection as any put. `landed` runs on the transfer's
/// thread once the file is in place, with its path, size and sha256.
///
/// # Errors
///
/// Busy, a size over the cap, a name that is not one name, no space — said
/// in words safe for the log; the ticket is never echoed.
pub fn start_inbox(
    xfer: &Arc<Xfer>,
    dir: OwnedFd,
    real_dir: PathBuf,
    request: &XferRequest,
    landed: Option<Landed>,
    lent: Lent,
) -> Result<Ack, String> {
    let slot = Slot::take(xfer).ok_or_else(|| BUSY.to_owned())?;
    let plan = prepare_inbox_put(dir, real_dir, request, landed)?;
    launch(slot, Plan::Put(plan), &request.ticket, lent)
}

/// A put ready to receive: the parent directory's descriptor, the names under
/// it, and the open `.part`.
struct PutPlan {
    dir: OwnedFd,
    real_dir: PathBuf,
    name: OsString,
    part_name: OsString,
    /// The `.part` when it exists; a new one is made at the first byte.
    part: Option<File>,
    shown: String,
    size: u64,
    sha256: String,
    offset: u64,
    /// The replaced file's permission bits; `None` for a new name.
    mode: Option<Mode>,
    received: u64,
    dropped: bool,
    /// Told once the file is in place (puts into a caller-owned directory).
    landed: Option<Landed>,
}

/// A get ready to send: the open file and what the ack said about it.
struct GetPlan {
    file: File,
    shown: String,
    size: u64,
    modified: SystemTime,
    sha256: String,
    offset: u64,
    /// Where the file lies when it came from the shelf.
    shelved: Option<Shelved>,
}

/// The parent directory of a requested path, opened (symlinks followed: its
/// real path is what gets checked), and the last name.
struct Target {
    dir: OwnedFd,
    real_dir: PathBuf,
    path: PathBuf,
    name: OsString,
}

fn open_target(policy: &Policy, full: bool, requested: &str) -> Result<Target, String> {
    let path = policy.expand(requested);
    if requested.contains('\0') || !path.is_absolute() {
        return Err(PathRefusal::Shape.words());
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(PathRefusal::Shape.words());
    };
    if full {
        // As the full-mode `write`: missing parents are made.
        std::fs::create_dir_all(parent).map_err(|e| PathRefusal::Open(e.to_string()).words())?;
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir = rustix::fs::open(parent, flags, Mode::empty())
        .map_err(|e| PathRefusal::Open(e.to_string()).words())?;
    let real_dir = fsx::fd_path(&dir).map_err(|e| PathRefusal::Open(e.to_string()).words())?;
    if !full {
        if policy.denied(&path) || policy.denied(&real_dir.join(name)) {
            return Err(PathRefusal::Denied.words());
        }
        if !policy.inside_roots(&real_dir) {
            return Err(PathRefusal::Outside.words());
        }
    }
    let name = name.to_os_string();
    Ok(Target {
        dir,
        real_dir,
        path,
        name,
    })
}

fn prepare_put(policy: &Policy, full: bool, request: &XferRequest) -> Result<PutPlan, String> {
    if request.size > SIZE_MAX {
        return Err(format!("size: over the {SIZE_MAX}-byte cap"));
    }
    let target = open_target(policy, full, &request.path)?;
    plan_put(target, request, (!full).then_some(policy), None)
}

/// A put's plan against an opened target. `policy`, when given, is only the
/// deny list over the `.part`'s names (the roots were the opening's to
/// check); a directory the caller owns outright (the files organ's inbox,
/// #0395) needs none — the directory is the policy. `landed` is told when
/// the file is in place.
fn plan_put(
    target: Target,
    request: &XferRequest,
    policy: Option<&Policy>,
    landed: Option<Landed>,
) -> Result<PutPlan, String> {
    let mut part_name = target.name.clone();
    part_name.push(format!(
        ".{}.part",
        request.sha256.get(..12).unwrap_or_default()
    ));
    if let Some(policy) = policy {
        let requested_part = target.path.with_file_name(&part_name);
        if policy.denied(&requested_part) || policy.denied(&target.real_dir.join(&part_name)) {
            return Err(PathRefusal::Denied.words());
        }
    }
    sweep_dir(&target.dir, SystemTime::now());
    let mode = existing_mode(&target.dir, &target.name)?;
    let part = existing_part(&target.dir, &part_name)?;
    if part.is_none() {
        // Made at the first byte; whether it can be made is known now.
        rustix::fs::accessat(&target.dir, ".", Access::WRITE_OK, AtFlags::empty())
            .map_err(|e| format!("open .part: {e}"))?;
    }
    let mut offset = part
        .as_ref()
        .map(File::metadata)
        .transpose()
        .map_err(|e| format!("open .part: {e}"))?
        .map_or(0, |meta| meta.len());
    if offset > request.size {
        // Longer than the file it is named for: not a prefix of it, start over.
        if let Some(file) = &part {
            file.set_len(0).map_err(|e| format!("open .part: {e}"))?;
        }
        offset = 0;
    }
    check_space(&target.dir, request.size.saturating_sub(offset))?;
    Ok(PutPlan {
        dir: target.dir,
        real_dir: target.real_dir,
        name: target.name,
        part_name,
        part,
        shown: request.path.clone(),
        size: request.size,
        sha256: request.sha256.clone(),
        offset,
        mode,
        received: offset,
        dropped: false,
        landed,
    })
}

/// The permission bits of a regular file at the final name (never set-id or
/// sticky), which the finished file keeps; `None` for a new name. Read with
/// `fstatat` and no open. A directory there is refused now, not after the bytes.
fn existing_mode(dir: &OwnedFd, name: &OsStr) -> Result<Option<Mode>, String> {
    let Ok(stat) = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return Ok(None);
    };
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::Directory => Err(PathRefusal::Kind("regular file").words()),
        FileType::RegularFile => Ok(Some(
            Mode::from_raw_mode(stat.st_mode) & (Mode::RWXU | Mode::RWXG | Mode::RWXO),
        )),
        _ => Ok(None),
    }
}

/// The `.part` already under `dir`, if any: never through a symlink, and only
/// a regular file with one link.
fn existing_part(dir: &OwnedFd, part_name: &OsStr) -> Result<Option<File>, String> {
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat(dir, part_name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(format!("open .part: {e}")),
    };
    let part = File::from(fd);
    let meta = part.metadata().map_err(|e| format!("open .part: {e}"))?;
    if !meta.is_file() {
        return Err("open .part: not a regular file".to_owned());
    }
    if meta.nlink() != 1 {
        return Err("open .part: it has other links".to_owned());
    }
    Ok(Some(part))
}

/// The plan's `.part`, made now (`O_EXCL`, never through a symlink) when the
/// first byte arrives; mode `0666` less the umask, as the hands' `write`.
/// A new one is marked with [`PART_MARK`], so the sweep knows it as ours.
fn part_file(plan: &mut PutPlan) -> Result<&mut File, Stop> {
    if plan.part.is_none() {
        let flags = OFlags::RDWR
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC
            | OFlags::CREATE
            | OFlags::EXCL;
        let mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH;
        let fd = rustix::fs::openat(&plan.dir, &plan.part_name, flags, mode)
            .map_err(|e| Stop::Failed(format!("open .part: {e}")))?;
        // Without extended attributes it stays unmarked: never swept, which is safe.
        let _marked: rustix::io::Result<()> = rustix::fs::fsetxattr(
            &fd,
            PART_MARK,
            plan.part_name.as_bytes(),
            XattrFlags::empty(),
        );
        plan.part = Some(File::from(fd));
    }
    plan.part
        .as_mut()
        .ok_or_else(|| Stop::Failed("open .part: gone".to_owned()))
}

/// The receiver checks free space (the bytes still to come plus a margin)
/// before accepting.
fn check_space(dir: &OwnedFd, remaining: u64) -> Result<(), String> {
    let stat = rustix::fs::fstatvfs(dir).map_err(|e| format!("no space check: {e}"))?;
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    if space_ok(free, remaining) {
        Ok(())
    } else {
        Err(format!(
            "no space: {remaining} bytes and a {SPACE_MARGIN}-byte margin needed, {free} free"
        ))
    }
}

fn space_ok(free: u64, remaining: u64) -> bool {
    remaining
        .checked_add(SPACE_MARGIN)
        .is_some_and(|need| free >= need)
}

fn prepare_get(
    policy: &Policy,
    full: bool,
    request: &XferRequest,
    paused: &tulpa_stop::Stop,
    shelf: Option<&Shelf>,
) -> Result<GetPlan, String> {
    let path = policy.expand(&request.path);
    let on_shelf = shelf.and_then(|shelf| shelf.name_of(&path).map(|name| (shelf, name)));
    let (file, shelved) = if let Some((shelf, name)) = on_shelf {
        // The shelf decides in every mode, so its file is handed over once.
        let (file, shelved) = shelf.open(name)?;
        (file, Some(shelved))
    } else if full {
        if request.path.contains('\0') || !path.is_absolute() {
            return Err(PathRefusal::Shape.words());
        }
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = File::from(
            rustix::fs::open(&path, flags, Mode::empty())
                .map_err(|e| PathRefusal::Open(e.to_string()).words())?,
        );
        (file, None)
    } else {
        let file = File::from(
            fsx::open_checked(policy, &request.path, false)
                .map_err(|e| e.words())?
                .0,
        );
        (file, None)
    };
    plan_get(file, shelved, request, paused)
}

/// A get's plan for its open file: within the size cap, the offset inside
/// it, and the sha256 of the whole file, its size and mtime unchanged
/// meanwhile.
fn plan_get(
    mut file: File,
    shelved: Option<Shelved>,
    request: &XferRequest,
    paused: &tulpa_stop::Stop,
) -> Result<GetPlan, String> {
    let (size, modified) = shape_of(&file)?;
    if size > SIZE_MAX {
        return Err(format!("size: over the {SIZE_MAX}-byte cap"));
    }
    if request.offset > size {
        return Err(format!(
            "offset: {} is past the end ({size})",
            request.offset
        ));
    }
    let hasher = hash_from_start(&mut file, size, paused).map_err(|stop| stop.words())?;
    if shape_of(&file)? != (size, modified) {
        return Err("changed while its sha256 was computed".to_owned());
    }
    Ok(GetPlan {
        file,
        shown: request.path.clone(),
        size,
        modified,
        sha256: hex(&hasher.finalize()),
        offset: request.offset,
        shelved,
    })
}

/// A regular file's size and mtime, from its descriptor.
fn shape_of(file: &File) -> Result<(u64, SystemTime), String> {
    let meta = file.metadata().map_err(|e| format!("stat: {e}"))?;
    if !meta.is_file() {
        return Err(PathRefusal::Kind("regular file").words());
    }
    let modified = meta.modified().map_err(|e| format!("stat: {e}"))?;
    Ok((meta.len(), modified))
}

/// Hashes the first `len` bytes of `file`, from its start; the file is left
/// positioned at `len`. Pause ends it.
fn hash_from_start(file: &mut File, len: u64, paused: &tulpa_stop::Stop) -> Result<Sha256, Stop> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| Stop::Failed(format!("read: {e}")))?;
    let mut hasher = Sha256::new();
    let mut reader = file.take(len);
    let mut buf = vec![0_u8; CHUNK];
    let mut seen: u64 = 0;
    loop {
        // Looked at between reads, not waited on: a blocking `read` of a file cannot be woken (a stuck volume or a
        // fifo holds it until it returns), so Pause takes effect once the read in progress returns.
        if paused.is_set() {
            return Err(Stop::Paused);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Stop::Failed(format!("read: {e}"))),
        };
        hasher.update(buf.get(..n).unwrap_or_default());
        seen = seen.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    if seen == len {
        Ok(hasher)
    } else {
        Err(Stop::Failed("the file shrank while it was read".to_owned()))
    }
}

/// Removes from `dir` the `.part` files this program made that are untouched
/// for [`PART_MAX_AGE`]; the count removed. Only our name shape, only regular
/// files, only with [`PART_MARK`] naming them: a user's own file of that shape
/// is never touched.
fn sweep_dir(dir: &OwnedFd, now: SystemTime) -> usize {
    let Ok(entries) = Dir::read_from(dir) else {
        return 0;
    };
    let mut swept: usize = 0;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if is_part_name(name.to_bytes())
            && stale_and_ours(dir, name, now)
            && rustix::fs::unlinkat(dir, name, AtFlags::empty()).is_ok()
        {
            swept = swept.saturating_add(1);
        }
    }
    swept
}

/// A regular file at `name` (opened with `O_NOFOLLOW`, so never a link's
/// target), untouched for [`PART_MAX_AGE`] and marked as ours under this name.
fn stale_and_ours(dir: &OwnedFd, name: &CStr, now: SystemTime) -> bool {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let Ok(fd) = rustix::fs::openat(dir, name, flags, Mode::empty()) else {
        return false;
    };
    let file = File::from(fd);
    let stale = file.metadata().is_ok_and(|meta| {
        meta.is_file()
            && meta
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > PART_MAX_AGE)
    });
    stale && marked(&file, name.to_bytes())
}

/// Whether `file` carries [`PART_MARK`] with exactly `name` as its value.
fn marked(file: &File, name: &[u8]) -> bool {
    // One byte more than the name: a longer value fails or shows its length.
    let mut value = vec![0_u8; name.len().saturating_add(1)];
    rustix::fs::fgetxattr(file, PART_MARK, value.as_mut_slice())
        .is_ok_and(|len| value.get(..len) == Some(name))
}

/// Takes [`PART_MARK`] off a finished `.part`, so the file it becomes never
/// carries it. A mark that is not there, or a filesystem without extended
/// attributes, is fine; any other failure fails the put.
fn unmark(part: &File) -> Result<(), Stop> {
    match rustix::fs::fremovexattr(part, PART_MARK) {
        Err(e) if e != NO_ATTR && e != rustix::io::Errno::NOTSUP => {
            Err(Stop::Failed(format!("unmark .part: {e}")))
        }
        _ => Ok(()),
    }
}

/// `<name>.<12 lowercase hex>.part`, with a non-empty name.
fn is_part_name(name: &[u8]) -> bool {
    let Some(stem) = name.strip_suffix(b".part") else {
        return false;
    };
    let Some(cut) = stem.len().checked_sub(13) else {
        return false;
    };
    let (head, tail) = stem.split_at(cut);
    let Some(hex12) = tail.strip_prefix(b".") else {
        return false;
    };
    !head.is_empty()
        && hex12
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Why a running transfer stopped.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// Pause: the data connection is closed at once.
    Paused,
    /// No byte for this long: closed at once.
    Idle(Duration),
    /// The data connection broke or could not be opened.
    Broken(String),
    /// A check failed; the receiver says why in its verdict.
    Failed(String),
}

impl Stop {
    fn words(&self) -> String {
        match self {
            Self::Paused => "paused".to_owned(),
            Self::Idle(idle) if idle.subsec_millis() == 0 => format!("idle {} s", idle.as_secs()),
            Self::Idle(idle) => format!("idle {} ms", idle.as_millis()),
            Self::Broken(why) | Self::Failed(why) => why.clone(),
        }
    }

    /// Whether the connection is closed without waiting for the peer.
    const fn abrupt(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

/// One read from the data connection, handed over by the pump thread.
enum Chunk {
    Data(Vec<u8>),
    End,
    Broken(String),
}

/// Reads `reader` on its own thread, so a waiting transfer also wakes on
/// Pause and the idle deadline; at most a few chunks are held (backpressure).
fn pump(mut reader: Box<dyn Read + Send>) -> tulpa_stop::Receiver<Chunk> {
    let (tx, rx) = tulpa_stop::sync_channel(4);
    let _spawned: io::Result<thread::JoinHandle<()>> = thread::Builder::new()
        .name("tulpa-xfer-read".to_owned())
        .spawn(move || {
            loop {
                let mut buf = vec![0_u8; CHUNK];
                let chunk = match reader.read(&mut buf) {
                    Ok(0) => Chunk::End,
                    Ok(n) => {
                        buf.truncate(n);
                        Chunk::Data(buf)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Chunk::Broken(format!("data connection: {e}")),
                };
                let last = !matches!(chunk, Chunk::Data(_));
                if tx.send(chunk).is_err() || last {
                    break;
                }
            }
        });
    // A pump that never started (or died) drops its sender: the receiver reads that end as broken.
    rx
}

/// The transfer's view of the world while it waits.
struct Ctl<'a> {
    xfer: &'a Xfer,
    paused: &'a tulpa_stop::Stop,
}

/// The next chunk, unless Pause or the idle limit comes first.
fn wait(
    chunks: &tulpa_stop::Receiver<Chunk>,
    ctl: &Ctl<'_>,
    heard: &mut Instant,
) -> Result<Chunk, Stop> {
    match chunks.recv_until(&[ctl.paused], heard.checked_add(ctl.xfer.idle)) {
        Got::Item(chunk) => {
            *heard = Instant::now();
            Ok(chunk)
        }
        Got::Stopped => Err(Stop::Paused),
        Got::Timeout => Err(Stop::Idle(ctl.xfer.idle)),
        Got::Ended => Ok(Chunk::Broken(
            "data connection: the reader ended".to_owned(),
        )),
    }
}

/// Runs one prepared transfer to its end; the log line and the menu's
/// last-outcome words.
fn run(xfer: &Xfer, lent: &Lent, ticket: &Ticket, plan: Plan) -> (String, String) {
    let started = Instant::now();
    let ctl = Ctl {
        xfer,
        paused: &lent.paused,
    };
    match plan {
        Plan::Put(mut plan) => {
            xfer.begin(
                Direction::Put,
                short_name(&plan.shown),
                plan.offset,
                plan.size,
            );
            let outcome = put(&ctl, ticket, &mut plan);
            let shown = logged_path(&plan.shown);
            let name = short_name(&plan.shown);
            let secs = started.elapsed().as_secs_f64();
            match outcome {
                Ok(()) => (
                    format!(
                        "xfer: put {shown:?}: ok, {} bytes ({} from offset {}) in {secs:.1} s",
                        plan.size,
                        plan.received.saturating_sub(plan.offset),
                        plan.offset
                    ),
                    format!("put {name}: ok"),
                ),
                Err(stop) => {
                    let part = if plan.dropped { "dropped" } else { "kept" };
                    let why = stop.words();
                    (
                        format!(
                            "xfer: put {shown:?}: failed ({why}); .part {part} at {}/{} bytes after {secs:.1} s",
                            plan.received, plan.size
                        ),
                        format!("put {name}: failed ({})", clip(&why, 60)),
                    )
                }
            }
        }
        Plan::Get(mut plan) => {
            let shown = logged_path(&plan.shown);
            let name = short_name(&plan.shown);
            let (size, offset) = (plan.size, plan.offset);
            let shelved = plan.shelved.take();
            let sha256 = plan.sha256.clone();
            xfer.begin(Direction::Get, name.clone(), offset, size);
            let sent = Arc::new(AtomicU64::new(0));
            let outcome = get(&ctl, ticket, plan, &sent);
            let at = offset.saturating_add(sent.load(Ordering::SeqCst));
            let secs = started.elapsed().as_secs_f64();
            match outcome {
                Ok(heard) => {
                    // A shelf file leaves once: when the receiver says it holds this very file.
                    let handed = shelved.map(|shelved| {
                        if heard.names(size, &sha256) {
                            format!("; {}", shelved.hand_over())
                        } else {
                            "; kept on the shelf (the verdict did not name its size and sha256)"
                                .to_owned()
                        }
                    });
                    (
                        format!(
                            "xfer: get {shown:?}: ok, {size} bytes ({} from offset {offset}) in {secs:.1} s{}",
                            size.saturating_sub(offset),
                            handed.unwrap_or_default()
                        ),
                        format!("get {name}: ok"),
                    )
                }
                Err(stop) => {
                    let why = stop.words();
                    (
                        format!(
                            "xfer: get {shown:?}: failed ({why}) at {at}/{size} bytes after {secs:.1} s"
                        ),
                        format!("get {name}: failed ({})", clip(&why, 60)),
                    )
                }
            }
        }
    }
}

/// put: hash what the `.part` holds, dial, take the rest, check, rename; the
/// verdict goes back on the data connection.
fn put(ctl: &Ctl<'_>, ticket: &Ticket, plan: &mut PutPlan) -> Result<(), Stop> {
    let hasher = match plan.part.as_mut() {
        Some(part) => hash_from_start(part, plan.offset, ctl.paused)?,
        None => Sha256::new(),
    };
    let conn = ctl
        .xfer
        .dial
        .dial(ticket, Some(plan.offset))
        .map_err(|e| Stop::Broken(format!("data dial: {e}")))?;
    let DataConn {
        reader,
        mut writer,
        closer,
    } = conn;
    let chunks = pump(reader);
    let received = receive(&chunks, ctl, plan, hasher);
    let verdict = match &received {
        Ok(sha256) => serde_json::json!({"ok": true, "size": plan.size, "sha256": sha256}),
        Err(stop) => serde_json::json!({"ok": false, "err": stop.words()}),
    };
    let mut line = verdict.to_string().into_bytes();
    line.push(b'\n');
    let _said: io::Result<()> = writer
        .write_all(&line)
        .and_then(|()| writer.flush())
        .and_then(|()| writer.half_close());
    match &received {
        Err(stop) if stop.abrupt() => closer.close(),
        _ => closer.linger(LINGER),
    }
    if let Ok(sha256) = &received
        && let Some(landed) = plan.landed.take()
    {
        landed(&plan.real_dir.join(&plan.name), plan.size, sha256);
    }
    received.map(drop)
}

/// Takes exactly the bytes still missing and finishes the put as soon as they
/// are in, without waiting for the sender's EOF; the sha256 of the whole file.
fn receive(
    chunks: &tulpa_stop::Receiver<Chunk>,
    ctl: &Ctl<'_>,
    plan: &mut PutPlan,
    mut hasher: Sha256,
) -> Result<String, Stop> {
    let mut heard = Instant::now();
    while plan.received < plan.size {
        match wait(chunks, ctl, &mut heard)? {
            Chunk::Data(bytes) => {
                let more = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let total = plan
                    .received
                    .checked_add(more)
                    .filter(|total| *total <= plan.size)
                    .ok_or_else(|| {
                        Stop::Failed(format!("over size: more than {} bytes", plan.size))
                    })?;
                part_file(plan)?
                    .write_all(&bytes)
                    .map_err(|e| Stop::Failed(format!("write .part: {e}")))?;
                hasher.update(&bytes);
                plan.received = total;
                ctl.xfer.advance(total);
            }
            Chunk::End => {
                return Err(Stop::Failed(format!(
                    "short: {} of {} bytes",
                    plan.received, plan.size
                )));
            }
            Chunk::Broken(why) => return Err(Stop::Broken(why)),
        }
    }
    finish_put(plan, hasher)
}

/// fsync, sha256, the mark off, the final mode, the rename, the directory's
/// fsync.
fn finish_put(plan: &mut PutPlan, hasher: Sha256) -> Result<String, Stop> {
    let size = plan.size;
    let part = part_file(plan)?;
    // The hash covers exactly the bytes taken: nothing past them may ride along.
    part.set_len(size)
        .map_err(|e| Stop::Failed(format!("write .part: {e}")))?;
    part.sync_all()
        .map_err(|e| Stop::Failed(format!("fsync .part: {e}")))?;
    let sha256 = hex(&hasher.finalize());
    if sha256 != plan.sha256 {
        // Bytes that hash wrong are no prefix to resume from.
        plan.dropped = rustix::fs::unlinkat(&plan.dir, &plan.part_name, AtFlags::empty()).is_ok();
        return Err(Stop::Failed("sha256 mismatch".to_owned()));
    }
    // Before the mode: a read-only mode would forbid taking the mark off.
    if let Some(part) = plan.part.as_ref() {
        unmark(part)?;
    }
    if let (Some(mode), Some(part)) = (plan.mode, plan.part.as_ref()) {
        rustix::fs::fchmod(part, mode).map_err(|e| Stop::Failed(format!("chmod .part: {e}")))?;
    }
    rustix::fs::renameat(&plan.dir, &plan.part_name, &plan.dir, &plan.name)
        .map_err(|e| Stop::Failed(format!("rename: {e}")))?;
    let _synced: rustix::io::Result<()> = rustix::fs::fsync(&plan.dir);
    Ok(sha256)
}

/// get: dial, send from the offset (re-checking the file before the last
/// bytes leave), half-close, read the verdict.
fn get(
    ctl: &Ctl<'_>,
    ticket: &Ticket,
    plan: GetPlan,
    sent: &Arc<AtomicU64>,
) -> Result<Heard, Stop> {
    let conn = ctl
        .xfer
        .dial
        .dial(ticket, None)
        .map_err(|e| Stop::Broken(format!("data dial: {e}")))?;
    let DataConn {
        reader,
        writer,
        closer,
    } = conn;
    let offset = plan.offset;
    let (tell, told) = tulpa_stop::channel();
    let counter = Arc::clone(sent);
    let spawned = thread::Builder::new()
        .name("tulpa-xfer-send".to_owned())
        .spawn(move || {
            let done = send_bytes(plan, writer, &counter, &tell);
            let _told: Result<(), tulpa_stop::Gone> = tell.send(Sending::Done(done));
        });
    if let Err(e) = spawned {
        closer.close();
        return Err(Stop::Broken(format!("xfer: no thread: {e}")));
    }
    if let Err(stop) = supervise_send(&told, ctl, offset, sent) {
        closer.close();
        return Err(stop);
    }
    let verdict = read_verdict(&pump(reader), ctl);
    closer.close();
    verdict
}

/// What the sending thread tells [`supervise_send`].
enum Sending {
    /// More bytes left (the total is in the shared counter).
    Moved,
    /// It ended.
    Done(Result<(), Stop>),
}

/// Watches the sending thread: Pause and the idle limit end it, progress goes
/// to the menu.
fn supervise_send(
    told: &tulpa_stop::Receiver<Sending>,
    ctl: &Ctl<'_>,
    offset: u64,
    sent: &AtomicU64,
) -> Result<(), Stop> {
    let mut moved = Instant::now();
    loop {
        match told.recv_until(&[ctl.paused], moved.checked_add(ctl.xfer.idle)) {
            Got::Item(Sending::Moved) => {
                moved = Instant::now();
                ctl.xfer
                    .advance(offset.saturating_add(sent.load(Ordering::SeqCst)));
            }
            Got::Item(Sending::Done(result)) => return result,
            Got::Stopped => return Err(Stop::Paused),
            Got::Timeout => return Err(Stop::Idle(ctl.xfer.idle)),
            Got::Ended => return Err(Stop::Broken("the sending thread ended".to_owned())),
        }
    }
}

/// The sending thread: the bytes from the offset, the size and mtime checked
/// again before the last bytes leave, then the half-close.
fn send_bytes(
    mut plan: GetPlan,
    mut writer: Box<dyn HalfWrite>,
    sent: &AtomicU64,
    tell: &tulpa_stop::Sender<Sending>,
) -> Result<(), Stop> {
    plan.file
        .seek(SeekFrom::Start(plan.offset))
        .map_err(|e| Stop::Failed(format!("read: {e}")))?;
    let mut left = plan.size.saturating_sub(plan.offset);
    if left == 0 {
        recheck(&plan)?;
    }
    let mut buf = vec![0_u8; CHUNK];
    while left > 0 {
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let n = match plan.file.read(buf.get_mut(..want).unwrap_or_default()) {
            Ok(0) => return Err(Stop::Failed("changed while sending (shorter)".to_owned())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Stop::Failed(format!("read: {e}"))),
        };
        left = left.saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
        if left == 0 {
            recheck(&plan)?;
        }
        for piece in buf.get(..n).unwrap_or_default().chunks(WRITE_PIECE) {
            writer
                .write_all(piece)
                .map_err(|e| Stop::Broken(format!("data connection: {e}")))?;
            sent.fetch_add(u64::try_from(piece.len()).unwrap_or(0), Ordering::SeqCst);
            let _told: Result<(), tulpa_stop::Gone> = tell.send(Sending::Moved);
        }
    }
    writer
        .flush()
        .and_then(|()| writer.half_close())
        .map_err(|e| Stop::Broken(format!("data connection: {e}")))
}

/// At EOF the file must still be what the ack described.
fn recheck(plan: &GetPlan) -> Result<(), Stop> {
    match shape_of(&plan.file) {
        Ok(shape) if shape == (plan.size, plan.modified) => Ok(()),
        Ok(_) => Err(Stop::Failed(
            "changed while sending (size or mtime)".to_owned(),
        )),
        Err(why) => Err(Stop::Failed(why)),
    }
}

#[derive(Deserialize)]
struct Verdict {
    ok: bool,
    #[serde(default)]
    err: Option<String>,
    /// Read as any JSON value: an odd one only fails to name the file, it
    /// never turns an ok verdict into a failed one.
    #[serde(default)]
    size: Option<serde_json::Value>,
    #[serde(default)]
    sha256: Option<serde_json::Value>,
}

/// What an ok verdict said it holds.
#[derive(Debug, Default)]
struct Heard {
    size: Option<u64>,
    sha256: Option<String>,
}

impl Heard {
    /// True when the verdict names exactly this size and sha256.
    fn names(&self, size: u64, sha256: &str) -> bool {
        self.size == Some(size)
            && self
                .sha256
                .as_deref()
                .is_some_and(|said| said.eq_ignore_ascii_case(sha256))
    }
}

/// The receiver's one verdict line after the half-close.
fn read_verdict(chunks: &tulpa_stop::Receiver<Chunk>, ctl: &Ctl<'_>) -> Result<Heard, Stop> {
    let mut line: Vec<u8> = Vec::new();
    let mut heard = Instant::now();
    loop {
        match wait(chunks, ctl, &mut heard)? {
            Chunk::Data(bytes) => {
                line.extend_from_slice(&bytes);
                if let Some(end) = line.iter().position(|b| *b == b'\n') {
                    line.truncate(end);
                    break;
                }
                if line.len() > VERDICT_MAX {
                    return Err(Stop::Broken("verdict: line too long".to_owned()));
                }
            }
            Chunk::End if !line.is_empty() => break,
            Chunk::End => return Err(Stop::Broken("no verdict from the receiver".to_owned())),
            Chunk::Broken(why) => return Err(Stop::Broken(why)),
        }
    }
    match serde_json::from_slice::<Verdict>(&line) {
        Ok(Verdict {
            ok: true,
            size,
            sha256,
            ..
        }) => Ok(Heard {
            size: size.as_ref().and_then(serde_json::Value::as_u64),
            sha256: sha256
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        }),
        Ok(Verdict { err, .. }) => Err(Stop::Failed(format!(
            "the receiver said: {}",
            clip(err.as_deref().unwrap_or("not ok"), ERR_SHOWN_MAX)
        ))),
        Err(_) => Err(Stop::Broken("verdict: not a JSON line".to_owned())),
    }
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut out, b| {
            let _wrote: fmt::Result = write!(out, "{b:02x}");
            out
        },
    )
}

/// The last name of a path, for the menu: printable, at most 60 chars.
fn short_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned());
    clip(&name, 60)
}

/// A path as the log shows it: scrubbed and capped; the caller quotes it with
/// `{:?}`, so a newline in a name cannot start a line of its own.
fn logged_path(path: &str) -> String {
    scrub_secrets(path)
        .unwrap_or_default()
        .chars()
        .take(LOGGED_PATH_MAX)
        .collect()
}

/// At most `max` chars, printable, for the log and the menu.
fn clip(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Megabytes with one decimal, in integer arithmetic.
fn megabytes(bytes: u64) -> String {
    let whole = bytes.checked_div(1_000_000).unwrap_or(0);
    let tenth = bytes
        .checked_rem(1_000_000)
        .and_then(|rest| rest.checked_div(100_000))
        .unwrap_or(0);
    format!("{whole}.{tenth}")
}

#[cfg(test)]
mod tests;
