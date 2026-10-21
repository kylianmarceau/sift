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
