//! The camera's still photos, kept private on the Mac until the agent fetches one.
//!
//! [`PhotoStore::save`] keeps one JPEG as a new file in the store's directory.
//! The directory is made `0700` (an existing one is brought to `0700`) and
//! opened without following a symlink as its last component. The file is
//! created under that descriptor with `O_CREAT | O_EXCL | O_NOFOLLOW`, set to
//! `0600`, written whole and synced, and named by the UTC second it was
//! taken: `photo-YYYYMMDDTHHMMSSZ.jpg`, then `-2`, `-3` … up to `-99` for
//! more photos in the same second. A name that is already taken, even by a
//! symlink, is never opened. A failed write removes its file. The path comes
//! back with the size and the sha256 of the bytes, so the agent can check
//! what it fetched.
//!
//! [`is_photo_name`] is true for exactly the names the store makes; the data
//! stream hands over a file from this directory only when its name has that
//! shape.
//!
//! A photo that is never fetched does not stay: [`PhotoStore::sweep`] removes
//! the ones untouched for [`PHOTO_MAX_AGE`] (24 h, the data stream's rule for
//! its `.part` files), and [`PhotoStore::waiting`] counts the ones still
//! there. Both look only at regular files with a photo's name directly in the
//! directory, and follow no symlink, neither the directory nor a file.

use std::ffi::CStr;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

/// The largest photo kept, 32 MiB; a camera still is a few MB at most.
pub const PHOTO_MAX_BYTES: usize = 32 * 1024 * 1024;
/// A photo untouched this long (24 h) is swept, fetched or not: the data
/// stream's rule for a `.part`.
pub const PHOTO_MAX_AGE: Duration = Duration::from_secs(86_400);
/// The highest count a name can carry for one second (`-2` … `-99`).
const SAME_SECOND_MAX: u32 = 99;
/// Every photo name starts so.
const PREFIX: &str = "photo-";
/// Every photo name ends so.
const SUFFIX: &str = ".jpg";
/// The start-of-image marker a JPEG file opens with.
const JPEG_START: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// One photo the store kept.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Photo {
    /// Where it is: the store's directory and the photo's name.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// The sha256 of its bytes, 64 lowercase hex digits.
    pub sha256: String,
}

/// Why a photo was not kept. Nothing is left behind in any case.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PhotoError {
    /// The bytes are empty or do not start like a JPEG.
    NotJpeg,
    /// More bytes than [`PHOTO_MAX_BYTES`].
    TooBig(usize),
    /// The time stamp is not `YYYYMMDDTHHMMSSZ`.
    Stamp,
    /// The directory could not be made, opened or made private.
    Dir(String),
    /// The file could not be made, written or synced.
    Write(String),
    /// Every name for this second is taken.
    NamesTaken,
}

impl fmt::Display for PhotoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJpeg => write!(formatter, "not a JPEG"),
            Self::TooBig(len) => write!(
                formatter,
                "{len} bytes, over the {PHOTO_MAX_BYTES}-byte cap"
            ),
            Self::Stamp => write!(formatter, "the time stamp is not YYYYMMDDTHHMMSSZ"),
            Self::Dir(why) => write!(formatter, "photo directory: {why}"),
            Self::Write(why) => write!(formatter, "photo file: {why}"),
            Self::NamesTaken => write!(formatter, "every name for this second is taken"),
        }
    }
}

impl std::error::Error for PhotoError {}

/// The private directory photos are kept in.
#[derive(Debug, Clone)]
pub struct PhotoStore {
    dir: PathBuf,
}

