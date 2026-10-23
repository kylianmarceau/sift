#!/usr/bin/env bash
# Points the Homebrew cask (github.com/kylianmarceau/homebrew-sift) at this
# version's release zip, then commits and pushes the tap. Run it after the
# GitHub release is published: `brew upgrade` downloads from the release.
#
#   scripts/bump-cask.sh
#
# Optional environment:
#   TAP_DIR   A checkout of the tap (default: homebrew-sift next to this
#             repository).
#
# See RELEASING.md for the whole process.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
TAP_DIR="${TAP_DIR:-$(dirname "$ROOT")/homebrew-sift}"
CASK="$TAP_DIR/Casks/sift.rb"
ZIP="Sift-$VERSION-arm64.zip"
URL="https://github.com/kylianmarceau/sift/releases/download/v$VERSION/$ZIP"

die() {
    echo "bump-cask: $*" >&2
    exit 1
}

[[ -f "$CASK" ]] || die "no cask at $CASK (set TAP_DIR, or clone the tap there)"
SHA="$(awk -v f="$ZIP" '$2 == f { print $1 }' "$ROOT/dist/SHA256SUMS" 2>/dev/null || true)"
[[ -n "$SHA" ]] || die "$ZIP is not in dist/SHA256SUMS; run scripts/release.sh first"

# Every brew user gets this file, so check that the published zip is the
# one that was built here.
PUBLISHED="$(curl -fsSL "$URL" | shasum -a 256 | cut -d' ' -f1)" ||
    die "cannot download $URL; is the release published?"
[[ "$PUBLISHED" == "$SHA" ]] || die "the published $ZIP does not match dist/SHA256SUMS"

git -C "$TAP_DIR" pull --ff-only --quiet
sed -i '' \
    -e "s/^  version \".*\"/  version \"$VERSION\"/" \
    -e "s/^  sha256 \".*\"/  sha256 \"$SHA\"/" \
    "$CASK"
if git -C "$TAP_DIR" diff --quiet; then
    echo "The cask already points at $VERSION."
    exit 0
fi
git -C "$TAP_DIR" commit --quiet -am "Update Sift to $VERSION"
git -C "$TAP_DIR" push --quiet
echo "The cask now points at $VERSION; brew upgrade will pick it up."
