#!/bin/bash
# watcher_lib.sh — shared helpers for background watcher scripts.
#
# Source this file:
#   . /home/nic/proj/hangman2/scripts/watcher_lib.sh
#
# Why this exists: polling in bash is easy, but the safe patterns have footguns
# (pgrep self-matching, PID recycling, race windows). Encode the patterns that
# worked and keep the unsafe ones clearly labeled.

NTFY_TOPIC="${NTFY_TOPIC:-hangman2-compute-nic}"
NTFY_URL="${NTFY_URL:-https://ntfy.sh/$NTFY_TOPIC}"

# ---- logging ----

# log <msg...> — write a timestamped line to stderr.
log() {
    echo "[$(date -Iseconds)] $*" >&2
}

# ntfy <msg...> — post a message to the shared ntfy topic. Silent on failure.
ntfy() {
    local msg="$*"
    curl --max-time 10 -sS -d "$msg" "$NTFY_URL" > /dev/null 2>&1 || true
}

# ---- waiting primitives ----

# wait_pid_exit <pid> [poll_sec]
#   Block until the given PID is no longer running. Race-free: uses kill -0,
#   which is unaffected by process argv or our own command line. Default poll: 30s.
#   Returns immediately (0) if PID already gone.
wait_pid_exit() {
    local pid="$1" poll="${2:-30}"
    [ -z "$pid" ] && { log "wait_pid_exit: missing pid"; return 2; }
    while kill -0 "$pid" 2>/dev/null; do sleep "$poll"; done
    return 0
}

# wait_file <path> [poll_sec]
#   Block until the file exists. Good for cloud-init readiness markers.
wait_file() {
    local path="$1" poll="${2:-15}"
    until [ -e "$path" ]; do sleep "$poll"; done
}

# wait_log_marker <file> <grep_pattern> [poll_sec]
#   Block until `grep -q <pattern> <file>` succeeds. The file may not exist yet.
wait_log_marker() {
    local file="$1" pattern="$2" poll="${3:-30}"
    until [ -f "$file" ] && grep -q -- "$pattern" "$file" 2>/dev/null; do
        sleep "$poll"
    done
}

# ---- process lookup ----

# safe_pgrep <exe_basename> <cmdline_substring>
#   Print PIDs matching BOTH:
#     (a) /proc/<pid>/comm equals <exe_basename>  (the actual binary name, not argv[0])
#     (b) /proc/<pid>/cmdline contains <cmdline_substring> as a substring
#   Excludes the caller's own PID and its parent shell chain.
#
#   Avoids the classic `pgrep -f` self-match: because (a) requires the
#   process's real executable name (from /proc/<pid>/comm — the kernel's
#   record of the binary), a bash script that merely *mentions* the binary's
#   path in its argv will not match. Only actual executions of that binary
#   are returned.
#
#   Example:
#     safe_pgrep precompute '--lengths 7 '
safe_pgrep() {
    local exe="$1" needle="$2"
    local self="$$" ppid_self="${PPID:-0}"
    local pid comm cmd
    for pid in /proc/[0-9]*; do
        pid="${pid#/proc/}"
        [ "$pid" = "$self" ] && continue
        [ "$pid" = "$ppid_self" ] && continue
        comm=$(cat "/proc/$pid/comm" 2>/dev/null) || continue
        [ "$comm" = "$exe" ] || continue
        cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || continue
        case "$cmd" in
            *"$needle"*) echo "$pid" ;;
        esac
    done
}

# wait_process_start <exe_basename> <cmdline_substring> [poll_sec]
#   Block until at least one process matching safe_pgrep exists. Prints the
#   first matching PID on stdout when done.
wait_process_start() {
    local exe="$1" needle="$2" poll="${3:-30}"
    local pid=""
    while true; do
        pid=$(safe_pgrep "$exe" "$needle" | head -1)
        [ -n "$pid" ] && { echo "$pid"; return 0; }
        sleep "$poll"
    done
}
