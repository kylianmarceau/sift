# Architecture

The `native-window-manager` branch also adds native Rust window controls and a tiling engine
(`sift-wm`) and per-application macOS AX workers (`sift-macos::wm`). The service
is opt-in and separate from the search/render paths. See
[Native window management](docs/window-manager.md) for design, commands,
recovery, the AeroSpace reference revision, and acceptance status.

This document records the technical decisions behind sift and the
reasoning for them. It covers what exists today: the indexer and search
engine (Phase 0), the launcher (Phase 1), live file search in the
launcher (Phase 2), the productivity features (Phase 3), reliability and
accessibility (Phase 4), the visual design, and packaging and release
(Phase 5).

## Workspace

```text
sift/
├── crates/
│   ├── sift-store/    SQLite persistence: schema, root replacement, path resolution
│   ├── sift-indexer/  parallel filesystem scanner that writes into the store
│   ├── sift-search/   name folding, memory-mapped snapshot, matching and ranking
│   ├── sift-cli/      `sift` binary: index / search / status / remove / bench / wm
│   ├── sift-core/     launcher result types, the Provider trait, query clock, selection
│   ├── sift-wm/       UI-independent window trees, workspaces, rules, commands and IPC types
│   ├── sift-apps/     installed-application discovery and the app provider
│   ├── sift-macos/    AppKit glue and native window-management workers
│   ├── sift-watch/    FSEvents change notifications (objc2-core-services)
│   ├── sift-files/    file provider, query filters and the live index service
│   ├── sift-calc/     calculator: expression evaluator and provider
│   ├── sift-state/    config.toml and state.db (recents, favorites, clipboard)
│   └── sift-launcher/ GPUI launcher app (binary `sift-launcher`, bundled as Sift.app)
├── scripts/bench.sh       reproducible file-search benchmark run
├── scripts/bundle-app.sh  builds target/release/Sift.app
├── PLAN.md            roadmap and milestone status
└── BENCHMARKS.md      measured results
```

Dependency directions:

- File search: `cli → {indexer, search} → store`. The indexer and search
  crates do not know about each other; the CLI composes them.
- Launcher: `launcher → {core, apps, files, calc, state, macos}`,
  `apps → {core, search}`, `files → {core, indexer, search, store, watch}`,
  `calc → core`, `state → core`.
  `sift-apps` reuses only `sift-search`'s folding and name matcher, so
  apps and files rank by the same rules.
- Window management: `macos → wm`, with `launcher → {wm, macos}` for cached
  window search and controls; `state → wm` for configuration and `cli → {wm,
  state}` for its command client. `wm` has no GPUI/AppKit dependency. Opening
  the panel queues metadata refresh through the model service; it does not
  enumerate applications or wait on an AX worker before drawing.

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

