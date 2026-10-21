#!/usr/bin/env bash
# Measures Option+Space → first painted frame with the real hotkey.
#
#   scripts/measure-hotkey.sh [samples]     # default 20 (quits a running Sift)
#
# Starts target/release/Sift.app with logging, then you press Option+Space
# repeatedly (each press alternately shows and hides the panel). Every show
# logs "hotkey to first frame"; the script prints min / median / p90 / max
# once enough samples arrive. Needs an unlocked screen: macOS draws no
# frames while the display sleeps.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/Sift.app/Contents/MacOS/Sift"
SAMPLES="${1:-20}"
LOG="$(mktemp -t sift-hotkey)"

# Only one process can own Option+Space, so stop a running copy first.
if pgrep -x Sift >/dev/null || pgrep -x sift-launcher >/dev/null; then
    echo "Quitting the running Sift so this measurement can register Option+Space."
    pkill -x Sift || true
    pkill -x sift-launcher || true
    sleep 0.5
fi
[[ -x "$BIN" ]] || "$ROOT/scripts/bundle-app.sh"

SIFT_LOG=info "$BIN" 2>"$LOG" &
PID=$!
disown "$PID"  # no "Terminated" job message when it is stopped at the end
trap 'kill $PID 2>/dev/null; rm -f "$LOG"' EXIT
sleep 1

echo "Press Option+Space $((SAMPLES * 2)) times (show, hide, show, …), about once a second."
count() { grep -c "hotkey to first frame" "$LOG" || true; }
while (( $(count) < SAMPLES )); do
    sleep 0.5
    printf '\r%d / %d samples' "$(count)" "$SAMPLES"
done
echo

sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep "hotkey to first frame" \
    | sed 's/.*latency_ms=\([0-9.]*\).*/\1/' | sort -n \
    | awk '{ v[NR] = $1 } END {
        printf "samples %d  min %.2f ms  median %.2f ms  p90 %.2f ms  max %.2f ms\n",
            NR, v[1], v[int((NR + 1) / 2)], v[int(NR * 0.9 + 0.5)], v[NR] }'
