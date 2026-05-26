#!/usr/bin/env python3
"""Smoke-test the hint endpoint for monotonicity.

For each word length, plays a game on Hard difficulty by always taking
the hint's recommended letter. After each guess, re-queries the hint and
checks that its `value` did not INCREASE. An increase indicates a stale
LOWER bound or other cache inconsistency.

Usage:
    scripts/smoke_hints.py                       # default https://deadletters.fun
    scripts/smoke_hints.py --base http://localhost:3000
    scripts/smoke_hints.py --lengths 5,6,11      # subset
"""

import argparse
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import json


def http_get(url: str, retries: int = 2) -> dict:
    last_err = None
    for _ in range(retries + 1):
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code == 503:
                last_err = e
                time.sleep(1.5)
                continue
            msg = e.read().decode("utf-8", "replace")[:200]
            raise RuntimeError(f"GET {url} -> {e.code}: {msg}") from e
        except Exception as e:
            last_err = e
            time.sleep(0.5)
    raise last_err


def http_post(url: str, payload: dict) -> dict:
    body = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        msg = e.read().decode("utf-8", "replace")[:200]
        raise RuntimeError(f"POST {url} -> {e.code}: {msg}") from e


def play_one(base: str, k: int) -> tuple[bool, list[tuple[str, int, int, str]]]:
    """Play k once on Hard. Return (clean, trail).

    trail entries: (letter_guessed, hint_value_pre, expected_post, result)
    where result is 'miss' or 'hit:N' (N = letters revealed).

    Bug check: under optimal play with adversarial referee, V(S_{t+1})
    equals V(S_t) - miss_cost exactly. So after a miss, hint value must
    drop by 1; after a hit, it must stay the same. Any deviation means
    the cache is inconsistent — either over-reporting (sub-optimal letter
    picked due to LOWER bounds masquerading as cache misses) or
    under-reporting (the previous hint was too low).
    """
    new = http_get(f"{base}/api/new?length={k}&difficulty=hard")
    game_id = new["game_id"]
    trail: list[tuple[str, int, int, str]] = []
    expected_v: int | None = None  # what THIS step's hint value should be

    while True:
        try:
            hint = http_get(f"{base}/api/hint?game_id={game_id}")
        except urllib.error.HTTPError as e:
            if e.code == 400:
                # game over
                break
            raise
        v_pre = hint.get("value")
        letter = hint["letter"]

        # Strict check: hint at this state must equal what optimal play
        # would have produced from the previous state minus the miss cost.
        if expected_v is not None and v_pre is not None and v_pre != expected_v:
            label = "OVER-REPORT" if v_pre > expected_v else "UNDER-REPORT"
            trail.append((letter, expected_v, v_pre, label))
            return False, trail

        guess = http_post(f"{base}/api/guess", {"game_id": game_id, "letter": letter.lower()})
        # Server might queue; treat queued as fail-soft for now
        if "request_id" in guess and "pattern" not in guess:
            # Tier 2 async — poll until done.
            qid = guess["request_id"]
            done = False
            for _ in range(60):
                time.sleep(1)
                try:
                    status = http_get(f"{base}/api/guess_status?request_id={qid}")
                except RuntimeError as e:
                    # Server may discard finished records quickly under churn.
                    if "404" in str(e):
                        time.sleep(0.5)
                        continue
                    raise
                if status.get("state") == "done" and status.get("result"):
                    guess = status["result"]
                    done = True
                    break
                if status.get("state") == "failed":
                    trail.append((letter, v_pre, -1, f"FAILED:{status.get('error')}"))
                    return False, trail
            if not done:
                trail.append((letter, v_pre, -1, "TIMEOUT"))
                return False, trail

        wrong = guess["wrong_letters"]
        if letter.lower() in (w.lower() for w in wrong):
            result = "miss"
            miss_cost = 1
        else:
            positions = guess.get("positions") or []
            result = f"hit:{len(positions)}"
            miss_cost = 0

        # Compute expected V for next state: V_post = V_pre - miss_cost.
        next_expected = (v_pre - miss_cost) if v_pre is not None else None
        trail.append((letter, v_pre if v_pre is not None else -1,
                      next_expected if next_expected is not None else -1, result))

        if guess.get("game_over"):
            break

        expected_v = next_expected

    return True, trail


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="https://deadletters.fun")
    ap.add_argument("--lengths", default="3-14", help="comma list or N-M range")
    ap.add_argument("--iterations", type=int, default=1,
                    help="games per k (referee tie-breaking varies by HashMap seed → different paths)")
    args = ap.parse_args()

    if "-" in args.lengths and "," not in args.lengths:
        a, b = args.lengths.split("-")
        lengths = list(range(int(a), int(b) + 1))
    else:
        lengths = [int(x) for x in args.lengths.split(",")]

    overall_ok = True
    for k in lengths:
        unique_paths: dict[tuple, bool] = {}
        first_failure = None
        runs_attempted = 0
        for i in range(args.iterations):
            try:
                ok, trail = play_one(args.base, k)
            except Exception as e:
                print(f"k={k:2d} run {i+1}  ERROR  {e!r}")
                overall_ok = False
                continue
            runs_attempted += 1
            path_key = tuple(letter for letter, _, _, _ in trail)
            unique_paths[path_key] = ok and unique_paths.get(path_key, True)
            if not ok and first_failure is None:
                first_failure = trail

        clean_runs = sum(1 for ok in unique_paths.values() if ok)
        bad_runs = len(unique_paths) - clean_runs
        if first_failure is None:
            print(f"k={k:2d}  OK    {runs_attempted} run(s), {len(unique_paths)} unique path(s) — all monotone")
        else:
            overall_ok = False
            last = first_failure[-1]
            print(f"k={k:2d}  FAIL  {bad_runs}/{len(unique_paths)} unique paths violated; "
                  f"first: hint went {last[1]} → {last[2]} after guessing {last[0]!r}")
            letters = "".join(t[0] for t in first_failure)
            print(f"        sequence: {letters}")
    return 0 if overall_ok else 1


if __name__ == "__main__":
    sys.exit(main())
