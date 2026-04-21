#![deny(clippy::all, clippy::pedantic)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unreadable_literal,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::naive_bytecount,
)]

//! Simulate games against the cached solver and measure per-turn time.
//!
//! For each trace, starts from the root state and plays turns to termination:
//!   1. Player picks a letter via `--strategy`.
//!   2. Simulate the full server path for that guess: partition by letter,
//!      cache-lookup each partition, live-solve any misses with a 10s total
//!      deadline (4s per partition), pick the worst partition.
//!   3. Record the turn's wall-clock. Advance state to the worst partition.
//!   4. Stop when the remaining word set collapses to 1 (or max-turns).
//!
//! Aggregate across N traces and report max per-turn time, percentiles,
//! and list of slow turns. The metric the server cares about is
//! max-per-turn <8s (10s budget with safety margin).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::MemoizedSolver;
use hangman2::solver::serving::{canonical_hash_for_words, fold_required_letters, pos_mask};

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short = 'k', long)]
    length: usize,

    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    #[arg(long, default_value_t = 100)]
    traces: u32,

    /// One of: optimal, random, adversarial (stress server).
    #[arg(long, default_value = "optimal")]
    strategy: String,

    /// Per-turn total deadline in seconds (matches server's 10s budget).
    #[arg(long, default_value_t = 10)]
    turn_deadline_secs: u64,

    /// Flag any turn slower than this (seconds).
    #[arg(long, default_value_t = 3)]
    warn_secs: u64,

    /// Cap per-partition live-solve (matches server's 4s cap).
    #[arg(long, default_value_t = 4)]
    per_partition_secs: u64,

    /// Safety: stop a trace after this many turns.
    #[arg(long, default_value_t = 30)]
    max_turns: u32,

    /// RNG seed for reproducibility.
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Print a line for every turn (default on for --traces 1).
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

struct State {
    indices: Vec<usize>,
    masked: u32,
    turns: u32,
}

fn partition_by_letter(
    words: &[Vec<u8>],
    indices: &[usize],
    letter: u8,
) -> HashMap<u32, Vec<usize>> {
    let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in indices {
        let m = pos_mask(&words[idx], letter);
        parts.entry(m).or_default().push(idx);
    }
    parts
}

/// Simple LCG for reproducible random picks without adding a rand dep.
fn lcg_next(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state
}

/// Simulate one full server turn for a given (state, letter). Returns
/// (turn_wall_time_ms, worst_partition_indices, worst_partition_masked, worst_value).
#[allow(clippy::too_many_arguments)]
fn simulate_turn(
    words: &[Vec<u8>],
    state: &State,
    letter: u8,
    dc: &DiskCache,
    solver: &MemoizedSolver,
    turn_deadline_secs: u64,
    per_partition_secs: u64,
) -> (u64, Vec<usize>, u32, u32) {
    let t0 = Instant::now();
    let new_masked = state.masked | letter_bit(letter);
    let parts = partition_by_letter(words, &state.indices, letter);

    // Phase 1: look up each partition in cache.
    let mut parts_vec: Vec<(u32, Vec<usize>)> = parts.into_iter().collect();
    let mut values: Vec<Option<u32>> = Vec::with_capacity(parts_vec.len());
    let mut unsolved: Vec<usize> = Vec::new();
    for (i, (_pmask, indices)) in parts_vec.iter().enumerate() {
        if indices.len() <= 1 {
            values.push(Some(0));
            continue;
        }
        let folded = fold_required_letters(words, indices, new_masked);
        let hash = canonical_hash_for_words(words, indices, folded);
        if let Some(packed) = dc.get(hash) {
            // value is bits 0..4 of packed entry
            let val = packed & 0x1F;
            values.push(Some(val));
        } else {
            values.push(None);
            unsolved.push(i);
        }
    }

    // Phase 2: live-solve cache misses with total deadline + per-partition cap.
    if !unsolved.is_empty() {
        let total_deadline = Instant::now() + Duration::from_secs(turn_deadline_secs);
        // Smallest first, like the server.
        unsolved.sort_by_key(|&i| parts_vec[i].1.len());
        for &i in &unsolved {
            let now = Instant::now();
            if now >= total_deadline {
                break;
            }
            let per_cap = Duration::from_secs(per_partition_secs);
            let remaining = total_deadline.saturating_duration_since(now);
            let dl = now + remaining.min(per_cap);
            let v = solver.solve_position_with_deadline(&parts_vec[i].1, new_masked, Some(dl));
            if !solver.was_cancelled() {
                values[i] = Some(v);
            }
            // If cancelled, values[i] stays None — worst-case picker will skip it.
        }
    }

    // Phase 3: pick worst partition (referee-optimal). Unsolved → treated as
    // worst (unbounded), so they lose to any solved partition. Match server.
    let mut best_idx: Option<usize> = None;
    let mut best_val: u32 = 0;
    for (i, (pmask, _indices)) in parts_vec.iter().enumerate() {
        if let Some(v) = values[i] {
            let miss_cost = u32::from(*pmask == 0);
            let total_val = miss_cost + v;
            if best_idx.is_none() || total_val > best_val {
                best_idx = Some(i);
                best_val = total_val;
            }
        }
    }
    // Fallback: if all missed (shouldn't happen for small indices), pick biggest.
    if best_idx.is_none() {
        let (i, _) = parts_vec
            .iter()
            .enumerate()
            .max_by_key(|(_, p)| p.1.len())
            .expect("empty partitions");
        best_idx = Some(i);
    }

    let turn_ms = t0.elapsed().as_millis() as u64;
    let idx = best_idx.unwrap();
    let (_pmask, indices) = parts_vec.swap_remove(idx);
    // The guessed letter is always added to `masked` — it has been claimed
    // whether the referee answered miss (0) or hit (pmask != 0).
    (turn_ms, indices, new_masked, best_val)
}

