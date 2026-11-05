#!/usr/bin/env bash
# two-host — campaign runner.
#
# Self-contained: this script resolves the workspace from its own location, so the experiment
# runs wherever it is unpacked. Provisioning is `vxedge experiment up`; what stays here is the
# method — how many repetitions, how long to warm up, how long to measure.
#
#   sudo -E env "PATH=$PATH" ./scripts/run.sh [--quick]
set -euo pipefail

WS="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VXEDGE="${VXEDGE:-vxedge}"

QUICK=0
for arg in "$@"; do case "$arg" in
  --quick) QUICK=1 ;;
  *) echo "error: unknown argument '$arg' (supported: --quick)" >&2; exit 1 ;;
esac; done

# --- method -----------------------------------------------------------------
# --quick is pipeline validation only. Its sample counts are below tail-percentile
# support, so its numbers must never be reported as results.
if [[ $QUICK -eq 1 ]]; then
  REPS=1; WARMUP_S=3; WINDOW_S=12
else
  REPS=5; WARMUP_S=10; WINDOW_S=60
fi

if [[ $(id -u) -ne 0 ]]; then
  echo "error: provisioning needs CAP_NET_ADMIN — sudo -E env \"PATH=\$PATH\" $0" >&2
  exit 1
fi
command -v "$VXEDGE" >/dev/null || { echo "error: vxedge not on PATH (set VXEDGE=/path/to/vxedge)" >&2; exit 1; }

RESULTS="$WS/results/$( [[ $QUICK -eq 1 ]] && echo quick || date +%Y-%m-%d )"
mkdir -p "$RESULTS"

cleanup() { "$VXEDGE" --chdir "$WS" experiment down >/dev/null 2>&1 || true; }
trap cleanup EXIT

for rep in $(seq 1 "$REPS"); do
  echo "== rep$rep: experiment up"
  "$VXEDGE" --chdir "$WS" experiment up

  echo "   warmup ${WARMUP_S}s"
  sleep "$WARMUP_S"

  echo "   measuring ${WINDOW_S}s"
  "$VXEDGE" --chdir "$WS" traffic up
  sleep "$WINDOW_S"
  "$VXEDGE" --chdir "$WS" traffic down

  # Collection: point vxtelemetry at this workspace; the archive carries the workspace itself,
  # so a result is re-runnable from what is inside it.
  if command -v vxtelemetry >/dev/null; then
    vxtelemetry collect --workspace "$WS" --ingest-dir "$WS/results/.ingest" \
      --out "$RESULTS/rep$rep.zip"
  fi

  "$VXEDGE" --chdir "$WS" experiment down
done

echo "== done: $RESULTS"
