# Native window management

Sift includes a Rust window-management engine inspired by AeroSpace. It runs
inside Sift: no AeroSpace installation, subprocess calls for tiling, or separate
application is required. Available in Sift 0.2.2 on the
`native-window-manager` branch, it is experimental and off by default. Version
0.2.1 separates manual shortcuts from optional automatic tiling. Native smoke
checks pass; broader application/multi-monitor acceptance remains open.

Reference: [AeroSpace](https://github.com/nikitabobko/AeroSpace) at
`74a1bf17e82d70e0a21945ba04bb4f590bf19f83`. Its MIT notice is retained in
`packaging/AeroSpace-LICENSE.txt` and shipped in the app bundle. Sift uses its own
configuration section and command protocol; existing AeroSpace configurations
and third-party clients are not drop-in compatible.

## Enable

Upgrade and restart Sift first:

```sh
brew update
brew upgrade --cask kylianmarceau/sift/sift
open -a Sift
```

Grant permission to **Sift** itself. A permission granted to the separate
**Sift WM Check** development harness does not grant it to the installed app.

Open Sift Settings → Window Management. Grant window-control permission to Sift,
return to Settings, and enable **Window Shortcuts and Controls**. Leave
**Automatic Tiling off** for manual control: windows stay where you put them,
and only an explicit shortcut or CLI command moves/resizes/focuses a window.
Discovery, app launches, focus changes and window closure never arrange other
windows in manual mode. Manual changes stay in place when controls stop or Sift
quits. Other launcher features do not need window-control permission.

Automatic Tiling is a separate opt-in for AeroSpace-style arrangement and Sift
workspaces. Turning that option off restores the pre-tiling positions and leaves
manual controls running. Upgrading from 0.2.0 also defaults to manual control
unless `automatic_tiling = true` is explicitly configured.

On macOS 27 this permission is named **Device Control and Data Access** under
Privacy & Security. Older macOS versions call it **Accessibility**.

The same option can be set in `config.toml`:

```toml
[window_manager]
enabled = true
automatic_tiling = false          # manual controls; true enables tiling/workspaces
default_layout = "tiles"          # tiles or accordion
default_orientation = "horizontal" # horizontal or vertical
accordion_padding = 30
workspaces = ["1", "2", "3", "code", "web"]

[window_manager.gaps]
inner = 8
left = 8
right = 8
top = 8
bottom = 8

# Partial overrides inherit the global values. Main/secondary selectors apply
# first, then display-name matches, then exact names, then native display IDs.
[window_manager.monitor_gaps.secondary]
inner = 12
left = 16

[window_manager.monitor_gaps."DELL"]
top = 20
```

Sift reloads hand edits when opened. `sift wm reload-config` explicitly reloads
window-management rules and bindings; an invalid reload keeps the working
configuration. Turning controls off in Settings removes their observers and
shortcuts. `sift wm enable off` pauses controls but keeps the control service alive
so `enable on` can resume it. In manual mode pause/stop never restore or reposition
windows. Only automatic tiling owns recovery positions.

## Persistent window shortcuts (manual or automatic mode)

Use **Edit Rules and Shortcuts…** to open the actual configuration. Merge these
sections into it; do not duplicate an existing table. A configured `main` table
replaces its default bindings, so include every shortcut you want to keep.

```toml
[window_manager]
enabled = true
automatic_tiling = false

[window_manager.targets.terminal]
app_id = "com.apple.Terminal"
# Optional regex to select one document/window rather than any Terminal window:
# title = '(?i)sift|project'
launch = true

[window_manager.targets.browser]
app_id = "com.apple.Safari"

[window_manager.bindings.main]
ctrl-alt-t = ["focus --target terminal"]
ctrl-alt-b = ["focus --target browser"]
ctrl-alt-left = ["snap left"]
ctrl-alt-right = ["snap right"]
ctrl-alt-up = ["snap maximize"]
ctrl-alt-down = ["snap restore"]
```

Save, then run `sift wm reload-config`. `sift wm list-apps` shows app names and
bundle IDs; `sift wm list-windows` shows current titles. Targets use an **exact
bundle ID** and optional `app_name`/`title` **regular expressions**. All supplied
conditions must match. A title-only target is also permitted, but cannot launch
an app without an `app_id`. Target matching is independent of startup rules.

The same saved target resolves against current window metadata on each press.
It survives window closure/reopening, app restart and Sift restart; it never
saves a native window ID. If several windows match, Sift prefers the currently
focused match, then the most recently focused match, then the lowest native ID.
Use a title expression to distinguish windows in the same application. Browser
tabs are not independent native windows, and titles must continue matching.

With `launch = true` (the default), a missing target launches/reopens its app
through Launch Services (preferring its previously observed bundle path, with
bundle-ID lookup for first launches or moved apps) and waits up to eight seconds for a matching window.
Existing matches focus directly without launching a process. A failed launch or
missing matching title is reported in window-control status. The CLI acknowledges
a queued launch immediately; the service completes the pending action asynchronously.
Sift cannot recreate
a particular document/tab or force an app to open a window. Repeated presses
coalesce while launching. Reload, pause and shutdown cancel pending actions.
Closing or minimizing a missing target never launches its app.

```sh
sift wm focus --target terminal
sift wm snap left --target terminal
sift wm resize width +50 --target terminal
```

For an existing window, `--target` positions/resizes it without changing focus.
Launching a missing application can activate it through macOS.
To focus and position it, bind `["focus --target terminal", "snap left"]`.
If launch is needed, the remaining actions wait for the matching window instead
of running on the previously focused window. Manual snap positions are `left`,
`right`, `top`, `bottom`, `top-left`, `top-right`, `bottom-left`, `bottom-right`,
`center`, `maximize`, and `restore`. They use the monitor's usable screen area.
Restore remembers the frame before the first snap for that window during this
Sift session; these temporary restore frames are not persistent bindings.

## Search and shortcuts

Open windows and workspaces appear in ordinary search while the manager runs.
Type `win` or `win project` to search windows only, and `ws` or `ws code` for
workspaces (only with Automatic Tiling enabled). Enter focuses the window or switches workspace. Window identities
are temporary and are not added to recents or favorites.

Most default bindings use **Control–Option**, leaving Option–Space for Sift.
Numbered workspace bindings use **Control**:

| Keys | Action |
| --- | --- |
| Control–Option–H/J/K/L | Focus left/down/up/right |
| Control–Option–Shift–H/J/K/L | Move left/down/up/right |
| Control–1…9 | Switch workspace |
| Control–Shift–1…9 | Send window to workspace |
| Control–Option–Tab | Previous workspace |
| Control–Option–F | Toggle Sift fullscreen |
| Control–Option–Minus/Equal | Shrink/grow selected tile |
| Control–Option–Left/Right | Snap current window to left/right half (manual) |
| Control–Option–Up/Down | Maximize/restore current window (manual) |
| Control–Option–Slash | Cycle tiles/horizontal/vertical |
| Control–Option–Comma | Cycle accordion/horizontal/vertical |

All bindings are registered by physical key. A configured mode replaces the
default binding table for that mode. No hotkeys are registered when the feature
is off. Shortcut recording temporarily suspends both launcher and window
shortcuts. A window shortcut conflicting with Sift's launcher shortcut is
reported; macOS does not reliably report conflicts with other applications.
Workspace/tree shortcuts are inactive while Automatic Tiling is off. Directional
focus, move (50-point nudges), resize and fullscreen also work in manual mode.
Manual fullscreen fills the usable screen and restores the previous frame when
toggled again. Workspace routing rules, automatic mouse behavior, and automatic
frame writes are inactive in manual mode.

```toml
[window_manager.bindings.main]
alt-h = ["focus left"]
alt-j = ["focus down"]
alt-k = ["focus up"]
alt-l = ["focus right"]
alt-r = ["mode resize"]
alt-shift-1 = ["move-node-to-workspace 1", "workspace 1"]

[window_manager.bindings.resize]
minus = ["resize smart -50"]
equal = ["resize smart +50"]
enter = ["mode main"]
```

Main-mode bindings require Command, Control, or Option, or a function key.
Explicit secondary modes can use bare keys. Escape returns to main mode unless
you explicitly bind it to something else.

## Application rules and displays

Application conditions are regular expressions, compiled once. All supplied
conditions must match. Rules run when a new window is discovered, in order;
`check_further = true` continues to the next matching rule. Moving a window by
hand or command does not reapply its startup rule.

```toml
[[window_manager.rules]]
app_id = '^com\.apple\.Terminal$'
workspace = "code"

[[window_manager.rules]]
app_id = '^com\.apple\.Safari$'
workspace = "web"

[[window_manager.rules]]
title = '(?i)preferences|settings'
floating = true

[window_manager.workspace_monitors]
code = "main"
web = "DELL" # display name substring; native display IDs also work
```

Optional `include_apps` and `exclude_apps` arrays under `[window_manager]` filter
bundle IDs with regular expressions. An empty include list permits all regular
applications. Sift's own windows and accessory/background applications are never
managed. Removing an application from management restores its windows.

Each monitor displays one workspace; hidden workspaces keep their windows near
a bottom screen corner, as AeroSpace does. Sift chooses the corner with the
least overlap with other monitors. A one-pixel edge may remain visible because
macOS prevents completely off-screen placement. These are Sift workspaces, not
native Mission Control Spaces. Monitor disconnection rehomes its workspaces and
constrains floating windows to a remaining display.

Dialogs and utility windows float by default. Explicit application rules can
override that choice. Native minimized/fullscreen windows are kept out of the
tiling tree until they return.

## Callback hooks

Hooks are optional shell scripts configured by you. They run after startup,
when a new managed window appears, or when managed focus, a visible workspace,
or the focused monitor changes. Geometry/title updates do not repeat a
window-detected callback. Empty hooks create no command-worker threads.

```toml
[window_manager.hooks]
after_startup = []
on_window_detected = ['logger -t sift "window $SIFT_WINDOW_ID from $SIFT_APP_ID"']
on_focus_changed = []
on_workspace_changed = ['logger -t sift "workspace $SIFT_PREV_WORKSPACE -> $SIFT_WORKSPACE"']
on_focused_monitor_changed = []
```

Each invocation receives `SIFT_EVENT`, `SIFT_WORKSPACE`,
`SIFT_PREV_WORKSPACE`, `SIFT_MONITOR_ID` and `SIFT_PREV_MONITOR_ID`. When a target
window exists, `SIFT_WINDOW_ID`, `SIFT_APP_ID` and `SIFT_WINDOW_TITLE` identify it;
those variables are absent for an empty workspace. Values are passed as
environment data. Quote variables in your shell commands. `SIFT_CONFIG_DIR`
points at this running manager's support directory so a child `sift wm` client
can reach its socket. To act on the originating window after focus has changed,
pass `--window-id "$SIFT_WINDOW_ID"` explicitly.

Commands within one event execute in order. Two workers allow independent
events to progress concurrently; at most 32 event batches are queued. Overflow
is reported in manager status. Reloading hooks or stopping the manager cancels
queued work; an already-running user script can finish. Callback scripts run
outside the model/search threads. The control socket cannot submit shell hooks.

## Mouse behavior

Mouse options default to off. No global mouse listener or pointer-hit-test
worker is installed until focus-follow-mouse or drag rearrangement is enabled.

```toml
[window_manager.mouse]
focus_follows_mouse = false
drag_to_rearrange = false
# Optional: window-center, window-lazy-center, monitor-center,
# or monitor-lazy-center. Omit to leave the pointer where it is.
# follows_focus = "monitor-lazy-center"
```

Focus-follow-mouse uses native hit testing so overlapping/floating windows are
resolved by macOS. Hit tests run off the main thread, coalesce movement, and
are limited to 60 per second while moving; no timer runs when idle. Only managed
windows can receive focus. Mouse observation uses AppKit's global mouse-event
monitor; no keyboard-event monitor or additional Input Monitoring permission
is installed.

With drag rearrangement enabled, move a native window by its title bar and
drop near another tile's edge to insert it there. Dropping on another monitor
uses that monitor's visible workspace. A native border resize updates tile
weights when released. Plain clicks and content selections leave the tree
alone because their window frame has not moved. Tiling frame writes are held
during a left-button gesture and resume at release. Floating windows retain
their normal native move/resize behavior. Swap exchanges tiles with tiles or
floating frames with floating frames.

`sift wm move-mouse window-center` and `sift wm move-mouse monitor-center`
position the pointer explicitly. The `window-lazy-center` and
`monitor-lazy-center` forms leave it alone if it is already inside the target.
The `follows_focus` option applies the same action when managed focus or the
focused monitor changes.

## Commands

The existing CLI supports `sift wm COMMAND`. The app executable also accepts
`/Applications/Sift.app/Contents/MacOS/Sift --wm COMMAND`, exiting before creating
a launcher window, session marker, or hotkeys. The app must already be running
with Window Management enabled.

Homebrew installs the bundled CLI as `sift`. A manual app installation can use
`/Applications/Sift.app/Contents/MacOS/sift-cli wm COMMAND` directly.

```sh
sift wm status
sift wm list-windows
sift wm list-workspaces
sift wm list-monitors
sift wm list-apps
sift wm list-modes
sift wm focus left
sift wm focus --window-id 123
sift wm move right
sift wm swap left
sift wm split vertical
sift wm join-with right
sift wm layout h_tiles
sift wm layout accordion
sift wm layout floating tiling
sift wm layout --root vertical
sift wm resize width +50
sift wm resize width 600
sift wm balance-sizes
sift wm flatten-workspace-tree
sift wm fullscreen
sift wm macos-native-fullscreen on
sift wm macos-native-minimize
sift wm close
sift wm close-all-windows-but-current
sift wm workspace code
sift wm workspace next
sift wm workspace-back-and-forth
sift wm summon-workspace web
sift wm move-node-to-workspace web --focus-follows-window
sift wm move-node-to-workspace code --window-id 123
sift wm focus-monitor next
sift wm move-node-to-monitor main
sift wm move-workspace-to-monitor next
sift wm mode resize
sift wm enable off
sift wm enable on
sift wm reload-config
```

Output is JSON; failures exit nonzero. The socket lives in the user-owned 0700
`window-manager` directory under Sift's support directory. Messages are bounded
to 1 MiB, peers must have the same uid, and commands have response deadlines.
`exec-and-forget` is allowed in configured bindings, not over the control socket.
Binding arrays sequence commands without spawning a shell.

This is a Sift command interface, not a promise to support every AeroSpace flag,
format expression, shell operator, callback, or socket message. AeroSpace's
non-window utilities such as volume commands are outside this engine.

Full drop-in AeroSpace parity remains open: configuration import, all CLI flags
and format expressions, and third-party protocol compatibility are not provided.
The mouse gesture model has deterministic/randomized coverage; native pointer
positioning and monitor installation/removal are checked by the smoke harness.
Broader physical mouse-gesture and application compatibility still need manual
acceptance before describing Sift as a daily-driver replacement.

## Isolation, recovery and validation

Manual controls in 0.2.1 have native acceptance coverage for matching a named
target, closing/reopening its window, relaunching its application, snapping only
the named window, passive refresh, preserving chosen positions on shutdown,
and loading the saved target after controls restart. The complete workspace has
271 passing tests. [Native output](benchmarks/manual-window-controls-smoke-2026-10-25.txt)
is retained alongside the earlier automatic-mode measurements.

- `sift-wm` is independent of GPUI and AppKit. It models trees, layouts, focus,
  workspaces, monitors, rules and commands. Searches consume a cached snapshot.
- Each application has an AX worker/run loop with 50 ms messaging timeouts.
  A stalled application cannot block the model worker or search. Geometry
  requests coalesce by window ID; one slow app cannot build an unbounded queue.
- The model, IPC listener and AX workers block awaiting events. There is no
  periodic idle polling. An app worker makes at most three startup retries if
  AX initially reports no windows, then waits for notifications. Explicit
  opening refreshes potentially missed events.
- Desired frames are deduplicated and unchanged frames are skipped. Notifications
  caused by our own moves do not raise/focus windows again.
- Window presence is separate from successful metadata reads. A frame/role
  timeout for an existing native identity preserves its model and original
  recovery position; it is removed only when absent from the app's window list.
- Recovery positions are atomically written before arranging/parking windows.
  Recovery matches the process start time, PID, native window ID and bundle ID,
  so recycled process/window IDs do not restore another application's windows.
  Reload and shutdown retain positions for apps that have not responded. Quit
  waits asynchronously for restoration, with a bounded timeout; the journal
  remains available after an interrupted exit.
- Stable native window IDs currently use the dynamically resolved
  `_AXUIElementGetWindow` bridge, as used by macOS tiling managers. It is a private
  API and needs checking on supported macOS versions. No SIP changes are made.

```sh
cargo test -p sift-wm
cargo test -p sift-macos wm::
cargo run --release -p sift-wm --example layout_latency
scripts/bundle-wm-check.sh
scripts/run-wm-check.sh
```

For a separate, recognizable permission entry, run
`scripts/bundle-wm-check.sh`. Add `target/debug/Sift WM Check.app` in System
Settings → Privacy & Security → Device Control and Data Access (macOS 27),
or Accessibility (older macOS). Launch it using `scripts/run-wm-check.sh`.
Launching the executable directly from a shell can attribute the permission
check to the shell instead of the app. The bundle contains a stable launcher;
backend test helpers are rebuilt outside it so normal backend rebuilds preserve
the launcher's permission identity. `--refresh-runner` deliberately replaces
that identity and may require removing/re-adding its permission entry. Permission
for the ordinary Sift app is independent of permission for the test harness.

The native check deliberately manages only disposable fixture windows, and
refuses to run without Accessibility access. It checks discovery, actual native
geometry, focus, workspaces, minimize/restore, control responsiveness while an
app stalls, and position restoration. It does not grant its own permission or
operate on your regular application windows.

Before calling this a daily-driver replacement, complete native checks plus
manual coverage of multiple monitors, sleep/wake, full-screen apps, dialogs,
native tabs, permission revocation and rapid enable/disable. Compare the old and
new launcher with Window Management off, then measure the enabled service with
representative apps; layout arithmetic alone is not an end-to-end benchmark.

Validation on 2026-10-25: 261 workspace tests pass, workspace/all-targets Clippy
passes with warnings denied, and both the launcher bundle and isolated check
bundle build. The isolated native smoke check passes discovery, actual frames,
activation from an inactive app, focus, native cursor positioning, workspaces,
minimize/restore, stalled-app responsiveness, pause/resume, repeated native mouse
monitor installation/removal, and shutdown restoration. The
[smoke output](benchmarks/native-wm-smoke-2026-10-25.txt) is retained. Broader application and monitor compatibility is
still pending. Disabled-launcher performance measurements
and their limits are recorded in [BENCHMARKS.md](../BENCHMARKS.md).

To compare the release launcher's UI, memory and hidden idle with management
off/on against the same real file index, use the optional isolated benchmark:

```sh
SIFT_WM_LAUNCHER="$PWD/target/release/sift-launcher" \
SIFT_WM_INDEX=/path/to/test/index \
SIFT_WM_BENCH_OUTPUT=/tmp/sift-wm-enabled-performance.json \
scripts/run-wm-check.sh
```

Set `SIFT_WM_BENCH_AUTOMATIC_TILING=false` for the same comparison with manual
controls enabled instead of automatic tiling.

It alternates off/on/off/on with fresh temporary configurations, three UI
benchmark rounds and 30 seconds of hidden idle per run. Enabled runs must
actually manage exactly the three fixture windows; disabled runs must have no
manager socket. Results retain the UI summaries, physical footprint, RSS and
CPU-time samples. Original fixture positions are restored before each test
launcher is terminated. Existing user Sift processes are not stopped.
