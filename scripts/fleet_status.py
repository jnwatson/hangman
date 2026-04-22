#!/usr/bin/env python3
"""One-command status dashboard for all hangman2 compute machines.

Reads scripts/fleet.json, probes each machine in parallel (local for
`probe:local`, SSH for `probe:ssh`, skipped for `probe:pending`), and
prints a compact table with state, progress, RSS, last-log age, and
accumulated cost.

Safe to run repeatedly — read-only.
"""

import concurrent.futures
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path

FLEET = Path("/home/nic/proj/hangman2/scripts/fleet.json")
SSH_OPTS = [
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "UserKnownHostsFile=/home/nic/.ssh/known_hosts_hangman",
    "-o", "ConnectTimeout=10",
    "-o", "BatchMode=yes",
]

PROBE_SH_TMPL = r"""
set -u
LOG="__LOG__"
LOG_GLOB="__LOG_GLOB__"
PID_MATCH="__PID_MATCH__"
# If LOG_GLOB is set, pick the most recently modified matching file.
if [ -n "$LOG_GLOB" ]; then
    LATEST=$(ls -1t $LOG_GLOB 2>/dev/null | head -1)
    [ -n "$LATEST" ] && LOG=$LATEST
fi
if [ -f "$LOG" ]; then
    MTIME=$(stat -c %Y "$LOG")
    SIZE=$(stat -c %s "$LOG")
    LAST=$(tail -1 "$LOG" 2>/dev/null | tr '\t' ' ' | cut -c1-100)
    PCT=$(grep -oE '\[ *[0-9]+\.[0-9]+%\]' "$LOG" 2>/dev/null | tail -1 | tr -d '[]% ')
    FINISHED=$(grep -cE 's total$|^Done: ' "$LOG" 2>/dev/null || echo 0)
else
    MTIME=0; SIZE=0; LAST=""; PCT=""; FINISHED=0
fi
# Find the precompute child (not the /usr/bin/time -v wrapper) whose
# cmdline includes PID_MATCH. pgrep -x matches exact binary name, then we
# verify the specific k+depth args on each candidate.
PID=""
for pid in $(pgrep -x precompute); do
    if tr '\0' ' ' < /proc/$pid/cmdline 2>/dev/null | grep -qF -- "$PID_MATCH"; then
        PID=$pid
        break
    fi
done
if [ -n "$PID" ]; then
    RSS=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
    THR=$(ps -o nlwp= -p "$PID" 2>/dev/null | tr -d ' ')
    CPU=$(ps -o %cpu= -p "$PID" 2>/dev/null | tr -d ' ')
else
    RSS=0; THR=0; CPU=0
fi
echo "PID=$PID"
echo "RSS_KB=$RSS"
echo "THREADS=$THR"
echo "CPU=$CPU"
echo "MTIME=$MTIME"
echo "LOG_SIZE=$SIZE"
echo "PCT=$PCT"
echo "FINISHED=$FINISHED"
echo "LAST=$LAST"
"""


def _build_probe(log, log_glob, pid_match):
    return (PROBE_SH_TMPL
            .replace("__LOG__", log)
            .replace("__LOG_GLOB__", log_glob)
            .replace("__PID_MATCH__", pid_match))


def probe_ssh(m):
    ip = m["ip"]
    user = m.get("user", "root")
    log = m.get("log_path", "/root/precompute.log")
    log_glob = m.get("log_glob", "")
    pid_match = m.get("pid_match", "")
    try:
        r = subprocess.run(
            ["ssh", *SSH_OPTS, f"{user}@{ip}", _build_probe(log, log_glob, pid_match)],
            capture_output=True, text=True, timeout=25,
        )
        if r.returncode != 0:
            return {"error": (r.stderr or r.stdout).strip()[:80] or "ssh failed"}
        return _parse_probe(r.stdout)
    except subprocess.TimeoutExpired:
        return {"error": "ssh timeout"}
    except Exception as e:
        return {"error": str(e)[:80]}


def _resolve_log_path(m):
    """For chained jobs, log_path is a template that may refer to a single
    old depth's file. If `log_glob` is set, pick the most recently modified
    file matching that glob — which tracks whichever depth the chain is
    currently writing to.
    """
    glob_pat = m.get("log_glob")
    if glob_pat:
        import glob
        matches = sorted(glob.glob(glob_pat), key=lambda p: Path(p).stat().st_mtime if Path(p).exists() else 0)
        if matches:
            return Path(matches[-1])
    return Path(m["log_path"])


