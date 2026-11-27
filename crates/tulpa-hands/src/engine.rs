//! One request line in, one reply line out.
//!
//! Wire shape (the phone broker's): `{"id":"7","verb":"read","arg":"~/w/r/README.md"}`
//! → `{"id":"7","ok":true,"result":"…"}` or `{"id":"7","ok":false,"err":"…"}`.
//! Verbs: `ping`, `read <path>`, `list <path>`, `git.status|git.diff|git.log <repo>`,
//! `run <name>`. Anything else is refused with `err: "verb"` and logged.
//!
//! The data stream's `xfer.put` / `xfer.get` are verbs too; their `arg` is an
//! object (see [`crate::xfer`]). [`shelf_get`] answers one `xfer.get` of a
//! file on the data stream's shelf without the hands.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tulpa_stop::{Attempts, Stop};

use crate::REPLY_CAP_BYTES;
use crate::deny;
use crate::exec::{self, Ended};
use crate::fsx;
use crate::policy::{Policy, Posture};
use crate::scrub::scrub_secrets;
use crate::xfer::{self, Ack, Direction, Xfer, XferRequest};

/// git's own reach is cut: no fsmonitor, no hooks, no external
/// diff, no signature check, no pager.
const GIT_SAFETY: &[&str] = &[
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "diff.external=",
    "-c",
    "log.showSignature=false",
    "-c",
    "core.pager=cat",
    "-c",
    "core.untrackedCache=false",
];
/// A git call never runs longer than this.
const GIT_LIMIT: Duration = Duration::from_secs(30);
/// How much of an argument the activity log keeps.
const LOG_ARG_MAX: usize = 200;

/// The answer to a local click request.
///
/// `#[non_exhaustive]` (clippy `exhaustive_enums`): the tray matches on this
/// across the crate line; a new answer must not break its `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AskAnswer {
    /// The owner clicked allow.
    Allow,
    /// The owner clicked deny.
    Deny,
    /// Nobody clicked in time.
    Expired,
}

/// Puts an `ask` in front of the owner and waits for the click (design §2, §4).
pub trait Asker: Send + Sync {
    /// Shows `what` (verb and full argument) and blocks until a click or `timeout`.
    fn ask(&self, what: &str, timeout: Duration) -> AskAnswer;
}

/// The local activity log: one line per request, never the payload.
pub trait Journal: Send + Sync {
    /// Appends one line.
    fn record(&self, line: &str);
}

/// One control line as it arrives: `arg` is text for most verbs, an object
/// for the data stream's.
#[derive(Deserialize)]
struct Line {
    id: String,
    verb: String,
    #[serde(default)]
    arg: Option<serde_json::Value>,
}

struct Request {
    id: String,
    verb: String,
    arg: Option<String>,
}

/// The hands: a policy, a way to ask, a log, the pause flag the tray owns,
/// and the link's attempts, whose current stop a request takes as its link's death.
pub struct Engine {
    policy: Policy,
    asker: Arc<dyn Asker>,
    journal: Arc<dyn Journal>,
    paused: Arc<Stop>,
    link: Arc<Attempts>,
    full: Arc<AtomicBool>,
    working: Option<WorkHook>,
    xfer: Option<Arc<Xfer>>,
}

/// Told `true` when a verb's real work starts and `false` when it ends.
///
/// Only past the policy and the click, never for `ping` or a refusal: the
/// Mac's keep-awake holds on this, and the broker's idle ping every 60 s
/// must not keep a Mac awake for good.
pub type WorkHook = Arc<dyn Fn(bool) + Send + Sync>;

/// Calls the hook with `false` when the work ends, however it ends.
struct WorkGuard<'a>(Option<&'a WorkHook>);

