//! Opening a path the way a review asked for.
//!
//! `O_NOFOLLOW`, then the real path of the **opened descriptor** (`F_GETPATH`
//! on macOS, `/proc/self/fd` elsewhere) is checked against the roots and the
//! deny list — never a path resolved beforehand, so a symlink swapped in
//! between can't steer the read.

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use rustix::fs::{Dir, FileType, Mode, OFlags};

use crate::policy::Policy;

/// Why a path was refused; the words go back over the link.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathRefusal {
    /// Not absolute (after `~`), or not valid.
    Shape,
    /// Opening failed (missing, a symlink as the last component, no access).
    Open(String),
    /// The opened object is outside every root.
    Outside,
    /// The opened object is on the deny list.
    Denied,
    /// Not the kind of object the verb takes.
    Kind(&'static str),
    /// Not text.
    Binary,
}

impl PathRefusal {
    /// The refusal as one safe line, fit for the wire and the log.
    #[must_use]
    pub fn words(&self) -> String {
        match self {
            Self::Shape => "path: absolute or ~/ only".to_owned(),
            Self::Open(why) => format!("open: {why}"),
            Self::Outside => "path: outside the roots".to_owned(),
            Self::Denied => "path: denied".to_owned(),
            Self::Kind(kind) => format!("path: not a {kind}"),
            Self::Binary => "path: binary, not sent".to_owned(),
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn fd_path(fd: &OwnedFd) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let path = rustix::fs::getpath(fd)?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(
        path.into_bytes(),
    )))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn fd_path(fd: &OwnedFd) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Opens `requested` without following a last-component symlink and checks
/// the descriptor's real path. Returns the descriptor and that path.
///
/// # Errors
///
/// Returns a [`PathRefusal`] when the path's shape is bad, the open fails,
/// the real path is outside every root, on the deny list, or the wrong kind.
pub fn open_checked(
    policy: &Policy,
    requested: &str,
    directory: bool,
) -> Result<(OwnedFd, PathBuf), PathRefusal> {
    let path = policy.expand(requested);
    if !path.is_absolute() || requested.contains('\0') {
        return Err(PathRefusal::Shape);
    }
    // The requested spelling is checked too: a denied name never gets opened.
    if policy.denied(&path) {
        return Err(PathRefusal::Denied);
    }
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let fd = rustix::fs::open(&path, flags, Mode::empty())
        .map_err(|e| PathRefusal::Open(e.to_string()))?;
    let real = fd_path(&fd).map_err(|e| PathRefusal::Open(e.to_string()))?;
    if policy.denied(&real) {
        return Err(PathRefusal::Denied);
    }
    if !policy.inside_roots(&real) {
        return Err(PathRefusal::Outside);
    }
    let kind = rustix::fs::fstat(&fd).map(|st| FileType::from_raw_mode(st.st_mode));
    match (directory, kind) {
        (true, Ok(FileType::Directory)) | (false, Ok(FileType::RegularFile)) => Ok((fd, real)),
        (true, _) => Err(PathRefusal::Kind("directory")),
        (false, _) => Err(PathRefusal::Kind("regular file")),
    }
}

/// Reads a checked file as text, at most `cap` bytes; `true` when cut.
///
/// # Errors
///
/// Returns a [`PathRefusal`] when the open or read fails, or the content is
/// not text.
pub fn read_text(
    policy: &Policy,
    requested: &str,
    cap: usize,
) -> Result<(String, bool), PathRefusal> {
    let (fd, _real) = open_checked(policy, requested, false)?;
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    (&mut file)
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| PathRefusal::Open(e.to_string()))?;
    let cut = bytes.len() > cap;
    bytes.truncate(cap);
    if bytes.contains(&0) {
        return Err(PathRefusal::Binary);
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), cut))
}

/// Lists a checked directory: one line per entry, `d `/`f `/`l `/`? ` and the
/// name; denied names are left out, symlinks are shown, never followed.
///
/// # Errors
///
/// Returns a [`PathRefusal`] when the open or the directory read fails.
pub fn list_dir(policy: &Policy, requested: &str) -> Result<String, PathRefusal> {
    let (fd, real) = open_checked(policy, requested, true)?;
    let dir = Dir::read_from(&fd).map_err(|e| PathRefusal::Open(e.to_string()))?;
    let mut lines = Vec::new();
    for entry in dir {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." || policy.denied(&real.join(&name)) {
            continue;
        }
        let mark = match entry.file_type() {
            FileType::Directory => "d",
            FileType::RegularFile => "f",
            FileType::Symlink => "l",
            _ => "?",
        };
        lines.push(format!("{mark} {name}"));
    }
    lines.sort_unstable_by(|a, b| a.get(2..).cmp(&b.get(2..)));
    Ok(lines.join("\n"))
}

