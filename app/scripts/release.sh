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
# UNSIGNED BETA. The app is not signed with a Developer ID and is not notarized, so macOS blocks
# the first launch until the user chooses Open Anyway in Privacy & Security. Say so on the
# download page.
#
# ONE SIGNATURE FOR EVERY RELEASE (from 0.1.10). macOS ties Microphone and Accessibility to the
# app's signature. Signed ad hoc - as every release up to 0.1.7 was - each build is a different
# program to macOS: after an update the old ticks still show in System Settings and no longer
# count. So a release is signed with this Mac's own certificate, the one install.sh uses
# ("Project Playground Local Signing") - that very certificate, by its fingerprint
# (RELEASE_CERTIFICATE below), since another of the same name would be another signature
# (Codex's review, 2026-10-06). Self-made, so macOS trusts it no
# more than before, but the same from one release to the next, so the permissions stay. It
# holds a name and nothing else of his. **A release cannot be made without it** - one signed
# any other way would cost everybody their permissions once more. The certificate and its key
# live in this Mac's login keychain and nowhere else: lose them and the next release does
# exactly that. The move from an ad-hoc release (0.1.7) to the first one signed this way is
# one last switching-on.
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
print(f"BUNDLE_ID={shlex.quote(c['identifier'])}")
CONFPY
)"

[ "$(uname -m)" = "arm64" ] || { echo "Build the Apple silicon release on an Apple silicon Mac." >&2; exit 1; }

NAME="HucksVoiceToText-$VERSION-macOS-arm64-unsigned-beta"
# "Best" (large-v3-turbo, 5-bit) is the model inside the Mac app (hvtt_core::models, 2026-10-02).
MODEL="${HVTT_RELEASE_MODEL:-$HOME/Library/Application Support/com.huck.voice-to-text/models/ggml-large-v3-turbo-q5_0.bin}"
[ -f "$MODEL" ] || { echo "No speech model at $MODEL - run scripts/fetch-model.sh large-v3-turbo-q5_0" >&2; exit 1; }
[ "$(shasum -a 256 "$MODEL" | cut -c1-64)" = "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2" ] \
    || { echo "The speech model at $MODEL is not the published file." >&2; exit 1; }
# Startup asks for this exact name (hvtt_core::models): a renamed copy would never be found.
[ "$(basename "$MODEL")" = "ggml-large-v3-turbo-q5_0.bin" ] \
    || { echo "The speech model must be named ggml-large-v3-turbo-q5_0.bin." >&2; exit 1; }
VAD="${HVTT_RELEASE_VAD:-$HOME/Library/Application Support/com.huck.voice-to-text/models/ggml-silero-v5.1.2.bin}"
[ -f "$VAD" ] || { echo "No voice detector at $VAD - run scripts/fetch-model.sh silero-v5.1.2" >&2; exit 1; }
[ "$(shasum -a 256 "$VAD" | cut -c1-64)" = "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf" ] \
    || { echo "The voice detector at $VAD is not the published file." >&2; exit 1; }
[ "$(basename "$VAD")" = "ggml-silero-v5.1.2.bin" ] \
    || { echo "The voice detector must be named ggml-silero-v5.1.2.bin." >&2; exit 1; }
OUT="$PROJECT_ROOT/artifacts/macos/release"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hvtt-release.XXXXXX")"
LAYOUT=""
trap 'hdiutil detach "$WORK/mnt" -quiet 2>/dev/null || true; [ -z "$LAYOUT" ] || hdiutil detach "$LAYOUT" -quiet -force 2>/dev/null || true; rm -rf "$WORK"' EXIT
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
HVTT_BUNDLE_MODEL="$MODEL" HVTT_BUNDLE_VAD="$VAD" bash "$SCRIPT_DIR/assemble-app.sh" "$APP"

# This is deliberately a release failure, not a best-effort warning. It protects against a future
# compiler, dependency or custom target directory bypassing the remapping above.
PRIVATE_MARKERS='/''Users/|/''Volumes/|@''gmail\.com|huckletsplay''@'
if strings "$APP/Contents/MacOS/hvtt-desktop" \
    | grep -E "$PRIVATE_MARKERS" >/dev/null; then
    echo "Refusing to package: the executable contains a personal path or email address." >&2
    exit 1
fi

# The one certificate every release is signed with, by its SHA-1 fingerprint - which is public:
# it is in every copy of the app. Changing it costs everybody their permissions once.
RELEASE_CERTIFICATE="39C3C378F490A6690F7FBC8092A90A83EF212046"
if ! security find-identity -v -p codesigning 2>/dev/null | grep -qiF "$RELEASE_CERTIFICATE"; then
    echo "Refusing to package: the release certificate ($RELEASE_CERTIFICATE) is not on this Mac." >&2
    echo "Every release carries that one signature, so that updates keep their permissions." >&2
    exit 1
