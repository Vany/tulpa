//! The hands organ behind the tray.
//!
//! The engine is built from the local policy, asks wait in line for the owner
//! (numbered, timestamped, each expiring on its own clock), every request is
//! one line in the activity log, and the tray's Pause/Quit own the flag that
//! refuses verbs and kills a running child (the organ gate).
//!
//! The data stream rides along: [`data_stream`] builds the transfer side the
//! engine serves `xfer.put` / `xfer.get` with, and [`menu_line`] shows the
//! running transfer's byte count next to the hands.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tulpa_hands::{AskAnswer, Asker, DataDial, Engine, Journal, Policy, Shelf, Xfer};
use tulpa_stop::{Attempts, Stop};

use crate::config::{TrayAppConfig, expand_tilde};
use crate::link::{CurrentRoute, SshDataDial};
use crate::log::{ActivityLog, format_utc};
use crate::state::OrganMode;

/// How many questions may wait at once. One more is refused at once, as
/// expired: the link serves one request at a time, so one waits today, and
/// the bound keeps a parallel caller from piling up dialogs.
const WAITING_MAX: usize = 8;

/// How much of a question the menu and the dialog show, in characters; the
/// log keeps the whole request.
const SHOWN_MAX_CHARS: usize = 400;

/// A question waiting for the owner, as the menu and the dialog show it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Asked {
    /// The question's own number. An answer names it, so a click meant for
    /// one question never answers another.
    pub id: u64,
    /// What the agent asks, fit to show: no control or direction-changing
    /// characters, and cut after 400 characters.
    pub what: String,
    /// When it was asked, in UTC as the log writes it.
    pub at: String,
    /// When the engine stops waiting and refuses it as expired.
    pub deadline: Instant,
    /// How many questions wait behind it.
    pub behind: usize,
}

impl Asked {
    /// The menu line: the question, the time it was asked, and how many wait
    /// behind it.
    #[must_use]
    pub fn line(&self) -> String {
        let line = format!("The agent asks ({}): {}", self.at, self.what);
        match self.behind {
            0 => line,
            more => format!("{line} · {more} more waiting"),
        }
    }
}

/// `what` fit for the menu and the dialog. A control character (a newline
/// that would fake a line of its own) or one that reorders text (right-to-left
/// overrides) shows as `�`, and a long text is cut with the count it lost.
fn fit_to_show(what: &str) -> String {
    let shown: String = what
        .chars()
        .take(SHOWN_MAX_CHARS)
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
                )
            {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    match what.chars().count().saturating_sub(SHOWN_MAX_CHARS) {
        0 => shown,
        cut => format!("{shown}… ({cut} more characters)"),
    }
}

/// One question waiting for a click.
#[derive(Debug)]
struct Pending {
    id: u64,
    what: String,
    at: String,
    deadline: Instant,
    reply: mpsc::Sender<AskAnswer>,
}

/// Holds the questions waiting for the owner, in the order asked, and tells
/// the presenter when they change.
///
/// The first in line is the one the dialog and the menu show; the others
/// wait their turn, each on its own clock. Every question has its own number
/// and an answer names it: a click meant for a question that has just
/// expired answers nothing, never the next one (fail-closed).
pub struct AskBox {
    waiting: Mutex<VecDeque<Pending>>,
    changed: Box<dyn Fn() + Send + Sync>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for AskBox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AskBox")
            .field("first", &self.head())
            .finish_non_exhaustive()
    }
}

