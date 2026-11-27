//! Streamed speech for `voice.say`.
//!
//! The text is cut into sentences so the next one renders while the current one plays, and the
//! synthesis model can stay loaded in one resident child that answers one line per sentence.
//!
//! The resident child talks over its own stdin/stdout: one line of text in, one line out (an absolute
//! WAV path, or `ERR <why>`). Nothing listens on a port; the child dies with Tulpa.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tulpa_hands::exec;
use tulpa_stop::{Got, Stop};

/// A sentence longer than this is cut again at its commas, semicolons, colons or dashes.
pub const LONG_PIECE: usize = 160;
/// A piece shorter than this joins the next one (each piece costs a render's start-up).
pub const SHORT_PIECE: usize = 12;
/// The longest line read from the resident child.
const LINE_MAX: usize = 4096;

/// Cuts `text` into pieces to speak one after another: sentences (ended by `.`, `!`, `?` or `…`
/// followed by a space or the end), long ones cut again at clause marks, short ones joined forward.
#[must_use]
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut sentences = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        current.push(c);
        let ends = matches!(c, '.' | '!' | '?' | '…');
        let next = chars.get(i.saturating_add(1)).copied();
        if ends && next.is_none_or(char::is_whitespace) {
            push_trimmed(&mut sentences, &current);
            current.clear();
        }
    }
    push_trimmed(&mut sentences, &current);
    let mut pieces = Vec::new();
    for sentence in sentences {
        cut_long(&sentence, &mut pieces);
    }
    join_short(pieces)
}

fn push_trimmed(out: &mut Vec<String>, piece: &str) {
    let trimmed = piece.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_owned());
    }
}

/// Cuts a long sentence after clause marks (`, ` `; ` `: ` ` — `), greedily up to [`LONG_PIECE`].
fn cut_long(sentence: &str, out: &mut Vec<String>) {
    if sentence.chars().count() <= LONG_PIECE {
        out.push(sentence.to_owned());
        return;
    }
    let mut clauses = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = sentence.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        current.push(c);
        let next = chars.get(i.saturating_add(1)).copied();
        let mark = matches!(c, ',' | ';' | ':') || (c == '—' && i > 0);
        if mark && next.is_some_and(char::is_whitespace) {
            clauses.push(std::mem::take(&mut current));
        }
    }
    clauses.push(current);
    let mut piece = String::new();
    for clause in clauses {
        if !piece.is_empty()
            && piece.chars().count().saturating_add(clause.chars().count()) > LONG_PIECE
        {
            push_trimmed(out, &piece);
            piece.clear();
        }
        piece.push_str(&clause);
    }
    push_trimmed(out, &piece);
}

fn join_short(pieces: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut carry = String::new();
    for piece in pieces {
        let joined = if carry.is_empty() {
            piece
        } else {
            format!("{carry} {piece}")
        };
        if joined.chars().count() < SHORT_PIECE {
            carry = joined;
        } else {
            carry.clear();
            out.push(joined);
        }
    }
    if !carry.is_empty() {
        match out.last_mut() {
            Some(last) => {
                last.push(' ');
                last.push_str(&carry);
            }
            None => out.push(carry),
        }
    }
    out
}

/// The synthesis model kept loaded: one child, one line in, one line out.
#[derive(Debug)]
pub struct Resident {
    child: Child,
    /// Its process group (it leads its own): every signal goes through it, so none follows the reap.
    group: Arc<exec::Group>,
    stdin: ChildStdin,
    /// The child's stdout lines; its end is the child's exit (or its reader's death).
    lines: tulpa_stop::Receiver<String>,
    answered: bool,
    /// The argv it was started with: another one (a new output device) needs a new child.
    argv: Vec<String>,
}

