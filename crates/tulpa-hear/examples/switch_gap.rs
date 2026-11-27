//! Measures the mic's capture gap on a real default-input switch, through the real path: `mic::run` follows
//! `CoreAudio`'s default-input event and reopens the stream in the same process.
//!
//! A Mac with one input has nothing to switch to, so this makes one: a temporary aggregate device that wraps the
//! current default input. It switches the default input to that device and back, prints for each switch the time from
//! the switch to the event (`Switched`), and the gap from the last audio chunk of the old stream to the first chunk of
//! the new one, then puts the default input back and removes the aggregate device (also on an error).
//!
//! Run it from a terminal with mic access: `EARS_STATE=$(mktemp -d) cargo run -p tulpa-hear --example switch_gap`.
//! Measurement only: no binary ships it.
#![cfg_attr(
    target_os = "macos",
    expect(
        unsafe_code,
        reason = "measurement only: a temporary aggregate device and the default input are set through CoreAudio's C API"
    )
)]

#[cfg(not(target_os = "macos"))]
fn main() {
    let _said = std::io::Write::write_all(&mut std::io::stderr(), b"switch_gap: macOS only\n");
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    let out = &mut std::io::stdout();
    match mac::run(out) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let _said = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("switch_gap: {e}\n").as_bytes(),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

/// The measurement, macOS only.
#[cfg(target_os = "macos")]
pub mod mac {
    use std::io::Write;
    use std::ptr::NonNull;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use coreaudio::audio_unit::macos_helpers::get_default_device_id;
    use cpal::traits::{DeviceTrait, HostTrait};
    use objc2_core_audio::{
        AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, AudioObjectID,
        AudioObjectPropertyAddress, AudioObjectSetPropertyData,
        kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
        kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceUIDKey,
        kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    };
    use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFString, CFType};
    use tulpa_hear::event::Event;
    use tulpa_hear::mic::{self, Settings};
    use tulpa_stop::Stop;

    /// The aggregate device's name, as the follower sees it.
    const NAME: &str = "Tulpa switch gap";
    /// How long each device is listened to.
    const HOLD: Duration = Duration::from_secs(3);

    /// The default input set to `id`.
    fn set_default_input(id: AudioObjectID) -> Result<(), String> {
        let at = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut value = id;
        let size = u32::try_from(size_of::<AudioObjectID>()).map_err(|e| e.to_string())?;
        // SAFETY: the system object takes an AudioObjectID for this property; both pointers outlive the call.
        let status = unsafe {
            AudioObjectSetPropertyData(
                kAudioObjectSystemObject.unsigned_abs(),
                NonNull::from(&at),
                0,
                std::ptr::null(),
                size,
                NonNull::from(&mut value).cast(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("default input not set (OSStatus {status})"))
        }
    }

    fn key(k: &std::ffi::CStr) -> CFRetained<CFString> {
        CFString::from_str(&k.to_string_lossy())
    }

    /// The temporary aggregate device over the input `uid`; dropping it restores `restore` as the default input
    /// and removes the device.
    struct Aggregate {
        id: AudioObjectID,
        restore: AudioObjectID,
    }

    impl Aggregate {
        fn over(uid: &str, restore: AudioObjectID) -> Result<Self, String> {
            let sub_uid = CFString::from_str(uid);
            let sub = CFDictionary::<CFString, CFType>::from_slices(
                &[&*key(kAudioSubDeviceUIDKey)],
                &[&**sub_uid],
            );
            let subs = CFArray::<CFType>::from_objects(&[&**sub]);
            let own_uid = CFString::from_str(&format!("tulpa-switch-gap-{}", std::process::id()));
            let name = CFString::from_str(NAME);
            let keys = [
                key(kAudioAggregateDeviceUIDKey),
                key(kAudioAggregateDeviceNameKey),
                key(kAudioAggregateDeviceSubDeviceListKey),
                key(kAudioAggregateDeviceMainSubDeviceKey),
            ];
            let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
            let values: [&CFType; 4] = [&own_uid, &name, &subs, &sub_uid];
            let description = CFDictionary::<CFString, CFType>::from_slices(&key_refs, &values);
            let mut id: AudioObjectID = 0;
            // SAFETY: a dictionary of the documented aggregate-device keys and value types; `id` outlives the call.
            let status = unsafe {
                AudioHardwareCreateAggregateDevice(description.as_opaque(), NonNull::from(&mut id))
            };
            if status == 0 {
                Ok(Self { id, restore })
            } else {
                Err(format!("aggregate device not created (OSStatus {status})"))
            }
        }
    }

    impl Drop for Aggregate {
        fn drop(&mut self) {
            let restored = set_default_input(self.restore)
                .err()
                .unwrap_or_else(|| "ok".to_owned());
            // SAFETY: the id this process created.
            let status = unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
            let _said = writeln!(
                std::io::stderr(),
                "switch_gap: default input restored: {restored}; aggregate removed: OSStatus {status}"
            );
        }
    }

    /// What the capture reported (each kept with when it arrived).
    #[derive(Debug)]
    enum Seen {
        /// An audio chunk.
        Audio,
        /// The follower switched to this device.
        Switched(String),
        /// This device's stream is opening (sent before any of its audio).
        Device(String),
    }

    /// One switch, timed.
    #[derive(Debug, PartialEq, Eq)]
    struct Timed {
        /// From the switch to the follower's `Switched`.
        event: Duration,
        /// From `Switched` to the new device's `Device`: its stream is opened and started (`Device` goes out once
        /// `play()` returned, and the new device's audio only after it).
        opened: Duration,
        /// From the old device's last chunk to the new device's first.
        gap: Duration,
    }

    /// Times the switch to `to` made at `switched_at`, from what the capture reported, in order of arrival.
    ///
    /// The new device's audio starts at its `Device` event. A chunk that arrives between `Switched` and that event
    /// is the old stream's, late (its callback was already sending when the stream was dropped): it counts as the
    /// old device's last, never as the new one's first.
    ///
    /// # Errors
    ///
    /// Which of the switch, the new device's opening, or the audio on either side of it was not seen.
    fn time_switch(
        seen: &[(Instant, Seen)],
        switched_at: Instant,
        to: &str,
    ) -> Result<Timed, String> {
        let (at_event, event) = seen
            .iter()
            .enumerate()
            .find(|(_, (at, s))| {
                *at >= switched_at && matches!(s, Seen::Switched(name) if name == to)
            })
            .map(|(i, (at, _))| (i, *at))
            .ok_or_else(|| format!("no switch to {to} was seen"))?;
        let (at_open, opened) = seen
            .iter()
            .enumerate()
            .skip(at_event)
            .find(|(_, (_, s))| matches!(s, Seen::Device(name) if name == to))
            .map(|(i, (at, _))| (i, *at))
            .ok_or_else(|| format!("{to} never opened after the switch"))?;
        let last_old = seen
            .iter()
            .take(at_open)
            .filter(|(_, s)| matches!(s, Seen::Audio))
            .map(|(at, _)| *at)
            .next_back()
            .ok_or("no audio before the switch")?;
        let first_new = seen
            .iter()
            .skip(at_open)
            .find(|(_, s)| matches!(s, Seen::Audio))
            .map(|(at, _)| *at)
            .ok_or("no audio after the new device opened")?;
        Ok(Timed {
            event: event.duration_since(switched_at),
            opened: opened.duration_since(event),
            gap: first_new.duration_since(last_old),
        })
    }

    /// Makes the device, switches to it and back, and writes one line per switch to `out`.
    ///
    /// # Errors
    ///
    /// Whatever step failed; the default input and the device are put back all the same.
    pub fn run(out: &mut dyn Write) -> Result<(), String> {
        let host = cpal::default_host();
        let mic_dev = host.default_input_device().ok_or("no default input")?;
        let mic_name = mic_dev
            .description()
            .map_err(|e| e.to_string())?
            .name()
            .to_owned();
        let uid = mic_dev.id().map_err(|e| e.to_string())?.id().to_owned();
        let original = get_default_device_id(true).ok_or("no default input id")?;
        let aggregate = Aggregate::over(&uid, original)?;
        // Let the new device settle before the capture starts.
        thread::sleep(Duration::from_millis(500));

        let settings = Settings::from_env().map_err(|e| e.to_string())?;
        let stop = Arc::new(Stop::new());
        let (tx, rx) = mpsc::channel::<Event>();
        let capture = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || mic::run(&settings, &tx, &stop).map_err(|e| e.to_string()))
        };
        let collector = thread::spawn(move || {
            let mut seen = Vec::new();
            for ev in rx {
                let what = match ev {
                    Event::Audio(_) => Seen::Audio,
                    Event::Switched { to, .. } => Seen::Switched(to),
                    Event::Device { name, .. } => Seen::Device(name),
                    _ => continue,
                };
                seen.push((Instant::now(), what));
            }
            seen
        });
        thread::sleep(HOLD);
        let to_aggregate = Instant::now();
        set_default_input(aggregate.id)?;
        thread::sleep(HOLD);
        let back = Instant::now();
        set_default_input(original)?;
        thread::sleep(HOLD);
        stop.set();
        let captured = capture.join().map_err(|_| "the capture panicked")?;
        let seen = collector.join().map_err(|_| "the collector panicked")?;
        drop(aggregate);
        captured?;

        for (switched_at, to) in [(to_aggregate, NAME), (back, mic_name.as_str())] {
            let t = time_switch(&seen, switched_at, to)?;
            writeln!(
                out,
                "switch to {to}: event {} ms after the switch, stream open {} ms after it, gap {} ms (last old chunk -> first new chunk)",
                t.event.as_millis(),
                t.opened.as_millis(),
                t.gap.as_millis()
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The old stream's chunk that arrives after `Switched` (its callback was already sending) is old audio: the
        /// gap runs from it to the new device's first chunk, after its `Device`, not from the chunk before the switch
        /// to that late one.
        #[test]
        fn a_late_old_chunk_is_not_the_new_devices_first() -> Result<(), String> {
            let t0 = Instant::now();
            let at = |ms: u64| t0.checked_add(Duration::from_millis(ms)).ok_or("clock");
            let seen = vec![
                (at(0)?, Seen::Audio),
                (at(10)?, Seen::Audio),
                (at(20)?, Seen::Switched("New".to_owned())),
                (at(21)?, Seen::Audio),
                (at(60)?, Seen::Device("New".to_owned())),
                (at(80)?, Seen::Audio),
            ];
            let switched_at = at(5)?;
            assert_eq!(
                time_switch(&seen, switched_at, "New")?,
                Timed {
                    event: Duration::from_millis(15),
                    opened: Duration::from_millis(40),
                    gap: Duration::from_millis(59),
                }
            );
            Ok(())
        }

        #[test]
        fn a_new_device_that_never_opens_is_an_error() -> Result<(), String> {
            let t0 = Instant::now();
            let at = |ms: u64| t0.checked_add(Duration::from_millis(ms)).ok_or("clock");
            let seen = vec![
                (at(0)?, Seen::Audio),
                (at(20)?, Seen::Switched("New".to_owned())),
                (at(21)?, Seen::Audio),
            ];
            assert!(time_switch(&seen, t0, "New").is_err());
            Ok(())
        }
    }
}
