//! The crate on macOS: `CoreAudio`'s C listener API, the one place it needs `unsafe`.
#![expect(
    unsafe_code,
    reason = "CoreAudio's property listener API is C: registering, the callback, unregistering"
)]

use std::ffi::c_void;
use std::fmt;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::names::{self, Names};
use crate::newest::{Reads, newest_only};

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
    kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectSystemObject,
};

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// The default input device (the mic the system picks).
    DefaultInput,
    /// The default output device.
    DefaultOutput,
    /// A device was added or removed.
    Devices,
    /// A device was renamed (its own `kAudioObjectPropertyName`).
    Name,
    /// Whether some process runs audio on a device (`kAudioDevicePropertyDeviceIsRunningSomewhere`): only
    /// [`running`] watches it, and its function gets the value; [`listen`]'s never gets this one.
    Running,
}

/// The three properties [`listen`] watches, by selector.
const WATCHED: [(u32, Change); 3] = [
    (
        kAudioHardwarePropertyDefaultInputDevice,
        Change::DefaultInput,
    ),
    (
        kAudioHardwarePropertyDefaultOutputDevice,
        Change::DefaultOutput,
    ),
    (kAudioHardwarePropertyDevices, Change::Devices),
];

/// The one property [`running`] watches, on its device.
const RUNNING: [(u32, Change); 1] = [(
    kAudioDevicePropertyDeviceIsRunningSomewhere,
    Change::Running,
)];

/// The system object's id (`kAudioObjectSystemObject` is a C int constant, 1).
const SYSTEM: AudioObjectID = kAudioObjectSystemObject.unsigned_abs();

impl Change {
    /// The change a selector stands for, as [`listen`]'s function gets it; `None` for one it does not watch
    /// (`CoreAudio` may pass those too), and for [`Change::Running`], which only [`running`]'s function gets.
    fn of(selector: u32) -> Option<Self> {
        if selector == kAudioObjectPropertyName {
            return Some(Self::Name);
        }
        WATCHED
            .iter()
            .find(|(watched, _)| *watched == selector)
            .map(|(_, change)| *change)
    }

    /// `default input`, `default output`, `device list`, `device name` or `device running`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultInput => "default input",
            Self::DefaultOutput => "default output",
            Self::Devices => "device list",
            Self::Name => "device name",
            Self::Running => "device running",
        }
    }
}

/// The address of a system property in the global scope.
const fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// What a shell's function is told: a change [`listen`]'s caller hears, or a device's running state [`running`]'s
/// caller hears. Each wraps its caller's function to take only its own, so a late notification on a reused shell
/// never reaches a function of the other kind.
#[derive(Debug, Clone, Copy)]
enum Notice {
    Change(Change),
    /// A read of `device`'s running state, numbered by [`READS`]; `on` is `None` when it could not be read.
    Running {
        device: AudioObjectID,
        read: u64,
        on: Option<bool>,
    },
}

/// The caller's function, wrapped.
type OnChange = Box<dyn Fn(Notice) + Send + Sync>;

/// What the pointer `CoreAudio` hands back to [`on_property`] points at: the caller's function, and the devices
/// whose names are listened to.
struct Client {
    /// The caller's function. Taken out on drop, so the shell (and nothing the function holds) outlives the
    /// [`Listener`].
    on_change: Mutex<Option<OnChange>>,
    /// The devices this shell's listener is registered on for their names.
    named: Names,
}

