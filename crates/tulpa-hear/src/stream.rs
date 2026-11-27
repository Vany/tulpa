//! What the mic and the tap share on macOS: the `CoreAudio` host as the follower sees it, its device events, and
//! the `cpal` input stream both open (the tap is an input stream on an output device).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use coreaudio::audio_unit::macos_helpers::{get_device_id_from_name, get_device_name};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, SampleFormat, Stream};
use tulpa_audio_events::{ListenError, Listener};

use crate::dsp::Chain;
use crate::event::Event;
use crate::follow::{self, DeviceHost, Flow, Names, names_kept};
use crate::start::{Held, Live, Mark, gated, report, start};

/// Which of the system's devices a [`Host`] answers about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Input,
    Output,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// The system's `CoreAudio` host, answering the follower's questions about its inputs (the mic) or its outputs (the
/// tap).
pub struct Host {
    host: cpal::Host,
    direction: Direction,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host")
            .field("direction", &self.direction)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// The default (`CoreAudio`) host's inputs.
    #[must_use]
    pub fn inputs() -> Self {
        Self {
            host: cpal::default_host(),
            direction: Direction::Input,
        }
    }

    /// The default (`CoreAudio`) host's outputs.
    #[must_use]
    pub fn outputs() -> Self {
        Self {
            host: cpal::default_host(),
            direction: Direction::Output,
        }
    }

    fn devices(&self) -> Result<impl Iterator<Item = cpal::Device> + use<>, cpal::Error> {
        match self.direction {
            Direction::Input => self.host.input_devices(),
            Direction::Output => self.host.output_devices(),
        }
    }

    /// The device called `name`, if it is there.
    pub(crate) fn device(&self, name: &str) -> Option<cpal::Device> {
        self.devices()
            .ok()?
            .find(|d| device_name(d).as_deref() == Some(name))
    }
}

impl DeviceHost for Host {
    fn default_name(&self) -> Result<Option<String>, String> {
        let default = match self.direction {
            Direction::Input => self.host.default_input_device(),
            Direction::Output => self.host.default_output_device(),
        };
        default_read(default.as_ref(), || self.devices(), self.direction.as_str())
    }

    fn names(&self) -> Result<Names, String> {
        names_read(self.devices(), self.direction.as_str())
    }

    /// `CoreAudio`'s `AudioDeviceID` of the device called `name`, in this host's direction.
    fn id_of(&self, name: &str) -> Option<u64> {
        get_device_id_from_name(name, self.direction == Direction::Input).map(u64::from)
    }

    /// `CoreAudio`'s name of the device `id`, as [`DeviceHost::id_of`] compares it.
    fn name_of(&self, id: u64) -> Option<String> {
        get_device_name(u32::try_from(id).ok()?).ok()
    }
}

/// The name `cpal` reports for `d`, or why it was not read.
fn named(d: &cpal::Device) -> Result<String, String> {
    d.description()
        .map(|desc| desc.name().to_owned())
        .map_err(|e| format!("a device's name was not read: {e}"))
}

/// The name `cpal` reports for `d`.
fn device_name(d: &cpal::Device) -> Option<String> {
    named(d).ok()
}

/// The skips [`names_kept`] said in this process: each is said once.
static SKIPPED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// [`DeviceHost::names`] from one direction's device list; `what` is `input` or `output`. Only a list that was not
/// read is an error; a device whose name was not read is skipped ([`names_kept`]).
fn names_read<I>(devices: Result<I, cpal::Error>, what: &str) -> Result<Names, String>
where
    I: Iterator<Item = cpal::Device>,
{
    let devices = devices.map_err(|e| format!("{what} devices not read: {e}"))?;
    Ok(names_kept(devices.map(|d| named(&d)), what, &SKIPPED))
}

