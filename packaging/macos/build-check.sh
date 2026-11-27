#!/bin/sh
# build-check.sh — at every login, keeps the installed Tulpa on the latest good build.
#
#   sh build-check.sh                                      run the check once (the agent's run)
#   TULPA_CHECK_ORIGIN=<git-url> sh build-check.sh --install   install the agent on this Mac, run once
#
# Runs on the Mac as the user, unattended: no prompts, no questions, everything to
# ~/Library/Logs/tulpa/build-check.log. The check compares the installed bundle's commit marker
# (Contents/Resources/build, written by scripts/bundle-macos.sh) with refs/tags/latest-good on the origin —
# the ref the operator moves after each install verified on every Mac — and on a mismatch installs
# the staged bundle that carries the latest-good commit, through update.sh, unattended:
#  1. the installed bundle's marker; a bundle without one (the pre-marker builds) reads `unknown`,
#     which never equals latest-good, so the first check after this ships installs;
#  2. latest-good with `git ls-remote` (BatchMode ssh, 10 s to connect; an annotated tag is peeled
#     through `^{}`). No origin, or a failed lookup: `failed-origin` / `failed-ls-remote`;
#  3. commit = latest-good → outcome `ok`, nothing else happens;
#  4. otherwise the newest staged bundle (TULPA_STAGING, newest first) whose own marker carries
#     latest-good goes through TULPA_UPDATE with the bundle as its argument. rc 0 or 3 → `updated`
#     (3: installed, the app's plist wants a look — install.log says); any other rc →
#     `failed-install` (update.sh's own rollback job restores the previous bundle). An install
#     over a commit the previous marker did not record as running is `reverted` instead: a build
#     was installed by hand between checks, and the check put latest-good over it (whether or not
#     latest-good moved meanwhile) — said loudly in this log as a REVERTED line, never silently.
#     Both carry `from=`, the commit that was running before the install;
#  5. no staged bundle with latest-good, or no update.sh: `failed-staging`.
# Every outcome is written to the marker the tray watches (~/Library/Application Support/tulpa/
# build-check), atomically; the tray speaks it to the agent as a link `build-check` event when its
# meaning changes — a Mac that fell behind, or failed to catch up, becomes visible without anyone
# reading this log. `--install` copies this script beside that marker, writes the LaunchAgent
# (RunAtLoad, then every TULPA_CHECK_EVERY) with the caller's origin baked into its environment —
# the origin names a private host, so it lives in the installed plist, never in this repository.
# Only one check runs at a time (a directory lock beside the marker): a run that finds a live one
# exits 0 without touching the outcome — the running check's outcome is the answer.
#
# Exit status: 0 the check ran (the marker carries the outcome, whatever it was); 1 it could not
# even run, or --install failed; --install ends with the first check's exit status.
#
# Knobs (the defaults are the contract; the test harness shortens them):
#   TULPA_CHECK_ORIGIN  the git URL latest-good lives at (no default; --install bakes it in)
#   TULPA_STAGING       where staged bundles wait (~/Library/Application Support/tulpa/staging)
#   TULPA_UPDATE        the update.sh to install with ($TULPA_STAGING/update.sh)
#   TULPA_CHECK_EVERY   the agent's StartInterval in seconds (3600)
#   TULPA_LOCK_RECHECK  seconds a lock with no readable holder gets before takeover (1)
#   TULPA_LABEL         the app's LaunchAgent label, passed through to update.sh (com.tulpa.app)
set -u

label=${TULPA_LABEL:-com.tulpa.app}
every=${TULPA_CHECK_EVERY:-3600}
app=$HOME/Applications/Tulpa.app
support="$HOME/Library/Application Support/tulpa"
staging=${TULPA_STAGING:-$support/staging}
update=${TULPA_UPDATE:-$staging/update.sh}
agents=$HOME/Library/LaunchAgents
agent=com.tulpa.build-check
agent_plist=$agents/$agent.plist
installed=$support/build-check.sh
marker=$support/build-check
logdir=$HOME/Library/Logs/tulpa
logfile=$logdir/build-check.log
uid=$(id -u)
domain=gui/$uid

