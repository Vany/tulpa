//! One side's heartbeat, and the tap's silences: the adapter between a capture's events and the side's pipeline.
//!
//! [`Beating`] passes a capture's [`Event`]s on as they come, on a thread of its own ([`Beating::forward`]), and
//! does four things as they pass: it writes the side's `.alive` (the tray's self-check judges a side by its
//! directory changing), it withholds that beat from a side whose stream keeps failing ([`FLAPS`]), it fills
//! the tap's silent gaps with zeros ([`GAP`]) so the side's samples keep the wall clock's time, and it says when
//! the output plays but the tap hears nothing ([`DEAF_AFTER`]): the tap sends nothing while nothing plays, so
//! without that a dead tap and a quiet Mac look the same.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tulpa_segment::{ALIVE, BEAT, SAMPLE_RATE, put};
use tulpa_stop::Stop;

use crate::event::Event;
use crate::hear::{Side, say};

/// This many stream errors on one side within [`FLAP_WINDOW`] and the side does not beat: it is reopened and fails
/// again and again (the follower reopens every `RETRY_AFTER`), hearing in scraps, and that must show as DEGRADED
/// rather than as a side that beats once per short life. Only the error of a stream that had opened counts (its
/// `Device` came): a start whose `play()` failed sends no `Device`, and its error is a retry, not a flap. One error
/// is a lid close, a display sleep or an unplug, and the reopen brings the side back; two can be such an error and
/// a reopened stream that died again at once; three is a stream that will not stay up.
const FLAPS: usize = 3;

/// The window [`FLAPS`] counts in. It catches a stream that dies about every 10 s or sooner (three lives and two
/// 2 s retry delays fit in it) and keeps the side without a beat for as long as that goes on, well past the tray's
/// 20 s freshness limit; a side that then holds beats again at most this long after its third-last error.
const FLAP_WINDOW: Duration = Duration::from_secs(30);

/// A stream that has stayed up this long since its `Device` holds: the side's flap count starts again. Twice the
/// beat interval, so a stream that lived long enough to beat a few times is no flap, and short of the tray's 20 s
/// limit, so a side that recovers is beating again before the tray would call it stale.
const HOLD: Duration = Duration::from_secs(10);

/// A gap this long or longer in the tap's ("them") open stream is passed on as that much silence (zeros).
///
/// The tap sends nothing while nothing plays, so without it the side's samples would fall behind the wall clock
/// at every pause: its anchor ([`Anchor`], the earliest estimate kept) would put every later phrase early by the
/// pauses so far, and the phrase before a pause would stay open until the sound came back (the segmenter closes on
/// silence it is given). The silence goes out at each silent check ([`BEAT`]'s worth of time, as the silent beat)
/// and ahead of audio that resumes after a gap. Shorter gaps are a callback's jitter, not a pause.
const GAP: Duration = Duration::from_millis(200);

/// How often an open tap's owed silence is looked for and passed on while no event comes.
///
/// The segmenter closes a phrase after 2 s of the silence it is given: at a whole beat (5 s) between looks the
/// phrase before a pause came out about 7 s after it ended.
const FILL_EVERY: Duration = Duration::from_secs(1);

/// A gap longer than this in the tap's open stream is not filled: it is a break ([`Event::Lost`]).
///
/// The tap is silent for hours while nothing plays, and that silence is filled as it passes, a silent check at a
/// time. Only a stall of this thread (the process stopped, a wedged heartbeat write) leaves one gap this long, and
/// its zeros would be one allocation the size of the stall, owed to the segmenter and whisper's queue.
pub const LOST_AFTER: Duration = Duration::from_secs(30);

/// The tapped output has been running (some process plays through it) this long while the tap delivered nothing
/// but zeros: the tap is said to hear nothing, once per such stretch ([`Deaf`]).
///
/// Longer than the short sounds that run the output for a few seconds (an alert, a click, a notification sound;
/// the device may stay running a moment after), so those are no stretch; short of the tray's 20 s freshness limit,
/// so the line is in the side's error tail about as soon as anyone would wonder why "them" is quiet. Looked at on
/// the fill's own clock ([`FILL_EVERY`]) or at the tap's next event, so it is said about 10–11 s in.
pub const DEAF_AFTER: Duration = Duration::from_secs(10);

/// The length of `n` samples at [`SAMPLE_RATE`].
fn span(n: usize) -> Duration {
    let n = u64::try_from(n).unwrap_or(u64::MAX);
    Duration::from_micros(
        n.saturating_mul(1_000_000)
            .checked_div(u64::from(SAMPLE_RATE))
            .unwrap_or(0),
    )
}

