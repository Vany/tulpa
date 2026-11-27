//! One stop event for Pause, stop and Quit, so a loop that waits for something else (a child's exit, a line, a
//! deadline) also wakes the moment it is told to stop, instead of waking every few milliseconds to read a flag.
//!
//! - [`Stop`]: a flag that is set and cleared (Pause is lifted again), read without a lock, waited on with a deadline
//!   ([`Stop::wait_until`]), and able to run a waker of the waiter's own on every set ([`Stop::on_set`]): that is how
//!   a wait on something else (a pipe, a channel) is woken by a stop too.
//! - [`channel`] / [`sync_channel`]: an `mpsc` channel whose receiver waits for the next item, a set [`Stop`] or a
//!   deadline, whichever comes first ([`Receiver::recv_until`]). The waker it registers holds the raw channel, so
//!   the channel never reports a disconnect on its own; the producers' end is announced instead, by the last
//!   [`Sender`] to go ([`Got::Ended`], also when a producer thread panics). A hung-up producer is never mistaken for
//!   a live one.
//! - [`Attempts`]: a [`Stop`] per attempt of something retried (the link's dials), set when that attempt ends and
//!   never cleared, so a job reads the end of the attempt it started under and no later attempt can unset it.
//!
//! Standard library only.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// What [`Stop::on_set`] runs on every set. It runs on the setter's thread, so it must not block.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A stop event: set, cleared, read, and waited on. See the crate docs.
#[derive(Default)]
pub struct Stop {
    /// Mirrors `state.set` for [`Stop::is_set`], which takes no lock.
    flag: AtomicBool,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    set: bool,
    next_id: u64,
    wakers: Vec<(u64, Waker)>,
}

impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stop")
            .field("set", &self.is_set())
            .finish_non_exhaustive()
    }
}

impl Stop {
    /// A stop that is not set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            state: Mutex::new(State {
                set: false,
                next_id: 0,
                wakers: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is a flag and a list: a panic elsewhere cannot leave it half-written.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether it is set now.
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Sets it: every [`Stop::wait_until`] returns and every registered waker runs (on this thread, after the lock
    /// is let go). Setting it again while set does nothing.
    pub fn set(&self) {
        let wakers: Vec<Waker> = {
            let mut state = self.lock();
            if state.set {
                return;
            }
            state.set = true;
            self.flag.store(true, Ordering::SeqCst);
            self.changed.notify_all();
            state.wakers.iter().map(|(_, w)| Arc::clone(w)).collect()
        };
        for wake in wakers {
            wake();
        }
    }

    /// Clears it (Pause lifted). Nothing is woken.
    pub fn clear(&self) {
        let mut state = self.lock();
        state.set = false;
        self.flag.store(false, Ordering::SeqCst);
        drop(state);
    }

    /// Sets it when `on` is true, clears it otherwise.
    pub fn set_to(&self, on: bool) {
        if on { self.set() } else { self.clear() }
    }

    /// Waits until it is set or `deadline` passes (`None`: no deadline). Returns whether it is set.
    pub fn wait_until(&self, deadline: Option<Instant>) -> bool {
        let mut state = self.lock();
        loop {
            if state.set {
                return true;
            }
            state = match deadline {
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(at) => {
                    let Some(left) = at
                        .checked_duration_since(Instant::now())
                        .filter(|l| !l.is_zero())
                    else {
                        return false;
                    };
                    self.changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// Runs `wake` on every set from now until the returned guard is dropped; at once too when it is already set,
    /// so a waiter that registers and then blocks cannot miss a set that came in between.
    pub fn on_set(&self, wake: &Waker) -> OnSet<'_> {
        let (id, now) = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            state.wakers.push((id, Arc::clone(wake)));
            (id, state.set)
        };
        if now {
            wake();
        }
        OnSet { stop: self, id }
    }
}

/// A waker registered by [`Stop::on_set`]; dropping it unregisters the waker.
#[must_use = "the waker is unregistered when this is dropped"]
pub struct OnSet<'a> {
    stop: &'a Stop,
    id: u64,
}

impl fmt::Debug for OnSet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnSet").field("id", &self.id).finish()
    }
}

