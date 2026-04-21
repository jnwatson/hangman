#!/usr/bin/env python3
"""Watchdog for CAX precompute shards.

Every 10 min, SSH to each machine and check:
  - Is /root/run_precompute.sh still running?
  - Is /root/precompute.log mtime within the last STALE_THRESHOLD_SEC?

Emits ntfy alerts for:
  - STUCK: process alive but log hasn't grown for >threshold
  - DIED:  process not running but log doesn't contain 'total' (ended without finishing)

Once all shards are DONE or DIED, exit.
"""

import json
import subprocess
import sys
import time
import urllib.request

MANIFEST = "/home/nic/proj/hangman2/scripts/cax_machines.json"
NTFY = "https://ntfy.sh/hangman2-compute-nic"
POLL_SEC = 600          # 10 min
STALE_THRESHOLD_SEC = 3600  # 1 h
SSH_OPTS = [
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "UserKnownHostsFile=/home/nic/.ssh/known_hosts_hangman",
    "-o", "ConnectTimeout=15",
    "-o", "BatchMode=yes",
]


def notify(msg):
    try:
        urllib.request.urlopen(
            urllib.request.Request(NTFY, data=msg.encode(), method="POST"),
            timeout=10,
        )
    except Exception as e:
        print(f"[warn] ntfy failed: {e}", flush=True)


def ssh_probe(ip):
    """Return dict with: running, log_age_sec (None if no log), log_size, last_line, finished."""
    cmd = (
        "if pgrep -f 'run_precompute.sh\\|target/release/precompute' > /dev/null; "
        "then echo RUNNING; else echo NOT_RUNNING; fi; "
        "if [ -f /root/precompute.log ]; then "
        "  stat -c '%Y %s' /root/precompute.log; "
        "  tail -1 /root/precompute.log; "
        "  grep -c 'total,' /root/precompute.log; "
        "else echo NOLOG; fi"
    )
    try:
        r = subprocess.run(
            ["ssh", *SSH_OPTS, f"root@{ip}", cmd],
            capture_output=True, text=True, timeout=30,
        )
        if r.returncode != 0:
            return {"error": r.stderr.strip()[:120]}
        lines = r.stdout.strip().splitlines()
        out = {"running": lines[0] == "RUNNING"}
        if len(lines) > 1 and lines[1] == "NOLOG":
            out["has_log"] = False
        else:
            stat_line = lines[1].split()
            mtime = int(stat_line[0])
            size = int(stat_line[1])
            last = lines[2] if len(lines) > 2 else ""
            total_marker = int(lines[3]) if len(lines) > 3 else 0
            out["has_log"] = True
            out["log_age_sec"] = int(time.time()) - mtime
            out["log_size"] = size
            out["last_line"] = last[:100]
            out["finished"] = total_marker > 0
        return out
    except subprocess.TimeoutExpired:
        return {"error": "ssh timeout"}
    except Exception as e:
        return {"error": str(e)[:120]}


def main():
    machines = json.load(open(MANIFEST))
    state = {m["ip"]: "PENDING" for m in machines}
    alerted_stuck = set()
    notify(f"WATCHDOG start: monitoring {len(machines)} CAX shards")
    while True:
        live = 0
        for m in machines:
            ip = m["ip"]
            tag = f"k={m['k']} s={m['shard']}/4"
            if state[ip] in ("DONE", "DIED"):
                continue
            p = ssh_probe(ip)
            if "error" in p:
                print(f"[{time.strftime('%H:%M:%S')}] {tag} {ip}: SSH error {p['error']}", flush=True)
                live += 1
                continue
            if not p.get("has_log"):
                print(f"[{time.strftime('%H:%M:%S')}] {tag} {ip}: no log yet, running={p['running']}", flush=True)
                live += 1
                continue
            if p.get("finished"):
                if state[ip] != "DONE":
                    state[ip] = "DONE"
                    notify(f"WATCHDOG DONE {tag} host={ip}")
                    print(f"[{time.strftime('%H:%M:%S')}] {tag}: DONE", flush=True)
                continue
            if not p["running"]:
                # Process died without writing 'total,' marker
                state[ip] = "DIED"
                notify(f"WATCHDOG DIED {tag} host={ip} last='{p['last_line']}'")
                print(f"[{time.strftime('%H:%M:%S')}] {tag}: DIED", flush=True)
                continue
            # Running, not finished — check for stall
            age = p["log_age_sec"]
            print(f"[{time.strftime('%H:%M:%S')}] {tag} {ip}: log_age={age}s size={p['log_size']} last='{p['last_line'][:60]}'", flush=True)
            live += 1
            if age > STALE_THRESHOLD_SEC and ip not in alerted_stuck:
                alerted_stuck.add(ip)
                notify(f"WATCHDOG STUCK {tag} host={ip} stale={age}s last='{p['last_line']}'")
        if live == 0:
            notify("WATCHDOG exit: all shards DONE/DIED")
            print("all shards terminal — exiting", flush=True)
            return
        time.sleep(POLL_SEC)


if __name__ == "__main__":
    sys.exit(main())
