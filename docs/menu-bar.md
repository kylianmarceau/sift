# Sift's menu bar

Sift stays in the macOS menu bar while the launcher is hidden. Click it to open
the native menu; arrow keys navigate it and Escape closes it. Long titles are
truncated by rendered width; hover a truncated row to read all of it.

## The icon

With window controls off, the icon is Sift's sieve glyph. With them running, it
shows your workspaces instead: the current one in a filled badge, and the other
workspaces that have windows as dimmed labels beside it. For example
`1 [2] web` means you are on workspace 2, and 1 and web have windows. A paused
manager draws the current badge as an outline. With more than eight workspaces
in use, only the current one is shown. All of these are template images, so
they follow the menu bar's light and dark appearance. The tooltip shows the
version and window-control status.

## The menu, top to bottom

- **Open Sift** and **Search Clipboard History**, with their shortcuts. Clipboard
  search appears only while clipboard history is recording.
- **Workspaces** (while window controls are running). Every configured
  workspace, with the apps on it in dimmed text, its shortcut (Option-1…), and a
  checkmark on the ones visible on a monitor. Click one to switch to it. Numeric
  names sort numerically, including 10 and 11. The header notes a paused manager
  or a non-main binding mode; when paused, workspaces are listed but disabled
  and **Resume Window Management** appears below them.
  - **Windows** lists every managed window by workspace, with its title.
    Choosing one focuses it, switching workspace when needed. **Search
    Windows…** opens Sift's window search.
  - **Focused Window** acts on the window that was focused when the menu was
    built: **Move to Workspace** (including a new one), **Always Open APP In**
    (an app-wide assignment), and arranging: snapping, centering, maximize,
    restore and fullscreen in manual mode, or floating/tiling and fullscreen
    with Automatic Tiling.
- **Window Management**: the on/off switches for **Window Shortcuts and
  Controls** and **Automatic Tiling**, **Pause**/**Resume**, **Assign Apps to
  Workspaces**, **Add Workspace N**, **Add Missing Workspace Shortcuts**,
  **Edit Rules and Shortcuts…**, **Reload Configuration** and the guide.
- **Clipboard History** (with its entry count): search, record on/off, and
  clear unpinned entries. History is off by default.
- **Search Folders**: indexed-item counts and each folder's state (indexing,
  unavailable, failed), and **Manage Search Folders…**.
- **Settings…**, **Check for Updates…**, **Open at Login**, **Help** (version
  and build number, guides, configuration file, logs, and the running app in
  Finder), and **Quit Sift**.

Messages from the last action (a saved assignment, an error) appear at the top
until the next action; errors have a **Dismiss** item.

## Assigning apps to workspaces

**Window Management → Assign Apps to Workspaces** lists assigned apps first,
with their workspace, then every installed app by first letter. Each app opens
its list of workspaces: the current assignment is checked, choosing another
reassigns the app, and **Don't Assign APP** removes the assignment, leaving the
app's windows where they are. **Focused Window → Always Open APP In** is the
same list for the focused app. Apps that are assigned but not installed are
listed by bundle identifier, so stray assignments stay removable.

Assigning works before window controls are running: it saves the rule without
moving windows, and the message says so. With controls running, the app's
existing windows move to the workspace, Sift switches there, and an app with
no window is opened.

Switching to a workspace launches an assigned app when it has no managed window.
An app already open in that workspace is focused; an assigned app's existing
windows elsewhere are moved there. Newly launched apps are focused only if the
workspace is still active when their window appears.

Assignments are exact bundle-ID routing rules at the beginning of
`[[window_manager.rules]]`. An explicit menu assignment takes precedence over
later rules for that app, and reassigning replaces its previous simple app-wide
rule. Title-specific and floating rules, settings, snippets, unrelated rules
and comments are preserved. Invalid configuration is left unchanged, and the
menu reports save errors. Changes apply immediately and persist across
restarts. Include/exclude app filters still apply.

**Add Workspace N** saves the next unused positive number. If a custom
`[window_manager.bindings.main]` table replaced the numbered shortcuts, **Add
Missing Workspace Shortcuts** adds Option-1…9, their Shift variants for moving
windows, and Ctrl-Option-Tab where those keys are free. Existing bindings are
kept.

## When window controls are not running

If **Window Shortcuts and Controls** is on but the controls cannot start (most
often missing permission), the menu shows why, with **Grant Window Control
Permission…**, **Retry Window Controls** and **Show This Sift App in Finder**.
A preview build and the installed app can have different signing identities.
Ad-hoc builds acquire a new code identity when rebuilt; macOS may require
removing the stale Sift entry and adding the current app again. Closing the menu
also picks up a permission grant, without a polling timer. See
[Apple's code-signing requirements note](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).

## Implementation and validation

The menu reads cached launcher, index and window-manager state and listens for
their publications. It does not poll or run shell commands. Window moves and
resizes do not rebuild it: only changes it shows (windows, titles, focus,
workspaces, status) do. The app-assignment tree is rebuilt only when the app
catalog, workspace names or assignments change. AppKit items are created when
the menu opens, and each submenu only when it is first opened, so listing every
installed app costs nothing until you look at it.

Menu contents stay fixed while the menu is open, so background window changes
cannot move an action under your pointer. A targeted window's identity is
checked again before dispatch; a closed window produces a message instead of
acting on a different app.

Run the main-thread AppKit smoke check with:

```sh
cargo run -p sift-macos --example status_menu_check
```

It checks the glyph and workspace icons, section headers, dimmed detail text,
Option shortcuts, lazily filled nested menus, checkmarks, symbols, action
dispatch, stable contents during tracking, deferred refresh and teardown,
without opening a menu or sending input to other applications. Set
`SIFT_STATUS_PNG_DIR` to save the icons as PNGs for a visual check. Launcher
tests cover the menu model: workspace commands and shortcuts, paused actions,
focused-window actions, the assignment picker and stale window identities.

The workspace summaries, active-workspace indicator, pause, configuration and
reload controls take inspiration from
[AeroSpace's native menu](https://github.com/nikitabobko/AeroSpace/blob/main/Sources/AppBundle/ui/MenuBar.swift).
