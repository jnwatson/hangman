#![deny(clippy::all, clippy::pedantic)]

//! Quick benchmark for runtime serving latency at various word counts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::{DiskCache, MemoizedSolver};

#[derive(Parser)]
#[command(name = "serve_bench", about = "Benchmark runtime serving latency")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long)]
    length: usize,

    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// Number of miss letters to skip before testing (to reach smaller word sets)
    #[arg(long, default_value = "4")]
    skip_misses: usize,
}

fn partition_by_letter(
    words: &[Vec<u8>],
    indices: &[usize],
    letter: u8,
) -> Vec<(u32, Vec<usize>)> {
    let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
    for &i in indices {
        let mut pmask = 0u32;
        for (j, &b) in words[i].iter().enumerate() {
            if b == letter {
                pmask |= 1 << j;
            }
        }
        parts.entry(pmask).or_default().push(i);
    }
    let mut v: Vec<_> = parts.into_iter().collect();
    v.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    v
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    println!("k={}: {} words", cli.length, words.len());

    let map_size = 16 * 1024 * 1024 * 1024;
    let disk = DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?;
    if let Some(ref dc) = disk {
        println!("Disk cache: {} entries", dc.entry_count());
    } else {
        println!("No disk cache found");
    }

    let solver = Arc::new(MemoizedSolver::for_serving(
        words.clone(),
        disk.map(Arc::new),
    ));

    // Walk down the adversarial path using frequency-order guesses,
    // taking the largest partition at each step (simplified adversarial).
    let guess_order = b"esiaronltudcpmhgbfywkvxzjq";
    let all_indices: Vec<usize> = (0..words.len()).collect();
    let mut current_indices = all_indices;
    let mut masked: u32 = 0;
    let mut misses = 0u32;

    // Skip the first N misses to reach a smaller word set
    println!("\nSkipping first {} miss-guesses...", cli.skip_misses);
    let mut skip_remaining = cli.skip_misses;
    let mut guess_iter = guess_order.iter();

    while skip_remaining > 0 {
        let Some(&letter) = guess_iter.next() else { break };
        masked |= letter_bit(letter);
        let parts = partition_by_letter(&words, &current_indices, letter);
        // Take largest partition (adversarial)
        let (pmask, indices) = parts.into_iter().max_by_key(|(_, v)| v.len()).unwrap();
        let is_miss = pmask == 0;
        if is_miss {
            misses += 1;
            skip_remaining -= 1;
        }
        println!(
            "  skip '{}':{} => {} words",
            letter as char,
            if is_miss { " MISS" } else { " HIT " },
            indices.len()
        );
        current_indices = indices;
    }

    println!(
        "\nStarting position: {} words, {} misses, masked={masked:#010b}",
        current_indices.len(),
        misses
    );

    // Now time each subsequent move
    println!("\n{:>7} {:>4} {:>6} {:>5} {:>10}", "guess", "type", "words", "value", "time");
    println!("{}", "-".repeat(40));

    for &letter in guess_iter {
        if masked & letter_bit(letter) != 0 {
            continue;
        }
        let ch = letter as char;
        masked |= letter_bit(letter);

        let parts = partition_by_letter(&words, &current_indices, letter);
        let (worst_pmask, worst_indices) = parts.into_iter().max_by_key(|(_, v)| v.len()).unwrap();
        let is_miss = worst_pmask == 0;
        if is_miss {
            misses += 1;
        }

        let nwords = worst_indices.len();
        let start = Instant::now();
        let val = solver.solve_position(&worst_indices, masked);
        let elapsed = start.elapsed();

        let miss_str = if is_miss { "MISS" } else { "HIT " };
        println!("  '{ch}'   {miss_str} {nwords:>6} {val:>5} {elapsed:>10.2?}");

        current_indices = worst_indices;
        if current_indices.len() <= 1 {
            println!("\nGame over! Total misses: {misses}");
            break;
        }
    }

    println!("\nIn-memory cache: {} entries", solver.cache_size());
    Ok(())
}
