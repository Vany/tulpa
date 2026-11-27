//! The macOS microphone: a `CoreAudio` input stream (through `cpal`) on the device the [`Follower`] picks, fed
//! through the [`dsp`](crate::dsp) chain into 16 kHz mono `i16` chunks.
//!
//! [`run`] owns the stream on the calling thread and drives the follower on `CoreAudio`'s device events through
//! [`follow::drive`]; on a switch it
//! drops the stream, opens the new device at its default (native) input config, and appends `<epoch ms> <from> -> <to>` to
//! `<EARS_STATE>/mic-switch.log`. Everything it observes goes out as [`Event`]s on a channel. It never writes audio
//! anywhere.
//!
//! With `EARS_MIC_AEC=1` the same device is opened through the [`aec`](crate::aec) `VoiceProcessingIO` unit
//! instead, into the same chain; a unit that cannot be set up stops the capture rather than falling back. That unit
//! adds Apple's noise suppression and gain control; it does not cancel our own speech (see [`aec`](crate::aec)).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use cpal::Stream;
use cpal::traits::DeviceTrait;
use tulpa_audio_events::ListenError;
use tulpa_stop::Stop;

use crate::aec::{self, VoiceUnit};
use crate::follow::{self, Aec, Flow, FollowError, Follower, Halt, Mic, Opening, Source};
use crate::log::log_switch;
use crate::start::{Held, Live, gated, say};
use crate::stream::{Host, device_changes, feeder, open_stream, send};

pub use crate::log::SWITCH_LOG;

/// How the capture is set up.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Settings {
    /// Which device to follow.
    pub mic: Mic,
    /// Where `mic-switch.log` goes.
    pub state_dir: PathBuf,
    /// Plain input, or Apple's voice processing.
    pub aec: Aec,
}

impl Settings {
    /// From the environment: `EARS_MIC`, `EARS_MIC_AEC`, `EARS_STATE` (default `~/.cache/tulpa-ears`).
    ///
    /// # Errors
    ///
    /// [`MicError::Follow`] for a bad `EARS_MIC_AEC`; [`MicError::NoStateDir`] when neither `EARS_STATE` nor `HOME`
    /// is set.
    pub fn from_env() -> Result<Self, MicError> {
        let var = |k: &str| std::env::var(k).ok();
        let state_dir = match var("EARS_STATE").filter(|s| !s.is_empty()) {
            Some(s) => PathBuf::from(s),
            None => var("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| Path::new(&h).join(".cache/tulpa-ears"))
                .ok_or(MicError::NoStateDir)?,
        };
        Ok(Self {
            mic: Mic::from_env_value(var("EARS_MIC").as_deref()),
            state_dir,
            aec: Aec::from_env_value(var("EARS_MIC_AEC").as_deref())?,
        })
    }
}

pub use crate::event::Event;

/// Why the capture stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum MicError {
    /// The follower cannot go on (a pinned device is missing, or the device listener went away).
    Follow(FollowError),
    /// `CoreAudio` refused the device listener: device changes could not be followed.
    Listen(ListenError),
    /// Neither `EARS_STATE` nor `HOME` names a state directory.
    NoStateDir,
    /// `EARS_MIC_AEC=1`, but the `VoiceProcessingIO` unit cannot be set up.
    Aec(String),
}

impl fmt::Display for MicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Follow(e) => e.fmt(f),
            Self::Listen(e) => e.fmt(f),
            Self::NoStateDir => f.write_str("neither EARS_STATE nor HOME is set"),
            Self::Aec(e) => write!(f, "EARS_MIC_AEC=1: {e}"),
        }
    }
}

impl std::error::Error for MicError {}

impl From<FollowError> for MicError {
    fn from(e: FollowError) -> Self {
        Self::Follow(e)
    }
}

/// Captures until `stop` is set, sending [`Event`]s to `events`; the device is followed on `CoreAudio`'s events.
///
/// Returns `Ok` when stopped (or when the receiver hangs up).
///
/// # Errors
///
/// [`MicError::Follow`] when the pinned device is or goes missing; [`MicError::Listen`] when the device listener
/// cannot be registered; [`MicError::Aec`] when `EARS_MIC_AEC=1` and the unit cannot be set up.
pub fn run(settings: &Settings, events: &Sender<Event>, stop: &Stop) -> Result<(), MicError> {
    let (_listener, changes) = device_changes().map_err(MicError::Listen)?;
    let host = Host::inputs();
    let mut source = MicSource {
        host: &host,
        settings,
        events,
        stop,
    };
    follow::drive(
        Follower::new(settings.mic.clone()),
        &host,
        &mut source,
        &changes,
        stop,
    )
    .map_err(|h| match h {
        Halt::Follow(e) => MicError::Follow(e),
        Halt::Fatal(e) => MicError::Aec(e),
    })
}

/// The mic as a [`Source`]: opens the plain or voice capture, logs switches to `mic-switch.log`, reports events.
struct MicSource<'a> {
    host: &'a Host,
    settings: &'a Settings,
    events: &'a Sender<Event>,
    /// The run's stop: once it is set, the switch log is only appended to.
    stop: &'a Stop,
}

