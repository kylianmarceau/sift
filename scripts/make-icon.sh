#!/usr/bin/env bash
# Builds packaging/Sift.icns from packaging/icon.svg.
#
#   scripts/make-icon.sh
#
# Run it after editing the SVG and commit both files; the bundle scripts
# use the committed .icns and do not need this step.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cargo run --quiet --release -p sift-launcher --example render_icon \
    --manifest-path "$ROOT/Cargo.toml" -- "$ROOT/packaging/icon.svg" "$WORK/Sift.iconset"
iconutil --convert icns --output "$ROOT/packaging/Sift.icns" "$WORK/Sift.iconset"
echo "Wrote $ROOT/packaging/Sift.icns"
