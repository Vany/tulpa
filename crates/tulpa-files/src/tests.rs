//! The inbox and the delivery against a real filesystem and the real tar.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{Delivery, Inbox, Taken, name_ok};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tulpa-files-test-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    ));
    let _made: std::io::Result<()> = fs::create_dir_all(&dir);
    dir
}

/// Makes a tiny but complete Tulpa.app in `dir` and tars it beside it.
fn bundle_archive(dir: &Path, commit: &str) -> Result<PathBuf> {
    let app = dir.join("Tulpa.app");
    fs::create_dir_all(app.join("Contents/MacOS"))?;
    fs::create_dir_all(app.join("Contents/Resources"))?;
    let exe = app.join("Contents/MacOS/tulpa");
    fs::write(&exe, "#!/bin/sh\nsleep 1\n")?;
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
    fs::write(
        app.join("Contents/Resources/build"),
        format!("version=1.0.0\ncommit={commit}\nutc=2026-01-01T00:00:00Z\n"),
    )?;
    let archive = dir.join(format!("Tulpa-{}.tgz", &commit[..7.min(commit.len())]));
    let made = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir)
        .arg("Tulpa.app")
        .output()?;
    assert!(made.status.success(), "the test bundle archive was made");
    fs::remove_dir_all(&app)?;
    Ok(archive)
}

/// An archive at `path` of exactly the tree `top` under `dir`.
fn archive_of(path: &Path, dir: &Path, top: &str) -> Result<()> {
    let made = std::process::Command::new("tar")
        .arg("-czf")
        .arg(path)
        .arg("-C")
        .arg(dir)
        .arg(top)
        .output()?;
    assert!(made.status.success(), "the test archive was made");
    Ok(())
}

fn inbox_under(dir: &Path) -> Result<Inbox> {
    let inbox = Inbox::under(&dir.join("support"));
    drop(inbox.open()?.0); // made, as the serving path makes it before any put
    Ok(inbox)
}

#[test]
fn inbox_names_are_single_plain_names() {
    let good = [
        "Tulpa-1a2b3c4d.tgz",
        "update-2026-11-19.tar.gz",
        "a",
        "A.b_c-d+e",
    ]
    .map(String::from)
    .into_iter()
    .chain([format!("x{}", "x".repeat(127))]);
    for good in good {
        assert!(name_ok(&good), "{good}");
    }
    let bad = [
        "",
        ".hidden",
        "..",
        "../out",
        "out/../in",
        "/etc/passwd",
        "sub/dir/file",
        "with space",
        "with\nnewline",
        "юникод",
    ]
    .map(String::from)
    .into_iter()
    .chain([format!("x{}", "x".repeat(128))]);
    for bad in bad {
        assert!(!name_ok(&bad), "{bad}");
    }
}

#[test]
fn inbox_opens_makes_and_refuses_a_symlink() -> Result<()> {
    let dir = temp_dir("open");
    let inbox = Inbox::under(&dir.join("support"));
    let (fd, real) = inbox.open()?;
    assert!(real.is_dir());
    drop(fd); // it opened; that is all this test needs from it

    // A symlink where the inbox should be is refused, not followed.
    let dir2 = temp_dir("symlink");
    let support = dir2.join("support");
    let inbox2 = Inbox::under(&support);
    let target = dir2.join("elsewhere");
    fs::create_dir_all(&target)?;
    fs::create_dir_all(&support)?;
    std::os::unix::fs::symlink(&target, inbox2.dir())?;
    let refused = inbox2.open().err().unwrap_or_default();
    assert!(refused.contains("inbox"), "{refused}");
    Ok(())
}

#[test]
fn a_bundle_archive_is_staged_under_its_commit_and_the_copy_removed() -> Result<()> {
    let dir = temp_dir("stage");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    let archive = bundle_archive(&dir, "1a2b3c4d5e6f7890aabbccddeeff001122334455")?;
    let landed = inbox.target("Tulpa-1a2b3c4d.tgz");
    fs::copy(&archive, &landed)?;

    let said = match delivery.take(&landed) {
        Taken::Staged(said) => said,
        other => format!("{other:?}"),
    };
    assert!(said.contains("Tulpa.app-1a2b3c4d5e6f"), "{said}");
    let staged = delivery
        .staging()
        .join("Tulpa.app-1a2b3c4d5e6f7890aabbccddeeff001122334455");
    assert!(staged.join("Contents/MacOS/tulpa").is_file());
    assert!(
        fs::read_to_string(staged.join("Contents/Resources/build"))?
            .contains("commit=1a2b3c4d5e6f")
    );
    assert!(!landed.exists(), "the inbox copy is removed once staged");

    // Again, from a fresh copy: already staged, still consumed.
    fs::copy(&archive, &landed)?;
    let again = match delivery.take(&landed) {
        Taken::Staged(said) => said,
        other => format!("{other:?}"),
    };
    assert!(again.contains("already staged"), "{again}");
    assert!(!landed.exists());
    Ok(())
}

#[test]
fn other_files_are_kept_in_the_inbox() -> Result<()> {
    let dir = temp_dir("keep");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    let landed = inbox.target("notes.txt");
    fs::write(&landed, "hello")?;
    assert!(matches!(delivery.take(&landed), Taken::Kept(_)));
    assert_eq!(fs::read_to_string(&landed)?, "hello");
    Ok(())
}