impl Drop for WorkGuard<'_> {
    fn drop(&mut self) {
        if let Some(hook) = self.0 {
            hook(false);
        }
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Engine")
            .field("roots", &self.policy.roots())
            .field("paused", &self.paused.is_set())
            .field("link", &self.link)
            .field("full", &self.full.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// A full-mode `shell` command runs at most this long (a brew install can take a while).
const FULL_SHELL_LIMIT: Duration = Duration::from_secs(3600);

/// PATH for a full-mode shell: the user's, plus the package managers' usual places.
fn full_path() -> String {
    let own = std::env::var("PATH").unwrap_or_default();
    format!(
        "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:{own}:/usr/bin:/bin:/usr/sbin:/sbin"
    )
}

/// What a verb produced before the scrub, or why it was refused.
type Done = Result<String, String>;

impl Engine {
    /// Binds the hands to their policy, asker, log and flags. While `paused`
    /// is true every verb is refused and a running child is killed. A request
    /// takes `link`'s current attempt when it arrives; when that attempt's stop
    /// is set (the link it came on died mid-request) its running child is
    /// killed too, and a later attempt changes nothing about it.
    #[must_use]
    pub fn new(
        policy: Policy,
        asker: Arc<dyn Asker>,
        journal: Arc<dyn Journal>,
        paused: Arc<Stop>,
        link: Arc<Attempts>,
    ) -> Self {
        Self {
            policy,
            asker,
            journal,
            paused,
            link,
            full: Arc::new(AtomicBool::new(false)),
            working: None,
            xfer: None,
        }
    }

    /// Serves the data stream's `xfer.put` / `xfer.get` through `xfer`;
    /// without it both are refused.
    #[must_use]
    pub fn with_xfer(mut self, xfer: Arc<Xfer>) -> Self {
        self.xfer = Some(xfer);
        self
    }

    /// Sets the hook told when a verb's real work starts and ends (see [`WorkHook`]).
    #[must_use]
    pub fn with_working(mut self, hook: WorkHook) -> Self {
        self.working = Some(hook);
        self
    }

    fn working(&self) -> WorkGuard<'_> {
        if let Some(hook) = &self.working {
            hook(true);
        }
        WorkGuard(self.working.as_ref())
    }

    /// Shares the "full hands" switch (for a machine set up through the link alone).
    /// While it is true, `shell <command>` runs any command as the user (`sh -c`, the user's environment
    /// plus the usual package-manager paths), `write <path>\n<content>` writes any file, and `read` /
    /// `list` reach any path — no roots, no named commands, no deny list, no asking. Every request is
    /// still logged; Pause and a dead link still kill a running child.
    #[must_use]
    pub fn with_full(mut self, full: Arc<AtomicBool>) -> Self {
        self.full = full;
        self
    }

    /// Answers one request line with one reply line (newline-terminated).
    #[must_use]
    pub fn handle(&self, line: &[u8]) -> Vec<u8> {
        let Ok(line) = serde_json::from_slice::<Line>(line) else {
            self.journal.record("hands: unparseable line refused");
            return b"{\"err\":\"verb\"}\n".to_vec();
        };
        if !good_id(&line.id) {
            self.journal.record("hands: bad id refused");
            return b"{\"err\":\"verb\"}\n".to_vec();
        }
        if line.verb == "ping" {
            return reply(&line.id, Ok("pong".to_owned()));
        }
        if let Some(direction) = Direction::of_verb(&line.verb) {
            return self.handle_xfer(&line.id, &line.verb, direction, line.arg.as_ref());
        }
        let arg = match line.arg {
            None => None,
            Some(serde_json::Value::String(text)) => Some(text),
            Some(_) => {
                self.journal.record("hands: unparseable line refused");
                return b"{\"err\":\"verb\"}\n".to_vec();
            }
        };
        let request = Request {
            id: line.id,
            verb: line.verb,
            arg,
        };
        let started = Instant::now();
        let (decider, done) = self.decide_and_do(&request);
        let done = done
            .and_then(|text| scrub_secrets(&text).ok_or_else(|| "scrub unavailable".to_owned()));
        let done = done.map(cap);
        let arg = request.arg.as_deref().unwrap_or("");
        // A write's content is a payload: only its path (and length) reaches the log.
        let logged;
        let arg = if request.verb == "write" {
            let (path, content) = arg.split_once('\n').unwrap_or((arg, ""));
            logged = format!("{path} ({} bytes)", content.len());
            logged.as_str()
        } else {
            arg
        };
        let arg = scrub_secrets(arg).unwrap_or_default();
        let arg: String = arg.chars().take(LOG_ARG_MAX).collect();
        let result = match &done {
            Ok(text) => format!("ok bytes={}", text.len()),
            Err(why) => format!("refused ({why})"),
        };
        self.journal.record(&format!(
            "hands: id={} verb={} arg={arg:?} decider={decider} result={result} ms={}",
            request.id,
            request.verb,
            started.elapsed().as_millis()
        ));
        reply(&request.id, done)
    }

    fn decide_and_do(&self, request: &Request) -> (&'static str, Done) {
        // The attempt this request came on, taken before anything waits: its death, not a later attempt's, kills
        // what the request runs.
        let link = self.link.current();
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        let arg = request.arg.as_deref().unwrap_or("");
        if self.full.load(Ordering::SeqCst)
            && matches!(request.verb.as_str(), "shell" | "write" | "read" | "list")
        {
            let _work = self.working();
            return ("full", self.perform_full(&request.verb, arg, &link));
        }
        if matches!(request.verb.as_str(), "shell" | "write") {
            return (
                "policy",
                Err(format!("{}: the hands are not in full mode", request.verb)),
            );
        }
        let (posture, question) = match request.verb.as_str() {
            "run" => match self.policy.command(arg) {
                Some(command) => (
                    command.mode,
                    format!(
                        "run `{}` in {}",
                        command.argv.join(" "),
                        command.cwd.display()
                    ),
                ),
                None => return ("policy", Err("run: not a named command".to_owned())),
            },
            verb => match self.policy.posture(verb) {
                Some(posture) => (posture, format!("{verb} {arg}")),
                None => return ("policy", Err("verb".to_owned())),
            },
        };
        let decider = match posture {
            Posture::Off => return ("policy", Err("off".to_owned())),
            Posture::On => "policy",
            Posture::Ask => match self.asker.ask(&question, self.policy.ask_timeout()) {
                AskAnswer::Allow => "click",
                AskAnswer::Deny => return ("click", Err("denied by the owner".to_owned())),
                AskAnswer::Expired => return ("expired", Err("no click in time".to_owned())),
            },
        };
        // A pause pressed while the question waited wins over the click.
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        let _work = self.working();
        (decider, self.perform(&request.verb, arg, &link))
    }

    /// The data stream's control verbs: the same gate as the others (pause, full
    /// mode or posture and click), then the transfer's own checks; the ack goes
    /// back at once and the bytes move on the data connection. The log line
    /// carries the path and size, never the ticket.
    fn handle_xfer(
        &self,
        id: &str,
        verb: &str,
        direction: Direction,
        arg: Option<&serde_json::Value>,
    ) -> Vec<u8> {
        let started = Instant::now();
        let (logged, decider, done) = match XferRequest::parse(direction, arg) {
            Ok(request) => {
                let (decider, done) = self.decide_xfer(&request);
                (request.logged(), decider, done)
            }
            Err(why) => (String::new(), "policy", Err(why)),
        };
        self.journal.record(&xfer_line(
            "hands", id, verb, &logged, decider, &done, started,
        ));
        xfer_reply(id, done)
    }

    fn decide_xfer(&self, request: &XferRequest) -> (&'static str, Result<Ack, String>) {
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        let Some(stream) = &self.xfer else {
            return ("policy", Err("xfer: no data link".to_owned()));
        };
        let full = self.full.load(Ordering::SeqCst);
        let decider = if full {
            "full"
        } else {
            let verb = match request.direction {
                Direction::Put => "xfer.put",
                Direction::Get => "xfer.get",
            };
            match self.policy.posture(verb).unwrap_or(Posture::Off) {
                Posture::Off => return ("policy", Err("off".to_owned())),
                Posture::On => "policy",
                Posture::Ask => match self
                    .asker
                    .ask(&request.question(), self.policy.ask_timeout())
                {
                    AskAnswer::Allow => "click",
                    AskAnswer::Deny => {
                        return ("click", Err("denied by a click on the Mac".to_owned()));
                    }
                    AskAnswer::Expired => return ("expired", Err("no click in time".to_owned())),
                },
            }
        };
        // A pause pressed while the question waited wins over the click.
        if self.paused.is_set() {
            return ("pause", Err("paused".to_owned()));
        }
        // Hashing a get's file before the ack is real work too; the transfer
        // thread holds its own claim from there.
        let _work = self.working();
        let lent = xfer::Lent {
            paused: Arc::clone(&self.paused),
            journal: Arc::clone(&self.journal),
            working: self.working.clone(),
        };
        (
            decider,
            xfer::start(stream, &self.policy, full, request, lent),
        )
    }

    fn perform(&self, verb: &str, arg: &str, link: &Stop) -> Done {
        match verb {
            "read" => fsx::read_text(&self.policy, arg, REPLY_CAP_BYTES)
                .map(|(text, cut)| {
                    if cut {
                        format!("{text}\n[cut at 256 KB]")
                    } else {
                        text
                    }
                })
                .map_err(|e| e.words()),
            "list" => fsx::list_dir(&self.policy, arg).map_err(|e| e.words()),
            "git.status" | "git.diff" | "git.log" => self.git(verb, arg, link),
            "run" => self.run(arg, link),
            _ => Err("verb".to_owned()),
        }
    }

    /// The full-mode verbs: no policy but the user's own permissions.
    fn perform_full(&self, verb: &str, arg: &str, link: &Stop) -> Done {
        match verb {
            "shell" => {
                if arg.trim().is_empty() {
                    return Err("shell: a command".to_owned());
                }
                let mut command = std::process::Command::new("/bin/sh");
                command
                    .arg("-c")
                    .arg(arg)
                    .current_dir(self.policy.home())
                    .env("PATH", full_path())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                std::os::unix::process::CommandExt::process_group(&mut command, 0);
                let outcome = exec::run(
                    command,
                    &self.paused,
                    link,
                    FULL_SHELL_LIMIT,
                    REPLY_CAP_BYTES,
                )
                .map_err(|e| format!("shell: {e}"))?;
                let mut text = String::from_utf8_lossy(&outcome.stdout).into_owned();
                if !outcome.stderr.is_empty() {
                    text.push_str("\n--- stderr ---\n");
                    text.push_str(&String::from_utf8_lossy(&outcome.stderr));
                }
                let text = with_cut(text, outcome.cut);
                match outcome.ended {
                    Ended::Exit(code) => Ok(format!(
                        "exit {}\n{text}",
                        code.map_or_else(|| "signal".to_owned(), |c| c.to_string())
                    )),
                    Ended::Paused => Err("paused (killed)".to_owned()),
                    Ended::Abandoned => Err("link down (killed)".to_owned()),
                    Ended::Timeout => Err("timeout (killed)".to_owned()),
                }
            }
            "write" => {
                let (path, content) = arg
                    .split_once('\n')
                    .ok_or_else(|| "write: <path>\\n<content>".to_owned())?;
                let path = self.policy.expand(path.trim());
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| format!("write: {}: {e}", dir.display()))?;
                }
                std::fs::write(&path, content)
                    .map_err(|e| format!("write: {}: {e}", path.display()))?;
                Ok(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path.display()
                ))
            }
            "read" => {
                let path = self.policy.expand(arg.trim());
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("read: {}: {e}", path.display()))?;
                let cut = bytes.len() > REPLY_CAP_BYTES;
                let text = String::from_utf8_lossy(bytes.get(..REPLY_CAP_BYTES).unwrap_or(&bytes))
                    .into_owned();
                Ok(with_cut(text, cut))
            }
            _ => {
                let path = self.policy.expand(arg.trim());
                let mut names: Vec<String> = std::fs::read_dir(&path)
                    .map_err(|e| format!("list: {}: {e}", path.display()))?
                    .filter_map(Result::ok)
                    .map(|entry| {
                        let dir = entry.file_type().is_ok_and(|t| t.is_dir());
                        format!(
                            "{}{}",
                            entry.file_name().to_string_lossy(),
                            if dir { "/" } else { "" }
                        )
                    })
                    .collect();
                names.sort();
                Ok(names.join("\n"))
            }
        }
    }

    fn git_command(&self, repo: &Path) -> std::process::Command {
        let mut command = exec::cleared(self.policy.git_program(), self.policy.home());
        command
            .current_dir(repo)
            .env("GIT_DIR", repo.join(".git"))
            .env("GIT_WORK_TREE", repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(GIT_SAFETY);
        command
    }

    fn git(&self, verb: &str, arg: &str, link: &Stop) -> Done {
        let repo = fsx::repo_dir(&self.policy, arg).map_err(|e| e.words())?;
        let dot_git = std::fs::symlink_metadata(repo.join(".git"))
            .map_err(|_| "git: not a repository root".to_owned())?;
        if !dot_git.is_dir() {
            return Err(
                "git: .git is not a directory (worktrees and links are refused)".to_owned(),
            );
        }
        // Filter drivers are code the repo's config names; -c can't switch off
        // drivers it doesn't know by name, so a repo that defines any is refused.
        let mut probe = self.git_command(&repo);
        probe.args(["config", "--local", "--includes", "--name-only", "--list"]);
        let keys = exec::run(probe, &self.paused, link, GIT_LIMIT, REPLY_CAP_BYTES)
            .map_err(|e| format!("git: {e}"))?;
        let keys = String::from_utf8_lossy(&keys.stdout);
        if keys
            .lines()
            .any(|k| k.to_lowercase().starts_with("filter."))
        {
            return Err("git: the repo config defines filter drivers; refused".to_owned());
        }
        let mut command = self.git_command(&repo);
        match verb {
            "git.status" => command.args([
                "status",
                "--porcelain=v1",
                "--branch",
                "--ignore-submodules=all",
            ]),
            "git.diff" => command
                .args([
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--ignore-submodules=all",
                    "--",
                    ".",
                ])
                .args(deny::git_excludes(&repo, self.policy.home())),
            _ => command.args([
                "log",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "-n",
                "50",
                "--date=short",
                "--format=%h %ad %s",
            ]),
        };
        let outcome = exec::run(command, &self.paused, link, GIT_LIMIT, REPLY_CAP_BYTES)
            .map_err(|e| format!("git: {e}"))?;
        match outcome.ended {
            Ended::Exit(Some(0)) => Ok(with_cut(
                String::from_utf8_lossy(&outcome.stdout).into_owned(),
                outcome.cut,
            )),
            Ended::Exit(code) => Err(format!(
                "git exit {code:?}: {}",
                String::from_utf8_lossy(&outcome.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            )),
            Ended::Paused => Err("paused (killed)".to_owned()),
            Ended::Abandoned => Err("link down (killed)".to_owned()),
            Ended::Timeout => Err("timeout (killed)".to_owned()),
        }
    }

    fn run(&self, name: &str, link: &Stop) -> Done {
        let named = self
            .policy
            .command(name)
            .ok_or_else(|| "run: not a named command".to_owned())?;
        let (program, args) = named
            .argv
            .split_first()
            .ok_or_else(|| "run: empty argv".to_owned())?;
        let mut command = exec::cleared(Path::new(program), self.policy.home());
        command.current_dir(&named.cwd).args(args);
        let outcome = exec::run(command, &self.paused, link, named.timeout, REPLY_CAP_BYTES)
            .map_err(|e| format!("run: {e}"))?;
        let mut text = String::from_utf8_lossy(&outcome.stdout).into_owned();
        if !outcome.stderr.is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(&String::from_utf8_lossy(&outcome.stderr));
        }
        let text = with_cut(text, outcome.cut);
        match outcome.ended {
            Ended::Exit(code) => Ok(format!(
                "exit {}\n{text}",
                code.map_or_else(|| "signal".to_owned(), |c| c.to_string())
            )),
            Ended::Paused => Err("paused (killed)".to_owned()),
            Ended::Abandoned => Err("link down (killed)".to_owned()),
            Ended::Timeout => Err("timeout (killed)".to_owned()),
        }
    }
}

