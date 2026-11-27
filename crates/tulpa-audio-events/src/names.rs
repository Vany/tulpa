//! Which devices a listener's shell hears for their names, kept equal to the name listeners it really has.
//!
//! Two sides change them: a device-list notification (on `CoreAudio`'s thread) adds the new devices' listeners, and a
//! listener's drop (on its owner's) takes them all off. [`Names`] lets one side at a time do it, holding the list
//! across its calls, so the list never names a device whose listener the other side just removed (that device's
//! renames would go unheard on the reused shell). A notification never waits for the list: a removal may wait for a
//! notification under way, and that one waiting for the drop's list would never end. It asks instead, and whoever
//! holds the list does the look asked for before letting it go.
//!
//! Why no ask is lost: the ask is a flag under its own lock, held only to read or write it, so asking never waits on
//! the list. The asker sets it, then tries the list; the holder lets the list go, then reads it. If the holder's read
//! comes after the ask, it sees it and takes the list again (or a newer holder has it, which reads the flag after its
//! own release in the same way). If it comes before, the flag's lock orders the holder's release before the ask, so
//! the asker's try finds the list free, or held by a newer holder whose read is still to come. A bare atomic flag
//! would not do: a failed `try_lock` acquires nothing, so nothing ordered the holder's read after the asker's store.
#![expect(
    clippy::redundant_pub_crate,
    reason = "a private module's items: plain `pub` is rustc's unreachable_pub, which the workspace warns on"
)]

use std::mem;
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

/// What the name listeners are registered through: `CoreAudio`, or a test's fake.
pub(crate) trait Host {
    /// The device ids present now; empty when the list cannot be read.
    fn devices(&self) -> Vec<u32>;
    /// Registers the name listener on device `id`; whether it took.
    fn add(&self, id: u32) -> bool;
    /// Unregisters it. A device that is gone refuses, and that is all right: its listener went with it.
    fn remove(&self, id: u32);
}

/// The list and whether it is to be kept up.
struct Held {
    /// The devices whose name listener is registered.
    ids: Vec<u32>,
    /// Between [`Names::open`] and [`Names::close`]: device-list changes are looked at.
    open: bool,
}

/// A shell's name listeners (see the module docs).
pub(crate) struct Names {
    held: Mutex<Held>,
    /// A look was asked for and not done yet. Its lock is held only to read or write it (see the module docs).
    asked: Mutex<bool>,
}

impl Names {
    /// No listener, closed.
    pub(crate) const fn new() -> Self {
        Self {
            held: Mutex::new(Held {
                ids: Vec::new(),
                open: false,
            }),
            asked: Mutex::new(false),
        }
    }

    /// The listener is made (or its shell reused): the name of every device present is listened to, and from now on
    /// at each [`Names::changed`]. Waits for the list.
    pub(crate) fn open(&self, host: &impl Host) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.open = true;
        *self.flag() = true;
        self.look(held, host);
    }

    /// The device list changed: the new devices' names are listened to and the gone ones forgotten (their
    /// listeners went with them). Never waits: when the list is held, its holder does the look.
    pub(crate) fn changed(&self, host: &impl Host) {
        *self.flag() = true;
        if let Some(held) = self.free() {
            self.look(held, host);
        }
    }

    /// The listener is dropped: its name listener comes off every device, and no change is looked at until
    /// [`Names::open`]. Waits for the list, and holds it across the removals.
    pub(crate) fn close(&self, host: &impl Host) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.open = false;
        for id in mem::take(&mut held.ids) {
            host.remove(id);
        }
    }

    /// The devices whose name listener is registered.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<u32> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ids
            .clone()
    }

    /// The ask flag. Waits only for another read or write of it, never for the list.
    fn flag(&self) -> MutexGuard<'_, bool> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The list, unless another side holds it.
    fn free(&self) -> Option<MutexGuard<'_, Held>> {
        match self.held.try_lock() {
            Ok(held) => Some(held),
            Err(TryLockError::Poisoned(held)) => Some(held.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Does the asked look, then lets the list go, and takes it again for an ask that came meanwhile; the module docs
    /// say why none is lost.
    fn look<'a>(&'a self, mut held: MutexGuard<'a, Held>, host: &impl Host) {
        loop {
            let asked = mem::take(&mut *self.flag());
            if asked && held.open {
                sync(&mut held.ids, host);
            }
            drop(held);
            let asked = *self.flag();
            if !asked {
                return;
            }
            match self.free() {
                Some(again) => held = again,
                None => return,
            }
        }
    }
}

