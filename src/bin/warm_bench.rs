//! Benchmark solve times for gameplay positions with a warm TT.
//!
//! 1. Solve the root to populate the TT (this is the precompute step).
//! 2. For each of the 26 first-guess letters, partition the word set.
//! 3. For each resulting partition, measure warm vs cold solve time.
//!
//! This tells us: after the root solve, how fast can we handle arbitrary
//! first-move gameplay positions?

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use rustc_hash::FxHashMap;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::pos_mask;
use hangman2::solver::MemoizedSolver;

#[derive(Parser)]
#[command(name = "warm-bench")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long)]
    length: usize,

    /// Max partitions to actually solve per letter (default: 3, largest first)
    #[arg(long, default_value = "3")]
    sample: usize,

    /// Time budget per cold solve in seconds (skip if exceeds)
    #[arg(long, default_value = "30")]
    budget: u64,
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
    let root_value = solver.solve(&words);
    let root_elapsed = start.elapsed();
    println!(
        "Root solve: value={}, time={:.2}s, cache_size={}",
        root_value,
        root_elapsed.as_secs_f64(),
        solver.cache_size()
    );

    // Step 2: Enumerate first-move partitions and benchmark.
    let all_indices: Vec<usize> = (0..n).collect();
    let budget_ms = cli.budget as f64 * 1000.0;

    println!(
        "\n{:>3}  {:>6}  {:>8}  {:>10}  {:>10}  {:>6}",
        "ltr", "parts", "max_sz", "warm_ms", "cold_ms", "value"
    );
    println!("{}", "-".repeat(55));

    let mut overall_worst_warm = 0.0_f64;
    let mut overall_worst_cold = 0.0_f64;

    for li in 0..26u8 {
        let letter = b'a' + li;

        // Partition
        let mut partitions: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        for &idx in &all_indices {
            let mask = pos_mask(&words[idx], letter);
            partitions.entry(mask).or_default().push(idx);
        }

        let mut parts: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();
        parts.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

        let n_parts = parts.len();
        let max_sz = parts.first().map(|p| p.1.len()).unwrap_or(0);

        let mut worst_warm = 0.0_f64;
        let mut worst_cold = 0.0_f64;
        let mut worst_value = 0u32;
        let mut sampled = 0;

        for (pmask, subset) in &parts {
            if subset.len() <= 1 {
                continue;
            }
            if sampled >= cli.sample {
                break;
            }
            sampled += 1;

            let subset_words: Vec<Vec<u8>> =
                subset.iter().map(|&i| words[i].clone()).collect();

            // Warm solve: create a new solver seeded with the root solve's TT.
            let warm_solver = MemoizedSolver::new();
            warm_solver.copy_cache_from(&solver);
            let start = Instant::now();
            let val = warm_solver.solve(&subset_words);
            let warm_ms = start.elapsed().as_secs_f64() * 1000.0;

            // Cold solve: completely fresh solver.
            let cold_str = if warm_ms > budget_ms {
                "SKIP".to_string()
            } else {
                let cold_solver = MemoizedSolver::new();
                let start = Instant::now();
                let cold_val = cold_solver.solve(&subset_words);
                let cold_ms = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(val, cold_val, "warm/cold value mismatch!");
                worst_cold = worst_cold.max(cold_ms);
                format_time(cold_ms)
            };

            worst_warm = worst_warm.max(warm_ms);
            worst_value = worst_value.max(val);

            eprintln!(
                "  {} pmask={:#x} n={}: warm={} cold={} val={}",
                letter as char,
                pmask,
                subset.len(),
                format_time(warm_ms),
                cold_str,
                val
            );
        }

        if sampled > 0 {
            overall_worst_warm = overall_worst_warm.max(worst_warm);
            overall_worst_cold = overall_worst_cold.max(worst_cold);
            println!(
                "  {}  {:>6}  {:>8}  {:>10}  {:>10}  {:>6}",
                letter as char,
                n_parts,
                max_sz,
                format_time(worst_warm),
                if worst_cold > 0.0 {
                    format_time(worst_cold)
                } else {
                    "—".to_string()
                },
                worst_value
            );
        }
    }

    println!(
        "\nOverall worst: warm={}, cold={}",
        format_time(overall_worst_warm),
        format_time(overall_worst_cold)
    );

    Ok(())
}

fn format_time(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.1}s", ms / 1000.0)
    }
}
