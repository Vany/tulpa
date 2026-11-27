//! The files organ's two ends on the Mac: the inbox a delivery lands in, and
//! where landed files go from there.
//!
//! The inbox is one directory this organ owns outright
//! (`~/Library/Application Support/tulpa/inbox`). A file arrives whole —
//! hashed, resumed, renamed by the data stream ([`tulpa_hands::xfer`]) — and
//! only ever under a name this module accepts: a single name of
//! `[A-Za-z0-9._+-]`, no directory, no leading dot. Nothing outside the inbox
//! is written, whatever the request said; the sender side of that bargain is
//! the hub's, which is the only one that asks.
//!
//! [`Delivery`] then takes what landed: an archive a bundle update is staged
//! from (`*.tgz`, `*.tar.gz` — unpacked under the staging directory
//! `build-check.sh` installs from, verified before it is renamed into place),
//! anything else stays in the inbox for whoever asked for it. Delivery is
//! idempotent: run again over the same file, it stages nothing twice.
//!
//! The organ has no modes and no pause: it is on whenever the link is
//! (#0395). Its state is on disk — the `.part` files of a cut transfer, the
//! inbox files, the staging directory — so a restart picks up where it left
//! off.

use std::ffi::OsStr;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::{Mode, OFlags};

/// The inbox directory under the app's support directory: `inbox`.
pub const INBOX_DIR: &str = "inbox";
/// Where `build-check.sh` looks for staged bundles by default: `staging`.
pub const STAGING_DIR: &str = "staging";
/// The longest inbox name accepted (128), the length also shown in refusals.
pub const NAME_MAX: usize = 128;
/// The one top-level directory a bundle archive must hold.
const APP_NAME: &str = "Tulpa.app";
/// A marker commit (or any word the installer's `word()` accepts) is at most
/// this long; a full sha is 40 and `-dirty` makes 46.
const WORD_MAX: usize = 47;
/// Free space an unpack needs: the archive unpacked again beside it, both
/// halves, and the same 64 MiB margin the data stream asks for.
const UNPACK_SPACE_MARGIN: u64 = 67_108_864;

/// An inbox name this module accepts: one name (no `/`), 1..=[`NAME_MAX`]
/// bytes of `[A-Za-z0-9._+-]`, not starting with a dot.
///
/// The data stream's `.part` names and the deny shapes never match it by
/// accident.
#[must_use]
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// A name whose file the delivery takes as a bundle archive: exactly `.tgz`
/// or `.tar.gz` at the end (the exact case — the names are this organ's own).
fn is_bundle_name(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, "tgz")) => true,
        Some((head, "gz")) => matches!(head.rsplit_once('.'), Some((_, "tar"))),
        _ => false,
    }
}

/// The inbox: one directory, opened without following a symlink, that a
/// delivery lands in.
#[derive(Debug, Clone)]
pub struct Inbox {
    dir: PathBuf,
}

impl Inbox {
    /// The inbox at `<support>/inbox` (`support` is the app's
    /// `~/Library/Application Support/tulpa`).
    #[must_use]
    pub fn under(support: &Path) -> Self {
        Self {
            dir: support.join(INBOX_DIR),
        }
    }

    /// The inbox directory, as configured.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path a landed file would have. Only a name [`name_ok`] accepts
    /// lands there; this says where, it does not put anything there.
    #[must_use]
    pub fn target(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Opens the inbox directory, making it (and the support directory) if
    /// missing: never through a symlink, and only a directory.
    ///
    /// # Errors
    ///
    /// Refuses when the path exists and is not this organ's directory (a
    /// symlink, a file), or when it cannot be made or opened.
    pub fn open(&self) -> Result<(OwnedFd, PathBuf), String> {
        if let Some(parent) = self.dir.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("inbox: {}: {e}", parent.display()))?;
        }
        match fs::create_dir(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("inbox: {}: {e}", self.dir.display())),
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let dir = rustix::fs::open(&self.dir, flags, Mode::empty())
            .map_err(|e| format!("inbox: {}: {e}", self.dir.display()))?;
        let real = fs::canonicalize(&self.dir)
            .map_err(|e| format!("inbox: {}: {e}", self.dir.display()))?;
        Ok((dir, real))
    }
}

/// What taking one landed file ended in; the words are for the activity log.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Taken {
    /// The file reached where it belongs (the staging directory); the inbox
    /// copy is removed. Says what was staged.
    Staged(String),
    /// Not this delivery's shape; the file stays in the inbox.
    Kept(String),
    /// The file could not be taken; it stays, and the next pass tries again.
    Failed(String),
}

/// Where landed files go: bundle archives become staged bundles under the
/// staging directory `build-check.sh` installs from.
#[derive(Debug, Clone)]
pub struct Delivery {
    staging: PathBuf,
}

impl Delivery {
    /// The delivery into `<support>/staging`, the installer's own default.
    #[must_use]
    pub fn under(support: &Path) -> Self {
        Self {
            staging: support.join(STAGING_DIR),
        }
    }

    /// The staging directory, as configured.
    #[must_use]
    pub fn staging(&self) -> &Path {
        &self.staging
    }

