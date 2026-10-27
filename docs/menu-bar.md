# Sift's menu bar

Sift's Sieve icon stays in the macOS menu bar while the launcher is hidden. It
uses a template image so it follows the menu bar's light/dark appearance. Click
it to open the native menu; arrow keys navigate it and Escape closes it.
The header includes the build number so previews with the same release version
can be distinguished. Long row titles are truncated by rendered width; hover to
read their full text in a tooltip.

## Everyday actions

- **Open Sift** opens normal search. **Settings…** opens Sift's settings window.
- **Clipboard History** opens clipboard search, toggles recording, or clears
  unpinned entries. Clipboard history remains off by default.
- **File Index** shows indexed-item counts and the state of each search folder,
  including indexing, unavailable folders and failures. **Manage Search Folders…**
  opens Settings.
- **Open at Login** uses macOS Login Items; when macOS needs approval, open
  Settings from the menu. This option requires an installed app bundle.
- **Check for Updates…** shows progress/results in the launcher and opens the
  release page when a newer version is available. It does not install updates.
- **Help** opens the guides, configuration or logs. **Quit Sift** runs the normal
  shutdown and window-restoration path.

## Workspaces

**Workspace Apps and Windows** is always available. You can create workspaces
and use **Assign App…** to save app assignments before granting window-control
permission or enabling tiling. These setup actions do not move windows or launch
apps. With workspace control running, **Add and Open App…** also opens the app.
Saved assignments appear under **Assigned Apps**; choosing one there removes the
assignment, leaving that app's windows where they are. Apps that are assigned
but not installed stay listed by bundle identifier, so stray assignments are
always removable. Assigned apps also have checkmarks in the app picker. When
controls are unavailable, the menu bar shows a checkmark and the
menu explains that the assignment was saved without moving a window.

Turn on **Window Management → Window Shortcuts and Controls** and grant macOS
window-control permission. Workspaces work with Automatic Tiling on or off.

The active monitor's workspace appears beside the Sieve icon; paused management
shows a pause mark. **Workspaces** lists all configured workspaces with the apps
they contain. A checkmark marks a workspace currently visible on a monitor.
Click a workspace to switch to it. Numeric names sort numerically, including
10 and 11. A non-main binding mode also appears beside the workspace indicator.

Under **Workspace Apps and Windows → Workspace N**:

- **Move Focused Window Here** moves the window that was focused when the menu
  was built, leaving you on your current workspace.
- **Always Put APP Here** saves an app-wide assignment, moves that app's existing
  managed windows, and switches to the destination workspace.
- **Add and Open App…** lists installed apps alphabetically. Choosing one saves
  the same assignment and opens or activates the app in that workspace.
- **Assigned Apps** lists saved assignments; choosing one removes that app's
  assignment. Its windows stay where they are.
- Window entries focus the chosen window, switching workspace when needed.

**Add Workspace N** saves the next unused positive number. Use it again to add
more workspaces. **Edit Workspace Names and Rules…** opens `config.toml` for
custom names and keyboard bindings.

If a custom `[window_manager.bindings.main]` table replaced the numbered
shortcuts, choose **Add Missing Workspace Shortcuts**. This adds Option–1…9,
their Shift variants for moving windows, and Ctrl–Option–Tab where those keys are
missing. Existing bindings are preserved. Window shortcuts do not open the Sift
search panel; permission or paused-state errors appear in the menu status.

Switching to a workspace launches an assigned app when it has no managed window.
An app already open in that workspace is focused; an assigned app's existing
windows elsewhere are moved there. Newly launched apps are focused only if the
workspace is still active when their window appears.

Assignments are exact bundle-ID routing rules at the beginning of
`[[window_manager.rules]]`. An explicit menu assignment takes precedence over
later rules for that app. Reassigning the same app replaces its previous simple
app-wide rule; removing it from **Assigned Apps** deletes that rule.
Title-specific and floating rules remain in the file. Settings,
snippets, unrelated rules and comments are preserved. Invalid configuration is
left unchanged, and the menu reports save errors. Changes apply immediately and
persist across app and Sift restarts. Existing include/exclude app filters still
apply; apps without normal managed windows may not appear in window lists.

## Window controls

**Focused Window** shows the selected app/window. Manual mode offers left/right
snapping, centering, maximize/restore and fullscreen. Tiling mode offers
floating/tiling and fullscreen. **Search Open Windows** opens Sift's window search.
**Pause Window Management** suspends actions; **Resume** restores them. Workspace
actions are disabled while paused. Turning controls off uses the existing
window-restoration behavior.

If Settings shows Sift enabled but controls still report missing permission,
check the running copy with **Show This Sift App in Finder**. A preview build and
the installed app can have different signing identities. Ad-hoc builds acquire a
new code identity when rebuilt; macOS may require removing the stale Sift entry
and adding the current app again. **Retry Window Controls** rechecks permission,
and closing the menu also picks up a permission grant without a polling timer.
See [Apple's code-signing requirements note](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).

Menu contents stay fixed while the menu is open, so background window changes
cannot move an action under your pointer. A targeted window's identity is checked
again before dispatch; a closed window produces a message instead of acting on a
different app. Changes become visible on the next menu opening.

## Implementation and validation

The menu reads cached launcher/index/window-manager state and listens for their
publications. It does not poll or run shell commands. Geometry-only window
updates do not regenerate menu models; AppKit menu items are built only when
opening the menu. The status-item owner removes the icon on teardown.

Run the main-thread AppKit smoke check with:

```sh
cargo run -p sift-macos --example status_menu_check
```

It checks icon/template setup, nested menus, checkmarks, symbols, shortcuts,
action dispatch, stable contents during tracking, deferred refresh and teardown
without opening a menu or sending input to other applications. Launcher tests
cover workspace commands, paused actions and stale identities; state tests cover
saved assignments, regex escaping, invalid-file protection and symlinked configs.

The workspace summaries, active-workspace indicator, pause, configuration and
reload controls take inspiration from
[AeroSpace's native menu](https://github.com/nikitabobko/AeroSpace/blob/main/Sources/AppBundle/ui/MenuBar.swift).
