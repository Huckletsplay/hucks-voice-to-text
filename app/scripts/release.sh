#!/usr/bin/env bash
# Build the public macOS beta: a DMG and its SHA-256 checksum, verified, in artifacts/.
#
#   scripts/release.sh --unsigned-beta
#
# Produces, for the version in desktop/tauri.conf.json:
#   artifacts/macos/release/HucksVoiceToText-<version>-macOS-arm64-unsigned-beta.dmg
#   artifacts/macos/release/HucksVoiceToText-<version>-macOS-arm64-unsigned-beta.dmg.sha256.txt
#
# These names are the contract with the in-app updater (`desktop/src/update.rs`); change one and
# the other must change with it.
#
# UNSIGNED BETA. The app is signed ad hoc, not with a Developer ID, and is not notarized, so macOS
# blocks the first launch until the user chooses Open Anyway in Privacy & Security. An ad-hoc
# identity also changes with every build, so each update needs Microphone and Accessibility
# switched on again. Say both on the download page.
#
# The speech model is placed inside the app so a download works straight away. It is taken from
# HVTT_RELEASE_MODEL, or else this Mac's Application Support copy (`scripts/fetch-model.sh
# base.en`). The app prefers a model in Application Support when one exists.
#
# Everything is built on the local disk: this repository may live on exFAT, which cannot hold a
# signed bundle intact. Only the finished DMG and checksum are copied into artifacts/.

set -euo pipefail
export COPYFILE_DISABLE=1

[ "${1:-}" = "--unsigned-beta" ] || {
    echo "usage: scripts/release.sh --unsigned-beta" >&2
    echo "(Developer ID signing and notarization are not set up yet.)" >&2
    exit 2
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$(dirname "$APP_DIR")"
CONF="$APP_DIR/desktop/tauri.conf.json"

eval "$(python3 - "$CONF" <<'CONFPY'
import json, shlex, sys
c = json.load(open(sys.argv[1]))
print(f"PRODUCT_NAME={shlex.quote(c['productName'])}")
print(f"VERSION={shlex.quote(c['version'])}")
CONFPY
)"

[ "$(uname -m)" = "arm64" ] || { echo "Build the Apple silicon release on an Apple silicon Mac." >&2; exit 1; }

NAME="HucksVoiceToText-$VERSION-macOS-arm64-unsigned-beta"
MODEL="${HVTT_RELEASE_MODEL:-$HOME/Library/Application Support/com.huck.voice-to-text/models/ggml-base.en.bin}"
[ -f "$MODEL" ] || { echo "No speech model at $MODEL - run scripts/fetch-model.sh base.en" >&2; exit 1; }
OUT="$PROJECT_ROOT/artifacts/macos/release"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hvtt-release.XXXXXX")"
trap 'hdiutil detach "$WORK/mnt" -quiet 2>/dev/null || true; rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"
APP="$STAGE/$PRODUCT_NAME.app"
CARGO_TARGET_DIR="$WORK/target"
export CARGO_TARGET_DIR

# Rust and the native whisper.cpp build otherwise embed this Mac's absolute source, Cargo-cache
# and target paths in the executable. Besides making the build machine part of the public binary,
# that exposes the private project location and macOS account name. Keep useful source locations,
# but make them deterministic and anonymous. CARGO_ENCODED_RUSTFLAGS uses the unit separator so
# the project path remains one argument even though the external drive's name contains a space.
FLAG_SEPARATOR=$'\037'
PATH_REMAP_FLAGS="--remap-path-prefix=$PROJECT_ROOT=/src/huck-voice-to-text${FLAG_SEPARATOR}--remap-path-prefix=$HOME=/build${FLAG_SEPARATOR}--remap-path-prefix=$WORK=/build/work"
if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
    PATH_REMAP_FLAGS="$PATH_REMAP_FLAGS${FLAG_SEPARATOR}$CARGO_ENCODED_RUSTFLAGS"
fi
export CARGO_ENCODED_RUSTFLAGS="$PATH_REMAP_FLAGS"
NATIVE_PATH_REMAP="-ffile-prefix-map=$HOME=/build -ffile-prefix-map=$WORK=/build/work"
export CFLAGS="${CFLAGS:-} $NATIVE_PATH_REMAP"
export CXXFLAGS="${CXXFLAGS:-} $NATIVE_PATH_REMAP"
export CMAKE_C_FLAGS="${CMAKE_C_FLAGS:-} $NATIVE_PATH_REMAP"
export CMAKE_CXX_FLAGS="${CMAKE_CXX_FLAGS:-} $NATIVE_PATH_REMAP"

say() { printf '%s\n' "$*"; }

say "Building $PRODUCT_NAME $VERSION (release)…"
bash "$SCRIPT_DIR/dev.sh" build-release --features custom-protocol

say "Assembling, with the speech model inside…"
mkdir -p "$STAGE"
HVTT_BUNDLE_MODEL="$MODEL" bash "$SCRIPT_DIR/assemble-app.sh" "$APP"

# This is deliberately a release failure, not a best-effort warning. It protects against a future
# compiler, dependency or custom target directory bypassing the remapping above.
PRIVATE_MARKERS='/''Users/|/''Volumes/|@''gmail\.com|huckletsplay''@'
if strings "$APP/Contents/MacOS/hvtt-desktop" \
    | grep -E "$PRIVATE_MARKERS" >/dev/null; then
    echo "Refusing to package: the executable contains a personal path or email address." >&2
    exit 1
fi

say "Signing ad hoc (unsigned beta)…"
codesign --force --sign - "$APP"
codesign --verify --strict --verbose=1 "$APP"
ARCHS="$(lipo -archs "$APP/Contents/MacOS/hvtt-desktop")"
[ "$ARCHS" = "arm64" ] || { echo "Unexpected architectures: $ARCHS" >&2; exit 1; }

say "Making the DMG…"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "$PRODUCT_NAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO \
    -ov "$WORK/$NAME.dmg"
hdiutil verify -quiet "$WORK/$NAME.dmg"

say "Checking the app inside the DMG…"
mkdir -p "$WORK/mnt"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$WORK/mnt" "$WORK/$NAME.dmg"
MOUNTED="$WORK/mnt/$PRODUCT_NAME.app"
codesign --verify --strict "$MOUNTED"
[ -f "$MOUNTED/Contents/Resources/models/$(basename "$MODEL")" ] \
    || { echo "The speech model is missing from the mounted app." >&2; exit 1; }
[ -L "$WORK/mnt/Applications" ] || { echo "The Applications shortcut is missing." >&2; exit 1; }
hdiutil detach -quiet "$WORK/mnt"

# One line, the file name only - the form the updater requires.
( cd "$WORK" && shasum -a 256 "$NAME.dmg" > "$NAME.dmg.sha256.txt" )

mkdir -p "$OUT"
cp "$WORK/$NAME.dmg" "$WORK/$NAME.dmg.sha256.txt" "$OUT/"
find "$OUT" -name '._*' -delete 2>/dev/null || true
( cd "$OUT" && shasum -a 256 -c "$NAME.dmg.sha256.txt" >/dev/null ) \
    || { echo "The copied DMG does not match its checksum." >&2; exit 1; }

say ""
say "Release built:"
say "  $OUT/$NAME.dmg  ($(du -h "$OUT/$NAME.dmg" | cut -f1))"
say "  $OUT/$NAME.dmg.sha256.txt"
say "  SHA-256 $(cut -d' ' -f1 "$OUT/$NAME.dmg.sha256.txt")"
