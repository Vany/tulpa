//! Which device the ears listen to: the macOS default input (the mic) or default output (the system-audio tap),
//! followed as it changes, or for the mic one pinned by name.
//!
//! [`Follower`] is a pure state machine over [`DeviceHost`], so the rules are tested against a fake host and the
//! `CoreAudio` adapters only answer two questions, once for inputs and once for outputs. [`follow_once`] acts on its
//! [`Decision`] through a [`Source`] (open the capture, drop it, say so), and [`drive`] repeats that on every device
//! change the caller's listener reports (`tulpa-audio-events` in the mic and the tap), until stopped; the mic and the
//! tap share both. A change of the default device, or a device added or removed, is an event, and so is a stream's
//! error (its capture's failure mark, [`Source::failure`]); the one clock is [`RETRY_AFTER`], for a look that cannot
//! wait on an event (an open that failed, a stream that failed, devices that could not be read).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tulpa_stop::{Got, Receiver, Stop};

/// How soon the follower looks again after devices that could not be read, and the soonest a device is opened again
/// after its last attempt ended, when that open failed or its stream has since.
///
/// After [`Opening::Retry`], a stream's error or [`Decision::Unreadable`] no event says when the look would work (the
/// device listener only says when the devices change). The one delay for all three, so a device that fails at every
/// start is tried every 2 s, never in a loop; the only clock in [`drive`].
pub const RETRY_AFTER: Duration = Duration::from_secs(2);

/// The two questions the follower asks of the audio system, about one direction (inputs, or outputs).
pub trait DeviceHost {
    /// The name of the current default device, or `None` when there is none.
    ///
    /// # Errors
    ///
    /// The devices could not be read (said as a line for stderr): not the same as no device.
    fn default_name(&self) -> Result<Option<String>, String>;
    /// The names of all devices present now, and whether a device's name was not read.
    ///
    /// # Errors
    ///
    /// As [`DeviceHost::default_name`]: a list that was not read is not an empty one.
    fn names(&self) -> Result<Names, String>;
    /// The audio system's id of the device called `name` now, if it gives one.
    ///
    /// A device that is unplugged and plugged back, or that the audio system makes anew after a restart, comes
    /// back under the same name with another id: the stream that was open on the old one is dead, with no error
    /// reported and no change of name to see. The follower opens the device again when its id changes. `None`
    /// (the default): no id is known, and only the name is followed.
    fn id_of(&self, _name: &str) -> Option<u64> {
        None
    }
    /// The name the device with audio system id `id` has now ([`DeviceHost::id_of`]'s inverse), if it is there.
    ///
    /// A device renamed (in Audio MIDI Setup) keeps its id: the follower stays on it under its new name, pinned or
    /// not. `None` (the default): no id is known, and a pinned name that went away is missing.
    fn name_of(&self, _id: u64) -> Option<String> {
        None
    }
}

/// A device list that was read: the names read, and whether a device was skipped because its name was not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Names {
    /// The names read.
    pub read: Vec<String>,
    /// Whether a device's name was not read (that device is not in `read`).
    pub skipped: bool,
}

impl Names {
    /// The names of a list whose every name was read.
    #[must_use]
    pub const fn full(read: Vec<String>) -> Self {
        Self {
            read,
            skipped: false,
        }
    }

    /// The names read, and whether a device was skipped.
    #[must_use]
    pub const fn new(read: Vec<String>, skipped: bool) -> Self {
        Self { read, skipped }
    }
}

/// The device to listen to. The tap always follows the [`Default`](Mic::Default) output.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mic {
    /// Follow the system default input.
    Default,
    /// Always this device, by exact name.
    Pinned(String),
}

impl Mic {
    /// Reads `EARS_MIC`'s value: `":Name"` or `"Name"` pins `Name`; unset, empty or a bare `":"` follows the default.
    pub fn from_env_value(value: Option<&str>) -> Self {
        let name = value.map_or("", |v| v.strip_prefix(':').unwrap_or(v));
        if name.is_empty() {
            Self::Default
        } else {
            Self::Pinned(name.to_owned())
        }
    }
}

/// Whether the mic goes through Apple's voice processing or the plain input.
///
/// Voice processing today means Apple's noise suppression and gain control only. It does not cancel our own speech:
/// its echo reference is its own output unit, which we feed silence. The `.close` marker stays the echo guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Aec {
    /// The plain `CoreAudio` input (the default).
    Off,
    /// The `VoiceProcessingIO` audio unit.
    On,
}

impl Aec {
    /// Reads `EARS_MIC_AEC`'s value: unset, empty or `0` is [`Aec::Off`]; `1` is [`Aec::On`].
    ///
    /// # Errors
    ///
    /// [`FollowError::BadAec`] for any other value. Never a silent fallback to off.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, FollowError> {
        match value {
            None | Some("" | "0") => Ok(Self::Off),
            Some("1") => Ok(Self::On),
            Some(v) => Err(FollowError::BadAec(v.to_owned())),
        }
    }

    /// `on` or `off`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// What the caller should do after a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    /// Nothing changed; keep the current stream (or keep waiting for an input).
    Stay,
    /// Open this device: the first poll found it, or it is still the device and was marked to be opened again
    /// ([`Follower::reopen`]).
    Open(String),
    /// The device came back under the same name with another id ([`DeviceHost::id_of`]): the stream on the old
    /// one is dead, though nothing reported it. Drop it, say the break, and open the device again.
    Replaced(String),
    /// The device changed: drop the stream and open `to`. `from` is `None` when the ears had no input before.
    Switch {
        /// The device listened to until now, if any.
        from: Option<String>,
        /// The device to open.
        to: String,
    },
    /// There is no default device at all: drop the stream and say so. Returned once per outage; the polls after it
    /// [`Stay`](Decision::Stay) until a device appears, which is a [`Switch`](Decision::Switch).
    NoInput,
    /// The devices could not be read: keep the stream as it is, say this once, and look again at [`RETRY_AFTER`].
    /// Polls that still cannot read them [`Stay`](Decision::Stay) (and look again at the same clock).
    Unreadable(String),
}

/// Why the follower cannot go on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FollowError {
    /// The pinned device is not among the inputs, as read. Never a silent fallback to the default.
    PinnedMissing(String),
    /// The device listener went away while the follower still waited on it: no change would be seen any more.
    ListenerGone,
    /// `EARS_MIC_AEC` is neither unset, empty, `0` nor `1`.
    BadAec(String),
}

impl fmt::Display for FollowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PinnedMissing(name) => {
                write!(
                    f,
                    "pinned input device \"{name}\" (EARS_MIC) is not present"
                )
            }
            Self::ListenerGone => f.write_str("the audio device listener went away"),
            Self::BadAec(v) => write!(
                f,
                "EARS_MIC_AEC=\"{v}\" is not allowed: use 1 (on), or 0, empty or unset (off)"
            ),
        }
    }
}

impl std::error::Error for FollowError {}

/// The follower's state: what it pins, and what it last told the caller to listen to.
#[derive(Debug, Clone)]
pub struct Follower {
    mic: Mic,
    current: Option<String>,
    /// The id `current` had when it was last told to be opened ([`DeviceHost::id_of`]); `None`: not known.
    current_id: Option<u64>,
    started: bool,
    /// The last poll could not read the devices (and said so): look again at [`RETRY_AFTER`].
    unreadable: bool,
    /// `current` is to be opened again at the next poll, if it is still the device.
    reopen: bool,
    /// No reopen of `current` before this instant ([`Follower::reopen_not_before`]); a switch or no input is not held.
    not_before: Option<Instant>,
}

impl Follower {
    /// A follower that has not polled yet.
    pub const fn new(mic: Mic) -> Self {
        Self {
            mic,
            current: None,
            current_id: None,
            started: false,
            unreadable: false,
            reopen: false,
            not_before: None,
        }
    }

    /// The device the caller was last told to listen to.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Marks the current device to be opened again (its open failed, or its stream did): the next poll says
    /// [`Decision::Open`] if it is still the device, and decides as ever if it is not — a switch from it, or no
    /// input. It stays current meanwhile, so nothing is said about a device that did not change.
    pub const fn reopen(&mut self) {
        self.reopen = true;
    }

    /// Holds the reopen [`Follower::reopen`] asks for until `at` (`None`: no hold): a poll before it says
    /// [`Decision::Stay`] for the same device and keeps the reopen pending. [`drive`] sets it to [`RETRY_AFTER`] after
    /// the last attempt ended, so a device event never reopens a failed device sooner.
    pub const fn reopen_not_before(&mut self, at: Option<Instant>) {
        self.not_before = at;
    }

    /// Whether a reopen of the current device is pending ([`Follower::reopen`], not yet taken by a poll).
    pub const fn reopening(&self) -> bool {
        self.reopen
    }

    /// Whether the last poll could not read the devices: look again at [`RETRY_AFTER`] from now.
    pub const fn unreadable(&self) -> bool {
        self.unreadable
    }

    /// Whether a look is due on the clock rather than on an event: a reopen is pending, or the devices were not read.
    pub const fn retrying(&self) -> bool {
        self.reopen || self.unreadable
    }

