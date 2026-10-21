# sift

A Rust-native macOS launcher and file search engine: Raycast's keyboard-first
launcher combined with Everything-style instant filename search, built for low
memory use and near-zero idle CPU.

**Status:** Phase 2. Option+Space opens a launcher that searches
applications, files and folders. The file index updates itself as files
change (FSEvents). There is also a command-line indexer and search tool.
See [PLAN.md](PLAN.md) for the roadmap.

## Requirements

- macOS (Apple Silicon is the primary target)
- Rust 1.85 or newer (`rustup` stable)
- Xcode Command Line Tools (for the bundled SQLite C build). Full Xcode
  is not needed: the launcher compiles its Metal shaders at runtime.

## Usage

```bash
# Index one or more folders (the index lives in
# ~/Library/Application Support/sift/dev-index unless --index-dir is given)
cargo run --release -- index ~/Documents

# Search
cargo run --release -- search project
cargo run --release -- search "annual report" --limit 10
cargo run --release -- search report --ext pdf --files
cargo run --release -- search prjrs --stats      # fuzzy, with timing

# Inspect or drop roots
cargo run --release -- status
cargo run --release -- remove ~/Documents

# Check the index: SQLite integrity, internal consistency, and a fresh
# scan compared with what is indexed (--quick skips the scan)
cargo run --release -- verify
```

Useful index options: `--exclude <path>`, `--exclude-name node_modules`,
`--descend-packages`, `--cross-devices`, `--include-sensitive`, `--include-dependencies`. Dependency and cache folders (`node_modules`, `.git`, `.venv`, …) are skipped by default, and the options are remembered per root: `sift index` with no paths re-indexes every root the same way. `~/Library`
and `~/.Trash` are skipped by default, and indexing `/` or your whole home
folder requires `--allow-broad-root`.

## Trying the launcher

```bash
scripts/bundle-app.sh            # builds target/release/Sift.app
open target/release/Sift.app
```

The panel appears once at launch. After that, **Option+Space** shows or
hides it.

- Type to search apps, files and folders (files need two characters).
  With nothing typed, the panel lists your favorites and recent items.
- **↑/↓** (or Ctrl-P/Ctrl-N) to move.
- **Enter** to open (or copy, for calculator results, snippets and
  clipboard entries).
- **⌘Enter** to show the selection in Finder.
- **⌘Y** to preview the selected file with Quick Look (arrows keep
  working; Esc closes the preview).
- **⌘D** to add or remove a favorite (★). **Ctrl-X** removes an item
  from the recent list.
- **Esc** or clicking elsewhere to close.
- **Cmd-Q** while the panel is open quits Sift.

**Calculator:** type an expression such as `12.5 * 4`, `2^10 - 1` or
`sqrt(2)`. The value is the top result, and Enter copies it.

**File filters** narrow file results:

| filter | example |
|--------|---------|
| extension | `report ext:pdf`, `ext:jpg,png` |
| type | `type:image`, `type:video`, `type:audio`, `type:document`, `type:code`, `type:archive`, `type:folder`, `type:file` |
| folder | `notes in:projects`, `in:~/Documents` |
| size | `size:>10mb`, `size:<500k` |
| date | `modified:<7d`, `modified:>1y` (`min`, `h`, `d`, `w`, `mo`, `y`) |

**Snippets and custom commands:** search for *Edit Sift Configuration*.
It creates `~/Library/Application Support/sift/config.toml` with
commented examples. Snippets are found by name or keyword, and Enter
copies their text (`{date}`, `{time}` and `{datetime}` are filled in).
Custom commands open a URL, open a file or app, or run a shell command.
Changes apply the next time the panel opens.

**Clipboard history** is off by default. Set `enabled = true` under
`[clipboard]` in the config, then type `clip` (or `clip invoice`) to
search what you copied. Text that password managers mark as concealed is
never kept. *Clear Clipboard History* deletes everything kept.

Sift has no Dock icon and needs no Accessibility permission.

**VoiceOver:** while VoiceOver runs, Sift speaks the highlighted result
when it moves ("report.pdf, file, in Documents, 2 of 8"). It also speaks
the number of results and the top one when typing pauses, and any message
shown. **Increase Contrast** and **Differentiate Without Color** (System
Settings › Accessibility › Display) give stronger colours and an outlined
selection. Both are picked up the next time the panel opens.

**If something goes wrong:** Sift keeps a log at
`~/Library/Logs/Sift/sift.log`, including crash reports. Nothing leaves
your Mac. If Sift did not quit normally, it checks the file index at the
next start. A damaged index is moved aside (into a `damaged-…` folder
next to it) and rebuilt from the saved folder list. A damaged `state.db`
is renamed to `state.db.damaged-…`, and recents and favorites start
again.

**Choosing folders:** search for *Sift Settings* (or "preferences") and
press Enter. Add the folders you want searchable. macOS asks once for
access to Documents, Desktop or Downloads. Folders indexed with
`sift index` in a terminal show up there too; they share one index.

Environment options: `SIFT_INDEX_DIR` (index location),
`SIFT_CONFIG_DIR` (where `config.toml` and `state.db` live),
`SIFT_START_HIDDEN=1` (don't show the panel at launch), `SIFT_LOG=debug`,
`SIFT_LOG_DIR` (log location), and `SIFT_ANNOUNCE=1` (make the VoiceOver
announcements, logged at debug level, even when VoiceOver is off).

To see timing logs (startup, catalog load, hotkey-to-first-frame), run the
executable from a terminal instead:

```bash
target/release/Sift.app/Contents/MacOS/Sift
```

`scripts/bundle-app.sh --install` also copies the app to `~/Applications`.
The first launch after a build takes about a second, while GPUI compiles
its shaders.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/bench.sh            # reproducible benchmarks, see BENCHMARKS.md
scripts/ui-bench.sh 5       # types through the launcher, then memory and idle CPU
cargo run --release -p sift-apps --example bench_apps   # app search timing
cargo run --release -p sift-files --example live_latency # change-to-searchable timing

# Longer live-update fuzzing (the test suite runs 400 operations)
SIFT_FUZZ_SEED=7 SIFT_FUZZ_OPS=3000 cargo test --release -p sift-files --test service random_edits
```

`scripts/ui-bench.sh` needs the 100k-file tree from `scripts/bench.sh`. It
opens the panel on screen while it types, using its own config, index and
log directories.

Design notes are in [ARCHITECTURE.md](ARCHITECTURE.md), the roadmap is in
[PLAN.md](PLAN.md), and measurements are in [BENCHMARKS.md](BENCHMARKS.md).
