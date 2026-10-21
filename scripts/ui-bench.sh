#!/usr/bin/env bash
# UI responsiveness and memory benchmark for the launcher.
#
#   scripts/ui-bench.sh [ROUNDS]
#   LAUNCHER=/path/to/sift-launcher scripts/ui-bench.sh [ROUNDS]
#
# LAUNCHER runs a prebuilt launcher instead of building this checkout, to
# compare two builds (for example a copy of the previous release build).
#
# Runs the release launcher with SIFT_UI_BENCH (see
# crates/sift-launcher/src/uibench.rs): it shows the panel and types
# queries one character at a time through the real search path, against a
# copy of the 100k-file benchmark index from scripts/bench.sh. The panel
# is visible on screen while it runs. Afterwards the launcher stays
# running hidden, and this script records its memory and idle CPU.
#
# Uses its own config, index and log directories, so a running Sift and its
# data are not touched (the second instance cannot register Option+Space;
# that error in the log is expected).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS="${1:-5}"
BENCH="${TMPDIR:-/tmp}/sift-bench"
SOURCE="$BENCH/idx-files-100000"
WORK="$BENCH/ui-bench"
if [[ ! -d "$SOURCE" ]]; then
    echo "missing $SOURCE: run scripts/bench.sh first" >&2
    exit 1
fi
if [[ -n "${LAUNCHER:-}" ]]; then
    cargo build --release --quiet -p sift-cli --manifest-path "$ROOT/Cargo.toml"
else
    cargo build --release --quiet -p sift-launcher -p sift-cli --manifest-path "$ROOT/Cargo.toml"
    LAUNCHER="$ROOT/target/release/sift-launcher"
fi
# Bring the source's name snapshot up to date (a no-op when it is), so the
# launcher starts the way it does for a user, without a one-off rebuild.
"$ROOT/target/release/sift" --index-dir "$SOURCE" search sift-ui-bench-warmup >/dev/null 2>&1
rm -rf "$WORK"
mkdir -p "$WORK/config"
cp -R "$SOURCE" "$WORK/index"
LOG="$WORK/launcher.log"

SIFT_UI_BENCH="$ROUNDS" SIFT_START_HIDDEN=1 SIFT_INDEX_DIR="$WORK/index" \
    SIFT_CONFIG_DIR="$WORK/config" SIFT_LOG_DIR="$WORK/logs" \
    "$LAUNCHER" >"$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

for _ in $(seq 1 600); do
    grep -q "ui bench done" "$LOG" && break
    kill -0 "$PID" 2>/dev/null || { echo "launcher exited early:" >&2; cat "$LOG" >&2; exit 1; }
    sleep 0.5
done
sed -n '/== ui bench/,/ui bench done/p' "$LOG"

cputime() { ps -o time= -p "$1" | awk -F'[:.]' '{ if (NF==3) print $1*60+$2+$3/100; else print $1*3600+$2*60+$3+$4/100 }'; }
sleep 5
echo "== memory after the run (panel hidden)"
footprint "$PID" 2>/dev/null | grep -E "Footprint|phys_footprint" | head -3 || true
ps -o rss= -p "$PID" | awk '{ printf "   resident: %.1f MB\n", $1/1024 }'
echo "== idle CPU, panel hidden, 30 s"
a=$(cputime "$PID"); sleep 30; b=$(cputime "$PID")
echo "   cpu time: $(echo "$b - $a" | bc) s"
