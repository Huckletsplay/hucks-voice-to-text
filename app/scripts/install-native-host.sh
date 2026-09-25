#!/usr/bin/env bash
# Register Huck's Voice to Text as a Native Messaging host.
#
# This is what the real installer will do. Chrome will not talk to an extension's desktop
# counterpart unless a small JSON manifest exists in a per-browser directory, naming the
# executable and pinning the extension IDs allowed to reach it.
#
#   app/scripts/install-native-host.sh <extension-id> [path-to-binary]
#
# There is no port and no server: Chrome spawns the binary as a child process and talks to it
# over stdin/stdout. `--uninstall` removes the manifests again.

set -euo pipefail

HOST_NAME="com.huck.voicetotext"

DIRS=(
  "$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
  "$HOME/Library/Application Support/Google/Chrome Beta/NativeMessagingHosts"
  "$HOME/Library/Application Support/Microsoft Edge/NativeMessagingHosts"
  "$HOME/Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts"
  "$HOME/Library/Application Support/Chromium/NativeMessagingHosts"
)

if [[ "${1:-}" == "--uninstall" ]]; then
  for d in "${DIRS[@]}"; do
    [[ -f "$d/$HOST_NAME.json" ]] && rm -f "$d/$HOST_NAME.json" && echo "removed: $d"
  done
  exit 0
fi

EXT_ID="${1:-}"
if [[ -z "$EXT_ID" ]]; then
  echo "usage: $0 <extension-id> [path-to-binary]" >&2
  echo "       $0 --uninstall" >&2
  echo >&2
  echo "The extension id is shown on chrome://extensions after loading it unpacked." >&2
  exit 2
fi

# Prefer the installed app: that is the copy the user actually runs, and the one whose signed
# identity macOS knows. Fall back to a development build when nothing is installed yet.
INSTALLED="$HOME/Applications/Huck's Voice to Text.app/Contents/MacOS/hvtt-desktop"
if [[ -n "${2:-}" ]]; then
  BIN="$2"
elif [[ -x "$INSTALLED" ]]; then
  BIN="$INSTALLED"
else
  BIN="$HOME/.hvtt-build/target/debug/hvtt-desktop"
fi
if [[ ! -x "$BIN" ]]; then
  echo "no executable at $BIN - build it first with scripts/dev.sh build" >&2
  exit 3
fi

# Chrome execs the binary directly with no arguments, so the flag goes in a tiny launcher.
LAUNCHER_DIR="$HOME/Library/Application Support/com.huck.voice-to-text"
mkdir -p "$LAUNCHER_DIR"
LAUNCHER="$LAUNCHER_DIR/native-host"
cat > "$LAUNCHER" <<LAUNCH
#!/bin/bash
exec "$BIN" --native-messaging-host "\$@"
LAUNCH
chmod +x "$LAUNCHER"

written=0
for d in "${DIRS[@]}"; do
  parent="$(dirname "$d")"
  [[ -d "$parent" ]] || continue          # that browser is not installed
  mkdir -p "$d"
  cat > "$d/$HOST_NAME.json" <<JSON
{
  "name": "$HOST_NAME",
  "description": "Huck's Voice to Text",
  "path": "$LAUNCHER",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://$EXT_ID/"]
}
JSON
  echo "registered: $d"
  written=$((written + 1))
done

if [[ $written -eq 0 ]]; then
  echo "no Chromium browsers found" >&2
  exit 4
fi
echo
echo "Done. Restart the browser, then Huck's Voice to Text can pin fields in it."