    /// Asks `host` and decides.
    ///
    /// # Errors
    ///
    /// [`FollowError::PinnedMissing`] whenever a pinned device is not among the inputs of a list read in full, at the
    /// first poll or later. Absent while a device's name was not read, it may be that device: the list counts as
    /// unreadable ([`Decision::Unreadable`], looked at again at [`RETRY_AFTER`]), not as missing. A pinned device
    /// renamed is not missing: once opened, the pin follows its id ([`DeviceHost::name_of`]), whatever it is called.
    pub fn poll(&mut self, host: &impl DeviceHost) -> Result<Decision, FollowError> {
        let read = match &self.mic {
            Mic::Pinned(name) => match host.names() {
                Ok(names) => {
                    let by_id = self
                        .current_id
                        .and_then(|id| host.name_of(id))
                        .filter(|now| names.read.contains(now));
                    if by_id.is_some() {
                        Ok(by_id)
                    } else if names.read.contains(name) {
                        Ok(Some(name.clone()))
                    } else if names.skipped {
                        Err(format!(
                            "{name}: not among the inputs read, and a device's name was not read"
                        ))
                    } else {
                        return Err(FollowError::PinnedMissing(name.clone()));
                    }
                }
                Err(e) => Err(e),
            },
            Mic::Default => host.default_name(),
        };
        let seen = match read {
            Ok(seen) => seen,
            Err(e) => {
                let said = std::mem::replace(&mut self.unreadable, true);
                return Ok(if said {
                    Decision::Stay
                } else {
                    Decision::Unreadable(e)
                });
            }
        };
        self.unreadable = false;
        let first = !self.started;
        self.started = true;
        let reopen = std::mem::take(&mut self.reopen);
        let id = seen.as_deref().and_then(|name| host.id_of(name));
        // The same id under a new name: the device was renamed, not replaced or switched. Its stream goes on; only
        // the name it is opened by from now on changes.
        if seen.is_some() && self.current.is_some() && id.is_some() && id == self.current_id {
            self.current.clone_from(&seen);
        }
        if let Some(name) = seen.as_ref().filter(|_| reopen && seen == self.current) {
            // Held: the same device, before its retry delay ran out. Still pending, so it is neither lost nor taken.
            if self.not_before.is_some_and(|at| Instant::now() < at) {
                self.reopen = true;
                return Ok(Decision::Stay);
            }
            self.current_id = id;
            return Ok(Decision::Open(name.clone()));
        }
        // The same name under another id: the device was taken away and put back (or made anew) between two
        // looks, and the stream on the old one is dead without a word. Opened again, at once: it is a new device,
        // not a failed one, so no retry delay holds it.
        if let (Some(name), Some(was), Some(now)) = (seen.as_ref(), self.current_id, id)
            && seen == self.current
            && was != now
        {
            self.current_id = id;
            return Ok(Decision::Replaced(name.clone()));
        }
        if seen == self.current {
            // Same device, or still none: nothing to do. A first poll with no input is still loud.
            return Ok(if first && seen.is_none() {
                Decision::NoInput
            } else {
                Decision::Stay
            });
        }
        let from = std::mem::replace(&mut self.current, seen.clone());
        self.current_id = id;
        Ok(match seen {
            None => Decision::NoInput,
            Some(to) if first => Decision::Open(to),
            Some(to) => Decision::Switch { from, to },
        })
    }
}

/// The names of a list that was read, skipping each device whose name was not, and whether one was.
///
/// One unreadable name must not hide the rest. A skip is said on stderr once per reason in `skipped` (`cpal` gives no other handle
/// on a device it cannot name). A pinned device absent beside a skip may be the skipped one, so the follower does not
/// call it missing: it looks again at `RETRY_AFTER`, and only a list read in full says it is gone
/// ([`Follower::poll`]).
pub fn names_kept(
    names: impl Iterator<Item = Result<String, String>>,
    what: &str,
    skipped: &Mutex<BTreeSet<String>>,
) -> Names {
    let mut any = false;
    let read = names
        .filter_map(|name| {
            name.map_err(|why| {
                any = true;
                crate::warn_once(skipped, &format!("tulpa-hear: {what}: skipped: {why}"));
            })
            .ok()
        })
        .collect();
    Names::new(read, any)
}

/// Whether the caller goes on: [`Flow::Stop`] once the event receiver has hung up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Flow {
    /// Keep following.
    Go,
    /// Nobody listens any more; stop quietly.
    Stop,
}

/// Why a capture did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Opening<E> {
    /// Warn and offer the device again [`RETRY_AFTER`] after the last attempt ended (it vanished, or the stream
    /// failed).
    Retry(String),
    /// Warn once and keep the device as current without a capture, until the follower moves on (the device can
    /// never be captured this way, so a retry at every look would only repeat the warning).
    Skip(String),
    /// Stop the capture altogether.
    Fatal(E),
}

/// Why [`follow_once`] or [`drive`] stopped with an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Halt<E> {
    /// The follower cannot go on.
    Follow(FollowError),
    /// [`Source::open`] said [`Opening::Fatal`].
    Fatal(E),
}

/// What a followed capture does when the [`Follower`] decides.
pub trait Source {
    /// An open capture; dropping it stops it.
    type Capture;
    /// What [`Opening::Fatal`] carries.
    type Error;
    /// Opens a capture on the device called `name`.
    ///
    /// # Errors
    ///
    /// An [`Opening`] saying how the driver goes on.
    fn open(&mut self, name: &str) -> Result<Self::Capture, Opening<Self::Error>>;
    /// `capture`'s failure mark: set (once) when its stream reports a `StreamError`, so it must be opened again;
    /// [`drive`] waits on it beside the device events. Each opened capture has its own, so an error a replaced
    /// capture reports late is never taken for the current one's. `None`: this capture reports no stream error.
    fn failure<'a>(&'a self, capture: &'a Self::Capture) -> Option<&'a Stop>;
    /// The follower moved from `from` (`None` after an outage) to `to`; the old capture is already dropped.
    fn switched(&mut self, from: Option<&str>, to: &str) -> Flow;
    /// There is no default device; the capture is already dropped.
    fn no_device(&mut self) -> Flow;
    /// Something went wrong without stopping the capture.
    fn warn(&mut self, message: String) -> Flow;
}

/// Polls `follower` once and acts on its decision: drops `capture` on a switch or an outage, tells `source`, and
/// opens the new device into `capture`.
///
/// An unchanged device does nothing at all, unless its capture has failed: that capture is dropped and the device
/// opened again.
///
/// # Errors
///
/// [`Halt::Follow`] from the follower; [`Halt::Fatal`] when the source cannot open in a way that must stop.
pub fn follow_once<S: Source>(
    follower: &mut Follower,
    host: &impl DeviceHost,
    source: &mut S,
    capture: &mut Option<S::Capture>,
) -> Result<Flow, Halt<S::Error>> {
    // A failed capture (cpal paused its stream: the lid closed, the display slept, the stream was invalidated) is
    // dropped at once and its device marked to be reopened, so this same call opens it again, as a failed open's
    // retry does (or switches, or says there is no input, if the device changed meanwhile). Under `drive` the
    // reopen itself is held until `RETRY_AFTER` after the last attempt ended (`Follower::reopen_not_before`).
    let failed = |c: &S::Capture| source.failure(c).is_some_and(Stop::is_set);
    if let Some(name) = follower
        .current()
        .filter(|_| capture.as_ref().is_some_and(failed))
    {
        let line = format!("\"{name}\": the stream failed; opening it again");
        *capture = None;
        follower.reopen();
        if source.warn(line) == Flow::Stop {
            return Ok(Flow::Stop);
        }
    }
    let to = match follower.poll(host).map_err(Halt::Follow)? {
        Decision::Stay => return Ok(Flow::Go),
        // The device to open is the current one (the first look, or a reopen): a capture still held is the old
        // one's, dropped before the new one opens, as on a switch.
        Decision::Open(to) => {
            *capture = None;
            to
        }
        Decision::Switch { from, to } => {
            *capture = None;
            if source.switched(from.as_deref(), &to) == Flow::Stop {
                return Ok(Flow::Stop);
            }
            to
        }
        // The same device, anew: a break in the stream like any other reopen's (a failed stream's error is one),
        // said as a switch from the device to itself. Without it the phrase before the gap would run on into the
        // speech after it, and the side's clock would keep counting from before the gap.
        Decision::Replaced(to) => {
            *capture = None;
            if source.switched(Some(&to), &to) == Flow::Stop {
                return Ok(Flow::Stop);
            }
            to
        }
        Decision::NoInput => {
            *capture = None;
            return Ok(source.no_device());
        }
        Decision::Unreadable(m) => return Ok(source.warn(m)),
    };
    match source.open(&to) {
        Ok(c) => {
            *capture = Some(c);
            Ok(Flow::Go)
        }
        Err(Opening::Fatal(e)) => Err(Halt::Fatal(e)),
        Err(Opening::Retry(m)) => {
            follower.reopen();
            Ok(source.warn(m))
        }
        Err(Opening::Skip(m)) => Ok(source.warn(m)),
    }
}

/// Runs [`follow_once`] now and again on every item from `changes` and when the open capture's stream fails, until
/// `stop` or [`Flow::Stop`].
///
/// An item is a device change; a burst of them is one look. A stream's error ([`Source::failure`]) wakes it too: the
/// failed capture is dropped at once, and its device is opened again no sooner than [`RETRY_AFTER`] after its last
/// attempt ended, as after an [`Opening::Retry`]; a device event meanwhile switches or says there is no input at
/// once, but does not reopen the same device sooner. Devices that could not be read are looked at again
/// [`RETRY_AFTER`] later. On the way out it drops the capture.
///
/// # Errors
///
/// As [`follow_once`]; [`FollowError::ListenerGone`] when `changes` ends.
pub fn drive<S: Source>(
    follower: Follower,
    host: &impl DeviceHost,
    source: &mut S,
    changes: &Receiver<()>,
    stop: &Stop,
) -> Result<(), Halt<S::Error>> {
    paced(follower, host, source, changes, stop, RETRY_AFTER)
}

/// [`drive`] with its retry delay given, so tests need not wait [`RETRY_AFTER`].
fn paced<S: Source>(
    mut follower: Follower,
    host: &impl DeviceHost,
    source: &mut S,
    changes: &Receiver<()>,
    stop: &Stop,
    retry_after: Duration,
) -> Result<(), Halt<S::Error>> {
    let mut tried = Tried { source, at: None };
    let mut capture = None;
    while !stop.is_set() {
        let retry_due = tried.at.and_then(|at| at.checked_add(retry_after));
        follower.reopen_not_before(retry_due);
        if follow_once(&mut follower, host, &mut tried, &mut capture)? == Flow::Stop {
            break;
        }
        // The last attempt may have just ended: the delay runs from there.
        let retry_due = tried.at.and_then(|at| at.checked_add(retry_after));
        let look_at = if follower.unreadable() {
            Instant::now().checked_add(retry_after)
        } else if follower.reopening() {
            retry_due
        } else {
            None
        };
        // The capture's own mark: a set one ends the wait at once, even if it was set before this wait began.
        let failure = capture.as_ref().and_then(|c| tried.failure(c));
        let waits: Vec<&Stop> = [Some(stop), failure].into_iter().flatten().collect();
        match changes.recv_until(&waits, look_at) {
            Got::Item(()) => while changes.try_recv().is_some() {},
            // `stop` ends the loop at its head; a failed capture is dropped by the next `follow_once`, so its set
            // mark is not waited on again.
            Got::Stopped | Got::Timeout => {}
            Got::Ended => return Err(Halt::Follow(FollowError::ListenerGone)),
        }
    }
    drop(capture);
    Ok(())
}

