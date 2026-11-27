#!/bin/sh
# Harness for packaging/macos/update.sh: runs it against stub launchctl/open/ditto/plutil/caffeinate on PATH and a fake home
# with fake bundles whose "executable" is a shell script. A good bundle checks in a second after the stub
# starts it (it takes its executable's inode at startup, like the app), and SIGUSR1 is its menu's Quit (exit
# 0); a bad one runs and never checks in. The stub keeps the app alive as the loaded plist's KeepAlive says,
# and the bundle that was installed is running when update.sh starts. A "tick" asks the stub launchctl to run
# the rollback job, as StartInterval would.
#
#   sh test/update-sh/run.sh   (TEST_SHELL=bash runs update.sh itself under bash; TEST_KEEP=1 keeps the fake homes)
#
# (a) a good bundle checks in → it stays, Tulpa.app.prev is the bundle that ran, the job is gone;
# (b) a bundle that never checks in → the job, past its deadline, puts Tulpa.app.prev back and relaunches it
#     (a newer check-in from another executable does not count);
# (c) the installer is killed at the relaunch (by the bootout, as Tulpa's hands kill it) → the job's run finishes
#     the relaunch, and still rolls back;
# (d) the installer is killed after arming, before the swap → the job moves nothing;
# (e) the abort by hand (rollback.sh … now) rolls back at once, long before the deadline;
# (f) from an ssh session whose GUI domain refuses bootstrap and kickstart: the job runs in the user domain,
#     the relaunch stops the pid launchd reports, a good bundle stays;
# (g) the same with a bundle that never checks in: the job rolls back;
# (h) launchd's run of the job sees the check-in before update.sh does: still "installed", exit 0;
# (i) from an ssh session, a bundle that ignores SIGTERM and never checks in: killed, rolled back;
# (j) a rollback cut short between its renames is resumed by launchd's next run; a by-hand disarm meanwhile
#     is refused;
# (k) the app's job runs without ProcessType = Interactive: the good bundle stays, but update.sh fails loudly
#     (exit 3), and so does launchd's run of the job when it sees the check-in first;
# (l) install.command writes the app's plist with ProcessType = Interactive and KeepAlive = { SuccessfulExit =
#     false }; in a kept one it sets ProcessType = Interactive, changes KeepAlive = true to that dict and
#     leaves any other KeepAlive;
# (m) the restored bundle never checks in: the job says so loudly, keeps every bundle and stops;
# (n) a rollback cut short while it waited for the restored bundle's check-in resumes the wait, without a
#     second relaunch.
# (o) caffeinate cannot start: update.sh refuses before anything moves;
# (p) the Mac as it was found: launchd's job fails to spawn (its launch constraint pins another build), a
#     Tulpa opened by hand runs outside it: that one is stopped before the swap, the relaunch registers the
#     job again (bootout, bootstrap), and launchd runs the new bundle, the one Tulpa left;
# (q) re-registering does not clear the constraint: the failed spawn is logged with launchd's view of the
#     job (state, last exit, LWCR lines), and the dead-man rolls back;
# (r) Quit stops Tulpa: the KeepAlive update.sh loaded leaves a Tulpa that exits 0 down, until a kickstart,
#     and starts one that is killed again; a plist already { SuccessfulExit = false } is not edited, and
#     from an ssh session the relaunch's pid stop still starts the new bundle; KeepAlive = false is kept;
# (s) run under a Tulpa outside launchd's job (its hands): refused before arming, nothing changed.
# (a) also has a Tulpa outside the job beside the job's own (only it is stopped), and (a), (b), (p) run with
# the stub's launch constraint: the relaunch, and the rollback's, register the job again. Every fake home's
# plist has KeepAlive = true, as an older install.command wrote it: (a) checks that update.sh changes it to
# { SuccessfulExit = false } before the relaunch loads it, (d), (o) and (s) that a refusal leaves it.
# Every rollback above also waits for, and logs, the restored bundle's own check-in. (a) and (c) follow the
# keep-awake: update.sh's caffeinate -w its own pid, the job's caffeinate -w its holder's pid, held across the
# relaunch (and, with the installer killed, until the rollback ends), then released.
# Everything runs under umask 002: the job's plist must still be 0644 (launchd refuses group-writable ones).
# A fake bundle checks in about a second after its start, so a case whose rollback must see the restored
# bundle check in gives update.sh (and so the job's wait) at least 3 s.
set -u
umask 002
here=$(cd -- "$(dirname -- "$0")" && pwd)
update=$(cd -- "$here/../.." && pwd)/packaging/macos/update.sh
shell=${TEST_SHELL:-sh}
root=$(mktemp -d)
trap 'for p in "$root"/*/stub/supervisor.pid "$root"/*/stub/app.pid "$root"/*/stub/stray.pid; do [ -f "$p" ] && kill -9 "$(cat "$p")" 2>/dev/null; done
	if [ -n "${TEST_KEEP:-}" ]; then printf "kept %s\n" "$root"; else rm -rf "$root"; fi' EXIT
PATH=$here/stubs:$PATH
export PATH
fails=0

check() {
	if eval "$2"; then
		printf 'ok   %s\n' "$1"
	else
		printf 'FAIL %s\n' "$1"
		fails=$((fails + 1))
	fi
}
kind_of() { cat "$1/Contents/kind" 2>/dev/null || printf none; }
# Retries a condition for up to 5 s: the KeepAlive restart happens after the call that asked for it.
eventually() {
	tries=0
	until eval "$1"; do
		tries=$((tries + 1))
		[ "$tries" -lt 25 ] || return 1
		sleep 0.2
	done
}