/// A checked repository directory, for git: its real path.
///
/// # Errors
///
/// Returns a [`PathRefusal`] when the path checks fail.
pub fn repo_dir(policy: &Policy, requested: &str) -> Result<PathBuf, PathRefusal> {
    open_checked(policy, requested, true).map(|(_fd, real)| real)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Result<(tempfile::TempDir, Policy), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let root = home.path().join("proj");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::create_dir_all(home.path().join(".ssh"))?;
        std::fs::write(home.path().join(".ssh/id_ed25519"), "PRIVATE")?;
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n")?;
        std::fs::write(root.join(".env"), "TOKEN=x\n")?;
        std::fs::write(home.path().join("outside.txt"), "no\n")?;
        std::os::unix::fs::symlink(home.path().join(".ssh"), root.join("keys"))?;
        std::os::unix::fs::symlink(home.path().join(".ssh/id_ed25519"), root.join("k"))?;
        std::os::unix::fs::symlink(home.path().join("outside.txt"), root.join("out"))?;
        let policy = Policy::from_toml("roots = [\"~/proj\"]", home.path(), None)?;
        Ok((home, policy))
    }

    #[test]
    fn reads_inside_and_refuses_the_acceptance_cases() -> Result<(), Box<dyn std::error::Error>> {
        let (home, policy) = setup()?;
        let ok = read_text(&policy, "~/proj/src/main.rs", 1024);
        assert_eq!(ok, Ok(("fn main() {}\n".to_owned(), false)));
        // §9: ~/.ssh/id_ed25519, a symlink from a root into ~/.ssh, <root>/.env.
        assert_eq!(
            read_text(&policy, "~/.ssh/id_ed25519", 1024),
            Err(PathRefusal::Denied)
        );
        assert!(
            read_text(&policy, "~/proj/k", 1024).is_err(),
            "last-component symlink"
        );
        assert_eq!(
            read_text(&policy, "~/proj/keys/id_ed25519", 1024),
            Err(PathRefusal::Denied)
        );
        assert_eq!(
            read_text(&policy, "~/proj/.env", 1024),
            Err(PathRefusal::Denied)
        );
        assert_eq!(
            read_text(&policy, "~/outside.txt", 1024),
            Err(PathRefusal::Outside)
        );
        assert!(read_text(&policy, "~/proj/out", 1024).is_err());
        assert_eq!(
            read_text(&policy, "proj/src/main.rs", 1024),
            Err(PathRefusal::Shape)
        );
        assert_eq!(
            read_text(&policy, "~/proj/src", 1024),
            Err(PathRefusal::Kind("regular file"))
        );
        assert!(policy.denied(&home.path().join(".ssh")));
        Ok(())
    }

    #[test]
    fn cuts_at_the_cap_and_refuses_binary() -> Result<(), Box<dyn std::error::Error>> {
        let (home, policy) = setup()?;
        std::fs::write(home.path().join("proj/big.txt"), "a".repeat(100))?;
        std::fs::write(home.path().join("proj/bin.dat"), [1_u8, 0, 2])?;
        assert_eq!(
            read_text(&policy, "~/proj/big.txt", 10),
            Ok(("a".repeat(10), true))
        );
        assert_eq!(
            read_text(&policy, "~/proj/bin.dat", 10),
            Err(PathRefusal::Binary)
        );
        Ok(())
    }

    #[test]
    fn lists_without_denied_names_and_refuses_outside() -> Result<(), Box<dyn std::error::Error>> {
        let (_home, policy) = setup()?;
        let listing = list_dir(&policy, "~/proj").map_err(|e| e.words())?;
        assert_eq!(listing, "l k\nl keys\nl out\nd src");
        assert_eq!(list_dir(&policy, "~/.ssh"), Err(PathRefusal::Denied));
        assert_eq!(list_dir(&policy, "~"), Err(PathRefusal::Outside));
        assert!(
            list_dir(&policy, "~/proj/keys").is_err(),
            "a symlinked dir is not followed"
        );
        Ok(())
    }
}