/// [`DeviceHost::default_name`] from a default lookup that cannot tell "none" from "failed".
///
/// `looked_up` is cpal's lookup, with the found device's name read; `names` reads the device list. cpal gives `None`
/// for "none" and for a lookup that failed; on macOS it gives no `None` at all: with no device, `CoreAudio` answers
/// device 0, whose name then fails (`Some(Err)`). So both, `None` and a name that failed, are "no device" only when
/// the list, read, is empty with no name skipped: the capture is dropped, and a device that comes back is opened
/// anew. With devices present, or a device whose name was not read, it is a lookup that failed: an error, so the
/// follower keeps the stream and looks again at [`RETRY_AFTER`] ([`Decision::Unreadable`]).
///
/// # Errors
///
/// The list's read error, or a `None` or failed name beside devices that are present or were skipped; a failed
/// name's error leads the text.
pub fn default_from(
    looked_up: Option<Result<String, String>>,
    names: impl FnOnce() -> Result<Names, String>,
) -> Result<Option<String>, String> {
    let failed = match looked_up {
        Some(Ok(name)) => return Ok(Some(name)),
        Some(Err(e)) => format!("{e}: "),
        None => String::new(),
    };
    let names = names().map_err(|e| format!("{failed}{e}"))?;
    let present = names.read.len();
    if present == 0 && !names.skipped {
        return Ok(None);
    }
    let skipped = if names.skipped {
        ", a name not read"
    } else {
        ""
    };
    Err(format!(
        "{failed}the default device was not read ({present} devices present{skipped})"
    ))
}

/// The [`Source`] as [`drive`] sees it: when its last open returned.
struct Tried<'s, S> {
    source: &'s mut S,
    at: Option<Instant>,
}

impl<S: Source> Source for Tried<'_, S> {
    type Capture = S::Capture;
    type Error = S::Error;
    fn open(&mut self, name: &str) -> Result<S::Capture, Opening<S::Error>> {
        let opened = self.source.open(name);
        // When the attempt ended, not when it began: a start that blocks (a pending grant) and then fails still
        // waits the whole retry delay before the next.
        self.at = Some(Instant::now());
        opened
    }
    fn failure<'a>(&'a self, capture: &'a S::Capture) -> Option<&'a Stop> {
        self.source.failure(capture)
    }
    fn switched(&mut self, from: Option<&str>, to: &str) -> Flow {
        self.source.switched(from, to)
    }
    fn no_device(&mut self) -> Flow {
        self.source.no_device()
    }
    fn warn(&mut self, message: String) -> Flow {
        self.source.warn(message)
    }
}

