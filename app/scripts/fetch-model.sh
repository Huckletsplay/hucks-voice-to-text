#!/usr/bin/env bash
# Download a Whisper model into the application's data directory.
#
# Models are NEVER committed and never live on the project drive - the installed program must
# work with that SSD unplugged. They go to the OS app-data directory, which is where the app
# looks for them.
#
# Usage: scripts/fetch-model.sh [model]
#   no model: the built-in default, large-v3-turbo-q5_0 ("Best"), and the voice detector
#   silero-v5.1.2 (hvtt_core::models) - what a fresh source install needs to dictate.
#   Or one of: tiny.en ("Tiny"), base.en ("Quick"), small.en ("Better"), medium.en-q5_0 ("Medium"),
#   large-v3-turbo-q5_0 ("Best"), large-v3-q5_0 ("Large"), silero-v5.1.2.

set -euo pipefail

if [[ $# -eq 0 ]]; then
  bash "$0" large-v3-turbo-q5_0
  exec bash "$0" silero-v5.1.2
fi
MODEL="$1"
FILE="ggml-${MODEL}.bin"
# The files the program knows, pinned exactly as hvtt_core::models pins them: one commit, a size
# and a SHA-256. Anything else is fetched from the project's main branch, unchecked.
PINNED="https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1"
case "$MODEL" in
  tiny.en)             SIZE=77704715;  SHA=921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f; URL="$PINNED/$FILE" ;;
  medium.en-q5_0)      SIZE=539225533; SHA=76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0; URL="$PINNED/$FILE" ;;
  large-v3-q5_0)       SIZE=1081140203; SHA=d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1; URL="$PINNED/$FILE" ;;
  base.en)             SIZE=147964211; SHA=a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002; URL="$PINNED/$FILE" ;;
  small.en)            SIZE=487614201; SHA=c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d; URL="$PINNED/$FILE" ;;
  large-v3-turbo-q5_0) SIZE=574041195; SHA=394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2; URL="$PINNED/$FILE" ;;
  silero-v5.1.2)       SIZE=885098;    SHA=29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf
                       URL="https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/$FILE" ;;
  *)                   SIZE=""; SHA=""; URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$FILE"
                       echo "Note: $MODEL is not one the program knows; it is not checked." ;;
esac
# A file is good if it is the published one (when known) - an empty or broken one is not.
good() {
  [[ -s "$1" ]] || return 1
  [[ -z "$SHA" ]] && return 0
  [[ "$(wc -c < "$1" | tr -d ' ')" == "$SIZE" ]] && [[ "$(shasum -a 256 "$1" | cut -c1-64)" == "$SHA" ]]
}

case "$(uname)" in
  Darwin) DEST="$HOME/Library/Application Support/com.huck.voice-to-text/models" ;;
  *)      DEST="${LOCALAPPDATA:-$HOME/.local/share}/Huck's Voice to Text/models" ;;
esac

mkdir -p "$DEST"

if [[ -f "$DEST/$FILE" ]]; then
  if good "$DEST/$FILE"; then
    echo "Already present: $DEST/$FILE"
    exit 0
  fi
  echo "The copy in $DEST is not the published file - downloading it again."
  rm -f "$DEST/$FILE"
fi

echo "Downloading $FILE to $DEST"
echo "(models are MIT licensed, from the whisper.cpp project)"
curl -fL --proto '=https' --progress-bar -o "$DEST/$FILE.part" "$URL"
if ! good "$DEST/$FILE.part"; then
  rm -f "$DEST/$FILE.part"
  echo "The download is not the published file (size or SHA-256), so it was deleted." >&2
  exit 1
fi
mv "$DEST/$FILE.part" "$DEST/$FILE"
echo "Done: $DEST/$FILE"
ls -lh "$DEST/$FILE"
