//! `tulpa-hear --pcm-out`'s pump: capture [`Event`]s to raw 16 kHz mono s16le.
//!
//! Audio goes to one writer (stdout), one line per other event to another (stderr). Platform-neutral, so it is
//! tested with fake events.
//!
//! Only audio goes to `out`, flushed after every block; nothing else ever does. A capture without a device
//! ([`Event::NoInput`]) writes nothing: no silence fill, so the reader's clock simply sees no samples.

use std::io::{self, Write};
use std::sync::mpsc::Receiver;

use crate::dsp::{OUT_RATE, to_s16le};
use crate::event::Event;

/// How [`pump`] ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// `out` returned `BrokenPipe`: the reader went away. A clean stop.
    Closed,
    /// The capture hung up its sender (it stopped or failed; its own result says which).
    Ended,
}

/// Copies `rx`'s audio to `out` as s16le, flushing per block, and writes one line per other event to `err`, until
/// the sender hangs up or `out` breaks.
///
/// Lines on `err` are best effort: a failure to write one is ignored.
///
/// # Errors
///
/// Any error writing or flushing `out` other than [`io::ErrorKind::BrokenPipe`] (which is [`Outcome::Closed`]).
pub fn pump(
    rx: &Receiver<Event>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<Outcome> {
    for ev in rx {
        let line = match ev {
            Event::Audio(samples) => {
                match out
                    .write_all(&to_s16le(&samples))
                    .and_then(|()| out.flush())
                {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(Outcome::Closed),
                    Err(e) => return Err(e),
                }
                continue;
            }
            Event::Device {
                name,
                rate,
                channels,
            } => format!(
                "pcm-out from {name:?} ({rate} Hz, {channels} ch) as {OUT_RATE} Hz s16le mono"
            ),
            Event::Switched {
                from: Some(from),
                to,
            } => format!("switched from {from:?} to {to:?}"),
            Event::Switched { from: None, to } => format!("switched to {to:?}"),
            Event::NoInput => "no input device; writing nothing until one appears".to_owned(),
            Event::StreamError(e) => format!("warning: stream: {e}"),
            Event::Lost { ms } => format!("warning: {ms} ms with no audio passed on"),
            Event::Warning(w) => format!("warning: {w}"),
            Event::OutputRunning(on) => format!(
                "output running: {}",
                on.map_or("unknown", |on| if on { "true" } else { "false" })
            ),
        };
        let _said: io::Result<()> = writeln!(err, "tulpa-hear: {line}");
    }
    Ok(Outcome::Ended)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn run(events: Vec<Event>) -> (io::Result<Outcome>, Vec<u8>, String) {
        let (tx, rx) = mpsc::channel();
        for ev in events {
            tx.send(ev).ok();
        }
        drop(tx);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let r = pump(&rx, &mut out, &mut err);
        (r, out, String::from_utf8(err).unwrap_or_default())
    }

    fn device() -> Event {
        Event::Device {
            name: "Built-in Mic".to_owned(),
            rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn audio_is_exactly_the_samples_as_s16le_in_order() {
        let (r, out, err) = run(vec![
            Event::Audio(vec![0, 1, -1, 256]),
            Event::Audio(vec![i16::MIN, i16::MAX, -2]),
        ]);
        assert_eq!(r.ok(), Some(Outcome::Ended));
        assert_eq!(
            out,
            [
                0x00, 0x00, 0x01, 0x00, 0xff, 0xff, 0x00, 0x01, 0x00, 0x80, 0xff, 0x7f, 0xfe, 0xff
            ]
        );
        assert_eq!(err, "");
    }

    #[test]
    fn other_events_write_one_stderr_line_each_and_nothing_on_stdout() {
        let (r, out, err) = run(vec![
            device(),
            Event::Switched {
                from: Some("Built-in Mic".to_owned()),
                to: "USB Mic".to_owned(),
            },
            Event::Switched {
                from: None,
                to: "USB Mic".to_owned(),
            },
            Event::NoInput,
            Event::Warning("stream error".to_owned()),
        ]);
        assert_eq!(r.ok(), Some(Outcome::Ended));
        assert_eq!(out, Vec::<u8>::new());
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 5, "{err}");
        assert_eq!(
            lines.first().copied(),
            Some(
                "tulpa-hear: pcm-out from \"Built-in Mic\" (48000 Hz, 2 ch) as 16000 Hz s16le mono"
            )
        );
        assert!(lines.iter().all(|l| l.starts_with("tulpa-hear: ")));
    }

    #[test]
    fn audio_between_events_still_reaches_stdout_alone() {
        let (_, out, err) = run(vec![device(), Event::Audio(vec![7]), Event::NoInput]);
        assert_eq!(out, [7, 0]);
        assert_eq!(err.lines().count(), 2);
    }

    /// A writer that fails every write and flush with `kind`.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(self.0))
        }
    }

    fn pump_into(kind: io::ErrorKind) -> io::Result<Outcome> {
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Audio(vec![1, 2])).ok();
        tx.send(Event::Audio(vec![3])).ok();
        drop(tx); // a pump that ignored the error would end as `Ended` instead
        pump(&rx, &mut Failing(kind), &mut Vec::new())
    }

    #[test]
    fn a_broken_pipe_is_a_clean_close() {
        assert_eq!(
            pump_into(io::ErrorKind::BrokenPipe).ok(),
            Some(Outcome::Closed)
        );
    }

    #[test]
    fn another_write_error_is_an_error() {
        let r = pump_into(io::ErrorKind::StorageFull);
        assert_eq!(r.err().map(|e| e.kind()), Some(io::ErrorKind::StorageFull));
    }
}
