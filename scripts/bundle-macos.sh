#!/usr/bin/env bash
# Builds the release binary and the helpers and assembles a signed Tulpa.app.
#
# The bundle is signed with a stable local identity (a self-signed code-signing
# certificate in the login keychain), never ad hoc: an ad-hoc signature changes
# the code directory hash on every build and macOS forgets the microphone,
# audio-capture and camera grants each time.
#
# The camera helper (packaging/macos/tulpa-camera.swift, for eyes.photo) is
# compiled with the Xcode command line tools' swiftc into
# Contents/MacOS/tulpa-camera and signed first, with the same identity and
# entitlements; Tulpa runs it as its own child, so the camera grant is Tulpa's.
# The ears' helpers are built the same way (packaging/macos/tulpa-sysaudio.swift
# into Contents/MacOS/tulpa-sysaudio, and the tulpa-segment crate into
# Contents/MacOS/tulpa-segment); the ears' capture script ships as a resource
# (Contents/Resources/ears-capture.sh) and finds them in ../MacOS.
# tulpa-hear, the ears in one process (mic, system-audio tap and whisper.cpp
# in memory), is a release cargo build with the `whisper` feature (which needs
# cmake) into Contents/MacOS/tulpa-hear, signed like the other helpers. Both
# ears ship side by side; the config, not the bundle, chooses which one runs.
#
#   TULPA_SIGN_IDENTITY  certificate name to sign with (default: tulpa-dev)
#   TULPA_BUNDLE_DIR     where Tulpa.app is written (default: target/bundle)
#
# Usage: scripts/bundle-macos.sh
set -euo pipefail