/// Pick a letter for this state using the configured strategy.
fn pick_letter(
    strategy: &str,
    words: &[Vec<u8>],
    state: &State,
    dc: &DiskCache,
    rng: &mut u64,
) -> u8 {
    let legal: Vec<u8> = (0..26u8)
        .map(|li| b'a' + li)
        .filter(|&l| state.masked & letter_bit(l) == 0)
        .collect();
    if legal.is_empty() {
        return b'a';
    }
    match strategy {
        "random" => {
            let r = (lcg_next(rng) as usize) % legal.len();
            legal[r]
        }
        "least-common" => {
            // Pick the legal letter appearing in the fewest remaining words.
            // Deterministic, cache-independent; models the pessimal human
            // attacker whose every miss is reliably a miss.
            let mut best: Option<(u8, usize)> = None;
            for &letter in &legal {
                let count = state
                    .indices
                    .iter()
                    .filter(|&&i| words[i].contains(&letter))
                    .count();
                let take = match best {
                    None => true,
                    Some((_, cur)) => count < cur,
                };
                if take {
                    best = Some((letter, count));
                }
            }
            best.map_or(legal[0], |(l, _)| l)
        }
        "optimal" | "adversarial" => {
            let want_min = strategy == "optimal";
            let mut best: Option<(u8, u32)> = None;
            for &letter in &legal {
                let new_masked = state.masked | letter_bit(letter);
                let parts = partition_by_letter(words, &state.indices, letter);
                let mut worst: u32 = 0;
                for (pmask, indices) in &parts {
                    if indices.len() <= 1 {
                        continue;
                    }
                    let folded = fold_required_letters(words, indices, new_masked);
                    let hash = canonical_hash_for_words(words, indices, folded);
                    let v = dc.get(hash).map_or(99u32, |p| (p & 0x1F) + u32::from(*pmask == 0));
                    if v > worst {
                        worst = v;
                    }
                }
                let take = match best {
                    None => true,
                    Some((_, cur)) => {
                        if want_min { worst < cur } else { worst > cur }
                    }
                };
                if take {
                    best = Some((letter, worst));
                }
            }
            best.map_or(legal[0], |(l, _)| l)
        }
        _ => legal[0],
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dict = Dictionary::from_file(&cli.dict)?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    if words.is_empty() {
        anyhow::bail!("no words of length {}", cli.length);
    }
    println!("k={}: {} words", cli.length, words.len());

    let map_size = 256 * 1024 * 1024 * 1024;
    let dc = Arc::new(
        DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?
            .ok_or_else(|| anyhow::anyhow!("no cache"))?,
    );
    println!("cache entries: {}", dc.entry_count());
    let solver = MemoizedSolver::for_serving(words.clone(), Some(Arc::clone(&dc)));

    let mut rng = cli.seed;
    let warn_ms = cli.warn_secs * 1000;

    let mut all_turn_ms: Vec<u64> = Vec::new();
    let mut max_turn: (u64, String) = (0, String::new());
    let mut slow_turns: Vec<(u64, String)> = Vec::new();
    let mut timeouts: u64 = 0;
    let mut first_turn_ms: Vec<u64> = Vec::new();

    let run_start = Instant::now();
    for trace_i in 0..cli.traces {
        let mut state = State {
            indices: (0..words.len()).collect(),
            masked: 0,
            turns: 0,
        };
        let mut trace_log: Vec<(u8, u64)> = Vec::new();
        while state.indices.len() > 1 && state.turns < cli.max_turns {
            let letter = pick_letter(&cli.strategy, &words, &state, &dc, &mut rng);
            let (turn_ms, new_indices, new_masked, _worst_val) =
                simulate_turn(&words, &state, letter, &dc, &solver,
                              cli.turn_deadline_secs, cli.per_partition_secs);
            trace_log.push((letter, turn_ms));
            all_turn_ms.push(turn_ms);
            if state.turns == 0 {
                first_turn_ms.push(turn_ms);
            }
            let label = format!(
                "trace#{trace_i} turn#{} letter={} left={} -> {} ({}ms)",
                state.turns, letter as char, state.indices.len(),
                new_indices.len(), turn_ms
            );
            if turn_ms > max_turn.0 {
                max_turn = (turn_ms, label.clone());
            }
            if turn_ms >= cli.turn_deadline_secs * 1000 - 200 {
                timeouts += 1;
                slow_turns.push((turn_ms, label));
            } else if turn_ms >= warn_ms {
                slow_turns.push((turn_ms, label));
            }

            if cli.verbose || cli.traces == 1 {
                println!(
                    "  trace#{trace_i} turn#{} letter={} left={} -> {} ({}ms)",
                    state.turns, letter as char,
                    state.indices.len(), new_indices.len(), turn_ms,
                );
            }

            state.indices = new_indices;
            state.masked = new_masked;
            state.turns += 1;
        }
        if (trace_i + 1) % 10 == 0 {
            println!(
                "  [{}/{}] turns_so_far={} max_turn={}ms timeouts={}",
                trace_i + 1, cli.traces, all_turn_ms.len(), max_turn.0, timeouts
            );
        }
    }

    // Percentiles.
    all_turn_ms.sort_unstable();
    let n = all_turn_ms.len();
    let pct = |p: f64| -> u64 {
        if n == 0 { 0 } else { all_turn_ms[((n as f64 * p / 100.0) as usize).min(n - 1)] }
    };

    println!("\n=== summary ===");
    println!("traces:                {}", cli.traces);
    println!("strategy:              {}", cli.strategy);
    println!("turns total:           {}", n);
    println!("max per-turn time:     {}ms  [{}]", max_turn.0, max_turn.1);
    println!("p50 / p95 / p99:       {} / {} / {} ms", pct(50.0), pct(95.0), pct(99.0));
    println!("timeouts (>={}s):       {}", cli.turn_deadline_secs, timeouts);
    println!("turns >= {}s (slow):    {}", cli.warn_secs, slow_turns.len());
    println!("wall time:             {:.1}s", run_start.elapsed().as_secs_f64());

    if !slow_turns.is_empty() {
        slow_turns.sort_by(|a, b| b.0.cmp(&a.0));
        println!("\n=== slow turns (top 20) ===");
        for (ms, label) in slow_turns.iter().take(20) {
            let flag = if *ms >= cli.turn_deadline_secs * 1000 - 200 { "FAIL" } else { "slow" };
            println!("  {flag} {ms:>6}ms  {label}");
        }
    }

    Ok(())
}