#[test]
fn a_broken_archive_fails_and_is_kept() -> Result<()> {
    let dir = temp_dir("broken");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    let landed = inbox.target("Tulpa-broken.tgz");
    fs::write(&landed, "not a tar at all")?;
    let why = match delivery.take(&landed) {
        Taken::Failed(why) => why,
        other => format!("{other:?}"),
    };
    assert!(why.contains("Tulpa-broken.tgz"), "{why}");
    assert!(landed.exists(), "a failed take keeps the file");
    assert_eq!(fs::read_dir(delivery.staging())?.count(), 0);
    Ok(())
}

#[test]
fn an_archive_without_the_app_at_its_root_fails() -> Result<()> {
    let dir = temp_dir("wrongroot");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    let other = dir.join("SomethingElse");
    fs::create_dir_all(other.join("Contents/MacOS"))?;
    let landed = inbox.target("Tulpa-wrong.tgz");
    archive_of(&landed, &dir, "SomethingElse")?;
    let why = match delivery.take(&landed) {
        Taken::Failed(why) => why,
        other => format!("{other:?}"),
    };
    assert!(why.contains("no Tulpa.app"), "{why}");
    assert!(landed.exists());
    Ok(())
}

#[test]
fn an_archive_whose_executable_is_a_symlink_fails() -> Result<()> {
    let dir = temp_dir("symexe");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    // The real payload sits outside the app; the archive's exe is a link to it.
    fs::create_dir_all(dir.join("payload"))?;
    fs::write(dir.join("payload/real"), "x")?;
    let app = dir.join("Tulpa.app");
    fs::create_dir_all(app.join("Contents/MacOS"))?;
    fs::create_dir_all(app.join("Contents/Resources"))?;
    std::os::unix::fs::symlink("../../../payload/real", app.join("Contents/MacOS/tulpa"))?;
    fs::write(app.join("Contents/Resources/build"), "commit=abc123def\n")?;
    let landed = inbox.target("Tulpa-sym.tgz");
    archive_of(&landed, &dir, "Tulpa.app")?;
    let why = match delivery.take(&landed) {
        Taken::Failed(why) => why,
        other => format!("{other:?}"),
    };
    assert!(why.contains("regular file"), "{why}");
    assert!(landed.exists());
    Ok(())
}

/// An archive whose member tree sits under `../evil`: tar stores such names
/// at creation (the guard is extraction-side), and both GNU tar and bsdtar
/// refuse to extract them without `-P`.
fn escaping_archive(path: &Path, dir: &Path) -> Result<()> {
    let flavor = std::process::Command::new("tar")
        .arg("--version")
        .output()?;
    let gnu = String::from_utf8_lossy(&flavor.stdout).contains("GNU tar");
    let mut made = std::process::Command::new("tar");
    made.arg("-czf").arg(path).arg("-C").arg(dir);
    if gnu {
        made.arg("--transform=s,^Tulpa.app,../evil,");
    } else {
        made.arg("-s").arg("/^Tulpa.app/../evil/");
    }
    made.arg("Tulpa.app");
    let made = made.output()?;
    assert!(made.status.success(), "the escaping archive was made");
    Ok(())
}

#[test]
fn an_archive_with_an_escaping_member_fails_to_unpack() -> Result<()> {
    // akira's probe, pinned (2026-11-19): a member named `../evil` is
    // refused by the extractor itself, so a future extractor swap or flag
    // change opens loudly here, not silently beside the installer.
    let dir = temp_dir("escape");
    let inbox = inbox_under(&dir)?;
    let delivery = Delivery::under(&dir.join("support"));
    let app = dir.join("Tulpa.app");
    fs::create_dir_all(app.join("Contents/MacOS"))?;
    fs::create_dir_all(app.join("Contents/Resources"))?;
    let exe = app.join("Contents/MacOS/tulpa");
    fs::write(&exe, "#!/bin/sh\nsleep 1\n")?;
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
    fs::write(
        app.join("Contents/Resources/build"),
        "version=1.0.0\ncommit=deadbeefdead\nutc=2026-01-01T00:00:00Z\n",
    )?;
    let landed = inbox.target("Tulpa-escape.tgz");
    escaping_archive(&landed, &dir)?;
    let why = match delivery.take(&landed) {
        Taken::Failed(why) => why,
        other => format!("{other:?}"),
    };
    assert!(why.contains("Tulpa-escape.tgz"), "{why}");
    assert!(landed.exists(), "a refused unpack keeps the file");
    assert!(
        !dir.join("evil").exists(),
        "nothing escapes the unpack dir, not even beside it"
    );
    assert_eq!(fs::read_dir(delivery.staging())?.count(), 0);
    Ok(())
}

#[test]
fn leftovers_from_a_cut_unpack_are_cleared() -> Result<()> {
    let dir = temp_dir("leftovers");
    let delivery = Delivery::under(&dir.join("support"));
    let leftover = delivery.staging().join(".take.Tulpa-x.tgz.123");
    fs::create_dir_all(&leftover)?;
    fs::write(leftover.join("half"), "of an unpack")?;
    let keep = delivery.staging().join("Tulpa.app-old");
    fs::create_dir_all(&keep)?;
    delivery.clear_leftovers()?;
    assert!(!leftover.exists());
    assert!(keep.exists(), "only this organ's unpacking dirs go");
    Ok(())
}