/// [`DeviceHost::default_name`] from one direction's default device and device list, by [`follow::default_from`]:
/// a default that is `None`, or whose name fails (macOS's answer when there is no device), is "no device" only when
/// that list is empty with no name skipped.
fn default_read<I>(
    default: Option<&cpal::Device>,
    devices: impl FnOnce() -> Result<I, cpal::Error>,
    what: &str,
) -> Result<Option<String>, String>
where
    I: Iterator<Item = cpal::Device>,
{
    follow::default_from(default.map(named), || names_read(devices(), what))
}

/// `CoreAudio`'s device changes as the channel [`follow::drive`] waits on: one item per change, whatever changed (the
/// follower looks at what it follows). The changes stop with the listener; keep it while the channel is read.
///
/// # Errors
///
/// The listener could not be registered: fail, never fall back to a clock.
pub(crate) fn device_changes() -> Result<(Listener, tulpa_stop::Receiver<()>), ListenError> {
    let (tell, changes) = tulpa_stop::channel();
    let listener = tulpa_audio_events::listen(move |_| {
        let _told: Result<(), tulpa_stop::Gone> = tell.send(());
    })?;
    Ok((listener, changes))
}

/// Sends `ev`; [`Flow::Stop`] when the receiver has hung up.
pub(crate) fn send(events: &Sender<Event>, ev: Event) -> Flow {
    if events.send(ev).is_ok() {
        Flow::Go
    } else {
        Flow::Stop
    }
}

/// The chain at `rate` Hz and `channels`, as a callback that sends its output as `Audio` (or a `Warning`).
pub(crate) fn feeder(
    rate: u32,
    channels: u16,
    events: &Sender<Event>,
) -> Result<impl FnMut(&[f32]) + Send + 'static, String> {
    let mut chain = Chain::new(rate, channels).map_err(|e| e.to_string())?;
    let audio = events.clone();
    Ok(move |frames: &[f32]| match chain.push(frames) {
        Ok(out) if out.is_empty() => {}
        Ok(out) => {
            let _sent = audio.send(Event::Audio(out));
        }
        Err(e) => {
            let _sent = audio.send(Event::Warning(e.to_string()));
        }
    })
}

/// What a stream's error callback reports (the mic's and the tap's: both streams are opened here).
///
/// An error the stream lives through is a [`Event::Warning`] (`stream: …`); in cpal 0.18.2 those are an overrun
/// (`Xrun`: the device's processor-overload listener, which neither pauses nor stops the stream), a reroute of the
/// default device (`DeviceChanged`, reported without a pause after the audio unit rerouted; only default-output
/// streams get it) and refused real-time scheduling (`RealtimeDenied`, reported from inside a running data
/// callback). Every other kind — a device gone or a stream invalidated (cpal pauses the stream before it says
/// so), and any kind cpal adds later — may have stopped the stream: [`Event::StreamError`], the safe side.
pub(crate) fn stream_event(e: &cpal::Error) -> Event {
    match e.kind() {
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied => {
            Event::Warning(format!("stream: {e}"))
        }
        _ => Event::StreamError(e.to_string()),
    }
}

