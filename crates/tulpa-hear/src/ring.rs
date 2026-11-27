//! The raw ring, as the capture script keeps it: every consumed piece parks in a local directory, byte-bounded.
//!
//! The oldest piece is dropped first, so the last minute or so of both sides can be read back (`--replay`) after
//! the fact. Nothing here ships anything anywhere.
//!
//! Per piece, three files, each `0600`, named by side and the piece's stem (`me-c1795350156023.wav`): the
//! utterance as a 16 kHz mono s16le WAV, the segmenter's JSON line beside it, and `.whisper.json`, what the
//! recognizer made of it ([`Transcript::json`]: its tokens with their times and probabilities, its special-token
//! count, whether it found speech at all) — so a `<voice>` or `<no speech>` piece can be read back without running
//! whisper again.
//!
//! The directory is `EARS_RING_DIR`, default `/tmp/tulpa-ears-ring-<uid>`: the system's own `/tmp` cleanup is the
//! retention, and the uid keeps two accounts apart. It must be a plain directory this user owns (`0700`, made so
//! when missing, and excluded from Time Machine): a symlink or another user's directory planted there is refused at
//! [`Ring::open`] and, should it appear while running, drops the piece loudly instead of writing through it. The
//! bound is `EARS_RING_MAX_MB` (whole MiB, 1..1024, no leading zero; default [`RING_MAX_MB`]); after every piece the
//! oldest files go until the directory's files fit.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tulpa_segment::{Utterance, put, wav};

use crate::hear::Side;
use crate::recognize::Transcript;

/// The default bound (`EARS_RING_MAX_MB`), MiB: about a minute of both sides at 32 kB/s each, plus slack.
pub const RING_MAX_MB: u64 = 8;

/// The ring: its directory and bound; one writer at a time (both sides share it).
#[derive(Debug)]
pub struct Ring {
    dir: PathBuf,
    max_bytes: u64,
    uid: u32,
    lock: Mutex<()>,
    /// Whether [`Ring::keep`] deletes the oldest files past the bound; `false` once the run is told to stop
    /// ([`Ring::append_only`]).
    prunes: AtomicBool,
}

/// `EARS_RING_MAX_MB` in bytes: unset or empty is [`RING_MAX_MB`].
///
/// # Errors
///
/// A value that is not a whole number of MiB from 1 to 1024 written without a leading zero, named (the script's rule).
pub fn max_bytes_from(v: Option<&str>) -> Result<u64, String> {
    let mb = match v.filter(|s| !s.is_empty()) {
        None => RING_MAX_MB,
        Some(s) => s
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then_some(s)
            .filter(|s| !s.starts_with('0'))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| (1..=1024).contains(n))
            .ok_or_else(|| {
                format!("EARS_RING_MAX_MB must be a whole number of MiB, 1..1024, no leading zero: {s:?}")
            })?,
    };
    Ok(mb.saturating_mul(1024 * 1024))
}

/// The ring's directory: `EARS_RING_DIR` when set and not empty, else `/tmp/tulpa-ears-ring-<uid>`.
#[must_use]
pub fn dir_from(v: Option<&str>, uid: u32) -> PathBuf {
    v.filter(|s| !s.is_empty()).map_or_else(
        || PathBuf::from(format!("/tmp/tulpa-ears-ring-{uid}")),
        PathBuf::from,
    )
}

