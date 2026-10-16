#!/usr/bin/env bash
# Build the latest receiver and install it into /Applications.
#
# Run this after EVERY change to host/ — the bundle in /Applications is what
# actually gets double-clicked, so a stale copy there keeps running the old
# code (and the old bugs) no matter how green the working tree is.
#
# Usage: host/scripts/install_macos_app.sh        (or: pnpm host:install)
set -euo pipefail

HOST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REPO_DIR="$(cd "$HOST_DIR/.." && pwd)"
APP_NAME="RaptorQR Receiver"
SRC="$REPO_DIR/release/$APP_NAME.app"
DEST="/Applications/$APP_NAME.app"

# Refuse to rm -rf anything but the exact install target.
if [ "$DEST" != "/Applications/RaptorQR Receiver.app" ]; then
  echo "refusing to install to unexpected path: $DEST" >&2
  exit 1
fi

echo "==> [1/4] build + package (release, universal, signed)"
"$HOST_DIR/scripts/package_macos_app.sh" "$REPO_DIR/release"

# Code signing embeds the signing time, so re-signing identical source yields
# different bytes. Re-zip right here, so the distributable zip is exactly the
# artifact that gets installed (their sha256 must match afterwards).
echo "==> [2/4] refresh the distributable zip"
ZIP="$REPO_DIR/release/RaptorQR-Receiver-macos.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$SRC" "$ZIP"

echo "==> [3/4] install into /Applications"
if pgrep -x raptorqr-host >/dev/null 2>&1; then
  echo "    quitting the running receiver first"
  osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || pkill -x raptorqr-host || true
  for _ in 1 2 3 4 5; do
    pgrep -x raptorqr-host >/dev/null 2>&1 || break
    sleep 1
  done
fi
rm -rf "$DEST"
ditto "$SRC" "$DEST"

echo "==> [4/4] verify"
codesign --verify --strict "$DEST"
plutil -lint "$DEST/Contents/Info.plist" >/dev/null
BIN="$DEST/Contents/MacOS/raptorqr-host"
echo
echo "installed: $DEST"
echo "arch:      $(lipo -archs "$BIN" 2>/dev/null || echo native)"
echo "built at:  $(date -r "$BIN" '+%Y-%m-%d %H:%M:%S')"
echo "sha256:    $(shasum -a 256 "$BIN" | cut -d' ' -f1)"
echo "           (must equal release/RaptorQR Receiver.app/Contents/MacOS/raptorqr-host)"
echo "launch:    open \"$DEST\""
