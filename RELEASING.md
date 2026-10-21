# Releasing Sift

Releases are built on an Apple Silicon Mac with `scripts/release.sh` and
published as GitHub releases. The app's update check reads the latest
GitHub release, so publishing one is what tells existing users about it.

## One-time setup (for notarized releases)

Without these, a release is ad-hoc signed: it works, but users must click
**Open Anyway** in System Settings › Privacy & Security the first time.

1. Join the Apple Developer Program and create a **Developer ID
   Application** certificate (Xcode › Settings › Accounts › Manage
   Certificates, or developer.apple.com). Check that it is in your
   keychain:

   ```bash
   security find-identity -v -p codesigning
   ```

2. Create an app-specific password at account.apple.com, then store
   notarization credentials in your keychain under a profile name. The
   command asks for the password itself; the release script only ever
   sees the profile name.

   ```bash
   xcrun notarytool store-credentials sift-notary --apple-id YOUR_APPLE_ID --team-id YOUR_TEAM_ID
   ```

## Making a release

1. **Version.** Set `version` in the root `Cargo.toml` (it is shared by
   every crate) and run `cargo check` to update `Cargo.lock`. In
   `CHANGELOG.md`, replace "(unreleased)" after the version with today's
   date. Commit.

2. **Tag** the commit:

   ```bash
   git tag -a v0.1.0 -m "Sift 0.1.0"
   ```

3. **Build.** From a clean working tree:

   ```bash
   # Ad-hoc signed
   scripts/release.sh

   # Signed and notarized
   SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" NOTARY_PROFILE=sift-notary scripts/release.sh
   ```

   The script runs formatting, Clippy and the tests, builds the bundle,
   signs, notarizes and staples (when asked), and writes to `dist/`:

   - `Sift-VERSION-arm64.dmg` and `Sift-VERSION-arm64.zip`
   - `SHA256SUMS`
   - `BUILD-STATUS`: how the build was signed. Put it in the release
     notes.

4. **Check** the download as a user would get it: copy the `.dmg` to
   another Mac (or another user account), open it, drag Sift to
   Applications and launch it. For a notarized build there should be no
   warning at all.

5. **Publish.** Push the commit and the tag, then create the GitHub
   release from the tag and attach the three files:

   ```bash
   git push origin main v0.1.0
   gh release create v0.1.0 dist/Sift-0.1.0-arm64.dmg dist/Sift-0.1.0-arm64.zip dist/SHA256SUMS --title "Sift 0.1.0" --notes-file NOTES.md
   ```

   Mark test builds as pre-releases (`--prerelease`): the update check
   ignores drafts and pre-releases.

## Notes

- **The repository must be public** for the update check to see
  releases. While it is private, the GitHub API answers 404 and Sift
  reports that no newer release exists.
- Version tags must look like `v1.2.3`. The update check compares them
  numerically and ignores tags it cannot read.
- The release binary keeps its symbols, so crash reports in
  `~/Library/Logs/Sift/sift.log` name the functions involved.
- Only Apple Silicon builds are made. Intel Macs are not supported; the
  build has not been tested there.
