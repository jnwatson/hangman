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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words\n", dict.total_words());

    let lengths = cli.lengths.unwrap_or_else(|| vec![2, 3, 4]);

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

    for len in &lengths {
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

    Ok(())
}