impl PhotoStore {
    /// A store in `dir`, an absolute path; nothing is made before the first photo.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory the photos go to.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keeps `jpeg` as a new photo taken at `stamp`, the UTC time as
    /// `YYYYMMDDTHHMMSSZ`.
    ///
    /// # Errors
    ///
    /// Returns why nothing was kept: the bytes are not a JPEG or too many, the
    /// stamp is malformed, the directory is unusable (missing parents that
    /// cannot be made, or a symlink as its last component), the file cannot
    /// be written, or every name for the second is taken.
    pub fn save(&self, jpeg: &[u8], stamp: &str) -> Result<Photo, PhotoError> {
        if jpeg.len() > PHOTO_MAX_BYTES {
            return Err(PhotoError::TooBig(jpeg.len()));
        }
        if !looks_like_jpeg(jpeg) {
            return Err(PhotoError::NotJpeg);
        }
        if !is_stamp(stamp) {
            return Err(PhotoError::Stamp);
        }
        let dir = self.open_dir()?;
        for count in 1..=SAME_SECOND_MAX {
            let name = name_for(stamp, count);
            let created = rustix::fs::openat(
                &dir,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            );
            match created {
                Ok(file) => {
                    write_whole(&dir, &name, file, jpeg)?;
                    return Ok(Photo {
                        path: self.dir.join(&name),
                        size: u64::try_from(jpeg.len()).unwrap_or(u64::MAX),
                        sha256: hex(&Sha256::digest(jpeg)),
                    });
                }
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(PhotoError::Write(format!("{name}: {e}"))),
            }
        }
        Err(PhotoError::NamesTaken)
    }

    /// How many photos wait in the store: regular files with a photo's name
    /// directly in its directory. 0 without a directory.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.each_photo(|_, _, _| true)
    }

    /// Removes the photos untouched for [`PHOTO_MAX_AGE`] at `now`; the count
    /// removed. Only regular files with a photo's name, directly in the
    /// directory; neither the directory nor a file is reached through a
    /// symlink, and a file with a time after `now` stays.
    #[must_use]
    pub fn sweep(&self, now: SystemTime) -> usize {
        self.each_photo(|dir, name, meta| {
            let stale = meta
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > PHOTO_MAX_AGE);
            stale && rustix::fs::unlinkat(dir, name, AtFlags::empty()).is_ok()
        })
    }

    /// Calls `each` for every photo in the store (a regular file with a
    /// photo's name directly in the directory, opened with `O_NOFOLLOW` under
    /// the directory, itself opened with `O_NOFOLLOW`); the count of `true`.
    fn each_photo(&self, mut each: impl FnMut(&OwnedFd, &CStr, &Metadata) -> bool) -> usize {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let Ok(dir) = rustix::fs::open(&self.dir, flags, Mode::empty()) else {
            return 0;
        };
        let Ok(entries) = Dir::read_from(&dir) else {
            return 0;
        };
        let mut counted: usize = 0;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if !name.to_str().is_ok_and(is_photo_name) {
                continue;
            }
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let Ok(file) = rustix::fs::openat(&dir, name, flags, Mode::empty()) else {
                continue;
            };
            let Ok(meta) = File::from(file).metadata() else {
                continue;
            };
            if meta.is_file() && each(&dir, name, &meta) {
                counted = counted.saturating_add(1);
            }
        }
        counted
    }

    /// Makes the directory (`0700`; missing parents as the umask says), opens
    /// it without following a symlink there, and makes it `0700`.
    fn open_dir(&self) -> Result<OwnedFd, PhotoError> {
        if !self.dir.is_absolute() {
            return Err(PhotoError::Dir("not an absolute path".to_owned()));
        }
        let shown = self.dir.display();
        if let Some(parent) = self.dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PhotoError::Dir(format!("{}: {e}", parent.display())))?;
        }
        match rustix::fs::mkdir(&self.dir, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(PhotoError::Dir(format!("{shown}: {e}"))),
        }
        let dir = rustix::fs::open(
            &self.dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| PhotoError::Dir(format!("{shown}: {e}")))?;
        let stat = rustix::fs::fstat(&dir).map_err(|e| PhotoError::Dir(format!("{shown}: {e}")))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(PhotoError::Dir(format!("{shown}: not a directory")));
        }
        if Mode::from_raw_mode(stat.st_mode) != Mode::RWXU {
            rustix::fs::fchmod(&dir, Mode::RWXU)
                .map_err(|e| PhotoError::Dir(format!("{shown}: {e}")))?;
        }
        Ok(dir)
    }
}

