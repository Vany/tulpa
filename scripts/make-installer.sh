#!/usr/bin/env bash
# Builds a self-contained installer zip for one more Mac: Tulpa.app (signed by bundle-macos.sh), that Mac's
# config with its own link token, an ssh config entry and the server's host keys, and install.command, which
# the new Mac's owner double-clicks once. Everything about the server is passed in here, nothing is committed.
#
# Usage: scripts/make-installer.sh --device NAME --alias SSH_ALIAS --server-host HOST --server-user USER
#          --known-hosts FILE [--server-port PORT] [--jump JUMP_ALIAS_OR_USER@HOST] [--target-port 18031]
#          [--office-ssid NAME]... [--direct-host HOST]... [--full] [--out DIR]
#   --device       this Mac's name on the link (the server's token file must be <device>.token)
#   --alias        the ssh Host alias Tulpa dials (written into ~/.ssh/config on the new Mac if absent)
#   --server-host  the server's HostName; --server-user its tulpa-door user; --server-port its sshd port
#   --known-hosts  a file with the server's (and jump host's) known_hosts lines
#   --jump         ProxyJump for the alias (off the office network)
#   --office-ssid / --direct-host   the direct route on the office network (repeatable)
#   --full         the hands start in full mode (setting the Mac up through the link)
# Output: <out>/tulpa-<device>.zip and <out>/<device>.token (copy it to the server's devices dir, 0600).
set -euo pipefail
die() { printf 'make-installer: %s\n' "$*" >&2; exit 1; }

device="" alias="" host="" user="" port=22 known="" jump="" tport=18031 full=false out=""
ssids=() directs=()
while [ $# -gt 0 ]; do
	case $1 in
	--device) device=$2; shift 2 ;;
	--alias) alias=$2; shift 2 ;;
	--server-host) host=$2; shift 2 ;;
	--server-user) user=$2; shift 2 ;;
	--server-port) port=$2; shift 2 ;;
	--known-hosts) known=$2; shift 2 ;;
	--jump) jump=$2; shift 2 ;;
	--target-port) tport=$2; shift 2 ;;
	--office-ssid) ssids+=("$2"); shift 2 ;;
	--direct-host) directs+=("$2"); shift 2 ;;
	--full) full=true; shift ;;
	--out) out=$2; shift 2 ;;
	*) die "unknown argument $1" ;;
	esac
done
[[ $device =~ ^[A-Za-z0-9_-]{1,64}$ ]] || die "--device: letters, digits, _ or -"
[[ $alias =~ ^[A-Za-z0-9._-]+$ ]] || die "--alias: an ssh Host alias"
[ -n "$host" ] && [ -n "$user" ] || die "--server-host and --server-user are required"
[ -f "$known" ] || die "--known-hosts: a file of the server's known_hosts lines"

# BatchMode refuses an unknown host key, so the direct route only works if known_hosts has every
# LAN name it dials (and the server itself for the jump route).
for h in "$host" "${directs[@]+"${directs[@]}"}"; do
	if [ "$port" = 22 ]; then hk=$h; else hk="[$h]:$port"; fi
	ssh-keygen -F "$hk" -f "$known" >/dev/null || die "--known-hosts has no key for $hk (the route to it would fail under BatchMode); add it: ssh-keyscan -p $port $h"
done

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=${out:-$repo_root/target/installer}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pack=$stage/tulpa-$device
mkdir -p "$pack"

TULPA_BUNDLE_DIR=$pack "$repo_root/scripts/bundle-macos.sh"

umask 077
token=$(head -c 32 /dev/urandom | base64 | tr -d '\n/+=' | head -c 40)
printf '%s\n' "$token" >"$pack/token"
mkdir -p "$out"
printf '%s\n' "$token" >"$out/$device.token"

toml_list() { local s="" x; for x in "$@"; do s+="${s:+, }\"$x\""; done; printf '[%s]' "$s"; }
cat >"$pack/config.toml" <<CFG
[app]
log_directory = "~/Library/Logs/tulpa"

[tray]
enabled = true

[link]
enabled = true
host = "$alias"
target_host = "127.0.0.1"
target_port = $tport
device = "$device"
token_file = "~/.config/tulpa/token"
office_ssids = $(toml_list "${ssids[@]+"${ssids[@]}"}")
direct_hosts = $(toml_list "${directs[@]+"${directs[@]}"}")

[hands]
mode = "on"
policy = "~/.config/tulpa/hands.toml"

[ears]
mode = "off"
CFG
cat >"$pack/hands.toml" <<HANDS
# Set up by the installer. Full mode: any shell command, any file, every request logged.
full = $full
roots = []
HANDS
# The jump hop runs its own ssh: it gets its own Host entry with the same tulpa key (the alias's
# IdentityFile doesn't reach the hop), so the server side must authorize that key on the jump host too.
{
	if [ -n "$jump" ]; then
		jhost=${jump#*@}; juser=""; [ "$jhost" = "$jump" ] || juser=${jump%%@*}
		jport=22; case $jhost in *:*) jport=${jhost##*:}; jhost=${jhost%:*} ;; esac
		printf 'Host %s-jump\n  HostName %s\n  Port %s\n  IdentityFile ~/.ssh/tulpa_ed25519\n  IdentitiesOnly yes\n' "$alias" "$jhost" "$jport"
		[ -z "$juser" ] || printf '  User %s\n' "$juser"
		printf '\n'
	fi
	printf 'Host %s\n  HostName %s\n  User %s\n  Port %s\n  IdentityFile ~/.ssh/tulpa_ed25519\n  IdentitiesOnly yes\n' "$alias" "$host" "$user" "$port"
	[ -z "$jump" ] || printf '  ProxyJump %s-jump\n' "$alias"
} >"$pack/ssh_config"
cp "$known" "$pack/known_hosts"
cp "$repo_root/packaging/macos/install.command" "$pack/install.command"
chmod 0755 "$pack/install.command"

# No resource forks or extended attributes: an unzip of a ditto archive with them leaves ._ files inside the
# signed bundle and breaks its seal (found on the first build).
(cd "$stage" && /usr/bin/ditto -c -k --norsrc --noextattr --keepParent "tulpa-$device" "$out/tulpa-$device.zip")
printf 'make-installer: %s\nmake-installer: server token: %s (copy to the devices dir as %s.token, 0600)\n' "$out/tulpa-$device.zip" "$out/$device.token" "$device"
[ -z "$jump" ] || printf 'make-installer: the jump host must also authorize the Mac'"'"'s tulpa key (install.command prints it), restricted to the server: restrict,permitopen="%s:%s"\n' "$host" "$port"
