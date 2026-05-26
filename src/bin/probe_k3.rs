//! Investigate the k=3 wrong-EXACT bug at the path
//! "A (hit pmask=0x02), then B,C,D,E,F,G,H,M,P,R,S,W,T,Y,Z,V all misses".
//!
//! At the final state (pre-I), the prod cache claims V=1 but the smoke test
//! expects V=0. This probe:
//!   1. Walks the same path on a fresh solver (no disk cache)
//!   2. Reports the live solver's V at each step
//!   3. Computes per-letter worst-case at the final state
//!   4. Optionally compares to the prod disk cache value

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::MemoizedSolver;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry_raw, fold_required_letters, pos_mask,
};

#[derive(Parser)]
struct Cli {
    /// Path to dictionary (enable1.txt)
    #[arg(long, default_value = "enable1.txt")]
    dict: PathBuf,
    /// Word length
    #[arg(short = 'k', long, default_value_t = 3)]
    length: usize,
    /// Comma-separated steps; each step is letter:hex_pmask, e.g. "a:2,b:0,c:0,..."
    /// pmask 0 means miss. e.g. for pattern _A_, A's pmask=0x2.
    #[arg(long)]
    path: String,
    /// Optional disk cache to compare against
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dict = Dictionary::from_file(Path::new(&cli.dict))?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    println!("loaded {} k={} words", words.len(), cli.length);

    // Parse path.
    let steps: Vec<(u8, u32)> = cli
        .path
        .split(',')
        .map(|s| {
            let parts: Vec<&str> = s.split(':').collect();
            let letter = parts[0].as_bytes()[0];
            let pmask = u32::from_str_radix(parts[1].trim_start_matches("0x"), 16).unwrap();
            (letter, pmask)
        })
        .collect();

    let dc = if let Some(cache_dir) = &cli.cache_dir {
        let map_size: usize = 1024_usize * 1024 * 1024 * 1024;
        DiskCache::open_if_exists(cache_dir, cli.length, &words, map_size)?
            .map(Arc::new)
    } else {
        None
    };
    if let Some(dc) = &dc {
        println!("disk cache: {} entries", dc.entry_count());
    }

    // Walk path.
    let mut indices: Vec<usize> = (0..words.len()).collect();
    let mut masked: u32 = 0;
    for (step_i, (letter, target_pmask)) in steps.iter().enumerate() {
        masked |= letter_bit(*letter);
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], *letter);
            parts.entry(m).or_default().push(idx);
        }
        let chosen = parts.remove(target_pmask).unwrap_or_else(|| {
            panic!(
                "step {}: letter {:?} pmask 0x{:x} has no words; available pmasks: {:?}",
                step_i, *letter as char, target_pmask,
                parts.keys().collect::<Vec<_>>()
            )
        });
        // Show cache value at this position
        let folded = fold_required_letters(&words, &chosen, masked);
        let hash = canonical_hash_for_words(&words, &chosen, folded);
        let cached_v = dc.as_ref().and_then(|dc| dc.get(hash)).map(|p| {
            let (v, _, b) = decode_tt_entry_raw(p);
            (v, b)
        });
        let bound_name = match cached_v.map(|(_, b)| b) {
            Some(0) => "EXACT",
            Some(1) => "LOWER",
            Some(2) => "UPPER",
            Some(_) => "?",
            None => "ABSENT",
        };
        print!(
            "step {step_i}: {}/{:#06x} → {} words masked={masked:026b} cache={bound_name}",
            *letter as char,
            target_pmask,
            chosen.len()
        );
        if let Some((v, _)) = cached_v {
            print!(" v={v}");
        }
        println!();
        indices = chosen;
    }

    // Fresh solve.
    println!("\n--- Final state details ---");
    println!("words remaining ({}):", indices.len());
    for &i in &indices {
        let w: String = words[i].iter().map(|&b| b as char).collect();
        println!("  [{i}] {w}");
    }

    // Print the canonical hash of the final state for reproducibility.
    let folded = fold_required_letters(&words, &indices, masked);
    let hash = canonical_hash_for_words(&words, &indices, folded);
    println!("canonical hash = {hash:032x}");

    println!("\n--- Fresh solve of final state (no cache) ---");
    let fresh = MemoizedSolver::new().solve_position_smp(&words, &indices, masked);
    println!("Fresh V = {fresh}");

    // Repeated runs to check for nondeterminism.
    println!("\n--- 5 more fresh solves to check determinism ---");
    for i in 0..5 {
        let v = MemoizedSolver::new().solve_position_smp(&words, &indices, masked);
        println!("run {}: V = {v}", i + 2);
    }

    // Per-letter enumeration with fresh solvers.
    println!("\nPer-letter (fresh):");
    let mut min_worst = u32::MAX;
    let mut best_letter = 0u8;
    for letter in b'a'..=b'z' {
        if masked & letter_bit(letter) != 0 {
            continue;
        }
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }
        let new_masked = masked | letter_bit(letter);
        let mut worst: u32 = 0;
        for (pmask, sub) in &parts {
            let miss_cost = u32::from(*pmask == 0);
            let v = if sub.len() <= 1 {
                0
            } else {
                MemoizedSolver::new().solve_position_smp(&words, sub, new_masked)
            };
            worst = worst.max(miss_cost + v);
        }
        if worst < min_worst {
            min_worst = worst;
            best_letter = letter;
        }
        println!("  {}: worst={worst}", letter as char);
    }
    println!(
        "\nFresh per-letter min: {} (letter={})",
        min_worst,
        best_letter as char
    );
    println!("Direct solve V:        {fresh}");
    println!(
        "Match: {}",
        if min_worst == fresh { "YES" } else { "NO" }
    );
    Ok(())
}