log() {
	printf '%s build-check[%s]: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$$" "$*" >>"$logfile" 2>/dev/null
	printf 'build-check: %s\n' "$*"
}
xml() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }
word() {
	case $1 in
	'' | *[!A-Za-z0-9._+-]*) printf 'unknown' ;;
	*) printf '%s' "$1" ;;
	esac
}
# The marker the tray reads; atomic, so a reader never sees half of it. Ends the run. A fourth
# argument, the commit running before an install, adds a `from=` line.
finish() {
	outcome=$(word "$1")
	commit=$(word "$2")
	latest=$(word "$3")
	mkdir -p "$support" 2>/dev/null
	if {
		printf 'outcome=%s\ncommit=%s\nlatest=%s\n' "$outcome" "$commit" "$latest" &&
			{ [ $# -lt 4 ] || printf 'from=%s\n' "$(word "$4")"; } &&
			printf 'utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
	} >"$marker.tmp" && mv "$marker.tmp" "$marker"; then
		:
	else
		log "FAILED: could not write $marker"
	fi
	log "outcome $outcome (commit $commit, latest-good $latest)"
	exit 0
}
marker_commit_of() {
	sed -n 's/^commit=//p' "$1/Contents/Resources/build" 2>/dev/null | head -n 1
}

if [ "${1:-}" = "--install" ]; then
	[ $# -eq 1 ] || { printf 'usage: TULPA_CHECK_ORIGIN=<git-url> sh build-check.sh --install\n' >&2; exit 1; }
	[ -n "${TULPA_CHECK_ORIGIN:-}" ] || {
		printf 'build-check: TULPA_CHECK_ORIGIN is the git URL latest-good lives at; set it for --install\n' >&2
		exit 1
	}
	case $every in
	'' | *[!0-9]*) printf 'build-check: TULPA_CHECK_EVERY is whole seconds\n' >&2; exit 1 ;;
	esac
	[ "$every" -ge 60 ] || { printf 'build-check: TULPA_CHECK_EVERY is at least a minute\n' >&2; exit 1; }
	mkdir -p "$support" "$agents" "$logdir" "$staging" || {
		printf 'build-check: cannot create %s, %s or %s\n' "$support" "$agents" "$logdir" >&2
		exit 1
	}
	cp "$0" "$installed.tmp" && chmod 0755 "$installed.tmp" && mv "$installed.tmp" "$installed" || {
		printf 'build-check: could not write %s\n' "$installed" >&2
		exit 1
	}
	plist_tmp=$support/build-check.plist.tmp
	cat >"$plist_tmp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$agent</string>
  <key>ProgramArguments</key><array>
    <string>/bin/sh</string>
    <string>$(xml "$installed")</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>TULPA_CHECK_ORIGIN</key><string>$(xml "$TULPA_CHECK_ORIGIN")</string>
    <key>TULPA_LABEL</key><string>$(xml "$label")</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>StartInterval</key><integer>$every</integer>
  <key>StandardOutPath</key><string>$(xml "$logfile")</string>
  <key>StandardErrorPath</key><string>$(xml "$logfile")</string>
</dict></plist>
PLIST
	# launchd refuses a LaunchAgent plist that others can write; the caller's umask may allow that.
	if ! chmod 0644 "$plist_tmp" || ! mv "$plist_tmp" "$agent_plist"; then
		rm -f "$plist_tmp"
		printf 'build-check: could not write %s\n' "$agent_plist" >&2
		exit 1
	fi
	launchctl bootout "$domain/$agent" >/dev/null 2>&1
	launchctl bootout "user/$uid/$agent" >/dev/null 2>&1
	if launchctl bootstrap "$domain" "$agent_plist" 2>/dev/null; then
		:
	elif launchctl bootstrap "user/$uid" "$agent_plist"; then
		log "launchd's GUI domain refused the agent (an ssh session?); it runs in user/$uid"
	else
		printf 'build-check: launchd could not load %s\n' "$agent_plist" >&2
		exit 1
	fi
	printf 'build-check: %s installed (RunAtLoad, then every %s s); first check:\n' "$agent_plist" "$every"
	export TULPA_CHECK_ORIGIN TULPA_LABEL
	exec /bin/sh "$installed"
fi

mkdir -p "$logdir" "$support" 2>/dev/null || {
	printf 'build-check: cannot create %s or %s\n' "$logdir" "$support" >&2
	exit 1
}
log "start (pid $$)"

# One check at a time: --install's own first run lands in the same second as the agent's RunAtLoad,
# and both would run update.sh on the same staged bundle. The holder leaves "<pid> <epoch>" in the
# lock; a later run leaves the outcome to a live holder, and takes over a lock whose holder is gone
# or older than two intervals (a recycled pid must never wedge the check). A holder that reads as
# missing or unparsable gets one recheck window before the lock is called dead: the holder may be
# between its mkdir and its first write, and taking over in that gap is how two checks run at once.
lock=$support/build-check.lock
lock_stale_s=$((every * 2))
recheck_s=${TULPA_LOCK_RECHECK:-1}
case $recheck_s in '' | *[!0-9]*) recheck_s=1 ;; esac
lock_read() {
	holder_pid=0
	holder_at=0
	read -r holder_pid holder_at <"$lock/holder" 2>/dev/null || true
	case $holder_pid in '' | *[!0-9]*) holder_pid=0 ;; esac
	case $holder_at in '' | *[!0-9]*) holder_at=0 ;; esac
}
while ! mkdir "$lock" 2>/dev/null; do
	lock_read
	if [ "$holder_pid" -eq 0 ]; then
		sleep "$recheck_s"
		lock_read
	fi
	now=$(date +%s)
	if [ "$holder_pid" -ne 0 ] && kill -0 "$holder_pid" 2>/dev/null &&
		[ $((holder_at + lock_stale_s)) -gt "$now" ]; then
		log "check pid $holder_pid is already running; its outcome is the answer"
		exit 0
	fi
	rm -rf "$lock" 2>/dev/null || {
		log "could not remove a stale $lock; leaving the outcome to its holder"
		exit 0
	}
