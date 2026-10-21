# sift

A Rust-native macOS launcher and file search engine: Raycast's keyboard-first
launcher combined with Everything-style instant filename search, built for low
memory use and near-zero idle CPU.

**Status:** Phase 0 — a command-line file indexer and search engine. There is
no GUI yet. See [PLAN.md](PLAN.md) for the roadmap.

## Requirements

- macOS (Apple Silicon is the primary target)
- Rust 1.85 or newer (`rustup` stable)
- Xcode Command Line Tools (for the bundled SQLite C build)

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
`--descend-packages`, `--cross-devices`, `--include-sensitive`. `~/Library`
and `~/.Trash` are skipped by default, and indexing `/` or your whole home
folder requires `--allow-broad-root`.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/bench.sh            # reproducible benchmarks, see BENCHMARKS.md
```

Design notes are in [ARCHITECTURE.md](ARCHITECTURE.md), the roadmap is in
[PLAN.md](PLAN.md), and measurements are in [BENCHMARKS.md](BENCHMARKS.md).
