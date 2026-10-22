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
| Idle CPU ≈ 0 | **Met with the panel hidden** (see the correction in the Phase 2 section). |

## 2026-10-21 — Phase 2: live file search

Same machine. **Code:** commit `5e4da4c`, release build.

### Change to searchable

`cargo run --release -p sift-files --example live_latency [files]` indexes
a temporary tree through the index service (default settings: FSEvents
latency 300 ms with NoDefer, 150 ms quiet period). It then creates,
renames and deletes a file 20 times each, timing from the filesystem call
until search reflects it. For deletes the clock stops when the index
drops the file; the launcher already hides deleted files immediately by
checking each result.

**Statistics: median, p90, min and max over 20 rounds.**

| tree | operation | median | p90 | min | max |
|-----:|-----------|-------:|----:|----:|----:|
| 10,801 entries  | create | 179 ms | 180 ms | 170 ms | 180 ms |
| 10,801 entries  | rename | 179 ms | 181 ms | 174 ms | 181 ms |
| 10,801 entries  | delete | 168 ms | 170 ms | 164 ms | 170 ms |
| 100,801 entries | create | 205 ms | 221 ms | 200 ms | 224 ms |
| 100,801 entries | rename | 206 ms | 208 ms | 196 ms | 209 ms |
| 100,801 entries | delete | 168 ms | 171 ms | 165 ms | 171 ms |

The 150 ms quiet period is most of it, by design: it batches bursts such
as a build or `git checkout` into one update. The rest is the update
itself plus the snapshot rebuild, which grows with index size (about
25 ms more at 100k). At around a million entries the rebuild would take on
the order of a second per update. That is the case for the snapshot delta
described in ARCHITECTURE.md.

In the running launcher (scratch index of 202 entries), the log showed
3–4 ms to apply each change and about 164 ms from delivery to searchable.

### Idle cost with live updates

The launcher was started with a scratch index (`SIFT_INDEX_DIR`), watching
one root.

| state | CPU time in 60 s | footprint | peak footprint |
|-------|-----------------:|----------:|---------------:|
| panel hidden (`SIFT_START_HIDDEN=1`) | 0.00 s | 25 MB | 35 MB |
| panel left open | 0.30 s (≈ 0.5%) | 28 MB | 38 MB |

The index thread blocks on its channel while idle, and FSEvents costs
nothing until something changes. The 0.5% with the panel open is GPUI's
display link, which keeps ticking while a window is visible.

**Correction to Phase 1:** the "0.00 s in 60 s" Phase 1 measurement
was taken while the screen was locked, which pauses the display link. The
numbers above separate the two states: hidden (the normal idle state of a
launcher) costs nothing, and a panel left open costs about 0.5%. Reducing
the open-panel cost is a Phase 4 item.

### Against the Phase 2 goals

| goal | status |
|------|--------|
| Files and folders searchable from the launcher | Done (two-character minimum). |
| Incremental updates (create, rename, delete) | ≈ 180 ms to searchable at 10k, ≈ 205 ms at 100k (median). |
| Changes made while sift was not running | Replayed at startup from the saved FSEvents position, without a rescan (tested end to end). |
| Recovery from lost events | Subtree or root rescans (tested on the update path; drop and wrap flags are not reproducible in tests). |
| Idle CPU ≈ 0 with watching on | Met with the panel hidden: 0.00 s in 60 s. |

## 2026-10-21 — Phase 3: productivity features

Same machine. **Code:** commits `38ae94f`–`b249021`, release builds.

### Calculator

`cargo run --release -p sift-calc --example calc_latency`: 200,000
rounds over each input set.

| input | time per query |
|-------|---------------:|
| expressions (`1+2*3`, `2^10 - sqrt(144)`, `log(1000) * ln(e)`, …) | 184 ns |
| ordinary searches, rejected (`safari`, `1password`, `e-mail`, …) | 73 ns |
| longest accepted input (255 bytes) | 2.4 µs |

The calculator adds well under a microsecond to a keystroke, and nothing
measurable to searches that are not arithmetic.

### File filters

`cargo run --release -p sift-files --example filter_latency -- <index>`
on the store-only synthetic indexes from `scripts/bench.sh`. Each query
asks for 40 results, timed over 30 runs. The timing covers the scan,
resolving paths and the post-filter. It leaves out the final existence
check, because the synthetic paths are not on disk.

