# Rehearsing an update with the dead-man

`update.sh` replaces `~/Applications/Tulpa.app` and keeps the old bundle as `Tulpa.app.prev`. Before it
touches anything, it starts a launchd job, `<label>.rollback`, that puts `Tulpa.app.prev` back unless the
new bundle checks in. A check-in is the file `~/Library/Application Support/tulpa/checkin`. The app
rewrites it each time its link's hello is accepted. Rehearse on a Mac you can reach in other ways before
you update one you can't:

1. A good bundle checks in and stays, and launchd runs it as `ProcessType = Interactive`.
2. A broken bundle never checks in and is rolled back. The old bundle is back within about 90 s of the
   swap, and the job waits for its check-in too.
3. Optional: an old bundle that can't link after the rollback. The job says so loudly, keeps every
   bundle, and stops.

Throughout, the Mac must stay awake. `update.sh` holds a `caffeinate` of its own for the swap, the
rollback job holds one for its window, and the new Tulpa holds its own for as long as it runs
(`keep-awake: on (always, -is)`).

If direction 1 or 2 fails, don't update another Mac.

## Before you start

- Tulpa was installed on this Mac by `install.command`. The bundle is `~/Applications/Tulpa.app`. Every
  command below names the LaunchAgent through `$label`, so set it first, in each shell you use:

  ```sh
  label=${TULPA_LABEL:-com.tulpa.app}   # where the Mac's label differs, export TULPA_LABEL for update.sh too
  ```

- Check how launchd runs Tulpa now:

  ```sh
  launchctl print gui/$(id -u)/$label | grep -iE 'spawn type|process ?type'
  ```

  `spawn type = interactive (4)` is right. `spawn type = daemon (3)` means the plist has no
  `ProcessType`, and launchd's background limits starve the microphone capture. Fix it at the Mac, in a
  Terminal, never through Tulpa's own hands: run `install.command` again (it adds the key to a kept plist
  and reloads it), or do it by hand:

  ```sh
  plutil -replace ProcessType -string Interactive ~/Library/LaunchAgents/$label.plist
  launchctl bootout gui/$(id -u)/$label; launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$label.plist
  ```

  `update.sh` never sets `ProcessType`. Without the fix it still installs, then exits 3.
- Check that launchd runs Tulpa, and that no Tulpa runs outside its job:

  ```sh
  exe=$HOME/Applications/Tulpa.app/Contents/MacOS/tulpa
  launchctl print gui/$(id -u)/$label | grep -iE '^[[:space:]]*(state|pid|last exit code) =|lwcr'
  pid=$(launchctl print gui/$(id -u)/$label | awk '/^\tpid = /{ print $3 }'); echo "launchd's pid: ${pid:-none}"
  ps -o pid=,lstart=,command= -p "${pid:-0}"                 # the command is $exe
  pgrep -f "^$exe( |\$)" | grep -vx "${pid:-none}"                # must print nothing: no Tulpa outside the job
  ```

  `state = running` with a `pid` whose command is the installed executable is right. A Tulpa outside the
  job (opened by hand, for example after a Quit) is one `update.sh` stops before its swap. It refuses
  to run under such a Tulpa, through its hands. `state = spawn failed`, or `LWCR` lines with no pid,
  mean launchd can't start Tulpa now. Register the job again, which launchd allows over ssh when the
  GUI domain accepts it. A Tulpa outside the job keeps running meanwhile, and launchd's new one takes
  over from it:

  ```sh
  launchctl bootout gui/$(id -u)/$label; sleep 5
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$label.plist
  sleep 15; launchctl print gui/$(id -u)/$label | grep -iE '^[[:space:]]*(state|pid|last exit code) =|lwcr'
  ```
- Keep whatever keeps this Mac awake today running until the new build shows `keep-awake: on (always, -is)`
  (direction 1). Until then, an older Tulpa holds the Mac awake only while the agent works on it.
- Nobody else is using the Mac's Tulpa while you rehearse. Each direction restarts it.
- Where to run the commands:
  - **A Terminal on the Mac, or ssh to it.** You see `update.sh`'s outcome when it finishes. Over ssh,
    launchd's GUI domain may refuse to load the job or restart the app. `update.sh` then loads the job
    in `user/<uid>` and restarts the app by stopping the pid launchd reports, so KeepAlive starts the
    new bundle. `install.log` says when this happens.
  - **Tulpa's hands.** The relaunch stops Tulpa, which kills `update.sh` together with its hands
    job. This is expected: the launchd job finishes the update, and `install.log` records each step.

## Build the two bundles (build Mac)