The panel is 720 pt wide. Its height is search field + a section label
and up to 8 rows + footer, so it grows and shrinks as you type
(`window_height`, which must match the layout in `render`; see "Visual
design").

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
and when the shortcut is pressed while it is visible. Hiding clears the
query. Cmd-Q quits.

**Hotkey.** The shortcut (Option+Space unless changed) is registered with
the `global-hotkey` crate, which uses Carbon's `RegisterEventHotKey`.
That API needs no Accessibility or Input Monitoring permission. The press
is time-stamped in the event handler and sent through a channel to a GPUI
task. `on_next_frame` then logs the time from key press to the first
painted frame. Changing the shortcut is described under "Keyboard
shortcut" below.

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

**Packaging.** `scripts/bundle-app.sh` builds `target/release/Sift.app`
(see "Packaging and release" below). The executable is built as `sift-launcher` and renamed to `Sift` inside
the bundle. A binary named `Sift` would overwrite the `sift` CLI, because
APFS file names are case-insensitive.

**GPUI build.** GPUI 0.2.2 from crates.io, with default features off plus
`font-kit` and `runtime_shaders`. Runtime shaders mean building doesn't
need full Xcode and its Metal compiler; the shaders compile once on first
launch, which is why the first start after a build is slower (about
0.8 s against about 0.1 s).

## Live file search (Phase 2)

### Incremental updates

`sift_indexer::apply_changes` takes *directory-level* changes, which is
what FSEvents reports ("something in this folder changed"). For each one
it lists the directory, compares the listing with the stored children
(the `entries_parent` index added in schema v3), and applies the
difference in one transaction:

- names gone from disk → delete the entry and its subtree;
- new names → insert; new folders are walked in full with the same
  parallel walker as a full scan, because a folder moved in produces no
  events for its contents;
- same name, different kind (file replaced by folder) → replace;
- files with a new size or modification time → update the metadata.

The filesystem is the authority, not the event, so a change that is
applied twice, late, or out of order is harmless. That is what makes
coalesced, replayed and duplicated events safe. A path that is not
indexed yet is handled by re-listing its nearest indexed ancestor.
Changes inside excluded, dependency or package folders are ignored, using
the same rule as the full scan (`descends`). If the root itself is
missing, nothing changes (an unplugged drive must not wipe its index).

New ids are allocated above every existing id, so a child's id stays
larger than its parent's, as the snapshot builder requires.

Tests compare each update with a fresh full scan of the same tree,
including 60 random create/delete/move edits.

### FSEvents (`sift-watch`)

A thin wrapper over the C API: one stream for all roots, delivered on a
private dispatch queue, with `NoDefer` (the first change after a quiet
period is delivered at once) and `WatchRoot` (report the roots themselves
moving). Each event has a system-wide id. A root stores the last id
applied to it, plus the UUID of the volume's event store
(`FSEventsCopyUUIDForDevice`); the id is only meaningful with that UUID.

Recovery rules, in the index service:

| situation | action |
|-----------|--------|
| start, saved id and UUID valid | replay from the oldest saved id (catches changes made while sift was not running) |
| start, no saved id, or UUID changed | rescan the root once, recording the id from *before* the scan |
| `MustScanSubDirs`, `UserDropped`, `KernelDropped` | rescan that subtree |
| `EventIdsWrapped` | rescan every root |
| `RootChanged`, mount, unmount | re-check roots: missing → *unavailable* (index kept); back → rescan |

Because all roots share one stream and FSEvents delivers in id order,
after each batch every root's saved id can advance to the batch's highest
id, changed or not. Replay after a restart therefore starts close to where
it stopped. `sift index` in a terminal also records the id from before
its scan, so the launcher can resume from a CLI scan.

### Index service (`sift-files`)

One background thread owns the writable store:

1. collects events: FSEvents latency 300 ms (with `NoDefer`, the first
   event is immediate), then waits for 150 ms of quiet, at most 2 s;
2. applies them with `apply_changes`, recording the event position in the
   same transaction;
3. rebuilds the name snapshot (written to a new file and renamed into
   place) and swaps a freshly opened index into the shared `FileIndex`.

Searches hold the `FileIndex` mutex only for the search and the path
lookup (well under a millisecond). The old mapping stays valid during a
swap because the old snapshot file is never modified, only replaced.

Rebuilding the whole snapshot is the simple first version of step 3. It
costs about 25 ms at 100k entries (BENCHMARKS.md) but grows linearly, so
at around a million entries an in-memory delta (added and removed ids,
merged into a fresh snapshot in the background) would be needed.

The service also runs the settings commands (add, remove, re-index,
change options) and a cheap `Refresh` each time the panel opens. That
re-checks root availability and picks up changes another process made to
the index (such as `sift index`).

### File provider and ranking

`FileProvider` answers queries of two or more characters (a single
character matches a large part of any index and would bury the apps). It
checks each result with one `lstat` before offering it, so a file deleted
a moment ago is never shown, even before the update lands.

`sift_core::Search` runs providers in parallel and orders results by:

1. match tier (exact, stem, prefix, word, substring, fuzzy), so `notes.txt`
   (exact) beats a fuzzy app match;
2. provider priority (apps, then commands, then files), so `Notes` the app
   precedes `notes.txt` when both match equally well;
3. the provider's score, then the title.

Results with the same id (an app inside an indexed folder) appear once.

### Launcher and settings

Enter opens a file with its default application (`NSWorkspace
openURL:configuration:`), and ⌘Enter shows it in Finder. Icons render on
the background executor: apps and packages get their own icon, documents
share one per extension, and folders share one. A long result list
therefore costs a handful of renders.

The *Sift Settings* command opens a normal window. One scrolling page
holds General (shortcut, Open at Login, updates, a button that opens
`config.toml`), Clipboard History (on or off, history size, Clear History
with a confirmation step) and Locations: the indexed folders
with their state, item count and last scan, with:

- Add Folder (the standard folder picker);
- Re-index and Remove;
- Exclude Folder, and Include again to undo it;
- a dependency-folder toggle.

The home folder and the startup disk are refused with an explanation.
Opening the window and the picker happens in a foreground task outside
GPUI updates, for the same re-entrancy reason as resizing.

### Permissions

Sift reads folder listings only and never file contents. Reading
Documents, Desktop or Downloads makes macOS ask once ("Sift would like to
access files in your Documents folder"); choosing a folder in the picker
grants access to it. No Full Disk Access or Accessibility permission is
used. Builds are ad-hoc signed, so macOS may ask again after a rebuild.
Security-scoped bookmarks would only matter for a sandboxed build, which
Sift is not.

## Productivity features (Phase 3)

Every feature here is a provider or a small piece of launcher state. None
runs in the background unless the user turns it on, and each one's cost
is in BENCHMARKS.md.

### Results that are not files

`sift-core` gained result kinds (calculator, snippet, clipboard) and
actions: open a URL, copy text, run a shell command. Actions have a
stable text form (`app:`, `path:`, `url:`, `copy:`, `shell:`, `run:`) for
storing recents and favorites. Decoding accepts a built-in command id
only if the command still exists.

A rank key's tier sits in bits 48–55 (1 fuzzy … 6 exact). Tier 7 is new
and reserved for results that answer the query outright: the calculator
value and, in a `clip` search, clipboard entries.

### Usage: recents, favorites and boosts (`sift-state`)

`state.db` (SQLite, WAL) has three tables: `usage` (id, kind, title,
subtitle, encoded action, count, last use; pruned to 500 rows),
`favorites` and `clipboard`. Times are Unix seconds and are passed in, so
tests control the clock.

Search order is now: tier, then **usage boost**, then provider, then
score. The boost is `1 + log2(count × age weight)`, from 1 to 7. The
weight is 1 within a day, 0.7 within a week, 0.5 within a month and
0.25 after that. Favorites add 8, so a favorite outranks anything used
merely often. Because the boost comes after the tier, an exact name
always beats a frequently used fuzzy match. Boosts are recomputed after
each use (about 0.1 ms) and handed to `Search` as a shared map.

The empty query shows favorites, then up to 8 recent items. Stored rows
are never trusted as they are:

- snippets and custom commands are looked up again in the current
  config, so a command deleted from the file cannot run from history;
- files and apps must still exist (one `lstat` each);
- calculator results, clipboard entries and *Quit Sift* are never
  recorded.

The list is computed when the launcher hides (the query is cleared then),
so showing the launcher reads nothing from disk.

### Configuration (`config.toml`)

The file is TOML: `[clipboard]` options, `[[snippets]]`
(name, optional keyword, text) and `[[commands]]` (name plus exactly one
of `url`, `open`, `shell`). A parse error falls back to defaults, and an
invalid command is dropped. Either way, each problem is reported in the
launcher instead of failing silently. The file is checked with one `stat`
each time the launcher opens, and reloaded when its modification time
changes. *Edit Sift Configuration* writes a commented template if there
is no file, and opens it in the default editor (TextEdit when nothing is
registered for `.toml`).

**Written from Settings.** The settings window changes three options
(`[clipboard] enabled` and `max_items`, `[updates] check`) through
`Config::save_setting`. It edits the file with `toml_edit`, so comments,
order and everything else stay as they were, and only the value changes
(a comment after it stays too). A missing file starts from the template.
A file `toml_edit` cannot parse is refused with a message, so a hand edit
in progress is never overwritten, and an edit the launcher could not read
back is never written. The new text goes to a temporary file that is
renamed over the target. A symbolic link (a dotfiles checkout) is
followed, so the linked file changes and the link stays. The launcher
then reloads at once: turning clipboard history on starts watching
straight away. The settings window also reloads (one `stat`) when it
becomes active, so a hand edit shows up there.

**Location.** `SIFT_CONFIG_DIR/config.toml` when that variable is set.
Otherwise the first existing file of `$XDG_CONFIG_HOME/sift/config.toml`,
`~/.config/sift/config.toml` and the support directory's `config.toml`.
`~/.config` is checked even without `XDG_CONFIG_HOME`, because an app
opened from the Dock or at login doesn't see a shell profile's
variables. With no file yet, a new one goes in `$XDG_CONFIG_HOME/sift`
if that is set, else the support directory, so setting the variable
never hides a config already in use. The path is chosen once at launch
and logged. Everything else (`state.db`, `hotkey`, `update-check`)
stays in the support directory.

Shell commands run with `/bin/sh -c` from the home folder, with no
input. A background thread logs the exit status and the start of stderr.
They come only from the user's own config file.

### Calculator (`sift-calc`)

A recursive-descent evaluator over `f64`:

- `+ - * / % ^` (also `× ÷ **`), parentheses and unary minus;
- implicit multiplication (`2pi`, `3(1+2)`);
- `pi`, `tau`, `e`, and common functions.

There are no variables or assignments. Input is capped at 256 bytes and
nesting depth at 200.

To keep ordinary searches clean, a result appears only for a real
calculation: at least one operation, every token understood, and a
finite value. `2048`, `pi`, `1password`, `e-mail` and `1/0` show
nothing. Values are shown with up to 10 decimals, trailing zeros
trimmed (so 0.1 + 0.2 is 0.3), and in scientific notation outside
1e-6..1e15.

### File filters (`sift-files::FileQuery`)

Filter words are split from the name:

- `ext:` and `type:` (categories of extensions, or file/folder);
- `in:` (part of the containing path, `~` expanded);
- `size:` (`>10mb`, `<1k`);
- `modified:` (`<7d`, `>1y`).

Unknown `word:value` tokens stay part of the name.

Extension and kind filters run inside the index scan. The engine's
`Query` now takes a list of extensions. With only an extension, the
needle is `.`, so `type:image` is a single scan. Path, size and date
filters are checked on up to 2000 top matches, resolved in batches of
100 and stopping once enough pass. The cost is therefore bounded, but a
very selective filter on a common name can miss matches beyond the
first 2000. Queries with filters search files only, and a filter that
is not understood is explained where "No results" would be.

### Clipboard history

Opt-in (`[clipboard] enabled = true`).

- **Watching.** A foreground task reads `NSPasteboard.changeCount` every
  750 ms, and reads the contents only after a change. What was on the
  clipboard before watching started is not recorded.
- **Private content.** Content whose types include
  `org.nspasteboard.ConcealedType`, `TransientType` or
  `AutoGeneratedType` is skipped without reading the text. Password
  managers mark items this way (nspasteboard.org).
- **What is kept.** Plain text only, deduplicated (copying again moves an
  entry to the top), capped at `max_items`. Blank text and text over
  64 KiB are skipped.
- **Pinning.** ⌘D sets `clipboard.pinned_at` (schema 2). Pinned entries
  come first, in the order they were pinned, and do not count towards
  `max_items`; *Clear History* leaves them. Schema 2 was the first
  in-place upgrade of `state.db`: before migrating, `StateDb` copies the
  file to `state.db.schema-<old>` with `VACUUM INTO`, because an older
  Sift refuses a newer schema and the copy is the way back.
- **Deleting one entry.** ⌃X, which removes other results from recents,
  deletes the selected entry, pinned or not.
- **A stack.** The unpinned entries, newest first, are a stack whose top
  is on the clipboard: copying pushes. When ⌃X or *Clear History*
  deletes the text the clipboard holds, Sift writes the newest remaining
  unpinned entry there, or empties it when none is left. That write is
  recognised by its change count and not recorded as a copy, so the entry
  keeps its age and source.
- **Source app.** macOS does not say which app wrote to the pasteboard,
  so an entry records the frontmost app when the change is noticed
  (`sift_macos::frontmost_app`; name and bundle id, schema 3). That is
  the copying app unless the user switched apps within the 750 ms poll.
  Sift itself is never a source: its settings window is skipped, and a
  copy Sift made (Enter on an entry) is recognised by the change count
  right after writing, and keeps the entry's old source. `from:` in a
  `clip` search matches the app name.
- **Content types.** `clipboard::Content::of` sorts an entry into link
  (http/https), email address, hex colour, path (`/…` or `~/…`), code or
  plain text, once when the history loads. It never touches the file
  system and reads at most 4 KB. The type picks the row's tile (a colour
  is drawn as a swatch) and what ⌘↵ does: open the link, open a
  `mailto:` link, or reveal the path in Finder. Nothing is opened or run
  without that key press, and paths are only revealed, never opened.
- **Preview.** In a `clip` search with results, the list narrows to
  300 pt and the selected entry is shown beside it: the text wrapped
  (code in Menlo), a colour as a swatch with its RGB values, and the
  source app, age and word, character and line counts below. The panel
  then always has the height of 8 rows, so one entry still gets room.
  Only the first 3,000 characters are laid out (the rest is counted),
  which bounds text shaping for a 64 KiB entry. Each entry gets its own
  scroll element, so the preview starts at the top.
- **Ignored apps.** `[clipboard] ignore_apps` (empty by default) names
  apps, by name or bundle id, whose copies are dropped before anything
  is written. It relies on the same frontmost-app guess, so it is a
  convenience on top of the concealed-type check, not a guarantee.
- **Searching.** Entries are searched only after a `clip` prefix, never
  mixed into ordinary results, and Enter copies an entry back. Entries
  that contain every word come first, in history order; then entries
  whose title (the first 100 characters) matches with `NameMatcher`, the
  same tiers and fuzzy matching as names, best first. Only the title is
  matched loosely, since almost any letters appear in order somewhere in
  a long text, and only for the newest 2,000 entries not matched by
  words, which bounds the search near 2 ms (BENCHMARKS.md, 2026-10-26).
- **Paste and pop.** An optional shortcut (`hotkey::Slot::Pop`) pastes
  the top of the stack and deletes it. Pressing ⌘V for the user needs
  synthetic key events and so Accessibility, which is checked on each
  press and requested when the shortcut is recorded; nothing else in
  clipboard history needs it. Sift makes sure the clipboard holds the
  top entry, hides the panel (it never activates Sift, so the app
  behind gets the keys), and on the background executor waits up to a
  second for the shortcut's modifier keys to be let go, so they do not
  mix with ⌘, then posts ⌘V (`sift_macos::paste`). The key is the one
  that types "v" with ⌘ on the current layout (`UCKeyTranslate`), so
  Dvorak and AZERTY work. 300 ms later, the time the app gets to read
  the clipboard, the entry is deleted and the next one written to the
  clipboard, unless something new was copied meanwhile. Presses made
  during that time are queued, so pasting twice quickly cannot paste
  the same entry twice or the second one early.
- **Turning it off** stops the task and hides the history.
  *Clear Clipboard History* deletes it, except pinned entries.
- **Tests** use private, uniquely named pasteboards and never touch the
  user's clipboard.

### Quick Look (`sift-macos::QuickLook`)

`QLPreviewPanel` finds its data source through the key window's responder
chain. `QuickLook` defines a small `NSResponder` subclass
(`SiftQuickLookController`) that:

- adopts `QLPreviewPanelDataSource` (one item, the selected file's URL);
- accepts panel control;
- is inserted as the launcher window's `nextResponder`. AppKit does not
  retain that pointer, so `QuickLook` owns the controller and restores
  the chain when dropped.

Two details matter:

- The launcher's app is never active, because its panel is
  non-activating. The preview panel is therefore set not to hide on
  deactivation, and placed one level above the launcher.
- The preview panel makes itself key when it appears. As the panel's
  delegate, the controller hands key status back to the launcher in
  `windowDidBecomeKey:`, so typing, arrows and Esc keep working.

The launcher defers every show, move or close of the preview out of GPUI
updates, for the same re-entrancy reason as resizing. If it loses focus
while a preview is open, it waits 150 ms before deciding whether to
hide, because opening the preview moves focus for a moment.
`examples/quicklook_check.rs` verifies the wiring without a GUI session:
the controller is found and the launcher stays key.

## Reliability, performance and accessibility (Phase 4)

### Crash recovery

The design rule is that a crash at any moment leaves data that is either
intact or detectably damaged. Detected damage is set aside, never
deleted, and rebuilt from what the user chose.

- **Scans are one transaction.** A killed `sift index` leaves the root
  either absent or complete. `crates/sift-cli/tests/crash.rs` kills scans
  with SIGKILL at ten points and checks this.
- **The name snapshot** is written to `names.tmp-<pid>` and renamed into
  place. Temporary files older than ten minutes, left by a killed
  writer, are removed before the next write.
- **`roots.bin`** (`sift-files::roots_file`), next to the index, holds
  the list of indexed folders and their options. It is length-prefixed
  binary, because paths may contain any byte, and is written to a
  temporary file, then renamed. The database holds the same list, but
  the database is a cache, while the folder list is the user's choice.
- **Opening the index** (`IndexService`): if SQLite reports corruption,
  or a requested `quick_check` fails, the database (with `-wal` and
  `-shm`) and the snapshot move into `damaged-<time>/`. A fresh database
  is then rescanned from `roots.bin`, and the panel says where the
  damaged copy went.
- **Panics in the index thread** are caught per command and per event
  batch (`catch_unwind`). The service then rescans every root, restarts
  the watcher and reports it, instead of silently stopping updates.
- **`state.db`** (recents, favorites, clipboard history) gets the same
  treatment: on corruption it is renamed to `state.db.damaged-<time>` and
  started again, with a notice.
- **Unclean exits.** The launcher writes its pid to `sift.pid` in the
  config directory and removes it on a normal quit. If the file is still
  there at the next start and that process is gone, the previous run
  crashed or was killed. The index is then checked with `quick_check`
  before use (10.6 ms at 100k entries).
- **Logging.** The log goes to `~/Library/Logs/Sift/sift.log` at INFO,
  and to stderr at the `SIFT_LOG` level. The file is rotated to
  `sift.log.1` at startup past 1 MiB, and capped at 4 MiB per session. A
  panic hook records the panic and a backtrace. Logs stay local.

### Checking correctness

- **`sift verify`** runs SQLite's `quick_check`, then nine SQL
  invariants (`Store::verify`): every parent exists and is a folder in
  the same root, ids increase from parent to child, names are unique per
  folder, root rows and entry counts are consistent. Unless `--quick` is
  given, it then scans each root into a scratch database and compares
  path, kind, size and modification time (`sift_indexer::compare`).
  Folder sizes and dates are ignored. It exits non-zero on any finding.
- **Live-update fuzzing.** A test in `crates/sift-files/tests/service.rs`
  makes random creates, edits, renames, moves and removals in a temporary
  tree while the real service watches it through FSEvents. It then waits
  until the index matches a fresh scan and passes `verify`. The seed and
  length come from `SIFT_FUZZ_SEED` and `SIFT_FUZZ_OPS`.

### Responsiveness

- **Draw when results arrive.** GPUI draws on display-link ticks. A
  keystroke's results arrived in about 4 ms, then waited up to a refresh
  interval to be drawn. `set_results` now asks the panel's layer to
  display right away (`draw_soon`, the same call as `Panel::draw_now`),
  from a foreground task outside the GPUI update. Keystroke to frame went
  from a median 15.4 ms to 2.6 ms.
- **UI bench.** `SIFT_UI_BENCH=<rounds>` (`uibench.rs`) makes the
  launcher type queries into itself and report keystroke-to-frame and
  show-to-frame latencies. `scripts/ui-bench.sh` runs it and then reads
  memory and idle CPU.

### Memory and idle cost

- **Icons** are decoded into GPUI `RenderImage`s on the background
  executor, so the launcher owns every decoded icon.
  `icons::IconCache` keeps application icons (released and re-added when
  the catalog reloads) and at most 256 others, least recently used first
  out. Evicted icons are removed from the texture atlas
  (`App::drop_image`).
- **Clipboard history** polls the pasteboard change counter on a
  background-QoS thread (`sift_macos::ClipboardWatcher`). It wakes the
  main thread only when the counter changed. Hidden with history off,
  the launcher has 0.7 context switches a second, and `sample` shows
  every thread blocked.

### Accessibility

GPUI 0.2 draws into one Metal view and has no accessibility tree, so
VoiceOver cannot walk the result list. The launcher does three things
instead, with no extra permission:

- **Announcements** (`sift_macos::a11y::announce`, AppKit's
  `NSAccessibilityAnnouncementRequestedNotification`). When the highlight
  moves, it says the result, kind, folder name and position, interrupting
  other speech. When the summary changes, it waits 400 ms after the last
  change, then says the message, the empty-result text, or the count and
  top result, politely. The text comes from `speech.rs`. VoiceOver state
  is read each time the panel opens.
- **The window is titled "Sift"** (the title stays hidden), so VoiceOver
  and the window list name it.
- **Display settings.** Increase Contrast switches to stronger colours
  and outlines the selected row. Differentiate Without Color adds the
  outline, so the selection does not rely on colour alone. The settings
  are read when the panel opens (about 15 µs). The panel has no
  animations, so Reduce Motion needs nothing, and it is opaque, so Reduce
  Transparency needs nothing either.

These AppKit calls are made on the main thread only; elsewhere they return
defaults. Calling them from test threads crashed the test binary about
once in 130 runs.

## Visual design

The launcher and the settings window share one set of colour tokens
(`theme.rs`) and one layout grid. The aim is a calm, native-feeling panel
that costs nothing extra to draw.

**Layout (launcher, in points).**

| part | size |
|------|------|
| search field | 56 high; 19 pt text |
| section label ("Results", "Favorites and Recents", "Clipboard History") and "n of m" | 26 high; 11 pt |
| result rows | 42 high, inset 8 from the window edge, 8 pt corner radius; 28 pt icons |
| message (no results, errors) | 56 high, centred, at most two lines |
| footer | 38 high, on its own surface colour |
| hairlines | 1 pt |

The search icon, section labels and result icons share a left edge (18
pt). The search icon is centred over the 28 pt icon column, so the typed
text and the result titles start at the same x.

- **Rows.** A medium-weight title, then the folder or detail on the same
  line in secondary text. The detail truncates first, and the title
  never takes more than three quarters of the line. The kind (and a ★
  for favorites) sits at the right in tertiary text.
- **Icons.** Results without a file get a coloured tile with a white
  symbol: commands grey ⌘, calculator orange =, snippets purple ¶,
  clipboard teal (an SVG). The tile colours match in light and dark, like
  app icons.
- **Footer.** The status is on the left, with a dot: green when the index
  is up to date, the accent colour while loading or indexing, red if a
  folder is unavailable. The main action for the selection (Open, Run or
  Copy) is on the right in primary text with its key cap. After a
  separator come the other shortcuts in secondary text.

**Colour.** Tokens are `background`, `surface` (footer), `card`
(settings), `divider`, three text levels (`text`, `muted`,
`placeholder`), `selected`, `hover`, `accent` and `on_accent`, key caps,
and `error`, `success` and `favorite`. Each exists for light and dark at
normal and increased contrast. The accent follows System Settings ›
Appearance (`sift_macos::a11y::accent_color`, read with the theme when
the panel opens). A test checks WCAG contrast for every text level on
every surface: 7:1 for primary text, 4.5:1 for secondary, 3:1 for
tertiary and for text on the accent.

**Settings window.** Sections are cards under small headings, and the
page scrolls as a whole. History size is a row of choices (50, 200, 500,
1,000, plus the configured value if it is another one). Each folder is a
card with a folder tile, the path and its details, and a status dot. Below a hairline come the "Skip
dependency and cache folders" switch and the folder's buttons
(secondary, plus a red Remove). The footer holds the index facts and
the one primary button, Add Folder…. A damaged-index or picker message
appears as a tinted banner above it.

**What it costs.** Surfaces are opaque, with no blur, transparency,
shadows on the panel's content, or animation, so a frame is drawn only
when something changes. The symbols are three small SVGs compiled into
the binary (`assets.rs`). GPUI rasterises each once per size into its
texture atlas. Text levels and weights use the system font, which GPUI
already loads. Nothing drawn per frame may take the file index lock, which
a background search holds for its whole run. The footer's file count
therefore comes from the service status, not `FileIndex::len`, after a
first version that read it every frame raised p95 latency by 1.7 ms.
Measurements before and after are in BENCHMARKS.md.

## Packaging and release (Phase 5)

### The app bundle

`scripts/bundle-app.sh` builds `target/release/Sift.app` from
`packaging/`:

- **`Info.plist`** is a template. The script fills in the version from
  `Cargo.toml` and a build number (the commit count, so it always grows),
  then checks it with `plutil -lint`. Bundle id
  `io.github.kylianmarceau.sift`, `LSUIElement` (no Dock icon or menu
  bar), `LSMinimumSystemVersion` 13.0.
- **Deployment target 13.0** (`MACOSX_DEPLOYMENT_TARGET`), because
  `SMAppService` needs it. The build uses `--locked`.
- **Resources:** `Sift.icns`, `LICENSE`, and
  `THIRD_PARTY_LICENSES.txt`, generated by
  `scripts/third-party-licenses.py` from `cargo metadata`. It lists every
  crate linked into the app on Apple Silicon (build-only and proc-macro
  crates are left out), with each distinct licence text once. Crates
  that ship no licence file are listed with their SPDX expression.
- **Signing:** ad-hoc, with the hardened runtime, so a local build
  behaves like a release. Sift needs no entitlements: it is not
  sandboxed, and the hardened runtime's defaults (no JIT, no unsigned
  libraries) suit it. GPUI compiles its Metal shaders at runtime, which
  the hardened runtime allows.

Changing the bundle id from the development `dev.sift.Sift` means macOS
treats this as a new app: it asks again for Documents, Desktop or
Downloads, and the old id's permission entries stay until removed in
System Settings.

**Symbols.** The release binary keeps its symbols (12 MB binary, 13 MB
bundle; the zip is 5.0 MB and the disk image 5.5 MB). Stripping would
save about half, but crash backtraces in the log would lose their
function names, which are the only crash information Sift collects.

### The icon

`packaging/icon.svg` is the source: a magnifying glass over three lines
that narrow, like a sieve, on a blue-violet tile. It follows Apple's
macOS icon grid (an 824 pt tile on a 1024 pt canvas, with room for the
shadow). `scripts/make-icon.sh` renders it at every size with resvg (the
`render_icon` example, a dev-dependency only) and packs `Sift.icns` with
`iconutil`. The `.icns` is committed, so building the app needs neither.

### Release builds

`scripts/release.sh` makes a release on an Apple Silicon Mac. It refuses
to run from a working tree with uncommitted changes (unless told to),
runs fmt, Clippy and the tests, bundles, and then:

1. with `SIGN_IDENTITY`, signs with a Developer ID and a secure
   timestamp;
2. with `NOTARY_PROFILE`, submits the app to Apple's notary service with
   `notarytool`, staples the ticket, then does the same for the disk
   image, and checks both with `spctl` as Gatekeeper would;
3. packages a `.zip` (with `ditto`, which keeps the signature) and a
   compressed `.dmg` with an Applications link, and writes `SHA256SUMS`
   and `BUILD-STATUS`.

Credentials never pass through the script. `notarytool` reads them from
a keychain profile that the developer creates once
(`xcrun notarytool store-credentials`). See RELEASING.md.

**Apple Silicon only.** Intel Macs were last sold in 2023, and a
universal binary would double the download and need testing on hardware
not available here. The deployment target and code are not
Apple-Silicon-specific, so an Intel build remains possible.

### Open at Login (`sift_macos::login_item`)

`SMAppService.mainAppService` registers the app itself as a login item
(macOS 13+). This is the supported API: no helper app, no launch agent
plist, and the user sees and controls it in System Settings › General ›
Login Items. It is off until the user turns it on in Settings, and Sift
never registers itself.

The class is found at runtime and called through `msg_send!` (three
methods do not justify a binding crate). `status` maps to On, Off,
Needs Approval, or Unavailable. *Not found* is what an app bundle that
was never registered reports, so it reads as Off there and as
Unavailable for a bare development binary, which cannot be registered.
Settings reads the status when it opens and whenever its window becomes
active, so a change made in System Settings shows up.

### Update check (`sift-launcher::updates`)

The user chose to be told about updates, not to have them installed.
Sift never downloads or replaces code, so there is no updater to secure,
and no signing key whose loss could push code to every user.

- **When.** When the panel opens, if `[updates] check` is on (the
  default) and the last successful check is a day old. *Check for
  Updates* (launcher command and Settings button) checks on request,
  even with the setting off. A failed automatic check waits an hour
  before retrying. The time and the latest version seen are stored in
  `update-check`, a two-line text file in the support directory.
- **How.** A background thread runs `/usr/bin/curl` with HTTPS only
  (also for redirects), a 15 s timeout and a 1 MB reply limit, against
  the GitHub API's latest-release endpoint. `curl` uses the system's
  trust store and proxy settings and adds nothing to the binary; an HTTP
  client crate with TLS would add several megabytes. A 404 (no release
  yet, or the repository is private) means "nothing newer".
- **What is trusted.** Only `tag_name`, parsed as `major.minor.patch`.
  Drafts and pre-releases are ignored. The page Sift opens is always the
  fixed release page URL, never a URL from the reply, so a tampered
  reply can at most claim a version.
- **Display.** A newer version shows in the footer status ("Sift 0.2.0
  is available") and in Settings › General. A check runs on a background
  thread and costs nothing while idle.

## Keyboard shortcut (after 0.1.0)

Settings › General › Keyboard Shortcut shows the shortcut as key caps.
Clicking them records a new one; *Use ⌥Space* goes back to the default.

**Recording.** `sift_macos::key_capture` adds an AppKit local event
monitor for key presses. It sees presses in Sift's own windows before
they do, and swallows them, so the recorded keys don't also type or
trigger a menu. Local monitors need no permission (watching other apps'
keys would need Input Monitoring). The monitor exists only while
recording and is removed when recording ends, when the window loses
focus, or when it closes.

While recording, the current shortcut is unregistered. Carbon would
otherwise take the press before any window saw it, so pressing the
current shortcut would toggle the panel instead of being recorded.

**Physical keys.** A press reports the hardware key code (`kVK_…`) and
the character that key types in the current layout. The shortcut is
registered by key code, so it works the same whatever the layout. The
label ("K", "É") is taken from the layout at recording time, for
display only. GPUI's key events carry only the character, which on a
non-QWERTY layout would map back to the wrong physical key, so they are
not used. The key-code table matches `global-hotkey`'s, minus media keys
(watched through an event tap that needs a permission), Caps Lock, Help
and the input-source keys.

**Rules.** Escape alone cancels. A shortcut needs ⌘, ⌃ or ⌥ unless it
is a function key (F1–F20), so it cannot take over typing; Shift alone
is not enough. ⌘Space and ⌃Space are accepted, with a note that macOS
uses them for Spotlight and input sources by default.

**Registering.** The new shortcut is registered at once, then saved in
`hotkey` in the support directory: two lines, `global-hotkey`'s notation
("control+alt+KeyK") and the label. If macOS refuses it, the previous
shortcut is restored. At launch, a missing, damaged or unusable file
means Option+Space, and a saved shortcut that cannot be registered
falls back to Option+Space too.

**Clipboard history shortcut.** A second, optional shortcut
(`hotkey::Slot::Clipboard`, saved in `clipboard-hotkey`, empty or
missing for none) opens the panel with `clip ` typed, or hides it if it
already shows clipboard history. It has no default: the obvious choices
are taken (⌥⌘V moves files in Finder). A third, the paste-and-pop
shortcut (`Slot::Pop`, `paste-pop-hotkey`), works the same way (see
Clipboard history). All go through one `GlobalHotKeyManager`, whose
handler matches the event id against each slot. Recording any pauses
all, and no slot takes another's shortcut, since they would share a
hotkey id.

**Conflicts with other apps.** Carbon lets two processes register the
same shortcut without an error (tested: two Sift copies both registered
⌃⌥K, and a second copy registered ⌥Space while the first held it). So
Sift cannot tell that another app uses a shortcut. It reports only what
macOS itself refuses.

**What it costs.** Nothing while idle: the monitor and its task exist
only during recording, and the shortcut is read from a small file once
at launch.

## What is deliberately deferred

- A snapshot delta overlay for very large indexes (see "Index service").
- `getattrlistbulk` scanning, and Launch Services package detection.
- Launcher: lazy loading of
  application icons (they are rendered with the catalog), and app-folder
  watching (the catalog currently refreshes by age).
- A real accessibility tree for the result list (needs GPUI support).
  Announcements stand in for it.
- Security-scoped bookmarks (only needed for a sandboxed build).
- Auto-paste of snippets and clipboard entries (needs Accessibility),
  and non-text clipboard history.
- Editing snippets and custom commands in Settings; they stay in the
  file.
- Full-text content indexing (out of scope for the MVP).
- Installing updates automatically (Sparkle or similar). Sift only
  reports them.
- Intel and universal builds.
