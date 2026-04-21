#![deny(clippy::all, clippy::pedantic)]
#![allow(
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::uninlined_format_args,
    clippy::single_match_else,
    clippy::doc_markdown,
)]

//! Diagnostic: verify minimax consistency in the disk cache.
//!
//! For a given word length, this tool:
//! 1. Looks up the root state's cached minimax value.
//! 2. For each letter, partitions the root word set by that letter's pos_mask.
//! 3. Looks up each partition's cached minimax value.
//! 4. Computes `max_over_partitions (miss_cost + value)` — the worst case
//!    for the guesser when they play that letter.
//! 5. Computes `min_over_letters (max_above)` — the root minimax under
//!    optimal guesser play.
//!
//! If the cache is internally consistent, `cached_root_value == min_over_letters`.
//! Any mismatch means some cached entry is wrong OR some partition lookup
//! misses the cache (which would treat the partition as unbounded worst).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry_raw, fold_required_letters, pos_mask,
};

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long)]
    length: usize,

    #[arg(short, long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// If set, print the full partition breakdown for this specific letter.
    #[arg(long)]
    focus_letter: Option<String>,
}

fn bound_name(b: u32) -> &'static str {
    match b {
        0 => "EXACT",
        1 => "LOWER",
        2 => "UPPER",
        _ => "?",
    }
}

fn lookup(
    dc: &DiskCache,
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
) -> Option<(u32, Option<u8>, u32)> {
    let folded = fold_required_letters(words, indices, masked);
    let hash = canonical_hash_for_words(words, indices, folded);
    dc.get(hash).map(decode_tt_entry_raw)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    if words.is_empty() {
        anyhow::bail!("no words of length {}", cli.length);
    }
    println!("k={}: {} words", cli.length, words.len());

    let dc = DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, 256 * 1024 * 1024 * 1024)?
        .ok_or_else(|| anyhow::anyhow!("no cache found"))?;
    println!("cache entries: {}", dc.entry_count());

    let all: Vec<usize> = (0..words.len()).collect();
    let root = lookup(&dc, &words, &all, 0);
    let root_value: Option<u32> = match root {
        Some((v, bl, b)) => {
            println!(
                "ROOT: value={} bound={} best_letter={:?}",
                v,
                bound_name(b),
                bl.map(|c| c as char)
            );
            Some(v)
        }
        None => {
            println!("ROOT: not cached (precompute doesn't write depth-0)");
            None
        }
    };

    // For each letter, compute the max-over-partitions (what referee would pick).
    let focus = cli
        .focus_letter
        .as_ref()
        .and_then(|s| s.bytes().next().map(|b| b.to_ascii_lowercase()));

    let mut per_letter_max: Vec<(u8, Option<u32>)> = Vec::new();
    for li in 0..26u8 {
        let letter = b'a' + li;
        let new_masked = letter_bit(letter);

        // Partition by pos_mask for this letter.
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &all {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }

        let mut max_total: Option<u32> = None;
        let mut partition_details: Vec<(u32, usize, Option<(u32, Option<u8>, u32)>)> = Vec::new();

        for (pmask, indices) in &parts {
            let miss_cost = u32::from(*pmask == 0);
            let entry = if indices.len() <= 1 {
                Some((0, None, 0))
            } else {
                lookup(&dc, &words, indices, new_masked)
            };
            partition_details.push((*pmask, indices.len(), entry));

            match entry {
                Some((v, _, _bound)) => {
                    let total = miss_cost + v;
                    max_total = Some(max_total.map_or(total, |cur| cur.max(total)));
                }
                None => {
                    // Cache miss: would fallback-solve. Can't verify. Mark as None.
                    max_total = None;
                    break;
                }
            }
        }

        per_letter_max.push((letter, max_total));

        if focus == Some(letter) {
            println!("\n=== letter '{}' ===", letter as char);
            // Sort partitions miss-first, then descending-size
            partition_details.sort_by(|a, b| {
                let a_miss = u32::from(a.0 == 0);
                let b_miss = u32::from(b.0 == 0);
                b_miss.cmp(&a_miss).then(b.1.cmp(&a.1))
            });
            for (pmask, n, entry) in &partition_details {
                let kind = if *pmask == 0 { "miss " } else { "hit  " };
                match entry {
                    Some((v, bl, bound)) => {
                        let total =
                            u32::from(*pmask == 0) + v;
                        println!(
                            "  {} mask={:#06x}  n={:5}  value={:2} bound={:5} best={:?}  total={:2}",
                            kind,
                            pmask,
                            n,
                            v,
                            bound_name(*bound),
                            bl.map(|c| c as char),
                            total
                        );
                    }
                    None => {
                        println!("  {} mask={:#06x}  n={:5}  value=?  (NOT CACHED)", kind, pmask, n);
                    }
                }
            }
            println!("  max over partitions: {:?}", max_total);
        }
    }

    // Min over letters = inferred root value (under optimal guesser play).
    let all_cached: Vec<(u8, u32)> = per_letter_max
        .iter()
        .filter_map(|(l, v)| v.map(|v| (*l, v)))
        .collect();

    let any_missing = per_letter_max.iter().any(|(_, v)| v.is_none());
    println!("\n=== summary ===");
    if let Some(v) = root_value {
        println!("cached root value: {}", v);
    } else {
        println!("cached root value: (not in cache)");
    }
    if any_missing {
        let missing: Vec<char> = per_letter_max
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(l, _)| *l as char)
            .collect();
        println!("letters with unresolvable partitions (some sub-cache miss): {:?}", missing);
    }
    if !all_cached.is_empty() {
        let (min_letter, min_val) = all_cached.iter().min_by_key(|(_, v)| *v).copied().unwrap();
        println!(
            "min(max-over-partitions) across cached letters: {} (best guesser letter: '{}')",
            min_val, min_letter as char
        );
        if let Some(rv) = root_value {
            if min_val == rv {
                println!("✓ root value matches min-over-letters");
            } else {
                println!(
                    "\n⚠ INCONSISTENCY: cached root = {}, but min-over-letters = {}",
                    rv, min_val
                );
            }
        } else {
            println!("(no cached root to compare)");
        }
    }

    // Show all letters' max values for inspection
    println!("\n=== per-letter max (guesser picks the lowest) ===");
    let mut sorted = per_letter_max.clone();
    sorted.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    for (l, v) in &sorted {
        match v {
            Some(v) => println!("  '{}' → {}", *l as char, v),
            None => println!("  '{}' → ? (uncached partition)", *l as char),
        }
    }

    Ok(())
}