```sh
scripts/bundle-macos.sh        # target/bundle/Tulpa.app, with this commit's check-in marker
```

Copy `target/bundle/Tulpa.app` and `packaging/macos/update.sh` to the Mac, for example with
`scp -r` or `rsync -a`, into `~/tulpa-rehearsal/good/Tulpa.app` and `~/tulpa-rehearsal/update.sh`. Then
make the broken bundle on the Mac. It's a copy of the good one whose executable runs but never links:

```sh
cd ~/tulpa-rehearsal
ditto good/Tulpa.app broken/Tulpa.app
printf '#!/bin/sh\nexec sleep 3600\n' > broken/Tulpa.app/Contents/MacOS/tulpa
chmod 0755 broken/Tulpa.app/Contents/MacOS/tulpa
```

To watch the Mac's keep-awake holders hand over during each direction, run this in a second shell:

```sh
while sleep 2; do date -u +%T; ps -axo pid,ppid,command | grep '[c]affeinate'; done
```

Each `caffeinate` line shows whom it serves: `-w <pid>` is the process it watches.

## Direction 1: a good bundle stays

```sh
cd ~/tulpa-rehearsal
sh update.sh ~/tulpa-rehearsal/good/Tulpa.app; echo "exit $?"
```

Expect `exit 0` within about 5–15 s. Among the printed lines, in this order:

```text
update: keep-awake for the swap: caffeinate -dimsu -w <update.sh's pid> (caffeinate pid <n>)
update: the running <label>: ProcessType = Interactive (spawn type = interactive (4))
update: relaunching <label>: <label>.rollback registers it with launchd again
update: outcome: installed — the new bundle checked in (version 0.1.0, commit <commit>, at <UTC>); the previous bundle is kept as …/Tulpa.app.prev
update: <label> runs with ProcessType = Interactive (spawn type = interactive (4))
```

On a Mac whose plist still has `KeepAlive = true` (an older install), this line comes before
`relaunching`, and the relaunch loads the new `KeepAlive`:

```text
update: KeepAlive = true in …/<label>.plist is now { SuccessfulExit = false }: launchd starts Tulpa again after a crash, not after Quit
```

The job's run does the relaunch. In `install.log`, between `relaunching` and `outcome`:

```text
rollback[…]: relaunching <label> for update.sh
rollback[…]: relaunched <label> (bootout + bootstrap: launchd registered the job again from …/<label>.plist)
rollback[…]: launchd runs <label> as pid <n>, from …/Tulpa.app/Contents/MacOS/tulpa
```

If a Tulpa ran outside the job, `stopped a Tulpa outside launchd: pid <n> (started …)` comes before
`renamed the running bundle`.

Then check:

```sh
tail -n 20 ~/Library/Logs/tulpa/install.log
cat ~/Library/Application\ Support/tulpa/checkin       # commit = the good build, epoch = just now
ls -d ~/Applications/Tulpa.app*                        # Tulpa.app and Tulpa.app.prev (no .new, no .prev.old)
launchctl print gui/$(id -u)/$label.rollback           # must fail: the job is gone
ls ~/Library/LaunchAgents/$label.rollback.plist        # must fail: no such file
launchctl print gui/$(id -u)/$label | grep -iE 'spawn type|process ?type'   # spawn type = interactive (4)
plutil -extract KeepAlive.SuccessfulExit raw -o - ~/Library/LaunchAgents/$label.plist   # false
launchctl print gui/$(id -u)/$label | grep -E '^[[:space:]]*(state|pid) ='           # state = running, pid = <n>
ps -o lstart=,command= -p <n>             # started at the update; the command is …/Tulpa.app/Contents/MacOS/tulpa
pgrep -f "^$HOME/Applications/Tulpa.app/Contents/MacOS/tulpa( |\$)"             # <n> only: no Tulpa outside the job
grep 'keep-awake:' ~/Library/Logs/tulpa/tulpa.log | tail -n 1               # keep-awake: on (always, -is)
pmset -g assertions | grep -i caffeinate               # Tulpa's own caffeinate holds its assertions
ps -axo pid,ppid,command | grep '[c]affeinate'         # caffeinate -i -s -w <Tulpa's pid>; the swap's is gone
```

Tulpa's menu shows `keep-awake: on (always, -is)`.

