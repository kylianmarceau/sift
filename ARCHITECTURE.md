# Architecture

This document records the technical decisions behind sift and the
reasoning for them. It covers what exists today: the indexer and search
engine (Phase 0) and the minimal launcher (Phase 1). Later phases add
sections as they land.

## Workspace

```text
sift/
├── crates/
│   ├── sift-store/    SQLite persistence: schema, root replacement, path resolution
│   ├── sift-indexer/  parallel filesystem scanner that writes into the store
│   ├── sift-search/   name folding, memory-mapped snapshot, matching and ranking
│   ├── sift-cli/      `sift` binary: index / search / status / remove / bench
│   ├── sift-core/     launcher result types, the Provider trait, query clock, selection
│   ├── sift-apps/     installed-application discovery and the app provider
│   ├── sift-macos/    AppKit glue: icons, launching apps, panel placement (objc2)
│   └── sift-launcher/ GPUI launcher app (binary `sift-launcher`, bundled as Sift.app)
├── scripts/bench.sh       reproducible file-search benchmark run
├── scripts/bundle-app.sh  builds target/release/Sift.app
├── PLAN.md            roadmap and milestone status
└── BENCHMARKS.md      measured results
```

Dependency directions:

- File search: `cli → {indexer, search} → store`. The indexer and search
  crates do not know about each other; the CLI composes them.
- Launcher: `launcher → {core, apps, macos}`, `apps → {core, search}`.
  `sift-apps` reuses only `sift-search`'s folding and name matcher, so
  apps and files rank by the same rules.

`sift-core` holds only UI-free types, so it can be unit-tested without a
window and shared by future providers. All `unsafe` AppKit code lives in
`sift-macos`; the launcher crate has one `unsafe` call (turning GPUI's
window handle into a `Panel`).

## Storage: SQLite as the source of truth

`index.db` (WAL mode) holds:

- `roots`: indexed directories with scan statistics and the index options
  they were scanned with (so `sift index` with no arguments re-indexes
  every root the same way).
- `entries`: one row per file, folder or link: `id, root_id, parent_id,
  kind, name (BLOB), size, mtime`.
- `meta`: a `generation` counter bumped on every committed change.

Decisions:

- **Parent links, not full paths.** Paths are rebuilt by walking
  `parent_id` (a rowid lookup per level, with a per-query directory cache).
  This keeps the database small and will make a directory rename a
  one-row update once incremental updates exist. Resolving a page of 20
  results costs well under a millisecond (see BENCHMARKS.md).
- **Names are raw bytes.** APFS and HFS+ require UTF-8 names, but exFAT,
  SMB and NFS volumes may not. Bytes round-trip any name exactly; folding
  for search happens elsewhere.
- **Root rows store the absolute path as their name**, so the parent walk
  terminates without a separate lookup.
- **Each root is replaced in a single transaction.** Readers keep seeing
  the old index until commit, and an interrupted scan leaves it intact.
  The WAL is checkpointed (TRUNCATE) afterwards so a large scan does not
  leave a large WAL file behind.
- **No secondary indexes.** Search never queries `entries` by name, and
  whole-root deletes are a sequential scan. An index on `parent_id` will be
  needed for incremental updates (Phase 2); it costs disk, so it is added
  when that feature needs it.
- **Roots may not nest.** Indexing a folder inside an existing root is
  rejected; indexing a parent of existing roots absorbs them.
- **Schema versions migrate forward** (`PRAGMA user_version`). Version 2
  added `roots.options`. Read-only handles on an older database still
  work, and report no saved options.
- **Compaction after large deletes.** Replacing or removing a root can
  leave most pages free. After such a change, the store runs `VACUUM` if
  at least a quarter of a database of 256+ pages is free.
- `bundled` SQLite is compiled in for a predictable version with FTS5
  available, instead of depending on the system library's version.

## Indexing: parallel scan, single writer

`sift-indexer` walks a root with a small pool of threads (CPU count, at most
8). Workers pop directories from a shared stack, list them, `lstat` each
entry, and send batches of records over a bounded channel to the calling
thread, which is the only SQLite writer.

