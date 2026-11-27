# Installing and maintaining Tulpa

This page is for installing Tulpa on a Mac, updating it, and keeping it healthy. Day to day, use [USE.md](USE.md). This page exists so that nothing gets forgotten: each rule says why it exists and whether the code enforces it yet.

**Status.** This page describes the code in the commit that added it. There are no tagged releases yet. A feature that exists only on another branch is marked **not merged**. A rule the code does not enforce yet is marked **not enforced**, and the [known gaps](#known-gaps) list them all.

## Rules for a Mac you cannot reach another way

On a remote Mac, Tulpa's link may be the only way in. Treat it as the last door.

1. Keep the Mac awake: leave [`[keepawake] always`](#keep-awake) on, its default, and after every restart read the keep-awake line back (the menu, `tulpa.log`, or `keepawake` in `ears.status`).
2. Change one thing on one Mac, then run the [preflight](#preflight-after-every-change).
3. Update with the [dead-man](#update-a-mac-with-the-dead-man) only after rehearsing it on a Mac you can reach another way. Never update two Macs at once.
4. Never stop or reload Tulpa through Tulpa's own hands: no `launchctl bootout`, no `install.command`. Tulpa kills its hands' running command as it shuts down, so whatever should start it again never runs. The dead-man's `update.sh` is the one exception: a launchd job of its own finishes the work.
5. Keep the [signing identity](#keep-the-signing-identity). A new one loses the macOS grants, and only a person at the Mac can grant them again.

## Build and sign

`scripts/bundle-macos.sh` builds the release binary and the camera helper and writes a signed `Tulpa.app`. It runs on macOS only, with the Xcode command line tools (`xcode-select --install`) for the helper's `swiftc`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `TULPA_SIGN_IDENTITY` | `tulpa-dev` | The code-signing certificate's name in the login keychain. |
| `TULPA_BUNDLE_DIR` | `target/bundle` | Where `Tulpa.app` is written. |

- Create the identity once: Keychain Access → Certificate Assistant → Create a Certificate, type Code Signing, with the name above.
- The script refuses ad-hoc signing. It builds the camera helper, `packaging/macos/tulpa-camera.swift`, into `Contents/MacOS/tulpa-camera` and signs it first. Then it signs the app with the hardened runtime and the microphone and camera entitlements, and verifies both signatures (`--deep`).
- It builds with the commit (`git describe --always --dirty`) in `TULPA_COMMIT`. The app writes that commit into its [check-in marker](#update-a-mac-with-the-dead-man).
- It writes the bundle's identity into `Contents/Resources/build` — `version=`, `commit=` (the full sha from `rev-parse HEAD`, `-dirty` appended when the tree was not clean), `utc=` — before codesign, so the signature seals it. The [login build check](#the-login-build-check) reads it.
- The bundle id is `com.tulpa.app`. `Info.plist` carries the microphone, system-audio and camera usage texts. Tulpa runs as a menu-bar app with no Dock icon.

### Keep the signing identity

**Rule.** Sign every build with the same identity.

**Why.** macOS keeps the Microphone, Screen & System Audio Recording and Camera grants for the app's code identity: its bundle id and its signing certificate. A build signed with another certificate, or ad hoc, is a different app to macOS. The grants do not carry over, and a person at the Mac must grant them again in System Settings. A remote agent cannot click there.

**Check.** `codesign -d -r- ~/Applications/Tulpa.app` prints the designated requirement. It must be the same before and after an update.

**Measure through Tulpa.** Test microphone access only through Tulpa's own capture: the ears self-check, `ears.status`, and the chunks its capture writes. A tool started over SSH, such as `ffmpeg`, runs without Tulpa's grants, so its result says nothing about Tulpa.

### The camera grant

**Rule.** The camera grant is Tulpa's, and only a person at the Mac gives it.

**Why.** `eyes.photo` runs the bundle's helper, `Contents/MacOS/tulpa-camera`, as Tulpa's own child. macOS asks on Tulpa's behalf and keeps the answer for Tulpa's code identity, as it does for the microphone. A remote agent cannot click the prompt.

- The helper is a small Swift program: AVFoundation, the default camera, one JPEG on stdout. It is signed with Tulpa's identity and entitlements, including `com.apple.security.device.camera`, which the hardened runtime needs for the camera.
- The first `eyes.photo` with the eyes on shows the macOS camera prompt for Tulpa at the Mac. The request waits for the click within the helper's 60 s. Past that it answers `camera: no photo in 60 s (the macOS camera prompt was not answered; …)`, and the helper is killed with its whole process group: `pgrep tulpa-camera` prints nothing. Send `eyes.photo` again once the person at the Mac has answered.
- **Don't Allow**, or the grant switched off later, answers `camera: denied (…)` from then on. The person at the Mac turns it on in System Settings → Privacy & Security → Camera.
- To ask from scratch, at the Mac: `tccutil reset Camera com.tulpa.app`.
- Test the camera only through Tulpa, with `eyes.photo`. A camera tool started over SSH has none of Tulpa's grants.

### Test the eyes

On any machine, `cargo test --workspace` covers the Rust side with a stand-in camera, a shell script: the photo store's modes and names, the gate (`off`, `ask`, Pause, one photo at a time), **Deny** through the menu's own question box, the helper's refusals, the 60 s limit killing the helper's whole process group, the data stream's shelf, the fetch of the photo just taken with the hands off, and the 24 h sweep with the menu's count. The Swift helper is compiled only by `bundle-macos.sh`, on a Mac.

On a Mac you can reach another way, with the person at the Mac. Ask for each photo from the hub (its client's `eyes.photo` for this Mac). Keep a terminal on the Mac with `~/Library/Application Support/tulpa/photos` and `~/Library/Logs/tulpa/tulpa.log` at hand. Deny first, then the unanswered prompt, then Allow:

1. Build with `scripts/bundle-macos.sh`. `codesign -d --entitlements - target/bundle/Tulpa.app/Contents/MacOS/tulpa-camera` lists `com.apple.security.device.camera`.
2. Install the bundle, set `[eyes] mode = "ask"` and restart Tulpa. The menu shows `eyes: camera on request, asks first`. Reset the camera grant so that macOS asks again: `tccutil reset Camera com.tulpa.app`.
3. **Deny.** Ask for a photo, and click **Deny** in the Tulpa dialog. The answer is `denied by the owner`. The camera light stays off, no photo is in the directory on the Mac, and none arrives on the hub.
4. **The unanswered prompt.** Ask again, click **Allow** in the Tulpa dialog, and leave the macOS camera prompt unanswered. After 60 s the answer is `camera: no photo in 60 s (the macOS camera prompt was not answered; …)`. `pgrep tulpa-camera` prints nothing, and there is no photo on the Mac or on the hub. Note whether the macOS prompt is still up after the helper is gone; if it is, answer it **Allow**.
5. **Allow.** Ask again, click **Allow** in the Tulpa dialog, and **Allow** in the macOS prompt if it shows. The person at the Mac confirms that the camera light came on, for about 2 s, and went off. The hub prints where it keeps the photo: `-rw-------` there, in a `drwx------` directory. On the Mac the photo is gone from the directory, `tulpa.log` says `removed from the shelf`, and the eyes line counts no photo waiting.
6. Turn on **Pause all** and ask again: the answer is `paused`, and the camera light stays off.
7. Set `[hands] mode = "off"`, restart Tulpa and ask again: the photo is fetched as in step 5.

### Test the Allow/Deny dialog

On any machine, `cargo test --workspace` covers the dialog's logic, `crates/tulpa-tray/src/askdialog.rs`, with a recorder in place of the window. A new question shows one dialog. Its deadline closes it as expired. An answer from the menu closes it. Two questions show in order, and a late click on the first answers nothing. **Allow** is armed after 1 s. The window itself, `AlertPresenter` in `macos.rs`, is an `NSAlert` run as a sheet on a small panel of its own. It is only type-checked and linted for macOS on the build machine. None of it has run on a Mac yet.

On a Mac you can reach another way, with the person at the Mac, use a policy with `put = "ask"` and `ask_timeout_secs = 30`, or `[eyes] mode = "ask"`. Keep `~/Library/Logs/tulpa/tulpa.log` at hand. Ask from the hub each time:

1. With another app in front, ask. A window titled Tulpa comes up in front of it, with the question. A notification points at it. **Allow** is greyed out for about a second, then clickable. Repeat with a full-screen app in front, and from another Space.
2. Press Return. The request is denied, and the log says `decider=click`.
3. Ask again and click **Allow**. The request runs.
4. Ask again and answer nothing. After 30 s the dialog closes by itself, and the request answers `no click in time` (`decider=expired`).
5. Ask again and answer with **Deny** in the menu. The dialog closes.
6. Ask again. While the dialog is up, open the menu: it opens, and Tulpa's lines still change. Turn on **Pause all**: the dialog closes, and the request is refused.

Write down anything that differs: the window behind another app's, Return typed into the other app, **Allow** clickable at once, a menu that does not open while the dialog is up, a dialog left after its question went.

## First install

`scripts/make-installer.sh` builds one zip per Mac. Everything about the server is passed in; nothing is committed.

```sh
scripts/make-installer.sh --device <device> --alias <ssh-alias> \
  --server-host <server> --server-user <user> --known-hosts <file> \
  [--server-port 22] [--jump <jump-alias-or-user@host>] [--target-port 18031] \
  [--office-ssid <ssid>]... [--direct-host <host>]... [--full] [--out <dir>]
```

- It writes `<out>/tulpa-<device>.zip` and `<out>/<device>.token`. The token goes into the hub's devices directory as `<device>.token`, mode 0600.
- `--known-hosts` must hold a key for the server and for every `--direct-host`. The link runs `ssh` with `BatchMode=yes` and `StrictHostKeyChecking=yes`, so a route to an unknown host key fails. The script refuses to build without them.
- `--full` starts the hands in full mode. Without it the hands start on, with no roots.

On the Mac, the owner unpacks the zip and double-clicks `install.command`. Apart from the app itself, it keeps whatever already exists, and it says what it did:

| Item | What `install.command` does |
| --- | --- |
| `Tulpa.app` | Replaces `~/Applications/Tulpa.app`. |
| `config.toml`, `hands.toml`, `token` | Copies them to `~/.config/tulpa`, mode 0600, only when absent. |
| SSH key | Makes `~/.ssh/tulpa_ed25519` when absent. |
| SSH config | Appends the `Host` entry (and a `-jump` entry) when the alias is absent. |
| `known_hosts` | Appends the missing lines. |
| LaunchAgent | Writes `~/Library/LaunchAgents/com.tulpa.app.plist` when absent (`RunAtLoad`, `KeepAlive = { SuccessfulExit = false }`, GUI sessions only, `ProcessType = Interactive`). In a plist it keeps, it sets `ProcessType = Interactive`, turns `KeepAlive = true` (an older install's) into `{ SuccessfulExit = false }`, and changes nothing else. Then it boots the job out and in again in `gui/<uid>`. |

It ends by printing the Mac's public key. Authorize it on the server, and with a jump host there too, restricted to the server. If macOS refuses to open the downloaded app: `xattr -dr com.apple.quarantine ~/Applications/Tulpa.app`.

`install.command` replaces the bundle and restarts Tulpa with no rollback. Run it at the Mac only. Through Tulpa's own hands, the bootout stops Tulpa, Tulpa kills the script, and the bootstrap never runs: Tulpa stays down until the next login. To update a remote Mac, use the [dead-man](#update-a-mac-with-the-dead-man).

### ProcessType = Interactive

**Rule.** Tulpa's LaunchAgent runs with `ProcessType = Interactive`.

**Why.** Without a `ProcessType`, launchd applies its default resource limits to the job, which throttle its CPU and I/O. The capture runs as Tulpa's child, under the same limits. Throttled, it delivers only a fraction of the microphone's samples: chunks full of gaps, and few or no transcript lines.

**How the code keeps it:**

- `install.command` writes the key into a new plist. In a plist it keeps, it sets the key with `plutil -replace`. It then reloads the job, so the key applies at once. A Mac installed by an older `install.command` lacks the key until `install.command` runs there again or someone sets it by hand.
- `update.sh` never sets the key. (The one key it changes in the plist is an older install's `KeepAlive = true`, in [step 5](#update-a-mac-with-the-dead-man).) Its relaunch registers the job again from the plist ([bootout, then bootstrap](#why-the-relaunch-registers-the-job-again)), so launchd reads the plist then, except where it has to fall back to `kickstart -k`. It reads how launchd runs the job from `launchctl print gui/<uid>/com.tulpa.app`, where it accepts a `spawn type`, `process type` or `ProcessType` line that says interactive. It reads it before the swap and logs it, and again once the new bundle has checked in. When the job is not Interactive, the update still goes on: the new bundle stays, `install.log` gets a `FAILED:` line, and `update.sh` exits 3.

**Fix by hand**, in a Terminal at the Mac, never through Tulpa's own hands (rule 4). launchd reads the plist only when it loads the job, so reload it:

```sh
plist=~/Library/LaunchAgents/com.tulpa.app.plist
plutil -replace ProcessType -string Interactive "$plist"
launchctl bootout gui/$(id -u)/com.tulpa.app
launchctl bootstrap gui/$(id -u) "$plist"
```

Running `install.command` again at the Mac does the same, but it also replaces the app with the bundle in its folder.

### Keep-awake

**Rule.** Tulpa's own keep-awake is the only one a Mac needs. Leave `[keepawake] always` on, its default.

**Why.** A Mac that sleeps drops the link, and nothing reaches it until someone wakes it.

Three holders keep the Mac awake, each with a `caffeinate` that watches one pid (`-w`) and ends with it:

| Holder | When | What runs |
| --- | --- | --- |
| Tulpa | from its start until it exits (`[keepawake] always = true`, the default) | `caffeinate <flags> -w <Tulpa's pid>`; the flags are `-i -s` by default |
| `update.sh` | from before its copy until it exits, or dies with Tulpa at the relaunch | `caffeinate -dimsu -w <its own pid>` |
| the dead-man's job | from arming until the new bundle stays, or until the rollback and its wait for the old bundle's check-in end | `caffeinate -dimsu -w <pid of a small process of the job>` |

Tulpa's own:

- `[keepawake] flags` names the assertions, from the letters `d`, `i`, `m`, `s` and `u` only: `-d` display awake (where a sleeping display stops the system-audio capture), `-i` no idle sleep, `-m` no disk idle sleep, `-s` no system sleep on AC power, `-u` user active. Write them as `"-i -s"` or `"-dimsu"`. Any other value is refused at start and logged as `keep-awake: [keepawake] flags refused (…); holding -is instead`, and `-i -s` is held: a typo never leaves the Mac without a keep-awake.
- With `[keepawake] always = false`, Tulpa holds it only while a capture runs or a hands request is at work, and ends it 120 s after the last one. `ping`, a refused request and a question waiting for a click do not hold it.
- A `caffeinate` that ends while it should hold, or does not start, is started again after 1 s, then 2, 4 … up to 60 s apart while it keeps ending. A run of 60 s starts the waits over. The log gets one line per such episode, at its first end.
- The menu, `tulpa.log` at start, and `ears.status` (as `keepawake`) show the state with its flags: `keep-awake: on (always, -is)`, or `FAILED (…)` while no `caffeinate` runs.

The update's:

- `update.sh` refuses to update when its `caffeinate` does not start (exit 1, nothing changed).
- The job holds its own from arming on. launchd's runs of the job are short, so a small process of the job holds the `caffeinate`, and the job's plist sets `AbandonProcessGroup` so it outlives the run. It ends when the window closes, and runs at most 15 min; launchd's next run, 10 s later, starts another while the window is still open. A holder that does not start logs `FAILED: keep-awake for the job's window: …`, and the next run tries again.
- Run through Tulpa's hands, `update.sh`'s `caffeinate` dies with it at the relaunch, and the job's carries on until the new or the restored Tulpa holds its own.

**Check:**

```sh
grep 'keep-awake:' ~/Library/Logs/tulpa/tulpa.log | tail -n 1   # keep-awake: on (always, -is)
pmset -g assertions | grep -i caffeinate                         # Tulpa's caffeinate holds
ps -axo pid,ppid,command | grep '[c]affeinate'                   # caffeinate -i -s -w <Tulpa's pid>
```

**Not enforced:**

- Nothing checks that a Mac runs with `always = true`. Read it back in the preflight.
- Between Tulpa's exit and its next start, nothing holds the Mac awake unless `update.sh` or its job is running: after a crash, a kill, or a reload of the LaunchAgent. launchd starts Tulpa again (`KeepAlive`), and the new Tulpa holds its own from its start. After Quit, nothing holds the Mac awake until Tulpa is opened again.

A separate keep-awake LaunchAgent from an older setup is no longer needed. Remove it only once the Mac's Tulpa shows `keep-awake: on (always, …)`:

```sh
launchctl bootout gui/$(id -u)/<label>
rm ~/Library/LaunchAgents/<label>.plist
```

### Hands after an install or update

**Rule.** After any install, update or rollback, read the hands mode back and restore the one you intend.

**Why.** Several paths leave a Mac with other hands than you meant:

- A fresh install writes `[hands] mode = "on"` and a policy with no roots and `full = false` (`true` with `--full`). `install.command` keeps existing files, so this applies only where `~/.config/tulpa` was empty.
- A policy the running build cannot load turns the hands off: a key it does not know (an older build refuses a key that a newer one added), a root that no longer exists, a bad command. The menu then says `hands: off (no usable policy, see log)`.
- **Hands: full** in the menu is saved into the policy file. When that write fails, the next restart reverts it, and the log says so.
- `[hands] mode = "full"` is not a mode. The config is refused, and Tulpa does not start.

**Read it back:**

- At the Mac: the menu's hands line and the **Hands: full** check, and the log's latest `hands: policy loaded, N roots` (with `, FULL mode` when full) or `hands: policy refused: …`.
- From the agent: `ping` answers `pong` when the hands are on and `verb` when they are off. `shell true` answers `exit 0` in full mode and `shell: the hands are not in full mode` otherwise.

**Restore:** set `[hands] mode` and the policy's `full`, then restart Tulpa (the policy is read once at start), or use **Hands: full** in the menu.

## Update a Mac with the dead-man

`packaging/macos/update.sh` replaces the installed bundle with a launchd dead-man armed first. [REHEARSAL.md](../packaging/macos/REHEARSAL.md) walks through both directions on a reachable Mac, with the `install.log` lines to expect. Rehearse it before you rely on it.

```sh
sh update.sh /path/to/new/Tulpa.app
```

What it does, in order:

1. Holds the Mac awake for the swap with a [`caffeinate` of its own](#keep-awake). If that does not start, it refuses.
2. Copies the new bundle beside the installed one, as `Tulpa.app.new`. If `update.sh` runs under a Tulpa that launchd's job does not run (through that Tulpa's hands), it refuses here and changes nothing: step 4 would stop that Tulpa, and the script with it.
3. Arms the dead-man before it touches the installed bundle: a launchd job `com.tulpa.app.rollback` and a deadline file, set to the swap plus 80 s. The job holds its own keep-awake from then on. From an SSH session, where the GUI domain may refuse the job, it loads the job in `user/<uid>`. If the job cannot be loaded, nothing is swapped. It also logs how launchd runs Tulpa now ([ProcessType](#processtype--interactive)).
4. Stops every Tulpa from `~/Applications/Tulpa.app` that launchd's job does not run, such as one opened by hand after a Quit: SIGTERM, then SIGKILL after 10 s, logged as `stopped a Tulpa outside launchd: pid N`. No relaunch of the job stops such a Tulpa. A Tulpa is one whose command line is the installed executable's path; the job's own is the pid `launchctl print` shows. When launchd does not show the job, nothing is stopped.
5. Swaps by renaming only: the running bundle becomes `Tulpa.app.prev` (an older one waits as `Tulpa.app.prev.old` until the check-in), and the copy becomes `Tulpa.app`. Then, if the app's plist has `KeepAlive = true` (from an older `install.command`), it sets `KeepAlive = { SuccessfulExit = false }`, logged as `KeepAlive = true in … is now { SuccessfulExit = false }`, so that Quit stops Tulpa; the relaunch in step 6 loads it. Any other `KeepAlive` is left as it is. A rollback keeps the change. `update.sh` never sets `ProcessType`.
6. Relaunches Tulpa by registering its LaunchAgent with launchd again ([why](#why-the-relaunch-registers-the-job-again)): `launchctl bootout gui/<uid>/com.tulpa.app`, a wait of up to 30 s until launchd no longer shows the job and its old process has ended, then `launchctl bootstrap gui/<uid> ~/Library/LaunchAgents/com.tulpa.app.plist`, retried for 10 s. The dead-man job's own run does this, not `update.sh`: the bootout stops Tulpa, and through Tulpa's hands `update.sh` with it, so the bootstrap must come from a process that outlives both. `update.sh` leaves a request (`rollback.relaunch`) and starts the job; if the job is busy, its next run takes the request.
   - Where the job had to be loaded in `user/<uid>` (an SSH session), a bootstrap in `gui/<uid>` is not expected to work either, so it does not boot the app out. It uses `launchctl kickstart -k` instead, and where that is refused, it stops the pid launchd reports (SIGTERM, then SIGKILL after 10 s) for `KeepAlive` to start the new bundle: Tulpa ends by the signal, not with a successful exit. Both keep the job's launch constraint, and launchd keeps the `KeepAlive` it loaded before step 5 until the job is registered again.
   - If the bootstrap still fails after the bootout, the job is not loaded at all. It then opens the app outside launchd, so that Tulpa runs, and logs `FAILED:`.
   - Then it checks, for up to 20 s, that `launchctl print` shows a pid whose command line is the installed executable, and logs `launchd runs com.tulpa.app as pid N, from …`. If launchd does not, it logs `FAILED: launchd does not run com.tulpa.app from … after the relaunch:` with what launchd shows: the state, runs, last exit code, properties and launch-constraint (`LWCR`) lines.
7. Waits for the check-in. The new Tulpa rewrites `~/Library/Application Support/tulpa/checkin` each time the hub accepts its hello: `version`, `commit`, `exe_inode`, `pid`, `utc` and `epoch`, one `key=value` per line. Only a check-in from the new executable (by inode), no older than the swap, counts. If launchd reports a failed spawn or a non-zero exit meanwhile, it logs launchd's view once.
8. Checks that launchd runs Tulpa as `ProcessType = Interactive`.

The job runs every 10 s and looks once each time:

- A check-in from the new bundle: the new bundle stays, and the job removes itself. If it saw the check-in before `update.sh` did, it logs the process type too.
- The deadline passed without one: it logs what launchd shows for the app's job, moves the new bundle aside as `Tulpa.app.failed`, puts `Tulpa.app.prev` back, and relaunches it as in step 6, registering the job again. Otherwise a constraint now pinned to the new build could kill the restored one. Then it waits as long as `update.sh` waited, 60 s by default, for the restored bundle's own check-in: from the restored executable, no older than the restore. It logs `rollback: old bundle checked in (…)`, or, loudly, `rollback: old bundle did NOT check in within 60 s of its relaunch …`. Either way it keeps every bundle and removes itself. It never rolls back twice.
- Otherwise nothing yet.

`update.sh` and the job's runs claim each decision by renaming the deadline file, so two of them never act at once. A rollback cut short by a logout, a reboot or a kill is resumed by launchd's next run.

| Variable | Default | Meaning |
| --- | --- | --- |
| `TULPA_LABEL` | `com.tulpa.app` | The app's LaunchAgent label. |
| `TULPA_CHECKIN_WAIT` | 60 | Seconds `update.sh` waits for the check-in, and the job for the restored bundle's. |
| `TULPA_ROLLBACK_GRACE` | 20 | Seconds after that before the job rolls back. |
| `TULPA_ROLLBACK_EVERY` | 10 | The job's interval, in seconds. |

| Exit status | Meaning |
| --- | --- |
| 0 | The new bundle checked in and stays, and launchd runs it as Interactive. The job is gone; `Tulpa.app.prev` is kept. |
| 1 | Refused or failed before the swap. Nothing changed. |
| 2 | No check-in in time. At the swap plus 80 s, or up to 10 s later, the job puts `Tulpa.app.prev` back and waits for its check-in. |
| 3 | The new bundle checked in and stays, but launchd does not run it as Interactive. [Fix the plist](#processtype--interactive) at the Mac. |

Every step goes to `~/Library/Logs/tulpa/install.log`, as `update[pid]` and `rollback[pid]` lines. Run through Tulpa's hands, `update.sh` dies with Tulpa at the relaunch. That is expected: the job needs neither the script nor Tulpa, and it finishes the work.

**Rules:**

- Rehearse both directions on a Mac you can reach another way: a good bundle checks in and stays, and a broken bundle is rolled back and the old one checks in again. If either fails, update no other Mac.
- Update one Mac at a time. Start the next only when the first has checked in, or has been rolled back and its old bundle has checked in again.
- After a rollback, read `install.log`. `rollback: old bundle checked in` means the old bundle is back online. `did NOT check in` means it is not: look at the Mac, its network and the hub, because the job does not try again.
- A Mac that sleeps or reboots inside the 80 s window rolls back even a good bundle. That is the safe direction; update again with the Mac awake.
- A downgrade to a build without the check-in marker is always rolled back. A rollback to such a build always logs `did NOT check in`; confirm at the hub instead.

**Abort:**

```sh
sh ~/Library/Application\ Support/tulpa/rollback.sh com.tulpa.app "$HOME" now                       # roll back at once
sh ~/Library/Application\ Support/tulpa/rollback.sh com.tulpa.app "$HOME" disarm "aborted by hand"  # keep what is installed
```

`now` hands the work to launchd's own job, so it survives the restart even when run through the hands. `disarm` is refused while a rollback is in progress. REHEARSAL.md also shows the abort by hand, for when the job's script is gone.

### Why the relaunch registers the job again

**Rule.** `update.sh` and its job relaunch Tulpa with `launchctl bootout` and then `launchctl bootstrap`, not with `launchctl kickstart -k` alone, and first stop any Tulpa that runs outside launchd's job.

**What was seen** on one Mac, running a self-signed build with no Team ID:

- A rebuilt bundle, signed with the same identity, was installed by `update.sh` and relaunched with `kickstart -k`. It had the same designated requirement and a new cdhash. launchd's spawn of it was killed at once. The crash report reads `SIGKILL (Code Signature Invalid)`, termination namespace `CODESIGNING`, code 4, `Launch Constraint Violation`, parent launchd. No check-in came, and the dead-man rolled back.
- Afterwards, `launchctl print gui/<uid>/com.tulpa.app` showed the properties `needs LWCR update | managed LWCR | has LWCR`, an `LWCR reqs` entry of the form `cdhash $in [...]`, the state `spawn failed`, and `last exit code = 78` (EX_CONFIG). Tulpa never exits with 78, so that code is launchd's own record of a spawn it could not complete.
- At the same time, a Tulpa that had been opened by hand after a Quit was running outside the job: `launchctl print` showed no pid, and `kickstart -k` did not stop it.
- An hour earlier, another rebuilt bundle had installed and launched on the same Mac.

**What we infer**, without proof:

- launchd attaches a launch constraint (a lightweight code requirement, LWCR) to the job. For a build without a Team ID, the cdhash is the only thing it can pin, and every rebuild changes it.
- The constraint is computed when the job is registered (bootstrapped), and `kickstart -k` keeps the one from the last registration. The install that worked an hour earlier had, we believe, bootstrapped the job afresh. So the relaunch now registers the job again.
- Which part of macOS sets the constraint (launchd, or Background Task Management, which "managed" suggests), and when it updates it, is not documented. Whether a new registration always recomputes it is not verified yet.

**What the code does about it:**

- The relaunch registers the job again (step 6), and so does the rollback's relaunch.
- A Tulpa outside the job is stopped before the swap (step 4).
- After each relaunch, the job checks that launchd runs the installed executable, and logs the `LWCR` lines when it does not.

If our inference is wrong, the new build is still killed at its launch. `install.log` then has the `FAILED: launchd does not run …` line with launchd's view, and the dead-man rolls back. Re-registering the job then needs a person at the Mac: see the [known gaps](#known-gaps).

## The login build check

`packaging/macos/build-check.sh` keeps the installed Tulpa on the latest good build. Installed once, it runs at every login (a LaunchAgent, `RunAtLoad`, then hourly) and needs nobody: it compares the installed bundle's `Contents/Resources/build` commit with `refs/tags/latest-good` on the origin, and on a mismatch installs the staged bundle that carries that commit, through `update.sh`, unattended. Its behavior test, against a stubbed origin, staging and update, is `scripts/test-build-check.py`.

**The ref is the contract.** `latest-good` (on the private origin, never named in this repository) is moved by the operator after each build has been installed and verified on every Mac. The check only ever installs a commit the ref names — never `master`, never a branch head.

Install it once per Mac, from a machine that has the script:

```sh
TULPA_CHECK_ORIGIN=<the git url latest-good lives at> sh packaging/macos/build-check.sh --install
```

`--install` copies the script to `~/Library/Application Support/tulpa/build-check.sh`, writes `~/Library/LaunchAgents/com.tulpa.build-check.plist` with the caller's origin baked into the job's environment (the origin names a private host: it lives in the installed plist, never here), loads the job, and runs the first check at once.

What a run does:

1. Reads the installed bundle's marker. A bundle without one (built before the marker existed) reads `unknown`, which never equals `latest-good`, so the first check after this ships installs.
2. Reads `latest-good` with `git ls-remote` (BatchMode ssh, a 10 s connect timeout; an annotated tag is peeled through `^{}`). No origin configured, or a failed lookup: the outcome is `failed-origin` / `failed-ls-remote`.
3. A matching commit: the outcome is `ok`, and nothing else happens.
4. A mismatch: the newest bundle in `~/Library/Application Support/tulpa/staging` (a `Tulpa.app*` with an executable) whose own marker carries `latest-good` goes through the staging dir's `update.sh`, unattended. Its exit 0 or 3 is `updated` (3: installed, the app's plist wants a look — `install.log` says); anything else is `failed-install`, and `update.sh`'s own dead-man restores the previous bundle. No such bundle, or no `update.sh`: `failed-staging`.

Every outcome is written to `~/Library/Application Support/tulpa/build-check` (`outcome=`, `commit=`, `latest=`, `utc=`), atomically. Tulpa watches that marker and speaks each change of its meaning to the agent as a link `build-check` event — a Mac that fell behind, or failed to catch up, becomes visible without anyone reading the Mac's log. The event is the alarm; nothing rings locally.

Staging, then, is the pipeline: a new build is bundled, its commit tagged `latest-good` only after both Macs verified, and the bundle placed in each Mac's staging dir. The check does the rest at the next login. A dirty bundle (`-dirty` in its marker) never matches a released sha and always updates.

## Port forwards on the server

A port forward ([USE.md](USE.md#port-forwards)) needs the server to allow it for the Mac's key. Without the change below, sshd refuses `-W` to the port and `-R` outright: `AllowTcpForwarding local` and `PermitListen none`, the link's restricted setup, allow neither. Nothing here is applied by Tulpa. An operator applies it on the server, after review.

The boundary stays on the server: the Mac's key line names the exact ports, and an sshd `Match` block gives remote forwarding to the Tulpa user alone. Every other key keeps what it has. The Mac's own `[forward] ports` list must name the same ports; a port in only one of the two lists can't be used.

**1. The `Match` block.** sshd takes the first value it finds for each keyword, so a second block for the same user is silently ignored. If a `Match User <tulpa-user>` block already exists (the link's own restriction may live in one), edit that block. Otherwise add this one at the end of `/etc/ssh/sshd_config` (a `Match` runs to the end of the file or the next `Match`). `<tulpa-user>` is the account the Mac's key logs in as; `53682` stands for each allowed port:

```diff
+Match User <tulpa-user>
+    AllowTcpForwarding yes
+    PermitOpen localhost:<broker-port> 127.0.0.1:<broker-port> 127.0.0.1:53682
+    PermitListen 127.0.0.1:53682
+    GatewayPorts no
+    X11Forwarding no
+    PermitTTY no
```

`PermitOpen` keeps the link's own target (the broker's port, as the existing restriction has it) and adds each forward port. `PermitListen` names each port an in forward may listen on, on loopback only. `GatewayPorts no` keeps a listened port off the network.

**2. The key line** in `~<tulpa-user>/.ssh/authorized_keys`, the Mac's existing line with the forward ports added (`<mac-address>` is the existing `from=` value):

```diff
-restrict,port-forwarding,permitopen="localhost:<broker-port>",from="<mac-address>" ssh-ed25519 AAAA… tulpa@<mac>
+restrict,port-forwarding,permitopen="localhost:<broker-port>",permitopen="127.0.0.1:53682",permitlisten="127.0.0.1:53682",from="<mac-address>" ssh-ed25519 AAAA… tulpa@<mac>
```

**3. Apply**, with a second session to the server already open and kept open until the checks pass:

```sh
sudo cp -p /etc/ssh/sshd_config /etc/ssh/sshd_config.before-forwards
cp -p ~<tulpa-user>/.ssh/authorized_keys ~<tulpa-user>/.ssh/authorized_keys.before-forwards
# edit both as above
sudo sshd -t
# the effective config for the Mac's connection, not the edited file:
sudo sshd -T -C user=<tulpa-user>,host=<mac>,addr=<from-address> | grep -Ei 'allowtcpforwarding|permitlisten|permitopen'
#   expect: allowtcpforwarding yes · permitlisten 127.0.0.1:53682 · permitopen with the broker's port and 127.0.0.1:53682
sudo systemctl reload ssh     # or: sudo kill -HUP "$(cat /run/sshd.pid)"
```

`<from-address>` is the address sshd actually sees for the Mac's connection: on the jump route, the jump host's, not the Mac's. The key line's `from=` must name that same address. Never reload when `sshd -t` fails or `sshd -T` shows other values. **Rollback:** copy both `.before-forwards` files back, `sudo sshd -t`, reload.

**4. Check**, before any Mac uses it:

- Another user (or another key) on the server: `ssh -N -R 127.0.0.1:53682:127.0.0.1:53682 <server>` is refused (`remote port forwarding failed`).
- The Mac's key, a port off the list: `-R 127.0.0.1:9222:127.0.0.1:9222` is refused, and so is `-W 127.0.0.1:9222`.
- The Mac's key, a listed port: `forward.open "53682 in"` answers `open`; on the server, `ss -ltn` shows `127.0.0.1:53682` and nothing on `0.0.0.0:53682`; `forward.close "53682 in"` and it is gone.

**A listener inside a container.** When the tool that waits for the redirect runs in a container with its own network namespace, the server's `127.0.0.1:N` is not the container's. A relay from the host's loopback port into the container (for example `socat` in the container's namespace) is an operator's job, not Tulpa's. It must live and die with the forward: started for the sign-in, stopped at `forward.close` or when the forward's time is up, or given its own time limit no longer than the forward's. A relay with no end is a hole that outlives its job. Check: after `forward.close`, nothing listens on `N` in the host's namespace or the container's.

**The hub.** The forward verbs are a grant of their own at the hub, given by name to the steward and the operators, never covered by `*` or `write`. A member without that grant is refused at the hub, before the Mac hears it.

### Rehearse forwards on a Mac before a release

In this order, on a Mac you can reach another way, the menu open:

1. `forward.open "<port> out"` with the port not in `[forward] ports`: refused by Tulpa (`not on this Mac's forward list`), nothing in the menu.
2. A listed port, `out`: the row appears. Open `http://127.0.0.1:<port>/` in the browser twice: the first reaches the server, the second is refused, and the row is gone.
   Then the real flow, once in Chrome and once in Safari: `out` on the OAuth tool's port and its authorize step (`rclone authorize` for 53682). A browser may open a speculative extra socket; if the redirect's request goes over a second connection, `once` refuses it and the flow fails. If either browser fails, `once` must change (close after the first connection that carried bytes, or keep the listener a moment past the first accept) before release.
3. `out ttl=60 many`: leave it. At 60 s the row goes and the log says `its time limit`.
4. `in`: the row appears; on the server `ss -ltn` shows the port. Then `kill -9` Tulpa's process (`pgrep -x tulpa`). Within a few seconds the server's port is free and no `ssh … -R` is left on the Mac (`pgrep -fl "ssh.*-R 127.0.0.1"`). Start Tulpa again: the log says nothing was left (the watchdog got it) or names the ssh it killed.
5. `in` again, then **Pause all**: the row goes and the server's port is free. Resume opens nothing.
6. `in` again, then **Quit**: the same, before Tulpa exits.

## Preflight after every change

**Rule.** After every install, update, config change or restart, run one preflight: a single table of expected against actual values, one row per check. Run it after a change, not before every experiment.

**Rule.** When you change something on purpose, update its expected value in the same step. An expected value that lags behind turns the table red for nothing, and people learn to ignore it.

This repository ships no preflight script. Keep yours, and its expected values, outside the repository with the rest of your private setup. Read-only checks it should cover:

| Check | How to read it on the Mac |
| --- | --- |
| Tulpa's agent runs | `launchctl print gui/$(id -u)/com.tulpa.app` shows `pid = …` |
| `ProcessType` | In the plist: `/usr/libexec/PlistBuddy -c 'Print :ProcessType' ~/Library/LaunchAgents/com.tulpa.app.plist` prints `Interactive`. As launchd runs it: `launchctl print gui/$(id -u)/com.tulpa.app` has a `spawn type`, `process type` or `ProcessType` line that says interactive. |
| `KeepAlive` | `plutil -extract KeepAlive.SuccessfulExit raw -o - ~/Library/LaunchAgents/com.tulpa.app.plist` prints `false`: launchd starts Tulpa again after a crash or a kill, not after Quit. An older install's plist has `KeepAlive = true` until `update.sh` or `install.command` runs there; launchd reads the change when it loads the job again. |
| The keep-awake | The menu's keep-awake line, or `keepawake` in `ears.status`, reads `on (always, …)` with the flags you set; `tulpa.log` has the same line from Tulpa's start. `pmset -g assertions` lists Tulpa's `caffeinate`. |
| The installed build | `shasum -a 256 ~/Applications/Tulpa.app/Contents/MacOS/tulpa`, and `commit=` in `~/Library/Application Support/tulpa/checkin` |
| The signature | `codesign -d -r- ~/Applications/Tulpa.app` is unchanged |
| The hands mode | `[hands] mode` in `config.toml`, `full` in the policy file, and the log's latest `hands: policy loaded` line |
| The ears mode | `~/.config/tulpa/ears-mode`, and `mode` in `ears.status` |
| The lid | Whenever the ears should hear, the lid is open: `ioreg -r -k AppleClamshellState -d 4` shows `"AppleClamshellState" = No`. A closed lid stops the capture, and the ears then report DEAF with `(lid closed)`. A Mac without a lid shows no such line. |
| The system-audio side, through Tulpa | `ears.status` is not degraded on `them`, and the newest chunks in `them_dir`, written by Tulpa's own capture, are recent. Tulpa holds Screen & System Audio Recording. For as long as the system audio should be heard, the Mac's display-sleep and screen-lock settings, and `-d` in `[keepawake] flags`, keep the display awake and unlocked: a system-audio tool built on ScreenCaptureKit stops while the display sleeps or the screen is locked, and the ears then report `them` DEGRADED. |
| The microphone, through Tulpa | `ears.status` is neither degraded nor deaf, and the newest chunks in `me_dir`, written by Tulpa's own capture, hold sound for their whole length |
| The voice | the voice settings in `config.toml`; after a `voice.say`, no `voice: own voice failed` or `voice: resident synth failed` in the log |
| The eyes | `[eyes] mode` in `config.toml`; with the eyes on, Tulpa is listed and allowed under System Settings → Privacy & Security → Camera |
| Transcripts | the `transcript_files` grow while someone speaks |
| Sync | your sync tool's agent runs |

## Managed (corporate) Macs

A management policy can lock the screen, put the Mac to sleep, or block incoming SSH. Tulpa's outbound link may then be the only way in, so every [rule for a remote Mac](#rules-for-a-mac-you-cannot-reach-another-way) applies with no exceptions.

- Keep `[keepawake] always` on, and read the keep-awake line back after every restart.
- Update only with the dead-man, rehearsed on another Mac first.
- A sleeping display or a locked screen stops a system-audio tool built on ScreenCaptureKit. The ears then report `them` DEGRADED, often with `no display` in the error. `-d` in `[keepawake] flags` keeps the display awake; the policy may still lock the screen. A fallback that takes the system audio through a Core Audio process tap is being investigated (in progress, not in this repository).
- The system audio's lines go only in a call (since 2026-11-18), so a call left in `alone` or a playing video cannot leak other people's words. Before that date the gating keys (`wake_words` and friends) were the lever; they load but do nothing now.
- Keep the Mac's labels, paths and device name in your private notes, never in this repository.

## Transcript files and sync

**Rule.** Every Mac writes its own transcript files, named by device. Never let two Macs write one file in a synced folder.

**Why.** A sync tool resolves two writers of one file with conflict copies or by replacing one Mac's version with the other's. Lines get lost or doubled. Tulpa also reads a replaced file again from its start, so lines that already went out can go out again as new events.

**How.** Tulpa gives the capture command `TULPA_DEVICE`: `[link] device`, with every character outside `[A-Za-z0-9_-]` turned into `_`, at most 32 characters, and `tulpa` when nothing is left. `[ears] env` cannot override it. Write `{device}` in `[ears] transcript_files` for the same name, and name the synced folder there:

```toml
[link]
device = "<device>"

[ears]
transcript_files = ["~/tulpa-sync/ears/call-me.{device}.txt", "~/tulpa-sync/ears/call-them.{device}.txt"]
```

The capture script writes those same paths, using `$TULPA_DEVICE`. Anything else that reads the transcripts by name, such as the agent's reader or a sync rule, follows the per-device names.

## Stopping the capture

**What Tulpa does.** Each stop sends SIGKILL to the capture's whole process group at once: `ears.stop`, a mode switch, a new microphone, Pause, Quit, and Tulpa's own exit on SIGTERM, SIGINT or SIGHUP. When the capture's main process exits by itself, Tulpa kills the rest of its group too. There is no SIGTERM first, so a helper that ignores SIGTERM still dies, as long as it stays in the group. Hands commands are stopped the same way.

Capture helpers can ignore SIGTERM. That is why Tulpa never relies on it, and why a stop by hand must escalate.

**Known issues:**

- A helper that leaves the capture's process group (a new session or process group, a daemon, a LaunchAgent) is out of Tulpa's reach. Keep helpers in the script's group: plain background jobs in a `sh` or `bash` script stay in it.
- The capture gets no SIGTERM, so it cannot clean up. A partly written last chunk can stay in the capture directories.
- When a second Tulpa starts, it sends the running one SIGTERM and waits 10 s. It does not escalate. If the old one does not exit, the new one gives up (`another tulpa (pid N) still holds the instance lock`) and exits; under `KeepAlive`, launchd starts it again later.

**By hand.** Kill only a pid you have read, never by pattern (no `pkill -f`). Escalate:

```sh
kill -TERM <pid>; sleep 10; kill -0 <pid> 2>/dev/null && kill -KILL <pid>
```

`update.sh` and its job escalate the same way when they stop Tulpa: over SSH, when launchd refuses a restart; before the swap, a Tulpa outside launchd's job; at a bootout, the job's old process if it has not ended within 30 s. SIGTERM, then SIGKILL after 10 s. They find a Tulpa outside the job by its command line, the installed executable's path, then stop only the pids they have read.

## Files on the Mac

| Path | What |
| --- | --- |
| `~/Applications/Tulpa.app` | The app. `Tulpa.app.prev`, `.prev.old` and `.failed` come from the dead-man. |
| `~/Library/LaunchAgents/com.tulpa.app.plist` | Tulpa's LaunchAgent. `com.tulpa.app.rollback.plist` exists only while an update is armed or rolling back. |
| `~/.config/tulpa/config.toml`, `hands.toml`, `token` | The config, the hands policy and the link token, mode 0600. |
| `~/.config/tulpa/ears-mode` | The ears mode restored at start. |
| `~/Library/Logs/tulpa/tulpa.log` | The activity log. |
| `~/Library/Logs/tulpa/ears.log` | The capture's own output. |
| `~/Library/Logs/tulpa/install.log` | `update.sh` and its job. |
| `~/Library/Logs/tulpa/tulpa.lock` | The single-instance lock; it holds the running pid. |
| `~/Library/Application Support/tulpa/checkin` | The check-in marker: version, commit, executable inode, pid and time of the last accepted hello. |
| `~/Library/Application Support/tulpa/build-check` | The login build check's outcome marker (`outcome`, `commit`, `latest`, `utc`); the tray watches it. |
| `~/Library/Application Support/tulpa/build-check.sh` | The check script, written by its own `--install`. |
| `~/Library/Application Support/tulpa/staging/` | Where the bundles the check installs wait, newest first. |
| `~/Library/LaunchAgents/com.tulpa.build-check.plist` | The check's LaunchAgent (`RunAtLoad`, then hourly), with the origin baked in. |
| `~/Library/Logs/tulpa/build-check.log` | The check's own log. |
| `~/Library/Application Support/tulpa/photos/` | The eyes' photos until they are fetched or 24 h old: the directory `0700`, each photo `0600`. |
| `~/Library/Application Support/tulpa/rollback.sh` | The dead-man's script, written by `update.sh` and left in place; it also runs the abort. |
| `~/Library/Application Support/tulpa/rollback.deadline` | An armed update. `rollback.deadline.restoring` is a rollback in progress; `rollback.now`, `rollback.relaunch` (`update.sh`'s request that the job relaunch the new bundle) and `rollback.awake` exist only while an update is armed or rolling back. |

## Before you commit

Run the checks listed in [AGENTS.md](../AGENTS.md): build, `fmt`, `clippy`, tests and `cargo deny`, from the repository root. Review the full staged diff for private values: this repository is public.

## Before a public push

Whatever reaches the public repository can be copied before it is removed. Before every push:

1. Run the gate with the private word list, which is kept outside this repository:

   ```sh
   scripts/public-check.sh /path/to/private-words.txt                  # origin/main..HEAD
   scripts/public-check.sh /path/to/private-words.txt origin/main..public
   ```

   It reads the tree of the range's tip and, for every commit in the range, its message, author and committer and the lines it adds, so a line that one commit adds and a later one removes still counts. A merge adds only the lines that none of its parents has; binary files are read as text. It exits 1 and lists each match, as `path:line:text`, as the commit and its field, or as `commit <sha> adds <path>:<line>: <text>`, when it finds:

   - a pattern of the word list: one extended regular expression per line, matched case-insensitively anywhere in a line;
   - a private IPv4 address (10/8, 172.16/12, 192.168/16, and the shared 100.64/10), a `.local` host name, or a home path `/Users/<lowercase name>`.

   Files under `test/fixtures/` may hold such addresses and names, but no word of the list. Documentation addresses (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24), `.example` names, DNS-SD service types (`_ssh._tcp.local.`), `localhost.local` and `/Users/Shared` always pass. The gate prints the matches, never the list. It reads commits, not the working copy: commit first.

   A pattern also matches inside longer words, so anchor a short one, as in `(^|[^a-z])name`, and escape a literal dot. Exit 2 means the list or the range is unusable: no patterns, a pattern that does not compile or that matches an empty line, or a range without exactly one tip.
2. Run gitleaks over the full history and require zero findings: `gitleaks detect --source . --log-opts="--all" --redact`.

The gate's own test is `sh test/public-check/run.sh`.

## Known gaps

| Gap | Effect | Until it is fixed |
| --- | --- | --- |
| A Mac installed by an older `install.command` lacks `ProcessType = Interactive`; `update.sh` reports it (exit 3) but does not set it. | The capture is throttled and misses samples. | [Set the key at the Mac](#processtype--interactive) and check it in the preflight. |
| From an SSH session whose GUI domain refuses `bootstrap` (the `armed:` line in `install.log` names `user/<uid>`), `update.sh` can relaunch Tulpa only with `kickstart -k` or a pid stop, which keep the job's launch constraint. | On a Mac whose launchd pins the old build's cdhash, the new build is killed at its launch, and the dead-man rolls back. | Update from a Terminal at the Mac, or from a session whose `armed:` line names `gui/<uid>`. |
| That registering the job again recomputes its launch constraint is inferred from one Mac, not verified ([why](#why-the-relaunch-registers-the-job-again)). | If it does not, every update of a rebuilt self-signed bundle is rolled back, with `FAILED: launchd does not run …` and the `LWCR` lines in `install.log`. | Rehearse direction 1 on a Mac you can reach another way. If the constraint holds, a person at the Mac has to clear it. |
| `update.sh` refuses to run under a Tulpa that launchd's job does not run (through its hands). | A Mac where Tulpa was opened by hand cannot be updated through Tulpa. | At the Mac, or over SSH, register the job again (REHEARSAL.md, [Before you start](../packaging/macos/REHEARSAL.md#before-you-start)), then update. |
| Between Tulpa's exit and its next start, nothing holds the Mac awake unless `update.sh` or its job runs. | A crash or a reload of the LaunchAgent can let the Mac sleep before the new Tulpa holds its own. | Reload the LaunchAgent only at the Mac. |
| Nothing checks that `[keepawake] always` is on. | With `always = false`, an idle Mac can sleep and cut the link. | Read the keep-awake line back in the preflight. |
| A system-audio tool built on ScreenCaptureKit stops while the display sleeps or the screen is locked. | `them` is DEGRADED: the other side of a call is not heard. | `-d` in `[keepawake] flags` keeps the display awake. A Core Audio process-tap fallback is being investigated (in progress). |
| A microphone name with a `:` or a leading digit is refused only in the menu, not when typed into `[ears] mic_device` by hand. | The capture's ffmpeg may record another input than the one named. | Name such a device in the capture script, never in the config. |
| That Quit leaves Tulpa down rests on launchd's documented `KeepAlive = { SuccessfulExit = false }` and on Quit's exit status 0; it has not been seen on a Mac. | If launchd starts Tulpa again anyway, Quit is not a full stop, and the ears come back in their saved mode. | Pause or `ears.stop` before Quit; `launchctl bootout gui/$(id -u)/com.tulpa.app` at the Mac keeps it down until the next login. |
| A Mac updated from an SSH session whose `armed:` line names `user/<uid>` gets the new `KeepAlive` in its plist, but launchd loads it only when the job is registered again. | Until then, launchd still starts Tulpa again after Quit. | Register the job again at the Mac (REHEARSAL.md, [Before you start](../packaging/macos/REHEARSAL.md#before-you-start)), or log out and in. |
| A build from before `KeepAlive = { SuccessfulExit = false }` exits with status 0 on SIGTERM. After a rollback to such a build under the new `KeepAlive`, launchd does not start it again after a SIGTERM: a `kill`, a takeover, or the pid stop of an update over SSH. | Tulpa stays down. An update over SSH that has to stop the pid fails its relaunch; at the deadline the dead-man rolls back and opens the restored bundle outside launchd. | Update such a Mac from a Terminal at the Mac. |
| macOS's own quit of the app (a permission's **Quit & Reopen**, Activity Monitor's **Quit**) is expected to end Tulpa with exit status 0, as Quit does (AppKit's terminate; not seen on a Mac). | launchd does not start it again. After Quit & Reopen, Tulpa runs outside its LaunchAgent until the next login: not started again after a crash, and `update.sh` refuses to run through its hands. | Register the job again at the Mac (REHEARSAL.md, [Before you start](../packaging/macos/REHEARSAL.md#before-you-start)). |
| A second Tulpa sends SIGTERM only, then gives up after 10 s. | A hung old instance blocks the new one. | Kill the old pid by hand, escalating. |
| A capture helper outside the capture's process group is not stopped. | It keeps running after a stop. | Keep helpers in the script's group. |
| `[app] start_paused` in `config.example.toml` is not read. | Tulpa always starts unpaused. | Pause from the menu after a start if needed. |
| There is no pause hotkey. | Pause needs the menu. | Use **Pause all**. |
| A photo that is never fetched is deleted once it is 24 h old, but only at Tulpa's start and with the next `eyes.photo`. | On a Mac that is not restarted and takes no photo, a leftover photo stays longer than 24 h. | The menu's eyes line counts waiting photos; delete them at the Mac. |
| A photo left over after its own fetch (a cut transfer) leaves to any `xfer.get` the hands allow, not only to the hub's own fetch: the Mac cannot tell who on the hub asks. | Whoever the hub lets send `xfer.get` can fetch a leftover photo by its name. | Set the hands' `read` posture to `ask`, or keep the hub's `xfer.get` of a photo path as narrow as its `eyes.photo`. |
| The camera helper is compiled only by `bundle-macos.sh` on a Mac; CI does not build it. | A Swift error shows only when a bundle is built. | Build a bundle after changing the helper. |
| The Allow/Deny dialog has not run on a Mac. Its window, its place in front of other apps and its keys are unchecked. | If it does not come up in front, a question can run to its time unseen, as before the dialog. | Answer from the menu; [rehearse the dialog](#test-the-allowdeny-dialog) at a Mac. |
| The heads-up notification is posted through `osascript`. | A click on it opens Script Editor, not Tulpa, and answers nothing. | Answer in the dialog or the menu. |
