#!/usr/bin/env bash
# Assemble Huck's Voice to Text.app from an already-built release binary. Shared by install.sh
# (this Mac) and release.sh (the public DMG), so the two can never describe different apps.
#
#   scripts/assemble-app.sh <destination.app>
#
# HVTT_BUNDLE_MODEL=/path/to/ggml-*.bin  also places that speech model inside the app, at
#   Contents/Resources/models/. A downloaded release needs it; this Mac's copy uses the one in
#   Application Support instead.
#
# Name, identifier, version and minimum macOS are read from desktop/tauri.conf.json, the single
# source of truth. The bundle is not signed here; the caller signs it.

set -euo pipefail

# exFAT scatters AppleDouble "._" files, and codesign rejects a bundle containing them.
export COPYFILE_DISABLE=1

DEST="${1:?usage: assemble-app.sh <destination.app>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"
CONF="$APP_DIR/desktop/tauri.conf.json"
EXECUTABLE_NAME="hvtt-desktop"
BINARY="${CARGO_TARGET_DIR:-$HOME/.hvtt-build/target}/release/$EXECUTABLE_NAME"
[ -x "$BINARY" ] || { echo "No release binary at $BINARY - build it first." >&2; exit 1; }

# The product name contains both spaces and an apostrophe, so it is emitted as shell-quoted
# assignments rather than split on whitespace. Reading it with plain `read` produced a bundle
# called "Huck's.app".
eval "$(python3 - "$CONF" <<'CONFPY'
import json, shlex, sys
c = json.load(open(sys.argv[1]))
mac = c.get("bundle", {}).get("macOS", {})
for name, value in [
    ("PRODUCT_NAME", c["productName"]),
    ("BUNDLE_ID", c["identifier"]),
    ("VERSION", c["version"]),
    ("MIN_OS", mac.get("minimumSystemVersion", "11.0")),
]:
    print(f"{name}={shlex.quote(value)}")
CONFPY
)"

rm -rf "$DEST"
mkdir -p "$DEST/Contents/MacOS" "$DEST/Contents/Resources"
cp "$BINARY" "$DEST/Contents/MacOS/$EXECUTABLE_NAME"
chmod +x "$DEST/Contents/MacOS/$EXECUTABLE_NAME"
cp "$APP_DIR/desktop/icons/icon.icns" "$DEST/Contents/Resources/icon.icns"

if [ -n "${HVTT_BUNDLE_MODEL:-}" ]; then
    [ -f "$HVTT_BUNDLE_MODEL" ] || { echo "No model at $HVTT_BUNDLE_MODEL" >&2; exit 1; }
    mkdir -p "$DEST/Contents/Resources/models"
    cp "$HVTT_BUNDLE_MODEL" "$DEST/Contents/Resources/models/"
fi

# LSUIElement keeps it out of the Dock and the app switcher. That is the product design — a
# voice layer, not an application you manage — and it is also what lets the floating box appear
# without stealing focus. The runtime sets the same policy; this makes it true from launch.
#
# NSMicrophoneUsageDescription is not optional: without it macOS terminates the process the
# instant it touches the microphone, with no prompt and no useful error.
cat > "$DEST/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$PRODUCT_NAME</string>
  <key>CFBundleDisplayName</key><string>$PRODUCT_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$EXECUTABLE_NAME</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>$MIN_OS</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Huck's Voice to Text listens only while you are dictating, and transcribes entirely on this Mac.</string>
</dict>
</plist>
PLIST

find "$DEST" -name '._*' -delete 2>/dev/null || true
xattr -cr "$DEST" 2>/dev/null || true