impl Ring {
    /// The ring `EARS_RING_DIR` and `EARS_RING_MAX_MB` name, as `var` gives them (the environment's), owned by this
    /// process's user: [`dir_from`], [`max_bytes_from`], [`Ring::open`].
    ///
    /// # Errors
    ///
    /// A bad bound, or a directory [`Ring::open`] refuses, named.
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let uid = nix::unistd::getuid().as_raw();
        let dir = dir_from(var("EARS_RING_DIR").as_deref(), uid);
        let max = max_bytes_from(var("EARS_RING_MAX_MB").as_deref())?;
        Self::open(&dir, max, uid).map_err(|e| format!("ring {}: {e}", dir.display()))
    }

    /// The ring at `dir`, bounded to `max_bytes`, owned by `uid` (this process's): made when missing, and checked.
    ///
    /// # Errors
    ///
    /// The directory cannot be made, or is not a plain directory `uid` owns.
    pub fn open(dir: &Path, max_bytes: u64, uid: u32) -> io::Result<Self> {
        let ring = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            uid,
            lock: Mutex::new(()),
            prunes: AtomicBool::new(true),
        };
        ring.ready()?;
        // Sticky by path and needs no root; absent off macOS, and a failure only loses the insurance (Time Machine
        // already skips /tmp), so its outcome is not checked.
        let _excluded = std::process::Command::new("/usr/bin/tmutil")
            .arg("addexclusion")
            .arg(&ring.dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        Ok(ring)
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the directory is a plain one this user owns (made `0700` when missing; made `0700` again).
    fn ready(&self) -> io::Result<()> {
        if let Err(e) = fs::symlink_metadata(&self.dir) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
            fs::DirBuilder::new().mode(0o700).create(&self.dir)?;
        }
        let meta = fs::symlink_metadata(&self.dir)?;
        if !meta.is_dir() || meta.uid() != self.uid {
            return Err(io::Error::other(format!(
                "{} is not a plain directory of ours",
                self.dir.display()
            )));
        }
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
    }

    /// Parks one consumed piece — its WAV, its segmenter JSON (`anchor_ms`: the stream's first sample, epoch ms)
    /// and its transcript — then drops the oldest files past the bound. A ring that has become unusable, or a
    /// write that fails, drops the piece, said on stderr by side and stem, never by content.
    pub fn keep(&self, side: Side, stem: &str, u: &Utterance, anchor_ms: u64, t: &Transcript) {
        let _one = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let base = format!("{}-{stem}", side.as_str());
        let kept = self.ready().and_then(|()| {
            put(
                &self.dir,
                &format!("{base}.json"),
                u.json_line(anchor_ms).as_bytes(),
            )?;
            put(
                &self.dir,
                &format!("{base}.whisper.json"),
                t.json().as_bytes(),
            )?;
            put(&self.dir, &format!("{base}.wav"), &wav(&u.samples))
        });
        if let Err(e) = kept {
            crate::warn(&format!(
                "tulpa-hear: ring: dropping the piece ({} {stem}): {e}",
                side.as_str()
            ));
            return;
        }
        if self.prunes.load(Ordering::Relaxed)
            && let Err(e) = self.bound()
        {
            crate::warn(&format!("tulpa-hear: ring: bound not kept: {e}"));
        }
    }

    /// From now on pieces are only added, never deleted to keep the bound: for a run that has been told to stop.
    /// The capture that comes next shares the ring and keeps its bound; after the stop that is its job alone.
    pub fn append_only(&self) {
        self.prunes.store(false, Ordering::Relaxed);
    }

    /// Removes the oldest files (by mtime) until the directory's files fit in the bound.
    fn bound(&self) -> io::Result<()> {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file()
                    .then(|| Some((meta.modified().ok()?, meta.len(), e.path())))?
            })
            .collect();
        files.sort();
        prune(files, self.max_bytes, |path| fs::remove_file(path))
    }
}

