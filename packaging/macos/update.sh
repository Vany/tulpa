#!/bin/sh
# update.sh — replaces the installed ~/Applications/Tulpa.app with a new bundle, with a dead-man armed first.
#
#   sh update.sh /path/to/new/Tulpa.app
#
# Runs on the Mac as the user; no prompts, no sound. Every step goes to ~/Library/Logs/tulpa/install.log.
#  0. holds the Mac awake for the swap: caffeinate -dimsu -w <this script's pid>, until it exits (or dies
#     with Tulpa at the relaunch). The job holds its own for its window, and Tulpa its own from its start, so
#     the Mac does not sleep while no Tulpa holds one. No caffeinate, no update;
#  1. copies the new bundle beside the installed one (ditto; the same volume, so the renames are atomic);
#  2. BEFORE touching the installed bundle, arms the dead-man: a one-shot launchd job <label>.rollback (its own
#     plist in ~/Library/LaunchAgents; RunAtLoad, then every StartInterval) and a deadline file. Each run of
#     the job looks once: a check-in from the new executable, no older than the swap → it removes itself; the
#     deadline passed without one → it puts Tulpa.app.prev back, relaunches it, waits up to TULPA_CHECKIN_WAIT
#     seconds for the restored bundle's own check-in, logs whether it came, and removes itself. The
#     job needs neither this script nor Tulpa alive: run through Tulpa's hands, this script dies when the
#     relaunch stops Tulpa, and the job finishes the work. Where the GUI domain refuses to load the job (an
#     ssh session), it is loaded in the user domain instead. While an update is armed or rolling back, a
#     small process of the job holds caffeinate -dimsu -w <its own pid>; launchd's runs are short, so the
#     job's plist abandons its process group and that process ends by itself when the window closes.
#     Then, still before the swap, it stops every Tulpa from the installed bundle that launchd's job does not
#     run (one opened by hand: no relaunch of the job stops it). Run under such a Tulpa (its hands), it
#     refuses before arming instead, since stopping that Tulpa would stop this script;
#  3. renames the installed bundle to Tulpa.app.prev (never deleted; an older .prev waits as
#     Tulpa.app.prev.old and is dropped only after the check-in) and the copy to Tulpa.app. Then, in the
#     app's plist, an older install's KeepAlive = true becomes { SuccessfulExit = false }, for the relaunch
#     to load: launchd starts Tulpa again after a crash or a kill, not after the owner's Quit;
#  4. relaunches the LaunchAgent by registering it with launchd again, from the job's own run (it outlives
#     Tulpa, and this script with it under Tulpa's hands): launchctl bootout, a wait until launchd has let
#     go, then launchctl bootstrap from the app's plist. A kickstart -k is not enough: on a Mac that pinned
#     the old build's cdhash in the job's launch constraint, the new build is killed at its launch
#     (DEVELOP.md). Where launchd's GUI domain refuses the bootout, as it can for an ssh session:
#     kickstart -k, else it stops the pid launchd reports and KeepAlive starts the new bundle (a signal
#     does not end Tulpa with a successful exit). Then it checks that launchd runs a pid from the installed
#     bundle's executable, and logs what launchctl print shows (state, last exit, the launch-constraint
#     lines) when it does not;
#  5. waits up to TULPA_CHECKIN_WAIT seconds for the check-in the app writes once its link's hello is accepted
#     (~/Library/Application Support/tulpa/checkin); on success removes the job and keeps Tulpa.app.prev.
#     Meanwhile it logs launchd's view of the job once if launchd reports a failed spawn or a non-zero exit;
#  6. asserts that launchd runs the app's job with ProcessType = Interactive (launchctl print): without it
#     launchd limits the app like a background job and the microphone capture loses most of its samples.
#     The app's plist is install.command's; this script never sets its ProcessType (step 3 changes only
#     an older install's KeepAlive = true in it).
# Exit status: 0 installed, checked in, ProcessType = Interactive; 1 refused or failed before the swap
# (nothing changed); 2 no check-in in time (the job restores Tulpa.app.prev at the deadline); 3 installed and
# checked in, but the job is not ProcessType = Interactive (the new bundle stays; fix the plist at the Mac).
#
# Knobs (the defaults are the contract; the test harness shortens them):
#   TULPA_LABEL           the app's LaunchAgent label (com.tulpa.app)
#   TULPA_CHECKIN_WAIT    seconds this script waits for the check-in (60)
#   TULPA_ROLLBACK_GRACE  seconds past that before the job rolls back (20)
#   TULPA_ROLLBACK_EVERY  the job's StartInterval in seconds (10)
set -u

label=${TULPA_LABEL:-com.tulpa.app}
wait_s=${TULPA_CHECKIN_WAIT:-60}
grace=${TULPA_ROLLBACK_GRACE:-20}
every=${TULPA_ROLLBACK_EVERY:-10}

app=$HOME/Applications/Tulpa.app
stage=$app.new
prev=$app.prev
old=$app.prev.old
support="$HOME/Library/Application Support/tulpa"
job=$support/rollback.sh
deadline_file=$support/rollback.deadline
agents=$HOME/Library/LaunchAgents
job_plist=$agents/$label.rollback.plist
logdir=$HOME/Library/Logs/tulpa
logfile=$logdir/install.log
uid=$(id -u)
domain=gui/$uid

