#![deny(clippy::all, clippy::pedantic)]

//! Precompute the first two levels of the game tree for fast serving.
//!
//! For each word length, enumerate all 26 first-guess partitions and all
//! 26×25 second-guess partitions. Solve each with full SMP parallelism
//! and flush results to the shared LMDB disk cache.
//!
//! Positions already present (EXACT) in the disk cache are skipped,
//! making the binary resumable after interruption.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry, fold_required_letters,
};
use hangman2::solver::{DiskCache, MemoizedSolver};

#[derive(Parser)]
#[command(
    name = "precompute",
    about = "Precompute first two game-tree levels for serving"
)]
struct Cli {
    /// Path to dictionary file
    #[arg(short, long)]
    dict: PathBuf,

    /// Word lengths to precompute (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    lengths: Vec<usize>,

    /// Directory for disk cache databases
    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// Max depth to precompute (1 = first guess only, 2 = first two guesses)
    #[arg(long, default_value = "2")]
    depth: usize,

    /// Skip positions with more than this many words
    #[arg(long)]
    max_words: Option<usize>,
}

/// Partition word indices by their positional pattern for a given letter.
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

/// Check if a position is already solved (EXACT) in the disk cache.
fn is_cached_exact(dc: &DiskCache, words: &[Vec<u8>], indices: &[usize], masked: u32) -> bool {
    if indices.len() <= 1 {
        return true; // trivial
    }
    let folded = fold_required_letters(words, indices, masked);
    let hash = canonical_hash_for_words(words, indices, folded);
    dc.get(hash).and_then(decode_tt_entry).is_some()
}

/// Solve a position using full SMP and flush to disk cache.
/// Returns (value, elapsed, new_entries).
///
/// Uses `solve_position` with the full word list and correct masked value
/// so that TT entries are keyed identically to the server's lookup path.
fn solve_and_flush(
    dc: &Arc<DiskCache>,
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
) -> (u32, std::time::Duration, usize) {
    let solver = MemoizedSolver::with_disk_cache(Arc::clone(dc));
    let start = Instant::now();
    let value = solver.solve_position_smp(words, indices, masked);
    let elapsed = start.elapsed();
    let flushed = solver.flush_to_disk().unwrap_or(Ok(0)).unwrap_or(0);
    (value, elapsed, flushed)
}

/// Collect all positions at depth 1 and 2, sorted largest first.
struct Position {
    label: String,
    indices: Vec<usize>,
    masked: u32,
}

fn collect_positions(
    words: &[Vec<u8>],
    all_indices: &[usize],
    max_depth: usize,
) -> Vec<Position> {
    let mut positions = Vec::new();

    // Depth 1: partition by each letter
    for li in 0..26u8 {
        let letter = b'a' + li;
        let masked = letter_bit(letter);
        let parts = partition_by_letter(words, all_indices, letter);
        for (pmask, indices) in &parts {
            if indices.len() <= 1 {
                continue;
            }
            let ch = letter as char;
            let kind = if *pmask == 0 { "miss" } else { "hit" };
            positions.push(Position {
                label: format!("d1 '{ch}' {kind}({pmask:#x}) {}", indices.len()),
                indices: indices.clone(),
                masked,
            });

            // Depth 2: for each depth-1 partition, partition by remaining letters
            if max_depth >= 2 {
                for li2 in 0..26u8 {
                    if li2 == li {
                        continue;
                    }
                    let letter2 = b'a' + li2;
                    let masked2 = masked | letter_bit(letter2);
                    let parts2 = partition_by_letter(words, indices, letter2);
                    for (pmask2, indices2) in &parts2 {
                        if indices2.len() <= 1 {
                            continue;
                        }
                        let ch2 = letter2 as char;
                        let kind2 = if *pmask2 == 0 { "miss" } else { "hit" };
                        positions.push(Position {
                            label: format!(
                                "d2 '{ch}'{kind}+'{ch2}'{kind2} {}",
                                indices2.len()
                            ),
                            indices: indices2.clone(),
                            masked: masked2,
                        });
                    }
                }
            }
        }
    }

    // Sort largest first — they benefit most from precomputation, and their
    // TT entries help solve smaller positions via L2.
    positions.sort_by(|a, b| b.indices.len().cmp(&a.indices.len()));
    positions
}

fn rss_gb() -> f64 {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0)
        * 4096;
    rss as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words\n", dict.total_words());

    for &len in &cli.lengths {
        let words: Vec<Vec<u8>> = dict.words_of_length(len).to_vec();
        if words.is_empty() {
            println!("k={len}: no words, skipping");
            continue;
        }
        println!("=== k={len}: {} words ===", words.len());

        let map_size = 16 * 1024 * 1024 * 1024;
        let dc = Arc::new(DiskCache::open(&cli.cache_dir, len, &words, map_size)?);
        println!("  Disk cache: {} entries", dc.entry_count());

        let all_indices: Vec<usize> = (0..words.len()).collect();
        let positions = collect_positions(&words, &all_indices, cli.depth);

        let total = positions.len();
        let mut solved = 0usize;
        let mut skipped = 0usize;
        let mut total_flushed = 0usize;
        let wall_start = Instant::now();

        println!("  {} positions to check (depth 1..={})", total, cli.depth);

        let mut too_large = 0usize;
        for (i, pos) in positions.iter().enumerate() {
            if let Some(max) = cli.max_words {
                if pos.indices.len() > max {
                    too_large += 1;
                    continue;
                }
            }
            if is_cached_exact(&dc, &words, &pos.indices, pos.masked) {
                skipped += 1;
                continue;
            }

            let (value, elapsed, flushed) =
                solve_and_flush(&dc, &words, &pos.indices, pos.masked);
            solved += 1;
            total_flushed += flushed;

            let pct = (i + 1) as f64 / total as f64 * 100.0;
            println!(
                "  [{:>5.1}%] {} => value={}, {:.2?}, +{} entries (RSS {:.1}G)",
                pct,
                pos.label,
                value,
                elapsed,
                flushed,
                rss_gb(),
            );
        }

        let wall = wall_start.elapsed();
        println!(
            "  Done: {solved} solved, {skipped} cached, {too_large} skipped (too large), {total_flushed} entries flushed, {wall:.1?} total",
        );
        println!("  Disk cache now: {} entries\n", dc.entry_count());
    }

    Ok(())
}