| query | 100,001 entries median | p95 | 1,000,001 entries median | p95 |
|-------|-----:|----:|-----:|----:|
| `report` (no filter, for comparison) | 0.72 ms | 0.93 ms | 0.69 ms | 0.89 ms |
| `report ext:pdf` | 0.55 ms | 0.64 ms | 0.70 ms | 0.79 ms |
| `ext:pdf` (no name) | 0.90 ms | 1.07 ms | 2.97 ms | 3.38 ms |
| `type:image` (18 extensions, no name) | 1.56 ms | 1.85 ms | 9.71 ms | 12.92 ms |
| `report type:document` | 0.48 ms | 0.59 ms | 0.97 ms | 7.31 ms |
| `report in:src` | 12.77 ms | 13.25 ms | 18.47 ms | 18.84 ms |
| `report size:>1mb` | 20.17 ms | 23.55 ms | 34.29 ms | 36.81 ms |
| `report modified:<30d` | 19.81 ms | 22.74 ms | 32.82 ms | 35.74 ms |
| `a in:zzzz-no-such-folder` | 22.22 ms | 22.80 ms | 34.75 ms | 48.61 ms |

Extension and kind filters run inside the scan and cost about the same
as an unfiltered search. Path, size and date filters resolve top matches
until enough pass, at about 10–17 µs per match (one SQLite lookup and
path assembly each). The worst case is when nothing passes and all 2000
candidates are resolved: 20–35 ms. The synthetic entries carry no sizes
or dates, which makes the size and date rows that worst case. Real
queries usually stop earlier, as `report in:src` shows.

### State database

`cargo run --release -p sift-state --example state_latency`: a full
history (500 recents, 20 favorites, 200 clipboard entries of about
400 bytes), 200 runs each.

| operation | when it runs | median | p95 |
|-----------|--------------|-------:|----:|
| `record_use` (upsert + prune to 500) | after opening something | 273 µs | 307 µs |
| `recent(16)` | empty query (on hide) | 12 µs | 14 µs |
| `favorites()` | empty query, after ⌘D | 17 µs | 23 µs |
| `usage_boosts()` over 500 rows | after each use or ⌘D | 97 µs | 117 µs |
| `clipboard_add` (dedupe + cap) | after a copy, if enabled | 73 µs | 99 µs |
| `clipboard(200)` | after a copy, if enabled | 46 µs | 59 µs |

None of these run while typing. Showing the launcher costs one `stat` of
`config.toml`.

### Clipboard history idle cost

The release launcher was started hidden twice with scratch config and
index directories, for 60 s after a 10 s settle. Nothing was copied
during either run, and the history stayed empty.

| setting | CPU time in 60 s |
|---------|-----------------:|
| clipboard history off (default) | 0.00 s |
| clipboard history on (750 ms polling) | ≤ 0.01 s |

`ps` reports CPU time in 10 ms steps, so 80 counter reads a minute cost
less than one step. With the feature off, no timer exists.

### Not measured

- Hotkey-to-first-frame after Phase 3. The empty-query list is computed
  when the launcher hides, and showing adds one `stat`, so no change is
  expected. `scripts/measure-hotkey.sh` needs a real key press.
- Quick Look open time. The panel animates itself in, and it was checked
  for correctness only (`examples/quicklook_check.rs`).

## 2026-10-21 — Phase 4: optimization and polish

Same machine as above. Release builds. Each table says what it reports.

### Typing in the launcher (`scripts/ui-bench.sh`)

The launcher types queries into its own search field one character at a
time, 60 ms apart, through the real search path. It runs against a copy of
the 100k-file benchmark index plus the installed applications, with the
panel on screen. The clock starts when the character is inserted. It
stops inside the first `render` that shows the new results, so layout,
paint and presentation follow, as in the hotkey measurement above. Seven
queries, from `safari` to `monday ext:pdf`, give 59 keystrokes per round.

**The fix.** New results arrived 3.9 ms (median) after a keystroke, but
the frame waited for GPUI's next display-link tick, about 11 ms later. The
launcher now asks for a draw as soon as results arrive (`draw_soon`, the
same mechanism as `Panel::draw_now`).

| build | rounds | keystrokes | median | p95 | max |
|-------|-------:|-----------:|-------:|----:|----:|
| before (`acdeeee`) | 3 | 177 | 15.4 ms | 18.5 ms | — |
| after (`b2dab7c`)  | 3 | 177 | 3.4 ms  | 6.3 ms  | — |
| current (`35dc224`) | 5 | 295 | **2.62 ms** | **5.70 ms** | 7.95 ms |

