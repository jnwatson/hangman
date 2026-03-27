#!/usr/bin/env bash
set -euo pipefail

# Generate LMDB answer caches for all enable1.txt word lengths.
#
# Two strategies based on game-tree size:
#   k <= 3, k >= 8: full solve + warm_cache (all reachable positions get EXACT)
#   k = 4-7:        full solve only (game tree too large for warm_cache;
#                    server solves cache misses on-the-fly with disk L2)
#
# Runs one solve at a time. Monitors RSS and restarts if it exceeds the
# threshold — progress is preserved in the disk cache, so each restart
# picks up where it left off with a warm L2 cache.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DICT="$SCRIPT_DIR/enable1.txt"
CACHE_DIR="$SCRIPT_DIR/game_cache"
BENCH="$SCRIPT_DIR/target/release/bench"
LOCKFILE="/tmp/generate_cache.lock"
RSS_LIMIT_KB=$((38 * 1024 * 1024))  # 38 GB

# Prevent concurrent runs
if [ -e "$LOCKFILE" ]; then
    OTHER_PID=$(cat "$LOCKFILE" 2>/dev/null || echo "unknown")
    if kill -0 "$OTHER_PID" 2>/dev/null; then
        echo "ERROR: Another instance is running (PID $OTHER_PID). Exiting."
        exit 1
    else
        echo "WARNING: Stale lock file found (PID $OTHER_PID not running). Removing."
        rm -f "$LOCKFILE"
    fi
fi
echo $$ > "$LOCKFILE"
trap 'rm -f "$LOCKFILE"' EXIT

# Build release binary
echo "Building release binary..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3

if [ ! -x "$BENCH" ]; then
    echo "ERROR: bench binary not found at $BENCH"
    exit 1
fi

mkdir -p "$CACHE_DIR"

# warm_cache feasibility depends on word count (game tree grows combinatorially):
#   enable1.txt k=18 (594 words): 12s warm, 156K positions — OK
#   enable1.txt k=16 (5182 words): >60s, too large
# Threshold: warm_cache for k >= 18 (≤ ~1500 words). All other lengths
# rely on runtime on-the-fly solving with disk cache as L2.
needs_warm() {
    local k=$1
    [ "$k" -ge 18 ]
}

# All word lengths in enable1.txt, ordered fast-to-slow.
LENGTHS=(9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 27 28 8 7 6 2 5 3 4)

# Run a single length. Returns 0 on success, 1 on RSS-kill (retryable), 2 on error.
run_length() {
    local k=$1
    shift
    local extra_args=("$@")

    "$BENCH" --dict "$DICT" --lengths "$k" --max-words 100000 \
        --save-cache --load-cache --cache-dir "$CACHE_DIR" \
        ${extra_args[@]+"${extra_args[@]}"} &
    local pid=$!

    # Monitor RSS in background
    while kill -0 "$pid" 2>/dev/null; do
        sleep 10
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        local rss
        rss=$(awk '{print $2}' /proc/"$pid"/statm 2>/dev/null || echo 0)
        rss=$((rss * 4))  # pages to KB
        if [ "$rss" -gt "$RSS_LIMIT_KB" ]; then
            local rss_gb
            rss_gb=$(awk "BEGIN{printf \"%.1f\", $rss/1048576}")
            echo "  RSS ${rss_gb}G exceeds limit — killing PID $pid and restarting"
            kill "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null || true
            return 1
        fi
    done

    wait "$pid"
    return $?
}

TOTAL=${#LENGTHS[@]}
DONE=0
FAILED=()

for k in "${LENGTHS[@]}"; do
    DONE=$((DONE + 1))

    local_warm=""
    if needs_warm "$k"; then
        local_warm="--warm-cache"
        echo ""
        echo "===== [$DONE/$TOTAL] k=$k (solve + warm) ====="
    else
        echo ""
        echo "===== [$DONE/$TOTAL] k=$k (solve only) ====="
    fi

    # Delete old EXACT-only cache so the full solve regenerates with bounds.
    for d in "$CACHE_DIR"/tt_len"${k}"_*/; do
        if [ -d "$d" ]; then
            echo "  cleaning old cache: $d"
            rm -rf "$d"
        fi
    done

    MAX_ATTEMPTS=10
    attempt=0
    while true; do
        attempt=$((attempt + 1))
        if [ "$attempt" -gt "$MAX_ATTEMPTS" ]; then
            echo "k=$k FAILED after $MAX_ATTEMPTS attempts (RSS limit)"
            FAILED+=("$k")
            break
        fi

        if [ "$attempt" -gt 1 ]; then
            echo "  Retry #$attempt for k=$k (disk cache has accumulated entries)"
        fi

        START=$(date +%s)
        rc=0
        if [ -n "$local_warm" ]; then
            run_length "$k" "$local_warm" || rc=$?
        else
            run_length "$k" || rc=$?
        fi

        ELAPSED=$(( $(date +%s) - START ))
        if [ "$rc" -eq 0 ]; then
            echo "k=$k done in ${ELAPSED}s"
            break
        elif [ "$rc" -eq 1 ]; then
            echo "  k=$k killed at ${ELAPSED}s, will retry with warm cache"
            sleep 2
        else
            echo "k=$k FAILED (exit code $rc) after ${ELAPSED}s"
            FAILED+=("$k")
            break
        fi
    done
done

echo ""
echo "===== COMPLETE ====="
echo "Cache directory: $CACHE_DIR"
du -sh "$CACHE_DIR"/*/
if [ ${#FAILED[@]} -gt 0 ]; then
    echo ""
    echo "FAILED lengths: ${FAILED[*]}"
    exit 1
fi