/// Removes `files` (oldest first, each with its length) with `remove` until what is left fits in `max_bytes`.
///
/// A file that is gone already (`NotFound`) is counted as removed, and the pruning goes on: another writer of the
/// same ring (the tray's next capture, started before this one has ended) may have deleted it first. Any other
/// failure stops the pruning with that error.
fn prune(
    files: Vec<(std::time::SystemTime, u64, PathBuf)>,
    max_bytes: u64,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        match remove(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        total = total.saturating_sub(len);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::recognize::Token;

    fn uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tulpa-ring-{tag}-{}", std::process::id()));
        let _gone = fs::remove_dir_all(&d);
        d
    }

    fn utt(samples: usize) -> io::Result<Utterance> {
        let mut seg = tulpa_segment::Segmenter::new(tulpa_segment::Config::default());
        // A closed utterance needs speech then silence; a tone then quiet is enough for the cutter.
        let tone: Vec<i16> = (0..32_000)
            .map(|i| if (i / 20) % 2 == 0 { 8000 } else { -8000 })
            .collect();
        let mut got = seg.push(&vec![0; 16_000]);
        got.extend(seg.push(&tone));
        got.extend(seg.push(&vec![0; samples]));
        got.extend(seg.finish());
        got.into_iter()
            .next()
            .ok_or_else(|| io::Error::other("no utterance"))
    }

    #[test]
    fn the_bound_follows_the_scripts_rule() {
        assert_eq!(max_bytes_from(None), Ok(8 * 1024 * 1024));
        assert_eq!(max_bytes_from(Some("")), Ok(8 * 1024 * 1024));
        assert_eq!(max_bytes_from(Some("1")), Ok(1024 * 1024));
        assert_eq!(max_bytes_from(Some("1024")), Ok(1024 * 1024 * 1024));
        for bad in ["0", "08", "1025", "-1", "1.5", " 8", "x"] {
            assert!(max_bytes_from(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            dir_from(None, 501),
            PathBuf::from("/tmp/tulpa-ears-ring-501")
        );
        assert_eq!(
            dir_from(Some(""), 7),
            PathBuf::from("/tmp/tulpa-ears-ring-7")
        );
        assert_eq!(dir_from(Some("/x/r"), 7), PathBuf::from("/x/r"));
    }

    #[test]
    fn a_piece_parks_three_files_0600_in_a_0700_dir() -> io::Result<()> {
        let dir = tmp("keep");
        let ring = Ring::open(&dir, 8 * 1024 * 1024, uid())?;
        let u = utt(32_000)?;
        let t =
            Transcript::new(vec![Token::new(" \"да\"\t", 10, 250, 0.5)], false).with_specials(3);
        ring.keep(Side::Me, "c000007", &u, 1_000_000, &t);
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
        assert_eq!(mode(&dir), Some(0o700));
        for f in [
            "me-c000007.wav",
            "me-c000007.json",
            "me-c000007.whisper.json",
        ] {
            assert_eq!(mode(&dir.join(f)), Some(0o600), "{f}");
        }
        assert_eq!(
            fs::read(dir.join("me-c000007.wav")).ok(),
            Some(wav(&u.samples))
        );
        assert_eq!(
            fs::read_to_string(dir.join("me-c000007.json")).ok(),
            Some(u.json_line(1_000_000))
        );
        assert_eq!(
            fs::read_to_string(dir.join("me-c000007.whisper.json")).ok(),
            Some(t.json())
        );
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn the_oldest_go_past_the_bound() -> io::Result<()> {
        let dir = tmp("bound");
        let u = utt(32_000)?;
        let one = u64::try_from(wav(&u.samples).len()).unwrap_or(0);
        // Room for about two pieces.
        let ring = Ring::open(&dir, one * 2 + 2048, uid())?;
        let t = Transcript::default();
        for n in 1..=4 {
            ring.keep(Side::Them, &format!("c{n:06}"), &u, 0, &t);
            // mtimes must differ for "oldest" to mean something.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut left: Vec<String> = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| Path::new(n).extension().is_some_and(|x| x == "wav"))
            .collect();
        left.sort();
        assert_eq!(left, ["them-c000003.wav", "them-c000004.wav"]);
        let total: u64 = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert!(total <= one * 2 + 2048, "{total}");
        fs::remove_dir_all(&dir)
    }

    #[test]
    fn a_symlink_or_a_foreign_dir_is_refused() -> io::Result<()> {
        let dir = tmp("link");
        let target = tmp("target");
        fs::create_dir_all(&target)?;
        std::os::unix::fs::symlink(&target, &dir)?;
        assert!(Ring::open(&dir, 1024, uid()).is_err());
        fs::remove_file(&dir)?;
        // A directory that is not ours: the same check, with another uid.
        fs::create_dir_all(&dir)?;
        assert!(Ring::open(&dir, 1024, uid().wrapping_add(1)).is_err());
        // Made unusable mid-run: the piece is dropped, nothing written through the link.
        let ring = Ring::open(&dir, 1024 * 1024, uid())?;
        fs::remove_dir_all(&dir)?;
        std::os::unix::fs::symlink(&target, &dir)?;
        ring.keep(
            Side::Me,
            "c000001",
            &utt(16_000)?,
            0,
            &Transcript::default(),
        );
        assert_eq!(fs::read_dir(&target).map(Iterator::count).ok(), Some(0));
        fs::remove_file(&dir)?;
        fs::remove_dir_all(&target)
    }

    /// A prune that meets a file already gone goes on: two captures can share the ring for a moment, and the other
    /// one deleted it first. The next oldest is removed, and the pruning ends well.
    #[test]
    fn a_prune_that_finds_a_file_gone_goes_on() {
        use std::cell::RefCell;
        let t = std::time::SystemTime::UNIX_EPOCH;
        let files = vec![
            (t, 600, PathBuf::from("gone")),
            (t, 600, PathBuf::from("old")),
            (t, 600, PathBuf::from("new")),
        ];
        let removed = RefCell::new(Vec::new());
        let done = prune(files, 700, |path| {
            if path == Path::new("gone") {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            removed.borrow_mut().push(path.to_path_buf());
            Ok(())
        });
        assert!(done.is_ok(), "{done:?}");
        assert_eq!(removed.into_inner(), [PathBuf::from("old")]);
    }
}
