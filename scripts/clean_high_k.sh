#!/bin/bash
# Scrub stuck-LOWER bounds in high-k caches (k=15 → k=12) on local box,
# pushing each to prod. Single-depth runs at prod's current coverage —
# no extension. Run inside tmux session "pc".

set -u
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
NTFY=hangman2-compute-nic
HOST=$(hostname)

# K depth pairs — depth = deepest layer with entries already on prod.
RUNS=(
  "15 4"
  "14 5"
  "13 5"
  "12 5"
)

curl -sd "clean_high_k START $HOST" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true

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
    curl -sd "clean_high_k ABORT $HOST k=$K d=$D rc=$rc" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true
    exit $rc
  fi
done

curl -sd "clean_high_k FINISHED $HOST" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true
echo "=== $(date) all complete ==="