/// Registers the name listener on every device present that does not have it, and forgets the devices that are
/// gone. A device that refuses is left out: its rename shows at the next device event, as before names were
/// listened to.
fn sync(ids: &mut Vec<u32>, host: &impl Host) {
    let present = host.devices();
    ids.retain(|id| present.contains(id));
    for id in present {
        if !ids.contains(&id) && host.add(id) {
            ids.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread;
    use std::time::Duration;

    /// Long enough for a loaded machine; only a broken test waits it out.
    const GUARD: Duration = Duration::from_secs(30);

    /// Holds the next call of one kind until released: says it is in, waits.
    struct Gate {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    /// A gate, and its two ends for the test: in (the call is held) and release.
    fn gate() -> (Gate, Receiver<()>, Sender<()>) {
        let (entered, inside) = channel();
        let (release_tx, release) = channel();
        (Gate { entered, release }, inside, release_tx)
    }

    impl Gate {
        fn pass(self) {
            self.entered.send(()).unwrap();
            self.release
                .recv_timeout(GUARD)
                .expect("the test releases the call");
        }
    }

    /// Devices present, the listeners really registered (a set: `CoreAudio` keeps one registration of the same
    /// listener), and a gate for the next add or remove.
    #[derive(Default)]
    struct Fake {
        present: Mutex<Vec<u32>>,
        refuse: Vec<u32>,
        live: Mutex<BTreeSet<u32>>,
        hold_add: Mutex<Option<Gate>>,
        hold_remove: Mutex<Option<Gate>>,
    }

    impl Fake {
        fn with(present: &[u32]) -> Self {
            Self {
                present: Mutex::new(present.to_vec()),
                ..Self::default()
            }
        }
        fn live(&self) -> Vec<u32> {
            self.live.lock().unwrap().iter().copied().collect()
        }
    }

    impl Host for Fake {
        fn devices(&self) -> Vec<u32> {
            self.present.lock().unwrap().clone()
        }
        fn add(&self, id: u32) -> bool {
            let held = self.hold_add.lock().unwrap().take();
            if let Some(gate) = held {
                gate.pass();
            }
            if self.refuse.contains(&id) {
                return false;
            }
            self.live.lock().unwrap().insert(id);
            true
        }
        fn remove(&self, id: u32) {
            let held = self.hold_remove.lock().unwrap().take();
            if let Some(gate) = held {
                gate.pass();
            }
            self.live.lock().unwrap().remove(&id);
        }
    }

    /// #0530: a device-list change while the drop is taking the listeners off never leaves a device listed whose
    /// listener is gone, and the reused shell hears that device's renames again.
    #[test]
    fn a_change_during_the_drop_leaves_the_list_equal_to_the_listeners() {
        let host = Arc::new(Fake::with(&[7]));
        let names = Arc::new(Names::new());
        names.open(&*host);
        assert_eq!(names.ids(), [7]);
        let (held, inside, release) = gate();
        *host.hold_remove.lock().unwrap() = Some(held);
        let dropping = {
            let (host, names) = (Arc::clone(&host), Arc::clone(&names));
            thread::spawn(move || names.close(&*host))
        };
        inside.recv_timeout(GUARD).expect("the drop is removing");
        // The notification, on its own thread as `CoreAudio`'s is, while the removal is under way.
        let notified = {
            let (host, names) = (Arc::clone(&host), Arc::clone(&names));
            thread::spawn(move || names.changed(&*host))
        };
        notified.join().unwrap();
        release.send(()).unwrap();
        dropping.join().unwrap();
        assert_eq!(names.ids(), host.live(), "listed = registered");
        assert!(host.live().is_empty(), "the drop took them all");
        // The shell reused: the device's name is listened to again.
        names.open(&*host);
        assert_eq!(names.ids(), [7]);
        assert_eq!(host.live(), [7], "its renames are heard");
        names.close(&*host);
    }

    /// #0530: a device-list change while the list is held (here by the first look) is not lost: the holder looks
    /// again before letting go.
    #[test]
    fn a_change_while_the_list_is_held_is_looked_at_by_the_holder() {
        let host = Arc::new(Fake::with(&[1]));
        let names = Arc::new(Names::new());
        let (held, inside, release) = gate();
        *host.hold_add.lock().unwrap() = Some(held);
        let opening = {
            let (host, names) = (Arc::clone(&host), Arc::clone(&names));
            thread::spawn(move || names.open(&*host))
        };
        inside
            .recv_timeout(GUARD)
            .expect("the first look is adding");
        host.present.lock().unwrap().push(2);
        names.changed(&*host);
        release.send(()).unwrap();
        opening.join().unwrap();
        assert_eq!(names.ids(), [1, 2], "the device that came meanwhile");
        assert_eq!(host.live(), [1, 2]);
    }

    /// A device that refuses is left out, a gone one forgotten, a new one added; closed, a change adds nothing.
    #[test]
    fn the_list_follows_the_devices_while_open() {
        let host = Fake {
            refuse: vec![2],
            ..Fake::with(&[1, 2])
        };
        let names = Names::new();
        names.changed(&host);
        assert!(names.ids().is_empty(), "not open yet");
        names.open(&host);
        assert_eq!(names.ids(), [1], "the refusing device is left out");
        *host.present.lock().unwrap() = vec![2, 3];
        names.changed(&host);
        assert_eq!(names.ids(), [3], "gone forgotten, new added");
        names.close(&host);
        assert!(names.ids().is_empty());
        assert!(!host.live().contains(&3), "the one it listed is off");
        names.changed(&host);
        assert!(names.ids().is_empty(), "closed: a late change adds nothing");
    }
}