make_bundle() {
	mkdir -p "$1/Contents/MacOS"
	printf '%s\n' "$2" >"$1/Contents/kind"
	# The fakes keep running as `/bin/sh <executable>`, so `ps` shows the executable's path as it does for
	# the app; "kind" makes each one's content (the stub's stand-in for its cdhash) differ.
	case $2 in
	bad | mute) printf '#!/bin/sh\n# a broken fake Tulpa (kind %s): runs and never checks in\nwhile :; do sleep 1; done\n' "$2" >"$1/Contents/MacOS/tulpa" ;;
	deaf) printf '#!/bin/sh\n# a broken fake Tulpa that ignores SIGTERM and never checks in\ntrap "" TERM\nwhile :; do sleep 1; done\n' >"$1/Contents/MacOS/tulpa" ;;
	*)
		{
			printf '#!/bin/sh\n# kind: %s\n' "$2"
			cat <<'APP'
# a fake Tulpa that works: takes its executable's inode at startup, gets its hello accepted a second later
# SIGUSR1: the menu's Quit, exit 0
trap 'exit 0' USR1
ino=$(ls -i "$0" | awk '{ print $1 }')
sleep 1
dir="$HOME/Library/Application Support/tulpa"
mkdir -p "$dir"
printf 'version=0.1.0\ncommit=fake\nexe_inode=%s\npid=%s\nutc=%s\nepoch=%s\n' "$ino" "$$" \
	"$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(date +%s)" >"$dir/.checkin.tmp" && mv "$dir/.checkin.tmp" "$dir/checkin"
# FAKE_TICK: launchd's StartInterval fires right after the check-in, before update.sh looks.
[ -z "${FAKE_TICK:-}" ] || launchctl kickstart "gui/$(id -u)/com.tulpa.app.rollback" >/dev/null 2>&1
# FAKE_HANDS: a script this Tulpa runs through its "hands", as its child, once it checked in.
[ -z "${FAKE_HANDS:-}" ] || sh "$FAKE_HANDS"
while :; do sleep 1; done
APP
		} >"$1/Contents/MacOS/tulpa"
		;;
	esac
	chmod 0755 "$1/Contents/MacOS/tulpa"
}

