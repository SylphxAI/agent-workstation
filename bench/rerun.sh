#!/usr/bin/env bash
# One command reruns the full measurement: detection runs, perf probe, report.
#
#   ./rerun.sh [--egress NAME[=PROXY]]... [--out DIR] [--seeds A,A,A,A,A,B,C,D]
#
# Defaults: single direct egress, engines fpc + camoufox (+ camoufox-linux
# variant), seeds A,A,A,A,A,B,C,D (5 stability repeats + 3 distinct),
# parallel 4. Pass --egress residential=PROXY to measure on a proxied egress,
# e.g. --egress residential=socks5://127.0.0.1:18265.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"
. "${WSBENCH_PREFIX:-${XDG_CACHE_HOME:-$HOME/.cache}/wsbench}/env.sh"

EGRESS_SPECS=(direct)
OUT=results
SEEDS="A,A,A,A,A,B,C,D"
ENGINES=(fpc camoufox camoufox-linux)
while [ "$#" -gt 0 ]; do
  case "$1" in
    --egress) EGRESS_SPECS+=("$2"); shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --seeds) SEEDS="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
# Drop the default direct entry if explicit egresses were given.
if [ "${#EGRESS_SPECS[@]}" -gt 1 ]; then
  EGRESS_SPECS=("${EGRESS_SPECS[@]:1}")
fi

EGRESS_ARGS=()
ENGINE_ARGS=()
for e in "${EGRESS_SPECS[@]}"; do EGRESS_ARGS+=(--egress "$e"); done
for en in "${ENGINES[@]}"; do ENGINE_ARGS+=(--engine "$en"); done

export WSBENCH_CHROME_NO_SANDBOX="${WSBENCH_CHROME_NO_SANDBOX:-1}"
python -m wsbench.run --out "$OUT" "${ENGINE_ARGS[@]}" "${EGRESS_ARGS[@]}" \
  --seeds "$SEEDS" --parallel 4
python -m wsbench.perf "${ENGINE_ARGS[@]}" "${EGRESS_ARGS[@]}" --seeds A,A,A
python -m wsbench.report "$OUT/runs.jsonl"
echo "perf raw: $OUT-perf/perf.jsonl"