fn with_cut(text: String, cut: bool) -> String {
    if cut {
        format!("{text}\n[cut at 256 KB]")
    } else {
        text
    }
}

/// The scrubbed text cut to the reply cap on a char boundary.
fn cap(mut text: String) -> String {
    if text.len() <= REPLY_CAP_BYTES {
        return text;
    }
    let mut end = REPLY_CAP_BYTES.saturating_sub(32);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.truncate(end);
    text.push_str("\n[cut at 256 KB]");
    text
}

/// Answers an `xfer.get` of exactly `path`, a file on the shelf, without the hands.
///
/// The hands' mode, policy and postures do not apply, so it is served with
/// the hands off too. The tray's eyes hand over the photo they just took this
/// way; they decided when they took it. Pause refuses. `None` for any other
/// line, which goes on to the hands. The activity log gets one `shelf:` line,
/// never the ticket.
#[must_use]
pub fn shelf_get(
    stream: &Arc<Xfer>,
    line: &[u8],
    path: &Path,
    paused: &Arc<Stop>,
    journal: &Arc<dyn Journal>,
) -> Option<Vec<u8>> {
    let line = serde_json::from_slice::<Line>(line).ok()?;
    if line.verb != "xfer.get" || !good_id(&line.id) {
        return None;
    }
    let request = XferRequest::parse(Direction::Get, line.arg.as_ref()).ok()?;
    if !request.names(path) || !stream.on_shelf(path) {
        return None;
    }
    let started = Instant::now();
    let (decider, done) = if paused.is_set() {
        ("pause", Err("paused".to_owned()))
    } else {
        let lent = xfer::Lent {
            paused: Arc::clone(paused),
            journal: Arc::clone(journal),
            working: None,
        };
        ("shelf", xfer::start_shelf(stream, &request, lent))
    };
    journal.record(&xfer_line(
        "shelf",
        &line.id,
        &line.verb,
        &request.logged(),
        decider,
        &done,
        started,
    ));
    Some(xfer_reply(&line.id, done))
}