die() {
	printf 'bundle-macos: %s\n' "$*" >&2
	exit 1
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
identity=${TULPA_SIGN_IDENTITY:-tulpa-dev}
bundle_dir=${TULPA_BUNDLE_DIR:-$repo_root/target/bundle}
target_dir=${CARGO_TARGET_DIR:-$repo_root/target}
template=$repo_root/packaging/macos/Info.plist
entitlements=$repo_root/packaging/macos/entitlements.plist
camera_src=$repo_root/packaging/macos/tulpa-camera.swift

[[ $(uname -s) == Darwin ]] || die "macOS only (codesign, plutil, security)"
for tool in cargo codesign plutil security xcrun; do
	command -v "$tool" >/dev/null || die "$tool not found"
done
# tulpa-hear's `whisper` feature builds whisper.cpp with cmake.
command -v cmake >/dev/null || die "cmake not found (tulpa-hear's whisper build needs it: brew install cmake)"
[[ -f $template ]] || die "missing $template"
[[ -f $entitlements ]] || die "missing $entitlements"
[[ -f $camera_src ]] || die "missing $camera_src"

# Fail before building when the identity is missing. Without -v so a
# self-signed certificate that is not marked trusted still counts; codesign
# below is the final judge.
if ! security find-identity -p codesigning | grep -Fq "\"$identity\""; then
	die "no code-signing identity named \"$identity\" in the keychain.
Create one once (Keychain Access > Certificate Assistant > Create a Certificate,
type Code Signing, name \"$identity\") or set TULPA_SIGN_IDENTITY. Ad-hoc signing
is refused: it would reset the microphone permission on every build."
fi

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$repo_root/Cargo.toml")
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "could not read the workspace version"

# The commit goes into the check-in marker the app writes once its link is up (update.sh waits for it).
commit=$(git -C "$repo_root" describe --always --dirty --abbrev=12 2>/dev/null || printf unknown)

(cd -- "$repo_root" && TULPA_COMMIT=$commit cargo build --release --locked -p tulpa)
binary=$target_dir/release/tulpa
[[ -x $binary ]] || die "release binary missing at $binary"

app=$bundle_dir/Tulpa.app
rm -rf -- "$app"
mkdir -p -- "$app/Contents/MacOS"
cp -- "$binary" "$app/Contents/MacOS/tulpa"
sed "s/@VERSION@/$version/g" "$template" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null || die "Info.plist does not lint"

# The camera helper, for this Mac's architecture like the cargo build above.
helper=$app/Contents/MacOS/tulpa-camera
xcrun --sdk macosx swiftc -O -swift-version 5 -target "$(uname -m)-apple-macos11.0" \
	-o "$helper" "$camera_src" || die "the camera helper did not build (Xcode command line tools: xcode-select --install)"

# The ears' helpers: the system-audio tap (ScreenCaptureKit, like the camera
# helper a swiftc build for this Mac) and the utterance segmenter (a release
# cargo build of the workspace crate). The capture script
# (packaging/macos/ears-capture.sh) runs both as its own children and finds
# them beside itself in ../MacOS — it ships as a resource below.
sysaudio_src=$repo_root/packaging/macos/tulpa-sysaudio.swift
[[ -f $sysaudio_src ]] || die "missing $sysaudio_src"
sysaudio=$app/Contents/MacOS/tulpa-sysaudio
# -apple-macos13.0, not the camera helper's 11.0: ScreenCaptureKit's SCStream asks 12.3 and its
# audio capture (SCStreamConfiguration.capturesAudio) 13.0, and swiftc rejects the symbols below
# that (found on the first 09dddf9 build of the home Mac, 2026-11-18).
xcrun --sdk macosx swiftc -O -swift-version 5 -target "$(uname -m)-apple-macos13.0" \
	-o "$sysaudio" "$sysaudio_src" || die "the sysaudio helper did not build (Xcode command line tools: xcode-select --install)"
(cd -- "$repo_root" && cargo build --release --locked -p tulpa-segment)
segment=$target_dir/release/tulpa-segment
[[ -x $segment ]] || die "release tulpa-segment missing at $segment"
cp -- "$segment" "$app/Contents/MacOS/tulpa-segment"
# The ears in one process: the mic, the tap and whisper.cpp (Metal) in memory.
(cd -- "$repo_root" && cargo build --release --locked -p tulpa-hear --features whisper)
hear=$target_dir/release/tulpa-hear
[[ -x $hear ]] || die "release tulpa-hear missing at $hear"
cp -- "$hear" "$app/Contents/MacOS/tulpa-hear"

# The bundle's identity, as key=value lines a POSIX shell reads with sed. The
# login check (packaging/macos/build-check.sh) compares the commit with
# refs/tags/latest-good on the origin, whose tags are full shas — so this is
# `rev-parse HEAD`, not the describe form the binary gets, and a dirty tree
# says so (-dirty never equals a released sha). It lives in Contents/Resources
# — the seal's CodeResources covers only resources, and a bare file in
# Contents/ leaves the signature invalid ("code has no resources but signature
# indicates they must be present", found on the first real-Mac build) — and it
# is written before codesign: the signature seals it, and a write after would
# break the seal.
if marker_commit=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null); then
	git -C "$repo_root" diff-index --quiet HEAD 2>/dev/null || marker_commit=$marker_commit-dirty
else
	marker_commit=unknown
fi
mkdir -p "$app/Contents/Resources"
printf 'version=%s\ncommit=%s\nutc=%s\n' \
	"$version" "$marker_commit" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$app/Contents/Resources/build"
# The ears' capture script: a resource the [ears] command runs
# (/bin/sh + this path); it finds its helpers in ../MacOS.
cp -- "$repo_root/packaging/macos/ears-capture.sh" "$app/Contents/Resources/ears-capture.sh"

# Nested code first: the app's signature seals the helpers'.
codesign --force --options runtime --timestamp=none \
	--entitlements "$entitlements" --identifier com.tulpa.app.camera \
	--sign "$identity" "$helper"
codesign --force --options runtime --timestamp=none \
	--entitlements "$entitlements" --identifier com.tulpa.app.sysaudio \
	--sign "$identity" "$sysaudio"
codesign --force --options runtime --timestamp=none \
	--entitlements "$entitlements" --identifier com.tulpa.app.segment \
	--sign "$identity" "$app/Contents/MacOS/tulpa-segment"
codesign --force --options runtime --timestamp=none \
	--entitlements "$entitlements" --identifier com.tulpa.app.hear \
	--sign "$identity" "$app/Contents/MacOS/tulpa-hear"
codesign --force --options runtime --timestamp=none \
	--entitlements "$entitlements" \
	--sign "$identity" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

printf 'bundle-macos: %s (version %s, commit %s, signed by "%s")\n' "$app" "$version" "$commit" "$identity"
