#!/bin/bash
# install.command — first start of Tulpa on this Mac, from the installer folder it sits in. Double-click it.
# Never overwrites what exists (an earlier config, key or ssh entry is kept); says what it did. The edits:
# Tulpa's LaunchAgent, written or kept, runs with ProcessType = Interactive (without it launchd limits Tulpa
# like a background job and the microphone capture loses most of its samples), and with KeepAlive =
# { SuccessfulExit = false }: launchd starts Tulpa again after a crash or a kill, not after Quit (exit 0).
# A kept plist's KeepAlive = true, from an older install, is changed so; any other KeepAlive is kept.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
say() { printf 'tulpa: %s\n' "$*"; }
umask 077
mkdir -p ~/Applications ~/.config/tulpa ~/.ssh ~/Library/LaunchAgents ~/Library/Logs/tulpa
chmod 700 ~/.ssh ~/.config/tulpa

rm -rf ~/Applications/Tulpa.app && cp -R "$here/Tulpa.app" ~/Applications/Tulpa.app && say "Tulpa.app → ~/Applications"
for f in config.toml hands.toml token; do
	if [ -e ~/.config/tulpa/$f ]; then say "kept ~/.config/tulpa/$f"; else install -m 0600 "$here/$f" ~/.config/tulpa/$f && say "~/.config/tulpa/$f"; fi
done

[ -e ~/.ssh/tulpa_ed25519 ] || { ssh-keygen -q -t ed25519 -N "" -C "tulpa@$(scutil --get LocalHostName 2>/dev/null || hostname)" -f ~/.ssh/tulpa_ed25519 && say "made ~/.ssh/tulpa_ed25519"; }
alias=$(sed -n 's/^Host //p' "$here/ssh_config" | grep -v -- '-jump$' | head -n1)
if grep -q "^Host $alias\$" ~/.ssh/config 2>/dev/null; then say "kept the ssh entry $alias"; else { printf '\n'; cat "$here/ssh_config"; } >>~/.ssh/config && chmod 600 ~/.ssh/config && say "ssh entry $alias added"; fi
touch ~/.ssh/known_hosts; while IFS= read -r line; do grep -qxF "$line" ~/.ssh/known_hosts || printf '%s\n' "$line" >>~/.ssh/known_hosts; done <"$here/known_hosts"

plist=~/Library/LaunchAgents/com.tulpa.app.plist
keepalive='<dict><key>SuccessfulExit</key><false/></dict>'
if [ -e "$plist" ]; then
	say "kept $plist"
	if plutil -replace ProcessType -string Interactive "$plist"; then say "ProcessType = Interactive in $plist"; else say "could not set ProcessType = Interactive in $plist: the ears may lose samples"; fi
	if [ "$(plutil -extract KeepAlive raw -o - "$plist" 2>/dev/null)" = true ]; then
		if plutil -replace KeepAlive -xml "$keepalive" "$plist"; then say "KeepAlive = { SuccessfulExit = false } in $plist: Quit stops Tulpa"; else say "could not change KeepAlive = true in $plist: launchd starts Tulpa again after Quit"; fi
	fi
else cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.tulpa.app</string>
  <key>ProgramArguments</key><array><string>$HOME/Applications/Tulpa.app/Contents/MacOS/tulpa</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>$keepalive
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
</dict></plist>
PLIST
say "LaunchAgent $plist"; fi
launchctl bootout "gui/$(id -u)/com.tulpa.app" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist" && say "started (LaunchAgent com.tulpa.app)"

say "one step left on the server: authorize this key for the tulpa door —"
cat ~/.ssh/tulpa_ed25519.pub
say "send that line to whoever runs the server (it goes on the server and, off the office network, on the jump host). Once it is added, Tulpa connects by itself."
say "if macOS refuses to open Tulpa (downloaded files are quarantined): xattr -dr com.apple.quarantine ~/Applications/Tulpa.app"
