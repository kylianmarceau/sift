#!/usr/bin/env bash
# Builds a release of Sift for Apple Silicon: dist/Sift-VERSION-arm64.zip,
# dist/Sift-VERSION-arm64.dmg and dist/SHA256SUMS.
#
#   scripts/release.sh
#
# Optional environment:
#   SIGN_IDENTITY   "Developer ID Application: Your Name (TEAMID)". Signs the
#                   app and disk image with your certificate. Without it the
#                   build is ad-hoc signed, and users must approve it in
#                   System Settings › Privacy & Security the first time.
#   NOTARY_PROFILE  A notarytool keychain profile, created once with
#                     xcrun notarytool store-credentials NAME --apple-id … --team-id …
#                   (that command asks for your app-specific password and
#                   keeps it in your keychain; this script never sees it).
#                   Requires SIGN_IDENTITY. Notarizes and staples the app
#                   and the disk image.
#   ALLOW_DIRTY=1   Build from a working tree with uncommitted changes
#                   (the result is then marked as not reproducible).
#   SKIP_CHECKS=1   Skip formatting, Clippy and tests.
#
# See RELEASING.md for the whole process.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
DIST="$ROOT/dist"
APP="$ROOT/target/release/Sift.app"
NAME="Sift-$VERSION-arm64"
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
NOTARY_PROFILE="${NOTARY_PROFILE:-}"

step() { printf '\n== %s\n' "$*"; }
fail() { echo "error: $*" >&2; exit 1; }

[[ "$(uname -m)" == "arm64" ]] || fail "build on an Apple Silicon Mac"
if [[ -n "$NOTARY_PROFILE" && -z "$SIGN_IDENTITY" ]]; then
    fail "NOTARY_PROFILE needs SIGN_IDENTITY: Apple notarizes only Developer ID signed apps"
fi
if [[ -n "$(git -C "$ROOT" status --porcelain)" && "${ALLOW_DIRTY:-}" != "1" ]]; then
    fail "the working tree has uncommitted changes (set ALLOW_DIRTY=1 to build anyway)"
fi
if [[ -n "$SIGN_IDENTITY" ]]; then
    security find-identity -v -p codesigning | grep -qF "$SIGN_IDENTITY" \
        || fail "no signing identity \"$SIGN_IDENTITY\" in the keychain"
fi
TAG="v$VERSION"
if [[ "$(git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG^{commit}" 2>/dev/null)" \
      != "$(git -C "$ROOT" rev-parse HEAD)" ]]; then
    echo "note: HEAD is not tagged $TAG; tag it before publishing (see RELEASING.md)"
fi

if [[ "${SKIP_CHECKS:-}" != "1" ]]; then
    step "Checks"
    cargo fmt --all --check --manifest-path "$ROOT/Cargo.toml"
    cargo clippy --workspace --all-targets --locked --manifest-path "$ROOT/Cargo.toml" -- -D warnings
    cargo test --workspace --locked --manifest-path "$ROOT/Cargo.toml"
fi

step "Bundle"
"$ROOT/scripts/bundle-app.sh"

sign() {
    # --timestamp: a secure timestamp, which notarization requires.
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$@"
}
notarize() {
    xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait
}

if [[ -n "$SIGN_IDENTITY" ]]; then
    step "Sign the app with $SIGN_IDENTITY"
    sign "$APP/Contents/MacOS/sift-cli"
    sign "$APP"
fi
codesign --verify --deep --strict --verbose=2 "$APP"

rm -rf "$DIST"
mkdir -p "$DIST"

if [[ -n "$NOTARY_PROFILE" ]]; then
    step "Notarize the app"
    ditto -c -k --keepParent "$APP" "$DIST/notarize.zip"
    notarize "$DIST/notarize.zip"
    rm "$DIST/notarize.zip"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
fi

step "Package"
# ditto keeps the bundle's signature and extended attributes intact.
ditto -c -k --keepParent "$APP" "$DIST/$NAME.zip"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "Sift $VERSION" -srcfolder "$STAGE" -fs HFS+ \
    -format UDZO -ov "$DIST/$NAME.dmg"
if [[ -n "$SIGN_IDENTITY" ]]; then
    codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DIST/$NAME.dmg"
fi
if [[ -n "$NOTARY_PROFILE" ]]; then
    step "Notarize the disk image"
    notarize "$DIST/$NAME.dmg"
    xcrun stapler staple "$DIST/$NAME.dmg"
fi

step "Verify"
if [[ -n "$NOTARY_PROFILE" ]]; then
    # What Gatekeeper decides for a downloaded copy.
    spctl --assess --type execute --verbose=2 "$APP"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$DIST/$NAME.dmg"
    STATUS="signed and notarized"
elif [[ -n "$SIGN_IDENTITY" ]]; then
    STATUS="signed, not notarized"
else
    STATUS="ad-hoc signed (not notarized)"
fi
(cd "$DIST" && shasum -a 256 "$NAME.zip" "$NAME.dmg" > SHA256SUMS)
if [[ -n "$(git -C "$ROOT" status --porcelain)" ]]; then
    STATUS="$STATUS, from a working tree with uncommitted changes"
fi
echo "$STATUS" > "$DIST/BUILD-STATUS"

step "Done: $STATUS"
ls -lh "$DIST"
cat "$DIST/SHA256SUMS"