impl AskBox {
    /// An empty box; `changed` runs (on the asking or answering thread)
    /// whenever a question appears or goes, so the presenter can re-render.
    #[must_use]
    pub fn new(changed: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            waiting: Mutex::new(VecDeque::new()),
            changed,
            next_id: AtomicU64::new(0),
        }
    }

    /// The question first in line: the one the dialog and the menu show.
    #[must_use]
    pub fn head(&self) -> Option<Asked> {
        let waiting = self.waiting.lock().ok()?;
        let first = waiting.front()?;
        Some(Asked {
            id: first.id,
            what: first.what.clone(),
            at: first.at.clone(),
            deadline: first.deadline,
            behind: waiting.len().saturating_sub(1),
        })
    }

    /// The menu line for the question first in line.
    #[must_use]
    pub fn pending_line(&self) -> Option<String> {
        self.head().map(|asked| asked.line())
    }

    /// The owner's click, in the dialog or the menu: answers question `id`.
    ///
    /// Returns whether its asker gets this answer. A question answered
    /// already, denied by a pause or expired is gone, and the click answers
    /// nothing. The answer is sent while the line is held, and a question out
    /// of time leaves the line under that same lock ([`AskBox::time_up`]): a
    /// click in the moment its question expires either reaches it or answers
    /// nothing, never taken and then dropped.
    pub fn answer(&self, id: u64, answer: AskAnswer) -> bool {
        let answered = self.waiting.lock().is_ok_and(|mut waiting| {
            waiting
                .iter()
                .position(|pending| pending.id == id)
                .and_then(|place| waiting.remove(place))
                .is_some_and(|pending| pending.reply.send(answer).is_ok())
        });
        (self.changed)();
        answered
    }

    /// Denies every waiting question: Pause, Quit and a termination signal.
    pub fn deny_all(&self) {
        // Sent while the line is held, as an answer is.
        if let Ok(mut waiting) = self.waiting.lock() {
            for pending in waiting.drain(..) {
                let _sent: Result<(), mpsc::SendError<AskAnswer>> =
                    pending.reply.send(AskAnswer::Deny);
            }
        }
        (self.changed)();
    }

    /// The answer to question `id` once its time is up: it leaves the line
    /// now, by its own number, so the questions behind it stay, and is
    /// refused as expired. An answer that took it out of the line first was
    /// sent while the line was held, so it waits in `answers`: that one is
    /// the answer, as the click was told.
    fn time_up(&self, id: u64, answers: &mpsc::Receiver<AskAnswer>) -> AskAnswer {
        let Ok(mut waiting) = self.waiting.lock() else {
            return AskAnswer::Expired;
        };
        let place = waiting.iter().position(|pending| pending.id == id);
        if place.and_then(|place| waiting.remove(place)).is_some() {
            return AskAnswer::Expired;
        }
        answers.try_recv().unwrap_or(AskAnswer::Expired)
    }
}

impl Asker for AskBox {
    fn ask(&self, what: &str, timeout: Duration) -> AskAnswer {
        let (reply, answers) = mpsc::channel();
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return AskAnswer::Expired;
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        {
            let Ok(mut waiting) = self.waiting.lock() else {
                return AskAnswer::Expired;
            };
            if waiting.len() >= WAITING_MAX {
                return AskAnswer::Expired;
            }
            waiting.push_back(Pending {
                id,
                what: fit_to_show(what),
                at: format_utc(SystemTime::now()).unwrap_or_default(),
                deadline,
                reply,
            });
        }
        (self.changed)();
        // An answered question left the line already; one out of time leaves
        // it now.
        let answer = answers
            .recv_timeout(timeout)
            .unwrap_or_else(|_| self.time_up(id, &answers));
        (self.changed)();
        answer
    }
}

/// The activity log as the hands' journal.
#[derive(Debug)]
pub struct LogJournal(Mutex<ActivityLog>);

impl LogJournal {
    /// Appends to the same file the tray logs to (append mode, line at a time).
    ///
    /// # Errors
    ///
    /// Returns an error when the log can't be opened.
    pub fn open(path: &Path) -> Result<Self, crate::log::LogError> {
        ActivityLog::open(path).map(|log| Self(Mutex::new(log)))
    }
}

impl Journal for LogJournal {
    fn record(&self, line: &str) {
        if let Ok(mut log) = self.0.lock() {
            let _noted: Result<(), crate::log::LogError> = log.note(line);
        }
    }
}

/// What the hands line in the menu says.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandsState {
    /// Off in config: every line refused.
    Off,
    /// No usable policy: every line refused; the reason is logged.
    NoPolicy,
    /// Serving this many roots.
    Ready(usize),
}