/// The device ids present now; empty when the list cannot be read (the names are then not listened to until the
/// next device-list change reads it again).
fn devices() -> Vec<AudioObjectID> {
    let at = address(kAudioHardwarePropertyDevices);
    let one = std::mem::size_of::<AudioObjectID>();
    let mut bytes = 0_u32;
    // SAFETY: `at` and `bytes` are valid for the call; no qualifier is passed (size 0, null).
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            SYSTEM,
            NonNull::from(&at),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
        )
    };
    let count = usize::try_from(bytes)
        .unwrap_or(0)
        .checked_div(one)
        .unwrap_or(0);
    if status != 0 || count == 0 {
        return Vec::new();
    }
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    let Some(out) = NonNull::new(ids.as_mut_ptr().cast::<c_void>()) else {
        return Vec::new();
    };
    // SAFETY: `out` points at `count` ids, which is the `bytes` passed in: `CoreAudio` writes at most that many
    // bytes and sets `bytes` to what it wrote (the list may have changed since its size was read).
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM,
            NonNull::from(&at),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
            out,
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(
        usize::try_from(bytes)
            .unwrap_or(0)
            .checked_div(one)
            .unwrap_or(0),
    );
    ids
}

/// The name listeners of one shell, registered with `CoreAudio`.
struct NameListeners(&'static Client);

impl names::Host for NameListeners {
    fn devices(&self) -> Vec<AudioObjectID> {
        devices()
    }

    fn add(&self, id: AudioObjectID) -> bool {
        let at = address(kAudioObjectPropertyName);
        // SAFETY: as in `listen`: `at` is valid for the call, `on_property` has the listener signature, and the
        // client data is the `&'static Client` it reads (never freed).
        let status = unsafe {
            AudioObjectAddPropertyListener(
                id,
                NonNull::from(&at),
                Some(on_property),
                client_data(self.0),
            )
        };
        status == 0
    }

    fn remove(&self, id: AudioObjectID) {
        let at = address(kAudioObjectPropertyName);
        // SAFETY: the same object, address, function and client data as registered in `add`.
        let _status: i32 = unsafe {
            AudioObjectRemovePropertyListener(
                id,
                NonNull::from(&at),
                Some(on_property),
                client_data(self.0),
            )
        };
    }
}

/// The pointer `CoreAudio` keeps for `client` and passes back.
const fn client_data(client: &'static Client) -> *mut c_void {
    std::ptr::from_ref::<Client>(client).cast_mut().cast()
}

/// Shells of listeners gone (unregistered, their function taken out), for the next [`listen`] to reuse.
///
/// A shell is never freed: a notification `CoreAudio` already started when its listener was removed may still read
/// it (whether the removal waits for one is not documented). Reused instead, the shells made are at most the
/// listeners alive at once, however often [`listen`] is called or refused. A late notification on a reused shell
/// calls the new function once for a system change it would have heard anyway. A shell whose removal failed is
/// still registered, so it is never put here: it stays empty for good.
static SPARE: Mutex<Vec<&'static Client>> = Mutex::new(Vec::new());

/// A shell holding `on_change`: a spare one, or a new one leaked.
fn shell(on_change: OnChange) -> &'static Client {
    let spare = SPARE.lock().unwrap_or_else(PoisonError::into_inner).pop();
    match spare {
        Some(client) => {
            *client
                .on_change
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(on_change);
            client
        }
        None => Box::leak(Box::new(Client {
            on_change: Mutex::new(Some(on_change)),
            named: Names::new(),
        })),
    }
}

/// The registered listener; dropping it unregisters it.
pub struct Listener {
    /// Points at a leaked [`Client`] ([`SPARE`] says why), handed back to [`SPARE`] on drop unless its removal
    /// failed.
    client: &'static Client,
    /// The object listened on: the system ([`listen`]) or a device ([`running`]).
    object: AudioObjectID,
    /// Its properties: [`WATCHED`] or [`RUNNING`].
    watched: &'static [(u32, Change)],
    /// How many of `watched` are registered (in order).
    added: usize,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("object", &self.object)
            .field("added", &self.added)
            .finish_non_exhaustive()
    }
}

/// Why [`listen`] could not register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenError {
    /// The property that could not be listened to.
    pub change: Change,
    /// `CoreAudio`'s status.
    pub status: i32,
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot listen for CoreAudio's {} changes (OSStatus {})",
            self.change.as_str(),
            self.status
        )
    }
}

impl std::error::Error for ListenError {}

