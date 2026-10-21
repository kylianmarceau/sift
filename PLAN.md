# Plan

Roadmap and milestone status. Architecture decisions live in
[ARCHITECTURE.md](ARCHITECTURE.md); measurements live in
[BENCHMARKS.md](BENCHMARKS.md).

## Phase 0: file search engine (CLI) ✅ first milestone done

Done:

- [x] Cargo workspace: `sift-store`, `sift-indexer`, `sift-search`, `sift-cli`
- [x] SQLite store: parent-linked entries, raw-byte names, atomic per-root
      replacement, generation counter, corruption detection
- [x] Parallel scanner with bounded memory: no symlink following, no mount
      point crossing, opaque packages, permission errors sampled not fatal,
      sensitive-location exclusions
- [x] Memory-mapped name snapshot with automatic rebuild when stale or corrupt
- [x] Case- and accent-insensitive matching: exact / stem / prefix / word /
      substring / fuzzy tiers, kind and extension filters, bounded top-k
- [x] `sift index | search | status | remove | bench`
- [x] Tests: Unicode, unusual characters, duplicates, symlink cycles,
      permission-denied folders, corrupt database and snapshot, persistence
      across restarts, re-index after rename/delete, 60k-entry brute-force
      cross-check
- [x] Benchmarks at 10k / 100k (on disk) and 10k / 100k / 1M (store-only)
- [x] Real-data check on `~/Documents`, which led to:
  - [x] dependency and cache folders skipped by default
        (`--include-dependencies` to keep them)
  - [x] index options saved per root; `sift index` with no paths
        re-indexes every root
  - [x] database compaction after large deletes
  - [x] ranking: whole-word matches and the user's own files rank above
        partial matches and files inside dependency folders

Next (small milestones, in order):

1. **1M on-disk indexing benchmark** (needs your go-ahead): it creates a
   million empty files under `$TMPDIR`. Also try a cold-cache run.
2. **`getattrlistbulk` scanner**: fetch names and attributes per directory
   in bulk instead of one `lstat` per entry. Measure against the current
   scanner.
3. **Query syntax**: `f:` / `d:` prefixes and `ext:` filters in the query
   text itself (shared later by the launcher), plus path-fragment filters
   (`src/ main`).
4. **Ranking polish**: CamelCase word boundaries, a light bonus for folders
   and apps, and a configurable deterministic tie-break.
5. CI: `fmt` / `clippy` / `test` on a macOS runner.

## Phase 1: minimal launcher (GPUI) ✅ built, manual check pending

Done:

- [x] `sift-core`: `Provider` trait, `ResultItem`/`Action`, multi-provider
      `Search`, `QueryClock` (stale-result protection), `Selection`
- [x] `sift-apps`: app discovery (system, user and CoreServices folders,
      symlinked apps, duplicate bundle ids) and the app provider, using the
      same matcher as file search
- [x] `sift-macos`: app icons as PNG, `NSWorkspace` launching with error
      reporting, panel placement and sizing, accessory activation policy
- [x] `sift-launcher`: GPUI 0.2.2 floating panel (720 pt, dynamic height,
      rounded corners, light and dark mode, footer hints), IME-capable
      search field, arrow/Ctrl-N/Ctrl-P navigation, Enter opens,
      Esc / click-away / Option+Space hides
- [x] Global hotkey Option+Space without Accessibility permission (Carbon)
- [x] `scripts/bundle-app.sh` → `target/release/Sift.app` (no Dock icon,
      ad-hoc signed)
- [x] Measured: 26 MB idle footprint, 0 s CPU in 60 s idle, about 100 ms
      warm startup, app search ≤ 11 µs (BENCHMARKS.md)

Still open in Phase 1:

1. **Manual GUI check** (needs you, see README "Trying the launcher"):
   look, typing, arrows, Enter, Esc, Option+Space, dark/light switch,
   multiple displays, full-screen apps.
2. ~~Hotkey-to-visible latency~~: done. Median 4.87 ms with the real
   hotkey, after fixing a 45 ms wait for GPUI's display link.
3. Fix whatever the manual check finds.

