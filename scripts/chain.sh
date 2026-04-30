#!/bin/bash
# chain.sh K MAX_D START_D — runs precompute for word length K at depths
# START_D..MAX_D, pushes cache to deadletters.fun between depths.
# Breaks on non-zero exit.
#
# After each depth's main run, runs the SAME precompute again as a
# verification pass: most positions are already EXACT and skip via
# is_cached_exact, so it's quick. If the verify pass solves any
# positions, that's evidence the main pass dropped some force-stores —
# the verify pass repairs them before push.
#
# Disk-space sanity check before rsync so we never fill prod.

set -u
K=$1
MAX_D=$2
START_D=$3

declare -A HASHES
HASHES[2]=07a0ba4703b5ae64
HASHES[3]=2b1858705b11a320
HASHES[4]=1c5a446e3c21477f
HASHES[5]=001681db3f9e04df
HASHES[6]=e5395d4ba5709f78
HASHES[7]=3bed095b96f6c1b9
HASHES[8]=0b6115514cee00b4
HASHES[9]=c3230bb401c0d42e
HASHES[10]=484da118e847b268
HASHES[11]=93f2e667c4b26199
HASHES[12]=120c84e50203ce6f
HASHES[13]=a7a882a8d4474446
HASHES[14]=a6f7dee94ccf66df
HASHES[15]=5691b3f2a75913e3

HASH=${HASHES[$K]}
CACHE_DIR=tt_len${K}_${HASH}
NTFY=hangman2-compute-nic
HOST=$(hostname)
PROD=hc4-prod
HOMEDIR=/home/nic/proj
SAFETY_BYTES=10737418240  # 10 GB headroom

cd $HOMEDIR/hangman2

run_precompute() {
  local label="$1"
  local logfile="$2"
  /usr/bin/time -v ./target/release/precompute \
    --dict enable1.txt \
    --lengths $K \
    --cache-dir game_cache \
    --depth $D \
    --max-words 999999 \
    --flush-every-n-positions 100 \
    > "$logfile" 2>&1
}