/// Calls `on_change` on every change of the default input, the default output or the device list, until the
/// returned [`Listener`] is dropped.
///
/// # Errors
///
/// [`ListenError`] when `CoreAudio` refuses one of the three; none stays registered then.
pub fn listen(on_change: impl Fn(Change) + Send + Sync + 'static) -> Result<Listener, ListenError> {
    let listener = register(
        SYSTEM,
        &WATCHED,
        Box::new(move |notice| {
            if let Notice::Change(change) = notice {
                on_change(change);
            }
        }),
    )?;
    // The names of the devices there now; those that come later are added at each device-list change.
    listener.client.named.open(&NameListeners(listener.client));
    Ok(listener)
}

/// Calls `on_running` with whether some process runs audio on `device`, until the returned [`Listener`] is dropped.
///
/// Running is playing through an output or recording from an input. The state read just after registering is told
/// once, then each change. A state that cannot be read is told as `None` (unknown), and said once per process on
/// stderr. A state older than one already told is never told (`newest` says why).
///
/// # Errors
///
/// [`ListenError`] (its change [`Change::Running`]) when `CoreAudio` refuses: a device gone, say.
pub fn running(
    device: AudioObjectID,
    on_running: impl Fn(Option<bool>) + Send + Sync + 'static,
) -> Result<Listener, ListenError> {
    let newest = newest_only(device, on_running);
    let listener = register(
        device,
        &RUNNING,
        Box::new(move |notice| {
            if let Notice::Running { device, read, on } = notice {
                newest(device, read, on);
            }
        }),
    )?;
    // Read after registering, so a change from here on is notified too; told through the shell, as a notification
    // is, and read outside its lock (see `on_property`). A notification racing this read may be told first; then
    // this read, numbered before it, is dropped.
    tell(listener.client, &[read_running(device)]);
    Ok(listener)
}

/// Registers the shell holding `on_change` on `object` for each of `watched`.
fn register(
    object: AudioObjectID,
    watched: &'static [(u32, Change)],
    on_change: OnChange,
) -> Result<Listener, ListenError> {
    // On a refusal the listener's drop unregisters what was added and hands the shell back.
    let mut listener = Listener {
        client: shell(on_change),
        object,
        watched,
        added: 0,
    };
    for &(selector, change) in watched {
        let at = address(selector);
        // SAFETY: `at` is a valid address for the call; `on_property` has the listener signature and only reads
        // `client_data` as a `&Client`, which `listener.client` is for the rest of the process (never freed).
        let status = unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&at),
                Some(on_property),
                listener.client_data(),
            )
        };
        if status != 0 {
            return Err(ListenError { change, status });
        }
        listener.added = listener.added.saturating_add(1);
    }
    Ok(listener)
}

/// Numbers every read of a running state, across all listeners ([`newest_only`] says why).
static READS: Reads = Reads::new();

/// Reads `device`'s running state now, numbered before the read.
fn read_running(device: AudioObjectID) -> Notice {
    let read = READS.start();
    Notice::Running {
        device,
        read,
        on: running_somewhere(device),
    }
}

/// Whether some process runs audio on `device` now; `None` when it cannot be read, said once per process.
fn running_somewhere(device: AudioObjectID) -> Option<bool> {
    let at = address(kAudioDevicePropertyDeviceIsRunningSomewhere);
    let mut value = 0_u32;
    let mut bytes = u32::try_from(std::mem::size_of::<u32>()).ok()?;
    let out = NonNull::from(&mut value).cast::<c_void>();
    // SAFETY: `at` is valid for the call; `out` points at one `u32`, the `bytes` passed in, and the property is a
    // `UInt32`; no qualifier is passed (size 0, null).
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&at),
            0,
            std::ptr::null(),
            NonNull::from(&mut bytes),
            out,
        )
    };
    if status != 0 && !SAID_UNREAD.swap(true, Ordering::Relaxed) {
        let _said: std::io::Result<()> = writeln!(
            std::io::stderr(),
            "tulpa-audio-events: cannot read whether device {device} is running (OSStatus {status}); told as \
             unknown"
        );
    }
    (status == 0).then_some(value != 0)
}

