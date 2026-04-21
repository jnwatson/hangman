# CLAUDE.md — hangman2

## Project Overview

Solver for **Schrödinger's Hangman** (aka adversarial/evil hangman). This is a 2-player asymmetric perfect-information game:

- **Guesser** picks a letter each turn, trying to minimize total misses.
- **Referee** responds with either a miss or reveals all positions of that letter in the word — but the referee has no fixed word in mind. They may "imagine" any word consistent with prior reveals and misses.
- Both players agree on a dictionary and word length upfront.

**Goal:** Prove we have a **strong solution** to adversarial hangman — i.e., compute minimax-optimal play from every reachable position, for every supported word length, and persist those results so any position can be answered on demand. Finding the root minimax miss count per length is a byproduct; the deliverable is a complete strategy, not just the numbers.

## Tech Stack

- **Language:** Rust (latest stable edition)
- **Build:** Cargo
- **Dictionary:** Configurable at runtime; support multiple word list sources (system dictionaries, custom files)

## Architecture

- Modular design: separate crates/modules for:
  - `game` — game state, rules, legal moves
  - `solver` — minimax search, pruning, caching
  - `dictionary` — word list loading, filtering, pattern matching
  - `cli` — command-line interface and result output
- Output is results-oriented: optimal miss counts per word length. No interactive play or visualization needed.

## Development Approach

- **Research-first:** Understand the problem structure and existing work on adversarial hangman before committing to an algorithm. Consider alpha-beta pruning, transposition tables, and problem-specific optimizations.
- **Performance is critical:** The search space is large. Profile early, optimize data structures and search order. Consider bitsets for letter sets, efficient word partitioning, and memoization.

## Code Quality

- **Clippy strict:** All clippy lints enabled, no warnings allowed. Use `#![deny(clippy::all, clippy::pedantic)]`.
- **rustfmt:** Always format with default rustfmt settings.
- **Testing:** Heavily tested. Use property-based testing (proptest/quickcheck) where applicable. Unit tests for core game logic and solver correctness. Integration tests to verify known results.

## Commands

```bash
cargo build              # Build
cargo build --release    # Optimized build (use for benchmarks/solving)
cargo test               # Run all tests
cargo clippy             # Lint (must pass clean)
cargo fmt -- --check     # Check formatting
```

## Conventions

- Prefer `&str` over `String` where lifetimes allow.
- Use bitwise operations for letter sets (u32 bitmask for 26 letters).
- Document public APIs. Internal comments only where logic is non-obvious.
- Error handling: use `anyhow` for CLI, typed errors for library code.
- No `unsafe` unless profiling proves it necessary and it's well-documented.

## Solver Optimization Lessons

These are hard-won lessons from benchmarking. Violating them causes regressions.

### Lower bounds: cutoff only, never alpha-tightening
Using a lower bound to raise alpha poisons TT entries — values get stored as UPPER_BOUND instead of EXACT, degrading cache effectiveness across the entire search. Any new lower bound or heuristic should only be used for cutoff (`lb >= beta → return`). Never modify alpha based on a bound that isn't from the TT itself.

### ETC is net negative with expensive canonicalization
Enhanced Transposition Cutoff probes TT for each partition before recursing, but computing canonical keys (`dedup_and_hash`) is O(n·k·log n) and dominates at intermediate nodes. TT hit rate isn't high enough to amortize. Only use ETC when key computation is cheap (e.g., Zobrist hashing).

### Miss-chain lower bound: depth and threshold tuning
- Full miss-chain (depth 2-3): gate at ≥5000 words only
- Depth-1 fast path: when `beta <= 1` and `required_splitting == 0`, just check `present_letters != 0` — this is O(n) and resolves null-window scouts instantly (biggest single optimization, ~35% speedup on k=8)
- Don't extend to depth=2 for beta≤2 at intermediate nodes — O(26²·n) overhead outweighs pruning

### Partition UB pruning
Skip partitions where `miss_cost + present_letters(partition).count_ones() <= worst`. But don't try to track the letter union during partitioning inline — enlarging the FxHashMap value type makes it slower. Compute `present_letters` separately after the size check.

