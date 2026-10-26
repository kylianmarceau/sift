# Changelog

## 0.3.0 (2026-10-26)

- Pin clipboard entries with ⌘D, preserving them beyond the history limit and
  Clear History; delete individual entries with Ctrl-X.
- Record the source app, filter with `clip from:`, and exclude apps using
  `[clipboard].ignore_apps`.
- Search clipboard history with loose matches and preview selected entries,
  including content icons, colour swatches, and actions for links and paths.
- Configure a dedicated clipboard-history shortcut and a Paste and Pop shortcut
  that pastes into the frontmost app and advances to the previous unpinned entry.
- Keep the system clipboard in sync when deleting or clearing its current entry.
- Upgrade the state database with a backup of the previous schema for rollback.
- Navigate results with Ctrl-J/Ctrl-K alongside the existing arrow and Ctrl-N/P keys.
- Stop ignoring obsolete graphify output.

## 0.2.2 (2026-10-25)

- Acknowledge an already-restored tiling recovery entry immediately. A no-op AX
  frame write emits no notification; waiting for one could retain a stale journal
  that later restored old positions after manual movement and restart.
- Native regression coverage for a clean automatic shutdown, manual startup,
  native manual movement, and another restart without replaying stale positions.

## 0.2.1 (2026-10-25)

- Separate Window Shortcuts and Controls from Automatic Tiling. Controls now
  default to manual mode: no automatic tiling, parking, routing or mouse actions.
- Persistent app/title targets for global shortcuts and `sift wm --target`
  actions, resolved against current windows after closure/reopening and app restart.
- Launch a missing target's app when possible and wait for its matching window;
  bounded/coalesced requests and pending binding sequences avoid acting on an
  unrelated focused window while an app opens.
- Manual half/quarter-screen snapping, centering, maximize/restore, movement,
  resizing, focus and fullscreen. Manual positions persist when controls stop.
- Turning Automatic Tiling off restores its positions while retaining manual
  shortcuts; prior manual positions become the next tiling session's originals.

## 0.2.0 (2026-10-25)

- Optional native Rust window management: nested tiles and accordion layouts,
  floating windows, workspaces, monitor assignments, application rules, binding
  modes, and commands for focus, moving, resizing and fullscreen.
- Open-window and workspace search (`win` / `ws`), Settings controls, `sift wm`
  and an app-bundled command client (`Sift --wm`).
- Per-application AX workers and coalesced geometry updates; recovery positions
  are saved before arranging windows and restored when disabled or quitting.
- Per-display gaps, bounded startup/window/focus/workspace callbacks, optional
  focus-follow-mouse, cursor positioning/following and native drag/resize handling.
- Isolated native smoke coverage for actual frames, focus, workspace switching,
  minimize/restore, stalled applications, pause/resume and shutdown restoration.
- Homebrew now installs the standalone `sift` CLI bundled inside Sift.app,
  including `sift wm` commands. File search/index commands remain available.
- Window management is experimental and off by default. Enabling requires
  Device Control and Data Access on macOS 27, or Accessibility on older macOS.
  Broader native application, multi-monitor and physical-gesture acceptance
  remains pending; AeroSpace configuration/CLI compatibility is not complete.

All notable changes are listed here. Versions follow
[Semantic Versioning](https://semver.org).

## 0.1.4 (2026-10-25)

- Replace the app icon with Sieve: three graphite bars and a cobalt-blue
  square on an ivory background, with artwork for every macOS icon size.

## 0.1.3 (2026-10-24)

- Settings has a Clipboard History section: turn it on or off, choose
  how many entries to keep, and clear it. Settings › General can turn
  the automatic update check on or off and open `config.toml`. These
  edit `config.toml` in place and keep your comments, snippets and
  commands; a file with a mistake in it is left alone.

## 0.1.2 (2026-10-23)

- `config.toml` can live in `~/.config/sift` (or `$XDG_CONFIG_HOME/sift`)
  for XDG-style dotfiles. Sift uses it there when it exists; otherwise
  nothing changes.

## 0.1.1 (2026-10-22)

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