fi
say "Signing with the release certificate (unsigned beta: no Developer ID, not notarized)…"
codesign --force --sign "$RELEASE_CERTIFICATE" --timestamp=none "$APP"
codesign --verify --strict --verbose=1 "$APP"
# What macOS will hold the permissions against: this program by name, signed with that
# certificate - not a fingerprint of this one build.
REQUIREMENT="$(codesign -d -r- "$APP" 2>/dev/null | grep "designated =>" || true)"
EXPECTED_LEAF="$(printf '%s' "$RELEASE_CERTIFICATE" | tr 'A-F' 'a-f')"
case "$REQUIREMENT" in
    *"identifier \"$BUNDLE_ID\" and certificate leaf = H\"$EXPECTED_LEAF\""*) ;;
    *)
        echo "Refusing to package: the signature is not tied to the certificate ($REQUIREMENT)." >&2
        exit 1
        ;;
esac
ARCHS="$(lipo -archs "$APP/Contents/MacOS/hvtt-desktop")"
[ "$ARCHS" = "arm64" ] || { echo "Unexpected architectures: $ARCHS" >&2; exit 1; }

say "Making the DMG…"
ln -s /Applications "$STAGE/Applications"
# Dressed like the floating box: the background drawn by make_install_art.py (1x and 2x in one
# TIFF, so a Retina screen gets the sharp one), the app and Applications on its two tiles. Finder
# stores the layout in the volume's .DS_Store, so it is laid out once in a writable image, then
# compressed. The first run asks to let this terminal control Finder; allow it.
ART="$APP_DIR/desktop/installer"
mkdir -p "$STAGE/.background"
tiffutil -cathidpicheck "$ART/dmg-background.png" "$ART/dmg-background@2x.png" \
    -out "$STAGE/.background/background.tiff" >/dev/null
hdiutil create -quiet -volname "$PRODUCT_NAME" -srcfolder "$STAGE" -fs HFS+ -format UDRW \
    -ov "$WORK/layout.dmg"
LAYOUT="$(hdiutil attach -readwrite -noverify -noautoopen "$WORK/layout.dmg" \
    | awk -F'\t' '/Apple_HFS/ {print $NF}')"
[ -d "$LAYOUT" ] || { echo "Could not open the DMG to lay it out." >&2; exit 1; }
# Window: 660 x 400 points of content, the size of the art. Icon centres match APP_AT / APPS_AT
# in make_install_art.py.
osascript - "$(basename "$LAYOUT")" "$PRODUCT_NAME.app" <<'OSA'
on run argv
    set volumeName to item 1 of argv
    set appName to item 2 of argv
    tell application "Finder"
        tell disk volumeName
            open
            set current view of container window to icon view
            set toolbar visible of container window to false
            set statusbar visible of container window to false
            set the bounds of container window to {200, 120, 860, 548}
            set opts to the icon view options of container window
            set arrangement of opts to not arranged
            set icon size of opts to 128
            set text size of opts to 13
            set background picture of opts to file ".background:background.tiff"
            set position of item appName of container window to {180, 190}
            set position of item "Applications" of container window to {480, 190}
            update without registering applications
            delay 1
            close
        end tell
    end tell
end run
OSA
# Finder writes its .DS_Store a moment after the window closes: give it up to ten seconds.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sync
    [ -f "$LAYOUT/.DS_Store" ] && break
    sleep 1
done
[ -f "$LAYOUT/.DS_Store" ] || { echo "Finder did not save the DMG's layout." >&2; exit 1; }
hdiutil detach -quiet "$LAYOUT"
LAYOUT=""
hdiutil convert -quiet "$WORK/layout.dmg" -format UDZO -o "$WORK/$NAME.dmg"
rm -f "$WORK/layout.dmg"
hdiutil verify -quiet "$WORK/$NAME.dmg"

say "Checking the app inside the DMG…"
mkdir -p "$WORK/mnt"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$WORK/mnt" "$WORK/$NAME.dmg"
MOUNTED="$WORK/mnt/$PRODUCT_NAME.app"
codesign --verify --strict "$MOUNTED"
[ -f "$MOUNTED/Contents/Resources/models/$(basename "$MODEL")" ] \
    || { echo "The speech model is missing from the mounted app." >&2; exit 1; }
[ -f "$MOUNTED/Contents/Resources/models/$(basename "$VAD")" ] \
    || { echo "The voice detector is missing from the mounted app." >&2; exit 1; }
[ -L "$WORK/mnt/Applications" ] || { echo "The Applications shortcut is missing." >&2; exit 1; }
[ -f "$WORK/mnt/.background/background.tiff" ] && [ -f "$WORK/mnt/.DS_Store" ] \
    || { echo "The DMG's branded background or layout is missing." >&2; exit 1; }
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
