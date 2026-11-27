#!/usr/bin/env python3
"""Exercise the login build check against a stubbed origin, staging and update.

Run from the repository root. The check script, its marker, the installed app
and the staged bundles all live in a temporary HOME with a fake `git`,
`launchctl` and `update.sh` on PATH; nothing outside the temporary directory is
read, written or run.
"""

import os
import pathlib
import shutil
import subprocess
import tempfile
import textwrap
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHECK = ROOT / "packaging/macos/build-check.sh"

LATEST = "9f8e7d6c5b4a" + "0" * 28  # a full 40-hex sha, as ls-remote prints
OLD = "1a2b3c4d5e6f" + "0" * 28


class Rig:
    def __init__(self, home: pathlib.Path):
        self.home = home
        self.bin = home / "bin"
        self.bin.mkdir(parents=True)
        self.stubs = {
            "git": textwrap.dedent(
                """\
                #!/bin/sh
                [ "$1" = ls-remote ] || exit 1
                [ -n "${TEST_LS_RC:-}" ] && exit "$TEST_LS_RC"
                cat "$TEST_LS_OUT"
                exit 0
                """
            ),
            "launchctl": "#!/bin/sh\nexit 0\n",
        }
        for name, text in self.stubs.items():
            stub = self.bin / name
            stub.write_text(text)
            stub.chmod(0o755)
        self.ls_out = home / "ls.out"
        self.update_calls = home / "update.calls"
        self.set_ls(f"{LATEST}\trefs/tags/latest-good\n")
        self.app = home / "Applications/Tulpa.app"
        self.staging = home / "Library/Application Support/tulpa/staging"
        self.staging.mkdir(parents=True)
        self.marker = home / "Library/Application Support/tulpa/build-check"

    def set_ls(self, text: str) -> None:
        self.ls_out.write_text(text)

    def bundle(self, path: pathlib.Path, commit: str | None) -> None:
        macos = path / "Contents/MacOS"
        macos.mkdir(parents=True, exist_ok=True)
        (macos / "tulpa").write_text("#!/bin/sh\n")
        (macos / "tulpa").chmod(0o755)
        if commit is not None:
            resources = path / "Contents/Resources"
            resources.mkdir(parents=True, exist_ok=True)
            (resources / "build").write_text(
                f"version=0.1.0\ncommit={commit}\nutc=2026-11-18T17:00:00Z\n"
            )

    def update_sh(self, rc: int = 0) -> pathlib.Path:
        path = self.staging / "update.sh"
        path.write_text(
            "#!/bin/sh\n"
            f'printf "%s\\n" "$*" >>"{self.update_calls}"\n'
            f'exit {rc}\n'
        )
        path.chmod(0o755)
        return path

    def env(self, **extra: str) -> dict:
        env = os.environ.copy()
        env["HOME"] = str(self.home)
        env["PATH"] = f"{self.bin}:{env['PATH']}"
        env["TEST_LS_OUT"] = str(self.ls_out)
        env.update(extra)
        return env

    def run(self, *args: str, **extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["/bin/sh", str(CHECK), *args],
            env=self.env(**extra),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=60,
        )

    def outcome(self) -> dict[str, str]:
        lines = self.marker.read_text().splitlines()
        return dict(line.split("=", 1) for line in lines)


def check(
    rig: Rig,
    result: subprocess.CompletedProcess,
    outcome: str,
    commit: str,
    latest: str,
    what: str,
    frm: str | None = None,
) -> None:
    """Asserts the run exited 0 and the marker says exactly this outcome;
    `frm` is the `from=` line an install writes, None where there is none."""
    if result.returncode != 0:
        raise RuntimeError(f"{what}: exit {result.returncode}: {result.stderr.decode(errors='replace')}")
    got = rig.outcome()
    for key, value in (("outcome", outcome), ("commit", commit), ("latest", latest), ("from", frm)):
        if got.get(key) != value:
            raise RuntimeError(f"{what}: {key}={got.get(key)!r}, want {value!r} (marker {got})")