- **Bounded memory.** At most 64 batches × 1024 records are in flight;
  names in a batch share one buffer. The pending-directory stack holds only
  paths of directories not yet read, and a stack (rather than a queue)
  keeps the traversal roughly depth-first so it stays small.
- **Ids are allocated at discovery** from an atomic counter. A directory is
  always discovered before it is read, so children get larger ids than
  parents, which the snapshot builder relies on to compute depth in one
  pass.
- **Symlinks are recorded and never followed**, which rules out cycles.
- **Mount points are not crossed** by default (device id differs from the
  root's), so a root never wanders into `/Volumes` or network shares.
- **Package bundles** (`.app`, `.framework`, `.photoslibrary`, …) are
  recorded but not descended, matching how Finder presents them. This is
  an extension list for now. The authoritative check (Launch Services'
  package flag) belongs in the future platform crate.
- **Dependency and cache folders are skipped by default.** Folders such
  as `node_modules`, `.git`, `.venv`, `site-packages`, `__pycache__`,
  `Pods`, `DerivedData` and `.build`, and any folder containing a
  `CACHEDIR.TAG` file, are recorded but not descended. On a real
  `~/Documents` this cut the index from 263k to 54k entries and the
  database from 11.5 to 2.0 MiB. `--include-dependencies` turns it off.
- **Errors are data.** Unreadable directories and entries (permission
  denied, privacy-protected locations, files vanishing mid-scan) are
  counted and sampled, never fatal.
- **Sensitive locations.** `~/Library` and `~/.Trash` are excluded when
  they fall inside a root, unless the user names them as roots or passes
  `--include-sensitive`. The CLI refuses `/` and `~` without
  `--allow-broad-root`, and always excludes the index directory itself.

Planned improvement: macOS `getattrlistbulk(2)` returns names, types,
sizes and dates for a whole directory in a few system calls instead of one
`lstat` per entry, which is how the fastest macOS file tools scan. It
fits behind the same worker interface.

## Search: a memory-mapped name snapshot

Matching does **not** query SQLite. Stepping through a million rows costs
on the order of 100 ns each in SQLite, far beyond a 20 ms budget once
substring and fuzzy matching are added (BENCHMARKS.md has a measured
comparison). Instead `sift-search` derives a **snapshot**
(`names.snap`) from the store:

```text
header   64 B   magic, version, store generation, counts
ids      u64×n  entry id per name
bags     u64×n  character bag per name
offsets  u32×n+1
meta     u16×n  kind (2 bits) | noisy (1 bit) | depth (13 bits)
names    bytes  folded names, NUL-separated, contiguous
```

- **Memory-mapped, not loaded.** Pages are clean and file-backed: they
  count toward resident size while in use, but not toward the process's
  memory *footprint*, and macOS can evict them under pressure and fault
  them back in later. Heap memory holding the same data would be neither.
  The store stays the source of truth.
- **Self-validating and disposable.** The header records the store
  generation it was built from. A missing, stale, truncated or corrupt
  snapshot is rebuilt from the store on open. Snapshots are written to a
  temporary file and renamed into place, so an open mapping is never
  modified underneath a reader (the one `unsafe` block, `Mmap::map`,
  relies on this).
- **Size.** About 23 bytes plus the folded name length per entry.
- **Noisy entries.** The builder marks everything under folders that
  usually hold generated or third-party files (`node_modules`, `vendor`,
  `dist`, `build`, `target`, `cache`, names ending in `-deps` or `_cache`,
  …) as *noisy*. Noisy entries are still found, but rank below the user's
  own files in the same tier. This complements the indexer's skip list,
  which can't cover every naming convention.

### Folding

Names and queries are folded identically: lowercase → NFD → drop Latin
combining marks (U+0300–U+036F) → NFC. Search is therefore case- and
accent-insensitive (`cafe` finds `Café.pdf`), and decomposed names from
older filesystems match composed queries. Marks from other scripts (e.g.
Japanese dakuten) are kept because they change meaning. Invalid UTF-8 is
folded lossily for matching only; display and opening use the raw bytes.

### Matching and ranking

A query is answered in up to two parallel passes over 16k-name chunks
(rayon):

1. **Substring pass.** A single SIMD `memchr::memmem` search runs over each
   chunk's contiguous name buffer, which is much faster than a search per
   name. NUL separators make a cross-name match impossible. Each hit is
   mapped to its name by advancing a cursor through `offsets`, filtered
   (kind and extension), and graded.
2. **Fuzzy pass.** This runs only if the substring pass found fewer matches
   than the result limit. Fuzzy matches always rank below substring
   matches, so otherwise none could appear. Each name is rejected first by
   its 64-bit character bag (one AND), then by an in-order byte check, and
   only the survivors are scored by `nucleo-matcher`. Multi-word queries
   match each word independently.

Each chunk keeps a bounded top-k heap, so ranking cost and memory scale
with the result limit, not with the number of matches. Results are packed
into a single `u64` sort key:

| bits  | meaning |
|-------|---------|
| 48–55 | tier: exact (6) > exact stem (5, `report` = `report.pdf`) > prefix (4) > word prefix (3, `q3-report`) > substring (2) > fuzzy (1) |
| 47    | clean: not inside a noisy folder |
| 46    | whole word: the match ends at a word boundary (`project-story` beats `projection` for `project`) |
| 32–45 | earlier match position (substring tiers) or nucleo score (fuzzy), saturated to 14 bits |
| 16–31 | shorter name |
| 0–15  | shallower path |

The tier always wins. Within a tier, the user's own files come before
dependency files, and whole-word matches before partial ones. This
ranking cost about 5–15% in search time at 1M entries (BENCHMARKS.md).

Ties go to the lower snapshot position. Paths and metadata are resolved
from SQLite only for the final page of results.

**Why not FTS5?** FTS5's trigram tokenizer handles substring queries of 3+
characters, but not 1–2 character queries or subsequence-style fuzzy
matching. It would also add an index several times the size of the name
data. A contiguous name scan with SIMD covers every query shape with one
code path.

**Why not keep names on the heap?** This was considered and rejected:
same scan speed, but the data would count fully against footprint and
could not be reclaimed under pressure.

**Scaling limit.** The scan is O(n) per keystroke. Measured times stay
well inside the 20 ms target at 1M entries (BENCHMARKS.md). If much
larger indexes need it, a trigram posting list can be added as a
candidate generator in front of the same ranking code.

## Launcher (Phase 1)

`sift-launcher` is a GPUI application that runs in the background with no
Dock icon (`LSUIElement` in the bundle, plus the Accessory activation
policy at runtime, because GPUI switches to Regular while launching).

**Window.** One window is created at startup and reused. GPUI's
`WindowKind::PopUp` gives a non-activating `NSPanel` at pop-up level that
can still become key, joins every Space and appears over full-screen apps.
`sift-macos::Panel` then:

- hides the title, title bar and traffic-light buttons, keeping the titled
  style for macOS's native rounded corners and shadow;
- centres the panel on the screen under the mouse, with its top edge
  about 28% down;
- resizes it to fit the results, keeping the top edge fixed.

The panel is 720 pt wide. Its height is search field + up to 8 rows +
footer, so it grows and shrinks as you type.

Resizes are never made from inside a GPUI update. AppKit reports a
resize back to GPUI synchronously, and GPUI drops that report while its
app state is borrowed. The window would then grow while GPUI kept laying
out at the old size, leaving the new area black (a bug in the first
build). The launcher therefore resizes from a separate foreground task,
as GPUI's own `Window::resize` does. With `SIFT_LOG=debug` every
viewport change is logged, so a mismatch is easy to spot.

**Show and hide.** Showing is `makeKeyAndOrderFront` on the existing
window, and hiding is `orderOut`. GPUI pauses its display link while the
window is hidden and restarts it only when AppKit later reports the window
visible (about 45 ms after showing). So after ordering the panel in, the
launcher asks the view's layer to display, which makes GPUI draw the
first frame immediately (`Panel::draw_now`). Nothing is rebuilt, so the process,
GPUI's text and image caches and the app catalog stay warm. The panel
hides on Escape, on Cmd-W, when it loses key status (a click elsewhere),
and when Option+Space is pressed while it is visible. Hiding clears the
query. Cmd-Q quits.

**Hotkey.** Option+Space is registered with the `global-hotkey` crate,
which uses Carbon's `RegisterEventHotKey`. That API needs no
Accessibility or Input Monitoring permission. The press is time-stamped
in the event handler and sent through a channel to a GPUI task.
`on_next_frame` then logs the time from key press to the first painted
frame.

**Text input.** `SearchInput` implements GPUI's `EntityInputHandler`, so
the panel goes through the macOS text system. Dead keys and input methods
(marked text) work, and composition doesn't trigger a search until it is
committed.

**Providers.** `sift-core` defines:

```rust
trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem>;
}
```

`Search` fans a query out to every provider and merges the results by
score. `ResultItem` carries a stable id, a kind, a title and subtitle,
the score and an `Action` (`OpenApplication`, `OpenPath`). Phase 1 has
one provider (`sift-apps`). Phase 2 adds files behind the same trait.

**Stale-result protection.** Every keystroke takes a ticket from a
`QueryClock` (an atomic counter) and runs the search on GPUI's background
executor. When results come back, they are applied only if the ticket is
still the latest. A slow provider can therefore never overwrite newer
results. `Selection` keeps the highlighted row on the same item (by id)
when results are replaced, or resets it to the top for a new query.

**Applications.** `sift-apps` lists `.app` bundles in:

- `/Applications` and `~/Applications` (plus one level of subfolders);
- `/System/Applications`;
- `/System/Library/CoreServices/Applications`;
- Finder.

It follows top-level symlinks (Safari is a symlink into a cryptex), and
deduplicates by canonical path and by `CFBundleIdentifier`. The catalog
and 64 px icons (rendered by `NSWorkspace iconForFile` into PNGs, off the
main thread) load in the background at startup. They reload when the
panel is shown, if the catalog is more than a minute old.

**Launching.** `NSWorkspace openApplicationAtURL:configuration:
completionHandler:` launches the app or brings it forward. The panel hides
immediately. Failures arrive on a background queue, travel back over a
channel, and are shown in the panel.

**Packaging.** `scripts/bundle-app.sh` builds `target/release/Sift.app`:

- `Info.plist` with `LSUIElement`, `NSHighResolutionCapable` and
  bundle id `dev.sift.Sift`;
- ad-hoc signed, not notarized.

The executable is built as `sift-launcher` and renamed to `Sift` inside
the bundle. A binary named `Sift` would overwrite the `sift` CLI, because
APFS file names are case-insensitive.

**GPUI build.** GPUI 0.2.2 from crates.io, with default features off plus
`font-kit` and `runtime_shaders`. Runtime shaders mean building doesn't
need full Xcode and its Metal compiler; the shaders compile once on first
launch, which is why the first start after a build is slower (about
0.8 s against about 0.1 s).

## What is deliberately deferred

- Incremental updates via FSEvents (Phase 2). The design for it:
  - Apply events to the store, then update the snapshot through a small
    in-memory delta (added and removed ids) that is merged into a fresh
    snapshot in the background. Fall back to a rescan of the affected
    subtree when FSEvents reports dropped or coalesced history
    (`MustScanSubDirs`, `UserDropped`/`KernelDropped`), or when the stored
    event id is no longer valid.
- Path filters, size and date filters, and usage-based ranking (recency,
  favorites).
- `getattrlistbulk` scanning, and Launch Services package detection.
- Launcher: a configurable hotkey, launch at login, usage-based ranking,
  lazy icon loading, and app-folder watching (the catalog currently
  refreshes by age).
- Security-scoped bookmarks for sandboxed folder access (Phase 2/5).
- Full-text content indexing (out of scope for the MVP).