Fixed after the first manual try: a black area under the results (GPUI
missed the window's resize events; see ARCHITECTURE.md, "Launcher").

Before Phase 2 (small, optional):

- Lazy icon loading (icons for visible rows only) to shorten the first
  catalog load.
- A configurable hotkey and launch at login (`SMAppService`).
- Recency boost for recently launched apps.

## Phase 2: unified file search ✅ built, manual check pending

Done:

- [x] Store schema v3: parent index, saved FSEvents position per root,
      transactional incremental `Updater`
- [x] Incremental updates (`apply_changes`) checked against fresh full
      scans: renames, deletions, moved-in folders, kind changes,
      permission errors, missing roots, 60 random edits
- [x] `sift-watch`: FSEvents with replay from a saved id, root-changed,
      dropped and wrapped flags, event-store UUID check
- [x] `sift-files`: shared `FileIndex`, `FileProvider`, background
      `IndexService` (startup recovery, batching, snapshot swap, settings
      commands, unavailable roots)
- [x] Launcher: apps, commands and files in one search, ranked by match
      tier then provider. Enter opens, ⌘Enter shows in Finder. File icons
      are cached per type
- [x] Commands: *Sift Settings*, *Quit Sift*
- [x] Settings window: folders with state and counts, Add Folder (macOS
      picker), Re-index, Remove, Exclude Folder, dependency toggle. Home
      folder and startup disk refused
- [x] Measured: change to searchable ≈ 180 ms (10k) / 205 ms (100k)
      median; hidden idle 0.00 s CPU per minute, 25 MB (BENCHMARKS.md)

Still open in Phase 2:

1. **Manual GUI check** (needs you): add a folder in Settings, search
   files, Enter / ⌘Enter, exclude a folder, remove a folder, and accept
   the macOS permission prompt for Documents.
2. Your existing `~/Documents` index predates event positions, so the
   launcher rescans it once on first start (about 0.2 s).

Next small milestone: a snapshot delta overlay for indexes near a
million entries (moved to Phase 4).

## Phase 3: productivity features ✅ built, manual check pending

Each feature does no background work unless it is used, and its cost is
measured in BENCHMARKS.md.

Done:

- [x] `sift-state`: `config.toml` (snippets, custom commands, clipboard
      options; problems reported, never fatal) and `state.db` (recents,
      favorites, clipboard history)
- [x] Recents and favorites: the empty query lists favorites, then recent
      items. ⌘D toggles a favorite (★), ⌃X removes an item from the
      history. Use counts and favorites order results within a match tier
- [x] Calculator (`sift-calc`): arithmetic, functions and constants; the
      value is the top result and Enter copies it. Ordinary searches
      never show it
- [x] Snippets (Enter copies, `{date}` / `{time}` / `{datetime}`) and
      custom commands (URL, file or app, shell command) from the config,
      reloaded when the file changes. *Edit Sift Configuration* command
- [x] File filters: `ext:`, `type:`, `in:`, `size:`, `modified:`; scan-side
      where possible, explained when not understood
- [x] Clipboard history, opt-in: plain text only, private items skipped,
      searched with `clip …`, *Clipboard History* and *Clear Clipboard
      History* commands. Enter copies; nothing is pasted automatically
- [x] Quick Look: ⌘Y previews the selected file, follows the selection,
      Esc closes it first
- [x] Measured: calculator 0.2 µs per query; filters 0.5–35 ms; state
      database 12–273 µs per operation; clipboard polling ≤ 0.01 s CPU per
      minute (BENCHMARKS.md)

Still open in Phase 3:

1. **Manual GUI check** (needs you), see README "Trying the launcher":
   favorites and recents, calculator, a snippet and a custom command from
   the config, filters, clipboard history after enabling it, and Quick
   Look (⌘Y, arrows while previewing, Esc).
2. Footer hints were sized by estimate, not on screen; with a file
   selected the footer shows five hints.

Deliberately left out:

- Auto-paste of snippets and clipboard entries: it would need the
  Accessibility permission. Enter copies instead.
- Images and files in clipboard history (text only for now).
- Filters with no name and no extension (`in:projects` alone): the index
  scan needs a name to match, so these ask for one.

## Phase 4: optimization and polish ✅ built, manual check pending

Instruments needs Xcode, which is not installed. Profiling used `sample`,
`footprint`, `vmmap`, `top` and `/usr/bin/time`, none of which need
`sudo`. All figures are in BENCHMARKS.md.

Done:

- [x] UI benchmark (`SIFT_UI_BENCH`, `scripts/ui-bench.sh`) that types
      through the real launcher
- [x] Keystroke to frame: median 15.4 ms → 2.6 ms (p95 5.7 ms) at 100k
      files, by drawing as soon as results arrive
- [x] Memory: about 25–34 MB without a file index, about 47–55 MB with
      100k files, and no growth with use. The icon cache is now bounded
      and frees GPU textures
- [x] Idle: 0 s CPU over 30 s hidden. Clipboard polling moved to a
      background-QoS thread (6.3 → 2.3 wake-ups a second while on)
- [x] Indexing throughput re-measured against Phase 0, side by side (6%
      slower at 100k, from new per-folder checks) and profiled
- [x] Crash recovery:
      - a scan is one transaction, tested with kills at ten points;
      - damaged `index.db` and `state.db` are set aside and rebuilt, the
        folder list from `roots.bin`;
      - index-thread panics are contained;
      - an unclean exit is detected, and the index is checked at the
        next start;
      - a log file with crash backtraces
- [x] Correctness:
      - `sift verify` (integrity, nine invariants, comparison with disk);
      - live-update fuzzing through FSEvents (18,000 operations over
        8 seeds matched)
- [x] Accessibility:
      - VoiceOver announcements (selection, result count, messages);
      - a named window;
      - Increase Contrast and Differentiate Without Color

Still open in Phase 4:

1. **Manual checks** (need you):
   - **VoiceOver:** turn it on (⌘F5), open Sift, type, and use the
     arrows. Does it speak the highlighted result and the result count?
     Is it too talkative?
   - **Increase Contrast:** turn it on and open Sift.
   - The earlier Phase 3 checks (clipboard capture, Quick Look, the
     footer).
2. **Not measured:**
   - battery use over hours (`powermetrics` needs `sudo`);
   - cold-cache indexing (`sudo purge`);
   - a real crash notice in the GUI (the recovery paths are covered by
     tests, the notice text was not seen on screen).

Moved to later:

- Snapshot delta overlay for indexes of millions of entries. Search at 1M
  synthetic entries takes a median 0.5–3.5 ms, and an update rebuilds the
  snapshot in about 0.2 s, off the main thread.
- Faster scanning with `getattrlistbulk` and deferred index creation.
  The profile shows where the time goes, but the current speed (about
  550k entries a second, warm) meets the targets.
- Configurable hotkey and a settings page for `config.toml` options.
  (Launch at login landed in Phase 5, opt-in.)

## UI redesign ✅ built, visual check pending

- [x] Launcher: an aligned icon and text grid, a section label with the
      position, single-line rows (title, folder and kind), coloured tiles
      for commands, calculator, snippets and clipboard, and a footer
      action bar with key caps and a status dot
- [x] Settings: folder cards, status dots, a switch for dependency
      folders, and primary, secondary and destructive buttons
- [x] Shared colour tokens for light and dark at normal and increased
      contrast, with the system accent colour and WCAG contrast tested
- [x] Measured against the previous UI in alternation: no latency, idle
      or memory regression (BENCHMARKS.md). That check caught and fixed a
      per-frame index-lock wait in the new footer

Still open: **a visual check by you.** GPUI cannot capture its own frames,
and screen capture needs a permission Sift does not ask for. The design
was checked with a browser mockup using the same sizes and colours, but
not on screen. Please look at both themes, Increase Contrast, a long
file name, an error message, and the settings window.

## Phase 5: packaging and release ✅ built, release not yet published

Decisions: MIT licence; updates are checked and reported, never
installed; Apple Silicon only; Open at Login off by default.

- [x] MIT licence, package metadata, and a generated list of third-party
      licences shipped in the bundle
- [x] App icon (`packaging/icon.svg` → `Sift.icns`)
- [x] A proper bundle: Info.plist template with version and build
      number, bundle id `io.github.kylianmarceau.sift`, macOS 13+,
      hardened runtime
- [x] `scripts/release.sh`: checks, Developer ID signing, notarization
      and stapling when credentials are given, `.zip` and `.dmg`,
      checksums. Tested ad-hoc: both packages extract to a bundle with a
      valid signature, checksums match
- [x] Open at Login through `SMAppService`, a switch in Settings
- [x] Daily update check against GitHub Releases, reported in the footer
      and Settings, with `[updates] check = false` to turn it off
- [x] Install, privacy, uninstall, contributing, security, changelog and
      release documentation

Not yet done, and needing you:

- **Open at Login on your Mac.** The switch's status reading is tested;
  registering was not, because it changes your login items. Please turn
  it on in Settings, check System Settings › General › Login Items, log
  out and in, then turn it off again.
- **Folder access again.** The new bundle id makes macOS see Sift as a
  new app, so it asks again for Documents, Desktop or Downloads.
- **Signing and notarization** need your Apple Developer ID and a
  notarytool keychain profile (RELEASING.md). Without them, releases are
  ad-hoc signed and users click Open Anyway once.
- **Publishing.** Tagging v0.1.0, creating the GitHub release, and making
  the repository public (the update check cannot see releases of a
  private repository).
- **CI** (GitHub Actions on macOS) is not set up. On a private
  repository it uses paid runner minutes.

## Open questions for you

- When to make the repository public and publish 0.1.0.
- Whether to set up CI once the repository is public (free there).