impl Resident {
    /// Starts `argv` (no shell) with a cleared environment, in `cwd` when given.
    ///
    /// # Errors
    ///
    /// An empty `argv` or a spawn failure.
    pub fn spawn(argv: &[String], cwd: Option<&Path>, home: &Path) -> Result<Self, String> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| "empty voice_serve".to_owned())?;
        let mut command = exec::cleared(Path::new(program), home);
        command
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "voice_serve: no stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "voice_serve: no stdout".to_owned())?;
        let (tx, lines) = tulpa_stop::channel();
        let group = exec::Group::of(&child);
        // Beside the live voice process: a thread the OS refuses kills and reaps it (`Group::spawn_beside`), instead
        // of a panic that drops the `Child` and leaves it running.
        group
            .spawn_beside("tulpa-voice-read", move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            line.truncate(LINE_MAX);
                            if tx.send(line.trim().to_owned()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| format!("voice_serve: no reader thread ({e}); it was killed"))?;
        Ok(Self {
            child,
            group,
            stdin,
            lines,
            answered: false,
            argv: argv.to_vec(),
        })
    }

    /// Whether it was started with exactly `argv`.
    #[must_use]
    pub fn runs(&self, argv: &[String]) -> bool {
        self.argv == argv
    }

    /// Whether the child is still running (it may exit by itself when idle, to free its memory). One that has
    /// exited is reaped here, through its group ([`exec::Group::try_reap`]): what it left in its group is killed
    /// first, and from the reap on [`Resident::kill`] sends nothing (its id may be another process's by then).
    pub fn alive(&mut self) -> bool {
        matches!(self.group.try_reap(&mut self.child), Ok(None))
    }

    /// Whether the child has answered at least once (its model is loaded).
    #[must_use]
    pub const fn warm(&self) -> bool {
        self.answered
    }

    /// Sends one line of text and waits up to `limit` for the WAV path. Lines that are neither a WAV
    /// path nor `ERR …` are skipped (library chatter). Any `Err` leaves the child unusable: drop it.
    ///
    /// # Errors
    ///
    /// `ERR` from the child, its exit, the limit, or Pause.
    pub fn render(
        &mut self,
        text: &str,
        limit: Duration,
        paused: &Stop,
    ) -> Result<PathBuf, String> {
        let one_line: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        writeln!(self.stdin, "{}", one_line.trim())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("voice_serve: write: {e}"))?;
        let deadline = Instant::now().checked_add(limit);
        loop {
            match self.lines.recv_until(&[paused], deadline) {
                Got::Item(line) => {
                    if let Some(why) = line.strip_prefix("ERR") {
                        return Err(format!("voice_serve: {}", why.trim()));
                    }
                    if let Some(wav) = wav_in(&line) {
                        self.answered = true;
                        return Ok(wav);
                    }
                }
                Got::Stopped => return Err("paused".to_owned()),
                Got::Timeout => {
                    return Err(format!("voice_serve: no answer in {}s", limit.as_secs()));
                }
                Got::Ended => return Err("voice_serve: the child exited".to_owned()),
            }
        }
    }
}

/// What a playing resident reports while it speaks (`voice_serve_plays`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Spoken {
    /// Piece `i` started sounding at this Unix time in ms.
    Start(u32, u64),
    /// Piece `i` stopped sounding at this Unix time in ms.
    End(u32, u64),
}

impl Resident {
    /// `voice_serve_plays`: sends the whole text as one line; the child cuts, renders and plays it
    /// itself, reporting `start <i> <unix_ms>` / `end <i> <unix_ms>` per piece and `done …` at the end
    /// (or `ERR <why>`). `on` sees each start and end. Between two lines at most `gap` (the first line
    /// `first`) may pass. Returns whether anything started sounding, with the result.
    ///
    /// # Errors
    ///
    /// `ERR` from the child, its exit, a silent gap, or Pause — then drop the child: that kills it.
    pub fn speak(
        &mut self,
        text: &str,
        first: Duration,
        gap: Duration,
        paused: &Stop,
        mut on: impl FnMut(Spoken),
    ) -> (bool, Result<(), String>) {
        let one_line: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if let Err(e) =
            writeln!(self.stdin, "{}", one_line.trim()).and_then(|()| self.stdin.flush())
        {
            return (false, Err(format!("voice_serve: write: {e}")));
        }
        let mut sounded = false;
        let mut limit = first;
        let mut since = Instant::now();
        loop {
            let line = match self.lines.recv_until(&[paused], since.checked_add(limit)) {
                Got::Item(line) => line,
                Got::Stopped => return (sounded, Err("paused".to_owned())),
                Got::Timeout => {
                    return (
                        sounded,
                        Err(format!("voice_serve: silent for {}s", limit.as_secs())),
                    );
                }
                Got::Ended => {
                    return (sounded, Err("voice_serve: the child exited".to_owned()));
                }
            };
            let mut words = line.split_whitespace();
            match words.next() {
                Some("ERR") => {
                    let why: Vec<&str> = words.collect();
                    return (sounded, Err(format!("voice_serve: {}", why.join(" "))));
                }
                Some("done") => {
                    self.answered = true;
                    return (sounded, Ok(()));
                }
                Some(kind @ ("start" | "end")) => {
                    let i = words.next().and_then(|w| w.parse().ok());
                    let at = words.next().and_then(|w| w.parse().ok());
                    if let (Some(i), Some(at)) = (i, at) {
                        self.answered = true;
                        if kind == "start" {
                            sounded = true;
                            on(Spoken::Start(i, at));
                        } else {
                            on(Spoken::End(i, at));
                        }
                    }
                }
                _ => {}
            }
            limit = gap;
            since = Instant::now();
        }
    }
}