log() {
	printf '%s update[%s]: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$$" "$*" >>"$logfile" 2>/dev/null
	printf 'update: %s\n' "$*"
}
die() {
	log "refused: $*"
	exit 1
}
utc_at() { date -u -r "$1" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$1" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || printf 'epoch %s' "$1"; }
inode_of() { ls -i "$1" 2>/dev/null | awk '{ print $1 }'; }
xml() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

[ $# -eq 1 ] || {
	printf 'usage: sh update.sh /path/to/new/Tulpa.app\n' >&2
	exit 1
}
new=${1%/}
for n in "$wait_s" "$grace" "$every"; do
	case $n in '' | *[!0-9]*)
		printf 'update: TULPA_CHECKIN_WAIT, TULPA_ROLLBACK_GRACE and TULPA_ROLLBACK_EVERY are whole seconds\n' >&2
		exit 1
		;;
	esac
done
[ "$every" -ge 1 ] || {
	printf 'update: TULPA_ROLLBACK_EVERY is at least 1 second\n' >&2
	exit 1
}
case $label in '' | *[!A-Za-z0-9._-]*)
	printf 'update: TULPA_LABEL must be a launchd label\n' >&2
	exit 1
	;;
esac
mkdir -p "$logdir" "$support" "$agents" || {
	printf 'update: cannot create %s, %s or %s\n' "$logdir" "$support" "$agents" >&2
	exit 1
}

log "start: $new over $app (LaunchAgent $label)"
[ -x "$new/Contents/MacOS/tulpa" ] || die "$new has no executable Contents/MacOS/tulpa"
[ -d "$app" ] || die "no $app to update (a first install is install.command's job)"
case $new in "$app" | "$stage" | "$prev" | "$old") die "$new is a path this script moves; copy it elsewhere first" ;; esac
if [ -e "$deadline_file" ] || [ -e "$deadline_file.restoring" ] || [ -e "$job_plist" ]; then
	armed_until=$(sed -n 's/^deadline=//p' "$deadline_file" 2>/dev/null)
	die "an earlier update is still armed ($deadline_file, $job_plist; deadline ${armed_until:+$(utc_at "$armed_until")}) — read $logfile"
fi

# 0. The keep-awake for the swap, held on this script's own pid: it ends when this script exits, or dies with
# Tulpa at the relaunch (by then the job's own holds the Mac awake, and the new Tulpa holds its own).
caffeinate -dimsu -w $$ >/dev/null 2>&1 &
awake=$!
sleep 1
kill -0 "$awake" 2>/dev/null || die "caffeinate -dimsu -w $$ did not start: the Mac could sleep during the swap"
log "keep-awake for the swap: caffeinate -dimsu -w $$ (caffeinate pid $awake)"

# A leftover from an interrupted run: the copy goes; a two-generations-old bundle goes unless it is the only
# previous one left. A leftover that cannot be removed is a loud stop, never a ditto into what remains: ditto
# merges into an existing directory, so a half copy would become the new bundle.
if [ -e "$stage" ]; then
	rm -rf "$stage" || die "could not remove a leftover $stage (another update running?)"
	log "removed a leftover $stage"
fi
if [ -e "$old" ]; then
	if [ -e "$prev" ]; then
		rm -rf "$old" && log "removed a leftover $old"
	else
		mv "$old" "$prev" && log "a leftover $old is $prev again"
	fi
fi

# 1. The copy, on the same volume as the installed bundle.
ditto "$new" "$stage" || {
	rm -rf "$stage"
	die "could not copy $new to $stage"
}
inode=$(inode_of "$stage/Contents/MacOS/tulpa")
case $inode in '' | *[!0-9]*)
	rm -rf "$stage"
	die "the copy at $stage has no readable executable"
	;;
esac
log "copied $new to $stage (executable inode $inode)"

# 2. The dead-man. Its script is written here, so this file is the only one that has to reach the Mac.
cat >"$job.tmp" <<'JOB'
#!/bin/sh
# Tulpa's update dead-man, written by update.sh. launchd runs it as <label>.rollback (RunAtLoad, then every
# StartInterval); each run looks once and exits:
#   a check-in from the new executable no older than the swap → the new bundle stays; the job removes itself;
#   the deadline passed without one → Tulpa.app.prev is put back and relaunched, the job waits (as long as
#     update.sh waited) for a check-in from the restored executable newer than the restore, logs
#     "rollback: old bundle checked in" or, loudly, "rollback: old bundle did NOT check in", and removes
#     itself either way (it never rolls back twice);
#   otherwise → nothing yet (the next run looks again).
# Whoever acts first claims the decision by renaming the deadline file, so two runners never both act. A
# rollback keeps its claim (rollback.deadline.restoring) until the previous bundle runs again; a run cut
# short is resumed by launchd's next run. The job needs neither update.sh nor Tulpa alive.
# After the swap, update.sh leaves rollback.relaunch and starts the job: the run that takes it relaunches
# the new bundle (see relaunch), so the relaunch outlives Tulpa's hands.
# update.sh calls: under (prints the pid of a Tulpa outside launchd's job that runs the caller; exit 0 when
# there is one), strays (stops the Tulpas outside the job), check SWAP INODE (exit 0 on a good check-in,
# judged on update.sh's own values), settle (the new bundle stays, unless another runner decided first),
# trouble (prints launchd's view of the app's job; exit 0 when it shows a failed spawn or a non-zero exit),
# processtype (prints the app job's process type; exit 0 when Interactive), disarm REASON. By hand: now
# (launchd's job rolls back at once), disarm REASON (keep what is installed), relaunch.
# launchd's runs start `awake`: the job's keep-awake for its window (see ensure_awake).
# Usage: rollback.sh LABEL HOME [check SWAP INODE | settle | relaunch | processtype | strays | under |
#   trouble | disarm REASON | now]
set -u
label=$1 home=$2 mode=${3:-tick}
PATH=$PATH:/usr/bin:/bin:/usr/sbin:/sbin
app=$home/Applications/Tulpa.app
exe=$app/Contents/MacOS/tulpa
prev=$app.prev
old=$app.prev.old
failed=$app.failed
support="$home/Library/Application Support/tulpa"
deadline_file=$support/rollback.deadline
restoring=$deadline_file.restoring
now_flag=$support/rollback.now
relaunch_flag=$support/rollback.relaunch
awake_file=$support/rollback.awake
job_script=$support/rollback.sh
checkin=$support/checkin
job_plist=$home/Library/LaunchAgents/$label.rollback.plist
agent_plist=$home/Library/LaunchAgents/$label.plist
logfile=$home/Library/Logs/tulpa/install.log
uid=$(id -u)
domain=gui/$uid