done
printf '%s %s\n' "$$" "$(date +%s)" >"$lock/holder" 2>/dev/null
trap 'rm -rf "$lock" 2>/dev/null' EXIT

[ -d "$app" ] || finish failed-marker unknown unknown
commit=$(word "$(marker_commit_of "$app")")

[ -n "${TULPA_CHECK_ORIGIN:-}" ] || finish failed-origin "$commit" unknown
refs=$(GIT_SSH_COMMAND='ssh -o BatchMode=yes -o ConnectTimeout=10' \
	git ls-remote "$TULPA_CHECK_ORIGIN" 'refs/tags/latest-good^{}' refs/tags/latest-good 2>>"$logfile") ||
	finish failed-ls-remote "$commit" unknown
latest=$(printf '%s\n' "$refs" | sed -n 's/^\([0-9a-f]\{40\}\)[[:space:]]refs\/tags\/latest-good\^{}$/\1/p' | head -n 1)
[ -n "$latest" ] ||
	latest=$(printf '%s\n' "$refs" | sed -n 's/^\([0-9a-f]\{40\}\)[[:space:]]refs\/tags\/latest-good$/\1/p' | head -n 1)
[ -n "$latest" ] || finish failed-ls-remote "$commit" unknown

if [ "$commit" = "$latest" ]; then
	finish ok "$commit" "$latest"
fi

found=
while IFS= read -r staged; do
	[ -n "$staged" ] || continue
	[ -x "$staged/Contents/MacOS/tulpa" ] || continue
	staged_commit=$(word "$(marker_commit_of "$staged")")
	if [ "$staged_commit" = "$latest" ]; then
		found=$staged
		break
	fi
done <<EOF
$(ls -dt "$staging"/Tulpa.app* 2>/dev/null)
EOF
[ -n "$found" ] || finish failed-staging "$commit" "$latest"
[ -f "$update" ] || finish failed-staging "$commit" "$latest"

# The commit the previous marker saw running, before this run replaces it: if something else is
# running now (and it is not latest-good), it was installed by hand between checks, and this
# install reverts it. A retry after failed-staging or failed-install runs what that marker saw.
previous_commit=$(word "$(sed -n 's/^commit=//p' "$marker" 2>/dev/null | head -n 1)")
from=$commit

log "running $commit → $latest: update.sh $found"
if sh "$update" "$found" >>"$logfile" 2>&1; then
	rc=0
else
	rc=$?
fi
case $rc in
0 | 3)
	now=$(word "$(marker_commit_of "$app")")
	if [ "$previous_commit" != unknown ] && [ "$from" != unknown ] &&
		[ "$from" != "$previous_commit" ] && [ "$from" != "$latest" ]; then
		log "REVERTED: $from was running but is not latest-good $latest; installed latest-good"
		finish reverted "$now" "$latest" "$from"
	fi
	finish updated "$now" "$latest" "$from"
	;;
*)
	finish failed-install "$commit" "$latest"
	;;
esac