impl HandsState {
    /// The menu line, `paused` taken into account.
    #[must_use]
    pub fn line(&self, paused: bool) -> String {
        match (self, paused) {
            (Self::Off, _) => "hands: off".to_owned(),
            (Self::NoPolicy, _) => "hands: off (no usable policy, see log)".to_owned(),
            (Self::Ready(_), true) => "hands: paused".to_owned(),
            (Self::Ready(roots), false) => format!("hands: on ({roots} roots)"),
        }
    }
}

/// The hands line: the state, how many requests run now and, while one runs
/// or after it ended, the data stream's transfer with the Mac's byte count.
#[must_use]
pub fn menu_line(
    state: &HandsState,
    paused: bool,
    in_flight: usize,
    xfer: Option<&Xfer>,
) -> String {
    let line = format!("{} · running: {in_flight}", state.line(paused));
    match xfer.and_then(Xfer::line) {
        Some(transfer) => format!("{line} · {transfer}"),
        None => line,
    }
}

/// The data stream behind the hands, when the link is configured.
///
/// Each transfer dials a second `ssh -W` child with the link's dial spec on
/// the route `route` holds; `changed` re-renders the menu's transfer line.
/// `shelf`, the eyes' photos while they are on, is handed over on `get`.
#[must_use]
pub fn data_stream(
    config: &TrayAppConfig,
    route: &CurrentRoute,
    changed: Box<dyn Fn() + Send + Sync>,
    shelf: Option<Shelf>,
) -> Option<Arc<Xfer>> {
    let spec = config.dial_spec().ok().flatten()?;
    let dial: Arc<dyn DataDial> =
        Arc::new(SshDataDial::new(spec, config.log_path(), Arc::clone(route)));
    let stream = Xfer::new(dial, changed);
    Some(Arc::new(match shelf {
        Some(photos) => stream.with_shelf(photos),
        None => stream,
    }))
}

/// Writes the menu's full-mode choice into the hands policy file.
///
/// A restart must not silently undo it. The `full = …` line is replaced, or added at the top;
/// the rest of the file and its mode are kept.
///
/// # Errors
///
/// Returns the error of reading or writing the policy file.
pub fn persist_full(path: &Path, full: bool) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let line = format!("full = {full}");
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| {
            let key = l.split('=').next().unwrap_or_default().trim();
            if key == "full" && !l.trim_start().starts_with('#') {
                found = true;
                line.clone()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !found {
        lines.insert(0, line);
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, lines.join("\n") + "\n")?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, path)
}

