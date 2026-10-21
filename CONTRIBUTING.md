# Contributing to Sift

Thank you for helping. Bug reports, measurements from your own Mac and
small, focused pull requests are all welcome.

## Reporting a bug

Open an issue with:

- your macOS version and Mac model (Sift supports Apple Silicon on
  macOS 13 or later);
- the Sift version (search *Check for Updates*, or look at
  `Sift.app › Get Info`);
- what you did, what you expected and what happened;
- the relevant part of `~/Library/Logs/Sift/sift.log`. Look through it
  first: it contains file paths from your Mac. `SIFT_LOG=debug` gives more
  detail (run `/Applications/Sift.app/Contents/MacOS/Sift` from a
  terminal with it set).

Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Building

See *Building from source* in the [README](README.md). In short:

```bash
scripts/bundle-app.sh
open target/release/Sift.app
```

## Before you open a pull request

All three must pass (`scripts/release.sh` runs the same checks):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then try the change in the bundled app, not only in tests.

## How the code is written

[ARCHITECTURE.md](ARCHITECTURE.md) explains the design and why. Some
rules that are easy to break by accident:

- **Measure, don't guess.** A change that could affect latency, memory or
  idle CPU needs numbers before and after (`scripts/bench.sh`,
  `scripts/ui-bench.sh`, see [BENCHMARKS.md](BENCHMARKS.md)). Report what
  you measured, including results that did not improve.
- **Nothing on the frame path may wait.** A background search holds the
  file index lock for its whole run, so code that runs while drawing must
  not take it (not even `FileIndex::len`).
- **AppKit only on the main thread.** The helpers in `sift-macos` check
  for the main thread and do nothing elsewhere; keep it that way.
- **File names and metadata are untrusted input.** Never build shell
  commands from them, and do not assume they are valid UTF-8.
- **No new permissions** (Accessibility, Full Disk Access, …) unless a
  feature cannot work without one, and then only when the user turns
  that feature on.
- **No telemetry, and no new network requests.** The daily update check
  is the only one, and it can be turned off.
- **Never delete user data.** A damaged index or database is moved aside,
  not removed.
- `unwrap()` is a Clippy warning; use `expect("why this cannot fail")` in
  tests, and handle the error elsewhere.

## Commits

One logical change per commit, with a short imperative subject line
("Add Open at Login through SMAppService") and, when it helps, a body that
says why.

By contributing, you agree that your contribution is licensed under the
[MIT License](LICENSE).
