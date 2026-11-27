//! The order of a capture's start: its `Device` only once it runs, and none of its audio before that `Device`.
//!
//! For the mic's and the tap's streams and the mic's voice unit: the `Device` goes out once the start returned with
//! no stream error meanwhile. Platform-free (std and [`Event`]), so the order is tested everywhere; `mic` wires it to
//! cpal's callbacks and the voice unit's on macOS.
//!
//! Both orders matter downstream. A `Device` sent while the start still blocks (a pending System Audio Recording
//! grant) would count the side open and its silence would beat, so a pending grant would look healthy. Audio sent
//! ahead of its `Device` would be counted as the previous device's (`examples/switch_gap.rs`, the rehearsal's gap).
//! So [`start`] sends the `Device` after the start, and [`gated`] drops the device's audio until then: at most the
//! callbacks that run before the start returns, a few ms on a start that does not block.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use tulpa_stop::Stop;

use crate::event::Event;

/// One capture's start, shared by its opener and its callbacks: whether its `Device` is out, and its failure mark.
#[derive(Debug, Default)]
pub(crate) struct Mark {
    /// Orders an error the callback reports with the `Device` and with the stream's retirement: the check of
    /// `failed` and the send of the `Device` happen under it, which the mark alone could not. Held for that check and
    /// one send, never on the audio path. `true` once retired ([`Held`] dropped): the stream is no longer the
    /// side's, and nothing it reports goes out.
    retired: Mutex<bool>,
    /// Whether the `Device` went out and the stream is not retired: stored (`Release`) after the `Device`'s send and
    /// at the retirement, under the lock; the audio callback reads it (`Acquire`) and takes no lock, so it passes
    /// audio only once the `Device` is on the channel and none once the stream is retired.
    said: AtomicBool,
    /// Set when the error callback reports a `StreamError`: the stream will send nothing more and must be opened
    /// again. The follower waits on it.
    failed: Stop,
}

impl Mark {
    /// The failure mark: set when the capture's error callback reported a `StreamError`.
    pub(crate) const fn failure(&self) -> &Stop {
        &self.failed
    }

    /// The stream is no longer the side's (dropped for another device, for none, or after it failed): from now on
    /// its callbacks send nothing, so a late error of a replaced stream never closes the side's current one.
    fn retire(&self) {
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        *retired = true;
        self.said.store(false, Ordering::Release);
        drop(retired);
    }
}

/// A stream's [`Mark`], held by the capture that owns the stream: dropping it retires the mark ([`Mark::retire`]).
/// Hold it ahead of the stream (drop order), so the mark is retired before the stream is torn down.
#[derive(Debug, Default)]
pub(crate) struct Held(Arc<Mark>);

impl Held {
    /// The mark, for the stream's callbacks and its start.
    pub(crate) const fn mark(&self) -> &Arc<Mark> {
        &self.0
    }

    /// The failure mark ([`Mark::failure`]).
    pub(crate) fn failure(&self) -> &Stop {
        self.0.failure()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.retire();
    }
}

/// A running stream and its own [`Mark`]; dropping it retires the mark, then stops the stream.
///
/// The order is [`Drop`]'s doing, not the fields': a value's own `drop` runs before any of its fields is dropped,
/// so the mark is retired before the stream is torn down however the fields are declared, and an error the
/// stream's callback reports during the teardown goes nowhere (the side has moved on).
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        dead_code,
        reason = "built by the macOS-only stream; elsewhere only its drop order is tested"
    )
)]
pub(crate) struct Live<S> {
    mark: Held,
    /// Held so that dropping it stops the stream.
    stream: S,
}

impl<S> Drop for Live<S> {
    fn drop(&mut self) {
        self.mark.mark().retire();
    }
}

#[cfg_attr(
    not(target_os = "macos"),
    expect(
        dead_code,
        reason = "built by the macOS-only stream; elsewhere only its drop order is tested"
    )
)]
impl<S> Live<S> {
    /// `stream`, whose callbacks report through `mark`.
    pub(crate) const fn new(mark: Held, stream: S) -> Self {
        Self { mark, stream }
    }

