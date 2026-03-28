# Dead Letters

A minimax solver and playable web game for **adversarial hangman** (also known as evil hangman or Schrödinger's hangman).

In adversarial hangman, the referee doesn't pick a word in advance. Instead, after each guess, the referee chooses whichever response (hit or miss) is worst for the guesser — as long as at least one word in the dictionary remains consistent with all previous responses. The guesser's goal is to minimize total misses; the referee's goal is to maximize them.

This project computes **minimax-optimal strategies** for both sides: the provably best guess at every position, and the provably worst-case response. It also serves a playable web version where you play the guesser against the optimal referee.

**Play it live at [deadletters.fun](https://deadletters.fun)**

## Minimax Results

Optimal miss counts for every word length (enable1.txt, 172,820 words):

| Length | Words | Optimal Misses |
|--------|-------|----------------|
| 2 | 96 | 14 |
| 3 | 972 | 17 |
| 4 | 3,903 | 16 |
| 5 | 8,636 | 15 |
| 6 | 15,232 | 12 |
| 7 | 23,109 | 11 |
| 8 | 28,420 | 8 |
| 9 | 24,873 | 6 |
| 10 | 20,300 | 5 |
| 11 | 15,504 | 6 |
| 12 | 11,357 | 4 |
| 13 | 7,827 | 4 |
| 14 | 5,127 | 3 |
| 15 | 3,192 | 3 |
| 16 | 1,943 | 2 |
| 17 | 1,127 | 2 |
| 18 | 594 | 2 |
| 19 | 329 | 2 |
| 20 | 160 | 1 |
| 21+ | <100 | 0-1 |

## How It Works

The solver uses alpha-beta search with several optimizations:

- **MTD(f) with Lazy SMP**: Main thread does iterative deepening with null-window probes; helper threads do full-window search in parallel, all sharing a lock-free transposition table.
- **Transposition table**: DashMap-based in-memory TT with LMDB disk cache for cross-session persistence. Canonical hashing folds equivalent positions.
- **Move ordering**: History heuristic tracks empirically good letters across the search.
- **Lower bound pruning**: Fast miss-chain analysis prunes positions that provably require more misses than the current bound allows.
- **Partition pruning**: Skips referee responses that can't improve the adversary's outcome.
- **Endgame solvers**: Specialized fast paths for 3- and 4-word subgames.

## Project Structure

```
src/              Core library (game logic, solver, dictionary)
  bin/            Precompute, benchmarking, and diagnostic tools
server/           Axum HTTP server (game API + hint endpoint)
web/              SvelteKit frontend (static site)
deploy.sh         Production deployment script
```

## Building

Requires Rust (stable) and Node.js 18+.

```bash
# Solver library and tools
cargo build --release

# Server
cargo build --release --manifest-path server/Cargo.toml

# Frontend
cd web && npm install && npm run build
```

## Solving

Compute optimal strategies and cache results to disk:

```bash
# Solve all positions for 8-letter words (depth 2 = first two guesses precomputed)
target/release/precompute --dict enable1.txt --lengths 8 --cache-dir game_cache --depth 2 --max-words 999999
```

Short word lengths (10+) solve in seconds. Longer words (3-7) can take hours and benefit from machines with many cores.

## Running the Server

```bash
target/release/dead-letters-server \
    --dictionary enable1.txt \
    --cache-dir game_cache \
    --port 3000
```

The server provides:
- `GET /api/new?length=N` — Start a new game
- `POST /api/guess` — Submit a letter guess (referee responds optimally)
- `GET /api/hint?game_id=ID` — Get the best move for the guesser
- `GET /api/wordlist?length=N` — Get all words of a given length

Positions not in the disk cache are solved on-demand (up to 10 seconds per request).

## License

MIT