At `35dc224`, results took a median 2.39 ms after the keystroke, and the
wait to draw after that was a median 0.10 ms. The slowest full queries
were `monday ext:pdf` (median 4.76 ms, a filter that reads sizes and
dates) and `rport` (3.58 ms, a fuzzy match).

**Show to first frame** in the same runs (the bench opens the panel once
per round): median 8.59 ms, max 27.0 ms (n=5). The first show of a run is
the slowest, because GPUI builds its glyph atlas and text layouts then.
The hand-measured hotkey numbers above remain the reference for this.

### Memory

`footprint` (physical footprint, as Activity Monitor shows it). Single
samples unless stated. Scratch config and index directories were used.

| state | footprint | peak |
|-------|----------:|-----:|
| no file index, started hidden | 25 MB | — |
| no file index, after a UI bench | 34 MB | — |
| 100k-file index, started hidden | 29–30 MB (2 runs) | 40 MB |
| 100k-file index, after 1 bench round | 50–53 MB (2 runs) | 72–78 MB |
| 100k-file index, after 8 bench rounds | 47–55 MB (2 runs) | 70–78 MB |
| 100k-file index, after 5 rounds at `35dc224` | 50 MB | 78 MB |

- **No growth with use.** One round and eight rounds end at the same
  footprint, within run-to-run noise. The rise after the first show is a
  one-time cost: Metal drawables for the panel and IOSurfaces (about
  6 MB), plus GPUI's glyph atlas and text-layout caches (about 7 MB of
  malloc). `vmmap -summary` showed these before and after.
- **Earlier figures of 38 MB hidden** included a one-off rebuild of the
  name snapshot (the benchmark index had an older snapshot version). The
  bench script now refreshes the snapshot first.
- **Icon cache.** It was unbounded. Application icons are now kept, plus
  at most 256 others, with the least recently used dropped first. Dropped
  icons are also removed from GPUI's texture atlas. A bench round renders
  about 24 non-application icons, so the bound does not change these
  numbers. It caps a long session that sees many file types and bundles
  at about 4 MB of icon pixels.

Against the targets: the basic launcher idles under 50 MB (25–34 MB), and
with a 100k-file index it idles under 100 MB (47–55 MB). A larger index
adds its database pages and snapshot as mapped, mostly clean memory.

### Idle CPU and wake-ups

The panel was hidden after a 10 s settle. CPU time comes from `ps` over
30–60 s, and context switches from `top -stats csw` over 30 s.

| setting | CPU time | context switches/s |
|---------|---------:|-------------------:|
| clipboard history off | 0 s in 30 s (after a 5-round bench) | 0.7 |
| clipboard history on, before | — | 6.3 |
| clipboard history on, now | — | 2.3 |

The clipboard poll moved from a GPUI timer on the main thread to a
background-QoS thread. That thread reads the pasteboard change counter
and wakes the main thread only when it changed. `sample` of the idle
process showed every thread blocked. While the panel is open, GPUI's
display link runs at the screen's refresh rate (about 0.5% CPU). That is
inside GPUI and stops when the panel hides.

### Indexing throughput

Seven runs each, medians (min–max). Warm cache, 8 directory-reading
threads. The Phase 0 code (`189da10`) was built in a scratch worktree and
run in alternation with the current code (`35dc224`), two rounds.

| tree | Phase 0 scan | current scan | current total |
|------|-------------:|-------------:|--------------:|
| 10,001 entries  | 18.9–19.8 ms   | 21.1–21.7 ms   | 23.0–23.6 ms |
| 100,001 entries | 168.6–171.1 ms | 179.4–181.0 ms | 194.6–196.1 ms |

