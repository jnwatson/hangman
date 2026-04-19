#![deny(clippy::all, clippy::pedantic)]

//! Precompute the first two levels of the game tree for fast serving.
//!
//! For each word length, enumerate all 26 first-guess partitions and all
//! 26×25 second-guess partitions. Solve each with full SMP parallelism
//! and flush results to the shared LMDB disk cache.
//!
//! Positions already present (EXACT) in the disk cache are skipped,
//! making the binary resumable after interruption.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
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

    /// Process only positions where `index % N == i`. Format: "i/N" (e.g. "2/4"
    /// means this machine handles positions 2, 6, 10, ... out of every 4).
    /// The `positions` list is deterministic across machines given the same
    /// dict/length/depth, so modulo sharding produces non-overlapping slices.
    #[arg(long)]
    shard: Option<String>,

    /// Debug: solve only this single position index and stop. Useful for
    /// reproducing hangs on specific states.
    #[arg(long)]
    only_index: Option<usize>,
}

/// Parse a `--shard` argument of the form `"i/N"`. Returns `(i, N)`.
///
/// # Errors
/// - missing slash
/// - non-integer parts
/// - `N` is zero
/// - `i >= N`
fn parse_shard(s: &str) -> Result<(usize, usize)> {
    let (i_str, n_str) = s.split_once('/')
        .ok_or_else(|| anyhow::anyhow!("--shard must be i/N (e.g. 2/4), got {s:?}"))?;
    let i: usize = i_str.parse()
        .map_err(|e| anyhow::anyhow!("--shard i not an integer ({s:?}): {e}"))?;
    let n: usize = n_str.parse()
        .map_err(|e| anyhow::anyhow!("--shard N not an integer ({s:?}): {e}"))?;
    if n == 0 {
        anyhow::bail!("--shard N must be ≥ 1, got {s:?}");
    }
    if i >= n {
        anyhow::bail!("--shard i must be < N, got {s:?}");
    }
    Ok((i, n))
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
    let flushed = match solver.flush_to_disk() {
        Some(Ok(n)) => n,
        Some(Err(e)) => {
            eprintln!("  WARNING: flush_to_disk failed: {e:#}");
            0
        }
        None => 0,
    };
    (value, elapsed, flushed)
}

/// Collect all positions up to `max_depth`, sorted largest first.
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
    let path = String::new();
    collect_recursive(words, all_indices, 0, max_depth, &path, &mut positions);

    // Sort largest first — they benefit most from precomputation, and their
    // TT entries help solve smaller positions via L2.
    positions.sort_by(|a, b| b.indices.len().cmp(&a.indices.len()));

    // Deduplicate — different guess orderings can produce the same word set
    // (e.g., 'e'hit+'q'miss == 'q'miss+'e'hit). Hash (masked, indices) as a
    // fast proxy for canonical equivalence.
    let before = positions.len();
    let mut seen = HashSet::new();
    positions.retain(|pos| {
        let mut h = std::hash::DefaultHasher::new();
        pos.masked.hash(&mut h);
        pos.indices.hash(&mut h);
        seen.insert(h.finish())
    });
    let dupes = before - positions.len();
    if dupes > 0 {
        println!("  Removed {dupes} duplicate positions ({before} -> {})", positions.len());
    }

    positions
}

