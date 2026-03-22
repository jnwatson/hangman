#![deny(clippy::all, clippy::pedantic)]

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::solver::{DagSolver, MemoizedSolver, NaiveSolver};

#[derive(Parser)]
#[command(name = "bench", about = "Benchmark the hangman solver")]
struct Cli {
    /// Path to dictionary file
    #[arg(short, long)]
    dict: PathBuf,

    /// Word lengths to benchmark (comma-separated). Default: 2,3,4
    #[arg(short, long, value_delimiter = ',')]
    lengths: Option<Vec<usize>>,

    /// Also run the naive solver for oracle comparison (slow!)
    #[arg(long)]
    naive: bool,

    /// Also run the DAG solver for comparison
    #[arg(long)]
    dag: bool,

    /// Maximum word count to attempt (skip lengths with more words)
    #[arg(long, default_value = "10000")]
    max_words: usize,

    /// Sample N words per length (use full set if omitted)
    #[arg(short, long)]
    sample: Option<usize>,

    /// Estimate runtime by iterative deepening instead of full solve
    #[arg(long)]
    estimate: bool,

    /// Time budget in seconds for estimation (default: 60)
    #[arg(long, default_value = "60")]
    estimate_budget: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words\n", dict.total_words());

    let lengths = cli.lengths.clone().unwrap_or_else(|| vec![2, 3, 4]);

    if cli.estimate {
        run_estimate(&cli, &dict, &lengths);
    } else {
        run_benchmark(&cli, &dict, &lengths);
    }

    Ok(())
}

fn run_estimate(cli: &Cli, dict: &Dictionary, lengths: &[usize]) {
    for len in lengths {
        let all_words = dict.words_of_length(*len);
        if all_words.is_empty() {
            println!("Length {len}: no words");
            continue;
        }

        let words: Vec<Vec<u8>> = if let Some(n) = cli.sample {
            all_words.iter().take(n).cloned().collect()
        } else {
            all_words.to_vec()
        };

        if words.len() > cli.max_words {
            println!(
                "Length {len}: {} words (skipped, > {})",
                words.len(),
                cli.max_words
            );
            continue;
        }

        println!("Length {len}: {} words", words.len());
        println!(
            "  {:>5} {:>12} {:>12} {:>10} {:>10} {:>8}",
            "depth", "time", "cumul", "hash_call", "cache_hit", "result"
        );
        println!("  {}", "-".repeat(65));

        let solver = MemoizedSolver::new();
        let mut prev_secs = 0.0_f64;
        let mut cumul_time = 0.0_f64;
        let mut ratios: Vec<f64> = Vec::new();
        let mut is_solved = false;
        let mut last_depth = 0u32;
        let budget = std::time::Duration::from_secs(cli.estimate_budget);
        let wall_start = Instant::now();

        for depth in 1..=25 {
            let start = Instant::now();
            let result = solver.solve_bounded(&words, depth);
            let elapsed = start.elapsed();
            let secs = elapsed.as_secs_f64();
            cumul_time += secs;
            last_depth = depth;

            let hash_calls = solver.hash_calls();
            let cache_hits = solver.cache_hits();

            let status = if result <= depth {
                is_solved = true;
                format!("= {result}")
            } else {
                format!("> {depth}")
            };

            println!(
                "  {depth:>5} {secs:>12.3}s {cumul_time:>12.3}s {hash_calls:>10} {cache_hits:>10} {status:>8}",
            );

            if is_solved {
                println!("\n  Solved! Optimal misses = {result}");
                break;
            }

            if secs > 0.001 && prev_secs > 0.001 {
                ratios.push(secs / prev_secs);
            }
            prev_secs = secs;

            // Stop probing if we've exceeded the time budget.
            if wall_start.elapsed() >= budget {
                println!("  (time budget of {}s reached)", cli.estimate_budget);
                break;
            }
        }

        if !is_solved {
            print_estimate(&ratios, prev_secs, cumul_time, last_depth);
        }
        println!();
    }
}

