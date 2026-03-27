#![deny(clippy::all, clippy::pedantic)]
//! Measure TT coverage: after a root solve, what fraction of game-tree
//! positions at each depth are already in the transposition table?
//!
//! This is the key metric for deciding whether runtime solving with a
//! TT-seeded cache is viable for a given word length.
//!
//! Flow:
//! 1. Solve the root to populate the TT.
//! 2. BFS the game tree level by level.
//! 3. At each position, compute the canonical key and check the TT.
//! 4. Report: total positions, exact hits, bound hits, misses per depth.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use rustc_hash::FxHashMap;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{canonical_hash_for_words, decode_tt_entry_raw, fold_required_letters, pos_mask};
use hangman2::solver::MemoizedSolver;

#[derive(Parser)]
#[command(name = "tt-coverage")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long)]
    length: usize,

    /// Max depth to enumerate (default: 10)
    #[arg(long, default_value = "10")]
    max_depth: usize,

    /// Use solve_bounded with this max-misses instead of full solve
    #[arg(long)]
    bounded: Option<u32>,

    /// Also time the N largest TT-miss positions per depth to estimate
    /// runtime solve cost
    #[arg(long, default_value = "0")]
    sample_solves: usize,

    /// Budget per sample solve in seconds
    #[arg(long, default_value = "10")]
    solve_budget: u64,
}

/// Compact bitvector representing a subset of the word list.
#[derive(Clone)]
struct BitVec {
    bits: Vec<u64>,
    count: u32,
}

impl BitVec {
    fn new(num_words: usize) -> Self {
        Self {
            bits: vec![0u64; (num_words + 63) / 64],
            count: 0,
        }
    }

    fn from_indices(indices: &[usize], num_words: usize) -> Self {
        let mut bv = Self::new(num_words);
        for &i in indices {
            bv.bits[i / 64] |= 1u64 << (i % 64);
        }
        bv.count = indices.len() as u32;
        bv
    }

    fn len(&self) -> usize {
        self.count as usize
    }

    fn to_indices(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.count as usize);
        for (chunk_idx, &chunk) in self.bits.iter().enumerate() {
            let mut c = chunk;
            while c != 0 {
                let bit = c.trailing_zeros() as usize;
                out.push(chunk_idx * 64 + bit);
                c &= c - 1;
            }
        }
        out
    }
}

struct Position {
    words: BitVec,
    masked: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    let words = dict.words_of_length(cli.length).to_vec();
    let n = words.len();
    println!("k={}: {} words\n", cli.length, n);

    if words.is_empty() {
        return Ok(());
    }

    // Step 1: Solve root to populate TT.
    let solver = MemoizedSolver::new();
    let start = Instant::now();
    let root_value = if let Some(max) = cli.bounded {
        solver.solve_bounded(&words, max)
    } else {
        solver.solve(&words)
    };
    let root_elapsed = start.elapsed();
    println!(
        "Root solve: value={}, time={:.2}s, cache_size={}",
        root_value,
        root_elapsed.as_secs_f64(),
        solver.cache_size()
    );

    // Step 2: BFS the game tree, checking TT coverage at each depth.
    let all_indices: Vec<usize> = (0..n).collect();
    let root_folded = fold_required_letters(&words, &all_indices, 0);
    let root_hash = canonical_hash_for_words(&words, &all_indices, root_folded);

    let root = Position {
        words: BitVec::from_indices(&all_indices, n),
        masked: root_folded,
    };

    println!(
        "\n{:>5}  {:>10}  {:>10}  {:>10}  {:>10}  {:>8}  {:>8}  {:>10}",
        "depth", "positions", "exact", "lower", "upper", "miss", "hit%", "max_words"
    );
    println!("{}", "-".repeat(90));

    let mut current_level: Vec<Position> = vec![root];
    let mut seen: HashSet<u128> = HashSet::new();
    seen.insert(root_hash);

    // Check root coverage
    check_tt_coverage(&cli, &solver, &words, n, 0, &current_level, &seen);

