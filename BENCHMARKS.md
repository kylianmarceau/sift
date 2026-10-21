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

The Phase 1 section below covers the long-running launcher.

## 2026-10-21 — After the ranking and exclusion changes

Same machine and toolchain as above.

### Search at 1,000,001 entries (commit `1aa2a47`)

The whole-word and "own files first" ranking signals (ARCHITECTURE.md,
"Matching and ranking") add work per match. I re-ran `sift bench search`
on the same synthetic 1M dataset, 50 timed runs per query.

**Statistic: median over 50 runs.**

| query | before (`189da10`) | after (`1aa2a47`) |
|-------|-------------------:|------------------:|
| `report`         | 0.437 ms | 0.51 ms |
| `prjrpt` (fuzzy) | 0.760 ms | 0.77 ms |
| `a` (worst case) | 3.168 ms | 3.46 ms |

That is roughly 5–15% slower, still far below the 20 ms target. The other
queries were not recorded for this run.

### Real data: `~/Documents`

Indexed with the defaults (dependency and cache folders skipped). Single
runs, warm cache.

| | default options | before dependency skipping |
|---|---:|---:|
| entries | 54,206 | 263,000 (approx.) |
| database | 2.0 MiB | 11.5 MiB |
| scan time | 215 ms | — |

One-shot `sift search <q> --limit 20 --stats` in a new process (single
sample each):

| query | matches | open | search | resolve | footprint |
|-------|--------:|-----:|-------:|--------:|----------:|
| `project` | 76+    | 0.85 ms | 1.03 ms | 0.39 ms | 2.1 MiB |
| `report`  | 80+    | 0.73 ms | 0.28 ms | 0.37 ms | 2.1 MiB |
| `readme`  | 113+   | 0.67 ms | 0.21 ms | 0.40 ms | 2.1 MiB |
| `prjrpt`  | 22     | 0.64 ms | 0.84 ms | 0.50 ms | 2.3 MiB |
| `a`       | 27,836+ | 0.60 ms | 0.63 ms | 0.33 ms | 2.0 MiB |

## 2026-10-21 — Phase 1 launcher

Same machine. **Code:** commit `48e7368`, release build, run from
`target/release/Sift.app`. The catalog had 107 applications.

### Startup

Measured from the `INFO` log lines (`SIFT_LOG=info`, the default).

| run | process start → window ready | app catalog with icons |
|-----|-----------------------------:|-----------------------:|
| first launch after a build | 779 ms | 1,613 ms |
| warm, run 1 | 108 ms | 130 ms |
| warm, run 2 | 103 ms | 88 ms |
| warm, run 3 | 101 ms | 88 ms |

The first launch after a build is slower because GPUI compiles its Metal
shaders at runtime (see ARCHITECTURE.md, "GPUI build") and the icon
caches are cold. The catalog loads in the background, so the panel can be
used before it finishes.

### Idle memory and CPU

The launcher was started and left idle with the panel hidden.

| measurement | value | tool |
|-------------|------:|------|
| physical footprint after 10 s | 26 MB | `footprint <pid>` |
| peak physical footprint | 34 MB | `footprint <pid>` |
| resident size (RSS) | 63–64 MB | `ps -o rss` |
| CPU time used between 10 s and 70 s | 0.00 s | `ps -o cputime` (0:00.18 at both points) |

RSS includes shared system frameworks (AppKit, Metal) and is larger than
the footprint for that reason. The footprint is the number that matches
Activity Monitor's "Memory" column.

### Application search

`cargo run --release -p sift-apps --example bench_apps` runs each query
200 times against the 107 discovered apps. This is the provider alone,
without rendering. Discovery itself took 54.75 ms in that run, without
icons.

| query | median | p99 | top results |
|-------|-------:|----:|-------------|
| `s`               | 5.5 µs | 11.1 µs | Siri, Safari, Stocks |
| `te`              | 5.3 µs | 11.2 µs | Terminal, TextEdit, Notes |
| `term`            | 2.0 µs | 3.2 µs  | Terminal, System Information |
| `cal`             | 2.2 µs | 3.5 µs  | Calendar, Calculator, … |
| `vsc`             | 2.0 µs | 3.5 µs  | Visual Studio Code |
| `system settings` | 1.7 µs | 2.8 µs  | System Settings |
| `xyzzy` (miss)    | 1.7 µs | 2.5 µs  | — |
| `act mon`         | 2.0 µs | 3.2 µs  | Activity Monitor |

### Hotkey to first frame

**Real hotkey, before the fix** (`scripts/measure-hotkey.sh`, commit
`5b72241`, 20 presses by hand): min 11.70 ms, **median 48.22 ms**, p90
52.38 ms, max 75.44 ms. At that commit the clock stopped when GPUI
started the frame (an `on_next_frame` callback).

**Cause.** GPUI stops its display link while the window is hidden, and
starts it again only when AppKit reports the window visible. That report
is asynchronous and arrived about 45 ms after the panel was shown. The
fix (`Panel::draw_now`) asks the view's layer to display right after
showing, so GPUI draws immediately.

**Show path, before and after the fix.** A temporary probe, removed
afterwards, called the same `toggle` the hotkey uses every 300 ms, with
10 shows per run. The clock stops inside the first `render`. This
excludes the delivery of the key event itself (Carbon event to GPUI task,
well under a millisecond).

| build | min | median | max |
|-------|----:|-------:|----:|
| without `draw_now` | 47.94 ms | 48.53 ms | 58.26 ms |
| with `draw_now`    | 4.26 ms  | 5.30 ms  | 12.42 ms |

Layout, paint and GPU presentation follow `render`. The frame reaches the
display at the next screen refresh, up to one refresh interval later.

**Real hotkey, after the fix** (`scripts/measure-hotkey.sh`, commit
`a712c1b`, 21 presses by hand, clock stops inside the first `render`):
min 3.39 ms, **median 4.87 ms**, p90 10.64 ms, max 11.58 ms. That is
about 10× faster than before, and every sample is well under the 50 ms
target.

### Not measured yet

- Keystroke to rendered results (search plus layout plus paint), and
  memory after long use with many queries.

### Against the Phase 1 targets

| target | status |
|--------|--------|
| Idle memory < 50 MB | **Met:** 26 MB footprint idle, 34 MB peak. |
| Hotkey to visible < 50 ms | **Met:** with the real hotkey, 4.87 ms median and 11.58 ms max to the first render (48.22 ms median before the display-link fix). |
| Search < 20 ms | **Met** for the app provider: ≤ 11 µs p99. Rendering time was not measured separately. |
| Idle CPU ≈ 0 | **Met:** no CPU time used in 60 s idle. |