/// Whether a running state that could not be read was said on stderr: once per process.
static SAID_UNREAD: AtomicBool = AtomicBool::new(false);

impl Listener {
    /// The pointer `CoreAudio` keeps and passes back.
    const fn client_data(&self) -> *mut c_void {
        client_data(self.client)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.client.named.close(&NameListeners(self.client));
        let mut kept = None;
        for (selector, change) in self.watched.iter().take(self.added) {
            let at = address(*selector);
            // SAFETY: the same object, address, function and client data as registered.
            let status: i32 = unsafe {
                AudioObjectRemovePropertyListener(
                    self.object,
                    NonNull::from(&at),
                    Some(on_property),
                    self.client_data(),
                )
            };
            if status != 0 {
                kept.get_or_insert((*change, status));
            }
        }
        // A notification under way finishes first (it holds the lock); later ones find nothing to call.
        drop(
            self.client
                .on_change
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        // A registration that would not go still points at this shell: reused, the next listener's function would
        // be called through it too (two listeners on one shell). It stays empty and out of the spares instead.
        if let Some((change, status)) = kept {
            if !SAID_KEPT.swap(true, Ordering::Relaxed) {
                let _said: std::io::Result<()> = writeln!(
                    std::io::stderr(),
                    "tulpa-audio-events: the {} listener was not removed (OSStatus {status}); its shell is not \
                     reused",
                    change.as_str()
                );
            }
            return;
        }
        SPARE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.client);
    }
}

/// Whether a shell kept out of [`SPARE`] (its removal failed) was said on stderr: once per process.
static SAID_KEPT: AtomicBool = AtomicBool::new(false);

/// The listener `CoreAudio` calls: each watched address in the batch goes to the caller's function.
unsafe extern "C-unwind" fn on_property(
    object: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: `client_data` is the `&'static Client` that `listen` registered (never freed).
    let client = unsafe { &*client_data.cast_const().cast::<Client>() };
    let count = usize::try_from(count).unwrap_or(0);
    // SAFETY: `CoreAudio` passes `count` (at least one) valid addresses for the duration of the call.
    let addresses = unsafe { std::slice::from_raw_parts(addresses.as_ptr().cast_const(), count) };
    // A panic must not unwind into CoreAudio's thread.
    let _called = catch_unwind(AssertUnwindSafe(|| {
        // A running state is read here, before the lock: no call into `CoreAudio` under it (`tell` says why). A
        // read that fails is still told, as unknown: the change is never swallowed.
        let notices: Vec<Notice> = addresses
            .iter()
            .filter_map(|a| {
                if a.mSelector == kAudioDevicePropertyDeviceIsRunningSomewhere {
                    Some(read_running(object))
                } else {
                    Change::of(a.mSelector).map(Notice::Change)
                }
            })
            .collect();
        let listed = tell(client, &notices);
        // Devices came or went: the new ones' names are listened to from here on. A listener dropped meanwhile
        // has closed its names, so nothing is added to its shell.
        if listed {
            client.named.changed(&NameListeners(client));
        }
    }));
    0
}

