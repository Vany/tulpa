//! `CoreAudio`'s device events, so whatever follows the audio devices waits for a change instead of re-reading
//! them on a clock.
//!
//! [`listen`] registers one listener on the system object for three properties: the default input, the default
//! output and the device list (a device added or removed, which also covers a pinned device that goes away and
//! comes back). The same listener is registered on every device for its name, and again on each device that
//! appears, so a device renamed in Audio MIDI Setup is a change too. Each change calls the caller's function with a
//! [`Change`], on `CoreAudio`'s own notification thread; the function must not block (send on a channel, set a
//! flag). [`Listener`] unregisters on drop.
//!
//! [`running`] listens on one device for whether some process runs audio on it
//! (`kAudioDevicePropertyDeviceIsRunningSomewhere`), and calls its function with that state, the same way; `None`
//! when it cannot be read. It is never told a state older than one it was told already.
//!
//! macOS only; elsewhere the crate is empty. The one shipped place in the workspace with `unsafe` (the other is the
//! measurement example `crates/tulpa-hear/examples/switch_gap.rs`): `CoreAudio`'s listener API is C, and no crate in
//! the tree wraps it for the system object.

#[cfg(target_os = "macos")]
mod macos;
// Portable, so the name listeners' bookkeeping is tested on any host, against a fake.
#[cfg(any(target_os = "macos", test))]
mod names;
// Portable, so the order of the running-state reads is tested on any host.
#[cfg(any(target_os = "macos", test))]
mod newest;

#[cfg(target_os = "macos")]
pub use macos::*;