fn collect_recursive(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    depth_remaining: usize,
    path: &str,
    out: &mut Vec<Position>,
) {
    if depth_remaining == 0 {
        return;
    }
    let depth = path.chars().filter(|&c| c == '+').count() + 1;

    for li in 0..26u8 {
        let letter = b'a' + li;
        if masked & letter_bit(letter) != 0 {
            continue;
        }
        let new_masked = masked | letter_bit(letter);
        let parts = partition_by_letter(words, indices, letter);
        for (pmask, part_indices) in &parts {
            if part_indices.len() <= 1 {
                continue;
            }
            let ch = letter as char;
            let kind = if *pmask == 0 { "miss" } else { "hit" };
            let label = if path.is_empty() {
                format!("d{depth} '{ch}' {kind}({pmask:#x}) {}", part_indices.len())
            } else {
                format!(
                    "d{depth} {path}+'{ch}'{kind} {}",
                    part_indices.len()
                )
            };
            out.push(Position {
                label,
                indices: part_indices.clone(),
                masked: new_masked,
            });

            if depth_remaining > 1 {
                let sub_path = if path.is_empty() {
                    format!("'{ch}'{kind}")
                } else {
                    format!("{path}+'{ch}'{kind}")
                };
                collect_recursive(
                    words,
                    part_indices,
                    new_masked,
                    depth_remaining - 1,
                    &sub_path,
                    out,
                );
            }
        }
    }
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

    // Diagnostic: explicitly build rayon pool if RAYON_NUM_THREADS is set
    // (the crate's automatic env-var handling is inconsistent in some envs).
    if let Ok(s) = std::env::var("RAYON_NUM_THREADS")
        && let Ok(n) = s.parse::<usize>()
        && n >= 1
    {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok();
        eprintln!("rayon pool size: {}", rayon::current_num_threads());
    }

    let (shard_i, shard_n) = match cli.shard.as_deref() {
        Some(s) => parse_shard(s)?,
        None => (0, 1),
    };
    if shard_n > 1 {
        println!("Shard: {shard_i}/{shard_n} (processing positions where index % {shard_n} == {shard_i})\n");
    }

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words\n", dict.total_words());

    for &len in &cli.lengths {
        let words: Vec<Vec<u8>> = dict.words_of_length(len).to_vec();
        if words.is_empty() {
            println!("k={len}: no words, skipping");
            continue;
        }
        println!("=== k={len}: {} words ===", words.len());

        let map_size = 256 * 1024 * 1024 * 1024;
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
        let mut not_my_shard = 0usize;
        for (i, pos) in positions.iter().enumerate() {
            if let Some(target) = cli.only_index {
                if i != target {
                    continue;
                }
            }
            if shard_n > 1 && i % shard_n != shard_i {
                not_my_shard += 1;
                continue;
            }
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
        let shard_note = if shard_n > 1 {
            format!(", {not_my_shard} skipped (other shard)")
        } else {
            String::new()
        };
        println!(
            "  Done: {solved} solved, {skipped} cached, {too_large} skipped (too large){shard_note}, {total_flushed} entries flushed, {wall:.1?} total",
        );
        println!("  Disk cache now: {} entries\n", dc.entry_count());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_valid() {
        assert_eq!(parse_shard("0/1").unwrap(), (0, 1));
        assert_eq!(parse_shard("0/4").unwrap(), (0, 4));
        assert_eq!(parse_shard("3/4").unwrap(), (3, 4));
        assert_eq!(parse_shard("7/8").unwrap(), (7, 8));
    }

    #[test]
    fn parse_shard_missing_slash() {
        let err = parse_shard("4").unwrap_err().to_string();
        assert!(err.contains("i/N"), "msg: {err}");
    }

    #[test]
    fn parse_shard_zero_n() {
        let err = parse_shard("0/0").unwrap_err().to_string();
        assert!(err.contains("N must be"), "msg: {err}");
    }

    #[test]
    fn parse_shard_i_out_of_range() {
        let err = parse_shard("4/4").unwrap_err().to_string();
        assert!(err.contains("i must be"), "msg: {err}");
        assert!(parse_shard("5/4").is_err());
    }

    #[test]
    fn parse_shard_non_integer() {
        assert!(parse_shard("a/4").is_err());
        assert!(parse_shard("2/b").is_err());
        assert!(parse_shard("-1/4").is_err());
    }

    /// For any deterministic position list, sharding produces a disjoint
    /// partition whose union equals the full list.
    #[test]
    fn shard_partitions_positions_disjointly() {
        let positions: Vec<usize> = (0..100).collect();
        for n in [1usize, 2, 3, 4, 5, 8, 10] {
            let mut covered: Vec<usize> = Vec::new();
            for i in 0..n {
                for (idx, _) in positions.iter().enumerate() {
                    if idx % n == i {
                        covered.push(idx);
                    }
                }
            }
            covered.sort_unstable();
            let expected: Vec<usize> = (0..100).collect();
            assert_eq!(covered, expected, "n={n}: shards did not cover all positions exactly once");
        }
    }
}
