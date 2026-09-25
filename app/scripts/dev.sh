#!/usr/bin/env bash
# Build and run helper for Huck's Voice to Text.
#
# Two things about this machine and this drive have to be set up before cargo runs, and both
# are easy to forget, so they live here rather than in a README step.
#
#   1. BUILD OFF THE SSD. The project source sits on an exFAT volume that stores no symlinks
#      and no executable bit. Rust build output goes to the local disk instead.
#
#   2. WORK AROUND A BROKEN COMMAND LINE TOOLS C++ HEADER DIRECTORY. On some macOS installs
#      /Library/Developer/CommandLineTools/usr/include/c++/v1 holds a handful of stale files
#      and shadows the SDK's complete set, so any C++ build (whisper.cpp here) fails with
#      "'mutex' file not found". The real fix is reinstalling the Command Line Tools:
#          sudo rm -rf /Library/Developer/CommandLineTools
#          xcode-select --install
#      Until that is done, pointing the compiler at the SDK's headers is enough.
#
# Usage: scripts/dev.sh [build|build-release|run|test]   (default: run)

set -euo pipefail

APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="$(dirname "$APP_DIR")"

# 1. Build output on the local disk, never on the exFAT drive.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.hvtt-build/target}"

# 2. Repair the C++ include path only if it is actually broken.
if [[ "$(uname)" == "Darwin" ]]; then
  SDK="$(xcrun --show-sdk-path 2>/dev/null || true)"
  if [[ -n "$SDK" ]] && ! printf '#include <mutex>\nint main(){}\n' \
      | clang++ -x c++ -fsyntax-only - >/dev/null 2>&1; then
    if [[ -d "$SDK/usr/include/c++/v1" ]]; then
      echo "[hvtt] working around stale Command Line Tools C++ headers (see scripts/dev.sh)" >&2
      export CXXFLAGS="${CXXFLAGS:-} -isystem $SDK/usr/include/c++/v1"
      export CMAKE_CXX_FLAGS="${CMAKE_CXX_FLAGS:-} -isystem $SDK/usr/include/c++/v1"
    else
      echo "[hvtt] C++ headers are broken and the SDK has no replacement - reinstall the" >&2
      echo "       Command Line Tools (see the comment at the top of this script)." >&2
      exit 1
    fi
  fi
fi

# exFAT stores no extended attributes, so macOS writes AppleDouble "._name" sidecar files
# next to anything that has them. Tauri's capability scanner tries to parse them as JSON and
# fails on the binary contents, so they are pruned before every build.
find "$PROJECT_DIR" -name '._*' -delete 2>/dev/null || true
xattr -rc "$APP_DIR" 2>/dev/null || true

cd "$APP_DIR"
case "${1:-run}" in
  build)  cargo build -p hvtt-desktop ;;
  # Release build, used by install.sh. It lives here so the Command Line Tools workaround
  # above stays in exactly one place. Extra arguments let the public packager enable Tauri's
  # production custom-protocol feature without changing local development installs.
  build-release) shift; cargo build --release -p hvtt-desktop "$@" ;;
  # Extra arguments pass through, e.g. `scripts/dev.sh test -- --ignored`.
  test)   shift; cargo test --workspace "$@" ;;
  bundle) echo "Use scripts/install.sh - it assembles and signs the .app without the Tauri CLI." >&2; exit 2 ;;
  run)    cargo run -p hvtt-desktop ;;
  *)      echo "usage: scripts/dev.sh [build|build-release|run|test]" >&2; exit 2 ;;
esac
