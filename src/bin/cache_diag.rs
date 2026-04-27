#![allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
//! Diagnostic: replay a sequence of guesses, partition each step, and report
//! the cache state of every resulting partition. Distinguishes ABSENT,
//! EXACT, UPPER-bound, LOWER-bound — so we can tell whether the precompute
//! reached a position at all and, if so, what bound type it stored.
//!
//! Usage:
//!   cache-diag --dict enable1.txt --length 6 --cache-dir /opt/dead-letters/cache --path qjxzv

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
#[command(name = "cache-diag")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short = 'k', long)]
    length: usize,

    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// Letters to guess in order, e.g. "qjxzv". After each guess we follow
    /// the largest partition (typically the miss).
    #[arg(long)]
    path: String,
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
    if words.is_empty() {
        anyhow::bail!("no words of length {}", cli.length);
    }
    let map_size: usize = 1024_usize * 1024 * 1024 * 1024;
    let dc = DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?
        .ok_or_else(|| anyhow::anyhow!("no cache at {}", cli.cache_dir.display()))?;
    println!(
        "k={} dict_size={} cache_entries={}",
        cli.length,
        words.len(),
        dc.entry_count()
    );

    let mut indices: Vec<usize> = (0..words.len()).collect();
    let mut masked: u32 = 0;

    for (turn, letter_ch) in cli.path.chars().enumerate() {
        let letter = letter_ch as u8;
        if !letter.is_ascii_lowercase() {
            anyhow::bail!("invalid letter {letter_ch}");
        }
        if masked & letter_bit(letter) != 0 {
            anyhow::bail!("letter {letter_ch} already guessed");
        }
        let new_masked = masked | letter_bit(letter);

        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }
        let mut sorted_parts: Vec<(u32, Vec<usize>)> = parts.into_iter().collect();
        sorted_parts.sort_by_key(|(_, ix)| std::cmp::Reverse(ix.len()));

        println!(
            "\n=== turn {} letter={} pre={} parts={} ===",
            turn,
            letter_ch,
            indices.len(),
            sorted_parts.len()
        );

        let mut absent = 0usize;
        let mut exact = 0usize;
        let mut bounded = 0usize;
        let mut trivial = 0usize;
        for (pmask, part_indices) in &sorted_parts {
            if part_indices.len() <= 1 {
                trivial += 1;
                continue;
            }
            let folded = fold_required_letters(&words, part_indices, new_masked);
            let hash = canonical_hash_for_words(&words, part_indices, folded);
            let raw = dc.get(hash);
            let (state_str, value_str) = match raw {
                None => {
                    absent += 1;
                    ("ABSENT".to_string(), String::new())
                }
                Some(packed) => {
                    let (val, _bl, bound) = decode_tt_entry_raw(packed);
                    if bound == 0 {
                        exact += 1;
                    } else {
                        bounded += 1;
                    }
                    (
                        format!("PRESENT {}", bound_name(bound)),
                        format!("v={}", val),
                    )
                }
            };
            let kind = if *pmask == 0 {
                "miss".to_string()
            } else {
                format!("hit({:#06x})", pmask)
            };
            println!(
                "  {kind:<14} n={:>5} folded={:08b} {state_str} {value_str}",
                part_indices.len(),
                folded
            );
        }
        println!(
            "  summary: {} EXACT, {} BOUND-only, {} ABSENT, {} trivial",
            exact, bounded, absent, trivial
        );

        let chosen = sorted_parts
            .into_iter()
            .max_by_key(|(_, ix)| ix.len())
            .unwrap();
        masked = new_masked;
        indices = chosen.1;
    }
    Ok(())
}