/// Calls `client`'s function, if it still has one, with each of `notices`; whether the device list changed.
///
/// The lock is released before any call into `CoreAudio`: the look after a device-list change reads the device list
/// and adds listeners, and a `CoreAudio` call made from inside its own notification while holding our lock could
/// wait for something that waits for the lock (a removal in `Listener::drop` takes it).
fn tell(client: &Client, notices: &[Notice]) -> bool {
    let on_change = client
        .on_change
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut listed = false;
    if let Some(on_change) = on_change.as_ref() {
        for &notice in notices {
            listed |= matches!(notice, Notice::Change(Change::Devices));
            on_change(notice);
        }
    }
    drop(on_change);
    listed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_watched_selector_maps_to_its_change_and_others_to_none() {
        assert_eq!(
            Change::of(kAudioHardwarePropertyDefaultInputDevice),
            Some(Change::DefaultInput)
        );
        assert_eq!(
            Change::of(kAudioHardwarePropertyDefaultOutputDevice),
            Some(Change::DefaultOutput)
        );
        assert_eq!(
            Change::of(kAudioHardwarePropertyDevices),
            Some(Change::Devices)
        );
        assert_eq!(Change::of(kAudioObjectPropertyName), Some(Change::Name));
        assert_eq!(Change::of(0), None);
        // Only `running`'s function hears a running state, with its value.
        assert_eq!(
            Change::of(kAudioDevicePropertyDeviceIsRunningSomewhere),
            None
        );
    }

    /// #0490: the listener's name listeners are on devices, each once, and the drop takes them all back; after it,
    /// a device-list notification puts none back on the shell given up (#0530). Only what this test caused is
    /// asserted: which devices accept a listener, and whether one comes or goes meanwhile, is the Mac's (the list's
    /// rules are tested against a fake in `names`).
    #[test]
    fn listens_for_the_names_and_the_drop_takes_them_back() -> Result<(), ListenError> {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let listener = listen(|_| {})?;
        let client = listener.client;
        let named = client.named.ids();
        let mut once = named.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(once.len(), named.len(), "each device once: {named:?}");
        drop(listener);
        assert!(
            client.named.ids().is_empty(),
            "the drop takes the name listeners back"
        );
        client.named.changed(&NameListeners(client));
        assert!(
            client.named.ids().is_empty(),
            "a late device-list change adds nothing to a dropped listener's shell"
        );
        Ok(())
    }

    /// One listening test at a time: they share the process's spare shells, and each counts its own.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// Listening again and again (as the tray's menu-open retry does) makes no new shell each time: a gone
    /// listener's shell is reused. No other test listens meanwhile, so every listen here after the first takes
    /// the shell the one before it gave back, however many listening tests there are.
    #[test]
    fn listening_again_reuses_the_shell() -> Result<(), ListenError> {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let mut shells = Vec::new();
        for _ in 0..20 {
            let listener = listen(|_| {})?;
            let at = std::ptr::from_ref::<Client>(listener.client);
            if !shells.contains(&at) {
                shells.push(at);
            }
        }
        assert_eq!(shells.len(), 1, "{} shells for 20 listens", shells.len());
        Ok(())
    }

    /// The real system object takes all three, and the drop takes them back and lets the function go.
    #[test]
    fn listens_on_this_mac_and_unregisters() -> Result<(), ListenError> {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let held = std::sync::Arc::new(());
        let mine = std::sync::Arc::clone(&held);
        let listener = listen(move |_| drop(std::sync::Arc::clone(&mine)))?;
        assert_eq!(listener.added, WATCHED.len());
        assert_eq!(std::sync::Arc::strong_count(&held), 2);
        drop(listener);
        assert_eq!(
            std::sync::Arc::strong_count(&held),
            1,
            "the function is let go"
        );
        Ok(())
    }

    /// A device takes the running listener, tells its state once at the start (whatever it is: other processes
    /// decide), and the drop takes it back and lets the function go.
    #[test]
    fn listens_for_a_device_running_and_unregisters() -> Result<(), ListenError> {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(&device) = devices().first() else {
            return Ok(());
        };
        let (told, states) = std::sync::mpsc::channel();
        let held = std::sync::Arc::new(());
        let mine = std::sync::Arc::clone(&held);
        let listener = running(device, move |on| {
            let _kept = std::sync::Arc::clone(&mine);
            let _told = told.send(on);
        })?;
        assert_eq!((listener.object, listener.added), (device, RUNNING.len()));
        assert!(states.try_recv().is_ok(), "the state at the start is told");
        drop(listener);
        assert_eq!(
            std::sync::Arc::strong_count(&held),
            1,
            "the function is let go"
        );
        Ok(())
    }
}
