# Changelog

All notable changes are listed here. Versions follow
[Semantic Versioning](https://semver.org).

## Unreleased

- Change the keyboard shortcut in Settings › General: click it and press
  a new one. It works by key position, so it is the same on any keyboard
  layout.

## 0.1.0 (2026-10-22)

The first release. Apple Silicon, macOS 13 or later.

**Launcher**

- Option+Space opens a search panel for applications, files and folders,
  ranked by match quality, your favorites and what you open often.
- Favorites (⌘D) and recent items when nothing is typed.
- Quick Look preview (⌘Y), show in Finder (⌘Enter).
- Calculator, snippets with date and time placeholders, and custom
  commands (open a URL, a file or an app, or run a shell command) from
  `config.toml`.
- Optional clipboard history, off by default, which skips text that
  password managers mark as concealed.
- File filters: `ext:`, `type:`, `in:`, `size:`, `modified:`.

**File search**

- A SQLite-backed index with a memory-mapped name snapshot for search.
- Live updates through FSEvents: a new, renamed or deleted file shows up
  in search about 0.2 s later (median, measured; see BENCHMARKS.md).
- Dependency and cache folders, `~/Library` and the Trash are skipped by
  default.
- Crash recovery: a damaged index is set aside and rebuilt, and the index
  is checked after an unclean exit.
- The `sift` command-line tool: index, search, status, remove, verify.

**Mac app**

- A signed app bundle with its own icon, no Dock icon and no special
  permissions.
- Settings for search folders, Open at Login (off by default) and update
  checks.
- A daily check for a newer release, which can be turned off. Sift
  only reports a new version; it never downloads or installs anything.
- Light and dark themes that follow the system accent colour, Increase
  Contrast and Differentiate Without Color.
- VoiceOver announcements for the highlighted result, result counts and
  messages.
- A log at `~/Library/Logs/Sift/sift.log`; no telemetry.
