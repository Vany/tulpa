//! What a capture (the mic or the tap) reports on its channel. Platform-neutral, so the [`hear`](crate::hear)
//! pipeline and its tests run everywhere; `mic` and `tap` produce it on macOS.

/// What the capture reports.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// A stream on this device runs, at its default input config: sent once its start returned with no stream error
    /// meanwhile, and before any audio of that device (the audio until then is dropped). A start that fails sends
    /// no `Device` (a [`StreamError`](Event::StreamError) and a [`Warning`](Event::Warning) instead).
    Device {
        /// The device's name.
        name: String,
        /// Its rate, Hz.
        rate: u32,
        /// Its channel count.
        channels: u16,
    },
    /// The follower moved to another device (logged to `mic-switch.log`). A [`Device`](Event::Device) follows
    /// before the new device's audio.
    Switched {
        /// The previous device, `None` after an outage.
        from: Option<String>,
        /// The new device.
        to: String,
    },
    /// There is no default device (an input for the mic, an output for the tap): the stream is dropped until one
    /// appears.
    NoInput,
    /// 16 kHz mono samples.
    Audio(Vec<i16>),
    /// The open stream reported an error (the audio API's error callback): it may have stopped delivering. Until
    /// the next [`Device`](Event::Device), the stream is not taken to be open.
    StreamError(String),
    /// This much of the stream's time went by with no audio passed on (a stall past the fill's bound): the stream's
    /// timeline is broken here. The phrase in progress ends, and what follows is anchored anew, as after a
    /// [`StreamError`](Event::StreamError). The stream itself is still open.
    Lost {
        /// The time lost, ms.
        ms: u64,
    },
    /// Whether some process plays through the tapped output device (the tap only: `CoreAudio`'s
    /// `kAudioDevicePropertyDeviceIsRunningSomewhere`), sent after the tap's [`Device`](Event::Device) and at each
    /// change; `None` when `CoreAudio` could not say (unknown). Running is not the same as audible: an app may keep
    /// the output running while it plays silence.
    OutputRunning(Option<bool>),
    /// Something went wrong without stopping the capture: a device that failed to open (retried `RETRY_AFTER` after
    /// the attempt ended), a sample the chain could not convert, or an unwritable switch log.
    Warning(String),
}