/// Opens an input stream on `device` (called `name`) at `supported`, its native config, and starts it ([`start`]:
/// `Device` once it runs); the callback runs the chain and sends `Audio`, none of it before the `Device`
/// ([`gated`]). On an output-only device `cpal` records its output through a process tap. A
/// `StreamError` its error callback reports sets the stream's failure mark ([`Live::failure`]), which wakes the
/// follower to reopen it.
///
/// # Errors
///
/// Why the stream could not be built or started, or that it reported an error while starting ([`start`]).
pub(crate) fn open_stream(
    device: &cpal::Device,
    name: &str,
    supported: cpal::SupportedStreamConfig,
    events: &Sender<Event>,
) -> Result<Live<Stream>, String> {
    let config = supported.config();
    let (rate, channels) = (config.sample_rate, config.channels);
    let mark = Held::default();
    let chain = feeder(rate, channels, events).map_err(|e| format!("\"{name}\": {e}"))?;
    // Nothing until `start` has sent the `Device`: the first buffers can come while `play` still runs.
    let mut feed = gated(Arc::clone(mark.mark()), chain);
    let on_error = error_callback(events.clone(), Arc::clone(mark.mark()));
    let built = match supported.sample_format() {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |d: &[f32], _: &cpal::InputCallbackInfo| feed(d),
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |d: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = d.iter().map(|&s| f32::from(s) / 32768.0).collect();
                feed(&f);
            },
            on_error,
            None,
        ),
        other => return Err(format!("\"{name}\": unsupported sample format {other}")),
    };
    let stream = built.map_err(|e| format!("\"{name}\": cannot open the input stream: {e}"))?;
    let device = Event::Device {
        name: name.to_owned(),
        rate,
        channels,
    };
    // Built before the start, so a start that fails drops it in order too: the mark retired, then the stream.
    let live = Live::new(mark, stream);
    start(events, live.mark(), device, || {
        live.stream()
            .play()
            .map_err(|e| format!("\"{name}\": cannot start the input stream: {e}"))
    })?;
    Ok(live)
}

/// The stream's error callback: [`report`]s what cpal said ([`stream_event`]: a `StreamError` sets the failure mark,
/// which is the follower's wake, and is sent, ordered with the `Device`).
fn error_callback(
    events: Sender<Event>,
    errored: Arc<Mark>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e: cpal::Error| report(&events, &errored, stream_event(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The error callback `open_stream` gives cpal: a stream error sets the failure mark (the follower's wake) and
    /// goes out as a `StreamError`; an overrun is only a warning and sets nothing.
    #[test]
    fn a_stream_error_marks_reports_and_wakes() {
        let (tx, rx) = std::sync::mpsc::channel();
        let errored = Arc::new(Mark::default());
        let mut callback = error_callback(tx, Arc::clone(&errored));
        callback(cpal::Error::new(ErrorKind::Xrun));
        // Each wait is a guard against a wedge, not a timing claim: the callback has returned.
        let guard = Duration::from_secs(60);
        let xrun = rx.recv_timeout(guard);
        assert!(matches!(xrun, Ok(Event::Warning(_))), "{xrun:?}");
        assert!(!errored.failure().is_set(), "an overrun does not mark");
        callback(cpal::Error::new(ErrorKind::StreamInvalidated));
        assert!(errored.failure().is_set(), "the mark");
        let error = rx.recv_timeout(guard);
        assert!(matches!(error, Ok(Event::StreamError(_))), "{error:?}");
    }

    /// cpal reports an overrun (on every processor overload of the tapped device), a reroute of the default device
    /// and refused real-time scheduling through the error callback while the stream keeps running: those are
    /// warnings. Any other kind may have stopped the stream, and so may a kind cpal adds later.
    #[test]
    fn a_stream_lives_through_an_overrun_a_reroute_and_no_realtime() {
        use cpal::Error;
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
        ] {
            assert!(
                matches!(stream_event(&Error::new(kind)), Event::Warning(w) if w.starts_with("stream: ")),
                "{kind:?}"
            );
        }
        // Every other kind of cpal 0.18.2, as the catch-all arm gives it (and a later kind).
        for kind in [
            ErrorKind::DeviceBusy,
            ErrorKind::DeviceNotAvailable,
            ErrorKind::HostUnavailable,
            ErrorKind::InvalidInput,
            ErrorKind::PermissionDenied,
            ErrorKind::ResourceExhausted,
            ErrorKind::StreamInvalidated,
            ErrorKind::UnsupportedConfig,
            ErrorKind::UnsupportedOperation,
            ErrorKind::BackendError,
            ErrorKind::Other,
        ] {
            assert!(
                matches!(stream_event(&Error::new(kind)), Event::StreamError(_)),
                "{kind:?}"
            );
        }
    }
}
