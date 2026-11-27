//! The ears' system audio ("them"): what the Mac plays on its default output, as 16 kHz mono `i16` chunks.
//!
//! It is recorded through `cpal`'s `CoreAudio` process tap and fed through the [`dsp`](crate::dsp) chain.
//!
//! `cpal` 0.18 records an output device when an *input* stream is opened on it: it creates a process tap on that
//! device plus a private aggregate device and reads the aggregate (`LoopbackDevice::from_device`). That needs macOS
//! 14.6 or later, which [`run`] checks first through [`osver`](crate::osver) and refuses loudly below; there is no
//! fallback.
//!
//! [`run`] follows the default output with the same [`follow::drive`] loop as the mic: an unchanged default does
//! nothing; a change drops the tap, writes one `them: default output switched: <from> -> <to>` line to stderr, and
//! builds one new tap on the new device at its own (native) output config.
//!
//! What it hears: `cpal` 0.18 builds the tap with an empty process list, which captures every process, and offers no
//! way to exclude any. So "them" includes all processes' audio, our own voice too; [`run`] says so once on stderr.
//! (The old `sysaudio` capture has the same property.) No `unsafe` works around it here; FFI stays in `cpal`.
//!
//! An output device that also has inputs (a USB headset, say) cannot be tapped this way: `cpal` would open its
//! microphone instead. Such a device is skipped with one warning until the default output changes.
//!
//! Each open tap also listens to whether its device is running (some process plays through it), and sends that as
//! [`Event::OutputRunning`]: the tap sends nothing while nothing plays, so only that tells a dead tap from a silent
//! Mac ([`beat::DEAF_AFTER`](crate::beat::DEAF_AFTER)).
//!
//! It never writes audio anywhere.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fmt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use cpal::Stream;
use cpal::traits::DeviceTrait;
use tulpa_audio_events::{ListenError, Listener};
use tulpa_stop::Stop;

use crate::event::Event;
use crate::follow::{
    self, DeviceHost, Flow, FollowError, Follower, Halt, Mic, Opening, Source, switch_line,
};
use crate::osver::{self, SYSTEM_VERSION_PLIST, VersionError};
use crate::start::Live;
use crate::stream::{Host, device_changes, open_stream, send};

/// The line [`run`] prints once at start.
pub const INCLUDES_ALL: &str =
    "them: includes all processes' audio, our own voice too (cpal 0.18 has no exclude list)";

/// Why the tap stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum TapError {
    /// This macOS cannot run a process tap, or its version could not be read.
    Os(VersionError),
    /// The follower cannot go on.
    Follow(FollowError),
    /// `CoreAudio` refused the device listener: output changes could not be followed.
    Listen(ListenError),
}

impl fmt::Display for TapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Os(e) => e.fmt(f),
            Self::Follow(e) => e.fmt(f),
            Self::Listen(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TapError {}

/// Checks the macOS version, then taps the default output until `stop` is set, sending [`Event`]s to `events`.
///
/// Returns `Ok` when stopped (or when the receiver hangs up).
///
/// # Errors
///
/// [`TapError::Os`] below macOS 14.6, or when `SystemVersion.plist` cannot be read or parsed.
pub fn run(events: &Sender<Event>, stop: &Stop) -> Result<(), TapError> {
    osver::check_tap_at(Path::new(SYSTEM_VERSION_PLIST)).map_err(TapError::Os)?;
    say(INCLUDES_ALL);
    let (_listener, changes) = device_changes().map_err(TapError::Listen)?;
    let outputs = Host::outputs();
    let mut source = TapSource {
        outputs: &outputs,
        events,
    };
    follow::drive(
        Follower::new(Mic::Default),
        &outputs,
        &mut source,
        &changes,
        stop,
    )
    .map_err(|h| match h {
        Halt::Follow(e) => TapError::Follow(e),
        Halt::Fatal(never) => match never {},
    })
}

/// One line to stderr.
fn say(line: &str) {
    crate::warn(line);
}

/// The failures to listen to an output's running state said in this process ([`TapSource::running`]).
static UNHEARD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// An open tap.
struct Tapped {
    /// Tells whether the device is running; `None` when it could not be listened to (said once per distinct
    /// failure).
    /// Declared first, so it is dropped first: no running state of this device comes after its stream is gone.
    _running: Option<Listener>,
    live: Live<Stream>,
}

/// The tap as a [`Source`].
struct TapSource<'a> {
    outputs: &'a Host,
    events: &'a Sender<Event>,
}

impl TapSource<'_> {
    /// Sends the output device `name`'s running state, now and at each change, while the returned listener lives;
    /// `None` when it cannot be listened to (the tap runs on, but a deaf tap goes unsaid), said once per distinct
    /// failure: a device that keeps failing is reopened every `RETRY_AFTER`.
    fn running(&self, name: &str) -> Option<Listener> {
        let listened = self
            .outputs
            .id_of(name)
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| format!("output device \"{name}\" has no CoreAudio id"))
            .and_then(|id| {
                let events = self.events.clone();
                tulpa_audio_events::running(id, move |on| {
                    let _sent = events.send(Event::OutputRunning(on));
                })
                .map_err(|e| e.to_string())
            });
        listened
            .map_err(|e| {
                crate::warn_once(
                    &UNHEARD,
                    &format!(
                        "them: warning: {e}; a tap that hears nothing while the output plays will not be said"
                    ),
                );
            })
            .ok()
    }
}

impl Source for TapSource<'_> {
    type Capture = Tapped;
    type Error = Infallible;

    fn open(&mut self, name: &str) -> Result<Tapped, Opening<Infallible>> {
        let device = self.outputs.device(name).ok_or_else(|| {
            Opening::Retry(format!(
                "output device \"{name}\" vanished before it opened"
            ))
        })?;
        if device.supports_input() {
            let why = format!(
                "them: output device \"{name}\" also has an input, so cpal 0.18 would record its microphone \
                 instead of tapping it; no system audio until the default output changes"
            );
            say(&why);
            return Err(Opening::Skip(why));
        }
        let config = device
            .default_output_config()
            .map_err(|e| Opening::Retry(format!("\"{name}\": no default output config: {e}")))?;
        let live = open_stream(&device, name, config, self.events)
            .map_err(|e| Opening::Retry(format!("them: {e}")))?;
        // After the stream's `Device`, so its running state follows it.
        Ok(Tapped {
            _running: self.running(name),
            live,
        })
    }

    fn failure<'a>(&'a self, capture: &'a Tapped) -> Option<&'a Stop> {
        Some(capture.live.failure())
    }

    fn switched(&mut self, from: Option<&str>, to: &str) -> Flow {
        say(&switch_line("them", "output", from, to));
        send(
            self.events,
            Event::Switched {
                from: from.map(str::to_owned),
                to: to.to_owned(),
            },
        )
    }

    fn no_device(&mut self) -> Flow {
        say("them: no default output; waiting for one");
        send(self.events, Event::NoInput)
    }

    fn warn(&mut self, message: String) -> Flow {
        send(self.events, Event::Warning(message))
    }
}
