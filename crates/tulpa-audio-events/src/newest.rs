//! Which reads of a device's running state reach [`running`](crate::running)'s caller: never one older than a read
//! already told.
//!
//! The state is read in two places: once by `running` just after it registers, and again by each notification.
//! Neither read is made under a lock (no `CoreAudio` call under ours, see the crate's `tell`), so the two can race:
//! a notification may read the new state and tell it before the constructor tells the value it read just before
//! the change. That value, told last, would stand until the next change.
//!
//! So every read takes a number from [`Reads`] *before* it reads, and [`newest_only`] tells only a read numbered
//! above every one told so far. Why a stale value cannot win: `CoreAudio` notifies after each change, so the
//! notification of the last change takes its number after that change, and it, like any read numbered after it,
//! reads the last state. The highest number told is therefore a read of the last state; a read that started earlier
//! (the constructor's, racing that notification) has a lower number and is dropped if it comes after.
#![expect(
    clippy::redundant_pub_crate,
    reason = "a private module's items: plain `pub` is rustc's unreachable_pub, which the workspace warns on"
)]

use std::sync::atomic::{AtomicU64, Ordering};

/// Hands out the reads' numbers, rising, from 1.
pub(crate) struct Reads(AtomicU64);

impl Reads {
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// The number of a read about to be made; take it before reading.
    pub(crate) fn start(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst).saturating_add(1)
    }
}

/// Wraps `on_running` (a caller's function for `device`) to take each read as `(device, number, state)`, and to pass
/// on only the state of a read of `device` numbered above every read it passed on before. Another device's read (a
/// late notification on a reused shell) is dropped too.
pub(crate) fn newest_only(
    device: u32,
    on_running: impl Fn(Option<bool>),
) -> impl Fn(u32, u64, Option<bool>) {
    let told = AtomicU64::new(0);
    move |of, read, on| {
        if of == device && told.fetch_max(read, Ordering::SeqCst) < read {
            on_running(on);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use super::*;

    /// What a [`newest_only`] wrapper for device 7 passes on, given `reads` in the order they are told.
    fn told(reads: &[(u32, u64, Option<bool>)]) -> Vec<Option<bool>> {
        let got = Mutex::new(Vec::new());
        let tell = newest_only(7, |on| {
            got.lock().unwrap_or_else(PoisonError::into_inner).push(on);
        });
        for &(of, read, on) in reads {
            tell(of, read, on);
        }
        drop(tell);
        got.into_inner().unwrap_or_else(PoisonError::into_inner)
    }

    /// The race: the constructor numbers its read (1) and reads "not running"; the output starts, its notification
    /// numbers its read (2), reads "running" and tells it first; the constructor's stale value comes after and is
    /// dropped, so "running" stands. Kills: telling every read (the stale `false` told last).
    #[test]
    fn a_read_started_earlier_and_told_later_is_dropped() {
        let reads = Reads::new();
        let constructor = reads.start();
        let notification = reads.start();
        assert!(constructor < notification);
        assert_eq!(
            told(&[(7, notification, Some(true)), (7, constructor, Some(false))]),
            [Some(true)]
        );
    }

    /// In order, every read is told, unknown too; a number told twice is told once; another device's read is not
    /// told. Kills: a filter that drops in-order reads (`<=`), drops unknowns, or ignores the device.
    #[test]
    fn reads_in_order_of_this_device_are_all_told() {
        assert_eq!(
            told(&[
                (7, 1, Some(false)),
                (7, 2, Some(true)),
                (7, 2, Some(true)),
                (8, 3, Some(false)),
                (7, 4, None),
                (7, 5, Some(false)),
            ]),
            [Some(false), Some(true), None, Some(false)]
        );
    }
}