### SMP helper tuning
- MTD(f) path (≤25K words): up to 7 helpers — iterative deepening warms TT, helpers benefit
- Pure Lazy SMP (>25K words): clamp to 2 helpers — DashMap contention from 7+ threads outweighs search diversity

### MTD(f) + SMP hybrid
For ≤25K words, main thread does MTD(f) iterative deepening while helpers do full-window search. Helpers should always use full-window (alpha=0, beta=MAX) — ID helpers regress performance. For >25K words, pure Lazy SMP (no iterative deepening) wins.

### Cancellation safety
Cancelled helpers must return beta (pessimistic) and skip cache stores. Returning alpha=0 poisons the TT — cancelled subtrees look "good" and get cached as correct results.

### Always verify with `--naive` oracle
After any pruning change, run benchmarks with `--naive` on small-to-medium lengths (k=8-15) to verify correctness. Pruning bugs produce plausible numbers that are simply too low.

## Serving Infrastructure

### Production server
- **Droplet:** deadletters.fun (64.225.9.155), user `nic`, service runs as `www`
- **Deploy:** `bash deploy.sh [--skip-cache]` — builds server + frontend, uploads, restarts systemd service
- **Cache:** LMDB on 100GB volume at `/mnt/volume_nyc3_01/cache`, service reads via `--cache-dir`
- **Nginx:** SPA fallback (`try_files $uri $uri/ /index.html`), proxies `/api/` to port 3000
- **Server binary:** `server/` crate (Axum), uses disk cache for partition evaluation
- **Frontend:** `web/` (SvelteKit), built as static site with adapter-static for deploy

### Game API design
- `GuessResponse` includes `solve_status`: `"solved"` (all partitions cached), `"degraded"` (some missed but chosen was cached), `"unresolved"` (chosen partition had cache miss — referee is guessing)
- Valid words sent as base64-encoded bitvector (`valid_words_bitvec`), not index array
- Frontend tracks worst `solveStatus` across all guesses in a game

### Partition selection bug (fixed)
`best_partition_mask` must be initialized from the first actual partition entry, not hardcoded to 0. Initializing to 0 (miss) causes the referee to mark letters as wrong while keeping words that contain them — when all partitions have value 0 (cache miss) and no miss partition exists.

## Precompute Infrastructure

### Precompute binary
`src/bin/precompute.rs` — enumerates depth-1 and depth-2 game tree positions, solves uncached ones with full SMP solver, flushes to LMDB disk cache.

```bash
# Solve all positions for k=8, no size limit
target/release/precompute --dict enable1.txt --lengths 8 --cache-dir game_cache --depth 2 --max-words 999999
```

- **Resumable:** checks disk cache before each solve via `is_cached_exact()`
- **Always use `--max-words 999999`** (default 5000 skips large positions, leaving cache gaps that break serving)
- Sorts positions largest-first, uses full `solve()` for SMP parallelism

### Compute droplets (DigitalOcean)
Three `c-16` droplets (16 vCPUs, 32GB RAM) in NYC1, tagged `hangman-compute`:

| Host | IP | Assignment |
|------|-----|------------|
| hc1 | 143.198.120.95 | k=7 (23K words, minimax=11) |
| hc2 | 64.227.4.233 | k=8 (28K words, minimax=8) |
| hc3 | 165.227.83.53 | k=5,6 (8.6K + 15K words) |

- SSH aliases `hc1`, `hc2`, `hc3` configured in `~/.ssh/config`
- Source code at `/root/hangman2/`, caches at `/root/hangman2/game_cache/`
- Logs at `/tmp/precompute.log` on each droplet
- **Notifications:** via `ntfy.sh/hangman2-compute-nic` — subscribe in ntfy app
- k=3,4 runs locally (small enough)

### Workflow: after precompute completes
1. Rsync cache from droplet back to local: `rsync -az hcN:/root/hangman2/game_cache/tt_lenK_* game_cache/`
2. Rsync cache to production: `rsync -az game_cache/tt_lenK_* nic@64.225.9.155:/mnt/volume_nyc3_01/cache/`
3. Fix permissions: `ssh nic@64.225.9.155 'sudo chown -R www:www /mnt/volume_nyc3_01/cache'`
4. Restart service: `ssh nic@64.225.9.155 'sudo systemctl restart dead-letters'`
5. Destroy droplets when done (DO API key in `env` file, never commit)