/// Sets the new file to `0600`, writes `jpeg` and syncs it; on any failure the
/// file is removed again.
fn write_whole(dir: &OwnedFd, name: &str, file: OwnedFd, jpeg: &[u8]) -> Result<(), PhotoError> {
    // The umask may have taken bits off: the mode is set, not left to it.
    let written = rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
        .map_err(std::io::Error::from)
        .and_then(|()| {
            let mut file = File::from(file);
            file.write_all(jpeg)?;
            file.sync_all()
        });
    if let Err(e) = written {
        let _removed: rustix::io::Result<()> = rustix::fs::unlinkat(dir, name, AtFlags::empty());
        return Err(PhotoError::Write(format!("{name}: {e}")));
    }
    // The new name survives a crash too; a directory that cannot sync loses nothing written.
    let _synced: rustix::io::Result<()> = rustix::fs::fsync(dir);
    Ok(())
}

/// True for exactly the names [`PhotoStore::save`] makes:
/// `photo-YYYYMMDDTHHMMSSZ.jpg`, or with `-2` … `-99` before `.jpg`.
#[must_use]
pub fn is_photo_name(name: &str) -> bool {
    let Some(core) = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
    else {
        return false;
    };
    match core.split_once('-') {
        None => is_stamp(core),
        Some((stamp, count)) => {
            is_stamp(stamp)
                && !count.starts_with('0')
                && count.bytes().all(|b| b.is_ascii_digit())
                && count
                    .parse::<u32>()
                    .is_ok_and(|n| (2..=SAME_SECOND_MAX).contains(&n))
        }
    }
}

/// True when `bytes` start like a JPEG file.
#[must_use]
pub fn looks_like_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&JPEG_START)
}

/// `YYYYMMDDTHHMMSSZ`: eight digits, `T`, six digits, `Z`.
fn is_stamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    bytes.len() == 16
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 => *byte == b'T',
            15 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

