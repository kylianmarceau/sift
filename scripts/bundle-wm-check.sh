#!/usr/bin/env bash
# Build a stable app identity for the isolated native Accessibility checks.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CHECK_APP="$ROOT/target/debug/Sift WM Check.app"
cargo build --locked --manifest-path "$ROOT/Cargo.toml" -p sift-macos \
    --example wm_fixture --example wm_native_check --example wm_check_runner
# Preserve the permission-bearing launcher across backend/helper rebuilds.
# Ad-hoc signing hashes change after rebuilding, which invalidates a grant.
if [[ -f "$CHECK_APP/Contents/Info.plist" ]] && \
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :SiftWMCheckRunnerVersion' "$CHECK_APP/Contents/Info.plist" 2>/dev/null || true)" == "1" ]] && \
    [[ "${1:-}" != "--refresh-runner" ]]; then
    echo "Updated native helpers; preserved $CHECK_APP and its permission identity."
    exit 0
fi
mkdir -p "$CHECK_APP/Contents/MacOS" "$CHECK_APP/Contents/Resources"
cp "$ROOT/target/debug/examples/wm_check_runner" "$CHECK_APP/Contents/MacOS/WMCheck"
cp "$ROOT/packaging/Sift.icns" "$CHECK_APP/Contents/Resources/Sift.icns"
cat > "$CHECK_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>io.github.kylianmarceau.sift.wm-check</string>
<key>CFBundleExecutable</key><string>WMCheck</string>
<key>CFBundleName</key><string>Sift WM Check</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>SiftWMCheckRunnerVersion</key><string>1</string>
<key>CFBundleIconFile</key><string>Sift.icns</string>
<key>LSUIElement</key><true/>
<key>LSMinimumSystemVersion</key><string>13.0</string>
</dict></plist>
PLIST
plutil -lint -s "$CHECK_APP/Contents/Info.plist"
codesign --force --sign - "$CHECK_APP" >/dev/null
echo "Built $CHECK_APP"
echo "Allow Sift WM Check in Privacy & Security → Device Control and Data Access (macOS 27), or Accessibility (older macOS)."
echo "Then run: scripts/run-wm-check.sh"