    /// The stream's mark, for its start.
    pub(crate) const fn mark(&self) -> &Arc<Mark> {
        self.mark.mark()
    }

    /// The stream itself, to start it.
    pub(crate) const fn stream(&self) -> &S {
        &self.stream
    }

    /// The stream's failure mark, set when its error callback reports a `StreamError` (cpal pauses the stream before
    /// it reports a device gone or a stream invalidated): the stream will send nothing more and must be opened
    /// again. The follower waits on it (`follow::drive`); it is this stream's own, and retired once the stream is
    /// dropped, so a late error from a stream already replaced neither sets it nor sends anything.
    pub(crate) fn failure(&self) -> &Stop {
        self.mark.failure()
    }
}

/// What the error callback does with `ev`: a `StreamError` sets the failure mark, and `ev` is sent, both under the
/// lock [`start`] takes to decide on the `Device`, so the two are ordered. The lock is held for the mark and the send
/// only, never across a call into the audio API. A retired stream's report sends and marks nothing: the events
/// channel is the side's, and the side has moved on to another stream.
pub(crate) fn report(events: &Sender<Event>, mark: &Mark, ev: Event) {
    let retired = mark.retired.lock().unwrap_or_else(PoisonError::into_inner);
    if *retired {
        return;
    }
    if matches!(ev, Event::StreamError(_)) {
        mark.failed.set();
    }
    let _sent = events.send(ev);
    drop(retired);
}

/// Starts the stream with `play`, then says it is open ([`say`] the `device`) — only when `play` returned `Ok` and
/// the error callback reported no `StreamError` meanwhile (or since the stream was built: `mark` is the stream's
/// own).
///
/// `play` runs with no lock held: it can block (a pending System Audio Recording grant), and the callback, on the
/// audio thread, must still be able to report. A `play` that fails is a `StreamError`, no `Device`, and its error is
/// returned. A start that blocks or fails sends no `Device`, so the side is not counted open (no silent beat) until
/// the stream really runs.
///
/// # Errors
///
/// `play`'s error; or, as [`say`], that the stream reported an error while starting.
pub(crate) fn start(
    events: &Sender<Event>,
    mark: &Mark,
    device: Event,
    play: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if let Err(e) = play() {
        report(events, mark, Event::StreamError(e.clone()));
        return Err(e);
    }
    say(events, mark, device)
}

/// Sends `device` (an [`Event::Device`]) once the capture runs, and lets its audio through ([`gated`]): under the
/// lock, so an error the callback reports after this check goes out after the `Device`.
///
/// # Errors
///
/// When the stream was marked failed by now, that it reported an error while starting: the stream is dead, no
/// `Device` goes out, and the caller opens it again (no second `StreamError`: the callback sent it).
pub(crate) fn say(events: &Sender<Event>, mark: &Mark, device: Event) -> Result<(), String> {
    let _ordered = mark.retired.lock().unwrap_or_else(PoisonError::into_inner);
    if mark.failed.is_set() {
        let name = match &device {
            Event::Device { name, .. } => name.as_str(),
            _ => "the input",
        };
        return Err(format!(
            "\"{name}\": the stream reported an error while starting"
        ));
    }
    let _sent = events.send(device);
    mark.said.store(true, Ordering::Release);
    Ok(())
}

