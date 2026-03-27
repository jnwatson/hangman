//! Measure the storage/compute tradeoff for precomputing the game tree.
//!
//! Enumerates the game tree level by level (BFS). At each depth, reports:
//! - Number of unique canonical positions
//! - Max/median/p90 word-set sizes
//! - Solve time for the largest leaves (sampled)
//!
//! Positions are stored as compact bitvectors over the word list.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use rustc_hash::FxHashMap;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{canonical_hash_for_words, fold_required_letters, pos_mask};
use hangman2::solver::MemoizedSolver;

#[derive(Parser)]
#[command(name = "tree-depth")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long)]
    length: usize,

    /// Max depth to enumerate (default: 26)
    #[arg(long, default_value = "26")]
    max_depth: usize,

    /// Actually solve the N largest leaves at each depth to measure time
    #[arg(long, default_value = "3")]
    sample_solves: usize,

    /// Skip solve timing (just count positions)
    #[arg(long)]
    count_only: bool,
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

    /// Memory footprint in bytes (approximate).
    fn mem_bytes(&self) -> usize {
        self.bits.len() * 8 + 4
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
    println!("k={}: {} words ({} bytes/bitvec)\n", cli.length, n, (n + 63) / 64 * 8);

    if words.is_empty() {
        return Ok(());
    }

    let all_indices: Vec<usize> = (0..n).collect();
    let root_folded = fold_required_letters(&words, &all_indices, 0);
    let root_hash = canonical_hash_for_words(&words, &all_indices, root_folded);

    let root = Position {
        words: BitVec::from_indices(&all_indices, n),
        masked: root_folded,
    };

    println!(
        "{:>5}  {:>10}  {:>10}  {:>8}  {:>8}  {:>10}  {:>12}  {:>10}",
        "depth", "new_pos", "max_words", "p90", "median", "max_solve", "cumul_pos", "~mem_MB"
    );
    println!("{}", "-".repeat(95));

    let mut current_level: Vec<Position> = vec![root];
    let mut seen: HashSet<u128> = HashSet::new();
    seen.insert(root_hash);
    let mut cumulative = 1u64;
    let mut total_mem_bytes = current_level[0].words.mem_bytes() + 4;

    print_level_stats(&cli, &words, n, 0, &current_level, cumulative, total_mem_bytes);

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

                // Partition by pos_mask
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

        // Free previous level
        drop(current_level);

        if next_level.is_empty() {
            println!("  (no more positions at depth {depth})");
            break;
        }

        cumulative += next_level.len() as u64;
        let level_mem: usize = next_level.iter().map(|p| p.words.mem_bytes() + 4).sum();
        total_mem_bytes = seen.capacity() * 16 + level_mem; // seen + current level
        print_level_stats(&cli, &words, n, depth, &next_level, cumulative, total_mem_bytes);

        let elapsed = level_start.elapsed();
        eprintln!("  depth {depth} enumerated in {:.1}s", elapsed.as_secs_f64());

        current_level = next_level;
    }

    println!(
        "\nTotal unique positions: {cumulative} ({:.1} MB at 20 bytes/entry)",
        cumulative as f64 * 20.0 / 1_048_576.0
    );

    Ok(())
}

fn print_level_stats(
    cli: &Cli,
    words: &[Vec<u8>],
    num_words: usize,
    depth: usize,
    level: &[Position],
    cumulative: u64,
    mem_bytes: usize,
) {
    let mut sizes: Vec<usize> = level.iter().map(|p| p.words.len()).collect();
    sizes.sort_unstable();
    let max_words = *sizes.last().unwrap_or(&0);
    let median = sizes[sizes.len() / 2];
    let p90 = sizes[sizes.len() * 9 / 10];
    let mem_mb = mem_bytes as f64 / 1_048_576.0;

    let solve_time_str = if cli.count_only || level.is_empty() {
        "—".to_string()
    } else {
        // Find and solve the N largest positions
        let mut largest: Vec<(usize, usize)> = level
            .iter()
            .enumerate()
            .map(|(i, p)| (p.words.len(), i))
            .collect();
        largest.sort_unstable_by(|a, b| b.0.cmp(&a.0));

        let mut max_solve_ms = 0.0_f64;
        let sample_n = cli.sample_solves.min(largest.len());
        for &(sz, idx) in &largest[..sample_n] {
            if sz <= 1 {
                continue;
            }
            let subset_words: Vec<Vec<u8>> = level[idx]
                .words
                .to_indices()
                .iter()
                .map(|&i| words[i].clone())
                .collect();
            let solver = MemoizedSolver::new();
            let start = Instant::now();
            let _ = solver.solve(&subset_words);
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "    solve {} words: {:.1}ms",
                sz, elapsed_ms
            );
            max_solve_ms = max_solve_ms.max(elapsed_ms);
        }

        if max_solve_ms < 1000.0 {
            format!("{max_solve_ms:.0}ms")
        } else {
            format!("{:.1}s", max_solve_ms / 1000.0)
        }
    };

    println!(
        "{depth:>5}  {new:>10}  {max_w:>10}  {p90:>8}  {med:>8}  {solve:>10}  {cum:>12}  {mem:>10.1}",
        new = level.len(),
        max_w = max_words,
        p90 = p90,
        med = median,
        solve = solve_time_str,
        cum = cumulative,
        mem = mem_mb,
    );
}
