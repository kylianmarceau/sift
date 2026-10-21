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

Next (small milestones, in order):

1. **Real-data check** (needs your go-ahead): index a real folder such as
   `~/Documents` or a code directory and compare ranking quality and timing
   with the synthetic results.
2. **1M on-disk indexing benchmark** (needs your go-ahead): it creates a
   million empty files under `$TMPDIR`. Also try a cold-cache run.
3. **`getattrlistbulk` scanner**: fetch names and attributes per directory
   in bulk instead of one `lstat` per entry. Measure against the current
   scanner.
4. **Query syntax**: `f:` / `d:` prefixes and `ext:` filters in the query
   text itself (shared later by the launcher), plus path-fragment filters
   (`src/ main`).
5. **Ranking polish**: CamelCase word boundaries, a light bonus for folders
   and apps, and a configurable deterministic tie-break.
6. CI: `fmt` / `clippy` / `test` on a macOS runner.

## Phase 1: minimal launcher (GPUI)

- Verify GPUI's current release and how it is distributed (crates.io or
  git), then build a floating panel window with a text field and result
  list, keyboard navigation, and fast show/hide.
- Discover applications (`/Applications`, `/System/Applications`,
  `~/Applications`) and launch through `NSWorkspace` (objc2).
- Add a global hotkey and measure hotkey-to-visible time.
- Introduce a `core` crate for provider and result types.

## Phase 2: unified file search

- File provider backed by `sift-search`, with stale-result suppression.
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