/// Builds the engine when hands are on in config and the policy loads;
/// otherwise says why not.
///
/// `paused` is the tray's flag; `link` is the link's attempts (the death of
/// the attempt a request came on kills its running child); `xfer`, when given, serves the data
/// stream's verbs.
pub fn build(
    config: &TrayAppConfig,
    asker: Arc<AskBox>,
    journal: Arc<LogJournal>,
    paused: Arc<Stop>,
    link: Arc<Attempts>,
    full: Arc<AtomicBool>,
    xfer: Option<Arc<Xfer>>,
) -> (HandsState, Option<Engine>) {
    if matches!(config.hands.posture(), Ok(OrganMode::Off) | Err(_)) {
        return (HandsState::Off, None);
    }
    let Some(policy_path) = config.hands.policy.as_deref().map(expand_tilde) else {
        journal.record("hands: no policy file named in [hands] policy; refusing every verb");
        return (HandsState::NoPolicy, None);
    };
    let home = expand_tilde("~");
    let loaded = std::fs::read_to_string(&policy_path)
        .map_err(|e| format!("{}: {e}", policy_path.display()))
        .and_then(|text| {
            Policy::from_toml(&text, &home, Some(&policy_path)).map_err(|e| e.to_string())
        });
    match loaded {
        Ok(policy) => {
            let roots = policy.roots().len();
            journal.record(&format!(
                "hands: policy loaded, {roots} roots{}",
                if policy.full() { ", FULL mode" } else { "" }
            ));
            full.store(policy.full(), std::sync::atomic::Ordering::SeqCst);
            let mut engine = Engine::new(policy, asker, journal, paused, link).with_full(full);
            if let Some(stream) = xfer {
                engine = engine.with_xfer(stream);
            }
            (HandsState::Ready(roots), Some(engine))
        }
        Err(why) => {
            journal.record(&format!(
                "hands: policy refused: {why}; refusing every verb"
            ));
            (HandsState::NoPolicy, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    /// Waits up to 5 s for `ready` to hold of the box.
    fn wait_for(ask_box: &AskBox, ready: impl Fn(&AskBox) -> bool) -> bool {
        for _ in 0..500 {
            if ready(ask_box) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Asks `what` on a thread of its own, as the engine does.
    fn asking(
        ask_box: &Arc<AskBox>,
        what: &str,
        timeout: Duration,
    ) -> std::thread::JoinHandle<AskAnswer> {
        let asker = Arc::clone(ask_box);
        let what = what.to_owned();
        std::thread::spawn(move || asker.ask(&what, timeout))
    }

    /// A question waits for its answer or its time; a second waits behind it
    /// instead of being refused, and each answer names its question.
    #[test]
    fn questions_wait_in_line_and_answers_name_them() -> Result<(), Box<dyn std::error::Error>> {
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        let ask_box = Arc::new(AskBox::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));
        assert_eq!(
            ask_box.ask("read x", Duration::from_millis(50)),
            AskAnswer::Expired
        );
        assert!(ask_box.head().is_none(), "an expired question leaves");

        let first = asking(&ask_box, "run `make test` in /w", Duration::from_secs(5));
        assert!(wait_for(&ask_box, |b| b.head().is_some()));
        let second = asking(&ask_box, "read y", Duration::from_secs(5));
        assert!(wait_for(&ask_box, |b| b
            .head()
            .is_some_and(|a| a.behind == 1)));
        let head = ask_box.head().ok_or("a question first in line")?;
        let line = head.line();
        assert!(
            line.starts_with("The agent asks (")
                && line.ends_with("): run `make test` in /w · 1 more waiting"),
            "{line}"
        );
        assert!(ask_box.answer(head.id, AskAnswer::Allow));
        assert_eq!(first.join().ok(), Some(AskAnswer::Allow));
        assert!(
            !ask_box.answer(head.id, AskAnswer::Allow),
            "a second click on an answered question answers nothing"
        );
        let next = ask_box.head().ok_or("the second question, now first")?;
        assert_eq!((next.what.as_str(), next.behind), ("read y", 0));
        assert_ne!(next.id, head.id);
        assert!(ask_box.answer(next.id, AskAnswer::Deny));
        assert_eq!(second.join().ok(), Some(AskAnswer::Deny));
        assert!(ask_box.head().is_none());
        assert!(changes.load(Ordering::SeqCst) >= 8);
        Ok(())
    }

    /// A question that runs out of time as its answer comes gets that answer
    /// or none, and the click is told which; the question behind it is never
    /// answered by it. Both orders of that moment, set by hand.
    #[test]
    fn an_answer_as_time_runs_out_reaches_its_question_or_nothing() {
        let ask_box = AskBox::new(Box::new(|| {}));
        let wait = |id: u64| {
            let (reply, answers) = mpsc::channel();
            if let Ok(mut waiting) = ask_box.waiting.lock() {
                waiting.push_back(Pending {
                    id,
                    what: format!("read {id}"),
                    at: String::new(),
                    deadline: Instant::now(),
                    reply,
                });
            }
            answers
        };
        let first = wait(1);
        let behind = wait(2);
        assert!(
            ask_box.answer(1, AskAnswer::Allow),
            "the click comes first: it takes the question"
        );
        assert_eq!(
            ask_box.time_up(1, &first),
            AskAnswer::Allow,
            "the asker, out of time a moment later, gets that click"
        );

        let third = wait(3);
        assert_eq!(
            ask_box.time_up(2, &behind),
            AskAnswer::Expired,
            "out of time first: refused"
        );
        assert!(
            !ask_box.answer(2, AskAnswer::Allow),
            "the click a moment later answers nothing"
        );
        assert_eq!(ask_box.head().map(|asked| asked.id), Some(3));
        assert!(third.try_recv().is_err(), "nor the question behind");
    }

    /// Pause, Quit and a termination signal deny every waiting question; one
    /// past the bound is refused at once.
    #[test]
    fn deny_all_answers_the_whole_line_and_the_line_is_bounded() {
        let ask_box = Arc::new(AskBox::new(Box::new(|| {})));
        let waiters: Vec<_> = (0..WAITING_MAX)
            .map(|n| asking(&ask_box, &format!("read {n}"), Duration::from_secs(10)))
            .collect();
        assert!(wait_for(&ask_box, |b| {
            b.head()
                .is_some_and(|a| a.behind == WAITING_MAX.saturating_sub(1))
        }));
        let started = Instant::now();
        assert_eq!(
            ask_box.ask("one too many", Duration::from_secs(10)),
            AskAnswer::Expired
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "refused at once"
        );
        ask_box.deny_all();
        for waiter in waiters {
            assert_eq!(waiter.join().ok(), Some(AskAnswer::Deny));
        }
        assert!(ask_box.head().is_none());
    }

    /// The menu and the dialog show no control or reordering characters, and
    /// a long question is cut with the count it lost.
    #[test]
    fn a_question_is_fit_to_show() {
        assert_eq!(
            fit_to_show("read a\nAllow is safe\u{202E}txt.exe"),
            "read a\u{FFFD}Allow is safe\u{FFFD}txt.exe"
        );
        let long = "x".repeat(SHOWN_MAX_CHARS.saturating_add(5));
        let shown = fit_to_show(&long);
        assert!(shown.ends_with("x… (5 more characters)"), "{shown}");
        assert_eq!(shown.chars().filter(|c| *c == 'x').count(), SHOWN_MAX_CHARS);
        assert_eq!(
            fit_to_show("put 3 B into ~/a b.txt"),
            "put 3 B into ~/a b.txt"
        );
    }

    /// The menu shows the transfer next to the hands; nothing before the first.
    #[test]
    fn the_hands_line_carries_the_transfer() -> Result<(), Box<dyn std::error::Error>> {
        let config = TrayAppConfig::from_toml(
            "[link]\nenabled = true\nhost = \"work-box\"\ndevice = \"dev-1\"\ntoken_file = \"/nonexistent/token\"\n",
        )?;
        let route: CurrentRoute = Arc::new(Mutex::new(None));
        let xfer =
            data_stream(&config, &route, Box::new(|| {}), None).ok_or("a configured link")?;
        assert_eq!(
            menu_line(&HandsState::Ready(1), false, 0, Some(&xfer)),
            "hands: on (1 roots) · running: 0",
            "no transfer yet"
        );
        assert_eq!(
            menu_line(&HandsState::Ready(1), true, 2, None),
            "hands: paused · running: 2"
        );
        let off = TrayAppConfig::from_toml("")?;
        assert!(
            data_stream(&off, &route, Box::new(|| {}), None).is_none(),
            "no link, no data stream"
        );
        Ok(())
    }

    #[test]
    fn hands_line_tracks_pause() {
        assert_eq!(HandsState::Ready(2).line(false), "hands: on (2 roots)");
        assert_eq!(HandsState::Ready(2).line(true), "hands: paused");
        assert_eq!(
            HandsState::NoPolicy.line(false),
            "hands: off (no usable policy, see log)"
        );
    }

    /// The menu's full-mode choice lands in the policy file — replaced in place, or added; the
    /// rest kept; the file loads with the new value.
    #[test]
    fn persist_full_rewrites_only_the_full_line() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("hands.toml");
        std::fs::write(&f, "# mine\nroots = []\n# full = true is a comment\n")?;
        persist_full(&f, true)?;
        assert_eq!(
            std::fs::read_to_string(&f)?,
            "full = true\n# mine\nroots = []\n# full = true is a comment\n"
        );
        persist_full(&f, false)?;
        assert_eq!(
            std::fs::read_to_string(&f)?,
            "full = false\n# mine\nroots = []\n# full = true is a comment\n"
        );
        let policy = Policy::from_toml(&std::fs::read_to_string(&f)?, &dir, None)?;
        assert!(!policy.full());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