/// The one stderr line for a followed device change: `<who>: default <what> switched: <from> -> <to>`, `from` being
/// `none` after an outage.
pub fn switch_line(who: &str, what: &str, from: Option<&str>, to: &str) -> String {
    format!(
        "{who}: default {what} switched: {} -> {to}",
        from.unwrap_or("none")
    )
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use std::cell::RefCell;
    use std::convert::Infallible;
    use std::sync::Arc;

    use crate::beat::Beating;
    use crate::event::Event;
    use crate::hear::Side;
    use crate::start::{Held, Mark, report, say};

    #[derive(Default)]
    struct Fake {
        default: RefCell<Option<String>>,
        names: RefCell<Vec<String>>,
        unreadable: RefCell<bool>,
    }

    impl Fake {
        fn with(default: Option<&str>, names: &[&str]) -> Self {
            let f = Self::default();
            f.set(default, names);
            f
        }
        fn set(&self, default: Option<&str>, names: &[&str]) {
            *self.default.borrow_mut() = default.map(str::to_owned);
            *self.names.borrow_mut() = names.iter().map(|&n| n.to_owned()).collect();
        }
    }

    impl DeviceHost for Fake {
        fn default_name(&self) -> Result<Option<String>, String> {
            self.read().map(|()| self.default.borrow().clone())
        }
        fn names(&self) -> Result<Names, String> {
            self.read()
                .map(|()| Names::full(self.names.borrow().clone()))
        }
    }

    impl Fake {
        fn read(&self) -> Result<(), String> {
            if *self.unreadable.borrow() {
                Err("devices not read".to_owned())
            } else {
                Ok(())
            }
        }
    }

    fn switch(from: Option<&str>, to: &str) -> Decision {
        Decision::Switch {
            from: from.map(str::to_owned),
            to: to.to_owned(),
        }
    }

    #[test]
    fn unpinned_default_change_switches_exactly_once() {
        let host = Fake::with(Some("Built-in"), &["Built-in", "USB"]);
        let mut f = Follower::new(Mic::Default);
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("Built-in".into()));
        host.set(Some("USB"), &["Built-in", "USB"]);
        assert_eq!(f.poll(&host).unwrap(), switch(Some("Built-in"), "USB"));
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        assert_eq!(f.current(), Some("USB"));
    }

    #[test]
    fn same_default_does_nothing() {
        let host = Fake::with(Some("Built-in"), &["Built-in"]);
        let mut f = Follower::new(Mic::Default);
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("Built-in".into()));
        for _ in 0..5 {
            assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        }
    }

    #[test]
    fn pinned_present_never_switches() {
        let host = Fake::with(Some("Built-in"), &["Built-in", "USB"]);
        let mut f = Follower::new(Mic::from_env_value(Some(":USB")));
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("USB".into()));
        host.set(Some("AirPods"), &["Built-in", "USB", "AirPods"]);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        host.set(None, &["USB"]);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        assert_eq!(f.current(), Some("USB"));
    }

    #[test]
    fn pinned_missing_is_an_error_naming_it() {
        let host = Fake::with(Some("Built-in"), &["Built-in"]);
        let mut f = Follower::new(Mic::from_env_value(Some("Studio Mic")));
        let e = f.poll(&host).unwrap_err();
        assert_eq!(e, FollowError::PinnedMissing("Studio Mic".into()));
        assert!(e.to_string().contains("Studio Mic"));
    }

    #[test]
    fn pinned_device_vanishing_later_is_an_error() {
        let host = Fake::with(Some("USB"), &["USB"]);
        let mut f = Follower::new(Mic::Pinned("USB".into()));
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("USB".into()));
        host.set(Some("Built-in"), &["Built-in"]);
        assert_eq!(
            f.poll(&host).unwrap_err(),
            FollowError::PinnedMissing("USB".into())
        );
    }

    #[test]
    fn no_default_is_no_input_then_appearing_device_switches() {
        let host = Fake::with(None, &[]);
        let mut f = Follower::new(Mic::Default);
        assert_eq!(f.poll(&host).unwrap(), Decision::NoInput);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        host.set(Some("USB"), &["USB"]);
        assert_eq!(f.poll(&host).unwrap(), switch(None, "USB"));
        host.set(None, &[]);
        assert_eq!(f.poll(&host).unwrap(), Decision::NoInput);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        host.set(Some("Built-in"), &["Built-in"]);
        assert_eq!(f.poll(&host).unwrap(), switch(None, "Built-in"));
    }

    #[test]
    fn reopen_offers_the_same_device_again_once() {
        let host = Fake::with(Some("USB"), &["USB", "Built-in"]);
        let mut f = Follower::new(Mic::Default);
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("USB".into()));
        f.reopen();
        assert_eq!(f.poll(&host).unwrap(), Decision::Open("USB".into()));
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay);
        f.reopen();
        host.set(Some("Built-in"), &["USB", "Built-in"]);
        assert_eq!(f.poll(&host).unwrap(), switch(Some("USB"), "Built-in"));
        f.reopen();
        host.set(None, &[]);
        assert_eq!(f.poll(&host).unwrap(), Decision::NoInput);
        assert_eq!(f.poll(&host).unwrap(), Decision::Stay, "the mark is spent");
    }

    #[test]
    fn ears_mic_accepts_both_spellings() {
        assert_eq!(Mic::from_env_value(None), Mic::Default);
        assert_eq!(Mic::from_env_value(Some("")), Mic::Default);
        assert_eq!(Mic::from_env_value(Some(":")), Mic::Default);
        assert_eq!(Mic::from_env_value(Some(":A B")), Mic::Pinned("A B".into()));
        assert_eq!(Mic::from_env_value(Some("A B")), Mic::Pinned("A B".into()));
    }

    #[test]
    fn ears_mic_aec_is_off_unless_one_and_rejects_anything_else() {
        assert_eq!(Aec::from_env_value(None).unwrap(), Aec::Off);
        assert_eq!(Aec::from_env_value(Some("")).unwrap(), Aec::Off);
        assert_eq!(Aec::from_env_value(Some("0")).unwrap(), Aec::Off);
        assert_eq!(Aec::from_env_value(Some("1")).unwrap(), Aec::On);
        for bad in ["yes", "true", " 1", "2", "on"] {
            let e = Aec::from_env_value(Some(bad)).unwrap_err();
            assert_eq!(e, FollowError::BadAec(bad.to_owned()));
            let msg = e.to_string();
            assert!(msg.contains("EARS_MIC_AEC") && msg.contains("1 (on)") && msg.contains('0'));
        }
    }

    /// Records what the driver asked of it; `open` fails as `fail` says.
    #[derive(Default)]
    struct Rec {
        opened: Vec<String>,
        lines: Vec<String>,
        warnings: Vec<String>,
        no_device: usize,
        fail: Option<Opening<&'static str>>,
        /// The capture of this device has failed: its name, and its failure mark (set).
        broken: Option<(String, Stop)>,
    }

    /// `name`'s capture has failed: its name with a set mark, for [`Rec::broken`].
    fn broken(name: &str) -> (String, Stop) {
        let mark = Stop::new();
        mark.set();
        (name.to_owned(), mark)
    }

    impl Source for Rec {
        type Capture = String;
        type Error = &'static str;
        fn open(&mut self, name: &str) -> Result<String, Opening<&'static str>> {
            if let Some(e) = self.fail.clone() {
                return Err(e);
            }
            self.opened.push(name.to_owned());
            Ok(name.to_owned())
        }
        fn failure<'a>(&'a self, capture: &'a String) -> Option<&'a Stop> {
            let (_, mark) = self.broken.as_ref().filter(|(name, _)| name == capture)?;
            Some(mark)
        }
        fn switched(&mut self, from: Option<&str>, to: &str) -> Flow {
            self.lines.push(switch_line("them", "output", from, to));
            Flow::Go
        }
        fn no_device(&mut self) -> Flow {
            self.no_device = self.no_device.saturating_add(1);
            Flow::Go
        }
        fn warn(&mut self, message: String) -> Flow {
            self.warnings.push(message);
            Flow::Go
        }
    }

    #[test]
    fn output_change_rebuilds_once_with_one_switch_line() {
        let host = Fake::with(Some("Speakers"), &["Speakers", "Headphones"]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["Speakers"]);
        assert_eq!(src.lines, [] as [String; 0]);
        host.set(Some("Headphones"), &["Speakers", "Headphones"]);
        for _ in 0..4 {
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        }
        assert_eq!(src.opened, ["Speakers", "Headphones"]);
        assert_eq!(
            src.lines,
            ["them: default output switched: Speakers -> Headphones"]
        );
        assert_eq!(cap.as_deref(), Some("Headphones"));
    }

    #[test]
    fn unchanged_output_neither_rebuilds_nor_logs() {
        let host = Fake::with(Some("Speakers"), &["Speakers"]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        for _ in 0..6 {
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        }
        assert_eq!(src.opened, ["Speakers"]);
        assert!(src.lines.is_empty() && src.warnings.is_empty());
    }

    #[test]
    fn no_output_drops_the_capture_once() {
        let host = Fake::with(Some("Speakers"), &["Speakers"]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        host.set(None, &[]);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.no_device, 1);
        assert!(cap.is_none());
    }

    /// A host shared with the driving thread: the devices, whether they cannot be read, and whether the next
    /// default lookup fails (as cpal's `None`).
    #[derive(Default)]
    struct Shared(
        std::sync::Mutex<(Option<String>, Vec<String>)>,
        std::sync::atomic::AtomicBool,
        std::sync::atomic::AtomicBool,
        /// Told each time a look found the devices unreadable.
        std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    );

    impl Shared {
        fn set(&self, default: Option<&str>, names: &[&str]) {
            *self.0.lock().unwrap() = (
                default.map(str::to_owned),
                names.iter().map(|&n| n.to_owned()).collect(),
            );
        }
    }

    impl Shared {
        fn unreadable(&self, on: bool) {
            self.1.store(on, std::sync::atomic::Ordering::SeqCst);
        }
        fn read(&self) -> Result<(), String> {
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(told) = self.3.lock().unwrap().as_ref() {
                    let _told = told.send(());
                }
                Err("devices not read".to_owned())
            } else {
                Ok(())
            }
        }
    }

    impl DeviceHost for Shared {
        fn default_name(&self) -> Result<Option<String>, String> {
            self.read()?;
            let (default, names) = self.0.lock().unwrap().clone();
            let failed = self.2.swap(false, std::sync::atomic::Ordering::SeqCst);
            default_from(if failed { None } else { default.map(Ok) }, || {
                Ok(Names::full(names))
            })
        }
        fn names(&self) -> Result<Names, String> {
            self.read()
                .map(|()| Names::full(self.0.lock().unwrap().1.clone()))
        }
    }

    /// Tells the test thread each open, when it happened.
    struct Told(
        std::sync::mpsc::Sender<(String, Instant)>,
        Option<Opening<&'static str>>,
    );

    impl Source for Told {
        type Capture = String;
        type Error = &'static str;
        fn open(&mut self, name: &str) -> Result<String, Opening<&'static str>> {
            let _sent = self.0.send((name.to_owned(), Instant::now()));
            self.1.clone().map_or_else(|| Ok(name.to_owned()), Err)
        }
        fn failure<'a>(&'a self, _: &'a String) -> Option<&'a Stop> {
            None
        }
        fn switched(&mut self, _: Option<&str>, _: &str) -> Flow {
            Flow::Go
        }
        fn no_device(&mut self) -> Flow {
            Flow::Go
        }
        fn warn(&mut self, _: String) -> Flow {
            Flow::Go
        }
    }

    type Opens = std::sync::mpsc::Receiver<(String, Instant)>;
    type Driving = std::sync::mpsc::Receiver<Result<(), Halt<&'static str>>>;

    /// Drives `mic` over `host` on a thread of its own; the opens come back on the first receiver, and how `drive`
    /// ended on the second. Not a scoped thread: a `drive` that never ends fails its test at the [`WAIT`] guard
    /// instead of hanging the scope's join.
    fn spawn_drive(
        host: &Arc<Shared>,
        mic: Mic,
        fail: Option<Opening<&'static str>>,
        changes: tulpa_stop::Receiver<()>,
        stop: &Arc<Stop>,
    ) -> (Opens, Driving) {
        let (tx, opens) = std::sync::mpsc::channel();
        let (ended, driving) = std::sync::mpsc::channel();
        let (host, stop) = (Arc::clone(host), Arc::clone(stop));
        std::thread::spawn(move || {
            let end = drive(
                Follower::new(mic),
                host.as_ref(),
                &mut Told(tx, fail),
                &changes,
                &stop,
            );
            let _told = ended.send(end);
        });
        (opens, driving)
    }

    /// A wedge guard only: far above any wait here, so a loaded machine never trips it, and a stuck `drive` still
    /// ends the test.
    const WAIT: Duration = Duration::from_secs(60);

    /// Sets the stop when dropped: an assert that fails unwinds through this, so the test's `drive` ends with it.
    struct StopOnDrop<'a>(&'a Stop);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.set();
        }
    }

    /// How long [`drive`] must not look on its own: past the 2 s poll this replaced, so a poll put back opens the new
    /// default inside it and fails the test. An absence window, not a bound on a wake: the open the event causes is
    /// waited for under [`WAIT`].
    const NO_LOOK: Duration = Duration::from_millis(2500);

    /// A change event makes [`drive`] look again at once; with none, it never looks (no clock).
    #[test]
    fn drive_follows_a_device_change_the_moment_it_is_told() {
        let host = Arc::new(Shared::default());
        host.set(Some("Built-in"), &["Built-in", "USB"]);
        let stop = Arc::new(Stop::new());
        let (tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let (opens, driving) = spawn_drive(&host, Mic::Default, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "Built-in");
            host.set(Some("USB"), &["Built-in", "USB"]);
            let looked = opens.recv_timeout(NO_LOOK);
            assert!(looked.is_err(), "no look without an event: {looked:?}");
            // No upper bound: `drive` has no clock here, so only the event opens USB before the WAIT guard.
            tell.send(()).unwrap();
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "USB");
            stop.set();
            assert_eq!(driving.recv_timeout(WAIT).unwrap(), Ok(()));
        }
    }

    /// A stop wakes [`drive`] (the old loop looked every 50 ms between 2 s polls): it has no clock here, so a stop it
    /// did not wake on runs into the [`WAIT`] guard.
    #[test]
    fn drive_ends_the_moment_it_is_stopped() {
        let host = Arc::new(Shared::default());
        host.set(Some("USB"), &["USB"]);
        let stop = Arc::new(Stop::new());
        let (_tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let (opens, driving) = spawn_drive(&host, Mic::Default, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "USB");
            std::thread::sleep(Duration::from_millis(50));
            stop.set();
            assert_eq!(driving.recv_timeout(WAIT).unwrap(), Ok(()));
        }
    }

    /// A listener that goes away is an error, never a silent stop: no change would be seen again.
    #[test]
    fn drive_fails_when_the_listener_goes() {
        let host = Arc::new(Shared::default());
        host.set(Some("USB"), &["USB"]);
        let stop = Arc::new(Stop::new());
        let (tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let (opens, driving) = spawn_drive(&host, Mic::Default, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "USB");
            drop(tell);
            assert_eq!(
                driving.recv_timeout(WAIT).unwrap(),
                Err(Halt::Follow(FollowError::ListenerGone))
            );
        }
    }

    /// A pinned device that goes away is seen on the device-list event, as the poll saw it.
    #[test]
    fn drive_stops_when_the_pinned_device_goes() {
        let host = Arc::new(Shared::default());
        host.set(Some("Built-in"), &["Built-in", "USB"]);
        let stop = Arc::new(Stop::new());
        let (tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let pinned = Mic::Pinned("USB".to_owned());
            let (opens, driving) = spawn_drive(&host, pinned, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "USB");
            host.set(Some("Built-in"), &["Built-in"]);
            tell.send(()).unwrap();
            assert_eq!(
                driving.recv_timeout(WAIT).unwrap(),
                Err(Halt::Follow(FollowError::PinnedMissing("USB".into())))
            );
        }
    }

    /// A default lookup that fails once (cpal's `None`, with devices present) in the middle of a switch: the stream
    /// stays, and the follower looks again at [`RETRY_AFTER`] with no further event, and follows the switch then.
    #[test]
    fn drive_recovers_from_a_failed_default_lookup_on_its_clock() {
        let host = Arc::new(Shared::default());
        host.set(Some("Built-in"), &["Built-in", "USB"]);
        let stop = Arc::new(Stop::new());
        let (tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let (opens, driving) = spawn_drive(&host, Mic::Default, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "Built-in");
            host.set(Some("USB"), &["Built-in", "USB"]);
            host.2.store(true, std::sync::atomic::Ordering::SeqCst);
            let told = Instant::now();
            tell.send(()).unwrap();
            let opened = opens.recv_timeout(RETRY_AFTER + WAIT);
            stop.set();
            assert_eq!(driving.recv_timeout(WAIT).unwrap(), Ok(()));
            let (name, at) = opened.unwrap();
            assert_eq!(name, "USB");
            let between = at.duration_since(told);
            // From below only: no earlier than its clock. How late a loaded machine wakes it is not the claim.
            assert!(between >= RETRY_AFTER, "{between:?}");
        }
    }

    /// A default lookup that found nothing (cpal's `None`) or failed to name what it found (macOS answers "no default"
    /// with device 0, whose name fails) is "no device" only beside an empty list with nothing skipped.
    #[test]
    fn a_none_default_is_no_device_only_without_devices() {
        assert_eq!(default_from(None, || Ok(Names::default())), Ok(None));
        assert!(default_from(None, || Ok(Names::full(vec!["USB".to_owned()]))).is_err());
        assert_eq!(
            default_from(Some(Ok("USB".to_owned())), || Ok(Names::default())),
            Ok(Some("USB".to_owned()))
        );
        assert!(default_from(None, || Err("not read".to_owned())).is_err());
        // Empty only because every name failed: not "no device", a failed lookup.
        let got = default_from(None, || Ok(Names::new(vec![], true)));
        assert!(got.is_err(), "left: {got:?}");
        let failed = || Some(Err("x".to_owned()));
        let got = default_from(failed(), || Ok(Names::new(vec![], false)));
        assert_eq!(got, Ok(None), "a failed name beside no device at all");
        let got = default_from(failed(), || Ok(Names::full(vec!["USB".to_owned()])));
        assert!(
            got.as_ref().is_err_and(|e| e.contains('x')),
            "left: {got:?}"
        );
        let got = default_from(failed(), || Ok(Names::new(vec![], true)));
        assert!(got.is_err(), "left: {got:?}");
    }

    /// A host whose default goes through [`default_from`], as the `CoreAudio` adapters' does: the lookup (its name,
    /// or why the name failed; `None` for cpal's `None`) and the list.
    struct Looked(RefCell<(Lookup, Vec<String>)>);

    /// A default lookup as [`default_from`] takes it.
    type Lookup = Option<Result<String, String>>;

    impl Looked {
        fn set(&self, looked_up: Option<Result<&str, &str>>, names: &[&str]) {
            *self.0.borrow_mut() = (
                looked_up.map(|r| r.map(str::to_owned).map_err(str::to_owned)),
                names.iter().map(|&n| n.to_owned()).collect(),
            );
        }
    }

    impl DeviceHost for Looked {
        fn default_name(&self) -> Result<Option<String>, String> {
            let (looked_up, names) = self.0.borrow().clone();
            default_from(looked_up, || Ok(Names::full(names)))
        }
        fn names(&self) -> Result<Names, String> {
            Ok(Names::full(self.0.borrow().1.clone()))
        }
    }

    /// The last device goes: macOS still answers the default lookup, with a device whose name fails, beside an empty
    /// list. That is no device: the capture is dropped (not kept as unreadable), and when "USB" comes back it is
    /// opened anew, not kept as the stream `CoreAudio` already killed.
    #[test]
    fn a_failed_default_beside_no_devices_drops_the_capture_and_reopens_on_return() {
        let host = Looked(RefCell::default());
        host.set(Some(Ok("USB")), &["USB"]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(cap.as_deref(), Some("USB"));
        host.set(Some(Err("device 0 has no name (fake)")), &[]);
        let got = f.poll(&host);
        assert_eq!(got, Ok(Decision::NoInput));
        assert!(!f.retrying());
        host.set(Some(Ok("USB")), &["USB"]);
        let got = f.poll(&host);
        assert_eq!(got, Ok(switch(None, "USB")), "opened anew");
        // The same through the driver: the capture is dropped, then opened again.
        let (mut f, mut src, mut cap) = (Follower::new(Mic::Default), Rec::default(), None);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        host.set(Some(Err("device 0 has no name (fake)")), &[]);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert!(cap.is_none(), "dropped: {cap:?}");
        assert_eq!(src.no_device, 1);
        host.set(Some(Ok("USB")), &["USB"]);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["USB", "USB"]);
        assert_eq!(cap.as_deref(), Some("USB"));
    }

    /// An open that failed is offered again at [`RETRY_AFTER`] without any event.
    /// Devices that could not be read are read again at [`RETRY_AFTER`] without any event: a default that changed
    /// meanwhile is followed then.
    #[test]
    fn drive_reads_unreadable_devices_again_on_its_clock() {
        let host = Arc::new(Shared::default());
        host.set(Some("Built-in"), &["Built-in", "USB"]);
        let stop = Arc::new(Stop::new());
        let (tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let (opens, driving) = spawn_drive(&host, Mic::Default, None, changes, &stop);
            assert_eq!(opens.recv_timeout(WAIT).unwrap().0, "Built-in");
            let (looked_tx, looked) = std::sync::mpsc::channel();
            *host.3.lock().unwrap() = Some(looked_tx);
            host.unreadable(true);
            let told = Instant::now();
            tell.send(()).unwrap();
            // The look that could not read them, as an event: only after it do the devices come back, so the open
            // below is the clock's look whatever the machine's load (a guard, not a timing claim).
            looked.recv_timeout(WAIT).unwrap();
            host.set(Some("USB"), &["Built-in", "USB"]);
            host.unreadable(false);
            let (name, at) = opens.recv_timeout(RETRY_AFTER + WAIT).unwrap();
            assert_eq!(name, "USB");
            let between = at.duration_since(told);
            // From below only: no earlier than its clock. How late a loaded machine wakes it is not the claim.
            assert!(between >= RETRY_AFTER, "{between:?}");
            stop.set();
            assert_eq!(driving.recv_timeout(WAIT).unwrap(), Ok(()));
        }
    }

    #[test]
    fn drive_offers_a_failed_open_again_on_its_clock() {
        let host = Arc::new(Shared::default());
        host.set(Some("USB"), &["USB"]);
        let stop = Arc::new(Stop::new());
        let (_tell, changes) = tulpa_stop::channel();
        {
            let _stopped = StopOnDrop(&stop);
            let fail = Some(Opening::Retry("busy".into()));
            let (opens, driving) = spawn_drive(&host, Mic::Default, fail, changes, &stop);
            let (_, first) = opens.recv_timeout(WAIT).unwrap();
            let (name, again) = opens.recv_timeout(RETRY_AFTER + WAIT).unwrap();
            assert_eq!(name, "USB");
            let between = again.duration_since(first);
            // From below only: no earlier than its clock. How late a loaded machine wakes it is not the claim.
            assert!(between >= RETRY_AFTER, "{between:?}");
            stop.set();
            assert_eq!(driving.recv_timeout(WAIT).unwrap(), Ok(()));
        }
    }

    /// Devices that cannot be read are neither "no input" nor a missing pin: the stream stays, it is said once, and
    /// the next look that reads them goes on as before.
    #[test]
    fn unreadable_devices_keep_the_stream_and_say_so_once() {
        for mic in [Mic::Default, Mic::Pinned("USB".to_owned())] {
            let host = Fake::with(Some("USB"), &["USB"]);
            let mut f = Follower::new(mic);
            let mut src = Rec::default();
            let mut cap = None;
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
            *host.unreadable.borrow_mut() = true;
            for _ in 0..3 {
                assert_eq!(follow_once(&mut f, &host, &mut src, &mut cap), Ok(Flow::Go));
                assert!(f.retrying(), "looks again on the clock");
            }
            assert_eq!(src.warnings, ["devices not read"]);
            assert_eq!((src.no_device, cap.as_deref()), (0, Some("USB")));
            *host.unreadable.borrow_mut() = false;
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
            assert!(!f.retrying());
            assert_eq!(src.opened, ["USB"], "the stream was never dropped");
            // Said again at the next outage.
            *host.unreadable.borrow_mut() = true;
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
            assert_eq!(src.warnings.len(), 2);
        }
    }

    #[test]
    fn retry_reoffers_skip_warns_once_fatal_halts() {
        let host = Fake::with(Some("USB"), &["USB"]);
        let mut src = Rec {
            fail: Some(Opening::Retry("gone".into())),
            ..Rec::default()
        };
        let mut f = Follower::new(Mic::Default);
        let mut cap = None;
        for _ in 0..3 {
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        }
        assert_eq!(src.warnings.len(), 3, "a retry warns at every poll");

        src.warnings.clear();
        src.fail = Some(Opening::Skip("has a mic".into()));
        let mut f = Follower::new(Mic::Default);
        for _ in 0..3 {
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        }
        assert_eq!(src.warnings, ["has a mic"]);

        src.fail = Some(Opening::Fatal("dead"));
        let mut f = Follower::new(Mic::Default);
        assert_eq!(
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap_err(),
            Halt::Fatal("dead")
        );
    }

    /// A capture whose stream failed while the device stayed the default (cpal paused it: the lid closed) is
    /// dropped and opened again by the next `follow_once`, with one warning; a healthy one is left alone.
    #[test]
    fn a_failed_capture_is_opened_again_by_follow_once() {
        let host = Fake::with(Some("Speakers"), &["Speakers"]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["Speakers"]);
        src.broken = Some(broken("Speakers"));
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        src.broken = None;
        assert_eq!(src.opened, ["Speakers", "Speakers"], "opened again");
        assert_eq!(cap.as_deref(), Some("Speakers"));
        assert_eq!(
            src.warnings,
            ["\"Speakers\": the stream failed; opening it again"]
        );
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened.len(), 2, "the new capture is left alone");
    }

    /// A source whose captures fail on demand. Each open hands out a fresh [`Held`] mark (as `mic.rs`'s `Live` holds
    /// one per stream, retired when the capture is dropped), says its `Device` on `events` as a clean start does, and
    /// sends the mark on `opened`; when `dead`, the stream's error comes at once, as a callback's would: it is
    /// reported before the open returns, so before `drive` waits on it. `refused` makes every open a
    /// [`Opening::Retry`] instead. Every open blocks `takes` first. An outage is told on `no_input`.
    struct Flaky {
        opened: std::sync::mpsc::Sender<Opened>,
        events: std::sync::mpsc::Sender<Event>,
        no_input: std::sync::mpsc::Sender<()>,
        dead: bool,
        refused: bool,
        takes: Duration,
    }

    /// One open of a `Flaky` source: its stream's mark, and when the open began and when it returned.
    #[derive(Debug)]
    struct Opened {
        mark: Arc<Mark>,
        began: Instant,
        ended: Instant,
    }

    impl Source for Flaky {
        type Capture = Held;
        type Error = Infallible;
        fn open(&mut self, name: &str) -> Result<Held, Opening<Infallible>> {
            let began = Instant::now();
            // A stand-in for a start that blocks (`play()` waiting on a System Audio Recording grant, a slow
            // device): the open takes this long before it returns.
            std::thread::sleep(self.takes);
            let held = Held::default();
            let _sent = self.opened.send(Opened {
                mark: Arc::clone(held.mark()),
                began,
                ended: Instant::now(),
            });
            if self.refused {
                return Err(Opening::Retry("refused".to_owned()));
            }
            let device = Event::Device {
                name: name.to_owned(),
                rate: 48_000,
                channels: 1,
            };
            let _said = say(&self.events, held.mark(), device);
            if self.dead {
                report(
                    &self.events,
                    held.mark(),
                    Event::StreamError("dead".to_owned()),
                );
            }
            Ok(held)
        }
        fn failure<'a>(&'a self, capture: &'a Held) -> Option<&'a Stop> {
            Some(capture.failure())
        }
        fn switched(&mut self, _: Option<&str>, _: &str) -> Flow {
            Flow::Go
        }
        fn no_device(&mut self) -> Flow {
            let _told = self.no_input.send(());
            Flow::Go
        }
        fn warn(&mut self, _: String) -> Flow {
            Flow::Go
        }
    }

    /// How a `Flaky` source fails: `(dead, refused, takes)`, as its fields.
    type Fails = (bool, bool, Duration);

    /// A [`Shared`] host that tells each look (each default lookup) on `looked`: a test waits for a look to know that
    /// `drive` has taken an event, and that the `follow_once` before that look is done.
    struct Seen(Arc<Shared>, std::sync::mpsc::Sender<()>);

    impl DeviceHost for Seen {
        fn default_name(&self) -> Result<Option<String>, String> {
            let _told = self.1.send(());
            self.0.default_name()
        }
        fn names(&self) -> Result<Names, String> {
            self.0.names()
        }
    }

    /// [`paced`] running on a thread of its own over a `Flaky` source.
    struct Running {
        /// Each open, as it returned.
        opens: std::sync::mpsc::Receiver<Opened>,
        /// Each look at the host.
        looks: std::sync::mpsc::Receiver<()>,
        /// Each outage the source was told of.
        no_input: std::sync::mpsc::Receiver<()>,
        /// The device events `drive` waits on.
        tell: tulpa_stop::Sender<()>,
        /// What the captures said on the side's channel, and a sender onto it (the streams' callbacks).
        events: std::sync::mpsc::Receiver<Event>,
        callbacks: std::sync::mpsc::Sender<Event>,
        stop: Arc<Stop>,
        /// How `paced` ended.
        end: std::sync::mpsc::Receiver<bool>,
    }

    impl Running {
        /// Tells `drive` of a device change and waits until it has looked: the `follow_once` before is done by then.
        fn look(&self) {
            assert!(self.tell.send(()).is_ok());
            // A guard against a wedge, not a timing claim.
            let looked = self.looks.recv_timeout(Duration::from_secs(60));
            assert!(looked.is_ok(), "drive looks on the event");
        }

        /// Stops `drive` and asserts it ended cleanly.
        fn ended(&self) {
            self.stop.set();
            // A guard against a wedge, not a timing claim.
            let ended = self.end.recv_timeout(Duration::from_secs(60));
            assert_eq!(ended, Ok(true), "the driver ends");
        }
    }

    /// Runs [`paced`] over `host` and a `Flaky` source failing as `fails` says, with `retry_after`, and a storm of
    /// device events every `every` (none if `None`) until stopped.
    fn running(
        host: &Arc<Shared>,
        (dead, refused, takes): Fails,
        retry_after: Duration,
        every: Option<Duration>,
    ) -> Running {
        let (opened, opens) = std::sync::mpsc::channel();
        let (looked, looks) = std::sync::mpsc::channel();
        let (outage, no_input) = std::sync::mpsc::channel();
        let (ended, end) = std::sync::mpsc::channel();
        let (tell, changes) = tulpa_stop::channel();
        let (callbacks, events) = std::sync::mpsc::channel();
        let stop = Arc::new(Stop::new());
        let mut source = Flaky {
            opened,
            events: callbacks.clone(),
            no_input: outage,
            dead,
            refused,
            takes,
        };
        let seen = Seen(Arc::clone(host), looked);
        let stopped = Arc::clone(&stop);
        std::thread::spawn(move || {
            let driven = paced(
                Follower::new(Mic::Default),
                &seen,
                &mut source,
                &changes,
                &stopped,
                retry_after,
            );
            let _sent = ended.send(driven.is_ok());
        });
        if let Some(every) = every {
            let (storm, stopped) = (tell.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stopped.wait_until(Instant::now().checked_add(every)) {
                    if storm.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        Running {
            opens,
            looks,
            no_input,
            tell,
            events,
            callbacks,
            stop,
            end,
        }
    }

    /// A host whose default is "Speakers".
    fn speakers() -> Arc<Shared> {
        let host = Arc::new(Shared::default());
        host.set(Some("Speakers"), &["Speakers", "Headset"]);
        host
    }

    /// A guard against a wedge, not a timing claim.
    const GUARD: Duration = Duration::from_secs(60);

    /// The stream's error wakes the driver, which drops the failed capture and opens the device again, with no device
    /// event and no poll: only the error's mark can end the wait.
    #[test]
    fn a_stream_error_wakes_the_reopen() {
        let run = running(
            &speakers(),
            (false, false, Duration::ZERO),
            RETRY_AFTER,
            None,
        );
        let first = run.opens.recv_timeout(GUARD);
        assert!(first.is_ok(), "the first open: {first:?}");
        if let Ok(Opened { mark, .. }) = &first {
            // As the stream's error callback.
            report(&run.callbacks, mark, Event::StreamError("gone".to_owned()));
        }
        let again = run.opens.recv_timeout(GUARD);
        run.ended();
        assert!(again.is_ok(), "opened again on the error's wake: {again:?}");
        assert_eq!(run.opens.try_iter().count(), 0, "once");
    }

    /// akira's C1 condition 1: the stream fails before `drive` waits on it again (its mark is set inside the open)
    /// and is still reopened, a full delay after that open returned. A mark set before the wait ends the wait.
    #[test]
    fn a_stream_that_fails_before_the_wait_is_still_reopened() {
        let delay = Duration::from_millis(100);
        let run = running(&speakers(), (true, false, Duration::ZERO), delay, None);
        let first = run.opens.recv_timeout(GUARD);
        let again = run.opens.recv_timeout(GUARD);
        run.ended();
        let (first, again) = (first.unwrap(), again.unwrap());
        let gap = again.began.saturating_duration_since(first.ended);
        assert!(gap >= delay, "no sooner than the delay: {gap:?}");
    }

    /// akira's C1 condition 2 and Fable F3: an error a replaced stream reports late (its device was switched away,
    /// its capture dropped) reopens nothing and closes nothing. Its callback still holds the side's channel, but its
    /// mark was retired with the capture: no `StreamError` goes out after the new stream's `Device` (Beating and the
    /// pipeline would take it as the new stream's and close the side), and no mark is set.
    #[test]
    fn a_late_error_from_a_replaced_stream_reopens_and_closes_nothing() {
        let host = speakers();
        let run = running(&host, (false, false, Duration::ZERO), Duration::ZERO, None);
        let old = run.opens.recv_timeout(GUARD).unwrap();
        run.looks.recv_timeout(GUARD).unwrap();
        host.set(Some("Headset"), &["Speakers", "Headset"]);
        run.look();
        let _new = run.opens.recv_timeout(GUARD).unwrap();
        // As the old stream's error callback, after its capture was dropped for the new one.
        report(
            &run.callbacks,
            &old.mark,
            Event::StreamError("late".to_owned()),
        );
        // Two looks, one after the other: a reopen the late error caused would be done before the second.
        run.look();
        run.look();
        run.ended();
        let said: Vec<Event> = run.events.try_iter().collect();
        let after = said
            .iter()
            .rposition(|e| matches!(e, Event::Device { name, .. } if name == "Headset"));
        assert!(after.is_some(), "the new stream's Device: {said:?}");
        let late = said
            .iter()
            .skip(after.unwrap_or(0))
            .filter(|e| matches!(e, Event::StreamError(_)))
            .count();
        assert_eq!(late, 0, "nothing after the new Device: {said:?}");
        assert!(!old.mark.failure().is_set(), "a retired mark is not set");
        assert_eq!(run.opens.try_iter().count(), 0, "no reopen");
    }

    /// akira's C1 condition 3 and glados's test: a stream's error and a device event come together inside the retry
    /// delay. The device is the same, so nothing reopens it early; the error is not lost, so it is reopened once
    /// the delay after its open has run.
    #[test]
    fn an_error_and_a_device_event_inside_the_delay_reopen_once_after_it() {
        let delay = Duration::from_millis(300);
        for every in [None, Some(Duration::from_millis(10))] {
            let run = running(&speakers(), (false, false, Duration::ZERO), delay, every);
            let first = run.opens.recv_timeout(GUARD).unwrap();
            report(
                &run.callbacks,
                &first.mark,
                Event::StreamError("gone".to_owned()),
            );
            let _told = run.tell.send(());
            let again = run.opens.recv_timeout(GUARD);
            let (_never, window) = std::sync::mpsc::channel::<()>();
            // A measure, not a claim: no second reopen in a further delay with the device unchanged.
            let _waited = window.recv_timeout(delay);
            run.ended();
            let again = again.unwrap();
            let gap = again.began.saturating_duration_since(first.ended);
            assert!(gap >= delay, "events {every:?}: no early reopen: {gap:?}");
            assert_eq!(run.opens.try_iter().count(), 0, "events {every:?}: once");
        }
    }

    /// C1: a device event does not reopen a failed device before its retry delay (here beyond every guard), but a
    /// switch and no input still act at once.
    #[test]
    fn a_device_event_switches_a_failed_device_at_once_but_never_reopens_it_early() {
        let host = speakers();
        let run = running(&host, (false, true, Duration::ZERO), HOUR, None);
        assert!(
            run.opens.recv_timeout(GUARD).is_ok(),
            "the first, failed open"
        );
        run.looks.recv_timeout(GUARD).unwrap();
        // The same device: looked at, not reopened.
        run.look();
        run.look();
        assert_eq!(
            run.opens.try_iter().count(),
            0,
            "no reopen before the delay"
        );
        host.set(Some("Headset"), &["Speakers", "Headset"]);
        run.look();
        assert!(
            run.opens.recv_timeout(GUARD).is_ok(),
            "the switch opens at once"
        );
        host.set(None, &[]);
        run.look();
        let said = run.no_input.recv_timeout(GUARD);
        run.ended();
        assert!(said.is_ok(), "no input is said at once");
    }

    /// Longer than every guard here: only an event or the error's mark brings an open.
    const HOUR: Duration = Duration::from_secs(3600);

    /// A device that fails at every start, by its stream's error or by its open, is opened again no sooner than the
    /// retry delay after each open: over a window, its opens stay within window / delay (+ the first), never a loop —
    /// with device events far more often than the delay (a storm every 10 ms), far less often, and none. A start
    /// that blocks (`takes`) adds to that: the delay runs from when the open returned, so its opens stay within
    /// window / (takes + delay) (+ the first).
    #[test]
    fn a_device_that_fails_at_every_start_is_not_reopened_in_a_loop() {
        let delay = Duration::from_millis(100);
        let storm = Some(Duration::from_millis(10));
        let none = Duration::ZERO;
        let fails: [Fails; 2] = [(true, false, none), (false, true, none)];
        let slow = Duration::from_millis(200);
        let blocking = [(true, false, slow), (false, true, slow)].map(|f| (storm, f));
        for (every, (dead, refused, takes)) in [storm, Some(Duration::from_millis(300)), None]
            .into_iter()
            .flat_map(|every| fails.map(|f| (every, f)))
            .chain(blocking)
        {
            let began = Instant::now();
            let run = running(&speakers(), (dead, refused, takes), delay, every);
            // The window the opens are counted over: a measure, not a claim (the bound is taken from the time that
            // really passed, and a slow machine only opens less).
            let (_never, window) = std::sync::mpsc::channel::<()>();
            let span = if takes.is_zero() { 500 } else { 1000 };
            let _waited = window.recv_timeout(Duration::from_millis(span));
            run.ended();
            let passed = began.elapsed().as_millis();
            let bound = passed
                .checked_div(takes.saturating_add(delay).as_millis())
                .unwrap_or(0)
                .saturating_add(1);
            let count = u128::try_from(run.opens.try_iter().count()).unwrap_or(u128::MAX);
            assert!(
                (1..=bound).contains(&count),
                "events {every:?}, dead {dead}, refused {refused}, takes {takes:?}: {count} opens in {passed} ms, \
                 at most {bound}"
            );
        }
    }

    /// A start that blocks longer than the retry delay and then fails (its open says Retry, or its stream fails at
    /// once) is tried again a full delay after it returned, never at once, through a storm of device events: each
    /// open begins at least the delay after the one before ended.
    #[test]
    fn a_start_that_blocks_then_fails_waits_the_delay_after_it() {
        let delay = Duration::from_millis(100);
        let takes = Duration::from_millis(300);
        for (dead, refused) in [(true, false), (false, true)] {
            let storm = Some(Duration::from_millis(10));
            let run = running(&speakers(), (dead, refused, takes), delay, storm);
            let three: Vec<Opened> = (0..3)
                .filter_map(|_| run.opens.recv_timeout(GUARD).ok())
                .collect();
            run.ended();
            assert_eq!(
                three.len(),
                3,
                "dead {dead}, refused {refused}: three opens"
            );
            let gaps: Vec<Duration> = three
                .windows(2)
                .filter_map(|w| Some(w.get(1)?.began.saturating_duration_since(w.first()?.ended)))
                .collect();
            assert!(
                gaps.iter().all(|&g| g >= delay),
                "dead {dead}, refused {refused}: each next open {delay:?} after the last returned: {gaps:?}"
            );
        }
    }

    /// "Speakers" is open; its capture then fails — its stream's error, or (`retry`) as the lid-close path: that
    /// error, and the reopen's start fails too ([`Opening::Retry`]) — and the default is then `to` (`None`: no output
    /// at all). Follows once more and returns what the source was told.
    fn after_a_failure(retry: bool, to: Option<&str>) -> (Rec, Option<String>) {
        let names = ["Speakers", "Headset"];
        let host = Fake::with(Some("Speakers"), &names);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        src.broken = Some(broken("Speakers"));
        if retry {
            src.fail = Some(Opening::Retry("paused as it started".into()));
            follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
            src.fail = None;
            src.broken = None;
        }
        host.set(to, if to.is_some() { &names } else { &[] });
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        src.broken = None;
        (src, cap)
    }

    /// The same device, after its capture failed (or its reopen failed to start): opened again quietly — no switch
    /// (no `Switched`, no switch line: the pipeline is not ended a second time), no outage.
    #[test]
    fn a_reopen_of_the_same_device_is_no_switch() {
        for retry in [false, true] {
            let (src, cap) = after_a_failure(retry, Some("Speakers"));
            assert_eq!(src.lines, [] as [String; 0], "retry {retry}: no switch");
            assert_eq!(src.no_device, 0, "retry {retry}");
            assert_eq!(src.opened, ["Speakers", "Speakers"], "retry {retry}");
            assert_eq!(cap.as_deref(), Some("Speakers"), "retry {retry}");
        }
    }

    /// The default moved while the capture was failed: exactly one switch, from the real previous device.
    #[test]
    fn a_failed_capture_whose_default_moved_is_one_real_switch() {
        for retry in [false, true] {
            let (src, cap) = after_a_failure(retry, Some("Headset"));
            assert_eq!(
                src.lines,
                ["them: default output switched: Speakers -> Headset"],
                "retry {retry}"
            );
            assert_eq!(src.opened, ["Speakers", "Headset"], "retry {retry}");
            assert_eq!(cap.as_deref(), Some("Headset"), "retry {retry}");
        }
    }

    /// The default went away while the capture was failed: an outage (`NoInput`), not silence.
    #[test]
    fn a_failed_capture_whose_device_is_gone_is_no_input() {
        for retry in [false, true] {
            let (src, cap) = after_a_failure(retry, None);
            assert_eq!(src.no_device, 1, "retry {retry}: no input");
            assert_eq!(src.lines, [] as [String; 0], "retry {retry}");
            assert!(cap.is_none(), "retry {retry}");
        }
    }

    /// The tap's [`Source`] as `tap.rs` has it, without the device: it reports on the events channel what
    /// `TapSource` reports (a `Device` once open, `Switched`, `NoInput`, a `Warning` for a skipped or failed open).
    struct TapLike {
        events: std::sync::mpsc::Sender<Event>,
        fail: Option<Opening<Infallible>>,
        /// The last opened stream's mark, as `mic.rs`'s `Live` keeps it: set when its error callback reports.
        mark: Arc<Stop>,
    }

    impl TapLike {
        /// The stream's error callback reports a stream error: it marks the stream and sends the error.
        fn stream_error(&self) {
            self.mark.set();
            let _sent = self.events.send(Event::StreamError("stopped".to_owned()));
        }
    }

    impl Source for TapLike {
        type Capture = Arc<Stop>;
        type Error = Infallible;
        fn open(&mut self, name: &str) -> Result<Arc<Stop>, Opening<Infallible>> {
            if let Some(e) = self.fail.clone() {
                return Err(e);
            }
            let _sent = self.events.send(Event::Device {
                name: name.to_owned(),
                rate: 48_000,
                channels: 2,
            });
            self.mark = Arc::default();
            Ok(Arc::clone(&self.mark))
        }
        fn failure<'a>(&'a self, capture: &'a Arc<Stop>) -> Option<&'a Stop> {
            Some(capture)
        }
        fn switched(&mut self, from: Option<&str>, to: &str) -> Flow {
            let _sent = self.events.send(Event::Switched {
                from: from.map(str::to_owned),
                to: to.to_owned(),
            });
            Flow::Go
        }
        fn no_device(&mut self) -> Flow {
            let _sent = self.events.send(Event::NoInput);
            Flow::Go
        }
        fn warn(&mut self, message: String) -> Flow {
            let _sent = self.events.send(Event::Warning(message));
            Flow::Go
        }
    }

    /// How the tap's side looks to the tray once the follower has done `then` to an open, silent tap: whether its
    /// `.alive` (beaten as `ears::run` beats it, by [`Beating::forward`]) is kept fresh. The tray marks a side
    /// stale when its directory stops changing; here that is asserted by the order of events, not by time: after
    /// `then` has been taken, the next silent checks beat (fresh) or do not (dead). Before `then`, the open tap's
    /// next check must beat, so a beat that never fires fails here, and a stale tap after `then` is `then`'s doing.
    fn fresh_after(
        tag: &str,
        then: impl FnOnce(&mut Follower, &Fake, &mut TapLike, &mut Cap),
    ) -> bool {
        let dir = std::env::temp_dir().join(format!("tulpa-follow-{tag}-{}", std::process::id()));
        let _gone = std::fs::remove_dir_all(&dir);
        let (tx, rx) = std::sync::mpsc::channel();
        let (to, pipeline) = std::sync::mpsc::channel();
        let (ticks_tx, ticks) = std::sync::mpsc::channel();
        let token = std::sync::Arc::new(());
        let reader = std::sync::Arc::downgrade(&token);
        let beating = Beating::new(rx, Side::Them, Some(dir.clone()))
            .with_every(Duration::from_millis(50), ticks_tx);
        let forwarding = std::thread::spawn(move || beating.forward(&to, reader));
        let host = Fake::with(Some("Speakers"), &["Speakers", "Headset"]);
        let mut f = Follower::new(Mic::Default);
        let mut tap = TapLike {
            events: tx,
            fail: None,
            mark: Arc::default(),
        };
        // Every wait below is a guard against a wedge, not a timing claim: a silent check comes every 50 ms.
        let guard = Duration::from_secs(60);
        // Until the events sent so far have all been taken (a marker sent after them comes through), and then the
        // silent checks queued meanwhile are dropped: the next ones see the state after those events.
        let settled = |tap: &TapLike| {
            let _sent = tap.events.send(Event::Warning("settled".to_owned()));
            loop {
                let ev = pipeline.recv_timeout(guard);
                assert!(ev.is_ok(), "{tag}: the forwarder: {ev:?}");
                if matches!(ev, Ok(Event::Warning(ref w)) if w == "settled") {
                    break;
                }
            }
            while ticks.try_recv().is_ok() {}
        };
        let next_beats = |n: usize| {
            (0..n)
                .map(|_| ticks.recv_timeout(guard))
                .collect::<Result<Vec<bool>, _>>()
        };
        let mut cap = None;
        follow_once(&mut f, &host, &mut tap, &mut cap).unwrap();
        settled(&tap);
        assert_eq!(
            next_beats(1),
            Ok(vec![true]),
            "{tag}: the open tap beats before the event"
        );
        then(&mut f, &host, &mut tap, &mut cap);
        settled(&tap);
        let after = next_beats(3);
        drop(tap);
        let _joined = forwarding.join();
        drop(token);
        let _gone = std::fs::remove_dir_all(&dir);
        after.unwrap().iter().any(|&b| b)
    }

    /// The default output moves to `to`; the follower switches (it waits for the move to hold) and opens as the
    /// tap's `fail` says.
    fn moved(
        f: &mut Follower,
        host: &Fake,
        tap: &mut TapLike,
        cap: &mut Cap,
        fail: Option<Opening<Infallible>>,
    ) {
        tap.fail = fail;
        host.set(Some("Headset"), &["Speakers", "Headset"]);
        for _ in 0..4 {
            follow_once(f, host, tap, cap).unwrap();
        }
    }

    /// The tap's stored capture in [`fresh_after`].
    type Cap = Option<Arc<Stop>>;

    // akira's gate: the freshness of the tap's `.alive` comes from a tap that is really open, never from the silent
    // beat alone. Each way the tap stops capturing makes it stale within the tray's limit; a healthy tap that is only
    // quiet, and one that switched and reopened, stay fresh.

    #[test]
    fn gate_a_tap_skipped_on_a_switch_goes_stale() {
        let skip = || Some(Opening::Skip("has a mic".into()));
        assert!(!fresh_after("skip", |f, h, t, c| moved(f, h, t, c, skip())));
    }

    #[test]
    fn gate_a_tap_whose_reopen_failed_goes_stale() {
        let retry = || Some(Opening::Retry("gone".into()));
        assert!(!fresh_after("retry", |f, h, t, c| moved(
            f,
            h,
            t,
            c,
            retry()
        )));
    }

    #[test]
    fn gate_a_tap_whose_stream_failed_goes_stale() {
        // As mic.rs's `open_stream` error callback, which the tap's stream shares; no poll has come yet.
        assert!(!fresh_after("error", |_, _, t, _| t.stream_error()));
    }

    #[test]
    fn gate_a_tap_whose_stream_failed_is_open_again_after_follow_once() {
        assert!(fresh_after("reopen", |f, h, t, c| {
            t.stream_error();
            follow_once(f, h, t, c).unwrap();
        }));
    }

    #[test]
    fn gate_a_healthy_quiet_tap_stays_fresh() {
        assert!(fresh_after("quiet", |_, _, _, _| {}));
    }

    #[test]
    fn gate_a_tap_that_switched_and_reopened_stays_fresh() {
        assert!(fresh_after("reopened", |f, h, t, c| moved(
            f, h, t, c, None
        )));
    }

    /// A device list as `cpal` reads it: each name, or why it was not read.
    struct Listed(
        Vec<Result<&'static str, &'static str>>,
        Mutex<BTreeSet<String>>,
    );

    impl Listed {
        fn new(names: Vec<Result<&'static str, &'static str>>) -> Self {
            Self(names, Mutex::default())
        }
    }

    impl DeviceHost for Listed {
        /// cpal's `None` default, read as the `CoreAudio` adapter reads it.
        fn default_name(&self) -> Result<Option<String>, String> {
            default_from(None, || self.names())
        }

        fn names(&self) -> Result<Names, String> {
            let names = self
                .0
                .iter()
                .map(|n| n.map(str::to_owned).map_err(str::to_owned));
            Ok(names_kept(names, "input", &self.1))
        }
    }

    /// A device whose name is not read, beside the pinned one that is present: the pin is opened, then stays.
    #[test]
    fn an_unreadable_name_beside_the_pinned_device_keeps_it() {
        let host = Listed::new(vec![Err("a device's name was not read (fake)"), Ok("USB")]);
        let mut follower = Follower::new(Mic::Pinned("USB".to_owned()));
        assert_eq!(follower.poll(&host), Ok(Decision::Open("USB".to_owned())));
        assert_eq!(follower.poll(&host), Ok(Decision::Stay));
        assert_eq!(host.1.lock().unwrap().len(), 1, "the skip is said once");
    }

    /// The pinned device is absent from a list read in full: it is missing.
    #[test]
    fn a_pinned_device_absent_from_a_list_read_in_full_is_missing() {
        let host = Listed::new(vec![Ok("Built-in")]);
        let mut follower = Follower::new(Mic::Pinned("USB".to_owned()));
        assert_eq!(
            follower.poll(&host),
            Err(FollowError::PinnedMissing("USB".to_owned()))
        );
    }

    /// The pinned device is absent while another device's name is not read: the unreadable one may be the pin
    /// itself, so it is not missing; the list is unreadable and looked at again at `RETRY_AFTER`.
    #[test]
    fn a_pinned_device_absent_beside_an_unreadable_name_is_looked_for_again() {
        let host = Listed::new(vec![
            Err("a device's name was not read (fake)"),
            Ok("Built-in"),
        ]);
        let mut follower = Follower::new(Mic::Pinned("USB".to_owned()));
        let got = follower.poll(&host);
        assert!(matches!(got, Ok(Decision::Unreadable(_))), "left: {got:?}");
        assert!(follower.retrying());
    }

    /// The default is `None` and the only device's name fails: not "no input" (that would drop the capture and look
    /// again only on a device event) but an unreadable list, looked at again at `RETRY_AFTER`.
    #[test]
    fn a_none_default_beside_an_unreadable_name_is_looked_for_again() {
        let host = Listed::new(vec![Err("a device's name was not read (fake)")]);
        let mut follower = Follower::new(Mic::Default);
        let got = follower.poll(&host);
        assert!(matches!(got, Ok(Decision::Unreadable(_))), "left: {got:?}");
        assert!(follower.retrying());
    }

    /// A host whose one device keeps its name while its id changes.
    struct Ided {
        id: RefCell<Option<u64>>,
    }

    impl DeviceHost for Ided {
        fn default_name(&self) -> Result<Option<String>, String> {
            Ok(Some("USB Mic".to_owned()))
        }
        fn names(&self) -> Result<Names, String> {
            Ok(Names::full(vec!["USB Mic".to_owned()]))
        }
        fn id_of(&self, _: &str) -> Option<u64> {
            *self.id.borrow()
        }
    }

    /// #0454 s2 follow-up 1: a device that is unplugged and plugged back between two looks (or made anew by the
    /// audio system) keeps its name, and its dead stream reports nothing. Its id changes: the follower opens it
    /// again, at once and without a switch line. The same id, or no id at all, is the same device: nothing happens.
    #[test]
    fn a_device_back_under_another_id_is_opened_again() {
        let host = Ided {
            id: RefCell::new(Some(41)),
        };
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["USB Mic"], "the same id: the same device");
        *host.id.borrow_mut() = Some(57);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(
            src.opened,
            ["USB Mic", "USB Mic"],
            "another id: opened again"
        );
        // A break in the stream, said as a switch from the device to itself: what was heard before the device
        // went away and what is heard after it came back are not one phrase, and not one timeline.
        assert_eq!(
            src.lines,
            ["them: default output switched: USB Mic -> USB Mic"],
            "one break"
        );
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened.len(), 2, "once per id");
        // An id that cannot be read is not a change: the stream is kept.
        *host.id.borrow_mut() = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened.len(), 2, "no id: only the name is followed");
        assert_eq!(cap.as_deref(), Some("USB Mic"));
    }

    /// A host whose devices have ids, and can be renamed under them.
    struct Renamable {
        /// Each input present: its id and its name now.
        devices: RefCell<Vec<(u64, String)>>,
        /// The default device's id.
        default: u64,
    }

    impl Renamable {
        fn new(default: u64, devices: &[(u64, &str)]) -> Self {
            Self {
                devices: RefCell::new(devices.iter().map(|&(id, n)| (id, n.to_owned())).collect()),
                default,
            }
        }
        fn rename(&self, id: u64, to: &str) {
            for (at, name) in self.devices.borrow_mut().iter_mut() {
                if *at == id {
                    *name = to.to_owned();
                }
            }
        }
    }

    impl DeviceHost for Renamable {
        fn default_name(&self) -> Result<Option<String>, String> {
            Ok(self.name_of(self.default))
        }
        fn names(&self) -> Result<Names, String> {
            Ok(Names::full(
                self.devices
                    .borrow()
                    .iter()
                    .map(|(_, n)| n.clone())
                    .collect(),
            ))
        }
        fn id_of(&self, name: &str) -> Option<u64> {
            self.devices
                .borrow()
                .iter()
                .find(|(_, n)| n == name)
                .map(|(id, _)| *id)
        }
        fn name_of(&self, id: u64) -> Option<String> {
            self.devices
                .borrow()
                .iter()
                .find(|(at, _)| *at == id)
                .map(|(_, n)| n.clone())
        }
    }

    /// #0530: a pinned mic renamed is the same device (same id): it stays pinned and capturing, with nothing said,
    /// and a reopen after that opens it by its new name. Only its going away is missing.
    #[test]
    fn a_pinned_device_renamed_stays_pinned() {
        let host = Renamable::new(1, &[(1, "Built-in"), (7, "USB")]);
        let mut f = Follower::new(Mic::Pinned("USB".to_owned()));
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        host.rename(7, "Desk Mic");
        assert_eq!(
            follow_once(&mut f, &host, &mut src, &mut cap),
            Ok(Flow::Go),
            "a rename is not a loss"
        );
        assert_eq!(src.opened, ["USB"], "not dropped and reopened");
        assert_eq!(cap.as_deref(), Some("USB"), "the capture goes on");
        assert!(src.lines.is_empty(), "no switch is said: {:?}", src.lines);
        assert_eq!(f.current(), Some("Desk Mic"));
        // Another device taking the old name does not take the pin: it follows the id.
        host.devices.borrow_mut().push((9, "USB".to_owned()));
        src.broken = Some(broken("USB"));
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["USB", "Desk Mic"], "reopened by its new name");
        // Its id gone, the pin is the name the human picked again.
        host.devices.borrow_mut().retain(|&(id, _)| id != 7);
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["USB", "Desk Mic", "USB"], "the name again");
        host.devices.borrow_mut().retain(|&(id, _)| id != 9);
        assert_eq!(
            follow_once(&mut f, &host, &mut src, &mut cap),
            Err(Halt::Follow(FollowError::PinnedMissing("USB".to_owned()))),
            "gone is still missing"
        );
    }

    /// #0530: the default device renamed is the same device (same id): kept, not dropped and reopened, and no switch
    /// is said.
    #[test]
    fn the_default_device_renamed_is_kept() {
        let host = Renamable::new(7, &[(1, "Built-in"), (7, "USB")]);
        let mut f = Follower::new(Mic::Default);
        let mut src = Rec::default();
        let mut cap = None;
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        host.rename(7, "Desk Mic");
        follow_once(&mut f, &host, &mut src, &mut cap).unwrap();
        assert_eq!(src.opened, ["USB"], "not dropped and reopened");
        assert_eq!(cap.as_deref(), Some("USB"), "the capture goes on");
        assert!(src.lines.is_empty(), "no switch is said: {:?}", src.lines);
        assert_eq!(f.current(), Some("Desk Mic"));
    }
}
