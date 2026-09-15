#!/usr/bin/env bash
# Build a double-clickable macOS app bundle: "RaptorQR Receiver.app".
#
# Usage: host/scripts/package_macos_app.sh [output-dir]
set -euo pipefail

HOST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HOST_DIR"

OUT_DIR="${1:-$HOST_DIR/../release}"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
APP_NAME="RaptorQR Receiver"
BUNDLE_ID="com.raptorqr.receiver"
VERSION="0.1.0"

echo "==> building release binary"
cargo build --release

echo "==> assembling $OUT_DIR/$APP_NAME.app"
APP="$OUT_DIR/$APP_NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/raptorqr-host "$APP/Contents/MacOS/raptorqr-host"

if [ -f assets/RaptorQR.icns ]; then
  cp assets/RaptorQR.icns "$APP/Contents/Resources/RaptorQR.icns"
  ICON_ENTRY='  <key>CFBundleIconFile</key><string>RaptorQR</string>'
else
  ICON_ENTRY=''
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>RaptorQR Receiver</string>
  <key>CFBundleDisplayName</key><string>RaptorQR Receiver</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>raptorqr-host</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleSignature</key><string>????</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
$ICON_ENTRY
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null

# Prefer a real (stable) signing identity: macOS only remembers the Screen
# Recording grant for an app whose code requirement stays the same, so ad-hoc
# signing means re-granting after every rebuild. Override with
# RAPTORQR_SIGN_IDENTITY=<name> if needed.
IDENTITY="${RAPTORQR_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
  for candidate in "RaptorQR Local Signing" "TVA Local Signing"; do
    if security find-identity -v -p codesigning 2>/dev/null | grep -q "$candidate"; then
      IDENTITY="$candidate"
      break
    fi
  done
fi

if [ -n "$IDENTITY" ]; then
  echo "==> code signing with identity: $IDENTITY"
  codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
else
  echo "==> ad-hoc code signing (no stable identity found — macOS will ask for"
  echo "    Screen Recording permission again after every rebuild)"
  codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"
fi

echo "==> verifying"
codesign --verify --strict --verbose=1 "$APP"
echo
echo "bundle:  $APP"
echo "binary:  $APP/Contents/MacOS/raptorqr-host ($(du -h "$APP/Contents/MacOS/raptorqr-host" | cut -f1))"
echo "launch:  open \"$APP\""