log() { printf '%s rollback[%s]: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$$" "$*" >>"$logfile" 2>/dev/null; }
field() { printf '%s\n' "$2" | sed -n "s/^$1=//p" | head -n 1; }
inode_of() { ls -i "$1" 2>/dev/null | awk '{ print $1 }'; }

# Read once: other runners rename or remove the file whole. A rollback in progress keeps the state in its claim.
state=$(cat "$deadline_file" 2>/dev/null) || state=$(cat "$restoring" 2>/dev/null) || state=
swap=$(field swap "$state")
deadline=$(field deadline "$state")
inode=$(field inode "$state")
phase=$(field phase "$state")
wait_s=$(field wait "$state")
case $wait_s in '' | *[!0-9]*) wait_s=60 ;; esac
armed=yes
case $swap in '' | *[!0-9]*) armed=no ;; esac
case $deadline in '' | *[!0-9]*) armed=no ;; esac
if [ "$mode" = check ]; then
	swap=${4:-} inode=${5:-} armed=yes
	case $swap in '' | *[!0-9]*) armed=no ;; esac
fi

# A check-in written by a process started from the new executable, no older than the swap; prints its summary.
good_checkin() {
	[ "$armed" = yes ] || return 1
	text=$(cat "$checkin" 2>/dev/null) || return 1
	at=$(field epoch "$text")
	ino=$(field exe_inode "$text")
	case $at in '' | *[!0-9]*) return 1 ;; esac
	if [ -n "$inode" ] && [ "$ino" = "$inode" ]; then
		[ "$at" -ge "$swap" ] || return 1
	elif [ "$ino" = unknown ]; then
		[ "$at" -gt "$swap" ] || return 1
	else
		return 1
	fi
	printf 'version %s, commit %s, at %s' "$(field version "$text")" "$(field commit "$text")" "$(field utc "$text")"
}

# The pid `launchctl print` shows for the app's LaunchAgent, if any.
shown_pid() { launchctl print "$domain/$label" 2>/dev/null | awk '/^\tpid = [0-9]+$/ { print $3; exit }'; }

# The pid launchd runs the app's LaunchAgent as, if it can be read.
app_pid() {
	pid=$(shown_pid)
	[ -n "$pid" ] || pid=$(launchctl list 2>/dev/null | awk -v l="$label" '$3 == l && $1 ~ /^[0-9]+$/ { print $1; exit }')
	[ -n "$pid" ] && printf '%s\n' "$pid"
}

# Tulpa processes from the installed bundle's executable: their command line names its path (a rename of
# the bundle leaves it there). One pid a line.
tulpa_pids() {
	ps -axww -o pid=,command= 2>/dev/null | awk -v exe="$exe" '{
		pid = $1
		sub(/^[ \t]*[0-9]+[ \t]+/, "")
		if (index(" " $0 " ", " " exe " ") > 0) print pid
	}'
}

# This script's pid and its callers', up to launchd.
ancestors() {
	p=$$ n=0
	while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null && [ "$n" -lt 64 ]; do
		printf '%s\n' "$p"
		p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
		n=$((n + 1))
	done
}

# The Tulpas launchd's job does not run (opened by hand, or left from an earlier start): every process from
# the installed executable but the pid `launchctl print` shows for the job. Fails when launchd does not show
# the job: then any of them may be launchd's.
strays() {
	shown=$(launchctl print "$domain/$label" 2>/dev/null) || return 1
	own=$(printf '%s\n' "$shown" | awk '/^\tpid = [0-9]+$/ { print $3; exit }')
	tulpa_pids | grep -vx "${own:-none}"
	return 0
}

# Stops pid $1 ($2 names it in the log): SIGTERM, then SIGKILL after 10 s.
stop_pid() {
	kill -TERM "$1" 2>/dev/null || return 0
	n=0
	while kill -0 "$1" 2>/dev/null && [ "$n" -lt 10 ]; do
		sleep 1
		n=$((n + 1))
	done
	if kill -0 "$1" 2>/dev/null; then
		kill -KILL "$1" 2>/dev/null
		log "$2 (pid $1) ignored SIGTERM for 10 s; killed it"
	fi
}

