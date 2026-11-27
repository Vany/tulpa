//! One side's events on their way to its pipeline: a queue that holds a bounded time of audio and, past it, lets
//! the OLDEST go.
//!
//! The pipeline's thread waits on whisper (and on the other side's whisper, behind the shared recognizer). While it
//! does, the side's audio queues here. Without a bound a recognizer slower than real time grows the queue, and the
//! lines' delay, without limit. With it the delay is bounded: audio older than the bound is dropped where it waits,
//! and an [`Event::Lost`] takes its place, so the pipeline ends the phrase there and anchors what follows anew
//! instead of sliding silently.
//!
//! Only audio is dropped, and silence before speech: a fill chunk (the tap's gap, all zeros) goes before any real
//! audio does. Every other event keeps its place. The reader waits on a condition variable: no poll.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tulpa_segment::SAMPLE_RATE;

use crate::event::Event;

#[derive(Debug, Default)]
struct State {
    events: VecDeque<Event>,
    /// The audio samples queued.
    samples: usize,
    /// The [`Feed`] is gone: what is queued is the end.
    ended: bool,
    /// The [`Drain`] is gone: nothing more is taken.
    unread: bool,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    /// The most audio samples held.
    max: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The writing end; dropping it ends the queue once it is read out.
#[derive(Debug)]
pub struct Feed(Arc<Shared>);

/// The reading end: the events in order, until the [`Feed`] is gone and the queue is empty.
#[derive(Debug)]
pub struct Drain(Arc<Shared>);

/// A queue that holds at most `max` audio samples.
#[must_use]
pub fn bounded(max: usize) -> (Feed, Drain) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        ready: Condvar::new(),
        max,
    });
    (Feed(Arc::clone(&shared)), Drain(shared))
}

/// The length of `n` samples, ms.
fn ms_of(n: usize) -> u64 {
    u64::try_from(n)
        .unwrap_or(u64::MAX)
        .saturating_mul(1000)
        .checked_div(u64::from(SAMPLE_RATE))
        .unwrap_or(0)
}

impl Feed {
    /// Queues `ev`; `false` when the reader is gone. Audio past the bound pushes the oldest audio out, which
    /// becomes (or adds to) an [`Event::Lost`] where it was.
    pub fn send(&self, ev: Event) -> bool {
        let mut st = self.0.lock();
        if st.unread {
            return false;
        }
        if let Event::Audio(a) = &ev {
            st.samples = st.samples.saturating_add(a.len());
        }
        st.events.push_back(ev);
        while st.samples > self.0.max {
            // A fill first (the tap's silence: all zeros), wherever it stands: real audio is never pushed out
            // while a fill is queued. With none left, the oldest audio. A fill is told by its content, so a
            // device that sends exact zeros (a mic muted in hardware) is evicted first too: acceptable, it is
            // silence as well, and nothing said is lost with it.
            let fill = |e: &Event| matches!(e, Event::Audio(a) if a.iter().all(|&s| s == 0));
            let oldest = |e: &Event| matches!(e, Event::Audio(_));
            let Some(at) = st
                .events
                .iter()
                .position(fill)
                .or_else(|| st.events.iter().position(oldest))
            else {
                break;
            };
            let gone = match st.events.get(at) {
                Some(Event::Audio(a)) => a.len(),
                _ => 0,
            };
            st.samples = st.samples.saturating_sub(gone);
            let before = at.checked_sub(1).and_then(|b| st.events.get_mut(b));
            if let Some(Event::Lost { ms }) = before {
                *ms = ms.saturating_add(ms_of(gone));
                drop(st.events.remove(at));
            } else if let Some(slot) = st.events.get_mut(at) {
                *slot = Event::Lost { ms: ms_of(gone) };
            }
        }
        drop(st);
        self.0.ready.notify_one();
        true
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.0.lock().ended = true;
        self.0.ready.notify_one();
    }
}

impl Iterator for Drain {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        let mut st = self.0.lock();
        loop {
            if let Some(ev) = st.events.pop_front() {
                if let Event::Audio(a) = &ev {
                    st.samples = st.samples.saturating_sub(a.len());
                }
                return Some(ev);
            }
            if st.ended {
                return None;
            }
            st = self
                .0
                .ready
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.0.lock().unread = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(tag: i16, n: usize) -> Event {
        Event::Audio(vec![tag; n])
    }

    /// Past the bound the oldest audio goes and a `Lost` stands where it was, growing as more goes; the newest
    /// audio and every other event stay, in order.
    #[test]
    fn the_oldest_audio_goes_and_a_lost_takes_its_place() {
        let (feed, drain) = bounded(3 * 1600);
        assert!(feed.send(Event::NoInput));
        for tag in 1..=5 {
            assert!(feed.send(audio(tag, 1600)));
        }
        assert!(feed.send(Event::Warning("after".to_owned())));
        drop(feed);
        let got: Vec<Event> = drain.collect();
        assert_eq!(
            got,
            [
                Event::NoInput,
                Event::Lost { ms: 200 },
                audio(3, 1600),
                audio(4, 1600),
                audio(5, 1600),
                Event::Warning("after".to_owned()),
            ]
        );
    }

    /// akira F4: a fill (the tap's silence, all zeros) goes before any real audio does. Real audio is never pushed
    /// out while a fill chunk is queued, wherever the fill stands; with no fill left, the oldest audio goes.
    #[test]
    fn a_fill_goes_before_real_audio_does() {
        let (feed, drain) = bounded(3 * 1600);
        assert!(feed.send(audio(1, 1600)));
        assert!(feed.send(audio(0, 1600)));
        assert!(feed.send(audio(2, 1600)));
        // Over the bound: the fill goes, not the older real audio.
        assert!(feed.send(audio(3, 1600)));
        // Over it again, and no fill is left: now the oldest audio goes.
        assert!(feed.send(audio(4, 1600)));
        drop(feed);
        assert_eq!(
            drain.collect::<Vec<_>>(),
            [
                Event::Lost { ms: 100 },
                Event::Lost { ms: 100 },
                audio(2, 1600),
                audio(3, 1600),
                audio(4, 1600),
            ]
        );
    }

    /// Inside the bound nothing is dropped, and reading makes room again.
    #[test]
    fn audio_inside_the_bound_passes_whole() {
        let (feed, mut drain) = bounded(2 * 1600);
        assert!(feed.send(audio(1, 1600)));
        assert!(feed.send(audio(2, 1600)));
        assert_eq!(drain.next(), Some(audio(1, 1600)));
        assert!(feed.send(audio(3, 1600)));
        drop(feed);
        assert_eq!(drain.collect::<Vec<_>>(), [audio(2, 1600), audio(3, 1600)]);
    }

    /// The reader waits for an event and for the end, and a writer learns that the reader is gone.
    #[test]
    fn the_reader_wakes_on_an_event_and_on_the_end() {
        let (feed, mut drain) = bounded(1600);
        let reading = std::thread::spawn(move || {
            let first = drain.next();
            let end = drain.next();
            (first, end)
        });
        assert!(feed.send(Event::NoInput));
        drop(feed);
        let (first, end) = reading.join().unwrap_or((None, Some(Event::NoInput)));
        assert_eq!((first, end), (Some(Event::NoInput), None));
        let (feed, drain) = bounded(1600);
        drop(drain);
        assert!(!feed.send(Event::NoInput), "the reader is gone");
    }
}