/// The samples at [`SAMPLE_RATE`] in `d`.
pub(crate) fn samples(d: Duration) -> usize {
    let n = d
        .as_micros()
        .saturating_mul(u128::from(SAMPLE_RATE))
        .checked_div(1_000_000)
        .unwrap_or(0);
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// One side's capture events, passed through as they come, with the side's heartbeat, and the tap's silences filled
/// ([`GAP`]).
///
/// It runs where the samples arrive ([`Beating::forward`], on a thread of its own), never on the pipeline's thread:
/// that one waits on whisper, and on the other side's whisper behind the shared recognizer, longer than the tray's
/// freshness window. `<dir>/.alive` (made when missing) is rewritten once the open stream has delivered a [`BEAT`] of
/// audio, and at every [`BEAT`] of audio after that.
///
/// The system-audio tap ("them") sends nothing at all while nothing plays, so on that side the heartbeat also beats
/// every [`BEAT`]'s worth of time while its device is open ([`Event::Device`], until [`Event::NoInput`],
/// [`Event::Switched`] or [`Event::StreamError`]). The mic
/// streams in silence too, so it beats on audio only: a mic stream that stops delivering goes stale. Either side
/// stops beating when its capture ends (the channel disconnects) or its pipeline is gone (the receiver hangs up, or
/// — for the silent beat, which sends nothing — the pipeline's [`Arc`](std::sync::Arc) token is dropped), never beats once the
/// run's stop is set ([`Beating::with_stop`]), and never beats before its stream opens. A side whose stream keeps failing ([`FLAPS`] stream errors within [`FLAP_WINDOW`]) does not beat
/// at all until it holds, so the tray sees it go stale.
#[derive(Debug)]
pub struct Beating {
    events: Receiver<Event>,
    side: Side,
    /// Where `.alive` goes; `None`: no heartbeat file (the shadow), the gaps are filled all the same.
    dir: Option<PathBuf>,
    /// The open stream's samples since its last beat, or since its `Device`: a beat is earned by a whole
    /// [`BEAT`] of that stream's own audio.
    since: usize,
    last: Option<Instant>,
    every: Duration,
    failed_said: bool,
    /// Whether the side's stream is open, and how it has been failing.
    stream: Stream,
    /// The tap's silences still to pass on.
    fill: Fill,
    /// Whether the tap hears what its output plays.
    deaf: Deaf,
    /// The pipeline's token: gone once the pipeline has ended; `None`: not followed.
    reader: Option<Weak<()>>,
    /// The run's stop; `None`: never stopped. Once it is set, `.alive` is not written any more ([`Beating::beat`]).
    stop: Option<Arc<Stop>>,
    /// An event held back for the next call: the audio a gap's silence went out ahead of.
    pending: Option<Event>,
    /// Tests: told after each silent check (a wait with no event) whether it beat.
    #[cfg(test)]
    ticks: Option<std::sync::mpsc::Sender<bool>>,
}

/// Where [`Beating::forward`] passes the events on to.
pub trait Pass {
    /// Takes `ev`; `false` when nobody reads any more.
    fn pass(&self, ev: Event) -> bool;
}

impl Pass for mpsc::Sender<Event> {
    fn pass(&self, ev: Event) -> bool {
        self.send(ev).is_ok()
    }
}

impl Pass for crate::queue::Feed {
    fn pass(&self, ev: Event) -> bool {
        self.send(ev)
    }
}

/// One side's stream as its events tell it: open or not, and its recent failures ([`FLAPS`]).
#[derive(Debug)]
struct Stream {
    /// When the open stream's `Device` came; `None` while no stream is open.
    opened: Option<Instant>,
    /// When the side's recent stream errors came (those within [`FLAP_WINDOW`]).
    failures: Vec<Instant>,
    /// How long a stream must stay up to start the flap count again ([`HOLD`]; tests set it shorter).
    hold: Duration,
    /// How long a stream error counts toward the flaps ([`FLAP_WINDOW`]; tests set it shorter).
    window: Duration,
}

impl Stream {
    const fn is_open(&self) -> bool {
        self.opened.is_some()
    }

    /// The side's `Device` came: its stream runs.
    fn open(&mut self) {
        self.opened = Some(Instant::now());
    }

    /// Not open until the next `Device`: the stream is gone, or may have stopped.
    const fn close(&mut self) {
        self.opened = None;
    }

    /// A stream error now. A flap only when a stream had opened (its `Device` came): a start that failed before
    /// any stream ran (`play()` failed, no `Device`) is a retry, not a stream that would not stay up. Those older
    /// than [`FLAP_WINDOW`] are let go.
    fn fail(&mut self) {
        if self.is_open() {
            let now = Instant::now();
            let window = self.window;
            self.failures
                .retain(|t| now.saturating_duration_since(*t) < window);
            self.failures.push(now);
        }
        self.close();
    }

    /// Another device: a broken one's flaps are not the new one's.
    fn switch(&mut self) {
        self.close();
        self.failures.clear();
    }

    /// The side's stream has failed [`FLAPS`] times within [`FLAP_WINDOW`].
    fn flapping(&self) -> bool {
        let recent = self.failures.iter().filter(|t| t.elapsed() < self.window);
        recent.count() >= FLAPS
    }

    /// The open stream has held for [`HOLD`]: the flap count starts again.
    fn held(&mut self) {
        if self.opened.is_some_and(|t| t.elapsed() >= self.hold) {
            self.failures.clear();
        }
    }
}

/// The tap's ("them") silent gaps, owed to the pipeline as zeros ([`GAP`]).
#[derive(Debug, Default)]
struct Fill {
    /// The tap's open stream only: when the audio passed on so far ends, wall clock; `None`: nothing is filled
    /// (the mic, or no open stream).
    heard: Option<Instant>,
}

impl Fill {
    /// `side`'s stream opened: the tap's time counts from here, and a silence from here on is filled.
    fn open(&mut self, side: Side) {
        self.heard = (side == Side::Them).then(Instant::now);
    }

    /// No open stream: nothing to fill.
    const fn close(&mut self) {
        self.heard = None;
    }

    /// The silence the tap's open stream owes the pipeline before `next` more samples (0: none, a silent check): the
    /// wall time since its audio so far ended, less the `next` samples' own length, as zeros when it is [`GAP`] or
    /// more. A shorter gap stays owed: the audio's end moves on by the `next` samples only (never past now), so
    /// short gaps add up until they reach [`GAP`]. A gap past [`LOST_AFTER`] is not owed as zeros: it is said as
    /// [`Event::Lost`], and the time counts from now.
    fn owed(&mut self, next: usize) -> Option<Event> {
        let now = Instant::now();
        let heard = self.heard.as_mut()?;
        let gap = now
            .saturating_duration_since(*heard)
            .saturating_sub(span(next));
        if gap < GAP {
            // Not a pause yet: the `next` samples follow the audio so far, and what is still missing stays owed,
            // so short gaps add up instead of being forgotten one by one. Never past now: audio that comes in a
            // burst is not ahead of its time.
            *heard = heard
                .checked_add(span(next))
                .map_or(now, |end| end.min(now));
            return None;
        }
        *heard = now;
        if gap > LOST_AFTER {
            let ms = u64::try_from(gap.as_millis()).unwrap_or(u64::MAX);
            return Some(Event::Lost { ms });
        }
        Some(Event::Audio(vec![0; samples(gap)]))
    }
}

/// Whether the tap hears what its output plays: the stretch for which the output has run with only zeros from the
/// tap, and whether that stretch was said. Pure: the caller gives the time.
#[derive(Debug)]
struct Deaf {
    /// When the current stretch began; `None`: the output is not running (or not known to be).
    since: Option<Instant>,
    /// The current stretch's warning went out.
    said: bool,
    /// How long a stretch lasts before it is said ([`DEAF_AFTER`]; tests set it shorter).
    after: Duration,
}

impl Deaf {
    const fn new(after: Duration) -> Self {
        Self {
            since: None,
            said: false,
            after,
        }
    }

    /// The output's running state: on starts a stretch unless one runs; off ends it, and so does unknown (`None`):
    /// a warning is never said from a state nobody read.
    const fn running(&mut self, on: Option<bool>, now: Instant) {
        if !matches!(on, Some(true)) {
            self.end();
        } else if self.since.is_none() {
            self.since = Some(now);
        }
    }

    /// The tap delivered `chunk`. Sound (any sample but zero) ends the stretch; the output still runs, so a new one
    /// starts now. Zeros are no sound: a tap that is refused or broken can deliver nothing else.
    fn heard(&mut self, chunk: &[i16], now: Instant) {
        if self.since.is_some() && chunk.iter().any(|&s| s != 0) {
            self.since = Some(now);
            self.said = false;
        }
    }

    /// No stretch: the output stopped, or the tap's stream (whose device's state this was) is gone.
    const fn end(&mut self) {
        self.since = None;
        self.said = false;
    }

    /// How long the current stretch has lasted, once it has lasted [`Deaf::after`] and was not said yet; then it is
    /// said.
    fn due(&mut self, now: Instant) -> Option<Duration> {
        let long = now.saturating_duration_since(self.since?);
        if self.said || long < self.after {
            return None;
        }
        self.said = true;
        Some(long)
    }
}

impl Beating {
    /// `events` of `side`, beating in `dir`.
    #[must_use]
    pub fn new(events: Receiver<Event>, side: Side, dir: Option<PathBuf>) -> Self {
        if let Some(dir) = &dir
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            say(side, &format!("{} not made: {e}", dir.display()));
        }
        let every_ms = u64::try_from(BEAT)
            .unwrap_or(u64::MAX)
            .saturating_mul(1000)
            .checked_div(u64::from(SAMPLE_RATE))
            .unwrap_or(u64::MAX);
        Self {
            events,
            side,
            dir,
            since: 0,
            last: None,
            every: Duration::from_millis(every_ms),
            failed_said: false,
            stream: Stream {
                opened: None,
                failures: Vec::new(),
                hold: HOLD,
                window: FLAP_WINDOW,
            },
            fill: Fill::default(),
            deaf: Deaf::new(DEAF_AFTER),
            reader: None,
            stop: None,
            pending: None,
            #[cfg(test)]
            ticks: None,
        }
    }

    /// Passes the events on to `to` (the side's pipeline, which holds the strong side of `reader`) until the capture
    /// ends or the pipeline is gone, beating as they pass.
    pub fn forward(mut self, to: &impl Pass, reader: Weak<()>) {
        self.reader = Some(reader);
        for ev in self {
            if !to.pass(ev) {
                return;
            }
        }
    }

    /// This adapter with its time between silent beats at `every`, telling `ticks` after each silent check whether
    /// it beat (tests: the order of events, not elapsed time, is what they assert).
    #[cfg(test)]
    pub(crate) fn with_every(
        mut self,
        every: Duration,
        ticks: std::sync::mpsc::Sender<bool>,
    ) -> Self {
        self.every = every;
        self.ticks = Some(ticks);
        self
    }

    /// This adapter with a stream counted as holding after `hold` instead of [`HOLD`] (tests: no 10 s wait).
    #[cfg(test)]
    pub(crate) const fn with_hold(mut self, hold: Duration) -> Self {
        self.stream.hold = hold;
        self
    }

    /// This adapter with a stream error counted toward the flaps for `window` instead of [`FLAP_WINDOW`] (tests:
    /// no 30 s wait).
    #[cfg(test)]
    pub(crate) const fn with_flap_window(mut self, window: Duration) -> Self {
        self.stream.window = window;
        self
    }

    /// This adapter saying a deaf tap after `after` instead of [`DEAF_AFTER`] (tests: no 10 s wait).
    #[cfg(test)]
    pub(crate) const fn with_deaf_after(mut self, after: Duration) -> Self {
        self.deaf.after = after;
        self
    }

    /// This adapter under the run's `stop`: once it is set, the side's `.alive` is written no more.
    #[must_use]
    pub fn with_stop(mut self, stop: Arc<Stop>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// A beat: the count starts again, and `.alive` is rewritten. Not once the run is told to stop: `.alive` is
    /// the side's sign of life to the tray, which may already be judging the next capture by it, and a run that
    /// only drains what it has must not vouch for its successor. The events are passed on all the same.
    fn beat(&mut self) {
        self.since = 0;
        self.last = Some(Instant::now());
        if self.stop.as_ref().is_some_and(|stop| stop.is_set()) {
            return;
        }
        let Some(dir) = &self.dir else { return };
        if let Err(e) = put(dir, ALIVE, b"")
            && !self.failed_said
        {
            self.failed_said = true;
            say(self.side, &format!("heartbeat not written: {e}"));
        }
    }

    /// The tap's open, silent stream is due a beat (and its pipeline still runs).
    fn silent_due(&self) -> bool {
        self.side == Side::Them
            && self.stream.is_open()
            && !self.stream.flapping()
            && self.last.is_none_or(|t| t.elapsed() >= self.every)
            && self.reader.as_ref().is_none_or(|r| r.strong_count() > 0)
    }

    /// The warning that the open tap hears nothing while its output plays, when it is due ([`Deaf::due`]).
    fn deaf_line(&mut self) -> Option<Event> {
        if !self.stream.is_open() {
            return None;
        }
        let long = self.deaf.due(Instant::now())?;
        Some(Event::Warning(format!(
            "output playing for {} s but the tap hears nothing",
            long.as_secs()
        )))
    }
}

impl Iterator for Beating {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        if let Some(ev) = self.pending.take() {
            return Some(ev);
        }
        loop {
            // A poll, on purpose: the tap gives no callback and no event while nothing plays, so a quiet, open
            // stream has nothing to wake on; the timeout is what beats for it, and what passes its silence on.
            // While a silence may be owed the look comes every FILL_EVERY, so the segmenter gets the pause that
            // closes a phrase about as it happens; the beat keeps its own interval (`silent_due`). A disconnect
            // still ends the side.
            let wait = if self.fill.heard.is_some() {
                self.every.min(FILL_EVERY)
            } else {
                self.every
            };
            let ev = match self.events.recv_timeout(wait) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    self.stream.held();
                    let due = self.silent_due();
                    if due {
                        self.beat();
                    }
                    #[cfg(test)]
                    if let Some(ticks) = &self.ticks {
                        let _told = ticks.send(due);
                    }
                    if let Some(silence) = self.fill.owed(0) {
                        self.pending = self.deaf_line();
                        return Some(silence);
                    }
                    if let Some(deaf) = self.deaf_line() {
                        return Some(deaf);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return None,
            };
            match &ev {
                // A Device is sent only once its stream runs: after `play()` returned, and only if no stream error
                // came meanwhile (`start::start`). A start that blocks (a pending grant) or fails sends none, so
                // the side is not open and the tap's silent beat stays off.
                Event::Device { .. } => {
                    self.stream.open();
                    // This stream's samples count from here: what a stream before it left does not beat for it.
                    self.since = 0;
                    // The silent beat's clock starts at the open: its first beat comes a full interval later, and
                    // only if the stream is still open then. A stream that fails just after its start never beats.
                    self.last = Some(Instant::now());
                    self.fill.open(self.side);
                }
                // The old stream is gone (a switch drops it first; an open that is skipped or fails sends no
                // Device), or it may have stopped (a stream error): not open until the next Device.
                Event::NoInput => {
                    self.stream.close();
                    self.fill.close();
                    self.deaf.end();
                }
                Event::Switched { .. } => {
                    self.stream.switch();
                    self.fill.close();
                    self.deaf.end();
                }
                Event::StreamError(_) => {
                    self.stream.fail();
                    self.fill.close();
                    self.deaf.end();
                }
                Event::OutputRunning(on) => self.deaf.running(*on, Instant::now()),
                Event::Audio(chunk) => {
                    self.stream.held();
                    self.since = self.since.saturating_add(chunk.len());
                    // Only an open stream's audio beats, or tells the tap hears. The capture drops a buffer
                    // delivered while `play()` ran (`start::gated`), so none comes before its Device; this gate
                    // holds whatever a capture sends, so audio with no Device (or after a stream error) never makes
                    // a dead side look alive. The samples are passed on all the same.
                    if self.stream.is_open() {
                        self.deaf.heard(chunk, Instant::now());
                    }
                    if self.since >= BEAT && self.stream.is_open() && !self.stream.flapping() {
                        self.beat();
                    }
                }
                _ => {}
            }
            if self.silent_due() {
                self.beat();
            }
            // Audio after a gap: the gap's silence goes first, so the samples count the wall time.
            if let Event::Audio(chunk) = &ev
                && let Some(silence) = self.fill.owed(chunk.len())
            {
                self.pending = Some(ev);
                return Some(silence);
            }
            // A tap that delivers zeros all the time never times out: its warning is looked at here too.
            if let Some(deaf) = self.deaf_line() {
                self.pending = Some(ev);
                return Some(deaf);
            }
            return Some(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use super::*;
    use crate::testkit::{speakers, temp_state};

    /// The heartbeat follows the audio: once the stream has delivered 5 s of samples, and once per 5 s after that;
    /// the events pass through unchanged and end with the capture.
    #[test]
    fn the_heartbeat_counts_samples() {
        let state = temp_state("beat-count");
        let dir = state.join("me");
        let alive = dir.join(tulpa_segment::ALIVE);
        let (tx, rx) = mpsc::channel();
        let mut beating = Beating::new(rx, Side::Me, Some(dir));
        let device = Event::Device {
            name: "Mic".to_owned(),
            rate: 48_000,
            channels: 1,
        };
        let _sent = tx.send(device.clone());
        assert_eq!(beating.next(), Some(device));
        assert!(!alive.exists(), "the mic beats on audio only");
        let mut written = Vec::new();
        for i in 0..120 {
            let _sent = tx.send(Event::Audio(vec![0; 1600]));
            assert_eq!(beating.next(), Some(Event::Audio(vec![0; 1600])));
            if std::fs::remove_file(&alive).is_ok() {
                written.push(i);
            }
        }
        // Every 50 chunks of 0.1 s: once the stream has delivered 5 s, and each 5 s after that.
        assert_eq!(written, [49, 99]);
        drop(tx);
        assert_eq!(beating.next(), None);
        let _gone = std::fs::remove_dir_all(&state);
    }

    /// Whether any of the next three silent checks beat. Each wait is a guard against a wedge, not a timing claim: a
    /// check comes every 50 ms.
    fn three_checks(ticks: &mpsc::Receiver<bool>) -> bool {
        let mut beat = false;
        for _ in 0..3 {
            let tick = ticks.recv_timeout(Duration::from_secs(60));
            assert!(tick.is_ok(), "a silent check: {tick:?}");
            beat |= tick.unwrap_or(false);
        }
        beat
    }

    /// A tap whose start is blocked (a pending System Audio Recording grant: no `Device` is sent until `play`
    /// returns) does not beat; once its `Device` comes, it does. Asserted by the order of events: three silent checks
    /// with no `Device` beat none, and `.alive` is not written; the `Device` is taken (it comes through the
    /// forwarder), and the next check beats.
    #[test]
    fn a_tap_whose_start_is_blocked_does_not_beat() {
        let state = temp_state("beat-blocked");
        let dir = state.join("them");
        let alive = dir.join(tulpa_segment::ALIVE);
        let (tx, rx) = mpsc::channel();
        let (to, pipeline) = mpsc::channel();
        let (ticks_tx, ticks) = mpsc::channel();
        let token = Arc::new(());
        let reader = Arc::downgrade(&token);
        let beating =
            Beating::new(rx, Side::Them, Some(dir)).with_every(Duration::from_millis(50), ticks_tx);
        let forwarding = std::thread::spawn(move || beating.forward(&to, reader));
        let blocked = three_checks(&ticks);
        let written_while_blocked = alive.exists();
        let device = Event::Device {
            name: "Speakers".to_owned(),
            rate: 48_000,
            channels: 2,
        };
        let _sent = tx.send(device.clone());
        // Each wait is a guard against a wedge, not a timing claim.
        let forwarded = pipeline.recv_timeout(Duration::from_secs(60));
        while ticks.try_recv().is_ok() {}
        let next = ticks.recv_timeout(Duration::from_secs(60));
        drop(tx);
        let _joined = forwarding.join();
        drop(token);
        let written = alive.exists();
        let _gone = std::fs::remove_dir_all(&state);
        assert!(!blocked, "no beat before the Device");
        assert!(
            !written_while_blocked,
            ".alive not written before the Device"
        );
        assert_eq!(forwarded.ok(), Some(device));
        assert_eq!(next.ok(), Some(true), "the check after the Device beats");
        assert!(written);
    }

    /// A forwarded silent tap at a 50 ms beat; `cycle` drives it with its event sender and its ticks, and gets back
    /// whether `.alive` had been written, taking it away. Returns what `cycle` returned.
    fn tap_beating<T>(
        tag: &str,
        cycle: impl FnOnce(&mpsc::Sender<Event>, &mpsc::Receiver<bool>, &dyn Fn() -> bool) -> T,
    ) -> T {
        tap_beating_holding(tag, HOLD, cycle)
    }

    /// [`tap_beating`], a stream counted as holding after `hold`.
    fn tap_beating_holding<T>(
        tag: &str,
        hold: Duration,
        cycle: impl FnOnce(&mpsc::Sender<Event>, &mpsc::Receiver<bool>, &dyn Fn() -> bool) -> T,
    ) -> T {
        tap_beating_within(tag, hold, FLAP_WINDOW, cycle)
    }

    /// [`tap_beating_holding`], a stream error counted toward the flaps for `window`.
    fn tap_beating_within<T>(
        tag: &str,
        hold: Duration,
        window: Duration,
        cycle: impl FnOnce(&mpsc::Sender<Event>, &mpsc::Receiver<bool>, &dyn Fn() -> bool) -> T,
    ) -> T {
        let state = temp_state(tag);
        let dir = state.join("them");
        let alive = dir.join(tulpa_segment::ALIVE);
        let (tx, rx) = mpsc::channel();
        let (to, _pipeline) = mpsc::channel();
        let (ticks_tx, ticks) = mpsc::channel();
        let token = Arc::new(());
        let reader = Arc::downgrade(&token);
        let beating = Beating::new(rx, Side::Them, Some(dir))
            .with_every(Duration::from_millis(50), ticks_tx)
            .with_hold(hold)
            .with_flap_window(window);
        let forwarding = std::thread::spawn(move || beating.forward(&to, reader));
        let out = cycle(&tx, &ticks, &|| std::fs::remove_file(&alive).is_ok());
        drop(tx);
        let _joined = forwarding.join();
        drop(token);
        let _gone = std::fs::remove_dir_all(&state);
        out
    }

    fn stopped() -> Event {
        Event::StreamError("stopped".to_owned())
    }

    /// Fable N5: the tap's silence reaches the pipeline about a second after the sound stops, not a whole beat
    /// (5 s) later: the segmenter closes a phrase on the silence it is given, so the phrase before a pause comes
    /// out some 3 s after it ends, not 7. The beat interval is an hour here, so only the fill's own look
    /// ([`FILL_EVERY`]) can pass the silence on: waiting a beat instead would wait the hour. The bound is a guard
    /// against that wedge, not a timing claim.
    #[test]
    fn a_pause_in_the_tap_is_filled_by_its_own_look() {
        let (tx, rx) = mpsc::channel();
        let (ticks, _ticked) = mpsc::channel();
        let mut beating =
            Beating::new(rx, Side::Them, None).with_every(Duration::from_secs(3600), ticks);
        let _sent = tx.send(speakers());
        let _sent = tx.send(Event::Audio(vec![1; 1600]));
        let (got_tx, got) = mpsc::channel();
        let looking = std::thread::spawn(move || {
            let _device = beating.next();
            let _audio = beating.next();
            let _sent = got_tx.send(beating.next());
        });
        let silence = got.recv_timeout(Duration::from_secs(30));
        // Ends the hour's wait, if that is where it is.
        drop(tx);
        let _joined = looking.join();
        assert!(
            matches!(&silence, Ok(Some(Event::Audio(a))) if a.iter().all(|&s| s == 0)),
            "the pause's silence: {silence:?}"
        );
    }

    /// Fable N3: a gap shorter than [`GAP`] is not forgotten. Chunks of 0.1 s that come 0.25 s apart each leave
    /// 0.15 s unfilled; the deficits add up and go on as silence once they reach [`GAP`], so the side's samples keep
    /// up with the wall clock instead of sliding back by every short gap.
    #[test]
    fn short_gaps_add_up_to_their_silence() {
        let (tx, rx) = mpsc::channel();
        let mut beating = Beating::new(rx, Side::Them, None);
        let before = Instant::now();
        let _sent = tx.send(speakers());
        let _device = beating.next();
        let began = Instant::now();
        let mut zeros = 0_usize;
        let mut audio = 0_usize;
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(250));
            let _sent = tx.send(Event::Audio(vec![3; 1600]));
            loop {
                match beating.next() {
                    Some(Event::Audio(a)) if a.first() == Some(&3) => {
                        audio += a.len();
                        break;
                    }
                    Some(Event::Audio(a)) => zeros += a.len(),
                    other => {
                        assert!(other.is_none(), "only audio comes: {other:?}");
                        break;
                    }
                }
            }
        }
        // What the wall clock says is missing: the time since the open less the audio itself. All of it is owed,
        // but for the last deficit still under GAP; none of it more than once.
        let least = samples(
            began
                .elapsed()
                .saturating_sub(span(audio))
                .saturating_sub(GAP),
        );
        let most = samples(before.elapsed().saturating_sub(span(audio)));
        assert!(
            zeros >= least && zeros <= most,
            "{least} <= {zeros} <= {most} samples of silence"
        );
    }

    /// Fable N1 (akira A3): a fill is never sized by how long this thread stalled. A gap in the tap's open stream
    /// up to [`LOST_AFTER`] goes on as silence; a longer one (the process stopped, a wedged write) is a break in the
    /// stream: one `Lost` naming the gap, and no audio event longer than the bound.
    #[test]
    fn a_gap_past_the_bound_is_a_break_not_a_fill() {
        let after = |ago: Duration| {
            let (tx, rx) = mpsc::channel();
            let mut beating = Beating::new(rx, Side::Them, None);
            let _sent = tx.send(speakers());
            let _device = beating.next();
            // As if the audio so far ended `ago` back: the thread stalled since.
            beating.fill.heard = Instant::now().checked_sub(ago);
            assert!(beating.fill.heard.is_some(), "the machine is up {ago:?}");
            let _sent = tx.send(Event::Audio(vec![2; 1600]));
            drop(tx);
            beating.collect::<Vec<Event>>()
        };
        let longest = |evs: &[Event]| {
            evs.iter()
                .filter_map(|ev| match ev {
                    Event::Audio(a) => Some(a.len()),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        };
        let bound = samples(LOST_AFTER);
        let stalled = after(Duration::from_secs(31));
        assert!(
            longest(&stalled) <= bound,
            "no audio event past the bound: {} samples",
            longest(&stalled)
        );
        let lost: Vec<u64> = stalled
            .iter()
            .filter_map(|ev| match ev {
                Event::Lost { ms } => Some(*ms),
                _ => None,
            })
            .collect();
        assert!(
            matches!(lost.as_slice(), [ms] if (30_900..40_000).contains(ms)),
            "one break, naming the gap less the chunk's own 0.1 s: {lost:?}"
        );
        assert!(
            matches!(stalled.last(), Some(Event::Audio(a)) if a.first() == Some(&2)),
            "the break goes ahead of the audio that resumes"
        );
        let short = after(Duration::from_secs(29));
        assert!(
            longest(&short) >= samples(Duration::from_secs(28)),
            "a gap inside the bound is still silence: {} samples",
            longest(&short)
        );
    }

    /// Fable F1: the tap sends nothing while nothing plays. A gap in its open stream goes on as that much silence,
    /// ahead of the audio that resumes after it, so the side's samples keep the wall clock's time; the mic, which
    /// streams in silence, gets none. Run without a heartbeat dir, as the shadow runs it.
    #[test]
    fn a_gap_in_the_tap_is_passed_on_as_silence() {
        let pause = Duration::from_millis(500);
        let passed = |side: Side| {
            let (tx, rx) = mpsc::channel();
            let mut beating = Beating::new(rx, side, None);
            let mut got = Vec::new();
            let mut take = |ev: Event| {
                let _sent = tx.send(ev);
                got.push(beating.next());
            };
            take(speakers());
            // From before the first audio: the gap is counted from when that audio was taken, so however the
            // thread is scheduled, no more than this passed.
            let before = Instant::now();
            take(Event::Audio(vec![1; 1600]));
            std::thread::sleep(pause);
            take(Event::Audio(vec![2; 1600]));
            let most = samples(before.elapsed());
            drop(tx);
            got.extend(beating.by_ref().map(Some));
            (got, most)
        };
        let (them, most) = passed(Side::Them);
        let filled: Vec<usize> = them
            .iter()
            .filter_map(|ev| match ev {
                Some(Event::Audio(a)) if a.iter().all(|&s| s == 0) => Some(a.len()),
                _ => None,
            })
            .collect();
        // The gap is the pause less the resumed chunk's own 0.1 s; no more than the time that really passed.
        let least = samples(pause.saturating_sub(span(1600)));
        assert_eq!(filled.len(), 1, "one silence: {filled:?}");
        let n = filled.first().copied().unwrap_or(0);
        assert!(n >= least && n <= most, "{least} <= {n} <= {most}");
        assert!(
            matches!(them.last(), Some(Some(Event::Audio(a))) if a.first() == Some(&2)),
            "the silence goes ahead of the resumed audio"
        );
        let (me, _) = passed(Side::Me);
        assert_eq!(
            me,
            [
                Some(speakers()),
                Some(Event::Audio(vec![1; 1600])),
                Some(Event::Audio(vec![2; 1600])),
            ],
            "the mic: no silence added"
        );
    }

    /// The next silent check: whether it beat. A guard against a wedge, not a timing claim: one comes every 50 ms.
    fn tick(ticks: &mpsc::Receiver<bool>) -> Option<bool> {
        ticks.recv_timeout(Duration::from_secs(60)).ok()
    }

    /// A tap reopened again and again whose stream fails each time just after it started (`Device`, then at once
    /// `StreamError`: the lid stays closed) does not beat: no silent check beats, `.alive` is never written. Between
    /// two reopens a silent check passes, so at least the beat's interval has gone by when the next `Device` comes.
    #[test]
    fn a_tap_that_fails_just_after_each_start_does_not_beat() {
        let (beats, written) = tap_beating("beat-each-start-fails", |tx, ticks, written| {
            let beats: Vec<Option<bool>> = (0..5)
                .map(|_| {
                    let _sent = tx.send(speakers());
                    let _sent = tx.send(stopped());
                    tick(ticks)
                })
                .collect();
            (beats, written())
        });
        assert_eq!(beats, [Some(false); 5], "no silent check beats");
        assert!(!written, ".alive never written");
    }

    /// A tap whose stream lives past one beat interval, dies, is reopened and dies again, over and over (each stream
    /// is up for one silent check, then fails) beats while that is new, and stops beating once it has failed
    /// [`FLAPS`] times within [`FLAP_WINDOW`], so the tray sees the side go stale (DEGRADED). The cycles here are
    /// far inside the window.
    #[test]
    fn a_tap_that_keeps_failing_stops_beating() {
        let beats = tap_beating("beat-flaps", |tx, ticks, _| {
            (0..6)
                .map(|_| {
                    let _sent = tx.send(speakers());
                    // Up for a silent check: it beats, unless the side flaps.
                    let up = tick(ticks);
                    let _sent = tx.send(stopped());
                    let down = tick(ticks);
                    (up, down)
                })
                .collect::<Vec<_>>()
        });
        let ups: Vec<Option<bool>> = beats.iter().map(|&(up, _)| up).collect();
        assert_eq!(
            ups,
            [
                Some(true),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(false)
            ],
            "beats until its third failure, then not"
        );
        assert!(
            beats.iter().all(|&(_, down)| down == Some(false)),
            "never while down: {beats:?}"
        );
    }

    /// A side that flapped beats again once a stream holds ([`HOLD`], 300 ms here), long before its errors leave
    /// [`FLAP_WINDOW`]: three errors on open streams, no beat; then a stream that stays up, and a beat comes.
    #[test]
    fn a_stream_that_holds_ends_the_flapping() {
        let hold = Duration::from_millis(300);
        let (first, beat) = tap_beating_holding("beat-holds", hold, |tx, ticks, _| {
            let errors = Instant::now();
            for _ in 0..3 {
                let _sent = tx.send(speakers());
                let _up = tick(ticks);
                let _sent = tx.send(stopped());
            }
            let _sent = tx.send(speakers());
            let first = tick(ticks);
            // Silent checks until one beats, each wait a 60 s guard against a wedge. Without the hold the beat
            // still comes, but only once the first error has left FLAP_WINDOW: when it came is what is asserted.
            let beat = (0..)
                .find_map(|_| tick(ticks).map_or(Some(None), |b| b.then(|| Some(errors.elapsed()))))
                .flatten();
            (first, beat)
        });
        assert_eq!(first, Some(false), "no beat while it flaps");
        assert!(
            beat.is_some_and(|after| after < FLAP_WINDOW),
            "a beat once the stream held, before the errors leave FLAP_WINDOW: {beat:?} after the first error"
        );
    }

    /// One buffer of 16 kHz audio (10 ms).
    fn buffer() -> Event {
        Event::Audio(vec![0; 160])
    }

    /// #0497 (akira A2): a stream's beat is earned by that stream's own audio. One that opens, delivers one buffer
    /// and dies has not beaten: the count of samples starts at its `Device`, it is not carried over from before
    /// (nor from the side's start, when it stood at a whole beat so that the first buffer beat at once).
    #[test]
    fn a_stream_that_dies_one_buffer_after_opening_never_beat() {
        let state = temp_state("beat-one-buffer");
        for side in [Side::Me, Side::Them] {
            let dir = state.join(side.as_str());
            let alive = dir.join(tulpa_segment::ALIVE);
            let (tx, rx) = mpsc::channel();
            let mut beating = Beating::new(rx, side, Some(dir));
            for ev in [
                speakers(),
                buffer(),
                stopped(),
                speakers(),
                buffer(),
                stopped(),
            ] {
                let _sent = tx.send(ev);
                let _passed = beating.next();
            }
            assert!(!alive.exists(), "{}: a beat from one buffer", side.as_str());
        }
        let _gone = std::fs::remove_dir_all(&state);
    }

    /// #0497 (akira A2, s3 r10): the flaps are counted in a window, and a side that flapped beats again once its
    /// errors have left it, without having to hold for [`HOLD`] first. Three errors on open streams: no beat; the
    /// stream then stays up, and the beat comes back when the window (300 ms here) has passed, long before the hold.
    #[test]
    fn beats_come_back_once_the_errors_leave_the_flap_window() {
        let window = Duration::from_millis(300);
        let (first, beat) = tap_beating_within("beat-window", HOLD, window, |tx, ticks, _| {
            for _ in 0..3 {
                let _sent = tx.send(speakers());
                let _up = tick(ticks);
                let _sent = tx.send(stopped());
            }
            let errors = Instant::now();
            let _sent = tx.send(speakers());
            let first = tick(ticks);
            // Silent checks until one beats, each wait a 60 s guard against a wedge.
            let beat = (0..)
                .find_map(|_| tick(ticks).map_or(Some(None), |b| b.then(|| Some(errors.elapsed()))))
                .flatten();
            (first, beat)
        });
        assert_eq!(first, Some(false), "no beat while it flaps");
        assert!(
            beat.is_some_and(|after| after < HOLD),
            "a beat once the errors left the window, long before the stream held: {beat:?} after the last error"
        );
    }

    /// The count toward a beat starts again at each `Device` (akira F2): a stream that delivered one sample short
    /// of a beat and failed leaves nothing to the stream that opens after it, whose first buffer does not beat.
    #[test]
    fn samples_before_a_device_do_not_count_toward_the_next_streams_beat() {
        let state = temp_state("beat-reset");
        for side in [Side::Me, Side::Them] {
            let dir = state.join(side.as_str());
            let alive = dir.join(tulpa_segment::ALIVE);
            let (tx, rx) = mpsc::channel();
            let mut beating = Beating::new(rx, side, Some(dir));
            let nearly = Event::Audio(vec![0; BEAT - 1]);
            for ev in [speakers(), nearly, stopped(), speakers(), buffer()] {
                let _sent = tx.send(ev);
                let _passed = beating.next();
            }
            assert!(
                !alive.exists(),
                "{}: the old stream's samples beat for the new one",
                side.as_str()
            );
        }
        let _gone = std::fs::remove_dir_all(&state);
    }

    /// A tap whose every retry delivers one buffer while `play()` runs and then fails before its `Device` is sent
    /// (the error came before the start's check): no stream is open, so the side never beats — not on that audio,
    /// not on a silent check — and its samples are still passed on.
    #[test]
    fn audio_before_any_device_does_not_beat() {
        let (ticks, written) = tap_beating("beat-audio-no-device", |tx, ticks, written| {
            let ticks: Vec<Option<bool>> = (0..5)
                .map(|_| {
                    let _sent = tx.send(buffer());
                    let _sent = tx.send(stopped());
                    tick(ticks)
                })
                .collect();
            (ticks, written())
        });
        assert_eq!(ticks, [Some(false); 5], "no silent check beats");
        assert!(!written, ".alive never written");
    }

    /// Three lives that each open, deliver a beat's worth of audio and die before the silent interval: the audio
    /// beats while the side is new, and not once it flaps (after the third failure).
    #[test]
    fn audio_does_not_beat_once_the_side_flaps() {
        let beat = Event::Audio(vec![0; tulpa_segment::BEAT]);
        let (while_new, after_third) = tap_beating("beat-audio-flaps", |tx, ticks, written| {
            for _ in 0..3 {
                let _sent = tx.send(speakers());
                let _sent = tx.send(beat.clone());
                let _sent = tx.send(stopped());
            }
            // Every event so far taken (a silent check comes only after a quiet interval); what they wrote, away.
            let _quiet = tick(ticks);
            let while_new = written();
            let _sent = tx.send(speakers());
            let _sent = tx.send(beat.clone());
            let _sent = tx.send(stopped());
            let _quiet = tick(ticks);
            (while_new, written())
        });
        assert!(while_new, "the audio beat while the side was new");
        assert!(!after_third, "no beat on audio once the side flaps");
    }

    /// Starts that fail before any stream ran (`play()` failed: a `StreamError` with no `Device`, as after a display
    /// sleep or on a pending grant) are not flaps: three of them, then a stream that opens, and the side beats at
    /// its first due check.
    #[test]
    fn failed_starts_are_not_flaps() {
        let up = tap_beating("beat-failed-starts", |tx, ticks, _| {
            for _ in 0..3 {
                let _sent = tx.send(stopped());
            }
            let _sent = tx.send(speakers());
            tick(ticks)
        });
        assert_eq!(up, Some(true), "the first check after the Device beats");
    }

    /// A switch starts the count again: a broken device's flaps do not pass to the next one. Two flaps, a switch,
    /// one error on the new device, and the side still beats once that device opens again.
    #[test]
    fn a_switch_starts_the_flap_count_again() {
        let up = tap_beating("beat-switch-flaps", |tx, ticks, _| {
            for _ in 0..2 {
                let _sent = tx.send(speakers());
                let _up = tick(ticks);
                let _sent = tx.send(stopped());
                let _down = tick(ticks);
            }
            let _sent = tx.send(Event::Switched {
                from: Some("Speakers".to_owned()),
                to: "Headset".to_owned(),
            });
            let _sent = tx.send(speakers());
            let _up = tick(ticks);
            let _sent = tx.send(stopped());
            let _sent = tx.send(speakers());
            tick(ticks)
        });
        assert_eq!(
            up,
            Some(true),
            "still beating after one error on the new device"
        );
    }

    /// Forwarding, the silent tap beats only while its pipeline holds the token: a pipeline that is gone (ended,
    /// or panicked) gets no events to fail a send on, so the token is what stops the beat.
    #[test]
    fn a_silent_tap_stops_beating_when_its_pipeline_is_gone() {
        let state = temp_state("beat-reader");
        let beats = |pipeline_alive: bool| {
            let dir = state.join(format!("them-{pipeline_alive}"));
            let alive = dir.join(tulpa_segment::ALIVE);
            let (tx, rx) = mpsc::channel();
            let (to, _pipeline_rx) = mpsc::channel();
            let token = Arc::new(());
            let reader = Arc::downgrade(&token);
            if !pipeline_alive {
                drop(token);
            }
            let (ticks_tx, ticks) = mpsc::channel();
            let beating = Beating::new(rx, Side::Them, Some(dir))
                .with_every(Duration::from_millis(50), ticks_tx);
            let forwarding = std::thread::spawn(move || beating.forward(&to, reader));
            let _sent = tx.send(Event::Device {
                name: "Speakers".to_owned(),
                rate: 48_000,
                channels: 2,
            });
            // The Device is taken before any wait times out: the next three silent checks come after it.
            let beat = three_checks(&ticks);
            drop(tx);
            let _joined = forwarding.join();
            assert_eq!(beat, alive.exists(), "a beat is a rewritten .alive");
            beat
        };
        assert!(beats(true), "open, silent, read");
        assert!(!beats(false), "the pipeline is gone");
        let _gone = std::fs::remove_dir_all(&state);
    }

    /// The tap sends nothing while nothing plays: its side beats on time while its device is open, not before it
    /// opens, not after it is gone, switched away (until the new one opens) or reported a stream error; the mic,
    /// silent the same way, does not.
    #[test]
    fn a_silent_open_tap_still_beats() {
        let state = temp_state("beat-silent");
        let device = || Event::Device {
            name: "Speakers".to_owned(),
            rate: 48_000,
            channels: 2,
        };
        // The events, each taken in turn; then three silent checks, then the capture ends: whether the quiet after
        // the events beat.
        let quiet_beat = |side: Side, events: Vec<Event>| {
            let dir = state.join(format!("{}-{}", side.as_str(), events.len()));
            let alive = dir.join(tulpa_segment::ALIVE);
            let (tx, rx) = mpsc::channel();
            let (ticks_tx, ticks) = mpsc::channel();
            let mut beating =
                Beating::new(rx, side, Some(dir)).with_every(Duration::from_millis(50), ticks_tx);
            for ev in events {
                let _sent = tx.send(ev);
                let _passed = beating.next();
            }
            let _gone = std::fs::remove_file(&alive);
            let ender = std::thread::spawn(move || {
                let beat = three_checks(&ticks);
                drop(tx);
                beat
            });
            // Only the tap's filled silence may come before the end (a check slowed past GAP fills it); the mic
            // gets none at all.
            let zeros = |ev: &Event| matches!(ev, Event::Audio(a) if a.iter().all(|&s| s == 0));
            assert!(
                beating.by_ref().all(|ev| side == Side::Them && zeros(&ev)),
                "nothing else passes"
            );
            let beat = ender.join().unwrap_or(false);
            assert_eq!(beat, alive.exists(), "a beat is a rewritten .alive");
            beat
        };
        assert!(quiet_beat(Side::Them, vec![device()]), "open and silent");
        assert!(!quiet_beat(Side::Them, vec![]), "never opened");
        assert!(
            !quiet_beat(Side::Them, vec![device(), Event::NoInput]),
            "the device went away"
        );
        assert!(
            !quiet_beat(Side::Me, vec![device()]),
            "a silent mic is stale"
        );
        let switched = || Event::Switched {
            from: Some("Speakers".to_owned()),
            to: "Headset".to_owned(),
        };
        assert!(
            !quiet_beat(Side::Them, vec![device(), switched()]),
            "switched, and the new device never opened"
        );
        assert!(
            quiet_beat(Side::Them, vec![device(), switched(), device()]),
            "switched, and the new device opened"
        );
        assert!(
            !quiet_beat(
                Side::Them,
                vec![device(), Event::StreamError("stopped".to_owned())]
            ),
            "the stream reported an error"
        );
        let _gone = std::fs::remove_dir_all(&state);
    }

    /// `t0` plus `s` seconds: the tests' clock.
    fn at(t0: Instant, s: u64) -> Instant {
        t0.checked_add(Duration::from_secs(s)).unwrap_or(t0)
    }

    /// The output runs past the bound and the tap delivers nothing, or only zeros: said exactly once.
    /// Kills: `heard` taking zeros for sound (none said); `due` not marking the stretch said (said at every look).
    #[test]
    fn a_running_output_the_tap_does_not_hear_is_said_once() {
        let t0 = Instant::now();
        let mut deaf = Deaf::new(DEAF_AFTER);
        deaf.running(Some(true), t0);
        deaf.heard(&[0; 1600], at(t0, 3));
        assert_eq!(deaf.due(at(t0, 9)), None, "not yet");
        deaf.heard(&[0; 1600], at(t0, 9));
        deaf.running(Some(true), at(t0, 9));
        assert_eq!(
            deaf.due(at(t0, 11)),
            Some(Duration::from_secs(11)),
            "zeros and a repeated running state are no new stretch"
        );
        assert_eq!(deaf.due(at(t0, 12)), None, "once");
        assert_eq!(deaf.due(at(t0, 60)), None, "once per stretch");
    }

    /// Sound from the tap ends the stretch: nothing is said while it keeps coming, and silence after it is a new
    /// stretch, said once more. Kills: `heard` not moving the stretch's start (said 2 s after the sound); `heard`
    /// not clearing `said` (the new stretch never said).
    #[test]
    fn sound_ends_the_stretch_and_a_new_one_is_said_again() {
        let t0 = Instant::now();
        let mut deaf = Deaf::new(DEAF_AFTER);
        deaf.running(Some(true), t0);
        deaf.heard(&[0, 0, 7, 0], at(t0, 8));
        assert_eq!(deaf.due(at(t0, 12)), None, "the tap heard it");
        assert_eq!(
            deaf.due(at(t0, 18)),
            Some(Duration::from_secs(10)),
            "silent again for the bound"
        );
        deaf.heard(&[-1], at(t0, 19));
        assert_eq!(deaf.due(at(t0, 25)), None, "heard again");
        assert_eq!(
            deaf.due(at(t0, 29)),
            Some(Duration::from_secs(10)),
            "a new stretch is said"
        );
        assert_eq!(deaf.due(at(t0, 40)), None, "and only once");
    }

    /// The output stops before the bound: no stretch, nothing said, however long after. Kills: `running(Some(false))`
    /// leaving the stretch open (said at 20 s).
    #[test]
    fn an_output_that_stops_before_the_bound_is_not_said() {
        let t0 = Instant::now();
        let mut deaf = Deaf::new(DEAF_AFTER);
        deaf.running(Some(true), t0);
        deaf.running(Some(false), at(t0, 5));
        assert_eq!(deaf.due(at(t0, 20)), None, "stopped at 5 s");
        assert_eq!(deaf.due(at(t0, 600)), None);
        // Positive control: running again is a stretch of its own, from its own start.
        deaf.running(Some(true), at(t0, 600));
        assert_eq!(deaf.due(at(t0, 609)), None);
        assert_eq!(deaf.due(at(t0, 610)), Some(Duration::from_secs(10)));
    }

    /// An unknown running state (a read `CoreAudio` refused) ends the stretch as a stop does: nothing said from it,
    /// however long; only a state read as running starts one again. Kills: `running(None)` keeping the stretch
    /// (said at 20 s), or starting one (said at 610 s from 600 s).
    #[test]
    fn an_unknown_running_state_ends_the_stretch() {
        let t0 = Instant::now();
        let mut deaf = Deaf::new(DEAF_AFTER);
        deaf.running(Some(true), t0);
        deaf.running(None, at(t0, 5));
        assert_eq!(deaf.due(at(t0, 20)), None, "unknown at 5 s");
        assert_eq!(deaf.due(at(t0, 600)), None);
        deaf.running(None, at(t0, 600));
        assert_eq!(deaf.due(at(t0, 610)), None, "unknown starts nothing");
        // Positive control: read as running again, a stretch of its own.
        deaf.running(Some(true), at(t0, 610));
        assert_eq!(deaf.due(at(t0, 619)), None);
        assert_eq!(deaf.due(at(t0, 620)), Some(Duration::from_secs(10)));
    }

    /// Audio before the tap's stream is open (no Device yet) does not count as heard: the stretch the running
    /// output started keeps its start. Kills: `heard` fed outside the open-stream gate (the start moves to the
    /// sound).
    #[test]
    fn only_an_open_streams_audio_resets_the_stretch() {
        let (tx, rx) = mpsc::channel();
        let mut beating = Beating::new(rx, Side::Them, None);
        for ev in [
            Event::OutputRunning(Some(true)),
            Event::Audio(vec![0, 9]),
            speakers(),
            Event::Audio(vec![0, 9]),
        ] {
            let _sent = tx.send(ev);
        }
        assert!(matches!(beating.next(), Some(Event::OutputRunning(_))));
        let started = beating.deaf.since;
        assert!(started.is_some(), "running starts a stretch");
        // Heard sound starts a new stretch, unsaid: marked said, the stretch shows whether a sound was heard by
        // what the code did, not by the order of two clock readings (they can be equal).
        beating.deaf.said = true;
        assert!(matches!(beating.next(), Some(Event::Audio(_))));
        assert!(
            beating.deaf.said && beating.deaf.since == started,
            "audio with no Device is not heard"
        );
        assert!(matches!(beating.next(), Some(Event::Device { .. })));
        assert!(matches!(beating.next(), Some(Event::Audio(_))));
        // Positive control: the open stream's sound starts a new stretch.
        assert!(!beating.deaf.said, "an open stream's sound is heard");
    }

    /// The warnings `events`, fed to an open-or-not tap's `Beating` that says a deaf tap at once, pass on.
    fn deaf_warnings(events: Vec<Event>) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        for ev in events {
            let _sent = tx.send(ev);
        }
        drop(tx);
        Beating::new(rx, Side::Them, None)
            .with_deaf_after(Duration::ZERO)
            .filter_map(|ev| match ev {
                Event::Warning(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    /// Through the side's real adapter: the tap's running output and its audio decide the warning, said in the
    /// pipeline's `warning:` shape; only an open tap's. Kills: the `OutputRunning` arm doing nothing, the `Audio`
    /// arm not feeding the state, no `deaf_line` on the event path (none said: no event wait times out here), no
    /// open-tap gate, and a stream error that keeps the stretch (the reopened tap's is not said).
    #[test]
    fn the_tap_adapter_says_a_deaf_tap_from_its_events() {
        let said = "output playing for 0 s but the tap hears nothing";
        let zeros = || Event::Audio(vec![0; 160]);
        assert_eq!(
            deaf_warnings(vec![
                speakers(),
                Event::OutputRunning(Some(true)),
                zeros(),
                zeros()
            ]),
            [said],
            "running, only zeros: once"
        );
        assert_eq!(
            deaf_warnings(vec![
                speakers(),
                Event::OutputRunning(Some(true)),
                zeros(),
                Event::Audio(vec![0, 9]),
                zeros(),
            ]),
            [said, said],
            "sound starts a new stretch"
        );
        assert!(
            deaf_warnings(vec![speakers(), zeros(), Event::Audio(vec![0, 9])]).is_empty(),
            "an output not running is no stretch"
        );
        assert!(
            deaf_warnings(vec![Event::OutputRunning(Some(true)), zeros()]).is_empty(),
            "no open tap, nothing said"
        );
        assert_eq!(
            deaf_warnings(vec![
                speakers(),
                Event::OutputRunning(Some(true)),
                Event::StreamError("stopped".to_owned()),
                zeros(),
                speakers(),
                zeros(),
                Event::OutputRunning(Some(true)),
                zeros(),
            ]),
            [said, said],
            "a reopened tap waits for its own listener's state, and is a stretch of its own"
        );
        assert!(
            deaf_warnings(vec![speakers(), Event::OutputRunning(None), zeros()]).is_empty(),
            "an unknown state is no stretch"
        );
    }
}