# Stops every Tulpa outside launchd's job, but one that runs this script (through its hands).
stop_strays() {
	found=$(strays) || {
		log "launchctl print $domain/$label failed: no Tulpa outside launchd's job is looked for"
		return 0
	}
	[ -n "$found" ] || return 0
	mine=$(ancestors)
	for p in $found; do
		if printf '%s\n' "$mine" | grep -qx "$p"; then
			log "a Tulpa outside launchd (pid $p) runs this script; it is not stopped"
			continue
		fi
		started=$(ps -o lstart= -p "$p" 2>/dev/null | sed 's/^ *//')
		stop_pid "$p" "a Tulpa outside launchd"
		log "stopped a Tulpa outside launchd: pid $p${started:+ (started $started)}"
	done
}

# launchd's view of the app's job on one line: the state, pid, runs, last exit, properties and every
# launch-constraint (LWCR) line `launchctl print` shows.
job_excerpt() {
	if ! shown=$(launchctl print "$domain/$label" 2>&1); then
		printf 'launchctl print %s/%s failed: %s' "$domain" "$label" "$(printf '%s\n' "$shown" | head -n 1)"
		return 0
	fi
	printf '%s\n' "$shown" |
		grep -iE '^[[:space:]]*((job )?state|pid|runs|last exit code|last terminating signal|properties)[[:space:]]*=|lwcr' |
		sed 's/^[[:space:]]*//' | tr '\n' ';' | sed -e 's/;$//' -e 's/;/; /g' | cut -c 1-1000
}
spawn_failed() { launchctl print "$domain/$label" 2>/dev/null | grep -qiE '^[[:space:]]*(job )?state[[:space:]]*=[[:space:]]*spawn failed'; }
# 0 when launchd shows the app's job in trouble: a spawn that failed, or a non-zero last exit code.
job_trouble() {
	shown=$(launchctl print "$domain/$label" 2>/dev/null) || return 1
	printf '%s\n' "$shown" | grep -qiE '^[[:space:]]*(job )?state[[:space:]]*=[[:space:]]*spawn failed' && return 0
	code=$(printf '%s\n' "$shown" | sed -n 's/^[[:space:]]*last exit code[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)
	[ -n "$code" ] && [ "$code" -ne 0 ]
}

# After a relaunch: waits (20 s, or the check-in wait when that is shorter) for launchd to run the app's
# job as a pid from the installed executable. Logs that pid, or, loudly, launchd's view of the job.
verify_running() {
	limit=20
	[ "$wait_s" -ge "$limit" ] || limit=$wait_s
	n=0 failed=0
	while :; do
		pid=$(shown_pid)
		if [ -n "$pid" ] && tulpa_pids | grep -qx "$pid"; then
			log "launchd runs $label as pid $pid, from $exe"
			return 0
		fi
		# A spawn launchd reports as failed twice running is not a moment between two states.
		if spawn_failed; then failed=$((failed + 1)); else failed=0; fi
		[ "$failed" -lt 2 ] && [ "$n" -lt "$limit" ] || break
		sleep 1
		n=$((n + 1))
	done
	log "FAILED: launchd does not run $label from $exe after the relaunch: $(job_excerpt)"
	return 1
}

# Registers the app's job with launchd again: bootout, a wait (up to 30 s) until launchd no longer shows
# it and its old pid has ended, then bootstrap from its plist (retried for 10 s while launchd still lets
# go). 0 registered again; 1 not booted out (refused, or still shown after 30 s); 2 booted out, but the
# bootstrap failed: the job is not loaded.
reregister() {
	[ -f "$agent_plist" ] || {
		log "no $agent_plist: $label is not registered again"
		return 1
	}
	if launchctl print "$domain/$label" >/dev/null 2>&1; then
		before=$(shown_pid)
		if ! out=$(launchctl bootout "$domain/$label" 2>&1) && launchctl print "$domain/$label" >/dev/null 2>&1; then
			log "launchctl bootout $domain/$label refused (${out:-no message})"
			return 1
		fi
		n=0
		while launchctl print "$domain/$label" >/dev/null 2>&1; do
			if [ "$n" -ge 30 ]; then
				log "launchd still shows $domain/$label 30 s after its bootout"
				return 1
			fi
			sleep 1
			n=$((n + 1))
		done
		if [ -n "$before" ]; then stop_pid "$before" "$label's old process"; fi
	fi
	n=0
	until out=$(launchctl bootstrap "$domain" "$agent_plist" 2>&1); do
		n=$((n + 1))
		if [ "$n" -ge 10 ]; then
			log "FAILED: launchctl bootstrap $domain $agent_plist, after the bootout: ${out:-no message}"
			return 2
		fi
		sleep 1
	done
	log "relaunched $label (bootout + bootstrap: launchd registered the job again from $agent_plist)"
}

# The process type launchd runs the app's LaunchAgent with, as `launchctl print` shows it. The line's name is
# not documented and may differ between macOS versions ("process type", "ProcessType", "spawn type"), so
# every such line is printed; 0 when one of them says Interactive.
process_type() {
	shown=$(launchctl print "$domain/$label" 2>/dev/null) || {
		printf 'launchctl print %s/%s failed' "$domain" "$label"
		return 1
	}
	types=$(printf '%s\n' "$shown" | grep -iE '^[[:space:]]*(process|spawn)[ _-]?type[[:space:]]*=' | sed 's/^[[:space:]]*//')
	[ -n "$types" ] || {
		printf 'launchctl print %s/%s shows no process type' "$domain" "$label"
		return 1
	}
	printf '%s' "$types" | tr '\n' ';' | sed 's/;/; /g'
	printf '%s\n' "$types" | grep -qiE '=[[:space:]]*interactive'
}
# What launchd's run logs about it once the new bundle checked in (update.sh, alive, asserts it itself).
note_process_type() {
	if types=$(process_type); then
		log "$label runs with ProcessType = Interactive ($types)"
	else
		log "FAILED: $label does not run with ProcessType = Interactive ($types): the microphone capture loses samples; set it in $agent_plist and reload the job at the Mac"
	fi
}

# Restarts the app's LaunchAgent from whatever bundle is installed now, so that launchd runs it:
#  - a Tulpa outside the job is stopped first;
#  - where launchd loaded this job in its GUI domain (so a bootstrap there works from here), the app's job
#    is registered again (reregister). kickstart -k alone keeps the launch constraint launchd recorded for
#    the job, and on a Mac where that pins the old build's cdhash the new build is killed at its launch;
#  - otherwise, or when the bootout is refused: kickstart -k; where the GUI domain refuses that too (an
#    ssh session), the pid launchd reports is stopped — SIGTERM, then SIGKILL after 10 s — and KeepAlive
#    starts the app again; that counts only once launchd reports a new pid;
#  - a job that is not loaded is bootstrapped; when even that fails, the app is opened outside launchd,
#    loudly, so that Tulpa runs.
# Then verify_running logs whether launchd runs the installed executable.
relaunch() {
	stop_strays
	if launchctl print "$domain/$label.rollback" >/dev/null 2>&1; then
		if reregister; then
			verify_running
			return 0
		fi
	else
		log "$label.rollback is not loaded in $domain, so a bootstrap there may be refused from here (an ssh session?): $label is not registered again, and launchd keeps the launch constraint it has"
	fi
	if launchctl kickstart -k "$domain/$label" >/dev/null 2>&1; then
		log "relaunched $label (launchctl kickstart -k)"
		verify_running
		return 0
	fi
	if pid=$(app_pid); then
		stop_pid "$pid" "$label"
		n=0
		while [ "$n" -lt 20 ]; do
			new=$(app_pid) && [ "$new" != "$pid" ] && {
				log "stopped $label's pid $pid (kickstart refused); launchd started it again as pid $new"
				verify_running
				return 0
			}
			sleep 1
			n=$((n + 1))
		done
		log "stopped $label's pid $pid, but launchd has not started it again within 20 s"
		return 1
	fi
	if [ -f "$agent_plist" ] && launchctl bootstrap "$domain" "$agent_plist" >/dev/null 2>&1; then
		log "loaded $agent_plist ($label was not loaded)"
		verify_running
	elif [ ! -f "$agent_plist" ] && open -g "$app" >/dev/null 2>&1; then
		log "opened $app (no LaunchAgent $label)"
	elif open -g "$app" >/dev/null 2>&1; then
		log "FAILED: launchd does not take $agent_plist; opened $app outside launchd so that Tulpa runs. Register the job again at the Mac (launchctl bootstrap $domain $agent_plist)"
	else
		log "could not relaunch $app (kickstart refused, no pid to stop)"
		return 1
	fi
}

# update.sh's request to relaunch the new bundle (rollback.relaunch): one run takes it (renamed) and does it.
take_relaunch() {
	mv "$relaunch_flag" "$relaunch_flag.$$" 2>/dev/null || return 0
	log "relaunching $label for update.sh"
	relaunch || log "relaunch failed; the job decides at the deadline"
	rm -f "$relaunch_flag.$$"
}

# The job's keep-awake for its window: launchd's runs are short, so a process of the job outlives them (the
# job's plist abandons its process group) and holds caffeinate -dimsu -w <its own pid> while an update is
# armed or rolling back. Started by launchd's runs unless one is alive; waits until it holds.
ensure_awake() {
	held=$(cat "$awake_file" 2>/dev/null) && kill -0 "$held" 2>/dev/null && return 0
	/bin/sh "$job_script" "$label" "$home" awake </dev/null >/dev/null 2>&1 &
	n=0
	until [ -s "$awake_file" ] || [ "$n" -ge 3 ]; do
		sleep 1
		n=$((n + 1))
	done
}
window_open() { [ -e "$deadline_file" ] || [ -e "$restoring" ]; }

# Takes the decision by renaming the deadline file to $1; fails when another runner took it first.
claim() { mv "$deadline_file" "$1" 2>/dev/null; }

# The job's end: the claim $1, the plist and the by-hand flag go first, then launchd forgets the job (when
# launchd runs this, that also ends this run).
finish() {
	rm -f "$1" "$job_plist" "$now_flag" "$relaunch_flag" "$relaunch_flag".*
	log "$2; rollback job removed"
	launchctl bootout "$domain/$label.rollback" >/dev/null 2>&1
	launchctl bootout "user/$uid/$label.rollback" >/dev/null 2>&1
	exit 0
}

# Puts the bundle from before the swap back as Tulpa.app: 0 put back, 1 failed (the next run tries again),
# 2 nothing to put back.
restore() {
	if [ -n "$inode" ] && [ "$(inode_of "$app/Contents/MacOS/tulpa")" = "$inode" ]; then
		[ -d "$prev" ] || {
			log "no $prev to put back; the new bundle stays"
			return 2
		}
		if [ -e "$failed" ]; then rm -rf "$failed"; fi
		mv "$app" "$failed" || return 1
		log "moved the new bundle aside as $failed"
		mv "$prev" "$app" || {
			mv "$failed" "$app"
			return 1
		}
		log "restored $prev as $app"
	elif [ ! -e "$app" ] && [ -d "$prev" ]; then
		mv "$prev" "$app" || return 1
		log "restored $prev as $app (the swap had stopped halfway)"
	else
		log "the swap never happened: $app is the bundle from before"
		return 2
	fi
	if [ ! -e "$prev" ] && [ -d "$old" ]; then mv "$old" "$prev" && log "the older bundle is $prev again"; fi
	return 0
}

# Records in the $restoring claim how far the rollback got and when: phase=relaunch with restored=<the
# restore's time>, then phase=checkin with relaunched=<the relaunch's>. A run cut short resumes there.
set_phase() {
	state=$(printf '%s\n' "$state" | grep -v -e '^phase=' -e "^$2=")
	state=$(printf '%s\nphase=%s\n%s=%s' "$state" "$1" "$2" "$(date +%s)")
	phase=$1
	printf '%s\n' "$state" >"$restoring.tmp" && mv "$restoring.tmp" "$restoring"
}

# After the relaunch: waits up to wait_s seconds from the relaunch for a check-in from the restored
# executable (by inode) no older than the restore. Logs the outcome either way; nothing is moved or retried.
await_old_checkin() {
	restored=$(field restored "$state")
	relaunched=$(field relaunched "$state")
	case $restored in '' | *[!0-9]*) restored=$(date +%s) ;; esac
	case $relaunched in '' | *[!0-9]*) relaunched=$(date +%s) ;; esac
	swap=$restored
	inode=$(inode_of "$app/Contents/MacOS/tulpa")
	until_at=$((relaunched + wait_s))
	while :; do
		if summary=$(good_checkin); then
			log "rollback: old bundle checked in ($summary)"
			return 0
		fi
		[ "$(date +%s)" -lt "$until_at" ] || break
		sleep 1
	done
	log "rollback: old bundle did NOT check in within $wait_s s of its relaunch — $app is the bundle from before the update but has not linked; all bundles are kept ($app, $prev, $failed) and the job does not try again: look at the Mac and $logfile"
	return 1
}

