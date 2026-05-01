#!/bin/bash
# uptime_check.sh — pings prod via curl, sends ntfy on state transitions.
# Run from cron every few minutes. Tracks last state in a file so it only
# fires on UP↔DOWN transitions, not on every failed poll.

URL="https://deadletters.fun/"
TIMEOUT=10
SLOW_THRESHOLD_MS=3000
NTFY="hangman2-compute-nic"
STATE_FILE="$HOME/.hangman2-uptime-state"
LOG="$HOME/.hangman2-uptime.log"

# curl: capture http_code + total_time. -m hard timeout. -k allow self-signed (we have real cert, but don't fail on cert hiccups).
result=$(curl -s -o /dev/null -w "%{http_code} %{time_total}" -m $TIMEOUT "$URL" 2>&1) || result="000 timeout"
code=$(echo "$result" | awk '{print $1}')
secs=$(echo "$result" | awk '{print $2}')
ms=$(echo "$secs" | awk '{printf "%d", $1 * 1000}')
ts=$(date +'%Y-%m-%dT%H:%M:%S%z')

# Classify
if [ "$code" = "200" ] && [ "$ms" -lt "$SLOW_THRESHOLD_MS" ]; then
    state="UP"
    note="$code in ${ms}ms"
elif [ "$code" = "200" ]; then
    state="SLOW"
    note="$code in ${ms}ms (>${SLOW_THRESHOLD_MS}ms)"
else
    state="DOWN"
    note="$code (${ms}ms)"
fi

echo "$ts $state $note" >> "$LOG"

# Read prior state
prior=$(cat "$STATE_FILE" 2>/dev/null || echo UP)
echo "$state" > "$STATE_FILE"

# Alert only on transitions (or first run if state unknown)
if [ "$prior" != "$state" ]; then
    if [ "$state" = "UP" ]; then
        msg="✓ deadletters.fun recovered — $note (was $prior)"
    else
        msg="⚠ deadletters.fun is $state — $note (was $prior)"
    fi
    curl -sd "$msg" "https://ntfy.sh/$NTFY" >/dev/null 2>&1 || true
fi