/// The `count`th name for `stamp`: the first has no count.
fn name_for(stamp: &str, count: u32) -> String {
    if count <= 1 {
        format!("{PREFIX}{stamp}{SUFFIX}")
    } else {
        format!("{PREFIX}{stamp}-{count}{SUFFIX}")
    }
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut out, byte| {
            let _wrote: fmt::Result = write!(out, "{byte:02x}");
            out
        },
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    /// A small byte string that starts like a JPEG; its sha256 was computed
    /// apart from this code (Python's hashlib).
    const FAKE_JPEG: &[u8] = b"\xFF\xD8\xFF\xE0tulpa test still\xFF\xD9";
    const FAKE_SHA256: &str = "5c82687cc60da67de591d890035368e6b37d16be5228bec6826180b5916d8eed";
    const STAMP: &str = "20260115T093000Z";

    fn mode_of(path: &Path) -> Result<u32> {
        Ok(std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777)
    }

    /// Makes `path` look last touched `ago` before now.
    fn age(path: &Path, ago: Duration) -> Result<()> {
        let then = SystemTime::now().checked_sub(ago).ok_or("clock")?;
        File::open(path)?.set_modified(then)?;
        Ok(())
    }

    const TWO_DAYS: Duration = Duration::from_secs(2 * 86_400);

    #[test]
    fn a_photo_is_kept_private_and_named_by_its_second() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("support").join("photos");
        let store = PhotoStore::new(dir.clone());
        let photo = store.save(FAKE_JPEG, STAMP)?;
        assert_eq!(photo.path, dir.join("photo-20260115T093000Z.jpg"));
        assert_eq!(photo.size, 22);
        assert_eq!(photo.sha256, FAKE_SHA256);
        assert_eq!(std::fs::read(&photo.path)?, FAKE_JPEG);
        assert_eq!(mode_of(&photo.path)?, 0o600, "the photo is 0600");
        assert_eq!(mode_of(&dir)?, 0o700, "its directory is 0700");

        let second = store.save(FAKE_JPEG, STAMP)?;
        assert_eq!(second.path, dir.join("photo-20260115T093000Z-2.jpg"));
        let third = store.save(FAKE_JPEG, STAMP)?;
        assert_eq!(third.path, dir.join("photo-20260115T093000Z-3.jpg"));
        for kept in [&photo, &second, &third] {
            let name = kept.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            assert!(is_photo_name(name), "{name}");
        }
        Ok(())
    }

    #[test]
    fn an_open_directory_is_made_private() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("photos");
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        PhotoStore::new(dir.clone()).save(FAKE_JPEG, STAMP)?;
        assert_eq!(mode_of(&dir)?, 0o700);
        Ok(())
    }

    #[test]
    fn a_taken_name_is_never_opened_even_as_a_link() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("photos");
        std::fs::create_dir(&dir)?;
        let elsewhere = home.path().join("elsewhere.txt");
        std::fs::write(&elsewhere, "keep me\n")?;
        std::os::unix::fs::symlink(&elsewhere, dir.join("photo-20260115T093000Z.jpg"))?;
        let photo = PhotoStore::new(dir.clone()).save(FAKE_JPEG, STAMP)?;
        assert_eq!(photo.path, dir.join("photo-20260115T093000Z-2.jpg"));
        assert_eq!(std::fs::read_to_string(&elsewhere)?, "keep me\n");
        Ok(())
    }

    #[test]
    fn a_linked_directory_is_refused() -> Result<()> {
        let home = tempfile::tempdir()?;
        let real = home.path().join("real");
        std::fs::create_dir(&real)?;
        let dir = home.path().join("photos");
        std::os::unix::fs::symlink(&real, &dir)?;
        let refused = PhotoStore::new(dir).save(FAKE_JPEG, STAMP);
        assert!(matches!(refused, Err(PhotoError::Dir(_))), "{refused:?}");
        assert_eq!(
            std::fs::read_dir(&real)?.count(),
            0,
            "nothing written through the link"
        );
        Ok(())
    }

    #[test]
    fn bad_bytes_and_stamps_leave_nothing() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("photos");
        let store = PhotoStore::new(dir.clone());
        assert_eq!(store.save(b"", STAMP), Err(PhotoError::NotJpeg));
        assert_eq!(store.save(b"\x89PNG\r\n", STAMP), Err(PhotoError::NotJpeg));
        let big = [FAKE_JPEG, &vec![0_u8; PHOTO_MAX_BYTES]].concat();
        assert_eq!(store.save(&big, STAMP), Err(PhotoError::TooBig(big.len())));
        for stamp in [
            "",
            "2026-01-15T09:30:00Z",
            "20260115T0930Z",
            "20260115X093000Z",
            "../photos/x",
        ] {
            assert_eq!(
                store.save(FAKE_JPEG, stamp),
                Err(PhotoError::Stamp),
                "{stamp}"
            );
        }
        assert!(!dir.exists(), "nothing made for a refused photo");
        let relative = PhotoStore::new(PathBuf::from("photos")).save(FAKE_JPEG, STAMP);
        assert!(matches!(relative, Err(PhotoError::Dir(_))), "{relative:?}");
        Ok(())
    }

    #[test]
    fn the_sweep_takes_only_photos_untouched_for_a_day() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("photos");
        let store = PhotoStore::new(dir.clone());
        assert_eq!(store.waiting(), 0, "no directory, nothing waits");
        assert_eq!(store.sweep(SystemTime::now()), 0);

        let old = store.save(FAKE_JPEG, "20260115T093000Z")?;
        let fresh = store.save(FAKE_JPEG, "20260115T093001Z")?;
        let minute = Duration::from_secs(60);
        age(&old.path, PHOTO_MAX_AGE.saturating_add(minute))?;
        age(&fresh.path, PHOTO_MAX_AGE.saturating_sub(minute))?;
        // Old things that are not the store's photos stay.
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "keep me\n")?;
        age(&notes, TWO_DAYS)?;
        let elsewhere = home.path().join("elsewhere.jpg");
        std::fs::write(&elsewhere, FAKE_JPEG)?;
        age(&elsewhere, TWO_DAYS)?;
        let link = dir.join("photo-20260115T093002Z.jpg");
        std::os::unix::fs::symlink(&elsewhere, &link)?;
        let folder = dir.join("photo-20260115T093003Z.jpg");
        std::fs::create_dir(&folder)?;
        age(&folder, TWO_DAYS)?;

        assert_eq!(
            store.waiting(),
            2,
            "the two photos, not the link, folder or notes"
        );
        assert_eq!(store.sweep(SystemTime::now()), 1);
        assert!(!old.path.exists(), "a day old: swept");
        assert!(fresh.path.exists(), "under a day: kept");
        assert_eq!(std::fs::read_to_string(&notes)?, "keep me\n");
        assert_eq!(
            std::fs::read(&elsewhere)?,
            FAKE_JPEG,
            "a link is never followed"
        );
        assert!(std::fs::symlink_metadata(&link).is_ok(), "nor removed");
        assert!(folder.is_dir());
        assert_eq!(store.waiting(), 1);

        assert_eq!(
            store.sweep(SystemTime::UNIX_EPOCH),
            0,
            "a photo newer than the clock stays"
        );
        let tomorrow = SystemTime::now()
            .checked_add(PHOTO_MAX_AGE)
            .ok_or("clock")?;
        assert_eq!(store.sweep(tomorrow), 1, "a day later the other goes too");
        assert_eq!(store.waiting(), 0);
        Ok(())
    }

    #[test]
    fn a_linked_store_is_neither_counted_nor_swept() -> Result<()> {
        let home = tempfile::tempdir()?;
        let real = home.path().join("real");
        std::fs::create_dir(&real)?;
        let photo = real.join("photo-20260115T093000Z.jpg");
        std::fs::write(&photo, FAKE_JPEG)?;
        age(&photo, TWO_DAYS)?;
        let dir = home.path().join("photos");
        std::os::unix::fs::symlink(&real, &dir)?;
        let store = PhotoStore::new(dir);
        assert_eq!(store.waiting(), 0);
        assert_eq!(store.sweep(SystemTime::now()), 0);
        assert!(photo.exists(), "nothing removed through the link");
        Ok(())
    }

    #[test]
    fn photo_names_have_one_shape() {
        for good in [
            "photo-20260115T093000Z.jpg",
            "photo-20260115T093000Z-2.jpg",
            "photo-20260115T093000Z-99.jpg",
        ] {
            assert!(is_photo_name(good), "{good}");
        }
        for bad in [
            "",
            "photo-.jpg",
            "photo-20260115T093000Z-1.jpg",
            "photo-20260115T093000Z-02.jpg",
            "photo-20260115T093000Z-100.jpg",
            "photo-20260115T093000Z-+2.jpg",
            "photo-20260115T093000Z.jpg.part",
            "photo-20260115T093000Z.JPG",
            "photo-2026115T093000Z.jpg",
            "photo-20260115T093000Z-2-3.jpg",
            ".photo-20260115T093000Z.jpg",
            "../photo-20260115T093000Z.jpg",
            "photo-20260115T093000Z/x.jpg",
            "id_ed25519",
        ] {
            assert!(!is_photo_name(bad), "{bad}");
        }
    }
}