fn print_estimate(ratios: &[f64], last_time: f64, cumul_time: f64, last_depth: u32) {
    if ratios.is_empty() {
        println!("\n  Not enough data to extrapolate (need at least 3 measured depths).");
        return;
    }

    // Use geometric mean of ratios as the branching factor estimate.
    let ln_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
    #[allow(clippy::cast_precision_loss)]
    let geo_mean = (ln_sum / ratios.len() as f64).exp();

    // Also show the last ratio for trend comparison.
    let last_ratio = ratios.last().copied().unwrap_or(geo_mean);

    println!("\n  Branching factor: {geo_mean:.2}x (geometric mean), last ratio: {last_ratio:.2}x");
    println!("  Extrapolated solve times (cumulative, from depth {last_depth}):");
    println!(
        "  {:>5} {:>14}  {:>14}",
        "depth", "conservative", "optimistic"
    );

    // Conservative: use the larger of geo_mean and last_ratio.
    let factor_conservative = geo_mean.max(last_ratio);
    // Optimistic: use the smaller.
    let factor_optimistic = geo_mean.min(last_ratio);

    let mut depth_t_con = last_time;
    let mut depth_t_opt = last_time;
    let mut cumul_con = cumul_time;
    let mut cumul_opt = cumul_time;

    for d in (last_depth + 1)..=(last_depth + 10) {
        depth_t_con *= factor_conservative;
        depth_t_opt *= factor_optimistic;
        cumul_con += depth_t_con;
        cumul_opt += depth_t_opt;
        println!(
            "  {:>5} {:>14} {:>14}",
            d,
            format_duration(cumul_con),
            format_duration(cumul_opt),
        );
    }
}

fn format_duration(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{:.1}m", secs / 60.0)
    } else if secs < 86400.0 {
        format!("{:.1}h", secs / 3600.0)
    } else {
        format!("{:.1}d", secs / 86400.0)
    }
}

fn run_benchmark(cli: &Cli, dict: &Dictionary, lengths: &[usize]) {
    if cli.dag {
        println!(
            "{:>6} {:>8} {:>8} {:>12} {:>8} {:>12} {:>8}",
            "length", "words", "misses", "dag_time", "dag_$", "memo_time", "memo_$"
        );
    } else {
        println!(
            "{:>6} {:>8} {:>8} {:>12} {:>8} {:>10} {:>10}",
            "length", "words", "misses", "memo_time", "memo_$", "hash_call", "cache_hit"
        );
    }
    println!("{}", "-".repeat(78));

    for len in lengths {
        let all_words = dict.words_of_length(*len);
        if all_words.is_empty() {
            println!("{len:>6} {:>8} (no words)", 0);
            continue;
        }

        let words: Vec<Vec<u8>> = if let Some(n) = cli.sample {
            all_words.iter().take(n).cloned().collect()
        } else {
            all_words.to_vec()
        };

        if words.len() > cli.max_words {
            println!(
                "{:>6} {:>8} (skipped, > {} words)",
                len,
                words.len(),
                cli.max_words
            );
            continue;
        }

        // DAG solver (optional)
        let mut dag_misses = None;
        let mut dag_elapsed = std::time::Duration::ZERO;
        let mut dag_cache = 0;
        if cli.dag {
            let start = Instant::now();
            let dag = DagSolver::new(words.clone());
            let misses = dag.solve();
            dag_elapsed = start.elapsed();
            dag_cache = dag.cache_size();
            dag_misses = Some(misses);
        }

        // Memoized solver
        let start = Instant::now();
        let memo = MemoizedSolver::new();
        let memo_misses = memo.solve(&words);
        let memo_elapsed = start.elapsed();
        let memo_cache = memo.cache_size();

        if let Some(dm) = dag_misses {
            assert_eq!(
                dm, memo_misses,
                "MISMATCH at length {len}: dag={dm} != memo={memo_misses}"
            );
        }

        // Naive solver (optional)
        if cli.naive {
            let refs: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
            let naive_misses = NaiveSolver::solve(&refs, 0);
            assert_eq!(
                memo_misses, naive_misses,
                "MISMATCH at length {len}: memo={memo_misses} != naive={naive_misses}"
            );
        }

        let hash_calls = memo.hash_calls();
        let cache_hits = memo.cache_hits();
        if cli.dag {
            println!(
                "{:>6} {:>8} {:>8} {:>12.2?} {:>8} {:>12.2?} {:>8}",
                len,
                words.len(),
                memo_misses,
                dag_elapsed,
                dag_cache,
                memo_elapsed,
                memo_cache,
            );
        } else {
            println!(
                "{:>6} {:>8} {:>8} {:>12.2?} {:>8} {:>10} {:>10}",
                len,
                words.len(),
                memo_misses,
                memo_elapsed,
                memo_cache,
                hash_calls,
                cache_hits,
            );
        }
    }
}
