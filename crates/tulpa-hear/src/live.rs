//! `--ears`' one way to stop: the end of its stdin or a termination signal.
//!
//! The tray stops the capture by closing the pipe it holds on the capture's stdin and sending SIGTERM to its group;
//! a tray that dies closes the pipe without a signal. Both are the same stop here: `stop` is set, the captures
//! return, each side's phrase in progress and a held turn are written, and the run ends [`Outcome::Ended`]. A side
//! that does not end (a capture stuck in its start) is given up [`GRACE`] after the stop; the tray's own bound, a
//! KILL of the group, comes later than that.
//!
//! Before this the only stop was SIGKILL, which wrote nothing: a held turn and the open phrase were lost on every
//! stop, and a tray that crashed in a quiet room left the capture holding the mic.

use std::io::{self, Read};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tulpa_stop::Stop;

use crate::assemble::Assembler;
use crate::ears::{self, Capture, Ears, GiveUp, Outcome, Sink};
use crate::recognize::Recognizer;
use crate::shadow::{Caught, CaughtSlot};

/// Set (to anything) by a tray that holds the capture's stdin as a pipe: its end is then a stop.
///
/// Without it the stdin is not watched: `/dev/null`, as an older tray or a script gives it, ends at once, and that
/// is no stop.
pub const STDIN_STOP_ENV: &str = "TULPA_EARS_STDIN_STOP";

/// What [`watch_eof`] puts in the slot: the stop's name where a signal's would be.
pub const EOF: &str = "EOF";

/// After the stop, the pipelines are waited for this long ([`ears::FAILED_GRACE`], the one bound of a run that is
/// ending), then given up.
pub const GRACE: Duration = ears::FAILED_GRACE;

/// Reads `input` to its end on a thread of its own, then fills `slot` and sets `stop`.
///
/// The other end's holder is gone, or closed it to stop us. A blocking read is the wait: no poll. What is read is
/// dropped; a read error is an end too.
///
/// # Errors
///
/// The thread's start.
pub fn watch_eof(
    mut input: impl Read + Send + 'static,
    slot: CaughtSlot,
    stop: Arc<Stop>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("tulpa-hear-stdin".to_owned())
        .spawn(move || {
            let mut sink = [0_u8; 256];
            while matches!(input.read(&mut sink), Ok(n) if n > 0) {}
            slot.fill(Caught::new(EOF, Instant::now()));
            stop.set();
        })
}

/// Runs the shared pipeline ([`ears::run`]) until it ends, or until `grace` after a stop lands in `slot`.
///
/// The stop is a signal through [`Signals::spawn`](crate::shadow::Signals::spawn), or the stdin's end through
/// [`watch_eof`]; both also set `stop`.
pub fn run<R, C, S>(
    ears: &Ears<R>,
    capture: &Arc<C>,
    assembler: &mut Assembler,
    sink: &mut S,
    stop: &Arc<Stop>,
    slot: &CaughtSlot,
    grace: Duration,
) -> Outcome
where
    R: Recognizer + Send + 'static,
    C: Capture + 'static,
    S: Sink + ?Sized,
{
    let at = || slot.get().and_then(|c| c.at.checked_add(grace));
    ears::run(
        ears,
        capture,
        assembler,
        sink,
        stop,
        Some(GiveUp::new(&at, slot.came())),
    )
}
