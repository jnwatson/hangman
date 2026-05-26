//! Replicate the hint endpoint's exact logic against a disk cache.
//! For a given game state, evaluate each unguessed letter's worst-case
//! using the same cache-lookup + live-solve path the server uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::MemoizedSolver;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry, fold_required_letters, pos_mask,
};

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "enable1.txt")]
    dict: PathBuf,
    #[arg(short = 'k', long)]
    length: usize,
    /// Comma-separated steps: letter:hex_pmask (e.g. "a:0,b:0,i:0x22")
    #[arg(long)]
    path: String,
    #[arg(long)]
    cache_dir: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dict = Dictionary::from_file(Path::new(&cli.dict))?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    let map_size: usize = 1024 * 1024 * 1024 * 1024;
    let dc = Arc::new(
        DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?
            .ok_or_else(|| anyhow::anyhow!("no cache"))?,
    );
    println!("k={} words={} cache={}", cli.length, words.len(), dc.entry_count());

    let solver = MemoizedSolver::for_serving(words.clone(), Some(Arc::clone(&dc)));

    // Walk path to reach target state.
    let steps: Vec<(u8, u32)> = cli.path.split(',').map(|s| {
        let parts: Vec<&str> = s.split(':').collect();
        let letter = parts[0].as_bytes()[0];
        let pmask = u32::from_str_radix(parts[1].trim_start_matches("0x"), 16).unwrap();
        (letter, pmask)
    }).collect();

    let mut indices: Vec<usize> = (0..words.len()).collect();
    let mut masked: u32 = 0;
    for (letter, target_pmask) in &steps {
        masked |= letter_bit(*letter);
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], *letter);
            parts.entry(m).or_default().push(idx);
        }
        let chosen = parts.remove(target_pmask).unwrap();
        println!("  {}/{:#06x} → {} words", *letter as char, target_pmask, chosen.len());
        indices = chosen;
    }
    println!("final state: {} words, masked={masked:026b}\n", indices.len());

    // Replicate hint endpoint logic exactly.
    let mut evals: Vec<(u8, Option<u32>, &str)> = Vec::new();
    let mut needs_solve: Vec<(u8, Vec<(u32, Vec<usize>)>)> = Vec::new();

    for letter in b'a'..=b'z' {
        if masked & letter_bit(letter) != 0 { continue; }
        let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            partitions.entry(m).or_default().push(idx);
        }
        let new_masked = masked | letter_bit(letter);
        let mut worst_value: u32 = 0;
        let mut all_cached = true;

        for (&pmask, part_indices) in &partitions {
            if part_indices.len() <= 1 {
                let miss_cost = u32::from(pmask == 0);
                worst_value = worst_value.max(miss_cost);
                continue;
            }
            let miss_cost = u32::from(pmask == 0);
            let folded = fold_required_letters(&words, part_indices, new_masked);
            let hash = canonical_hash_for_words(&words, part_indices, folded);
            let cached = dc.get(hash).and_then(decode_tt_entry).map(|e| e.value);

            if let Some(v) = cached {
                worst_value = worst_value.max(miss_cost + v);
            } else {
                all_cached = false;
            }
        }

        if all_cached {
            evals.push((letter, Some(worst_value), "cached"));
        } else {
            let parts: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();
            needs_solve.push((letter, parts));
        }
    }

    // Live-solve uncached letters (same as hint endpoint).
    let deadline = Instant::now() + Duration::from_secs(30);
    for (letter, parts) in needs_solve {
        let new_masked = masked | letter_bit(letter);
        let mut worst: u32 = 0;
        let mut was_cancelled = false;
        for (pmask, part_indices) in &parts {
            let miss_cost = u32::from(*pmask == 0);
            if part_indices.len() <= 1 {
                worst = worst.max(miss_cost);
                continue;
            }
            let (value, cancelled) = solver.solve_position_with_deadline(part_indices, new_masked, Some(deadline));
            if cancelled {
                was_cancelled = true;
                break;
            }
            worst = worst.max(miss_cost + value);
        }
        if was_cancelled {
            evals.push((letter, None, "TIMEOUT"));
        } else {
            evals.push((letter, Some(worst), "live"));
        }
    }

    // Detail: for each letter, show per-partition cache values.
    println!("\nPer-partition detail for all letters:");
    for letter in b'a'..=b'z' {
        if masked & letter_bit(letter) != 0 { continue; }
        let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            partitions.entry(m).or_default().push(idx);
        }
        let new_masked = masked | letter_bit(letter);
        let mut parts_info: Vec<String> = Vec::new();
        for (&pmask, part_indices) in &partitions {
            let miss_cost = u32::from(pmask == 0);
            if part_indices.len() <= 1 {
                parts_info.push(format!("p{pmask:#x}n{}triv", part_indices.len()));
                continue;
            }
            let folded = fold_required_letters(&words, part_indices, new_masked);
            let hash = canonical_hash_for_words(&words, part_indices, folded);
            let cached = dc.get(hash).and_then(decode_tt_entry).map(|e| e.value);
            let fresh_v = MemoizedSolver::new().solve_position_smp(&words, part_indices, new_masked);
            let marker = if cached == Some(fresh_v) { "" } else { "!!!" };
            parts_info.push(format!(
                "p{pmask:#x}n{}mc{miss_cost}cache={cached:?}fresh={fresh_v}hash={hash:032x}{marker}",
                part_indices.len()
            ));
        }
        parts_info.sort();
        println!("  {}: {}", letter as char, parts_info.join(" | "));
    }

    // Also run a FRESH solve (no cache) for comparison.
    let fresh_solver = MemoizedSolver::new();
    let fresh_v = fresh_solver.solve_position_smp(&words, &indices, masked);

    println!("Per-letter (hint-endpoint logic):");
    evals.sort_by_key(|e| e.1.unwrap_or(u32::MAX));
    for (letter, value, source) in &evals {
        let star = if *value == Some(fresh_v) { "" } else { " ←" };
        println!("  {}: worst={:?} [{}]{star}", *letter as char, value, source);
    }
    let best = evals.iter().filter_map(|e| e.1).min().unwrap_or(u32::MAX);
    println!("\nHint-endpoint min: {best}");
    println!("Fresh solver V:    {fresh_v}");
    println!("Match: {}", if best == fresh_v { "YES" } else { "NO ← BUG" });
    Ok(())
}
