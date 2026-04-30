#!/bin/bash
# Scrub stuck-LOWER bounds in low-k caches (k=3 → k=2) on local box,
# pushing each to prod. Single-depth runs at minimax-misses depth — prod
# already has full-tree coverage at these depths, just contaminated with
# BOUND_LOWERs from old binaries.
#
# Run after clean_high_k.sh completes.

set -u
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
NTFY=hangman2-compute-nic
HOST=$(hostname)

# K depth pairs — depth = minimax-misses (matches deepest BOUNDs probed).
RUNS=(
  "3 17"
  "2 14"
)

curl -sd "clean_low_k START $HOST" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true

for run in "${RUNS[@]}"; do
  K=$(echo $run | cut -d' ' -f1)
  D=$(echo $run | cut -d' ' -f2)
  echo "==================================================================="
  echo "=== $(date) starting k=$K d=$D ==="
  echo "==================================================================="
  bash "$SCRIPT_DIR/chain.sh" $K $D $D
  rc=$?
  echo "==================================================================="
  echo "=== $(date) finished k=$K d=$D rc=$rc ==="
  echo "==================================================================="
  if [ $rc -ne 0 ]; then
    curl -sd "clean_low_k ABORT $HOST k=$K d=$D rc=$rc" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true
    exit $rc
  fi
done

curl -sd "clean_low_k FINISHED $HOST" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true
echo "=== $(date) all complete ==="