At the Mac, once: choose **Quit** in Tulpa's menu. From 15 s later on,
`launchctl print gui/$(id -u)/$label | grep -E '^[[:space:]]*(state|pid|last exit code) ='` shows
`state = not running`, no `pid` and `last exit code = 0`: launchd does not start Tulpa again. After an
update whose `armed:` line names `user/<uid>`, launchd still has the old `KeepAlive`: register the job
again first, as in [Before you start](#before-you-start). Start Tulpa again before direction 2:
`launchctl kickstart gui/$(id -u)/$label`.

Three checks that must hold on every Mac, with `$label` as set in [Before you start](#before-you-start):

- **launchd runs Tulpa.** After every install and update, `launchctl print gui/$(id -u)/$label`
  shows `state = running` and a `pid` whose command is the installed executable, and `pgrep -f` of
  that path finds no other Tulpa.
- **ProcessType.** After bootstrap (install.command or the fix above), and after every update,
  `launchctl print gui/$(id -u)/$label` shows ProcessType = Interactive. The line reads
  `spawn type = interactive (4)`.
- **Keep-awake.** `pmset -g assertions` shows Tulpa's own assertion: its `caffeinate -i -s -w <Tulpa's
  pid>`, or the flags `[keepawake] flags` names (the status shows them, `-is` by default). During the swap, the temporary `caffeinate -dimsu -w <update.sh's pid>` holds one as well, and
  so does the job's for its window. Both are gone afterwards. Run `pmset -g assertions` during the swap
  too, or watch with the `ps` loop above.

From the agent's side, the device shows as online at the hub again, a `hostname` verb answers, and
`ears.status` (with `[ears]` configured) answers with `"keepawake": "on (always, -is)"`.

`exit 3` instead of 0 means the new bundle checked in and stays, but launchd does not run it as
Interactive. The last line says `FAILED: <label> does not run with ProcessType = Interactive (…)`. Fix
the plist as in [Before you start](#before-you-start). That needs a Terminal at the Mac.

## Direction 2: a broken bundle is rolled back, and the old one checks in again

Start this only after direction 1 has passed. The bundle that comes back is the good one from
direction 1, and its own check-in proves it's back online.

```sh
cd ~/tulpa-rehearsal
date -u; sh update.sh ~/tulpa-rehearsal/broken/Tulpa.app; echo "exit $?"
```

Expect `exit 2` after 60 s, ending with this line:

```text
update: outcome: no check-in from the new bundle within 60 s — the rollback job puts …/Tulpa.app.prev back at <UTC>
```

At that time (the swap plus 80 s), or up to 10 s later, the job rolls back. Then it waits up to 60 s for
the restored bundle's own check-in: a check-in from that bundle's executable that is newer than the
restore. Watch it:

```sh
tail -f ~/Library/Logs/tulpa/install.log
```

In order, look for these `rollback[…]` lines. The first comes when the job is armed, before the swap;
the others at the deadline:

```text
rollback[…]: keep-awake for the job's window: caffeinate -dimsu -w <holder pid> (caffeinate pid <n>)
rollback[…]: no check-in from the new bundle by the deadline; putting …/Tulpa.app.prev back. launchd shows: state = running; pid = <n>; …
rollback[…]: moved the new bundle aside as …/Tulpa.app.failed
rollback[…]: restored …/Tulpa.app.prev as …/Tulpa.app
rollback[…]: the older bundle is …/Tulpa.app.prev again
rollback[…]: relaunched <label> (bootout + bootstrap: launchd registered the job again from …/<label>.plist)
rollback[…]: launchd runs <label> as pid <n>, from …/Tulpa.app/Contents/MacOS/tulpa
rollback[…]: rollback: old bundle checked in (version 0.1.0, commit <the good build>, at <UTC>)
rollback[…]: rolled back: …/Tulpa.app is the bundle from before the update again, and it checked in; rollback job removed
rollback[…]: keep-awake for the job's window ended
```

Then check:

```sh
cat ~/Library/Application\ Support/tulpa/checkin       # a fresh epoch from the restored good bundle
ls -d ~/Applications/Tulpa.app*                        # Tulpa.app (good), Tulpa.app.prev, Tulpa.app.failed (broken)
launchctl print gui/$(id -u)/$label.rollback           # must fail: the job is gone
launchctl print gui/$(id -u)/$label | grep -E '^[[:space:]]*(state|pid) ='   # state = running, pid = <n>
pgrep -f "^$HOME/Applications/Tulpa.app/Contents/MacOS/tulpa( |\$)"     # <n> only
ps -axo pid,ppid,command | grep '[c]affeinate'         # Tulpa's own again; the job's has ended
```

This direction passes when `rollback: old bundle checked in` is in `install.log`, the device is online
at the hub, and a `hostname` verb answers.

## Direction 3 (optional): an old bundle that cannot link

This shows the loud line and that the job stops there. Run it only on a Mac you can reach without
Tulpa: for a few minutes, Tulpa can't link. Tulpa reads its link token at every attempt, so moving the
token aside keeps the restored bundle from checking in. The token is the file `[link] token_file` names,
by default `~/.config/tulpa/token`:

```sh
mv ~/.config/tulpa/token ~/.config/tulpa/token.aside
cd ~/tulpa-rehearsal
date -u; sh update.sh ~/tulpa-rehearsal/broken/Tulpa.app; echo "exit $?"   # exit 2 after 60 s, as in direction 2
tail -f ~/Library/Logs/tulpa/install.log
```

About 80–90 s after the swap the job rolls back as in direction 2. Then, 60 s after its relaunch:

```text
rollback[…]: rollback: old bundle did NOT check in within 60 s of its relaunch — …/Tulpa.app is the bundle from before the update but has not linked; all bundles are kept (…/Tulpa.app, …/Tulpa.app.prev, …/Tulpa.app.failed) and the job does not try again: look at the Mac and …/install.log
rollback[…]: rolled back: …/Tulpa.app is the bundle from before the update again, but it did NOT check in; rollback job removed
rollback[…]: keep-awake for the job's window ended
```

Check that every bundle is still there and the job stopped:

```sh
ls -d ~/Applications/Tulpa.app*                        # Tulpa.app, Tulpa.app.prev, Tulpa.app.failed
launchctl print gui/$(id -u)/$label.rollback           # must fail: the job is gone
sleep 60; tail -n 3 ~/Library/Logs/tulpa/install.log   # nothing new: no second rollback, no retry
```

Put the token back. Tulpa would link by itself at its next attempt, but its retries back off up to
5 minutes, so restart it:

```sh
mv ~/.config/tulpa/token.aside ~/.config/tulpa/token
launchctl bootout gui/$(id -u)/$label; sleep 5; launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$label.plist
sleep 15; cat ~/Library/Application\ Support/tulpa/checkin   # a fresh epoch
```

Over ssh, where the GUI domain refuses the bootout, use
`kill $(launchctl print gui/$(id -u)/$label | awk '/^\tpid = /{ print $3 }')` instead: KeepAlive starts
Tulpa again.

## Reading install.log

Each line is `<UTC> update[pid]: …` for the installer and `<UTC> rollback[pid]: …` for the job.

- `keep-awake for the swap: caffeinate …` comes first, before anything moves. The job's
  `keep-awake for the job's window: …` covers the time `update.sh` may be dead, until
  `keep-awake for the job's window ended`. During the swap, `pmset -g assertions` shows both
  `caffeinate` holders; afterwards only Tulpa's own remains.
  `FAILED: keep-awake for the job's window: …` means the job's `caffeinate` didn't start; its next run
  tries again.
- `the running <label>: ProcessType = Interactive (…)` comes before the swap. With
  `… is not ProcessType = Interactive …`, the update still goes ahead, and `update.sh` exits 3 after
  the check-in.
- `armed: <label>.rollback (<domain>) puts Tulpa.app.prev back at <UTC> …` comes before
  `renamed the running bundle …`. Without an `armed:` line, nothing was swapped. A domain of
  `user/<uid>` means the GUI domain refused the job, so the relaunch can't register the app's job
  again (see below).
- `stopped a Tulpa outside launchd: pid N (started …)` means a Tulpa that launchd's job didn't run
  (opened by hand) was stopped before the swap. `refused: this script runs under a Tulpa outside
  launchd (pid N) …` means `update.sh` ran through that Tulpa's hands. Nothing changed. Register the
  job again as in [Before you start](#before-you-start), then update.
- `relaunched <label> (bootout + bootstrap: …)` is the relaunch: the job is registered again, so launchd
  takes the new build's code requirement. `relaunched <label> (launchctl kickstart -k)`, or
  `stopped <label>'s pid …`, follows `… is not loaded in gui/<uid>, … not registered again` or
  `launchctl bootout … refused`. Then launchd keeps the job's old launch constraint.
- `launchd runs <label> as pid N, from …` confirms launchd's own process runs the installed executable.
  `FAILED: launchd does not run <label> from … after the relaunch: state = …; last exit code = …; …`
  shows what launchd says instead. `state = spawn failed`, `last exit code = 78: EX_CONFIG` and `LWCR`
  lines mean launchd refused to start that build. That's the launch constraint ([DEVELOP.md](../../docs/DEVELOP.md#why-the-relaunch-registers-the-job-again)).
  The dead-man rolls back at the deadline. `launchd reports a failed spawn or exit of <label>, and no
  check-in yet: …` is `update.sh`'s own note of the same.
- `FAILED: launchd does not take …; opened … outside launchd so that Tulpa runs` means the bootstrap
  failed after the bootout. Tulpa runs, but outside launchd. Register the job again at the Mac.
- `refused: …` means nothing changed. Examples are an earlier update that is still armed, a path with
  no `Contents/MacOS/tulpa`, or a `caffeinate` for the swap that didn't start.
- If `update[…]` lines stop after `relaunching <label>`, the installer died with Tulpa, which is
  normal under the hands. The `rollback[…]` lines that follow tell the rest:
  - `check-in seen … the new bundle stays`: the update succeeded. The job also logs the process type
    then: `<label> runs with ProcessType = Interactive (…)`, or `FAILED: …` when it doesn't.
  - `rolled back: …`: the update was rolled back. The line before it says whether the old bundle checked
    in again.
- `rollback: old bundle checked in (…)`: the restored bundle linked again after the rollback.
- `rollback: old bundle did NOT check in within N s …`: it didn't. The job keeps every bundle, tries
  nothing more, and removes itself. Look at the Mac: its network, the hub, `tulpa.log`. A bundle built
  before the check-in marker existed never writes one, so a rollback to it always ends this way. There,
  confirm at the hub instead.
- `the swap never happened` means the installer died between arming and the swap. Nothing was moved.
- `putting … back failed; the next run tries again` means a rename failed. The job retries every 10 s;
  read the `mv:` error above that line.
- `… is not running yet; the next run tries the relaunch again` means the old bundle is back on disk but
  launchd didn't confirm a restart. The job keeps its claim (`rollback.deadline.restoring`) and retries.
  `resuming the rollback an earlier run began` is that retry, or the job picking up again after a
  logout or reboot.
- `… (pid …) ignored SIGTERM for 10 s; killed it` means a hung Tulpa was killed: on the ssh-session
  path, a Tulpa outside the job, or the job's old process after a bootout.
- `outcome: the new bundle checked in (…) only after the rollback began` means the check-in came too
  late. The rollback stands.
- A Mac that sleeps or reboots inside the 80 s window comes back with the deadline passed. The job's
  first run then rolls back, even a bundle that might have been fine. That's the safe direction; run
  the update again with the Mac awake.

## Exit status

| Exit | Meaning |
| --- | --- |
| 0 | Installed: the new bundle checked in and stays, and launchd runs it as Interactive. |
| 1 | Refused or failed before the swap. Nothing changed. |
| 2 | No check-in in time. The job puts `Tulpa.app.prev` back at the deadline and waits for its check-in. |
| 3 | Installed and checked in, but launchd doesn't run it as Interactive. The bundle stays; fix the plist at the Mac. |

## The abort

Roll back now, without waiting for the deadline. launchd's own job does the restore, so it survives
the restart even when this is run through Tulpa's hands:

```sh
sh ~/Library/Application\ Support/tulpa/rollback.sh $label "$HOME" now
```

Keep the installed bundle and stop the dead-man. This is refused while a rollback is already in
progress:

```sh
sh ~/Library/Application\ Support/tulpa/rollback.sh $label "$HOME" disarm "aborted by hand"
```

If the job script is gone, do it by hand. Only an earlier failed copy is deleted; the bundles in use
are only renamed:

```sh
launchctl bootout gui/$(id -u)/$label.rollback; launchctl bootout user/$(id -u)/$label.rollback
rm -f ~/Library/LaunchAgents/$label.rollback.plist ~/Library/Application\ Support/tulpa/rollback.deadline
rm -f ~/Library/Application\ Support/tulpa/rollback.deadline.* ~/Library/Application\ Support/tulpa/rollback.now
cd ~/Applications && rm -rf Tulpa.app.failed && mv Tulpa.app Tulpa.app.failed && mv Tulpa.app.prev Tulpa.app
[ -d Tulpa.app.prev.old ] && mv Tulpa.app.prev.old Tulpa.app.prev
launchctl bootout gui/$(id -u)/$label; sleep 5; launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$label.plist
```

## Then the other Mac

Run direction 1 only, with the good bundle. You only need the check-in path you rehearsed. The job
puts the old bundle back by itself if the new one never links.

- If that Mac runs a build from before the check-in marker, a rollback there always logs
  `rollback: old bundle did NOT check in`. Confirm at the hub instead.
- Keep its current keep-awake until the new build shows `keep-awake: on (always, -is)`. Only then remove it.