impl Drop for OnSet<'_> {
    fn drop(&mut self) {
        self.stop.lock().wakers.retain(|(id, _)| *id != self.id);
    }
}

/// What the raw channel carries.
enum Msg<T> {
    Item(T),
    /// The last [`Sender`] went.
    End,
    /// A [`Stop`] the receiver waits on was set.
    Wake,
}

/// The raw sending side, unbounded or bounded.
enum Raw<T> {
    Unbounded(mpsc::Sender<Msg<T>>),
    Bounded(mpsc::SyncSender<Msg<T>>),
}

impl<T> Clone for Raw<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Unbounded(tx) => Self::Unbounded(tx.clone()),
            Self::Bounded(tx) => Self::Bounded(tx.clone()),
        }
    }
}

impl<T> Raw<T> {
    /// Sends, waiting for room on a bounded channel; `false` once the receiver is gone.
    fn send(&self, msg: Msg<T>) -> bool {
        match self {
            Self::Unbounded(tx) => tx.send(msg).is_ok(),
            Self::Bounded(tx) => tx.send(msg).is_ok(),
        }
    }

    /// Wakes the receiver without ever blocking: a full bounded channel already has items that wake it, and the
    /// receiver looks at its stops before each wait.
    fn wake(&self) {
        let _woken: bool = match self {
            Self::Unbounded(tx) => tx.send(Msg::Wake).is_ok(),
            Self::Bounded(tx) => {
                !matches!(tx.try_send(Msg::Wake), Err(TrySendError::Disconnected(_)))
            }
        };
    }
}

/// The producers' side: clone it freely; when the last clone goes (also by a panic), the receiver gets
/// [`Got::Ended`].
pub struct Sender<T>(Arc<Last<T>>);

/// Held by every [`Sender`] clone; its drop is the producers' end.
struct Last<T>(Raw<T>);

impl<T> Drop for Last<T> {
    fn drop(&mut self) {
        let _told: bool = self.0.send(Msg::End);
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl<T> Sender<T> {
    /// Sends `item` (waiting for room on a bounded channel).
    ///
    /// # Errors
    ///
    /// [`Gone`] when the receiver is gone; the item is dropped.
    pub fn send(&self, item: T) -> Result<(), Gone> {
        if self.0.0.send(Msg::Item(item)) {
            Ok(())
        } else {
            Err(Gone)
        }
    }
}

/// [`Sender::send`]'s error: the receiver is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Gone;

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the receiver is gone")
    }
}

impl std::error::Error for Gone {}

/// What [`Receiver::recv_until`] got.
#[derive(Debug, PartialEq, Eq)]
#[expect(
    clippy::exhaustive_enums,
    reason = "every waiter handles each case: a new one must not hide behind a wildcard"
)]
pub enum Got<T> {
    /// The next item.
    Item(T),
    /// One of the stops is set (checked before any wait, so a set stop wins over queued items and the end).
    Stopped,
    /// The deadline passed.
    Timeout,
    /// Every [`Sender`] is gone and every item was taken; it stays so (a set stop is still [`Got::Stopped`]).
    Ended,
}

