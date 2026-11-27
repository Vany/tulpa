//! The optional voice-processed mic (`EARS_MIC_AEC=1`): Apple's `VoiceProcessingIO` audio unit.
//!
//! What it does for us today is Apple's noise suppression and gain control. It does NOT cancel our own speech: its
//! echo reference is its own output unit, which we feed silence, so the agent's TTS played elsewhere reaches the mic
//! as before. The `.close` marker stays the echo guard.
//!
//! All FFI stays inside `coreaudio-rs`; this module only configures the unit through its safe API. The unit runs
//! duplex, as voice processing expects: input on element 1 from the followed device at its rate as mono `f32`,
//! output on element 0 (the default output) fed silence, so the unit plays nothing of its own.

use std::fmt;

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::get_device_id_from_name;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use objc2_audio_toolbox::{
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
};

type Args = render_callback::Args<data::Interleaved<f32>>;

/// A running `VoiceProcessingIO` unit; dropping it stops and disposes the unit.
pub struct VoiceUnit {
    _unit: AudioUnit,
}

impl fmt::Debug for VoiceUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceUnit").finish_non_exhaustive()
    }
}

/// The `CoreAudio` id of the input device called `name`, the same name `cpal` reports.
pub fn input_device_id(name: &str) -> Option<u32> {
    get_device_id_from_name(name, true)
}

/// Opens a `VoiceProcessingIO` unit on input device `device` at `rate` Hz, mono `f32`, and starts it; `feed`
/// receives every input buffer.
///
/// # Errors
///
/// The step that failed and `CoreAudio`'s error, when the unit cannot be created, configured, initialized or started.
pub fn open(
    device: u32,
    rate: u32,
    mut feed: impl FnMut(&[f32]) + 'static,
) -> Result<VoiceUnit, String> {
    let step =
        |what: &'static str| move |e: coreaudio::Error| format!("VoiceProcessingIO: {what}: {e}");
    let mut unit = AudioUnit::new_uninitialized(IOType::VoiceProcessingIO)
        .map_err(step("cannot create the unit"))?;
    let on = 1_u32;
    unit.set_property(
        kAudioOutputUnitProperty_EnableIO,
        Scope::Input,
        Element::Input,
        Some(&on),
    )
    .map_err(step("cannot enable input"))?;
    unit.set_property(
        kAudioOutputUnitProperty_CurrentDevice,
        Scope::Global,
        Element::Input,
        Some(&device),
    )
    .map_err(step("cannot select the input device"))?;
    let format = StreamFormat {
        sample_rate: f64::from(rate),
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels: 1,
    };
    unit.set_stream_format(format, Scope::Output, Element::Input)
        .map_err(step("cannot set the input format"))?;
    unit.set_stream_format(format, Scope::Input, Element::Output)
        .map_err(step("cannot set the output format"))?;
    unit.initialize().map_err(step("cannot initialize"))?;
    unit.set_input_callback(move |args: Args| {
        feed(args.data.buffer);
        Ok(())
    })
    .map_err(step("cannot set the input callback"))?;
    unit.set_render_callback(|args: Args| {
        args.data.buffer.fill(0.0);
        Ok(())
    })
    .map_err(step("cannot set the silent output"))?;
    unit.start().map_err(step("cannot start"))?;
    Ok(VoiceUnit { _unit: unit })
}
