#!/usr/bin/env bash
# Builds a local development bundle: target/release/Sift.app
#
#   scripts/bundle-app.sh            # build and bundle
#   scripts/bundle-app.sh --install  # also copy to ~/Applications
#
# The bundle is ad-hoc signed (required to run on Apple Silicon). It is
# not notarized and not meant for distribution.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/target/release/Sift.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"

cargo build --release -p sift-launcher --manifest-path "$ROOT/Cargo.toml"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/sift-launcher" "$APP/Contents/MacOS/Sift"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Sift</string>
    <key>CFBundleDisplayName</key>
    <string>Sift</string>
    <key>CFBundleIdentifier</key>
    <string>dev.sift.Sift</string>
    <key>CFBundleExecutable</key>
    <string>Sift</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <!-- Background utility: no Dock icon, no menu bar. -->
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$APP" >/dev/null
echo "Built $APP"

if [[ "${1:-}" == "--install" ]]; then
    mkdir -p "$HOME/Applications"
    rm -rf "$HOME/Applications/Sift.app"
    cp -R "$APP" "$HOME/Applications/Sift.app"
    echo "Installed $HOME/Applications/Sift.app"
fi