impl Source for MicSource<'_> {
    type Capture = Capture;
    type Error = String;

    fn open(&mut self, name: &str) -> Result<Capture, Opening<String>> {
        open_capture(self.host, self.settings.aec, name, self.events)
    }

    fn failure<'a>(&'a self, capture: &'a Capture) -> Option<&'a Stop> {
        match capture {
            Capture::Plain(live) => Some(live.failure()),
            // The voice unit reports no stream error: it is never reopened this way.
            Capture::Voice(_) => None,
        }
    }

    fn switched(&mut self, from: Option<&str>, to: &str) -> Flow {
        // A switch seen as the stop comes in is still logged, but a stopped capture rotates nothing.
        if let Err(e) = log_switch(&self.settings.state_dir, from, to, !self.stop.is_set()) {
            let _sent = self.events.send(Event::Warning(format!(
                "{} not written: {e}",
                self.settings.state_dir.join(SWITCH_LOG).display()
            )));
        }
        send(
            self.events,
            Event::Switched {
                from: from.map(str::to_owned),
                to: to.to_owned(),
            },
        )
    }

    fn no_device(&mut self) -> Flow {
        send(self.events, Event::NoInput)
    }

    fn warn(&mut self, message: String) -> Flow {
        send(self.events, Event::Warning(message))
    }
}

/// An open capture; dropping it stops it.
#[expect(dead_code, reason = "held only so that dropping the capture stops it")]
enum Capture {
    /// A `cpal` input stream.
    Plain(Live<Stream>),
    /// A `VoiceProcessingIO` unit.
    Voice(Live<VoiceUnit>),
}

/// The two ways to open the device the follower picked. A plain stream that fails is retried
/// [`RETRY_AFTER`](follow::RETRY_AFTER) after the last attempt ended; a voice-processing unit that cannot be set up is
/// fatal.
trait OpenCapture {
    /// What an open capture is.
    type Capture;
    /// The plain `CoreAudio` input stream; its error sets its failure mark.
    fn plain(&self, name: &str, events: &Sender<Event>) -> Result<Self::Capture, Opening<String>>;
    /// The `VoiceProcessingIO` unit.
    fn voice(&self, name: &str, events: &Sender<Event>) -> Result<Self::Capture, Opening<String>>;
}

/// Opens `name` the way `aec` says; never the other way.
fn open_capture<O: OpenCapture>(
    opener: &O,
    aec: Aec,
    name: &str,
    events: &Sender<Event>,
) -> Result<O::Capture, Opening<String>> {
    match aec {
        Aec::On => opener.voice(name, events),
        _ => opener.plain(name, events),
    }
}

impl OpenCapture for Host {
    type Capture = Capture;

    fn plain(&self, name: &str, events: &Sender<Event>) -> Result<Capture, Opening<String>> {
        let device = self
            .device(name)
            .ok_or_else(|| format!("input device \"{name}\" vanished before it opened"))
            .map_err(Opening::Retry)?;
        let config = device
            .default_input_config()
            .map_err(|e| Opening::Retry(format!("\"{name}\": no default input config: {e}")))?;
        open_stream(&device, name, config, events)
            .map(Capture::Plain)
            .map_err(Opening::Retry)
    }

    fn voice(&self, name: &str, events: &Sender<Event>) -> Result<Capture, Opening<String>> {
        let vanished =
            || Opening::Retry(format!("input device \"{name}\" vanished before it opened"));
        let rate = self
            .device(name)
            .ok_or_else(vanished)?
            .default_input_config()
            .map_err(|e| Opening::Retry(format!("\"{name}\": no default input config: {e}")))?
            .config()
            .sample_rate;
        let id = aec::input_device_id(name).ok_or_else(vanished)?;
        let fatal = |e: String| Opening::Fatal(format!("\"{name}\": {e}"));
        // As a plain stream's start: the `Device` once the unit runs, and none of its audio before it. The unit
        // reports no stream error, so its mark is never set and the `Device` always follows a unit that opened.
        // Held as the plain stream's is: the mark is retired when the capture is dropped, on both paths.
        let mark = Held::default();
        let feed = gated(
            Arc::clone(mark.mark()),
            feeder(rate, 1, events).map_err(fatal)?,
        );
        let unit = aec::open(id, rate, feed).map_err(fatal)?;
        let device = Event::Device {
            name: name.to_owned(),
            rate,
            channels: 1,
        };
        say(events, mark.mark(), device).map_err(fatal)?;
        Ok(Capture::Voice(Live::new(mark, unit)))
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// Records which way it was asked to open.
    struct Which;

    impl OpenCapture for Which {
        type Capture = &'static str;
        fn plain(&self, _: &str, _: &Sender<Event>) -> Result<&'static str, Opening<String>> {
            Ok("plain")
        }
        fn voice(&self, _: &str, _: &Sender<Event>) -> Result<&'static str, Opening<String>> {
            Ok("voice")
        }
    }

    #[test]
    fn aec_setting_picks_the_capture_path() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let open = |aec| open_capture(&Which, aec, "USB", &tx).unwrap();
        assert_eq!(open(Aec::On), "voice");
        assert_eq!(open(Aec::Off), "plain");
        let on = Aec::from_env_value(Some("1")).unwrap();
        assert_eq!(open(on), "voice");
        let off = Aec::from_env_value(Some("0")).unwrap();
        assert_eq!(open(off), "plain");
    }
}
