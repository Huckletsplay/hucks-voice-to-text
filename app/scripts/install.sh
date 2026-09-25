#!/usr/bin/env bash
# Install Huck's Voice to Text into ~/Applications as an ordinary macOS app.
#
#   cd app && scripts/install.sh
#
# WHAT THIS IS FOR
# Running from a terminal is fine for development, but it is not how the product is used, and it
# hides real problems: permission prompts behave differently, the menu-bar item and the global
# shortcut belong to a background app rather than a terminal session, and nothing survives
# closing the window. This installs the real thing.
#
# WHY IT DOES NOT USE THE TAURI CLI
# `cargo tauri build` would need the Tauri CLI installed, and it also wants to produce a DMG.
# A .app bundle is a directory with a binary, an Info.plist and an icon, so this assembles one
# directly — fewer moving parts, and nothing extra to install. `assemble-app.sh` builds it, the
# same way `release.sh` does, from `desktop/tauri.conf.json`.
#
# SIGNING, AND WHY IT MATTERS MORE THAN IT SOUNDS
# macOS ties permissions — microphone, Accessibility — to an app's identity. With no signing
# identity that identity is a fingerprint of the exact binary, so **every rebuild looks like a
# different app** and every permission has to be granted again. Signing with a stable local
# certificate gives the app one identity that survives rebuilds. This uses a self-signed
# "Project Playground Local Signing" code-signing certificate - the one Huck's programs share -
# or whatever HVTT_SIGN_IDENTITY names. It is for this Mac only; public releases use release.sh.
#
# Settings, models and recovery drafts live in ~/Library/Application Support and are never
# touched here, so reinstalling keeps them.

set -euo pipefail

# exFAT scatters AppleDouble "._" files, and codesign rejects a bundle containing them.
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$(dirname "$APP_DIR")"
CONF="$APP_DIR/desktop/tauri.conf.json"

eval "$(python3 - "$CONF" <<'CONFPY'
import json, shlex, sys
c = json.load(open(sys.argv[1]))
for name, value in [("PRODUCT_NAME", c["productName"]), ("BUNDLE_ID", c["identifier"])]:
    print(f"{name}={shlex.quote(value)}")
CONFPY
)"

EXECUTABLE_NAME="hvtt-desktop"
TARGET_DIR="$HOME/Applications"
# The bundle is assembled straight into its staging spot on the LOCAL DISK, and never on the
# project's exFAT drive. exFAT stores no executable bit and scatters AppleDouble "._" files,
# both of which break a code signature — copying a signed bundle off it fails verification with
# "a sealed resource is missing or invalid".
SOURCE_APP="$TARGET_DIR/.hvtt.installing.app"
TARGET_APP="$TARGET_DIR/$PRODUCT_NAME.app"
BACKUP_APP="$TARGET_DIR/.hvtt.previous.app"
SIGN_IDENTITY="${HVTT_SIGN_IDENTITY:-Project Playground Local Signing}"

say() { printf '%s\n' "$*"; }

# ---------------------------------------------------------------- 1. build

say "Building $PRODUCT_NAME (release)…"
bash "$SCRIPT_DIR/dev.sh" build-release

# ---------------------------------------------------------------- 2. assemble

say "Assembling the app bundle…"
mkdir -p "$TARGET_DIR"
rm -rf "$BACKUP_APP"
bash "$SCRIPT_DIR/assemble-app.sh" "$SOURCE_APP"

# ---------------------------------------------------------------- 3. sign

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$SIGN_IDENTITY"; then
    say "Signing as \"$SIGN_IDENTITY\"…"
    codesign --force --sign "$SIGN_IDENTITY" --timestamp=none "$SOURCE_APP"
    SIGNED_STABLY=yes
else
    # Ad-hoc signing still produces a runnable app, but its identity changes with every build,
    # so macOS asks for microphone and Accessibility permission again after each reinstall.
    say "No \"$SIGN_IDENTITY\" certificate found — signing ad-hoc."
    say "Permissions will reset on each reinstall. To fix that once, create a code-signing"
    say "certificate named \"$SIGN_IDENTITY\" in Keychain Access (Certificate Assistant ›"
    say "Create a Certificate, type Code Signing), or set HVTT_SIGN_IDENTITY to one you have."
    codesign --force --sign - "$SOURCE_APP"
    SIGNED_STABLY=no
fi
codesign --verify --strict "$SOURCE_APP"

# ---------------------------------------------------------------- 4. install

# Stop only our own installed copy, matched on its exact path.
if [ -x "$TARGET_APP/Contents/MacOS/$EXECUTABLE_NAME" ]; then
    say "Stopping the running copy…"
    pkill -f "^$TARGET_APP/Contents/MacOS/$EXECUTABLE_NAME$" 2>/dev/null || true
    for _ in $(seq 1 25); do
        pgrep -f "^$TARGET_APP/Contents/MacOS/$EXECUTABLE_NAME$" >/dev/null 2>&1 || break
        sleep 0.2
    done
fi

# Swap only after the replacement is built and verified, and keep the old one until it lands.
[ -d "$TARGET_APP" ] && mv "$TARGET_APP" "$BACKUP_APP"
if ! mv "$SOURCE_APP" "$TARGET_APP"; then
    [ -d "$BACKUP_APP" ] && mv "$BACKUP_APP" "$TARGET_APP"
    say "Installation failed; the previous app was restored." >&2
    exit 1
fi
if ! codesign --verify --strict "$TARGET_APP"; then
    rm -rf "$TARGET_APP"
    [ -d "$BACKUP_APP" ] && mv "$BACKUP_APP" "$TARGET_APP"
    say "The installed copy failed verification; the previous app was restored." >&2
    exit 1
fi
rm -rf "$BACKUP_APP"

# ---------------------------------------------------------------- 5. done

say ""
say "Installed: $TARGET_APP"
say "Settings and models: $HOME/Library/Application Support/$BUNDLE_ID"
if [ "$SIGNED_STABLY" = yes ]; then
    say "Signed with a stable identity, so microphone and Accessibility grants survive reinstalls."
else
    say "Ad-hoc signed: macOS will ask for permissions again after each reinstall."
fi
say ""
say "It runs in the background with no Dock icon. Look for the H in the menu bar."
say "Press Option + Space to dictate. Quit from the menu-bar item."
say ""
say "Opening it now…"
open "$TARGET_APP"