(The ranges are the two rounds' medians.) The current scan is about 6%
slower at 100k. One known cause is that it now checks each folder for a
`CACHEDIR.TAG` file (6,365 folders here); the difference was not broken
down further. The 158 ms in the Phase 0 table was a single sample; the
same code measures 169–171 ms today.

By thread count (100k, single runs): 1 thread 376 ms, 2 threads 264 ms,
4 threads 209 ms, 8 threads 195 ms. A `sample` profile of a 2-thread run
(1 ms interval, 100 samples on the writer):

- **Readers:** opening directories (65 samples), `fstatat` per entry
  (49), and `getdirentries` (14).
- **Writer:** waiting for batches (36 samples), and SQLite inserts
  including index maintenance and WAL writes (33).

Possible future work: read names and attributes together with
`getattrlistbulk` instead of `readdir` plus `fstatat`, and build the
secondary indexes after the bulk insert. Neither is needed for the
current targets.

Peak footprint while indexing 100k: 12.1–12.9 MiB (`/usr/bin/time -l`,
7 runs).

### Crash recovery and correctness

| check | result |
|-------|--------|
| `sift index` killed with SIGKILL at 10 points (5–150 ms) into a 20k-file scan | 8 left no root, 2 a complete root, 0 a partial one. All passed `quick_check` and `sift verify` (test `crash.rs`) |
| `PRAGMA quick_check` on the 100k index (run at startup after a crash) | 10.6 ms (single run) |
| `sift verify`, 10k index (check, invariants, compare with disk) | 4 + 17 + 62 ms |
| `sift verify`, 100k index | 18 + 104 + 329 ms |
| `sift verify --quick`, 1M synthetic index | 176 ms + 1.4 s |
| Live-update fuzz: random creates, renames, moves, edits and removals through FSEvents, then a comparison with a fresh scan | seeds 1–8, 2,000–3,000 operations each (18,000 total): all matched, about 270 ms after the last edit |

The fuzz test runs 400 operations with a fixed seed in `cargo test`.
`SIFT_FUZZ_SEED` and `SIFT_FUZZ_OPS` run longer or different sequences.

### Accessibility cost

Reading the accessibility display settings and VoiceOver state when the
panel opens takes 11–45 µs (8 shows, debug log, since removed). With
VoiceOver off, the launcher does nothing else for accessibility.

### Not measured

- Battery impact over hours. The idle figures above (0 s CPU, 0.7 wake-ups
  per second) are the proxy. `powermetrics` needs `sudo`, and Energy
  Impact in Activity Monitor needs a person to watch it.
- Announcements as heard with VoiceOver on. They were checked through
  `SIFT_ANNOUNCE=1` and the debug log (see README). VoiceOver itself
  needs a person.
- Cold-cache indexing (needs `sudo purge`).

## 2026-10-21 — UI redesign (launcher and settings)

Same machine. The previous UI (`e8e4b6b`) and the redesign (`1ef3ae9`)
were built in release mode and run **in alternation**, so background load
(WindowServer and other apps at about 40% CPU each during these runs)
affected both. Each typing run is `scripts/ui-bench.sh 5`: 5 rounds,
295 keystrokes, with the 100k-file index plus the installed
applications. The table below uses only runs that completed all 295
keystrokes.

### Before the fix: a regression the comparison caught

The first version of the new footer read the file count with
`FileIndex::len` on every frame. That takes the index lock, which a
background search holds for the whole search. So a frame drawn while a
search ran waited for it, and the results waited behind the frame. In
complete alternating pairs, the median was unchanged (+0.06 to
+0.10 ms), but **p95 keystroke-to-frame rose from 6.55 to 8.21–8.30 ms**
and run CPU time rose 2–3%. The footer now sums the per-folder counts
from the service status (no lock).

### After the fix (three alternating pairs)

| | previous UI | redesign |
|---|---:|---:|
| keystroke to frame, median (per run) | 2.97, 2.82, 3.16 ms | 2.99, 3.12, 3.12 ms |
| keystroke to frame, p95 (per run) | 6.32, 6.95, 7.20 ms | 6.21, 6.89, 6.37 ms |
| results to frame (drawing), p95 | 1.71–1.79 ms | 1.68–1.82 ms |
| show to first frame, median | 5.16–6.74 ms | 4.95–5.96 ms |
| CPU time, launch to end of run | 4.24–4.37 s | 4.25–4.45 s |
| footprint after the run | 30–32 MB | 30–33 MB |
| idle CPU after the run, 30 s hidden | 0 s | 0 s |

Hidden start, 10 s settle, then 30 s idle (three alternating pairs):

| | previous UI | redesign |
|---|---:|---:|
| footprint | 19, 19, 17 MB | 19, 19, 17 MB |
| context switches in 30 s | 31, 26, 35 | 26, 26, 25 |

**Result:**
- **Responsiveness:** no regression. The redesign's p95 was at or below
  the previous UI's in every pair, and its medians are within run-to-run
  noise (−0.04 to +0.30 ms per pair).
- **Memory:** identical after a hidden start, and within 1 MB after use
  (the budget was 5 MB). The three SVG symbols are rasterised once into
  GPUI's atlas.
- **Idle cost:** unchanged.

These absolute numbers are higher than the Phase 4 table (2.6 ms median
there). That is because of the background load during these runs; only
the alternating comparison is meaningful here.

**Benchmark fixes found on the way.**

- **Rescans during typing.** The bench's source index was built by the
  `sift` CLI, which records no file-system event position. So the
  launcher rescanned all 100k files at the start of every run, sometimes
  while typing. That made runs noisy and dropped keystrokes.
  `ui-bench.sh` now lets the launcher record a position in the source
  once.
- **Lost focus.** Another app taking focus hid the panel mid-run, which
  cut the measurement short. The bench now keeps the panel open.
- **Comparing builds.** `LAUNCHER=` runs a saved binary for comparisons
  like this one.

## 2026-10-22 — Sift compared with Raycast

**Machine:** the Apple M2 above, macOS 27.0.1. **Sift:** 0.1.0, release
build from `target/release/Sift.app`. **Raycast:** 2.8.2. Both apps were
already running with their usual settings, and both were measured at the
same time under the same background load.

### Memory

Physical footprint from `footprint <pid>`, the number Activity Monitor
shows in its "Memory" column. Raycast runs three processes; all three
are counted.

| process | Sift | Raycast |
|---------|-----:|--------:|
| main app | 54 MB (peak 55 MB) | 64 MB (peak 66 MB) |
| extension backend (`node`) | — | 168 MB (peak 200 MB) |
| clipboard helper (XPC) | — | 8 MB |
| **total** | **54 MB** | **240 MB** |
| app bundle on disk (`du -sh`) | 13 MB | 242 MB |

Sift's 54 MB is higher than the 26 MB idle figure in the Phase 1 table
because the panel had been used. `footprint` breaks it down as 25 MB of
heap (app icons, catalog, file index state), 13 MB of IOSurface and
8 MB of graphics memory. The last two are GPU frame buffers that stay
allocated after the panel has been shown once.

### Idle CPU

CPU time from `ps -o cputime`, with both apps idle and their panels
hidden.

| window | Sift | Raycast (all processes) |
|--------|-----:|------------------------:|
| 60 s sample | 0.03 s | 0.15 s |
| average since launch | ~1 s per hour | ~5 s per hour |

Both are negligible.

### File search

Raycast's file search is built on Spotlight, so `mdfind` stands in for
its search backend. Both commands searched `~/Documents` (Sift's index:
54,492 entries, 2.0 MiB snapshot). Each query ran 15 times; the clock
covers the whole process, start to exit, so both include process
start-up.

```bash
sift search --paths-only -n 20 <query>
mdfind -onlyin ~/Documents -name <query>
```

| query | Sift median | Sift max | Sift hits | `mdfind` median | `mdfind` max | `mdfind` hits |
|-------|-----:|-----:|----:|------:|------:|-----:|
| `report`  | 5.2 ms | 6.1 ms | 20 | 60.2 ms | 61.3 ms | 149 |
| `readme`  | 4.9 ms | 5.4 ms | 20 | 74.1 ms | 84.5 ms | 1,244 |
| `package` | 5.0 ms | 5.4 ms | 20 | 79.7 ms | 86.2 ms | 1,584 |
| `main`    | 4.9 ms | 5.4 ms | 20 | 64.9 ms | 65.9 ms | 413 |
| `prjrpt`  | 5.1 ms | 5.7 ms | 20 | 54.5 ms | 57.8 ms | 0 |

Sift was 11–16× faster. Inside the process, the search itself took about
1 ms (`sift search --stats report`: open 0.66 ms, search 1.12 ms,
resolve 0.34 ms).

### Caveats

- **Uneven uptime.** Raycast had been running for 37 hours and Sift for
  73 minutes. Raycast's backend may grow over time, so a fresh start
  could show less.
- **Different result sets.** `mdfind` returns every match, unranked;
  Sift returns the top 20, ranked. Spotlight has no approximate
  matching, so `prjrpt` found nothing.
- **Backend only.** The `mdfind` timings leave out Raycast's own UI and
  ranking on top of Spotlight. Raycast's application search keeps its
  catalog in memory and was not measured.
- **Not measured.** Hotkey-to-visible latency for Raycast: timing it the
  way `scripts/measure-hotkey.sh` times Sift needs Accessibility
  permission to send the hotkey, which the measuring shell did not have.
  Startup time for Raycast: measuring it would have meant quitting the
  running copy.
