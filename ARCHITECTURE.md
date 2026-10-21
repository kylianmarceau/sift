# Architecture

This document records the technical decisions behind sift and the
reasoning for them. It covers what exists today (Phase 0: the indexer and
search engine); later phases add sections as they land.

## Workspace

```text
sift/
├── crates/
│   ├── sift-store/    SQLite persistence: schema, root replacement, path resolution
│   ├── sift-indexer/  parallel filesystem scanner that writes into the store
│   ├── sift-search/   name folding, memory-mapped snapshot, matching and ranking
│   └── sift-cli/      `sift` binary: index / search / status / remove / bench
├── scripts/bench.sh   reproducible benchmark run
├── PLAN.md            roadmap and milestone status
└── BENCHMARKS.md      measured results
```

Dependency direction: `cli → {indexer, search} → store`. The indexer and
search crates do not know about each other; the CLI (later the launcher)
composes them. There is deliberately no `core` crate yet: the only shared
domain types (`EntryKind`, `EntryId`) live in `sift-store`, which every
other crate already depends on. A `core` crate for launcher providers and
result types arrives with Phase 1, when there is something real to put in it.

## Storage: SQLite as the source of truth

`index.db` (WAL mode) holds:

- `roots`: indexed directories with scan statistics.
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
meta     u16×n  kind (2 bits) | depth (14 bits)
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
| 32–47 | earlier match position (substring tiers) or nucleo score (fuzzy) |
| 16–31 | shorter name |
| 0–15  | shallower path |

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
- Security-scoped bookmarks for sandboxed folder access (Phase 2/5).
- Full-text content indexing (out of scope for the MVP).
