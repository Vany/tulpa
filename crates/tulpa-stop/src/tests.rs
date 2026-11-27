#![expect(clippy::unwrap_used, reason = "tests")]

use super::*;
use std::thread;
use std::time::Duration;

/// How long a test lets a waiter settle into its wait before it sets the stop (an order, not a bound: a set before
/// the wait is seen at once too).
const SETTLE: Duration = Duration::from_millis(50);
/// The wedge guard: a wait the event does not end fails the test here, instead of hanging it.
const GUARD: Duration = Duration::from_secs(60);
/// A deadline past the guard, so only the event can end the wait before the guard: a deadline that came first
/// would return a set stop as if woken by it.
const PAST_GUARD: Duration = Duration::from_secs(3600);

fn far() -> Option<Instant> {
    Instant::now().checked_add(PAST_GUARD)
}

/// Runs `f` on a plain thread; its result comes on the returned channel, read with [`GUARD`].
fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _told = tx.send(f());
    });
    rx
}

#[test]
fn wait_until_returns_on_set_not_at_the_deadline() {
    let stop = Arc::new(Stop::new());
    let waiter = {
        let stop = Arc::clone(&stop);
        on_thread(move || stop.wait_until(far()))
    };
    thread::sleep(SETTLE);
    stop.set();
    assert_eq!(waiter.recv_timeout(GUARD), Ok(true));
}

#[test]
fn wait_until_times_out_unset_and_returns_at_once_when_set() {
    let stop = Stop::new();
    let began = Instant::now();
    assert!(!stop.wait_until(began.checked_add(Duration::from_millis(30))));
    assert!(began.elapsed() >= Duration::from_millis(30));
    stop.set();
    assert!(stop.wait_until(None));
    stop.clear();
    assert!(!stop.is_set());
    assert!(!stop.wait_until(Some(Instant::now())));
}

#[test]
fn a_waker_runs_on_every_set_until_its_guard_goes() {
    let stop = Stop::new();
    let (tx, rx) = mpsc::channel();
    {
        let wake: Waker = Arc::new(move || {
            let _sent = tx.send(());
        });
        let _watch = stop.on_set(&wake);
        stop.set();
        stop.set(); // already set: no second wake
        stop.clear();
        stop.set();
    }
    stop.clear();
    stop.set();
    assert_eq!(rx.try_iter().count(), 2);
}

#[test]
fn a_waker_registered_on_a_set_stop_runs_at_once() {
    let stop = Stop::new();
    stop.set();
    let (tx, rx) = mpsc::channel();
    let wake: Waker = Arc::new(move || {
        let _sent = tx.send(());
    });
    let _watch = stop.on_set(&wake);
    assert_eq!(rx.try_iter().count(), 1);
}

#[test]
fn recv_until_wakes_on_stop_not_at_the_deadline() {
    let stop = Arc::new(Stop::new());
    let (_tx, rx) = channel::<u8>();
    let got = {
        let stop = Arc::clone(&stop);
        on_thread(move || rx.recv_until(&[&stop], far()))
    };
    thread::sleep(SETTLE);
    stop.set();
    assert_eq!(got.recv_timeout(GUARD), Ok(Got::Stopped));
}

#[test]
fn recv_until_takes_items_then_ends_when_the_last_sender_goes() {
    let (tx, rx) = channel();
    let stop = Stop::new();
    let tx2 = tx.clone();
    tx.send(1).unwrap();
    drop(tx);
    tx2.send(2).unwrap();
    assert_eq!(rx.recv_until(&[&stop], far()), Got::Item(1));
    assert_eq!(rx.recv_until(&[&stop], far()), Got::Item(2));
    drop(tx2);
    assert_eq!(rx.recv_until(&[&stop], far()), Got::Ended);
    assert_eq!(rx.recv_until(&[&stop], None), Got::Ended);
}

#[test]
#[expect(clippy::panic, reason = "the test is about a producer that panics")]
fn a_panicking_producer_still_ends_the_channel() {
    let (tx, rx) = channel::<u8>();
    let producer = thread::spawn(move || {
        let _held = tx;
        panic!("producer died");
    });
    assert!(producer.join().is_err());
    assert_eq!(rx.recv_until(&[], far()), Got::Ended);
}

#[test]
fn recv_until_times_out_and_a_stale_wake_is_not_a_stop() {
    let (_tx, rx) = channel::<u8>();
    let stop = Stop::new();
    {
        // A wake queued while waiting on `stop`, which is then cleared again.
        let _watch = rx_wake(&rx, &stop);
        stop.set();
    }
    stop.clear();
    let began = Instant::now();
    let got = rx.recv_until(&[&stop], began.checked_add(Duration::from_millis(30)));
    assert_eq!(got, Got::Timeout);
}

/// Registers the receiver's own waker on `stop`, as [`Receiver::recv_until`] does.
fn rx_wake<'a, T>(rx: &Receiver<T>, stop: &'a Stop) -> OnSet<'a>
where
    T: Send + 'static,
{
    let wake = rx.wake.clone();
    stop.on_set(&(Arc::new(move || wake.wake()) as Waker))
}

#[test]
fn a_full_bounded_channel_still_sees_the_stop_and_never_blocks_the_setter() {
    let (tx, rx) = sync_channel(1);
    tx.send(7).unwrap();
    let stop = Arc::new(Stop::new());
    {
        let _watch = rx_wake(&rx, &stop);
        // The channel is full: the wake must not wait for room (a set that did would never return).
        let set = {
            let stop = Arc::clone(&stop);
            on_thread(move || stop.set())
        };
        assert_eq!(set.recv_timeout(GUARD), Ok(()));
    }
    assert_eq!(rx.recv_until(&[&stop], far()), Got::Stopped);
    stop.clear();
    assert_eq!(rx.recv_until(&[&stop], far()), Got::Item(7));
    assert_eq!(rx.try_recv(), None);
}

#[test]
fn either_of_two_stops_wakes_the_receiver() {
    let paused = Arc::new(Stop::new());
    let abandoned = Arc::new(Stop::new());
    let (_tx, rx) = channel::<u8>();
    let got = {
        let (paused, abandoned) = (Arc::clone(&paused), Arc::clone(&abandoned));
        on_thread(move || rx.recv_until(&[&paused, &abandoned], far()))
    };
    thread::sleep(SETTLE);
    abandoned.set();
    assert_eq!(got.recv_timeout(GUARD), Ok(Got::Stopped));
    assert!(abandoned.is_set() && !paused.is_set());
}

/// A set stop wins over the producers' end, as it wins over queued items: the producer ends (and that end is
/// taken), then the stop is set, and the next wait is `Stopped`, not `Ended`. Ordered on events, no sleeps.
#[test]
fn a_set_stop_wins_over_the_end() {
    let (tx, rx) = channel::<u8>();
    let stop = Stop::new();
    drop(tx);
    assert_eq!(rx.recv_until(&[&stop], None), Got::Ended);
    stop.set();
    assert_eq!(rx.recv_until(&[&stop], None), Got::Stopped);
    stop.clear();
    assert_eq!(rx.recv_until(&[&stop], None), Got::Ended, "it stays ended");
}
