# Changelog

## 0.4.4 (2026-10-28)

- Windows no longer disappear after using a native full-screen app. Sift keeps
  windows that macOS places on other Spaces, so Option–number works from a
  full-screen app and brings the window back.
- A window stranded at the workspace parking corner is moved back on screen.
- Switching to a workspace whose remembered window closed now focuses its most
  recently used window.
- Window shortcuts in **Settings → Window Management** are a grouped dropdown
  instead of one long list.

## 0.4.3 (2026-10-27)

- Workspace shortcuts default to Option–1…9 (Option–Shift–1…9 moves a window)
  instead of Control–1…9, which many applications and macOS features use.
- Change any window-management shortcut in **Settings → Window Management →
  Shortcuts**: choose a key cap and press the new shortcut. Conflicts with
  other bindings or Sift's launcher shortcuts are refused, and changes are
  saved to config.toml and re-registered immediately.

## 0.4.2 (2026-10-27)

- Remove a workspace app assignment by choosing it under **Assigned Apps** in
  the menu bar, without editing `config.toml`. Assignments of uninstalled apps
  stay listed by bundle identifier, so stray assignments are always removable.

## 0.4.1 (2026-10-27)

- Use workspaces and assigned-app shortcuts without automatic tiling, while
  preserving window positions when switching workspaces.

## 0.4.0 (2026-10-27)

- Use Ctrl–1…9 for workspace switching and launch assigned apps on demand;
  workspace shortcuts never open or type into the Sift launcher.
- Show saved app assignments and confirmations, including an explanation when
  window controls cannot move an app. Report blocked window shortcuts in the
  launcher and add missing numbered bindings without replacing custom shortcuts.
- Bound menu row text by its rendered width, preserve full descriptions in
  tooltips, and show the app build number to distinguish preview versions.
- Keep workspace creation and app assignment visible before window-control
  permission is granted; recheck permission after menu use and offer an explicit
  retry and a way to locate the running Sift app in Finder.
- Native menu bar icon for Sift with launcher, settings, clipboard, file-index,
  login, update, configuration, help, logs and quit actions.
- Show the active workspace or paused state beside the icon. Switch workspaces,
  focus their windows, move the focused window, and add numbered workspaces.
- Add and open installed apps in a workspace, or save an app-wide workspace
  assignment for existing and future windows across restarts.
- Manual window snapping and tiling/fullscreen controls, plus pause/resume and
  independent controls/automatic-tiling switches directly in the menu.

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
