//! The tray's `.close` marker on the "me" side: the ask to end the utterance in progress.
//!
//! The tray touches `<EARS_STATE>/me/.close` when it starts to speak. The live path takes the marker
//! ([`CloseMode::Take`]: removing it is the ask); the shadow rehearsal only watches it ([`CloseMode::Watch`]). An
//! ask is dated (the marker's mtime) and waits in [`Closes`] until the audio up to that date has been pushed.

use std::io;

use crate::hear::{Pipeline, say};

/// How a side treats the tray's `.close` marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseMode {
    /// The live path's: a marker that is there is removed, and that removal is the ask.
    Take,
    /// The shadow's: the marker is only looked at, never removed or written. A marker that appears (there now, not
    /// at the last look) is one ask; one that stays is not asked again until it has gone. The live path removes it
    /// within one chunk, so a marker it takes before the shadow's look is missed here.
    Watch,
}

/// One side's asked closes: looked for after every audio chunk, due once the audio up to the ask has been pushed.
#[derive(Debug)]
pub(crate) struct Closes {
    mode: CloseMode,
    /// A marker that cannot be read or removed was said (once).
    stuck_said: bool,
    /// [`CloseMode::Watch`]: the marker was there at the last look.
    seen: bool,
    /// The closes asked but not yet reached, as ms into the stream (the anchor's count).
    asks: Vec<u64>,
}

impl Closes {
    pub(crate) const fn new(mode: CloseMode) -> Self {
        Self {
            mode,
            stuck_said: false,
            seen: false,
            asks: Vec::new(),
        }
    }

    /// Looks for the marker, and says whether a close is due now that the stream (begun at `start`, epoch ms) has
    /// `read_ms` of audio pushed.
    ///
    /// A close is asked at a time (the marker's mtime); with a clock (`now`) that is a point in the stream, and the
    /// cut waits until the audio up to it has been pushed. The audio queued while whisper held the pipeline's thread
    /// was heard before the ask, or after it: cutting at the first queued chunk would join the speech on both sides.
    /// Without a clock the cut is at once; a marker dated ahead of the clock is taken as asked now. The chunk just
    /// pushed was (mostly) heard before the ask: it goes in.
    pub(crate) fn due<R: ?Sized>(
        &mut self,
        p: &Pipeline<'_, R>,
        now: Option<u64>,
        start: u64,
        read_ms: u64,
    ) -> bool {
        // A side that has been told to stop does not look any more: a marker written from now on is the next
        // capture's ask, and taking it here would eat it. The asks taken before the stop still count.
        let stopped = p.stop.is_some_and(tulpa_stop::Stop::is_set);
        let asked = match self.mode {
            _ if stopped => None,
            CloseMode::Take => close_asked(p, &mut self.stuck_said, now),
            CloseMode::Watch => close_appeared(p, &mut self.seen, now),
        };
        if let Some(at_ms) = asked {
            self.asks
                .push(now.map_or(0, |now| at_ms.min(now).saturating_sub(start)));
        }
        let due = self.asks.iter().any(|&a| a <= read_ms);
        if due {
            self.asks.retain(|&a| a > read_ms);
        }
        due
    }

    /// The stream is over: the closes still waiting for a point in it are dropped with it.
    pub(crate) fn clear(&mut self) {
        self.asks.clear();
    }
}

/// When the close marker was asked (its mtime, epoch ms), if it was there, removing it. The date comes from the stat
/// that found it; one that cannot be read is `now` (asked now, never at 0; without a clock, `now` is `None` and the
/// close is at once anyway). A marker the stat does not find is left for the next look; one it cannot read is said
/// once. Only a removed one counts (one close per ask); one that can't be removed is said once and otherwise
/// ignored.
pub(crate) fn close_asked<R: ?Sized>(
    p: &Pipeline<'_, R>,
    stuck_said: &mut bool,
    now: Option<u64>,
) -> Option<u64> {
    let path = p.close.as_ref()?;
    let found = match std::fs::symlink_metadata(path) {
        Ok(found) => found,
        Err(e) => {
            // Not there: nothing asked. Any other failure (a dir we may not search) would skip every ask in
            // silence: said once, as a marker that can't be removed is.
            if e.kind() != io::ErrorKind::NotFound && !*stuck_said {
                *stuck_said = true;
                say(p.side, &format!(".close not readable: {e}"));
            }
            return None;
        }
    };
    match std::fs::remove_file(path) {
        Ok(()) => Some(asked_at(&found, now)),
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound && !*stuck_said {
                *stuck_said = true;
                say(p.side, &format!(".close not removed: {e}"));
            }
            None
        }
    }
}

/// When the close marker was asked (as [`close_asked`]), if it is there now and was not at the last look; reads
/// only (`symlink_metadata`).
fn close_appeared<R: ?Sized>(
    p: &Pipeline<'_, R>,
    seen: &mut bool,
    now: Option<u64>,
) -> Option<u64> {
    let path = p.close.as_ref()?;
    let found = std::fs::symlink_metadata(path).ok();
    let appeared = found.is_some() && !*seen;
    *seen = found.is_some();
    found.filter(|_| appeared).map(|m| asked_at(&m, now))
}

/// A marker's mtime, epoch ms; `now` when it cannot be read (or is before 1970), 0 only without a clock.
fn asked_at(m: &std::fs::Metadata, now: Option<u64>) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(
            || now.unwrap_or(0),
            |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
        )
}