for D in $(seq $START_D $MAX_D); do
  curl -sd "chain START $HOST k=$K d=$D" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true

  # Main pass.
  LOG=$HOMEDIR/precompute_k${K}_d${D}.log
  [ -f "$LOG" ] && mv "$LOG" "${LOG}.$(date +%Y%m%d_%H%M%S)"
  run_precompute "main" "$LOG"
  RC=$?
  curl -sd "chain END $HOST k=$K d=$D rc=$RC" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
  [ $RC -ne 0 ] && break

  # Verify pass: same args, second invocation. is_cached_exact skips EXACT
  # entries; non-EXACT (or missing) entries get re-solved. Catches dropped
  # force-stores from the main pass before they reach prod.
  VLOG=$HOMEDIR/precompute_k${K}_d${D}.verify.log
  [ -f "$VLOG" ] && mv "$VLOG" "${VLOG}.$(date +%Y%m%d_%H%M%S)"
  run_precompute "verify" "$VLOG"
  VRC=$?
  VSOLVED=$(grep -E "^  Done:" "$VLOG" | grep -oE "[0-9]+ solved" | grep -oE "[0-9]+" || echo "?")
  curl -sd "verify END $HOST k=$K d=$D rc=$VRC solved=$VSOLVED" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
  [ $VRC -ne 0 ] && break

  # Push cache to prod. Prefer mdb_copy -c (compacts free pages); fall back
  # to cp --sparse=auto if mdb_copy isn't installed. Safe because main+verify
  # have exited before this step — no concurrent writers.
  mkdir -p /tmp/snap_$CACHE_DIR
  if command -v mdb_copy >/dev/null 2>&1; then
    mdb_copy -c game_cache/$CACHE_DIR /tmp/snap_$CACHE_DIR/ 2>&1 | tail -3
  else
    cp --sparse=auto game_cache/$CACHE_DIR/data.mdb /tmp/snap_$CACHE_DIR/data.mdb
  fi

  # Disk-space sanity check.
  SNAP_BYTES=$(stat -c %s /tmp/snap_$CACHE_DIR/data.mdb 2>/dev/null || echo 0)
  SNAP_GB=$(( (SNAP_BYTES + 1073741823) / 1073741824 ))
  if [ "$SNAP_BYTES" -eq 0 ]; then
    curl -sd "ABORT push $HOST k=$K d=$D: snapshot empty/missing" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
    rm -rf /tmp/snap_$CACHE_DIR
    break
  fi
  PROD_AVAIL_BYTES=$(ssh -o ConnectTimeout=20 $PROD "df --output=avail -B1 /opt/dead-letters | tail -1" 2>/dev/null || echo 0)
  PROD_AVAIL_GB=$(( PROD_AVAIL_BYTES / 1073741824 ))
  if [ "$PROD_AVAIL_BYTES" -gt 0 ] && [ "$SNAP_BYTES" -gt $((PROD_AVAIL_BYTES - SAFETY_BYTES)) ]; then
    MSG="ABORT push $HOST k=$K d=$D snap=${SNAP_GB}GB > prod free=${PROD_AVAIL_GB}GB - 10GB. Snapshot kept at /tmp/snap_$CACHE_DIR for manual push."
    echo "$MSG"
    curl -sd "$MSG" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
    break
  fi

  # Pre-push spot check: confirm the pessimal probe path's depth-5 partitions
  # are all EXACT. If any are non-EXACT, refuse to push — the cache hasn't
  # converged. (k=6 only; cheap and high-signal.)
  if [ "$K" = "6" ] && [ -x ./target/release/cache_diag ]; then
    BOUND_COUNT=$(./target/release/cache_diag --dict enable1.txt --length 6 --cache-dir game_cache --path qjxzv 2>/dev/null \
      | grep -E "BOUND-only" | grep -oE "[0-9]+ BOUND-only" | grep -oE "[0-9]+" | awk '{s+=$1} END {print s+0}')
    if [ "$BOUND_COUNT" -gt 0 ]; then
      MSG="ABORT push $HOST k=$K d=$D — spot check found $BOUND_COUNT non-EXACT entries on probe path."
      echo "$MSG"
      curl -sd "$MSG" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
      rm -rf /tmp/snap_$CACHE_DIR
      break
    fi
  fi

  ssh $PROD "mkdir -p /opt/dead-letters/cache_stage/$CACHE_DIR"
  PUSH_RC=0
  rsync -az /tmp/snap_$CACHE_DIR/data.mdb $PROD:/opt/dead-letters/cache_stage/$CACHE_DIR/ || PUSH_RC=$?
  # IMPORTANT: also wipe lock.mdb. saferlmdb keys reader-table state to a
  # specific data.mdb epoch; replacing data.mdb without invalidating lock.mdb
  # leaves new readers seeing a stale empty index (issue #70 — silent push
  # failures that lost weeks of compute coverage on prod).
  ssh $PROD "sudo mv /opt/dead-letters/cache_stage/$CACHE_DIR/data.mdb /opt/dead-letters/cache/$CACHE_DIR/data.mdb && sudo chown www:www /opt/dead-letters/cache/$CACHE_DIR/data.mdb && sudo rm -f /opt/dead-letters/cache/$CACHE_DIR/lock.mdb && sudo rmdir /opt/dead-letters/cache_stage/$CACHE_DIR" || PUSH_RC=$?
  # Restart prod service so it sees the new data.mdb (mmap of replaced inode
  # otherwise sticks to the old file).
  ssh $PROD "sudo systemctl restart dead-letters" || PUSH_RC=$?
  rm -rf /tmp/snap_$CACHE_DIR
  if [ $PUSH_RC -ne 0 ]; then
    curl -sd "chain push FAILED $HOST k=$K d=$D rc=$PUSH_RC" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
    break
  fi
  curl -sd "chain pushed $HOST k=$K d=$D snap=${SNAP_GB}GB verify_solved=$VSOLVED" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
done
curl -sd "chain FINISHED $HOST k=$K (covered up to d=$MAX_D)" "https://ntfy.sh/$NTFY" > /dev/null 2>&1 || true