# Holding the $restoring claim: puts the previous bundle back, relaunches it and waits for its check-in. A
# failed rename or relaunch keeps the claim and the job, and launchd's next run resumes from where this one
# stopped; a missing check-in is only reported.
roll_back() {
	if [ "$phase" != relaunch ] && [ "$phase" != checkin ]; then
		restore
		case $? in
		1)
			log "putting $prev back failed; the next run tries again"
			exit 1
			;;
		2) finish "$restoring" "rolled back nothing" ;;
		esac
		set_phase relaunch restored
	fi
	if [ "$phase" != checkin ]; then
		relaunch || {
			log "$app is the bundle from before the update but is not running yet; the next run tries the relaunch again"
			exit 1
		}
		set_phase checkin relaunched
	fi
	if await_old_checkin; then
		finish "$restoring" "rolled back: $app is the bundle from before the update again, and it checked in"
	fi
	finish "$restoring" "rolled back: $app is the bundle from before the update again, but it did NOT check in"
}

# The new bundle checked in: it stays, unless another runner decided first.
settle() {
	summary=$(good_checkin) || return 1
	claim "$deadline_file.done.$$" || exit 0
	if [ -e "$old" ]; then rm -rf "$old" && log "dropped $old: one previous generation is kept"; fi
	if [ "$mode" = tick ]; then note_process_type; fi
	finish "$deadline_file.done.$$" "check-in seen ($summary): the new bundle stays, $prev kept"
}

