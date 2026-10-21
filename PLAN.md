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

## Phase 2: unified file search

- File provider backed by `sift-search`, plugged into the existing
  `Provider` trait and query clock.
- FSEvents incremental updates: event-id persistence, a delta overlay on
  the snapshot, and subtree rescan on dropped history.
- Settings UI for roots and exclusions, and security-scoped bookmarks.

## Phase 3: productivity features

Calculator, clipboard history, snippets, favorites, recent items, custom
commands, Quick Look, and more filters (size, date, path).

## Phase 4: optimization and polish

Profiling (Instruments), memory, battery, accessibility, crash recovery,
and index-correctness fuzzing.

## Phase 5: packaging and release

`.app` bundle, signing, notarization, updates, documentation, Apple
Silicon builds (Intel if justified).

## Open questions for you

- License for the open-source release (Rust projects usually use
  MIT OR Apache-2.0).
- Whether the GitHub repository should become public now or at a later
  milestone. It is currently private.