    for depth in 1..=cli.max_depth {
        let level_start = Instant::now();
        let mut next_level: Vec<Position> = Vec::new();

        for pos in &current_level {
            let indices = pos.words.to_indices();

            for li in 0..26u8 {
                if pos.masked & (1 << li) != 0 {
                    continue;
                }
                let letter = b'a' + li;
                let new_masked = pos.masked | letter_bit(letter);

                let mut partitions: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
                for &idx in &indices {
                    let mask = pos_mask(&words[idx], letter);
                    partitions.entry(mask).or_default().push(idx);
                }

                for (_, subset) in &partitions {
                    if subset.len() <= 1 {
                        continue;
                    }
                    let folded = fold_required_letters(&words, subset, new_masked);
                    let hash = canonical_hash_for_words(&words, subset, folded);
                    if seen.insert(hash) {
                        next_level.push(Position {
                            words: BitVec::from_indices(subset, n),
                            masked: folded,
                        });
                    }
                }
            }
        }

        drop(current_level);

        if next_level.is_empty() {
            println!("  (no more positions at depth {depth})");
            break;
        }

        check_tt_coverage(&cli, &solver, &words, n, depth, &next_level, &seen);

        let elapsed = level_start.elapsed();
        eprintln!("  depth {depth} enumerated in {:.1}s", elapsed.as_secs_f64());

        current_level = next_level;
    }

    println!(
        "\nTotal unique positions seen: {}",
        seen.len()
    );

    Ok(())
}

fn check_tt_coverage(
    cli: &Cli,
    solver: &MemoizedSolver,
    words: &[Vec<u8>],
    num_words: usize,
    depth: usize,
    level: &[Position],
    _seen: &HashSet<u128>,
) {
    let cache = solver.cache();
    let mut exact = 0u64;
    let mut lower = 0u64;
    let mut upper = 0u64;
    let mut miss = 0u64;
    let mut max_words = 0usize;
    let mut miss_positions: Vec<(usize, usize)> = Vec::new(); // (word_count, index)

    for (i, pos) in level.iter().enumerate() {
        let indices = pos.words.to_indices();
        let folded = fold_required_letters(words, &indices, pos.masked);
        let hash = canonical_hash_for_words(words, &indices, folded);
        max_words = max_words.max(pos.words.len());

        if let Some(packed) = cache.get(&hash) {
            let (_, _, bound) = decode_tt_entry_raw(*packed);
            match bound {
                0 => exact += 1,
                1 => lower += 1,
                2 => upper += 1,
                _ => miss += 1,
            }
        } else {
            miss += 1;
            if cli.sample_solves > 0 {
                miss_positions.push((pos.words.len(), i));
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let total = level.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let hit_pct = (exact + lower + upper) as f64 / total * 100.0;

    println!(
        "{depth:>5}  {total:>10.0}  {exact:>10}  {lower:>10}  {upper:>10}  {miss:>8}  {hit_pct:>7.1}%  {max_words:>10}",
    );

    // Optionally solve the largest TT-miss positions to estimate runtime cost.
    if cli.sample_solves > 0 && !miss_positions.is_empty() {
        miss_positions.sort_by(|a, b| b.0.cmp(&a.0));
        let n_samples = cli.sample_solves.min(miss_positions.len());
        let budget_ms = cli.solve_budget as f64 * 1000.0;

        for &(sz, idx) in &miss_positions[..n_samples] {
            let subset_words: Vec<Vec<u8>> = level[idx]
                .words
                .to_indices()
                .iter()
                .map(|&i| words[i].clone())
                .collect();
            let cold_solver = MemoizedSolver::new();
            // Seed with all entries from the root solve.
            cold_solver.copy_cache_from(solver);
            let start = Instant::now();
            let val = cold_solver.solve(&subset_words);
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let status = if elapsed_ms > budget_ms { " SLOW" } else { "" };
            eprintln!(
                "    miss @ depth {depth}: {sz} words, val={val}, warm_solve={:.0}ms{status}",
                elapsed_ms
            );
        }
    }
}