def probe_local(m):
    log = _resolve_log_path(m)
    pid_match = m.get("pid_match", "")
    d = {"PID": "", "RSS_KB": "0", "THREADS": "0", "CPU": "0",
         "MTIME": "0", "LOG_SIZE": "0", "PCT": "", "FINISHED": "0", "LAST": ""}
    if log.exists():
        st = log.stat()
        d["MTIME"] = str(int(st.st_mtime))
        d["LOG_SIZE"] = str(st.st_size)
        text = log.read_text(errors="replace")
        tail = text.rstrip().splitlines()[-1:] if text.strip() else [""]
        d["LAST"] = tail[0][:100] if tail else ""
        pct_matches = re.findall(r"\[ *([0-9]+\.[0-9]+)%\]", text)
        if pct_matches:
            d["PCT"] = pct_matches[-1]
        d["FINISHED"] = str(len(re.findall(r"s total$|^Done: ", text, re.MULTILINE)))
    # Find the specific precompute PID matching pid_match (via cmdline).
    try:
        pattern = f"target/release/precompute.*{pid_match}" if pid_match else "target/release/precompute"
        r = subprocess.run(
            ["pgrep", "-f", pattern],
            capture_output=True, text=True, timeout=3,
        )
        pids = r.stdout.split()
        if pids:
            # Filter out the /usr/bin/time -v wrapper — keep just the precompute binary.
            for pid in pids:
                cmd_path = Path(f"/proc/{pid}/comm")
                if cmd_path.exists() and cmd_path.read_text().strip() == "precompute":
                    d["PID"] = pid
                    ps = subprocess.run(
                        ["ps", "-o", "rss=,nlwp=,%cpu=", "-p", pid],
                        capture_output=True, text=True, timeout=3,
                    )
                    parts = ps.stdout.split()
                    if len(parts) >= 3:
                        d["RSS_KB"], d["THREADS"], d["CPU"] = parts[0], parts[1], parts[2]
                    break
    except Exception:
        pass
    return _parse_probe_dict(d)


def _parse_probe(stdout):
    d = {}
    for line in stdout.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            d[k.strip()] = v.strip()
    return _parse_probe_dict(d)


def _parse_probe_dict(d):
    out = {
        "pid": d.get("PID") or None,
        "rss_gb": float(d.get("RSS_KB") or 0) / (1024 * 1024),
        "threads": int(d.get("THREADS") or 0),
        "cpu": float(d.get("CPU") or 0),
        "log_mtime": int(d.get("MTIME") or 0),
        "log_size": int(d.get("LOG_SIZE") or 0),
        "pct": d.get("PCT") or "",
        "finished": int(d.get("FINISHED") or 0),
        "last": d.get("LAST") or "",
    }
    return out


def classify(p):
    """Return (state, note) for pretty-printing. PID existence wins over
    accumulated Done: markers so chained runs don't get falsely marked DONE."""
    if "error" in p:
        return ("ERROR", p["error"][:50])
    if p["pid"]:
        age = (datetime.datetime.now().timestamp() - p["log_mtime"]) if p["log_mtime"] else None
        if age is not None and age > 1800:
            return ("STALL", f"no log in {int(age)}s")
        return ("RUN", f"{p['threads']}t {p['cpu']:.0f}%cpu")
    if p["finished"] and p["finished"] > 0:
        return ("DONE", f"{p['finished']} stage(s) complete")
    if p["log_size"] > 0:
        return ("IDLE", "process gone, log exists")
    return ("NOLOG", "")


def fmt_age(secs):
    if secs < 60: return f"{int(secs)}s"
    if secs < 3600: return f"{int(secs/60)}m"
    return f"{secs/3600:.1f}h"


def fmt_cost(m):
    try:
        start = datetime.datetime.fromisoformat(m["started_at"])
    except Exception:
        return "?"
    elapsed_hr = (datetime.datetime.now(start.tzinfo) - start).total_seconds() / 3600
    cost = elapsed_hr * m.get("cost_per_hr_usd", 0)
    cap = m.get("cost_monthly_cap_usd")
    if cap:
        cost = min(cost, cap)
    return f"${cost:.2f}" if cost >= 0.01 else "$0"


def main():
    fleet = json.loads(FLEET.read_text())
    now = datetime.datetime.now().timestamp()

    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as ex:
        futures = {}
        for m in fleet:
            if m["probe"] == "local":
                futures[m["id"]] = ex.submit(probe_local, m)
            elif m["probe"] == "ssh":
                futures[m["id"]] = ex.submit(probe_ssh, m)
            else:
                futures[m["id"]] = None
        results = {mid: (f.result() if f else {"pending": True}) for mid, f in futures.items()}

    print(f"fleet_status @ {datetime.datetime.now().strftime('%H:%M:%S')}")
    hdr = f"{'ID':<32} {'STATE':<7} {'PROG':>6} {'RSS':>6} {'LOG-AGE':>8} {'COST':>7}  NOTE / LAST LINE"
    print(hdr)
    print("-" * len(hdr))

    total_cost = 0.0
    for m in fleet:
        p = results[m["id"]]
        if p.get("pending"):
            line = f"{m['id']:<32} {'WAIT':<7} {'':>6} {'':>6} {'':>8} {'$0':>7}  {m['job']}"
            print(line)
            continue
        state, note = classify(p)
        rss = f"{p['rss_gb']:.1f}G" if p["rss_gb"] > 0.05 else ""
        prog = f"{p['pct']}%" if p["pct"] else ""
        age = fmt_age(now - p["log_mtime"]) if p["log_mtime"] else ""
        cost = fmt_cost(m)
        try:
            total_cost += float(cost.lstrip("$"))
        except ValueError:
            pass
        # note takes priority if something is wrong, else show last log line
        tail = p.get("last", "")[:60] if state in ("RUN", "DONE") else note
        print(f"{m['id']:<32} {state:<7} {prog:>6} {rss:>6} {age:>8} {cost:>7}  {tail}")

    print("-" * len(hdr))
    print(f"total accumulated cost: ${total_cost:.2f}")


if __name__ == "__main__":
    sys.exit(main())
