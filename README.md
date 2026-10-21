# sift

A Rust-native macOS launcher and file search engine: Raycast's keyboard-first
launcher combined with Everything-style instant filename search, built for low
memory use and near-zero idle CPU.

**Status:** Phase 1. There is a command-line file indexer and search
engine, and a minimal launcher that finds and opens applications with
Option+Space. File search is not in the launcher yet. See
[PLAN.md](PLAN.md) for the roadmap.

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
hides it. Type to search apps, **↑/↓** (or Ctrl-P/Ctrl-N) to move,
**Enter** to open, **Esc** or clicking elsewhere to close. **Cmd-Q** while
the panel is open quits Sift. It has no Dock icon. No Accessibility
permission is needed.

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
cargo run --release -p sift-apps --example bench_apps   # app search timing
```

Design notes are in [ARCHITECTURE.md](ARCHITECTURE.md), the roadmap is in
[PLAN.md](PLAN.md), and measurements are in [BENCHMARKS.md](BENCHMARKS.md).
