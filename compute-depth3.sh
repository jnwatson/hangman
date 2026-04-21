#!/usr/bin/env bash
set -euo pipefail

# Depth-3 precompute for word lengths 4,6,7,8,9,10,11 (skip 5).
# Runs one length at a time, rsyncs cache from/to the game server.
# Reports progress at 25% intervals via ntfy.

NTFY_TOPIC="hangman2-compute-nic"
GAME_SERVER="nic@64.225.9.155"
CACHE_BASE="/mnt/volume_nyc3_01/cache"
LOCAL_CACHE="/root/hangman2/game_cache"
DICT="/root/hangman2/enable1.txt"
PRECOMPUTE="/root/hangman2/target/release/precompute"
LOG="/tmp/precompute.log"

# Word length -> cache directory name (hash depends on dictionary + length)
declare -A CACHE_DIRS
CACHE_DIRS[4]="tt_len4_1c5a446e3c21477f"
CACHE_DIRS[6]="tt_len6_e5395d4ba5709f78"
CACHE_DIRS[7]="tt_len7_3bed095b96f6c1b9"
CACHE_DIRS[8]="tt_len8_0b6115514cee00b4"
CACHE_DIRS[9]="tt_len9_c3230bb401c0d42e"
CACHE_DIRS[10]="tt_len10_484da118e847b268"
CACHE_DIRS[11]="tt_len11_93f2e667c4b26199"

LENGTHS=(4 6 7 8 9 10 11)

notify() {
    curl -sf -d "$1" "ntfy.sh/$NTFY_TOPIC" > /dev/null 2>&1 || true
}

notify "hc4 depth-3 precompute starting: lengths ${LENGTHS[*]}"

for k in "${LENGTHS[@]}"; do
    cache_dir="${CACHE_DIRS[$k]}"
    local_path="$LOCAL_CACHE/$cache_dir"

    notify "hc4: starting k=$k — pulling cache from game server"

    # Pull cache from game server
    mkdir -p "$local_path"
    rsync -az "$GAME_SERVER:$CACHE_BASE/$cache_dir/" "$local_path/"

    # Run depth-3 precompute, monitoring progress
    echo "=== k=$k: starting depth-3 precompute ===" >> "$LOG"
    last_threshold=0
    $PRECOMPUTE --dict "$DICT" --lengths "$k" --cache-dir "$LOCAL_CACHE" --depth 3 --max-words 999999 2>&1 | \
    while IFS= read -r line; do
        echo "$line" >> "$LOG"
        # Extract percentage and notify when crossing 25% thresholds
        if [[ "$line" =~ \[[[:space:]]*([0-9]+)\.[0-9]+%\] ]]; then
            pct="${BASH_REMATCH[1]}"
            for threshold in 25 50 75; do
                if [[ "$pct" -ge "$threshold" ]]; then
                    marker="/tmp/notify_k${k}_${threshold}"
                    if [ ! -f "$marker" ]; then
                        touch "$marker"
                        notify "hc4: k=$k depth-3 precompute ${threshold}% (at ${pct}%)"
                    fi
                fi
            done
        fi
    done

    notify "hc4: k=$k depth-3 precompute done — pushing cache to game server"

    # Push cache back to game server (stage in /tmp, then sudo copy)
    ssh "$GAME_SERVER" "mkdir -p /tmp/cache_stage/$cache_dir"
    rsync -az "$local_path/data.mdb" "$GAME_SERVER:/tmp/cache_stage/$cache_dir/"
    ssh "$GAME_SERVER" "sudo cp /tmp/cache_stage/$cache_dir/data.mdb $CACHE_BASE/$cache_dir/data.mdb && sudo chown www:www $CACHE_BASE/$cache_dir/data.mdb && rm -rf /tmp/cache_stage/$cache_dir"

    # Clean up local cache and markers
    rm -rf "$local_path"
    rm -f /tmp/notify_k${k}_*

    notify "hc4: k=$k complete and deployed"
    echo "=== k=$k: done ===" >> "$LOG"
done

notify "hc4: ALL depth-3 precomputes COMPLETE (lengths ${LENGTHS[*]})"
echo "=== ALL DONE ===" >> "$LOG"
