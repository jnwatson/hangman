#![allow(clippy::too_many_lines)]
//! Diagnostic: walk a path, find LOWER-bound positions, run
//! solve_position_smp on one, then check if it became EXACT. Tells us
//! whether the precompute's force-store mechanism actually fires.

use std::collections::HashMap;
use std::path::PathBuf;
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
#[command(name = "cache-repair-test")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short = 'k', long)]
    length: usize,

    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    #[arg(long)]
    path: String,

    /// If true, actually run solve_position_smp on the first LOWER position
    /// found, then re-check the cache state.
    #[arg(long, default_value_t = false)]
    repair: bool,
}

fn bound_name(bound: u32) -> &'static str {
    match bound {
        0 => "EXACT",
        1 => "LOWER",
        2 => "UPPER",
        _ => "?",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dict = Dictionary::from_file(&cli.dict)?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    let map_size: usize = 1024_usize * 1024 * 1024 * 1024;
    let dc = Arc::new(
        DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?
            .ok_or_else(|| anyhow::anyhow!("no cache"))?,
    );
    println!(
        "k={} dict={} cache={}",
        cli.length,
        words.len(),
        dc.entry_count()
    );

    // Walk the path, collecting LOWER positions at each step.
    let mut indices: Vec<usize> = (0..words.len()).collect();
    let mut masked: u32 = 0;

    let mut found: Option<(Vec<usize>, u32, u128)> = None;

    for letter_ch in cli.path.chars() {
        let letter = letter_ch as u8;
        let new_masked = masked | letter_bit(letter);
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }
        let mut sorted_parts: Vec<(u32, Vec<usize>)> = parts.into_iter().collect();
        sorted_parts.sort_by_key(|(_, ix)| std::cmp::Reverse(ix.len()));

        for (pmask, part_indices) in &sorted_parts {
            if part_indices.len() <= 1 {
                continue;
            }
            let folded = fold_required_letters(&words, part_indices, new_masked);
            let hash = canonical_hash_for_words(&words, part_indices, folded);
            if let Some(packed) = dc.get(hash) {
                let (val, _bl, bound) = decode_tt_entry_raw(packed);
                if bound != 0 && found.is_none() {
                    println!(
                        "[FOUND LOWER/UPPER] turn-letter={} pmask={:#06x} n={} {} v={} hash={:032x} new_masked={:026b}",
                        letter_ch,
                        pmask,
                        part_indices.len(),
                        bound_name(bound),
                        val,
                        hash,
                        new_masked
                    );
                    found = Some((part_indices.clone(), new_masked, hash));
                }
            }
        }
        let chosen = sorted_parts
            .into_iter()
            .max_by_key(|(_, ix)| ix.len())
            .unwrap();
        masked = new_masked;
        indices = chosen.1;
    }

    let (target_indices, target_masked, target_hash) = match found {
        Some(x) => x,
        None => {
            println!("no LOWER/UPPER positions found on this path");
            return Ok(());
        }
    };

    if !cli.repair {
        println!("Run with --repair to attempt fix");
        return Ok(());
    }

    println!(
        "\nBEFORE solve_position_smp: hash={:032x}",
        target_hash
    );
    if let Some(packed) = dc.get(target_hash) {
        let (val, _bl, bound) = decode_tt_entry_raw(packed);
        println!("  state = {} v={}", bound_name(bound), val);
    } else {
        println!("  state = ABSENT");
    }

    println!("\nRunning solve_position_smp...");
    let solver = MemoizedSolver::with_disk_cache(Arc::clone(&dc));
    let value = solver.solve_position_smp(&words, &target_indices, target_masked);
    println!("solve_position_smp returned value={}", value);

    println!("Flushing...");
    if let Some(res) = solver.flush_and_evict() {
        match res {
            Ok(n) => println!("flushed {n} entries"),
            Err(e) => println!("flush error: {e:#}"),
        }
    } else {
        println!("flush returned None");
    }

    println!("\nAFTER solve_position_smp: hash={:032x}", target_hash);
    if let Some(packed) = dc.get(target_hash) {
        let (val, _bl, bound) = decode_tt_entry_raw(packed);
        println!("  state = {} v={}", bound_name(bound), val);
        if bound == 0 {
            println!("\n✓ FIX WORKED — re-running precompute on LOWER positions converts them to EXACT");
        } else {
            println!("\n✗ STILL NOT EXACT — force-store didn't take effect; deeper bug");
        }
    } else {
        println!("  state = ABSENT (force-store may have used a different key)");
    }

    Ok(())
}
