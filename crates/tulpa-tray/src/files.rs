//! The files organ behind the tray (#0395): the always-on channel a file
//! arrives over, whatever the hands' mode and whatever is paused.
//!
//! The organ serves two verbs on the control line — `files.put` (a file into
//! the inbox, [`tulpa_files::Inbox`], over the data stream's put machinery
//! [`tulpa_hands::xfer`]: ticket, offset resume, sha256, the atomic rename)
//! and `files.state` (what the inbox and the staging directory hold). A
//! `files.put` is served with the hands off, with no policy loaded, and
//! while anything is paused: upgrades must not wait on the senses' state.
//! There is no posture and no click — the hub that holds the link's key is
//! the only one that asks, and the server's key line is the other half of
//! that bargain (a separate `command=` restriction on the hub, glados's
//! side).
//!
//! Only a name [`tulpa_files::name_ok`] accepts is taken, and it is checked
//! here before anything dials; the hands' own check (one name, not a path)
//! is the second lock. The organ keeps its own data stream ([`stream`]): a
//! bundle of a hundred megabytes never queues behind a hands transfer or a
//! photo fetch.
//!
//! What lands is delivered ([`tulpa_files::Delivery`]): a bundle archive is
//! unpacked, verified and staged as `Tulpa.app-<commit>` where
//! `build-check.sh` installs from — the first consumer of the channel. The
//! outcome is one line in the activity log and one event to the agent
//! (`{"event":"files",…}`); the bundle's own bytes never are. At start the
//! organ clears the debris of a cut unpack and takes whatever the last run
//! left in the inbox, so a restart needs nothing from the sender but the
//! usual re-request — the `.part` on disk answers with its offset.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tulpa_files::{Delivery, Inbox, Taken, name_ok};
use tulpa_hands::Journal;
use tulpa_hands::WorkHook;
use tulpa_hands::Xfer;
use tulpa_hands::xfer::{Ack, Direction, Landed, Lent, XferRequest, start_inbox};

use crate::config::{TrayAppConfig, expand_tilde};
use crate::hands;
use crate::link::{CurrentRoute, LineHandler, Outbox, refuse_line};

/// The app's support directory, the installer's and this organ's ground.
pub const SUPPORT_DIR: &str = "~/Library/Application Support/tulpa";

/// The files organ: the inbox, the delivery, and its own data stream.
pub struct Files {
    inbox: Inbox,
    delivery: Delivery,
    stream: Arc<Xfer>,
    journal: Arc<dyn Journal>,
    working: Option<WorkHook>,
    outbox: Arc<Outbox>,
}

/// The organ says its shape only: the directories and the journal are not
/// for debug output.
impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files").finish_non_exhaustive()
    }
}

/// The files organ's own data stream, when the link is configured: the same
/// dial as the hands', its own busy slot, nothing rendered in the menu.
fn stream(
    config: &TrayAppConfig,
    route: &CurrentRoute,
    journal: &Arc<dyn Journal>,
) -> Option<Arc<Xfer>> {
    let stream = hands::data_stream(config, route, Box::new(|| {}), None)?;
    journal.record("files: channel ready beside the hands' data stream");
    Some(stream)
}

/// Builds the organ when the link is configured, with its one start pass.
///
/// The debris of a cut unpack is cleared, whatever the last run left in the
/// inbox taken again. `None` (no dial spec) is no organ: the channel rides
/// the link, and with no link there is nothing to serve.
pub fn build(
    config: &TrayAppConfig,
    route: &CurrentRoute,
    journal: Arc<dyn Journal>,
    working: Option<WorkHook>,
    outbox: Arc<Outbox>,
) -> Option<Arc<Files>> {
    let support = expand_tilde(SUPPORT_DIR);
    let stream = stream(config, route, &journal)?;
    Some(Files::new(&support, stream, journal, working, outbox))
}

impl Files {
    /// The organ over a support directory and an already-built data stream,
    /// with its one start pass made.
    #[must_use]
    pub fn new(
        support: &Path,
        stream: Arc<Xfer>,
        journal: Arc<dyn Journal>,
        working: Option<WorkHook>,
        outbox: Arc<Outbox>,
    ) -> Arc<Self> {
        let files = Arc::new(Self {
            inbox: Inbox::under(support),
            delivery: Delivery::under(support),
            stream,
            journal,
            working,
            outbox,
        });
        files.start_pass();
        files
    }
}

impl Files {
    /// Serves one `files.*` line; any other line is not this organ's.
    /// The reply carries the request's id, as the data stream's replies do.
    #[must_use]
    pub fn serve(&self, line: &[u8]) -> Vec<u8> {
        #[derive(serde::Deserialize)]
        struct Line {
            verb: String,
            id: String,
            arg: Option<Value>,
        }
        let Ok(parsed) = serde_json::from_slice::<Line>(line) else {
            self.journal.record("files: unparseable line refused");
            return b"{\"err\":\"verb\"}\n".to_vec();
        };
        if !loggable_id(&parsed.id) {
            self.journal.record("files: bad id refused");
            return b"{\"err\":\"verb\"}\n".to_vec();
        }
        let started = Instant::now();
        match parsed.verb.as_str() {
            "files.put" => self.put(&parsed.id, parsed.arg.as_ref(), started),
            "files.state" => self.state(&parsed.id, started),
            _ => refuse_line(line),
        }
    }