case $mode in
check)
	good_checkin
	exit
	;;
settle)
	settle
	exit 1
	;;
relaunch)
	relaunch
	exit
	;;
processtype)
	process_type
	exit
	;;
strays)
	stop_strays
	exit 0
	;;
under)
	found=$(strays) || exit 1
	mine=$(ancestors)
	for p in $found; do
		if printf '%s\n' "$mine" | grep -qx "$p"; then
			printf '%s\n' "$p"
			exit 0
		fi
	done
	exit 1
	;;
trouble)
	job_excerpt
	job_trouble
	exit
	;;
awake)
	window_open || exit 0
	caffeinate -dimsu -w $$ >/dev/null 2>&1 &
	held=$!
	printf '%s\n' "$$" >"$awake_file.tmp" && mv "$awake_file.tmp" "$awake_file"
	log "keep-awake for the job's window: caffeinate -dimsu -w $$ (caffeinate pid $held)"
	sleep 1
	if ! kill -0 "$held" 2>/dev/null; then
		rm -f "$awake_file"
		log "FAILED: keep-awake for the job's window: caffeinate -dimsu -w $$ did not start; the Mac may sleep until Tulpa holds its own (the next run tries again)"
		exit 1
	fi
	# At most 15 min a holder; launchd's next run starts another while the window is still open.
	n=0
	while window_open && [ "$n" -lt 900 ]; do
		sleep 1
		n=$((n + 1))
	done
	if [ "$(cat "$awake_file" 2>/dev/null)" = "$$" ]; then rm -f "$awake_file"; fi
	log "keep-awake for the job's window ended"
	exit 0
	;;