impl Resident {
    /// Kills the child's whole process group (it leads its own), so nothing it started — a writer
    /// thread's buffered audio, a helper — outlives it. A signal only: it never waits (the reap is the drop's).
    /// Nothing once it is reaped ([`exec::Group::kill`]).
    pub fn kill(&mut self) {
        let _sent: bool = self.group.kill();
    }
}

impl Drop for Resident {
    /// Kills the child ([`Resident::kill`]), then reaps it ([`crate::ears::reap_killed`]): a wait, so never under a
    /// lock (`crate::ears`'s `retire`).
    fn drop(&mut self) {
        self.kill();
        crate::ears::reap_killed(&mut self.child);
    }
}

/// The absolute path of an existing `.wav` named in `line` (its last such word).
#[must_use]
pub fn wav_in(line: &str) -> Option<PathBuf> {
    line.split_whitespace()
        .rev()
        .find(|w| {
            Path::new(w)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
        })
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn sentences_split_on_their_ends_not_inside_numbers() {
        assert_eq!(
            split_sentences("Привет, это я. Версия 3.5 готова! Правда? Да…  Конец"),
            vec!["Привет, это я.", "Версия 3.5 готова!", "Правда? Да… Конец",]
        );
        assert_eq!(split_sentences("  "), Vec::<String>::new());
        assert_eq!(split_sentences("Ок."), vec!["Ок."]);
    }

    #[test]
    fn a_long_sentence_is_cut_at_its_clauses() {
        let clause = "слово ".repeat(12);
        let long = format!("{clause}, {clause}; {clause}: {clause}.");
        let pieces = split_sentences(&long);
        assert!(pieces.len() >= 2, "{pieces:?}");
        assert!(
            pieces.iter().all(|p| p.chars().count() <= LONG_PIECE),
            "{pieces:?}"
        );
        let rejoined: String = pieces.join(" ").split_whitespace().collect();
        let original: String = long.split_whitespace().collect();
        assert_eq!(rejoined, original, "nothing lost");
    }

    /// Pause ends a wait for the resident's reply (the old wait looked every 200 ms), rendering and playing alike.
    /// The reply limits are an hour out: a stop at the limit also reads as "paused", so only a wait that Pause woke
    /// ends inside the 60 s guard.
    #[test]
    fn pause_ends_a_wait_on_the_resident_at_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-voicestream-p-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "while read -r line; do sleep 30; done".to_owned(),
        ];
        for playing in [false, true] {
            let paused = std::sync::Arc::new(Stop::new());
            let mut r = Resident::spawn(&argv, None, &dir)?;
            let setter = {
                let paused = std::sync::Arc::clone(&paused);
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(100));
                    let at = Instant::now();
                    paused.set();
                    at
                })
            };
            let long = Duration::from_secs(3600);
            let said = if playing {
                r.speak("x", long, long, &paused, |_| {}).1
            } else {
                r.render("x", long, &paused).map(drop)
            };
            let woke = Instant::now();
            let set_at = setter.join().map_err(|_| "setter")?;
            assert_eq!(said, Err("paused".to_owned()));
            let took = woke.duration_since(set_at);
            assert!(
                took < Duration::from_secs(60),
                "playing {playing}: {took:?}"
            );
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_resident_child_answers_line_by_line_and_skips_chatter()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-voicestream-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let wav = dir.join("c1.wav");
        std::fs::write(&wav, b"")?;
        let script = format!(
            "echo loading model; while read -r line; do case \"$line\" in bad*) echo 'ERR no';; slow*) sleep 5;; *) echo \"{} 0.9s\";; esac; done",
            wav.display()
        );
        let argv = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];
        let paused = Stop::new();
        let mut r = Resident::spawn(&argv, None, &dir)?;
        assert!(!r.warm());
        assert_eq!(r.render("one", Duration::from_secs(5), &paused)?, wav);
        assert!(r.warm());
        assert_eq!(
            r.render("two\nlines", Duration::from_secs(5), &paused)?,
            wav
        );
        assert!(
            r.render("bad", Duration::from_secs(5), &paused)
                .is_err_and(|e| e.contains("no"))
        );
        let mut r = Resident::spawn(&argv, None, &dir)?;
        assert!(
            r.render("slow", Duration::from_millis(500), &paused)
                .is_err_and(|e| e.contains("no answer"))
        );
        paused.set();
        assert!(
            r.render("x", Duration::from_secs(5), &paused)
                .is_err_and(|e| e == "paused")
        );
        drop(r);
        paused.clear();
        // Playing mode: events per piece, then done; Pause kills the whole group (a helper too).
        let helper = dir.join("helper.pid");
        let play = format!(
            "while read -r line; do echo noise; echo start 1 1000; echo end 1 2000; echo start 2 2100; echo end 2 3000; echo done 2.0s; done; sleep 30 & echo $! > {} ; wait",
            helper.display()
        );
        let mut r = Resident::spawn(&["/bin/sh".to_owned(), "-c".to_owned(), play], None, &dir)?;
        let mut seen = Vec::new();
        let (sounded, done) = r.speak(
            "Раз. Два.",
            Duration::from_secs(5),
            Duration::from_secs(5),
            &paused,
            |e| seen.push(e),
        );
        assert!(sounded && done.is_ok(), "{done:?}");
        assert_eq!(
            seen,
            vec![
                Spoken::Start(1, 1000),
                Spoken::End(1, 2000),
                Spoken::Start(2, 2100),
                Spoken::End(2, 3000)
            ]
        );
        let stuck = format!(
            "sleep 30 & echo $! > {}; while read -r line; do echo start 1 5; done",
            helper.display()
        );
        let mut r = Resident::spawn(&["/bin/sh".to_owned(), "-c".to_owned(), stuck], None, &dir)?;
        let (sounded, cut) = r.speak(
            "x",
            Duration::from_secs(5),
            Duration::from_millis(400),
            &paused,
            |_| {},
        );
        assert!(sounded && cut.is_err_and(|e| e.contains("silent")));
        let pid: i32 = std::fs::read_to_string(&helper)?.trim().parse()?;
        drop(r);
        thread::sleep(Duration::from_millis(300));
        let status = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = status
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        assert!(
            status.is_empty() || state == "Z",
            "the helper went with the group: {status}"
        );
        let gone = vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()];
        let mut r = Resident::spawn(&gone, None, &dir)?;
        thread::sleep(Duration::from_millis(300));
        assert!(!r.alive(), "an exited child is seen as gone");
        paused.clear();
        assert!(r.render("x", Duration::from_secs(5), &paused).is_err());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// #0530: no signal after a reap, on the resident synth too. A resident that exits by itself is reaped by the
    /// look that finds it gone (`alive`), through its group: what it left in its group (a helper) is killed at that
    /// exit, while the id is still the leader's, and from the reap on the group sends nothing (a later Pause or
    /// Quit kill could otherwise reach a process that has since taken the id).
    #[test]
    fn an_exited_resident_is_reaped_through_its_group() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("tulpa-voicestream-r-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let helper = dir.join("helper.pid");
        let leaves = format!(
            "sleep 600 </dev/null >/dev/null 2>&1 & echo $! > {}; exit 0",
            helper.display()
        );
        let mut r = Resident::spawn(&["/bin/sh".to_owned(), "-c".to_owned(), leaves], None, &dir)?;
        // The leader's exit, learned without reaping it, bounded.
        let leader = rustix::process::Pid::from_child(&r.child);
        let (exited_tx, exited) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let exit =
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT;
            let _told = exited_tx
                .send(rustix::process::waitid(rustix::process::WaitId::Pid(leader), exit).is_ok());
        });
        assert_eq!(
            exited.recv_timeout(Duration::from_secs(30)),
            Ok(true),
            "the leader never exited"
        );
        let pid = rustix::process::Pid::from_raw(std::fs::read_to_string(&helper)?.trim().parse()?)
            .ok_or("no helper pid")?;
        assert!(
            rustix::process::test_kill_process(pid).is_ok(),
            "the helper lives on past the leader's exit"
        );
        assert!(!r.alive(), "an exited resident is seen as gone");
        // A poll, in the test only: the helper is not this process's child, so its end gives no event here.
        let gone = || {
            rustix::process::test_kill_process(pid).is_err()
                || std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero()))
                    .is_ok_and(|stat| {
                        stat.rsplit(')')
                            .next()
                            .is_some_and(|rest| rest.trim_start().starts_with('Z'))
                    })
        };
        let started = Instant::now();
        while !gone() && started.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(20));
        }
        let left = gone();
        if !left {
            let _killed: rustix::io::Result<()> =
                rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        assert!(left, "the helper outlived its leader's reap");
        assert!(!r.group.kill(), "reaped: nothing more is sent to its group");
        drop(r);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
