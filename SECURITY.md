# Security

## Reporting a vulnerability

Please report security problems privately through GitHub: on the
repository's **Security** tab, choose **Report a vulnerability**. Do not
open a public issue.

Include the Sift version, your macOS version, and the steps that show the
problem. Fixes go into the next release, and the release notes credit you
unless you prefer otherwise.

Only the latest release is supported.

## What Sift does that matters for security

- **Network.** One HTTPS request at most once a day, to
  `api.github.com`, to read the latest release's version number (see
  *Updates and privacy* in the README). It can be turned off with
  `[updates] check = false`. Only HTTPS is allowed, including after
  redirects. Only the version string of the reply is used; the page Sift
  opens is a fixed URL, never one taken from the reply. Sift downloads
  and installs nothing.
- **Permissions.** No Accessibility, Full Disk Access, Screen Recording
  or Input Monitoring. macOS's own folder prompts appear when you add
  Documents, Desktop or Downloads. Open at Login uses `SMAppService` and
  only when you turn it on.
- **Local data.** The index, recents, favorites and clipboard history
  stay in `~/Library/Application Support/sift`; logs stay in
  `~/Library/Logs/Sift`. Clipboard history is off by default and never
  keeps text that password managers mark as concealed.
- **Untrusted input.** File names, file metadata, the configuration file
  and the update reply are treated as untrusted. File names are never
  passed through a shell. Custom commands in `config.toml` do run shell
  commands: that file is yours to edit, and Sift runs only what you wrote
  in it.
- **Signing.** Release builds use the hardened runtime. A build signed
  with a Developer ID is also notarized; the release notes say which
  kind a release is. `SHA256SUMS` lists the checksums of each download.