disarm)
	claim "$deadline_file.disarmed.$$" && finish "$deadline_file.disarmed.$$" "${4:-disarmed}"
	if [ -e "$restoring" ]; then
		log "${4:-disarm}: refused, a rollback is in progress and finishes by itself"
		exit 1
	fi
	finish "$deadline_file" "${4:-disarmed}: nothing was armed"
	;;
now)
	[ "$armed" = yes ] && [ -e "$deadline_file" ] || {
		log "rollback by hand: nothing armed ($deadline_file)"
		exit 1
	}
	# launchd's own job rolls back: one restorer at a time, and it outlives Tulpa's hands.
	: >"$now_flag"
	log "rollback by hand: launchd's job rolls back now"
	launchctl kickstart "$domain/$label.rollback" >/dev/null 2>&1 ||
		launchctl kickstart "user/$uid/$label.rollback" >/dev/null 2>&1 ||
		launchctl bootstrap "$domain" "$job_plist" >/dev/null 2>&1 ||
		launchctl bootstrap "user/$uid" "$job_plist" >/dev/null 2>&1 ||
		log "could not start the job now; it rolls back at its next run"
	exit 0
	;;
tick) ;;
*)
	log "unknown mode $mode"
	exit 64
	;;
esac

# launchd's run. A rollback cut short (a logout, a reboot, a kill) is resumed: its decision was made.
if [ -n "$state" ]; then ensure_awake; fi
if [ -e "$restoring" ] && [ ! -e "$deadline_file" ]; then
	log "resuming the rollback an earlier run began"
	roll_back
fi
[ -n "$state" ] || finish "$deadline_file" "nothing armed (no $deadline_file)"
if [ "$armed" != yes ]; then
	claim "$deadline_file.bad.$$" || exit 0
	finish "$deadline_file.bad.$$" "unreadable $deadline_file: nothing moved"
fi
settle
if [ "$(date +%s)" -lt "$deadline" ] && [ ! -e "$now_flag" ]; then
	take_relaunch
	exit 0
fi
claim "$restoring" || exit 0
if [ -e "$now_flag" ]; then
	log "rollback by hand; putting $prev back"
else
	log "no check-in from the new bundle by the deadline; putting $prev back. launchd shows: $(job_excerpt)"
fi
roll_back
JOB
chmod 0755 "$job.tmp" && mv "$job.tmp" "$job" || {
	rm -rf "$stage" "$job.tmp"
	die "could not write $job"
}
# Run through the hands of a Tulpa outside launchd's job, this script would stop its own caller below.
if under=$(/bin/sh "$job" "$label" "$HOME" under); then
	rm -rf "$stage"
	die "this script runs under a Tulpa outside launchd (pid $under), which the update stops before the swap. Run it from a Terminal or ssh, or have launchd run Tulpa first (REHEARSAL.md); nothing was changed"
fi
if types=$(/bin/sh "$job" "$label" "$HOME" processtype); then
	log "the running $label: ProcessType = Interactive ($types)"
else
	log "the running $label is not ProcessType = Interactive ($types); the new bundle runs the same way, and this script exits 3 after the swap"
fi

rm -f "$support/rollback.now" "$support/rollback.relaunch"
swap=$(date +%s)
deadline=$((swap + wait_s + grace))
printf 'swap=%s\ndeadline=%s\ninode=%s\nwait=%s\n' "$swap" "$deadline" "$inode" "$wait_s" >"$deadline_file.tmp" &&
	mv "$deadline_file.tmp" "$deadline_file" || {
	rm -rf "$stage" "$deadline_file.tmp" "$deadline_file"
	die "could not write $deadline_file"
}
plist_tmp=$support/rollback.plist.tmp
cat >"$plist_tmp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$(xml "$label.rollback")</string>
  <key>ProgramArguments</key><array>
    <string>/bin/sh</string>
    <string>$(xml "$job")</string>
    <string>$(xml "$label")</string>
    <string>$(xml "$HOME")</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StartInterval</key><integer>$every</integer>
  <key>AbandonProcessGroup</key><true/>
  <key>StandardOutPath</key><string>$(xml "$logfile")</string>
  <key>StandardErrorPath</key><string>$(xml "$logfile")</string>
</dict></plist>
PLIST
written=$?
# launchd refuses a LaunchAgent plist that others can write; the caller's umask may allow that.
if [ "$written" -ne 0 ] || ! chmod 0644 "$plist_tmp" || ! mv "$plist_tmp" "$job_plist"; then
	rm -rf "$stage" "$deadline_file" "$plist_tmp"
	die "could not write $job_plist"
