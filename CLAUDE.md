# CLAUDE.md — hangman2

## Project Overview

Solver for **Schrödinger's Hangman** (aka adversarial/evil hangman). This is a 2-player asymmetric perfect-information game:

- **Guesser** picks a letter each turn, trying to minimize total misses.
- **Referee** responds with either a miss or reveals all positions of that letter in the word — but the referee has no fixed word in mind. They may "imagine" any word consistent with prior reveals and misses.
- Both players agree on a dictionary and word length upfront.

**Goal:** Find optimal strategies for both players (minimax) and compute the minimum number of misses for each word length.

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
