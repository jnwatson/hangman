#!/bin/bash
# health_probe.sh — for each word length, simulate a real game on the prod
# cache (optimal guesser + adversarial referee = the worst path the user
# could hit playing optimally) and report cache health.
#
# Reports per length:
#   - turns played to game-end
#   - total non-EXACT partitions encountered (each = a degraded turn for the user)
#   - max misses in a single turn
#   - max per-turn wall time (proxy for live-solve cost)
#
# Trace_sim treats only EXACT entries as cache hits — BOUND_LOWER and
# BOUND_UPPER count as misses, matching the server's serving logic. So a
# clean "✓" here means the user won't see degraded on this game path.
#
# Usage: scripts/health_probe.sh [--remote] [-k LENGTHS]
#   --remote        run on hc4 against prod cache (default: local)
#   -k 6,7,8        only probe these lengths

set -u

REMOTE=false
KS="2 3 4 5 6 7 8 9 10 11 12 13 14 15"
STRATEGY=optimal

while [ $# -gt 0 ]; do
    case "$1" in
        --remote) REMOTE=true; shift ;;
        -k) KS=$(echo "$2" | tr ',' ' '); shift 2 ;;
        --strategy) STRATEGY="$2"; shift 2 ;;
        *) echo "unknown arg: $1"; exit 1 ;;
    esac
done

if [ "$REMOTE" = true ]; then
    BIN=/root/hangman2/target/release/trace-sim
    DICT=/root/hangman2/enable1.txt
    CACHE=/opt/dead-letters/cache
    PREFIX="ssh -o BatchMode=yes hc4-prod"
else
    BIN=./target/release/trace-sim
    DICT=enable1.txt
    CACHE=game_cache
    PREFIX=""
fi

printf "%-3s %-6s %-7s %-8s %-7s %s\n" "k" "turns" "misses" "miss/trn" "maxMs" "status"
printf "%s\n" "------------------------------------------------------"

for k in $KS; do
    out=$($PREFIX "$BIN" --dict "$DICT" --length "$k" --cache-dir "$CACHE" \
        --traces 1 --strategy "$STRATEGY" --turn-deadline-secs 5 \
        --per-partition-secs 2 --warn-secs 1 2>&1)
    turns=$(echo "$out" | grep -oP 'turns total:\s+\K\d+')
    misses=$(echo "$out" | grep -oP 'partitions w/o EXACT:\s+\K\d+')
    max_miss_turn=$(echo "$out" | grep -oP 'max misses in 1 turn:\s+\K\d+')
    max_ms=$(echo "$out" | grep -oP 'max per-turn time:\s+\K\d+')
    if [ "${misses:-0}" -eq 0 ]; then
        status="✓ clean"
    else
        status="⚠ degraded ($misses non-EXACT)"
    fi
    printf "%-3s %-6s %-7s %-8s %-7s %s\n" "$k" "${turns:-?}" "${misses:-?}" "${max_miss_turn:-?}" "${max_ms:-?}" "$status"
done
