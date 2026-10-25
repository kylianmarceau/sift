#!/usr/bin/env bash
# Launch via Launch Services: a direct shell invocation inherits the shell's
# permission attribution rather than the app's own user-approved identity.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CHECK_APP="$ROOT/target/debug/Sift WM Check.app"
CHECK_LOGS="$(mktemp -d "${TMPDIR:-/tmp}/sift-wm-native.XXXXXXXX")"
echo "Native check logs: $CHECK_LOGS"
/usr/bin/open -n -W -a "$CHECK_APP" \
    --env "SIFT_WM_DEBUG=${SIFT_WM_DEBUG:-}" \
    --env "SIFT_WM_LAUNCHER=${SIFT_WM_LAUNCHER:-}" \
    --env "SIFT_WM_INDEX=${SIFT_WM_INDEX:-}" \
    --env "SIFT_WM_BENCH_OUTPUT=${SIFT_WM_BENCH_OUTPUT:-}" \
    --env "SIFT_WM_BENCH_AUTOMATIC_TILING=${SIFT_WM_BENCH_AUTOMATIC_TILING:-true}" \
    --stdout "$CHECK_LOGS/stdout.log" --stderr "$CHECK_LOGS/stderr.log"
cat "$CHECK_LOGS/stdout.log" "$CHECK_LOGS/stderr.log"
# open reports launch success, not the helper's exit status. Require the test's
# final success marker so a panic/permission denial cannot count as a pass.
grep -Fxq 'Native window-manager checks passed.' "$CHECK_LOGS/stdout.log"