    /// One put into the inbox: the name checked here, the directory opened
    /// fresh (never through a symlink), the transfer lent a slot with no
    /// pause to answer. The delivery runs on the transfer's own thread once
    /// the file is in place.
    fn put(&self, id: &str, arg: Option<&Value>, started: Instant) -> Vec<u8> {
        let request = XferRequest::parse(Direction::Put, arg)
            .and_then(|request| {
                // The charset keeps the log safe on its own: no `/`, no dot
                // first, plain ASCII — nothing to scrub.
                if name_ok(request.path()) {
                    Ok(request)
                } else {
                    Err("arg: not a name the inbox takes".to_owned())
                }
            })
            .and_then(|request| {
                let (dir, real) = self.inbox.open()?;
                let landed = self.landed();
                start_inbox(
                    &self.stream,
                    dir,
                    real,
                    &request,
                    Some(landed),
                    Lent::always_on(Arc::clone(&self.journal), self.working.clone()),
                )
            });
        let (result, reply) = match &request {
            Ok(Ack::Put { offset }) => (
                format!("ok offset={offset}"),
                json!({"id": id, "ok": true, "result": {"offset": offset}}),
            ),
            Err(why) => (
                format!("refused ({why})"),
                json!({"id": id, "ok": false, "err": why}),
            ),
            Ok(_) => ("refused (verb)".to_owned(), json!({"id": id, "ok": false})),
        };
        self.journal.record(&format!(
            "files: id={id} verb=files.put result={result} ms={}",
            started.elapsed().as_millis()
        ));
        reply_line(&reply)
    }

    /// What a landed file triggers, on the transfer's thread: the delivery's
    /// take, one activity line, one event to the agent.
    fn landed(&self) -> Landed {
        let delivery = self.delivery.clone();
        let journal = Arc::clone(&self.journal);
        let outbox = Arc::clone(&self.outbox);
        Box::new(move |path: &Path, _size: u64, sha256: &str| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            let short: String = sha256.chars().take(12).collect();
            let (state, said) = match delivery.take(path) {
                Taken::Staged(said) => ("staged", said),
                Taken::Kept(said) => ("kept", said),
                Taken::Failed(said) => ("failed", said),
                _ => ("failed", String::new()),
            };
            journal.record(&format!("files: {name} sha={short} {said}"));
            outbox.push(
                json!({"event": "files", "state": state, "name": name})
                    .to_string()
                    .into_bytes(),
            );
        })
    }

    /// What the two directories hold: the inbox's regular files, the staging
    /// directory as the installer sees it. Names only, never contents.
    fn state(&self, id: &str, started: Instant) -> Vec<u8> {
        let read = |dir: &Path| -> Result<Vec<String>, String> {
            let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let mut names = Vec::new();
            for entry in entries {
                let Ok(entry) = entry else { continue };
                if entry.file_type().is_ok_and(|t| t.is_file())
                    && let Some(name) = entry.file_name().to_str()
                {
                    names.push(name.to_owned());
                }
            }
            names.sort();
            Ok(names)
        };
        let inbox = read(self.inbox.dir());
        let staging = self
            .delivery
            .staging()
            .read_dir()
            .map(|entries| {
                let mut names: Vec<String> = entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();
        let (result, reply) = match inbox {
            Ok(inbox) => (
                format!("ok inbox={} staging={}", inbox.len(), staging.len()),
                json!({"id": id, "ok": true, "result": {"inbox": inbox, "staging": staging}}),
            ),
            Err(why) => (
                format!("refused ({why})"),
                json!({"id": id, "ok": false, "err": why}),
            ),
        };
        self.journal.record(&format!(
            "files: id={id} verb=files.state result={result} ms={}",
            started.elapsed().as_millis()
        ));
        reply_line(&reply)
    }

    /// The one start pass: a cut unpack's debris cleared, then whatever the
    /// inbox holds is taken again — a file that failed delivery retries,
    /// kept files just say so quietly.
    fn start_pass(&self) {
        if let Err(e) = self.delivery.clear_leftovers() {
            self.journal.record(&format!("files: staging sweep: {e}"));
        }
        let Ok(entries) = std::fs::read_dir(self.inbox.dir()) else {
            return; // the first put makes it
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            match self.delivery.take(&path) {
                Taken::Staged(said) => {
                    self.journal.record(&format!("files: start: {said}"));
                }
                Taken::Failed(said) => {
                    self.journal
                        .record(&format!("files: start: {said}; it waits in the inbox"));
                }
                _ => {}
            }
        }
    }
}

/// A request id the log can carry: the hands' engine's own rule (1..=64
/// characters, no control character), applied to this organ's lines too.
fn loggable_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.chars().any(char::is_control)
}

/// One reply value as a newline-terminated line.
fn reply_line(value: &Value) -> Vec<u8> {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// The link's line handler with the files organ in front of `others`.
///
/// `files.*` verbs go to the organ whatever the hands' state (off too, and
/// while paused — the channel is the link's, not the senses'); every other
/// line goes to `others`.
#[must_use]
pub fn route(others: LineHandler, files: Option<Arc<Files>>) -> LineHandler {
    #[derive(serde::Deserialize)]
    struct Verb {
        verb: String,
    }
    Arc::new(move |line: &[u8]| {
        let is_files =
            serde_json::from_slice::<Verb>(line).is_ok_and(|v| v.verb.starts_with("files."));
        match (is_files, &files) {
            (true, Some(files)) => files.serve(line),
            _ => others(line),
        }
    })
}

#[cfg(test)]
mod tests;