    /// Takes one landed inbox file to where it belongs.
    ///
    /// A `*.tgz` / `*.tar.gz` is unpacked into a private directory under the
    /// staging root, checked (one `Tulpa.app`, a real executable, a marker
    /// with a commit word) and renamed to `Tulpa.app-<commit>` — a name the
    /// installer's `Tulpa.app*` glob finds. Only then is the inbox copy
    /// removed. Anything else stays; a failure keeps both the file and, until
    /// the next pass, nothing half-unpacked.
    pub fn take(&self, file: &Path) -> Taken {
        let name = match file.file_name().and_then(OsStr::to_str) {
            Some(name) if name_ok(name) => name.to_owned(),
            _ => return Taken::Failed("not an inbox name".to_owned()),
        };
        if !is_bundle_name(&name) {
            return Taken::Kept(format!(
                "{name}: not a bundle archive; it stays in the inbox"
            ));
        }
        match self.stage(&name, file) {
            Ok(said) => {
                let _removed: std::io::Result<()> = fs::remove_file(file);
                Taken::Staged(said)
            }
            Err(why) => Taken::Failed(format!("{name}: {why}")),
        }
    }

    /// Removes unpacking directories a cut run left, older work this organ's
    /// own; called once before a pass over the inbox.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the staging directory.
    pub fn clear_leftovers(&self) -> std::io::Result<()> {
        let entries = fs::read_dir(&self.staging)?;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path
                .file_name()
                .is_some_and(|n| n.as_bytes().starts_with(b".take."))
                && let Ok(meta) = fs::symlink_metadata(&path)
                && meta.is_dir()
            {
                let _gone: std::io::Result<()> = fs::remove_dir_all(&path);
            }
        }
        Ok(())
    }

    /// One verified bundle from archive to `Tulpa.app-<commit>` under staging.
    fn stage(&self, name: &str, archive: &Path) -> Result<String, String> {
        let meta = fs::symlink_metadata(archive).map_err(|e| format!("not there anymore: {e}"))?;
        if !meta.is_file() {
            return Err("not a regular file".to_owned());
        }
        fs::create_dir_all(&self.staging)
            .map_err(|e| format!("staging {}: {e}", self.staging.display()))?;
        let unpack = self
            .staging
            .join(format!(".take.{name}.{}", std::process::id()));
        let _gone = UnpackGuard(&unpack);
        check_space(&self.staging, meta.len().saturating_mul(2))?;
        fs::create_dir(&unpack).map_err(|e| format!("unpack dir: {e}"))?;
        unpack_archive(archive, &unpack)?;
        let commit = verify_app(&unpack.join(APP_NAME))?;
        let staged = self.staging.join(format!("{APP_NAME}-{commit}"));
        if fs::symlink_metadata(&staged).is_ok() {
            return Ok(format!(
                "{commit} was already staged; the archive is dropped"
            ));
        }
        fs::rename(unpack.join(APP_NAME), &staged)
            .map_err(|e| format!("rename into staging: {e}"))?;
        Ok(format!(
            "staged {APP_NAME}-{commit} from {name} for the installer"
        ))
    }
}

/// Removes the unpacking directory when it is not renamed out of it first.
struct UnpackGuard<'a>(&'a Path);

impl Drop for UnpackGuard<'_> {
    fn drop(&mut self) {
        let _gone: std::io::Result<()> = fs::remove_dir_all(self.0);
    }
}

/// Runs the system tar into `dir`: no shell, a fixed PATH, gzip inside.
fn unpack_archive(archive: &Path, dir: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/tar")
        .arg("-x")
        .arg("-z")
        .arg("-f")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .map_err(|e| format!("tar: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tar: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ))
    }
}

/// The unpacked bundle must be one app directory with a real executable and a
/// marker naming its commit; the commit is returned.
fn verify_app(app: &Path) -> Result<String, String> {
    let kind = |path: &Path| fs::symlink_metadata(path).map(|m| m.file_type());
    if !matches!(kind(app), Ok(t) if t.is_dir()) {
        return Err(format!(
            "the archive has no {APP_NAME} directory at its root"
        ));
    }
    let exe = app.join("Contents/MacOS/tulpa");
    let exe_meta = fs::symlink_metadata(&exe).map_err(|e| format!("executable: {e}"))?;
    if !exe_meta.is_file() {
        return Err("the executable is not a regular file".to_owned());
    }
    if exe_meta.permissions().mode() & 0o111 == 0 {
        return Err("the executable is not executable".to_owned());
    }
    let marker = app.join("Contents/Resources/build");
    let marker_meta = fs::symlink_metadata(&marker).map_err(|e| format!("build marker: {e}"))?;
    if !marker_meta.is_file() {
        return Err("the build marker is not a regular file".to_owned());
    }
    let text = fs::read_to_string(&marker).map_err(|e| format!("build marker: {e}"))?;
    let commit = text
        .lines()
        .find_map(|line| line.strip_prefix("commit="))
        .unwrap_or_default()
        .trim();
    if commit.is_empty()
        || commit.len() > WORD_MAX
        || !commit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return Err("the build marker names no commit".to_owned());
    }
    Ok(commit.to_owned())
}

/// The staging root needs `want` bytes and the margin free.
fn check_space(dir: &Path, want: u64) -> Result<(), String> {
    let Ok(stat) = rustix::fs::statvfs(dir) else {
        return Ok(());
    };
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    if free >= want.saturating_add(UNPACK_SPACE_MARGIN) {
        Ok(())
    } else {
        Err(format!(
            "no space: {want} bytes and a margin needed, {free} free"
        ))
    }
}

#[cfg(test)]
mod tests;