/// The capture's audio callback, `feed`, passing nothing until [`say`] has sent the `Device`: no audio of a device
/// comes ahead of its `Device`. It runs on the audio thread and takes no lock: it reads the mark's `said` flag.
pub(crate) fn gated(
    mark: Arc<Mark>,
    mut feed: impl FnMut(&[f32]) + Send + 'static,
) -> impl FnMut(&[f32]) + Send + 'static {
    move |frames: &[f32]| {
        if mark.said.load(Ordering::Acquire) {
            feed(frames);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    /// A guard against a wedge, not a timing claim.
    const GUARD: Duration = Duration::from_secs(60);

    fn speakers() -> Event {
        Event::Device {
            name: "Speakers".to_owned(),
            rate: 48_000,
            channels: 2,
        }
    }

    /// (a) While `play` blocks (a start that waits, as on a pending System Audio Recording grant), no `Device` has
    /// gone out; once it returns, exactly one.
    #[test]
    fn no_device_while_the_start_blocks() {
        let (tx, rx) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel::<()>();
        let mark = Mark::default();
        std::thread::scope(|s| {
            let (tx, mark) = (&tx, &mark);
            let starting = s.spawn(move || {
                start(tx, mark, speakers(), move || {
                    let _sent = entered_tx.send(());
                    let _released = release.recv();
                    Ok(())
                })
            });
            assert!(entered.recv_timeout(GUARD).is_ok(), "play runs");
            // Look, then let play go before any assert: a failed assert must not leave play blocked.
            let early = rx.try_recv().ok();
            assert!(release_tx.send(()).is_ok());
            let late = rx.recv_timeout(GUARD).ok();
            assert_eq!(starting.join().ok(), Some(Ok(())));
            assert_eq!(early, None, "no Device while play blocks");
            assert_eq!(late, Some(speakers()), "one Device once play returns");
        });
        assert_eq!(rx.try_recv().ok(), None, "exactly one");
    }

    /// (e) No lock is held across `play`: while `play` blocks (a pending grant), the error callback, on the audio
    /// thread, still reports — it returns, and its stream error is on the channel before `play` is let go; no
    /// `Device` follows, and the start fails.
    #[test]
    fn the_callback_reports_while_the_start_blocks() {
        let (tx, rx) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel::<()>();
        let mark = Mark::default();
        let stopped = || Event::StreamError("stopped while starting".to_owned());
        std::thread::scope(|s| {
            let (tx, mark) = (&tx, &mark);
            let starting = s.spawn(move || {
                start(tx, mark, speakers(), move || {
                    let _sent = entered_tx.send(());
                    let _released = release.recv();
                    Ok(())
                })
            });
            assert!(entered.recv_timeout(GUARD).is_ok(), "play runs");
            // The callback, on a thread of its own as the audio API's is; it must return while play still blocks.
            let callback = s.spawn(move || report(tx, mark, stopped()));
            let got = rx.recv_timeout(GUARD);
            let reported = got.is_ok();
            assert!(release_tx.send(()).is_ok());
            assert_eq!(callback.join().ok(), Some(()));
            let started = starting.join().ok();
            assert!(reported, "the callback's error, while play blocks: {got:?}");
            assert_eq!(got.ok(), Some(stopped()));
            let failed = "\"Speakers\": the stream reported an error while starting";
            assert_eq!(started, Some(Err(failed.to_owned())), "a dead start");
        });
        assert_eq!(rx.try_recv().ok(), None, "no Device after it");
        assert!(mark.failure().is_set(), "the failure mark");
    }

    /// Runs [`start`] with `play` on a thread of its own and waits for its result, so a start that wedges (`play`
    /// reporting while the lock is held) fails at the guard instead of hanging the run. The thread is not scoped:
    /// a wedged one is left behind, not joined.
    fn guarded_start(
        tx: &Sender<Event>,
        mark: &Arc<Mark>,
        play: impl FnOnce(&Sender<Event>, &Mark) -> Result<(), String> + Send + 'static,
    ) -> Result<Result<(), String>, mpsc::RecvTimeoutError> {
        let (done_tx, done) = mpsc::channel();
        let (tx, mark) = (tx.clone(), Arc::clone(mark));
        std::thread::spawn(move || {
            let started = start(&tx, &mark, speakers(), || play(&tx, &mark));
            let _sent = done_tx.send(started);
        });
        done.recv_timeout(GUARD)
    }

    /// (b) The callback reports a stream error while `play` runs, and `play` then returns `Ok`: the stream is dead,
    /// so no `Device` follows the error, and the start fails, so the caller opens it again (no second error).
    #[test]
    fn a_stream_error_while_starting_sends_no_device() {
        let (tx, rx) = mpsc::channel();
        let mark = Arc::new(Mark::default());
        let paused = || Event::StreamError("paused as it started".to_owned());
        let started = guarded_start(&tx, &mark, move |tx, mark| {
            report(tx, mark, paused());
            Ok(())
        });
        assert_eq!(
            started,
            Ok(Err(
                "\"Speakers\": the stream reported an error while starting".to_owned()
            ))
        );
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [paused()]);
    }

    /// (c) A `play` that fails is a stream error, and no `Device`.
    #[test]
    fn a_failed_start_sends_a_stream_error_and_no_device() {
        let (tx, rx) = mpsc::channel();
        let mark = Mark::default();
        let failed = start(&tx, &mark, speakers(), || Err("cannot start".to_owned()));
        assert_eq!(failed, Err("cannot start".to_owned()));
        drop(tx);
        assert_eq!(
            rx.into_iter().collect::<Vec<_>>(),
            [Event::StreamError("cannot start".to_owned())]
        );
    }

    /// (d) An error after a clean start comes after its `Device`; a warning the callback reports does not mark
    /// the stream.
    #[test]
    fn a_stream_error_after_a_clean_start_comes_after_the_device() {
        let (tx, rx) = mpsc::channel();
        let mark = Arc::new(Mark::default());
        let xrun = || Event::Warning("stream: overrun".to_owned());
        let started = guarded_start(&tx, &mark, move |tx, mark| {
            report(tx, mark, xrun());
            Ok(())
        });
        assert_eq!(started, Ok(Ok(())));
        assert!(!mark.failure().is_set(), "a warning does not mark");
        let stopped = || Event::StreamError("stopped".to_owned());
        report(&tx, &mark, stopped());
        drop(tx);
        assert_eq!(
            rx.into_iter().collect::<Vec<_>>(),
            [xrun(), speakers(), stopped()]
        );
    }

    /// A gated audio callback over `events`, as `mic`'s: each call sends its frames' count as `Audio`.
    fn callback(mark: &Arc<Mark>, events: &Sender<Event>) -> impl FnMut(&[f32]) + Send + 'static {
        let audio = events.clone();
        gated(Arc::clone(mark), move |frames: &[f32]| {
            let n = i16::try_from(frames.len()).unwrap_or(i16::MAX);
            let _sent = audio.send(Event::Audio(vec![n]));
        })
    }

    /// The events so far, without waiting.
    fn taken(rx: &Receiver<Event>) -> Vec<Event> {
        rx.try_iter().collect()
    }

    /// C2: the audio callback runs while the start still runs (the audio API calls it from its first buffer, which
    /// can come before `play` returns). Until the `Device` is out it passes nothing; after it, the audio flows,
    /// behind the `Device`.
    #[test]
    fn no_audio_comes_before_the_device() {
        let (tx, rx) = mpsc::channel();
        let mark = Arc::new(Mark::default());
        let mut feed = callback(&mark, &tx);
        feed(&[0.0; 3]);
        let started = start(&tx, &mark, speakers(), || {
            feed(&[0.0; 5]);
            Ok(())
        });
        assert_eq!(started, Ok(()));
        feed(&[0.0; 7]);
        assert_eq!(taken(&rx), [speakers(), Event::Audio(vec![7])]);
    }

    /// C2: a start that fails, or a stream that failed while starting, never lets its audio through: no `Device`
    /// went out, so the stream is not open.
    #[test]
    fn a_failed_start_lets_no_audio_through() {
        let (tx, rx) = mpsc::channel();
        let refused = Arc::new(Mark::default());
        let mut feed = callback(&refused, &tx);
        let failed = start(&tx, &refused, speakers(), || Err("cannot start".to_owned()));
        assert!(failed.is_err());
        feed(&[0.0; 3]);
        let paused = Arc::new(Mark::default());
        let mut feed = callback(&paused, &tx);
        let stopped = || Event::StreamError("paused as it started".to_owned());
        let failed = start(&tx, &paused, speakers(), || {
            report(&tx, &paused, stopped());
            Ok(())
        });
        assert!(failed.is_err());
        feed(&[0.0; 3]);
        assert_eq!(
            taken(&rx),
            [Event::StreamError("cannot start".to_owned()), stopped()]
        );
    }

    /// akira's A1: the audio callback takes no lock. While the mark's lock is held elsewhere (a `say` or a `report`
    /// in progress), a callback on the audio thread still returns at once.
    #[test]
    fn the_audio_callback_takes_no_lock() {
        let (tx, rx) = mpsc::channel();
        let mark = Arc::new(Mark::default());
        assert_eq!(say(&tx, &mark, speakers()), Ok(()));
        let mut feed = callback(&mark, &tx);
        let held = mark.retired.lock().unwrap_or_else(PoisonError::into_inner);
        let (done_tx, done) = mpsc::channel();
        // Not scoped: a callback that wedges on the lock is left behind, and the test fails at the guard.
        std::thread::spawn(move || {
            feed(&[0.0; 3]);
            let _sent = done_tx.send(());
        });
        let returned = done.recv_timeout(GUARD);
        drop(held);
        assert!(returned.is_ok(), "the callback returned with the lock held");
        assert_eq!(taken(&rx), [speakers(), Event::Audio(vec![3])]);
    }

    /// Fable F3: once its capture is dropped ([`Held`]), a stream's callbacks send nothing on the side's channel: no
    /// late error, no late audio, and its mark is not set.
    #[test]
    fn a_retired_stream_sends_nothing() {
        let (tx, rx) = mpsc::channel();
        let held = Held::default();
        let mark = Arc::clone(held.mark());
        let mut feed = callback(&mark, &tx);
        assert_eq!(start(&tx, &mark, speakers(), || Ok(())), Ok(()));
        feed(&[0.0; 2]);
        drop(held);
        feed(&[0.0; 3]);
        report(&tx, &mark, Event::StreamError("late".to_owned()));
        assert_eq!(taken(&rx), [speakers(), Event::Audio(vec![2])]);
        assert!(!mark.failure().is_set(), "a retired mark is not set");
    }

    /// C2, the voice unit's path: it has no start of its own to wrap ([`say`] after the unit opened), and its audio
    /// waits for that `Device` all the same.
    #[test]
    fn a_said_device_opens_the_gate() {
        let (tx, rx) = mpsc::channel();
        let mark = Arc::new(Mark::default());
        let mut feed = callback(&mark, &tx);
        feed(&[0.0; 2]);
        assert_eq!(say(&tx, &mark, speakers()), Ok(()));
        feed(&[0.0; 4]);
        assert_eq!(taken(&rx), [speakers(), Event::Audio(vec![4])]);
    }

    /// A stream whose teardown reports an error, as cpal can while it stops a stream.
    struct ErrsOnDrop(Sender<Event>, Arc<Mark>);

    impl Drop for ErrsOnDrop {
        fn drop(&mut self) {
            report(&self.0, &self.1, Event::StreamError("torn down".to_owned()));
        }
    }

    /// Fable N4 (akira A1): dropping a live stream retires its mark before the stream is torn down, so an error
    /// reported during the teardown is not the side's: nothing is sent, nothing is marked. `Live`'s own `Drop`
    /// does it, so the order does not rest on how its fields are declared.
    #[test]
    fn an_error_from_the_teardown_of_a_dropped_stream_goes_nowhere() {
        let (tx, rx) = mpsc::channel();
        let mark = Held::default();
        let stream = ErrsOnDrop(tx.clone(), Arc::clone(mark.mark()));
        let live = Live::new(mark, stream);
        let failure = Arc::clone(live.mark());
        assert!(say(&tx, live.mark(), crate::testkit::speakers()).is_ok());
        drop(live);
        drop(tx);
        let said: Vec<Event> = rx.iter().collect();
        assert_eq!(
            said,
            [crate::testkit::speakers()],
            "only the Device it said while it ran"
        );
        assert!(!failure.failure().is_set(), "a retired mark is not set");
    }
}