/// A request id the log can carry: 1..=64 characters, none a control
/// character, which could start a line of its own in the log.
fn good_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.chars().any(char::is_control)
}

/// The activity-log line of a data stream request: the path and size or
/// offset, never the ticket.
fn xfer_line(
    who: &str,
    id: &str,
    verb: &str,
    logged: &str,
    decider: &str,
    done: &Result<Ack, String>,
    started: Instant,
) -> String {
    let logged = scrub_secrets(logged).unwrap_or_default();
    let logged: String = logged.chars().take(LOG_ARG_MAX).collect();
    let result = match done {
        Ok(Ack::Put { offset }) => format!("ok offset={offset}"),
        Ok(Ack::Get { size, .. }) => format!("ok size={size}"),
        Err(why) => format!("refused ({why})"),
    };
    format!(
        "{who}: id={id} verb={verb} arg={logged:?} decider={decider} result={result} ms={}",
        started.elapsed().as_millis()
    )
}

/// The data stream's ack: put says where its bytes resume, get says the size
/// and sha256 (flat, and under `result` for a hub that passes only that).
fn xfer_reply(id: &str, done: Result<Ack, String>) -> Vec<u8> {
    let value = match done {
        Ok(Ack::Put { offset }) => json!({"id": id, "ok": true, "result": {"offset": offset}}),
        Ok(Ack::Get { size, sha256 }) => json!({
            "id": id,
            "ok": true,
            "size": size,
            "sha256": sha256,
            "result": {"size": size, "sha256": sha256},
        }),
        Err(err) => json!({"id": id, "ok": false, "err": err}),
    };
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

fn reply(id: &str, done: Done) -> Vec<u8> {
    let value = match done {
        Ok(result) => json!({"id": id, "ok": true, "result": result}),
        Err(err) => json!({"id": id, "ok": false, "err": err}),
    };
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// `value[key]` without the panic path `serde_json`'s `Index` carries
    /// (`indexing_slicing` is denied): a missing key reads as `Null`, so the
    /// assert fails instead of the test panicking.
    fn field<'v>(value: &'v serde_json::Value, key: &str) -> &'v serde_json::Value {
        value.get(key).unwrap_or(&serde_json::Value::Null)
    }

    struct Fixed(AskAnswer, Mutex<Vec<String>>);
    impl Asker for Fixed {
        fn ask(&self, what: &str, _timeout: Duration) -> AskAnswer {
            if let Ok(mut asked) = self.1.lock() {
                asked.push(what.to_owned());
            }
            self.0
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

    struct Rig {
        home: tempfile::TempDir,
        engine: Engine,
        journal: Arc<Lines>,
        asker: Arc<Fixed>,
        paused: Arc<Stop>,
        link: Arc<Attempts>,
    }

    fn rig(answer: AskAnswer, extra: &str) -> Result<Rig, Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let root = home.path().join("proj");
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("notes.txt"),
            "AWS_SECRET_ACCESS_KEY=AKIAABCDEFGHIJKLMNOP\nhello\n", // gitleaks:allow
        )?;
        let text = format!(
            "{extra}\nroots = [\"~/proj\"]\nask_timeout_secs = 1\n[[command]]\nname = \"env\"\nargv = [\"/usr/bin/env\"]\ncwd = \"~/proj\"\nmode = \"on\"\n[[command]]\nname = \"asked\"\nargv = [\"/bin/echo\", \"hi\"]\ncwd = \"~/proj\"\n[[command]]\nname = \"sleepy\"\nargv = [\"/bin/sleep\", \"30\"]\ncwd = \"~/proj\"\nmode = \"on\"\n"
        );
        let policy = Policy::from_toml(&text, home.path(), None)?;
        let journal = Arc::new(Lines::default());
        let asker = Arc::new(Fixed(answer, Mutex::new(Vec::new())));
        let paused = Arc::new(Stop::new());
        let link = Arc::new(Attempts::default());
        // The dyn coercions are written out as typed bindings: argument-position
        // coercion through the generic Arc::clone miscompiles on the pinned
        // 1.97.1 (E0308); assignment-position coercion is exact.
        let asker_dyn: Arc<dyn Asker> = Arc::<Fixed>::clone(&asker);
        let journal_dyn: Arc<dyn Journal> = Arc::<Lines>::clone(&journal);
        let engine = Engine::new(
            policy,
            Arc::clone(&asker_dyn),
            Arc::clone(&journal_dyn),
            Arc::clone(&paused),
            Arc::clone(&link),
        );
        Ok(Rig {
            home,
            engine,
            journal,
            asker,
            paused,
            link,
        })
    }

    fn call(
        engine: &Engine,
        id: &str,
        verb: &str,
        arg: &str,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let line = json!({"id": id, "verb": verb, "arg": arg}).to_string();
        serde_json::from_slice(&engine.handle(line.as_bytes()))
    }

    #[test]
    fn read_is_scrubbed_and_logged_without_payload() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Deny, "")?;
        let out = call(&rig.engine, "1", "read", "~/proj/notes.txt")?;
        assert_eq!(field(&out, "ok"), &json!(true));
        let result = field(&out, "result").as_str().unwrap_or_default();
        assert!(
            result.contains("hello") && !result.contains("AKIA"),
            "{result}"
        );
        let log = rig
            .journal
            .0
            .lock()
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        assert!(
            log.contains("verb=read") && log.contains("decider=policy") && !log.contains("hello"),
            "{log}"
        );
        Ok(())
    }

    #[test]
    fn only_real_work_tells_the_work_hook() -> Result<(), Box<dyn std::error::Error>> {
        // The keep-awake holds on this hook; the broker's idle ping and refusals must not.
        let rig = rig(AskAnswer::Deny, "")?;
        let seen = Arc::new(Mutex::new(Vec::<bool>::new()));
        let sink = Arc::clone(&seen);
        let hook: WorkHook = Arc::new(move |on| {
            if let Ok(mut s) = sink.lock() {
                s.push(on);
            }
        });
        let engine = rig.engine.with_working(hook);
        let seen_now = || seen.lock().map(|s| s.clone()).unwrap_or_default();
        call(&engine, "1", "ping", "")?;
        call(&engine, "2", "shell", "ls")?;
        call(&engine, "3", "run", "rm")?;
        call(&engine, "4", "run", "asked")?;
        assert_eq!(engine.handle(b"not json"), b"{\"err\":\"verb\"}\n".to_vec());
        assert_eq!(
            seen_now(),
            Vec::<bool>::new(),
            "ping, refusals and a denied click hold nothing"
        );
        let out = call(&engine, "5", "read", "~/proj/notes.txt")?;
        assert_eq!(field(&out, "ok"), &json!(true));
        let out = call(&engine, "6", "read", "~/proj/missing.txt")?;
        assert_eq!(field(&out, "ok"), &json!(false));
        assert_eq!(
            seen_now(),
            vec![true, false, true, false],
            "work is held, failed work too, and always let go"
        );
        drop(rig.home);
        Ok(())
    }

    #[test]
    fn verbs_outside_the_policy_are_refused_and_logged() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Allow, "")?;
        for (verb, arg) in [("write", "~/proj/x"), ("run", "rm"), ("shell", "ls")] {
            let out = call(&rig.engine, "2", verb, arg)?;
            assert_eq!(field(&out, "ok"), &json!(false), "{verb}");
        }
        assert_eq!(
            rig.engine.handle(b"not json"),
            b"{\"err\":\"verb\"}\n".to_vec()
        );
        assert_eq!(rig.journal.0.lock().map(|l| l.len()).unwrap_or_default(), 4);
        Ok(())
    }

    #[test]
    fn run_gets_a_cleared_env_and_ask_waits_for_the_click() -> Result<(), Box<dyn std::error::Error>>
    {
        let rig = rig(AskAnswer::Deny, "")?;
        let out = call(&rig.engine, "3", "run", "env")?;
        let result = field(&out, "result").as_str().unwrap_or_default();
        assert!(
            result.starts_with("exit 0\n") && !result.contains("SSH_AUTH_SOCK"),
            "{result}"
        );
        let out = call(&rig.engine, "4", "run", "asked")?;
        assert_eq!(field(&out, "err"), &json!("denied by the owner"));
        let asked = rig.asker.1.lock().map(|a| a.join("|")).unwrap_or_default();
        assert!(asked.starts_with("run `/bin/echo hi` in "), "{asked}");
        let allow = rig_allow()?;
        assert_eq!(
            field(&call(&allow.engine, "5", "run", "asked")?, "result"),
            &json!("exit 0\nhi\n")
        );
        drop(rig.home);
        Ok(())
    }

    fn rig_allow() -> Result<Rig, Box<dyn std::error::Error>> {
        rig(AskAnswer::Allow, "")
    }

    #[test]
    fn a_dropped_link_kills_a_running_child() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Allow, "")?;
        let engine = Arc::new(rig.engine);
        let serving = Arc::clone(&engine);
        let waiter = std::thread::spawn(move || {
            serving.handle(br#"{"id":"9","verb":"run","arg":"sleepy"}"#)
        });
        // The link dies while the child runs: the run must end killed, not at
        // its 3600 s timeout (a review's check, compressed).
        std::thread::sleep(Duration::from_millis(300));
        rig.link.current().set();
        let started = Instant::now();
        let out = serde_json::from_slice::<serde_json::Value>(&waiter.join().unwrap_or_default())?;
        assert_eq!(field(&out, "err"), &json!("link down (killed)"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let log = rig
            .journal
            .0
            .lock()
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        assert!(log.contains("link down (killed)"), "{log}");
        Ok(())
    }

    /// Full mode runs any shell command and writes any file; off (the default) both are refused;
    /// every request is logged, a write's content never.
    #[test]
    fn full_mode_shell_and_write_only_when_on() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Deny, "")?;
        let full = Arc::new(AtomicBool::new(false));
        let engine = rig.engine.with_full(Arc::clone(&full));
        let refused = call(&engine, "1", "shell", "echo hi")?;
        assert_eq!(field(&refused, "ok"), &json!(false), "{refused}");
        full.store(true, Ordering::SeqCst);
        let ran = call(&engine, "2", "shell", "echo hi && echo there >&2")?;
        let text = field(&ran, "result")
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            text.starts_with("exit 0\nhi\n") && text.contains("there"),
            "{text}"
        );
        let target = rig.home.path().join("new/dir/file.txt");
        let wrote = call(
            &engine,
            "3",
            "write",
            &format!("{}\nsecret-content-xyz", target.display()),
        )?;
        assert_eq!(field(&wrote, "ok"), &json!(true), "{wrote}");
        assert_eq!(std::fs::read_to_string(&target)?, "secret-content-xyz");
        let read = call(&engine, "4", "read", &target.display().to_string())?;
        assert_eq!(
            field(&read, "result"),
            &json!("secret-content-xyz"),
            "any path, no roots"
        );
        let log = rig
            .journal
            .0
            .lock()
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        assert!(
            log.contains("verb=shell arg=\"echo hi && echo there >&2\" decider=full"),
            "{log}"
        );
        assert!(
            !log.contains("secret-content-xyz"),
            "a write's content is never logged: {log}"
        );
        Ok(())
    }

    #[test]
    fn paused_refuses_everything_but_ping() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Allow, "")?;
        rig.paused.set();
        assert_eq!(
            field(&call(&rig.engine, "6", "read", "~/proj/notes.txt")?, "err"),
            &json!("paused")
        );
        assert_eq!(
            field(&call(&rig.engine, "7", "ping", "")?, "ok"),
            &json!(true)
        );
        Ok(())
    }

    fn git(repo: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let status = std::process::Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()?;
        if status.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&status.stderr).into())
        }
    }

    #[test]
    fn git_never_runs_repo_code_and_hides_denied_diffs() -> Result<(), Box<dyn std::error::Error>> {
        let rig = rig(AskAnswer::Deny, "git_program = \"/usr/bin/git\"")?;
        let repo = rig.home.path().join("proj");
        git(&repo, &["init", "-q"])?;
        std::fs::write(repo.join("a.txt"), "one\n")?;
        std::fs::write(repo.join(".env"), "X=1\n")?;
        std::fs::write(repo.join(".git-credentials"), "https://u:CRED1@h\n")?;
        std::fs::write(repo.join("vault.kdbx"), "KDBX1\n")?;
        git(&repo, &["add", "."])?;
        git(&repo, &["commit", "-qm", "first"])?;
        std::fs::write(repo.join("a.txt"), "two\n")?;
        std::fs::write(repo.join(".env"), "X=2\n")?;
        std::fs::write(repo.join(".git-credentials"), "https://u:CRED2@h\n")?;
        std::fs::write(repo.join("vault.kdbx"), "KDBX2\n")?;
        // §10 B3 test: fsmonitor and a hook set to a script that leaves a mark.
        let mark = rig.home.path().join("ran");
        let script = rig.home.path().join("evil.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", mark.display()))?;
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        git(
            &repo,
            &["config", "core.fsmonitor", &script.display().to_string()],
        )?;
        git(
            &repo,
            &["config", "diff.external", &script.display().to_string()],
        )?;
        let status = call(&rig.engine, "8", "git.status", "~/proj")?;
        assert_eq!(field(&status, "ok"), &json!(true), "{status}");
        let diff = call(&rig.engine, "9", "git.diff", "~/proj")?;
        let text = field(&diff, "result").as_str().unwrap_or_default();
        assert!(text.contains("+two") && !text.contains("X=2"), "{text}");
        assert!(!text.contains("CRED2") && !text.contains("KDBX2"), "{text}");
        let log = call(&rig.engine, "10", "git.log", "~/proj")?;
        assert!(
            field(&log, "result")
                .as_str()
                .unwrap_or_default()
                .contains("first"),
            "{log}"
        );
        assert!(!mark.exists(), "repo-configured code ran");
        git(
            &repo,
            &["config", "filter.x.clean", &script.display().to_string()],
        )?;
        let refused = call(&rig.engine, "11", "git.status", "~/proj")?;
        assert_eq!(field(&refused, "ok"), &json!(false), "{refused}");
        assert!(!mark.exists());
        Ok(())
    }
}
