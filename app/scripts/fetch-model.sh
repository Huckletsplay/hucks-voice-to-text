#!/usr/bin/env bash
# Download a Whisper model into the application's data directory.
#
# Models are NEVER committed and never live on the project drive - the installed program must
# work with that SSD unplugged. They go to the OS app-data directory, which is where the app
# looks for them.
#
# Usage: scripts/fetch-model.sh [base.en|small.en|tiny.en|medium.en]   (default: base.en)

set -euo pipefail

MODEL="${1:-base.en}"
FILE="ggml-${MODEL}.bin"
URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/${FILE}"

case "$(uname)" in
  Darwin) DEST="$HOME/Library/Application Support/com.huck.voice-to-text/models" ;;
  *)      DEST="${LOCALAPPDATA:-$HOME/.local/share}/Huck's Voice to Text/models" ;;
esac

mkdir -p "$DEST"

if [[ -f "$DEST/$FILE" ]]; then
  echo "Already present: $DEST/$FILE"
  exit 0
fi

echo "Downloading $FILE to $DEST"
echo "(models are MIT licensed, from the whisper.cpp project)"
curl -fL --progress-bar -o "$DEST/$FILE.part" "$URL"
mv "$DEST/$FILE.part" "$DEST/$FILE"
echo "Done: $DEST/$FILE"
ls -lh "$DEST/$FILE"