# A fake home: the running bundle (old), the one before it (older), the app's LaunchAgent loaded (with
# ProcessType = Interactive, as install.command writes it, unless TEST_PROCESS_TYPE says otherwise; with
# KeepAlive = true, as an older install.command wrote it, unless TEST_KEEPALIVE says otherwise; with the
# stub's launch constraint when TEST_LWCR is on or stuck); the new bundle of KIND outside ~/Applications.
setup() {
	case_dir=$root/$1
	HOME=$case_dir/home
	STUB_STATE=$case_dir/stub
	export HOME STUB_STATE
	unset STUB_KILL_INSTALLER STUB_SSH
	app=$HOME/Applications/Tulpa.app
	support="$HOME/Library/Application Support/tulpa"
	log=$HOME/Library/Logs/tulpa/install.log
	job_plist=$HOME/Library/LaunchAgents/com.tulpa.app.rollback.plist
	agent=$HOME/Library/LaunchAgents/com.tulpa.app.plist
	mkdir -p "$HOME/Applications" "$HOME/Library/LaunchAgents" "$STUB_STATE/loaded"
	if [ -n "${TEST_LWCR:-}" ]; then printf '%s\n' "$TEST_LWCR" >"$STUB_STATE/lwcr"; fi
	old_kind=${TEST_OLD_KIND:-old}
	make_bundle "$app" "$old_kind"
	make_bundle "$app.prev" older
	printf '<plist><dict><key>Label</key><string>com.tulpa.app</string>%s%s</dict></plist>\n' \
		"${TEST_KEEPALIVE-<key>KeepAlive</key><true/>}" \
		"${TEST_PROCESS_TYPE-<key>ProcessType</key><string>Interactive</string>}" >"$agent"
	launchctl bootstrap "gui/$(id -u)" "$agent"
	eventually 'grep -q "^started .* ($old_kind)" "$STUB_STATE/calls"' || printf 'setup %s: the old app did not start\n' "$1"
	: >"$STUB_STATE/calls"
	new=$case_dir/new/Tulpa.app
	make_bundle "$new" "$2"
}
tick() { launchctl kickstart "gui/$(id -u)/com.tulpa.app.rollback"; }
# A Tulpa outside launchd's job, from the installed executable (as one opened by hand); detached, so that
# nothing here has to reap it. FAKE_HANDS, when set, is what it runs through its hands.
start_stray() {
	("$app/Contents/MacOS/tulpa" >/dev/null 2>&1 &
	printf '%s\n' "$!" >"$STUB_STATE/stray.pid")
	stray=$(cat "$STUB_STATE/stray.pid")
}
# The Mac as it was found: launchd's job cannot spawn the installed build (its launch constraint pins
# another one), and a Tulpa opened by hand runs outside it.
break_job() {
	printf 'on\n' >"$STUB_STATE/lwcr"
	printf 'another-build\n' >"$STUB_STATE/pin"
	kill "$(cat "$STUB_STATE/app.pid")"
	eventually 'launchctl print "gui/$(id -u)/com.tulpa.app" | grep -q "state = spawn failed"' || printf 'break_job: the job still spawns\n'
}
# The pids of fake Tulpas from the installed executable, as update.sh finds them; launchd's, as print shows it.
tulpas() { ps -axww -o pid=,command= | awk -v exe="$app/Contents/MacOS/tulpa" '{ pid = $1; sub(/^[ \t]*[0-9]+[ \t]+/, ""); if (index(" " $0 " ", " " exe " ")) print pid }'; }
launchd_pid() { launchctl print "gui/$(id -u)/com.tulpa.app" 2>/dev/null | awk '/^\tpid = [0-9]+$/ { print $3; exit }'; }
line_of() { grep -n "$1" "$log" | head -n 1 | cut -d: -f1; }
app_bootouts() { grep -c "^launchctl bootout gui/[0-9]*/com.tulpa.app\$" "$STUB_STATE/calls"; }
reregistered() { grep -q "$1.*relaunched com.tulpa.app (bootout + bootstrap" "$log"; }
# "<logger pid> <-w pid>" of a keep-awake line in install.log: $1 update|rollback, $2 its window's name.
awake_pids() { sed -n "s/.* $1\[\([0-9]*\)\]: keep-awake for $2: caffeinate -dimsu -w \([0-9]*\) (caffeinate pid [0-9]*)\$/\1 \2/p" "$log" | head -n 1; }
held_own_pid() { [ -n "$1" ] && [ "${1% *}" = "${1#* }" ]; }
call_line() { grep -n "^$1" "$STUB_STATE/calls" | head -n 1 | cut -d: -f1; }
released() { grep -q "^caffeinate -w $1 released\$" "$STUB_STATE/calls"; }
job_loaded() { [ -f "$STUB_STATE/loaded/com.tulpa.app.rollback.plist" ]; }
# The KeepAlive of plist $1: true (an older install's), or { SuccessfulExit = false }.
keepalive_true() { tr -d ' \t\n' <"$1" | grep -q '<key>KeepAlive</key><true/>'; }
crash_only() { tr -d ' \t\n' <"$1" | grep -q '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>'; }
# The app's plist as setup wrote it: KeepAlive = true, and no plutil ran.
plist_kept() { keepalive_true "$agent" && ! grep -q "^plutil" "$STUB_STATE/calls"; }
disarmed() { ! job_loaded && [ ! -e "$job_plist" ] && ! ls "$support" | grep -q '^rollback\.\(deadline\|now\)'; }
past_deadline() {
	deadline=$(sed -n 's/^deadline=//p' "$support/rollback.deadline")
	while [ "$(date +%s)" -lt "$deadline" ]; do sleep 1; done
}
# Runs the installer with its pid on record before it starts, so the stub can kill it.
install_killed_at() {
	STUB_KILL_INSTALLER=$1
	export STUB_KILL_INSTALLER
	$shell -c 'printf "%s\n" "$$" >"$STUB_STATE/installer.pid"; exec "$@"' sh \
		$shell "$update" "$new" >"$case_dir/out" 2>&1
	status=$?
}
rolled_back() {
	failed_kind=${2:-bad}
	check "$1 Tulpa.app is the bundle that ran before" '[ "$(kind_of "$app")" = old ]'
	check "$1 Tulpa.app.prev is the bundle before that again" '[ "$(kind_of "$app.prev")" = older ]'
	check "$1 the new bundle is kept aside as Tulpa.app.failed" '[ "$(kind_of "$app.failed")" = "$failed_kind" ]'
	check "$1 nothing is left as Tulpa.app.prev.old" '[ ! -e "$app.prev.old" ]'
	check "$1 the restored bundle is relaunched after the rollback" \
		'sed -n "/rollback.*\\(putting\\|resuming\\)/,\$p" "$log" | grep -qE "relaunched com.tulpa.app|stopped com.tulpa.app.s pid" &&
		eventually "grep \"^started\" \"\$STUB_STATE/calls\" | tail -n 1 | grep -q \"(old)\""'
	check "$1 the job removed itself" disarmed
	check "$1 install.log says it rolled back" 'grep -q "rolled back: .* the bundle from before the update again" "$log"'
	check "$1 the job waited for the restored bundle's own check-in" \
		'sed -n "/rollback.*relaunched com.tulpa.app\|rollback.*launchd started it again/,\$p" "$log" | grep -q "rollback.*: rollback: old bundle checked in (version 0.1.0, commit fake" &&
		grep -q "rolled back: .* again, and it checked in; rollback job removed" "$log"'
}

printf '# update.sh under %s, the job under /bin/sh\n' "$shell"

