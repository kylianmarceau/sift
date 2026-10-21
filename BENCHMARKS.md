# Benchmarks

All numbers here were measured, not estimated. Each table says what kind of
statistic it reports. To reproduce:

```bash
scripts/bench.sh                      # default: files 10k/100k, store-only 10k/100k/1M
FILE_COUNTS="10000" SYNTH_COUNTS="1000000" scripts/bench.sh /path/to/workdir
```

## 2026-10-21 — Phase 0 baseline

**Machine:** Apple M2 (4P+4E cores), 16 GiB, macOS 27.0.1, APFS on the
internal SSD. **Toolchain:** rustc 1.99.1, release profile (thin LTO,
1 codegen unit). **Code:** commit `189da10`. **Dataset:** `sift bench`
synthetic generator, seed 42 (see `crates/sift-cli/src/synth.rs`). Names
mix common words, separators, numbers and ~30 extensions. About 6% of
entries are folders, and the maximum depth is 10.

### Datasets

- **On-disk trees**: real empty files and folders created under `$TMPDIR`.
  These exercise the full scan path (directory listing, `lstat`, SQLite
  writes).
- **Store-only**: the same generator writing rows straight into the
  database under a virtual root, skipping the filesystem. These measure
  storage and search at 1M entries without creating a million real files.

### Indexing (on-disk trees)

Single runs (one sample each). The page cache was warm because the files
had just been created, so these are **warm-cache** numbers. A cold-cache
measurement needs `sudo purge` and was not run.

| entries | scan + write | total incl. commit | snapshot build | DB size | snapshot size | peak footprint | peak RSS |
|--------:|-------------:|-------------------:|---------------:|--------:|--------------:|---------------:|---------:|
| 10,001  | 16.9 ms  | 18.4 ms  | 22.5 ms | 384 KiB | 373 KiB | 4.1 MiB  | 6.6 MiB  |
| 100,001 | 158.2 ms | 169.7 ms | 45.1 ms | 3.7 MiB | 3.6 MiB | 11.8 MiB | 14.2 MiB |

Re-indexing (which replaces the existing root in one transaction) took
19.9 ms and 169.5 ms respectively, with the same memory. Peak footprint
and RSS come from `/usr/bin/time -l` and include the snapshot build.

**Not measured yet:** indexing 1,000,000 real files (deferred until agreed,
since it creates a million files on disk), cold-cache scans, and real
user directories.

### Store-only index build

Single runs.

| entries   | write rows | snapshot build | DB size  | snapshot size | peak footprint | peak RSS |
|----------:|-----------:|---------------:|---------:|--------------:|---------------:|---------:|
| 10,001    | 6.2 ms     | 5.3 ms         | 372 KiB  | 373 KiB       | 2.9 MiB        | 5.4 MiB  |
| 100,001   | 67.3 ms    | 24.6 ms        | 3.7 MiB  | 3.6 MiB       | 9.7 MiB        | 12.2 MiB |
| 1,000,001 | 818.8 ms   | 206.5 ms       | 37.0 MiB | 36.5 MiB      | 47.3 MiB       | 49.8 MiB |

At 1M, most of the 47 MiB peak is the snapshot builder assembling its
arrays in memory before writing them (by design, about the size of the
snapshot), plus the generator's own bookkeeping.

### Search latency (in-process, warm)

`sift bench search`: each query runs once untimed ("first", the first
query after opening the index), then 50 timed runs. Times cover the
engine search only. "resolve" is the median time to turn the 20 returned
hits into paths and metadata through SQLite. Result limit 20, default
rayon pool (8 threads).

**Statistics: first = single sample; median, p95 and max over 50 runs.**

1,000,001 entries (snapshot 36.5 MiB):

| query | matched | fuzzy pass | first | median | p95 | max | resolve (median) |
|-------|--------:|:----------:|------:|-------:|----:|----:|-----------------:|
| `readme.md`      | 126     | no  | 0.435 ms | 0.293 ms | 0.431 ms | 0.545 ms | 0.136 ms |
| `proj`           | 14,441  | no  | 0.474 ms | 0.384 ms | 0.463 ms | 0.495 ms | 0.114 ms |
| `report`         | 14,688  | no  | 0.475 ms | 0.437 ms | 0.507 ms | 0.522 ms | 0.114 ms |
| `port`           | 43,448  | no  | 0.483 ms | 0.494 ms | 0.630 ms | 0.651 ms | 0.109 ms |
| `budget-meeting` | 27      | no  | 0.255 ms | 0.222 ms | 0.314 ms | 0.332 ms | 0.199 ms |
| `annual report`  | 140     | yes | 0.693 ms | 0.501 ms | 0.599 ms | 0.782 ms | 0.239 ms |
| `prjrpt`         | 190     | yes | 0.949 ms | 0.760 ms | 0.912 ms | 0.931 ms | 0.237 ms |
| `a` (worst case) | 543,739 | no  | 3.560 ms | 3.168 ms | 3.409 ms | 3.478 ms | 0.113 ms |
| `zzqxj` (miss)   | 0       | yes | 0.406 ms | 0.389 ms | 0.529 ms | 0.578 ms | —        |

100,001 entries (snapshot 3.6 MiB): every query had a median ≤ 0.20 ms
except `a` (0.54 ms). The maximum over all runs was 0.62 ms.

10,001 entries (snapshot 373 KiB): every query had a median ≤ 0.02 ms
except `a` (0.15 ms). The maximum over all runs was 0.19 ms.

**Single-threaded** (`RAYON_NUM_THREADS=1`), 1,000,001 entries: medians
were 0.74–2.5 ms for the typical queries above, 0.97 ms for the miss, and
16.9 ms for `a`. Even without parallelism, every query stayed under the
20 ms target. Parallelism gives roughly a 3–5× speed-up here.

### One-shot CLI search (new process)

`sift search report --stats` as a fresh process at 1,000,001 entries
(single sample): index open 2.25 ms, which maps and validates the
snapshot; search 0.64 ms; resolve 0.24 ms. The process ended with a
**2.2 MiB physical footprint** and a 26.2 MiB peak RSS. The difference is
the memory-mapped snapshot: its clean, file-backed pages count toward RSS
but not footprint, and macOS can reclaim them under pressure.

### Baseline: querying SQLite directly

To check that the snapshot is worth having, the same 1,000,001-row
database was queried with the system `sqlite3` 3.54 CLI, three runs each:

| query | time (3 runs) |
|-------|---------------|
| `SELECT count(*) FROM entries WHERE name LIKE '%report%'` | 102, 94, 94 ms |
| `... WHERE instr(name, 'report') > 0` (case-sensitive) | 69, 69, 70 ms |

That is a count only: no ranking, no fuzzy matching, no accent folding.
The snapshot answers the same query, fully ranked, in 0.44 ms median
(1.75 ms single-threaded).

### Against the Phase 0 targets

| target | status |
|--------|--------|
| Typical indexed search < 20 ms | **Met on synthetic data up to 1M entries:** worst median 3.2 ms (8 threads), 16.9 ms (1 thread). Real-data validation pending. |
| Idle memory < 50 / 100 MB | **Not measurable yet:** there is no long-running process. The CLI's search footprint is 2.2 MiB at 1M entries. |
| Hotkey to visible < 50 ms | Not applicable until Phase 1. |
| Idle CPU ≈ 0 | Not applicable: no background process yet. |