/// The receiving side.
pub struct Receiver<T> {
    rx: mpsc::Receiver<Msg<T>>,
    /// For the stops' wakers: the raw channel, so it never disconnects while the receiver lives; the end comes as
    /// `Msg::End` from the last sender.
    wake: Raw<T>,
    ended: AtomicBool,
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("ended", &self.ended.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> Receiver<T> {
    /// Waits for the next item until one of `stops` is set or `deadline` passes (`None`: no deadline); a stop set by
    /// the deadline or by the producers' end is [`Got::Stopped`], never [`Got::Timeout`] or [`Got::Ended`].
    pub fn recv_until(&self, stops: &[&Stop], deadline: Option<Instant>) -> Got<T> {
        let stopped = || stops.iter().any(|s| s.is_set());
        if stopped() {
            return Got::Stopped;
        }
        if self.ended.load(Ordering::SeqCst) {
            return Got::Ended;
        }
        let wake = self.wake.clone();
        let wake: Waker = Arc::new(move || wake.wake());
        let _watches: Vec<OnSet<'_>> = stops.iter().map(|s| s.on_set(&wake)).collect();
        loop {
            let msg = deadline.map_or_else(
                || self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                |at| {
                    self.rx
                        .recv_timeout(at.saturating_duration_since(Instant::now()))
                },
            );
            match msg {
                Ok(Msg::Item(item)) => return Got::Item(item),
                // A wake left over from an earlier wait (its stop since cleared) is not a stop.
                Ok(Msg::Wake) if !stopped() => {}
                Ok(Msg::Wake) => return Got::Stopped,
                // A stop that lands as the deadline passes is a stop: the caller acts on it, not on the time.
                Err(RecvTimeoutError::Timeout) if stopped() => return Got::Stopped,
                Err(RecvTimeoutError::Timeout) => return Got::Timeout,
                // The receiver holds a raw sender, so a disconnect cannot happen; were it to, it is an end. A stop that
                // lands as the producers end is a stop, as at the deadline.
                Ok(Msg::End) | Err(RecvTimeoutError::Disconnected) => {
                    self.ended.store(true, Ordering::SeqCst);
                    return if stopped() { Got::Stopped } else { Got::Ended };
                }
            }
        }
    }

    /// The next item already queued, without waiting; `None` when there is none (or the producers ended).
    pub fn try_recv(&self) -> Option<T> {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Item(item)) => return Some(item),
                Ok(Msg::Wake) => {}
                Ok(Msg::End) => {
                    self.ended.store(true, Ordering::SeqCst);
                    return None;
                }
                Err(_) => return None,
            }
        }
    }
}

/// An unbounded channel; see [`Receiver::recv_until`].
#[must_use]
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    pair(Raw::Unbounded(tx), rx)
}

/// A channel of at most `bound` queued items: [`Sender::send`] waits for room (backpressure).
#[must_use]
pub fn sync_channel<T>(bound: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::sync_channel(bound);
    pair(Raw::Bounded(tx), rx)
}

fn pair<T>(raw: Raw<T>, rx: mpsc::Receiver<Msg<T>>) -> (Sender<T>, Receiver<T>) {
    let receiver = Receiver {
        rx,
        wake: raw.clone(),
        ended: AtomicBool::new(false),
    };
    (Sender(Arc::new(Last(raw))), receiver)
}

/// A [`Stop`] per attempt of something retried: the link's dials.
///
/// [`Attempts::begin`] makes the next attempt's stop, set by that attempt's owner when it ends and never cleared;
/// [`Attempts::current`] is the stop of the latest attempt, which a job takes when it starts. A job then reads the
/// end of its own attempt: a later attempt neither unsets it (a redial is not a resume) nor sets it (an old
/// attempt's late end is not the new one's). Before the first attempt the current stop is one nothing sets.
#[derive(Debug, Default)]
pub struct Attempts {
    current: Mutex<Arc<Stop>>,
}

impl Attempts {
    /// Starts the next attempt: its own stop, unset, now the current one.
    pub fn begin(&self) -> Arc<Stop> {
        let stop = Arc::new(Stop::new());
        *self.lock() = Arc::clone(&stop);
        stop
    }

    /// The latest attempt's stop.
    #[must_use]
    pub fn current(&self) -> Arc<Stop> {
        Arc::clone(&self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, Arc<Stop>> {
        // One pointer, swapped whole: a panic elsewhere cannot leave it half-written.
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