# (a) a good bundle, on a Mac whose launchd pins the build in the job's launch constraint, with a Tulpa
# outside launchd's job running beside the job's own
TEST_LWCR=on setup a good
start_stray
before=$(launchd_pid)
out=$(TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" 2>&1)
status=$?
check "(a) exit 0" '[ "$status" -eq 0 ]'
check "(a) the Tulpa outside the job, and only it, was stopped after arming, before the swap" \
	'! kill -0 "$stray" 2>/dev/null && [ "$(grep -c "stopped a Tulpa outside launchd" "$log")" -eq 1 ] &&
	grep -q "stopped a Tulpa outside launchd: pid $stray (started " "$log" &&
	[ "$(line_of "armed:")" -lt "$(line_of "stopped a Tulpa outside")" ] &&
	[ "$(line_of "stopped a Tulpa outside")" -lt "$(line_of "renamed the running bundle")" ]'
check "(a) the relaunch registered the job again, from the job's own run: bootout, then bootstrap" \
	'reregistered "rollback" && [ "$(app_bootouts)" -eq 1 ] && ! grep -q "^launchctl kickstart -k" "$STUB_STATE/calls" &&
	[ "$(grep -n "^launchctl bootout gui/[0-9]*/com.tulpa.app\$" "$STUB_STATE/calls" | cut -d: -f1)" -lt \
		"$(grep -n "^launchctl bootstrap gui/[0-9]* .*/com.tulpa.app.plist\$" "$STUB_STATE/calls" | cut -d: -f1)" ]'
check "(a) launchd runs the new bundle, and no Tulpa runs outside its job" \
	'pid=$(launchd_pid) && [ -n "$pid" ] && [ "$pid" != "$before" ] && [ "$(tulpas)" = "$pid" ] &&
	grep -q "launchd runs com.tulpa.app as pid $pid, from .*/Tulpa.app/Contents/MacOS/tulpa" "$log" &&
	! grep -q "^spawn refused" "$STUB_STATE/calls"'
check "(a) Tulpa.app is the new bundle" '[ "$(kind_of "$app")" = good ]'
check "(a) Tulpa.app.prev is the bundle that was running" '[ "$(kind_of "$app.prev")" = old ]'
check "(a) one generation: the older .prev is dropped after the check-in" '[ ! -e "$app.prev.old" ]'
check "(a) the job was armed before the swap" \
	'[ "$(grep -n "armed:" "$log" | cut -d: -f1)" -lt "$(grep -n "renamed the running bundle" "$log" | cut -d: -f1)" ]'
check "(a) the job removed itself" disarmed
check "(a) install.log says the older .prev was dropped" 'grep -q "dropped .*Tulpa.app.prev.old: one previous generation is kept" "$log"'
check "(a) install.log has the check-in and the outcome" 'grep -q "check-in seen (version 0.1.0, commit fake" "$log" && grep -q "outcome: installed" "$log"'
check "(a) the outcome is printed" 'printf "%s\n" "$out" | grep -q "outcome: installed"'
check "(a) after the swap, before the relaunch, the plist's KeepAlive = true became { SuccessfulExit = false }" \
	'crash_only "$agent" && ! keepalive_true "$agent" &&
	grep -q "update.*: KeepAlive = true in $agent is now { SuccessfulExit = false }: launchd starts Tulpa again after a crash, not after Quit" "$log" &&
	[ "$(line_of "installed the new bundle")" -lt "$(line_of "KeepAlive = true in")" ] &&
	[ "$(line_of "KeepAlive = true in")" -lt "$(line_of "relaunching com.tulpa.app")" ]'
check "(a) launchd's job has it: the relaunch loaded the plist after the change" \
	'crash_only "$STUB_STATE/loaded/com.tulpa.app.plist" &&
	[ "$(call_line "plutil -replace KeepAlive -xml")" -lt "$(call_line "launchctl bootstrap gui/[0-9]* .*/com.tulpa.app.plist\$")" ]'
check "(a) the changed plist is not writable by group or others under umask 002 (launchd refuses it)" \
	'[ "$(ls -l "$agent" | cut -c6,9)" = "--" ]'
check "(a) the job's ProcessType = Interactive is asserted, before and after the swap" \
	'grep -q "the running com.tulpa.app: ProcessType = Interactive (spawn type = interactive (4))" "$log" &&
	grep " update\[" "$log" | tail -n 1 | grep -q "com.tulpa.app runs with ProcessType = Interactive (spawn type = interactive (4))"'
upd_awake=$(awake_pids update "the swap")
job_awake=$(awake_pids rollback "the job's window")
check "(a) update.sh held caffeinate -dimsu -w its own pid, from before the copy" \
	'held_own_pid "$upd_awake" && [ -n "$(call_line "caffeinate -dimsu -w ${upd_awake#* }\$")" ] &&
	[ "$(grep -n "keep-awake for the swap" "$log" | cut -d: -f1)" -lt "$(grep -n "update.*: copied" "$log" | cut -d: -f1)" ]'
check "(a) the job held caffeinate -dimsu -w its own pid, from before the relaunch" \
	'held_own_pid "$job_awake" && [ "$(call_line "caffeinate -dimsu -w ${job_awake#* }\$")" -lt "$(call_line "launchctl bootout gui/[0-9]*/com.tulpa.app\$")" ]'
check "(a) both are released once the new bundle checked in" \
	'eventually "released ${upd_awake#* } && released ${job_awake#* }" && grep -q "keep-awake for the job.s window ended" "$log"'
tick
check "(a) a later tick finds no job" '[ "$(kind_of "$app")" = good ] && disarmed'

# (b) a bundle that never checks in, on a Mac whose launchd pins the build: the rollback registers the job
# again too, or the constraint pinned to the new build would kill the restored one
TEST_LWCR=on setup b bad
mkdir -p "$support"
printf 'version=0.1.0\ncommit=other\nexe_inode=1\npid=1\nutc=-\nepoch=%s\n' $(($(date +%s) + 600)) >"$support/checkin"
out=$(TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=3 $shell "$update" "$new" 2>&1)
status=$?
check "(b) exit 2" '[ "$status" -eq 2 ]'
check "(b) the outcome is printed" 'printf "%s\n" "$out" | grep -q "outcome: no check-in from the new bundle within 3 s"'
check "(b) the job stays armed" 'job_loaded && [ -e "$support/rollback.deadline" ]'
check "(b) its plist is 0644 under umask 002" '[ "$(ls -l "$job_plist" | cut -c1-10)" = "-rw-r--r--" ]'
check "(b) its plist abandons its process group (the keep-awake outlives launchd's short runs)" \
	'grep -q "<key>AbandonProcessGroup</key><true/>" "$job_plist"'
make_bundle "$case_dir/second/Tulpa.app" good
$shell "$update" "$case_dir/second/Tulpa.app" >/dev/null 2>&1
status=$?
check "(b) a second update is refused while the job is armed" '[ "$status" -eq 1 ] && [ "$(kind_of "$app")" = bad ] && grep -q "refused: an earlier update is still armed" "$log"'
tick
check "(b) a tick before the deadline changes nothing" '[ "$(kind_of "$app")" = bad ] && job_loaded'
past_deadline
tick
rolled_back "(b)"
check "(b) the rollback registered the job again, and launchd runs the restored bundle" \
	'sed -n "/putting .* back/,\$p" "$log" | grep -q "rollback.*relaunched com.tulpa.app (bootout + bootstrap" && [ "$(app_bootouts)" -eq 2 ] &&
	[ "$(tulpas)" = "$(launchd_pid)" ] && ! grep -q "^spawn refused" "$STUB_STATE/calls"'

# (c) the installer killed at the relaunch
setup c bad
TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=2 install_killed_at bootout
check "(c) the installer was killed" '[ "$status" -ge 128 ] && grep -q "killed the installer at bootout" "$STUB_STATE/calls"'
check "(c) it never reached an outcome" '! grep -q "outcome:" "$log"'
check "(c) the new bundle is in place and runs under launchd (the job's run finished the relaunch), the job armed" \
	'eventually "reregistered rollback && grep \"^started\" \"\$STUB_STATE/calls\" | tail -n 1 | grep -q \"(bad)\"" && [ "$(kind_of "$app")" = bad ] && job_loaded'
upd_awake=$(awake_pids update "the swap")
job_awake=$(awake_pids rollback "the job's window")
past_deadline
check "(c) the killed installer's caffeinate is gone; the job's still holds at the deadline" \
	'held_own_pid "$upd_awake" && eventually "released ${upd_awake#* }" && held_own_pid "$job_awake" && ! released "${job_awake#* }" && kill -0 "${job_awake#* }"'
tick
rolled_back "(c)"
check "(c) the job's keep-awake ends only after the rollback" \
	'eventually "released ${job_awake#* }" &&
	[ "$(grep -n "keep-awake for the job.s window ended" "$log" | cut -d: -f1)" -gt "$(grep -n "rolled back:" "$log" | cut -d: -f1)" ]'

# (d) the installer killed after arming, before the swap
setup d bad
TULPA_CHECKIN_WAIT=1 TULPA_ROLLBACK_GRACE=1 install_killed_at bootstrap
check "(d) the installer was killed before the swap" '[ "$status" -ge 128 ] && ! grep -q "renamed the running bundle" "$log"'
past_deadline
tick
check "(d) Tulpa.app and Tulpa.app.prev are untouched" '[ "$(kind_of "$app")" = old ] && [ "$(kind_of "$app.prev")" = older ] && [ ! -e "$app.failed" ]'
check "(d) the plist is untouched" plist_kept
check "(d) nothing was relaunched" '! grep -q "kickstart -k" "$STUB_STATE/calls" && [ "$(app_bootouts)" -eq 0 ]'
check "(d) the job removed itself" disarmed
check "(d) install.log says the swap never happened" 'grep -q "the swap never happened" "$log"'

# (e) the abort by hand
setup e bad
TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=600 $shell "$update" "$new" >/dev/null 2>&1
check "(e) armed with a deadline ten minutes out" 'job_loaded && [ "$(kind_of "$app")" = bad ]'
sh "$support/rollback.sh" com.tulpa.app "$HOME" now
check "(e) the rollback ran as launchd's job" 'grep -q "^launchctl kickstart gui/[0-9]*/com.tulpa.app.rollback" "$STUB_STATE/calls" && grep -q "rollback by hand; putting" "$log"'
rolled_back "(e)"
sh "$support/rollback.sh" com.tulpa.app "$HOME" now
status=$?
check "(e) a second abort finds nothing armed and moves nothing" '[ "$status" -eq 1 ] && [ "$(kind_of "$app")" = old ]'

# (f) from an ssh session, a good bundle
setup f good
STUB_SSH=1
export STUB_SSH
out=$(TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" 2>&1)
status=$?
check "(f) exit 0" '[ "$status" -eq 0 ]'
check "(f) the job ran in the user domain" 'grep -q "armed: com.tulpa.app.rollback (user/$(id -u))" "$log"'
check "(f) the relaunch stopped the pid launchd reported" 'grep -q "stopped com.tulpa.app.s pid [0-9]* (kickstart refused)" "$log"'
check "(f) Tulpa.app is the new bundle, Tulpa.app.prev the one that ran" '[ "$(kind_of "$app")" = good ] && [ "$(kind_of "$app.prev")" = old ]'
check "(f) the job removed itself from the user domain" disarmed

# (g) from an ssh session, a bundle that never checks in
setup g bad
STUB_SSH=1
export STUB_SSH
TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=2 $shell "$update" "$new" >/dev/null 2>&1
status=$?
check "(g) exit 2, the job armed in the user domain" '[ "$status" -eq 2 ] && job_loaded && [ "$(cat "$STUB_STATE/loaded/com.tulpa.app.rollback.domain")" = "user/$(id -u)" ]'
past_deadline
tick_user() { launchctl kickstart "user/$(id -u)/com.tulpa.app.rollback"; }
tick_user
rolled_back "(g)"

# (h) the job sees the check-in first
FAKE_TICK=1
export FAKE_TICK
setup h good
out=$(TULPA_CHECKIN_WAIT=5 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" 2>&1)
status=$?
unset FAKE_TICK
check "(h) the job disarmed itself on the check-in first" 'grep -q "rollback.*check-in seen" "$log" && ! grep -q "update.*check-in seen" "$log"'
check "(h) update.sh still reports the install, exit 0" '[ "$status" -eq 0 ] && printf "%s\n" "$out" | grep -q "outcome: installed"'
check "(h) Tulpa.app is the new bundle and the job is gone" '[ "$(kind_of "$app")" = good ] && disarmed'
check "(h) no stray second disarm" '[ "$(grep -c "rollback job removed" "$log")" -eq 1 ]'

# (i) from an ssh session, a bundle that ignores SIGTERM
setup i deaf
STUB_SSH=1
export STUB_SSH
TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=1 $shell "$update" "$new" >/dev/null 2>&1
past_deadline
tick_user
check "(i) the pid that ignored SIGTERM was killed" 'grep -q "ignored SIGTERM for 10 s; killed it" "$log"'
rolled_back "(i)" deaf

# (j) a rollback cut short after its first rename
setup j bad
TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=1 $shell "$update" "$new" >/dev/null 2>&1
mv "$support/rollback.deadline" "$support/rollback.deadline.restoring"
mv "$app" "$app.failed"
sh "$support/rollback.sh" com.tulpa.app "$HOME" disarm "aborted by hand"
status=$?
check "(j) a by-hand disarm during the rollback is refused" '[ "$status" -eq 1 ] && job_loaded && [ -e "$support/rollback.deadline.restoring" ] && [ -e "$job_plist" ]'
tick
check "(j) launchd's next run resumed it" 'grep -q "resuming the rollback" "$log"'
rolled_back "(j)"

# (k) the app's job without ProcessType = Interactive
TEST_PROCESS_TYPE= setup k good
out=$(TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" 2>&1)
status=$?
check "(k) warned before the swap" 'grep -q "the running com.tulpa.app is not ProcessType = Interactive (spawn type = daemon (3)); .* exits 3 after the swap" "$log"'
check "(k) exit 3, loudly, after the outcome" '[ "$status" -eq 3 ] && printf "%s\n" "$out" | tail -n 1 | grep -q "FAILED: com.tulpa.app does not run with ProcessType = Interactive (spawn type = daemon (3))"'
check "(k) the good bundle stays and the job is gone" '[ "$(kind_of "$app")" = good ] && [ "$(kind_of "$app.prev")" = old ] && disarmed'
check "(k) update.sh never sets ProcessType in the app's plist" '! grep -q "^plutil -replace ProcessType" "$STUB_STATE/calls"'
FAKE_TICK=1
STUB_PRINT_STYLE=key
export FAKE_TICK STUB_PRINT_STYLE
TEST_PROCESS_TYPE= setup k2 good
TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" >/dev/null 2>&1
status=$?
unset FAKE_TICK
check "(k) launchd's run, first to see the check-in, logs the missing ProcessType too" \
	'[ "$status" -eq 3 ] && grep -q "rollback.*FAILED: com.tulpa.app does not run with ProcessType = Interactive (launchctl print .* shows no process type)" "$log"'
TEST_PROCESS_TYPE='<key>ProcessType</key><string>Interactive</string>' setup k3 good
TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" >/dev/null 2>&1
status=$?
unset STUB_PRINT_STYLE
check "(k) a 'ProcessType = Interactive' line counts as well" '[ "$status" -eq 0 ] && grep -q "runs with ProcessType = Interactive (ProcessType = Interactive)" "$log"'

# (m) the restored bundle never checks in
TEST_OLD_KIND=mute
setup m bad
unset TEST_OLD_KIND
TULPA_CHECKIN_WAIT=2 TULPA_ROLLBACK_GRACE=1 $shell "$update" "$new" >/dev/null 2>&1
check "(m) the wait is recorded with the deadline" 'grep -qx "wait=2" "$support/rollback.deadline"'
past_deadline
tick
check "(m) the restored bundle is back and running" '[ "$(kind_of "$app")" = mute ] && grep "^started" "$STUB_STATE/calls" | tail -n 1 | grep -q "(mute)"'
check "(m) install.log says loudly it did NOT check in" \
	'grep -q "rollback: old bundle did NOT check in within 2 s of its relaunch — .* all bundles are kept" "$log" && ! grep -q "old bundle checked in" "$log"'
check "(m) every bundle is kept" '[ "$(kind_of "$app.failed")" = bad ] && [ "$(kind_of "$app.prev")" = older ]'
check "(m) the job stopped" 'disarmed && grep -q "but it did NOT check in; rollback job removed" "$log"'
tick
tick
check "(m) it never loops: one rollback, one relaunch" \
	'[ "$(grep -c "putting .* back" "$log")" -eq 1 ] && [ "$(app_bootouts)" -eq 2 ] && [ "$(kind_of "$app")" = mute ]'

# (n) a rollback cut short while it waited for the restored bundle's check-in
setup n bad
TULPA_CHECKIN_WAIT=4 TULPA_ROLLBACK_GRACE=1 $shell "$update" "$new" >/dev/null 2>&1
# What the job leaves when it is stopped in that wait: the bundles put back, the claim at phase=checkin.
{ cat "$support/rollback.deadline" && printf 'phase=checkin\nrestored=%s\nrelaunched=%s\n' "$(date +%s)" "$(date +%s)"; } >"$support/rollback.deadline.restoring"
rm -f "$support/rollback.deadline"
mv "$app" "$app.failed" && mv "$app.prev" "$app" && mv "$app.prev.old" "$app.prev"
: >"$STUB_STATE/calls"
kill "$(cat "$STUB_STATE/app.pid")"
tick
check "(n) launchd's next run resumed the wait, not the relaunch" \
	'grep -q "resuming the rollback" "$log" && ! grep -q "^launchctl kickstart -k" "$STUB_STATE/calls" && [ "$(app_bootouts)" -eq 0 ]'
check "(n) the restored bundle's check-in ends it" 'grep -q "rollback: old bundle checked in" "$log" && disarmed && [ "$(kind_of "$app")" = old ]'

# (o) caffeinate cannot start
setup o good
STUB_CAFFEINATE=fail $shell "$update" "$new" >/dev/null 2>&1
status=$?
check "(o) no keep-awake, no update: refused before anything moved" \
	'[ "$status" -eq 1 ] && grep -q "refused: caffeinate -dimsu -w [0-9]* did not start" "$log" && [ "$(kind_of "$app")" = old ] && [ ! -e "$app.new" ] && ! job_loaded && plist_kept'

# (p) the Mac as it was found: launchd's job cannot spawn (its launch constraint pins another build) and a
# Tulpa opened by hand runs outside it
TEST_LWCR=on setup p good
break_job
start_stray
out=$(TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" 2>&1)
status=$?
check "(p) exit 0: installed and checked in" '[ "$status" -eq 0 ] && [ "$(kind_of "$app")" = good ] && disarmed'
check "(p) the Tulpa outside launchd was stopped before the swap" \
	'! kill -0 "$stray" 2>/dev/null && grep -q "stopped a Tulpa outside launchd: pid $stray" "$log" &&
	[ "$(line_of "stopped a Tulpa outside")" -lt "$(line_of "renamed the running bundle")" ]'
check "(p) the relaunch registered the job again, which took the new build's constraint" \
	'reregistered "rollback" && grep "^started" "$STUB_STATE/calls" | tail -n 1 | grep -q "(good)" &&
	! sed -n "/^launchctl bootstrap gui\/[0-9]* .*com.tulpa.app.plist/,\$p" "$STUB_STATE/calls" | grep -q "^spawn refused"'
check "(p) launchd owns the one Tulpa, from the installed bundle" \
	'pid=$(launchd_pid) && [ -n "$pid" ] && [ "$(tulpas)" = "$pid" ] && grep -q "launchd runs com.tulpa.app as pid $pid" "$log"'

# (q) re-registering does not clear the constraint (it stays pinned to the old build): the failed spawn is
# logged with launchd's view of the job, and the dead-man rolls back to the build the constraint takes
TEST_LWCR=stuck setup q good
out=$(TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=2 $shell "$update" "$new" 2>&1)
status=$?
check "(q) exit 2: no check-in" '[ "$status" -eq 2 ] && [ "$(kind_of "$app")" = good ] && job_loaded'
check "(q) the relaunch logged the failed spawn: state, last exit, the LWCR lines" \
	'grep -q "rollback.*FAILED: launchd does not run com.tulpa.app from .* after the relaunch: state = spawn failed; runs = [0-9]*; last exit code = 78: EX_CONFIG; properties = needs LWCR update | managed LWCR | has LWCR; LWCR reqs = " "$log"'
check "(q) update.sh logged it too, before the outcome" \
	'grep -q "update.*: launchd reports a failed spawn or exit of com.tulpa.app, and no check-in yet: state = spawn failed;" "$log" &&
	[ "$(line_of "launchd reports a failed spawn")" -lt "$(line_of "outcome:")" ]'
past_deadline
tick
check "(q) the dead-man logged launchd's view when it decided" \
	'grep -q "no check-in from the new bundle by the deadline; putting .* back. launchd shows: state = spawn failed; .*LWCR" "$log"'
rolled_back "(q)" good

# (r) Quit stops Tulpa: under the KeepAlive the update loaded, a Tulpa that exits 0 stays down until a
# kickstart, and one that is killed is started again
setup r good
TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" >/dev/null 2>&1
status=$?
quit=$(launchd_pid)
kill -USR1 "${quit:-0}"
check "(r) after a Quit (exit 0) launchd does not start Tulpa again" \
	'[ "$status" -eq 0 ] && [ -n "$quit" ] && crash_only "$STUB_STATE/loaded/com.tulpa.app.plist" &&
	eventually "grep -q \"^exited 0: KeepAlive does not start it again\" \"\$STUB_STATE/calls\"" &&
	sleep 1 && [ -z "$(launchd_pid)" ] && [ -z "$(tulpas)" ] && [ "$(grep -c "^started" "$STUB_STATE/calls")" -eq 1 ]'
launchctl kickstart "gui/$(id -u)/com.tulpa.app"
eventually '[ -n "$(launchd_pid)" ]'
crashed=$(launchd_pid)
kill -KILL "${crashed:-0}"
check "(r) a kickstart starts it again, and after a kill launchd does by itself" \
	'[ -n "$crashed" ] && [ "$crashed" != "$quit" ] && eventually "again=\$(launchd_pid) && [ -n \"\$again\" ] && [ \"\$again\" != $crashed ]"'
# A plist install.command writes now: not edited; from an ssh session (no re-registration) the relaunch stops
# the pid launchd reports, which ends by the signal, not with a successful exit: KeepAlive starts the new bundle
TEST_KEEPALIVE='<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>' setup r2 good
STUB_SSH=1
export STUB_SSH
TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" >/dev/null 2>&1
status=$?
unset STUB_SSH
check "(r) a plist already { SuccessfulExit = false } is not edited, and over ssh the pid stop still relaunches" \
	'[ "$status" -eq 0 ] && ! grep -q "^plutil -replace" "$STUB_STATE/calls" && crash_only "$agent" &&
	grep -q "stopped com.tulpa.app.s pid [0-9]* (kickstart refused); launchd started it again as pid" "$log" &&
	[ "$(kind_of "$app")" = good ] && disarmed'
TEST_KEEPALIVE='<key>KeepAlive</key><false/>' setup r3 good
TULPA_CHECKIN_WAIT=10 TULPA_ROLLBACK_GRACE=5 $shell "$update" "$new" >/dev/null 2>&1
status=$?
check "(r) KeepAlive = false is left as it is" \
	'[ "$status" -eq 0 ] && ! grep -q "^plutil -replace" "$STUB_STATE/calls" && grep -q "<key>KeepAlive</key><false/>" "$agent" && ! grep -q "KeepAlive = true in" "$log"'

# (s) run through the hands of a Tulpa outside launchd's job: refused before arming, nothing changed
setup s good
printf '%s\n' "TULPA_CHECKIN_WAIT=3 TULPA_ROLLBACK_GRACE=2 $shell '$update' '$new' >'$case_dir/out' 2>&1; printf '%s\\n' \$? >'$case_dir/status'" >"$case_dir/hands.sh"
FAKE_HANDS=$case_dir/hands.sh
export FAKE_HANDS
start_stray
unset FAKE_HANDS
eventually '[ -s "$case_dir/status" ]' || eventually '[ -s "$case_dir/status" ]'
check "(s) refused: it runs under a Tulpa outside launchd" \
	'[ "$(cat "$case_dir/status")" = 1 ] && grep -q "refused: this script runs under a Tulpa outside launchd (pid $stray)" "$log"'
check "(s) nothing changed: no job, no copy, the old bundle and both Tulpas still run" \
	'[ "$(kind_of "$app")" = old ] && [ ! -e "$app.new" ] && ! job_loaded && [ ! -e "$job_plist" ] && kill -0 "$stray" && [ -n "$(launchd_pid)" ] && ! grep -q "armed:" "$log" && plist_kept'

# (l) install.command: a fresh install, then one over a kept plist without ProcessType and with KeepAlive = true,
# and one over a kept plist with KeepAlive = false
install_kit() {
	case_dir=$root/$1
	HOME=$case_dir/home
	STUB_STATE=$case_dir/stub
	export HOME STUB_STATE
	mkdir -p "$HOME" "$STUB_STATE/loaded"
	kit=$case_dir/kit
	make_bundle "$kit/Tulpa.app" good
	for f in config.toml hands.toml token; do printf '# placeholder\n' >"$kit/$f"; done
	printf 'Host tulpa-hub\n  HostName hub.example.invalid\n' >"$kit/ssh_config"
	: >"$kit/known_hosts"
	cp "$here/../../packaging/macos/install.command" "$kit/install.command"
	agent=$HOME/Library/LaunchAgents/com.tulpa.app.plist
}
print_app() { launchctl print "gui/$(id -u)/com.tulpa.app"; }
install_kit l
bash "$kit/install.command" >"$case_dir/out" 2>&1
status=$?
check "(l) a fresh install writes ProcessType = Interactive and loads it" \
	'[ "$status" -eq 0 ] && grep -q "<key>ProcessType</key><string>Interactive</string>" "$agent" && print_app | grep -q "spawn type = interactive"'
check "(l) a fresh install writes KeepAlive = { SuccessfulExit = false } and loads it" \
	'crash_only "$agent" && ! keepalive_true "$agent" && crash_only "$STUB_STATE/loaded/com.tulpa.app.plist"'
install_kit l2
mkdir -p "$HOME/Library/LaunchAgents"
printf '<plist><dict><key>Label</key><string>com.tulpa.app</string><key>KeepAlive</key><true/></dict></plist>\n' >"$agent"
bash "$kit/install.command" >"$case_dir/out" 2>&1
status=$?
check "(l) a kept plist gets ProcessType = Interactive before the reload" \
	'[ "$status" -eq 0 ] && grep -q "kept $agent" "$case_dir/out" && grep -q "ProcessType = Interactive in $agent" "$case_dir/out" &&
	[ "$(grep -n "^plutil -replace ProcessType -string Interactive" "$STUB_STATE/calls" | cut -d: -f1)" -lt "$(grep -n "^launchctl bootstrap" "$STUB_STATE/calls" | cut -d: -f1)" ] &&
	print_app | grep -q "spawn type = interactive"'
check "(l) a kept plist's KeepAlive = true becomes { SuccessfulExit = false } before the reload" \
	'grep -q "KeepAlive = { SuccessfulExit = false } in $agent: Quit stops Tulpa" "$case_dir/out" && crash_only "$agent" && ! keepalive_true "$agent" &&
	[ "$(grep -n "^plutil -replace KeepAlive -xml" "$STUB_STATE/calls" | cut -d: -f1)" -lt "$(grep -n "^launchctl bootstrap" "$STUB_STATE/calls" | cut -d: -f1)" ] &&
	crash_only "$STUB_STATE/loaded/com.tulpa.app.plist"'
install_kit l3
mkdir -p "$HOME/Library/LaunchAgents"
printf '<plist><dict><key>Label</key><string>com.tulpa.app</string><key>KeepAlive</key><false/><key>ProcessType</key><string>Interactive</string></dict></plist>\n' >"$agent"
bash "$kit/install.command" >"$case_dir/out" 2>&1
status=$?
# The last case: the stub has the app's pid on record once it logs the start, and the exit trap stops it then.
eventually 'grep -q "^started" "$STUB_STATE/calls"'
check "(l) a kept plist's KeepAlive = false is left as it is" \
	'[ "$status" -eq 0 ] && grep -q "<key>KeepAlive</key><false/>" "$agent" && ! grep -q "^plutil -replace KeepAlive" "$STUB_STATE/calls" && ! grep -q "KeepAlive" "$case_dir/out"'

if [ "$fails" -eq 0 ]; then
	printf 'update-sh: all checks passed\n'
else
	printf 'update-sh: %s check(s) failed\n' "$fails"
	exit 1
fi
