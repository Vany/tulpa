//! Runs the real [`WhisperRecognizer`] on one WAV file and prints the transcript as JSON, for the fixtures under
//! `tests/fixtures/whisper/` (never hand-edited).
//!
//! ```sh
//! cargo run --release -p tulpa-hear --features whisper --example transcribe_wav -- \
//!     <model-dir> <wav: 16 kHz mono s16> <lang> [--no-vad]
//! ```
//!
//! Output: [`Transcript::json`](tulpa_hear::recognize::Transcript::json)'s, the same the raw ring keeps.
//! Anything but a 16 kHz mono 16-bit PCM WAV is refused with exit status 2.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use tulpa_hear::recognize::{Recognizer, to_f32};
use tulpa_hear::whisper::WhisperRecognizer;

/// Prints `why` on stderr and gives exit status `code`.
fn fail(code: u8, why: &str) -> ExitCode {
    let _w = writeln!(std::io::stderr(), "transcribe_wav: {why}");
    ExitCode::from(code)
}

/// Exit status 2: bad arguments or input audio.
fn refuse(why: &str) -> ExitCode {
    fail(2, why)
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(i..i.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(i..i.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The samples of a RIFF/WAVE file holding 16 kHz mono 16-bit PCM, or why not.
fn read_wav(b: &[u8]) -> Result<Vec<i16>, String> {
    if b.get(0..4) != Some(b"RIFF") || b.get(8..12) != Some(b"WAVE") {
        return Err("not a RIFF/WAVE file".to_owned());
    }
    let mut fmt_ok = false;
    let mut i = 12_usize;
    while let Some(len) = u32_at(b, i.saturating_add(4)) {
        let id = b.get(i..i.saturating_add(4)).unwrap_or_default();
        let body = i.saturating_add(8);
        let len = usize::try_from(len).map_err(|e| e.to_string())?;
        let end = body.saturating_add(len);
        match id {
            b"fmt " => {
                let format = u16_at(b, body).ok_or("short fmt chunk")?;
                let channels = u16_at(b, body.saturating_add(2)).ok_or("short fmt chunk")?;
                let rate = u32_at(b, body.saturating_add(4)).ok_or("short fmt chunk")?;
                let bits = u16_at(b, body.saturating_add(14)).ok_or("short fmt chunk")?;
                // 0xFFFE is WAVE_FORMAT_EXTENSIBLE; its subformat is not checked beyond the bits, rate and channels.
                if !matches!(format, 1 | 0xFFFE) || channels != 1 || rate != 16_000 || bits != 16 {
                    return Err(format!(
                        "need 16 kHz mono s16 PCM, got format {format:#x}, {channels} channel(s), {rate} Hz, {bits} bits"
                    ));
                }
                fmt_ok = true;
            }
            b"data" => {
                if !fmt_ok {
                    return Err("data chunk before fmt chunk".to_owned());
                }
                let data = b.get(body..end).ok_or("data chunk runs past the file")?;
                return Ok(data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| i16::from_le_bytes(c))
                    .collect());
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        i = end.saturating_add(len % 2);
    }
    Err("no data chunk".to_owned())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_vad = args.iter().any(|a| a == "--no-vad");
    let pos: Vec<&String> = args.iter().filter(|a| *a != "--no-vad").collect();
    let [dir, wav, lang] = pos.as_slice() else {
        return refuse("usage: transcribe_wav <model-dir> <wav 16 kHz mono s16> <lang> [--no-vad]");
    };
    let bytes = match std::fs::read(wav) {
        Ok(b) => b,
        Err(e) => return refuse(&format!("cannot read {wav}: {e}")),
    };
    let samples = match read_wav(&bytes) {
        Ok(s) => s,
        Err(e) => return refuse(&format!("{wav}: {e}")),
    };
    let mut rec = match WhisperRecognizer::load(Path::new(dir.as_str()), None) {
        Ok(r) => r,
        Err(e) => return fail(1, &e.to_string()),
    };
    rec.set_vad(!no_vad);
    let t = match rec.transcribe(&to_f32(&samples), lang) {
        Ok(t) => t,
        Err(e) => return fail(1, &e.to_string()),
    };
    match std::io::stdout().write_all(t.json().as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(1, &format!("stdout: {e}")),
    }
}