fi
launchctl bootout "$domain/$label.rollback" >/dev/null 2>&1
launchctl bootout "user/$uid/$label.rollback" >/dev/null 2>&1
if launchctl bootstrap "$domain" "$job_plist" 2>/dev/null; then
	job_domain=$domain
elif launchctl bootstrap "user/$uid" "$job_plist"; then
	job_domain=user/$uid
	log "launchd's GUI domain refused the job (an ssh session?); it runs in user/$uid"
else
	rm -rf "$stage" "$deadline_file" "$job_plist"
	die "launchctl could not load $job_plist in $domain or user/$uid; nothing was swapped"
fi
log "armed: $label.rollback ($job_domain) puts Tulpa.app.prev back at $(utc_at "$deadline") unless the new bundle checks in"

# A Tulpa outside launchd's job (opened by hand) would outlive the relaunch: it goes before the swap.
/bin/sh "$job" "$label" "$HOME" strays

# 3. The swap: renames only, nothing deleted. A failed rename puts back what moved and disarms the job.
undo() {
	log "swap failed: $*; putting things back"
	[ -e "$app" ] || [ ! -e "$prev" ] || mv "$prev" "$app"
	[ -e "$prev" ] || [ ! -e "$old" ] || mv "$old" "$prev"
	rm -rf "$stage"
	/bin/sh "$job" "$label" "$HOME" disarm "the swap did not happen"
	exit 1
}
if [ -e "$prev" ]; then
	mv "$prev" "$old" || undo "could not move $prev aside"
	log "the older $prev waits as $old until the check-in"
fi
mv "$app" "$prev" || undo "could not rename $app"
log "renamed the running bundle to $prev"
mv "$stage" "$app" || undo "could not rename $stage"
log "installed the new bundle as $app"

# The app's plist: KeepAlive = true, as an older install.command wrote it, has launchd start Tulpa again after
# the owner's Quit. { SuccessfulExit = false } starts it again only after an end that is not a successful
# exit (a crash, a kill), not after Quit's exit 0. Changed after the swap, so that the relaunch loads it; any
# other KeepAlive is left as it is. plutil may write the file anew, and launchd refuses one others can write.
app_plist=$agents/$label.plist
if [ "$(plutil -extract KeepAlive raw -o - "$app_plist" 2>/dev/null)" = true ]; then
	if plutil -replace KeepAlive -xml '<dict><key>SuccessfulExit</key><false/></dict>' "$app_plist" >/dev/null 2>&1 &&
		chmod go-w "$app_plist"; then
		log "KeepAlive = true in $app_plist is now { SuccessfulExit = false }: launchd starts Tulpa again after a crash, not after Quit"
	else
		log "could not change KeepAlive = true in $app_plist: launchd still starts Tulpa again after Quit"
	fi
fi

# 4. The relaunch, by the job's own run: its bootout stops Tulpa, and under Tulpa's hands this script with
# it; the job finishes.
log "relaunching $label: $label.rollback registers it with launchd again"
if { : >"$support/rollback.relaunch"; } 2>/dev/null; then
	launchctl kickstart "$job_domain/$label.rollback" >/dev/null 2>&1 ||
		log "launchctl kickstart $job_domain/$label.rollback failed; the job's next run, within $every s, relaunches $label"
else
	log "could not write $support/rollback.relaunch; relaunching from here"
	/bin/sh "$job" "$label" "$HOME" relaunch || log "relaunch failed; the job decides at the deadline"
fi

# 5. The check-in.
until=$((swap + wait_s))
told=
while :; do
	if summary=$(/bin/sh "$job" "$label" "$HOME" check "$swap" "$inode"); then
		# The job keeps the new bundle and removes itself, unless launchd's run decided first.
		if [ -e "$deadline_file" ]; then /bin/sh "$job" "$label" "$HOME" settle; fi
		if [ -e "$deadline_file.restoring" ] || [ "$(inode_of "$app/Contents/MacOS/tulpa")" != "$inode" ]; then
			log "outcome: the new bundle checked in ($summary) only after the rollback began — the job puts $prev back"
			exit 2
		fi
		log "outcome: installed — the new bundle checked in ($summary); the previous bundle is kept as $prev"
		# 6. The process type: launchd reads it when it loads the job, as the relaunch did (but a kickstart).
		if types=$(/bin/sh "$job" "$label" "$HOME" processtype); then
			log "$label runs with ProcessType = Interactive ($types)"
			exit 0
		fi
		log "FAILED: $label does not run with ProcessType = Interactive ($types). launchd limits it like a background job and the microphone capture loses most of its samples. The new bundle stays; add ProcessType = Interactive to $agents/$label.plist and reload the job at the Mac (never through Tulpa's own hands)"
		exit 3
	fi
	if [ -z "$told" ] && shown=$(/bin/sh "$job" "$label" "$HOME" trouble); then
		log "launchd reports a failed spawn or exit of $label, and no check-in yet: $shown"
		told=yes
	fi
	[ "$(date +%s)" -lt "$until" ] || break
	sleep 1
done
if [ -e "$deadline_file" ] || [ -e "$deadline_file.restoring" ]; then
	log "outcome: no check-in from the new bundle within ${wait_s} s — the rollback job puts $prev back at $(utc_at "$deadline")"
else
	log "outcome: no check-in from the new bundle within ${wait_s} s, and the rollback job is no longer armed — read $logfile"
fi
exit 2
