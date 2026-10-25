#!/usr/bin/env bash
# Builds target/release/Sift.app.
#
#   scripts/bundle-app.sh            # build and bundle (ad-hoc signed)
#   scripts/bundle-app.sh --install  # also copy to ~/Applications
#
# The bundle is ad-hoc signed, which Apple Silicon requires to run it, with
# the hardened runtime that notarization asks for. scripts/release.sh signs
# it with a Developer ID, notarizes it and packages it for distribution.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/target/release/Sift.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
# The build number is the commit count, so every commit bundles a higher one.
BUILD="$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 1)"

# Match LSMinimumSystemVersion in packaging/Info.plist.
export MACOSX_DEPLOYMENT_TARGET=13.0
cargo build --release --locked -p sift-launcher -p sift-cli --manifest-path "$ROOT/Cargo.toml"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/sift-launcher" "$APP/Contents/MacOS/Sift"
# Use a distinct name on macOS's case-insensitive filesystems. Homebrew links
# this helper as `sift`; Sift (the main executable) remains the GUI launcher.
cp "$ROOT/target/release/sift" "$APP/Contents/MacOS/sift-cli"
sed -e "s/@VERSION@/$VERSION/" -e "s/@BUILD@/$BUILD/" \
    "$ROOT/packaging/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint -s "$APP/Contents/Info.plist"
cp "$ROOT/packaging/Sift.icns" "$APP/Contents/Resources/Sift.icns"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cp "$ROOT/packaging/AeroSpace-LICENSE.txt" "$APP/Contents/Resources/AeroSpace-LICENSE.txt"
python3 "$ROOT/scripts/third-party-licenses.py" \
    "$APP/Contents/Resources/THIRD_PARTY_LICENSES.txt" >/dev/null

codesign --force --options runtime --sign - "$APP/Contents/MacOS/sift-cli" >/dev/null
codesign --force --options runtime --sign - "$APP" >/dev/null
echo "Built $APP ($VERSION, build $BUILD)"

if [[ "${1:-}" == "--install" ]]; then
    mkdir -p "$HOME/Applications"
    rm -rf "$HOME/Applications/Sift.app"
    cp -R "$APP" "$HOME/Applications/Sift.app"
    echo "Installed $HOME/Applications/Sift.app"
fi