def main() -> None:
    root = pathlib.Path(tempfile.mkdtemp(prefix="tulpa-build-check-"))
    try:
        subprocess.run(["/bin/sh", "-n", str(CHECK)], check=True)

        rig = Rig(root / "home")
        rig.bundle(rig.app, LATEST)

        # ok: the installed marker equals latest-good; update.sh is not run.
        rig.set_ls(f"{LATEST}\trefs/tags/latest-good\n")
        rig.update_sh()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "ok", LATEST, LATEST, "ok")
        if rig.update_calls.exists():
            raise RuntimeError("ok ran update.sh")
        # a check that finishes leaves no lock behind.
        lock = rig.home / "Library/Application Support/tulpa/build-check.lock"
        if lock.exists():
            raise RuntimeError("a finished check left its lock behind")

        # the run lock: a second run with a live holder exits 0 and leaves the
        # outcome marker alone — the running check's outcome is the answer.
        lock.mkdir()
        (lock / "holder").write_text(f"{os.getpid()} {int(time.time())}\n")
        before = rig.marker.read_text()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        if result.returncode != 0:
            raise RuntimeError(f"locked-out run: exit {result.returncode}: {result.stderr.decode(errors='replace')}")
        if rig.marker.read_text() != before:
            raise RuntimeError("a second run overwrote a live check's outcome")
        if not lock.exists():
            raise RuntimeError("a locked-out run removed the live holder's lock")

        # a stale lock (a live holder from long ago — the pid likely recycled)
        # is taken over: the run answers, and releases.
        (lock / "holder").write_text(f"{os.getpid()} 1\n")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "ok", LATEST, LATEST, "stale-lock-takeover")
        if lock.exists():
            raise RuntimeError("a takeover run left the stale lock behind")

        # a lock whose holder is gone (dead pid, fresh timestamp) is taken over.
        gone = subprocess.Popen(["/bin/true"])
        gone.wait()
        lock.mkdir()
        (lock / "holder").write_text(f"{gone.pid} {int(time.time())}\n")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "ok", LATEST, LATEST, "dead-holder-takeover")
        if lock.exists():
            raise RuntimeError("a takeover run left the dead holder's lock behind")

        # a lock with no holder yet (a run between its mkdir and its first
        # write) gets the recheck window: a holder that appears in it is
        # respected, not taken over.
        lock.mkdir()
        before = rig.marker.read_text()
        appearing = threading.Timer(
            0.3,
            lambda: (lock / "holder").write_text(f"{os.getpid()} {int(time.time())}\n"),
        )
        appearing.start()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git", TULPA_LOCK_RECHECK="1")
        appearing.join()
        if result.returncode != 0:
            raise RuntimeError(f"recheck run: exit {result.returncode}: {result.stderr.decode(errors='replace')}")
        if rig.marker.read_text() != before:
            raise RuntimeError("a takeover during the recheck window overwrote the outcome")
        if not lock.exists():
            raise RuntimeError("the recheck run removed the appeared holder's lock")

        # a lock that stays holderless through the window is genuinely dead:
        # takeover, answer, release.
        shutil.rmtree(lock)
        lock.mkdir()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git", TULPA_LOCK_RECHECK="1")
        check(rig, result, "ok", LATEST, LATEST, "holderless-takeover")
        if lock.exists():
            raise RuntimeError("a takeover run left the holderless lock behind")

        # a holder that stays unparsable through the window is dead the same
        # way: takeover, answer, release.
        lock.mkdir()
        (lock / "holder").write_text("not a holder\n")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git", TULPA_LOCK_RECHECK="1")
        check(rig, result, "ok", LATEST, LATEST, "junk-holder-takeover")
        if lock.exists():
            raise RuntimeError("a takeover run left the junk lock behind")

        # a real overlap (the --install / RunAtLoad double-fire): while one
        # check is inside update.sh, a second exits 0 without running it, and
        # the marker ends up with the first run's outcome alone.
        rig.bundle(rig.app, OLD)
        overlap = rig.staging / "Tulpa.app.overlap"
        rig.bundle(overlap, LATEST)
        os.utime(overlap)
        slow = rig.staging / "update.sh"
        slow.write_text(
            "#!/bin/sh\n"
            f'printf "%s\\n" "$*" >>"{rig.update_calls}"\n'
            "sleep 2\n"
            "exit 0\n"
        )
        slow.chmod(0o755)
        rig.update_calls.unlink(missing_ok=True)
        first = subprocess.Popen(
            ["/bin/sh", str(CHECK)],
            env=rig.env(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        waited = time.time() + 20
        while not rig.update_calls.exists() and time.time() < waited:
            time.sleep(0.02)
        if not rig.update_calls.exists():
            first.kill()
            raise RuntimeError("the first check never reached update.sh")
        second = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        if second.returncode != 0:
            first.kill()
            raise RuntimeError(f"the overlapping run: exit {second.returncode}")
        first_out = first.communicate(timeout=30)
        if first.returncode != 0:
            raise RuntimeError(f"the first check: exit {first.returncode}: {first_out[1].decode(errors='replace')}")
        calls = rig.update_calls.read_text().splitlines()
        if calls != [str(overlap)]:
            raise RuntimeError(f"update.sh ran {len(calls)} time(s): {calls}")
        if rig.outcome()["outcome"] != "reverted":
            raise RuntimeError(f"overlap outcome: {rig.outcome()}")
        shutil.rmtree(overlap)
        rig.update_calls.unlink()
        rig.bundle(rig.app, LATEST)
        if lock.exists():
            raise RuntimeError("the first check left its lock behind")

        # an annotated tag: the peeled line is preferred.
        rig.bundle(rig.app, OLD)
        rig.set_ls(
            f"{'a' * 40}\trefs/tags/latest-good\n"
            f"{OLD}\trefs/tags/latest-good^{{}}\n"
        )
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "ok", OLD, OLD, "annotated")

        # mismatch: the newest staged bundle with latest-good goes through update.sh.
        rig.set_ls(f"{LATEST}\trefs/tags/latest-good\n")
        rig.bundle(rig.staging / "Tulpa.app.old", "0" * 40)
        rig.bundle(rig.staging / "Tulpa.app.new", LATEST)
        # newest by mtime: touch after creation
        os.utime(rig.staging / "Tulpa.app.new")
        rig.update_sh()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        # the stub update.sh changes nothing, so the re-read keeps the old commit
        check(rig, result, "updated", OLD, LATEST, "updated", OLD)
        calls = rig.update_calls.read_text().splitlines()
        if calls != [str(rig.staging / "Tulpa.app.new")]:
            raise RuntimeError(f"update.sh saw {calls}")

        # update.sh failing (its rollback job restores on its own): failed-install.
        rig.bundle(rig.app, OLD)
        rig.update_sh(rc=2)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "failed-install", OLD, LATEST, "failed-install")

        # update.sh rc 3 (installed, plist wants a look) is still updated; OLD
        # is what the failed-install marker saw running, so not a revert.
        rig.update_sh(rc=3)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", OLD, LATEST, "updated-3", OLD)

        # no staged bundle with latest-good: failed-staging, update.sh not run.
        rig.staging.joinpath("Tulpa.app.new").rename(rig.staging / "kept")
        rig.update_sh()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "failed-staging", OLD, LATEST, "failed-staging")

        # no origin: failed-origin; a failed lookup: failed-ls-remote.
        result = rig.run()
        check(rig, result, "failed-origin", OLD, "unknown", "failed-origin")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git", TEST_LS_RC="1")
        check(rig, result, "failed-ls-remote", OLD, "unknown", "failed-ls-remote")
        rig.set_ls("")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "failed-ls-remote", OLD, "unknown", "empty-ls")

        # a bundle without a marker (the pre-marker builds) reads unknown and updates.
        rig.set_ls(f"{LATEST}\trefs/tags/latest-good\n")
        (rig.app / "Contents/Resources/build").unlink()
        os.rename(rig.staging / "kept", rig.staging / "Tulpa.app.new")
        rig.update_sh()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", "unknown", LATEST, "no-marker-updates", "unknown")

        # --install writes the agent plist (origin baked in) and runs the first check.
        rig.bundle(rig.app, LATEST)
        plist = rig.home / "Library/LaunchAgents/com.tulpa.build-check.plist"
        result = rig.run("--install", TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        if result.returncode != 0 or not plist.is_file():
            raise RuntimeError(f"--install failed: {result.stderr.decode(errors='replace')}")
        xml = plist.read_text()
        for needle in (
            "<key>Label</key><string>com.tulpa.build-check</string>",
            "<key>RunAtLoad</key><true/>",
            "<key>StartInterval</key><integer>3600</integer>",
            "<key>TULPA_CHECK_ORIGIN</key><string>git@example.invalid:tulpa.git</string>",
            f"<string>{rig.home}/Library/Application Support/tulpa/build-check.sh</string>",
        ):
            if needle not in xml:
                raise RuntimeError(f"--install plist lacks {needle!r}:\n{xml}")
        if rig.outcome()["outcome"] != "ok":
            raise RuntimeError(f"--install's first check: {rig.outcome()}")
        installed = rig.home / "Library/Application Support/tulpa/build-check.sh"
        if not installed.is_file():
            raise RuntimeError("--install did not copy the script")

        # a dirty marker never equals latest-good; --install's ok saw LATEST
        # running, so the dirty build was installed by hand: a revert.
        rig.bundle(rig.app, LATEST + "-dirty")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "reverted", LATEST + "-dirty", LATEST, "dirty-reverts", LATEST + "-dirty")

        # reverts, on a fresh rig whose update.sh really installs the staged
        # bundle, so the marker's commit is the one now running.
        rig = Rig(root / "home-revert")
        staged = rig.staging / "Tulpa.app.good"

        def installer(rc: int = 0) -> None:
            path = rig.update_sh(rc)
            if rc in (0, 3):
                path.write_text(
                    path.read_text().replace(
                        f"exit {rc}\n",
                        f'rm -rf "{rig.app}" && cp -R "$1" "{rig.app}"\nexit {rc}\n',
                    )
                )

        installer()
        log = rig.home / "Library/Logs/tulpa/build-check.log"
        HAND = "77aa" + "0" * 36  # a build installed by hand, never latest-good
        L0 = "5c5c" + "0" * 36

        # a hand build running that the previous marker did not see: the
        # install is a revert, said loudly in the log.
        rig.bundle(staged, LATEST)
        rig.bundle(rig.app, LATEST)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "ok", LATEST, LATEST, "revert-setup")
        rig.bundle(rig.app, HAND)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "reverted", LATEST, LATEST, "reverted", HAND)
        want = f"REVERTED: {HAND} was running but is not latest-good {LATEST}; installed latest-good"
        if want not in log.read_text():
            raise RuntimeError(f"the log lacks the REVERTED line:\n{log.read_text()}")

        def at_l0() -> None:
            """An ok marker with L0 running and latest-good."""
            rig.set_ls(f"{L0}\trefs/tags/latest-good\n")
            rig.bundle(rig.app, L0)
            result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
            check(rig, result, "ok", L0, L0, "at-l0")
            rig.set_ls(f"{LATEST}\trefs/tags/latest-good\n")

        # latest-good moved forward (L0 -> LATEST) over what was running: an
        # update, not a revert.
        at_l0()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", LATEST, LATEST, "moved-updates", L0)

        # a hand build X, then latest-good moves (L0 -> LATEST): the install
        # still puts X back, and says so.
        at_l0()
        rig.bundle(rig.app, HAND)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "reverted", LATEST, LATEST, "hand-then-moved-reverts", HAND)

        # latest-good moved before its bundle was staged: failed-staging, then
        # the retry once staged runs over what the marker saw — updated.
        at_l0()
        staged.rename(rig.staging / "unstaged")
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "failed-staging", L0, LATEST, "moved-unstaged")
        (rig.staging / "unstaged").rename(staged)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", LATEST, LATEST, "staged-later-updates", L0)

        # a failed install, then a retry that works: updated, not a revert.
        at_l0()
        installer(rc=2)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "failed-install", L0, LATEST, "install-fails")
        installer()
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", LATEST, LATEST, "retry-updates", L0)

        # no previous marker: nothing to compare with, an update.
        rig.marker.unlink()
        rig.bundle(rig.app, HAND)
        result = rig.run(TULPA_CHECK_ORIGIN="git@example.invalid:tulpa.git")
        check(rig, result, "updated", LATEST, LATEST, "first-updates", HAND)
        if log.read_text().count("REVERTED:") != 2:
            raise RuntimeError(f"REVERTED said for an update, or not for a revert:\n{log.read_text()}")

        print("build-check: 24 cases passed against stubbed origin, staging and update")
    finally:
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
