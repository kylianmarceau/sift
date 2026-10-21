#!/usr/bin/env bash
# Reproducible Phase 0 benchmarks.
#
#   scripts/bench.sh [WORKDIR]
#
# WORKDIR defaults to $TMPDIR/sift-bench (outside ~/Documents, which may be
# synced to iCloud). Everything the script creates lives under WORKDIR and
# is removed and regenerated on each run.
#
# Environment:
#   FILE_COUNTS   sizes of on-disk trees to create and index  (default "10000 100000")
#   SYNTH_COUNTS  sizes of store-only synthetic indexes       (default "10000 100000 1000000")
#   ITERATIONS    timed runs per query                        (default 50)
#   SEED          dataset seed                                (default 42)
#
# On-disk trees measure the real scan path (directory listing, lstat,
# SQLite writes). Store-only datasets skip the filesystem so search can be
# measured at sizes where creating real files would be slow and disruptive.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${1:-${TMPDIR:-/tmp}/sift-bench}"
FILE_COUNTS="${FILE_COUNTS:-10000 100000}"
SYNTH_COUNTS="${SYNTH_COUNTS:-10000 100000 1000000}"
ITERATIONS="${ITERATIONS:-50}"
SEED="${SEED:-42}"
BIN="$ROOT/target/release/sift"

mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd -P)"

echo "== environment"
sw_vers | sed 's/^/   /'
echo "   $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) / 1073741824 )) GiB"
echo "   $(rustc --version)"
echo "   commit $(git -C "$ROOT" rev-parse --short HEAD)"
echo "   workdir $WORK"

cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml"

# Runs a command under /usr/bin/time -l and prints its output plus the
# process's peak RSS and peak memory footprint. With --quiet as the first
# argument, the command's stdout is discarded.
measure() {
    local log out=/dev/stdout
    if [[ "$1" == --quiet ]]; then out=/dev/null; shift; fi
    log="$(mktemp)"
    /usr/bin/time -l "$@" 2>"$log" >"$out" || { cat "$log"; rm -f "$log"; return 1; }
    grep -vE '^ +[0-9]+ +[a-z]' "$log" | grep -v 'real' || true
    awk '/maximum resident set size/ {printf "   peak RSS (time -l): %.1f MiB\n", $1/1048576}
         /peak memory footprint/   {printf "   peak footprint (time -l): %.1f MiB\n", $1/1048576}
         / real /                  {printf "   wall: %s s\n", $1}' "$log"
    rm -f "$log"
}

for n in $FILE_COUNTS; do
    tree="$WORK/files-$n"
    idx="$WORK/idx-files-$n"
    rm -rf "$tree" "$idx"
    echo
    echo "== on-disk tree: $n entries"
    "$BIN" bench files "$tree" --count "$n" --seed "$SEED"
    echo "-- first index (page cache warm from creation)"
    measure "$BIN" --index-dir "$idx" index "$tree"
    echo "-- re-index (replaces existing root)"
    measure "$BIN" --index-dir "$idx" index "$tree"
done

for n in $SYNTH_COUNTS; do
    idx="$WORK/idx-synth-$n"
    rm -rf "$idx"
    echo
    echo "== store-only synthetic index: $n entries"
    measure "$BIN" --index-dir "$idx" bench synth --count "$n" --seed "$SEED"
    echo "-- in-process query timing"
    "$BIN" --index-dir "$idx" bench search --iterations "$ITERATIONS"
    echo "-- one-shot CLI search (new process: open + search + resolve + print)"
    measure --quiet "$BIN" --index-dir "$idx" search report --stats
done
