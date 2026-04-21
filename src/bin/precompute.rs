#![deny(clippy::all, clippy::pedantic)]
#![allow(clippy::similar_names)]

//! Precompute the first N levels of the game tree for fast serving.
//!
//! For each word length, enumerate all reachable positions up to the
//! configured `--depth`, solve each with full SMP parallelism, and flush
//! results to the shared LMDB disk cache.
//!
//! Positions already present (EXACT) in the disk cache are skipped,
//! making the binary resumable after interruption.
//!
//! Positions are emitted via a streaming DFS walk with inline dedup, so
//! memory scales with unique position count (hundreds of MB at depth=4
//! for large k) rather than the pre-dedup enumeration count (tens of
//! GB).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;
use rustc_hash::{FxHashSet, FxHasher};

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{canonical_hash_for_words, decode_tt_entry, fold_required_letters};
use hangman2::solver::{DiskCache, MemoizedSolver};

/// One position queued for solving by the worker pool. Owned copies of
/// label + indices so it's Send and rayon-friendly.
struct Task {
    idx: usize,
    label: String,
    indices: Vec<usize>,
    masked: u32,
}

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

    /// Flush accumulated TT entries to LMDB only every N positions (default 1,
    /// which flushes after every solve). Larger values amortize LMDB B-tree
    /// write amplification and dramatically cut disk I/O on slow-storage hosts
    /// (e.g. Hetzner Cloud CAX), at the cost of higher in-memory TT footprint.
    #[arg(long, default_value = "1")]
    flush_every_n_positions: usize,

    /// Flush if the in-memory session TT reaches this many entries (default
    /// 0 = disabled). Useful to bound RAM when batching flushes.
    #[arg(long, default_value = "0")]
    flush_at_cache_entries: usize,
}

/// Parse a `--shard` argument of the form `"i/N"`. Returns `(i, N)`.
///
/// # Errors
/// - missing slash
/// - non-integer parts
/// - `N` is zero
/// - `i >= N`
fn parse_shard(s: &str) -> Result<(usize, usize)> {
    let (i_str, n_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("--shard must be i/N (e.g. 2/4), got {s:?}"))?;
    let i: usize = i_str
        .parse()
        .map_err(|e| anyhow::anyhow!("--shard i not an integer ({s:?}): {e}"))?;
    let n: usize = n_str
        .parse()
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
fn partition_by_letter(words: &[Vec<u8>], indices: &[usize], letter: u8) -> Vec<(u32, Vec<usize>)> {
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

/// Compute a 128-bit dedup key for a (masked, indices) position, avoiding
/// the 4e-5 birthday-collision risk a single u64 would have at ~40M entries
/// (k=11 depth=4). A missed dedup is harmless; a spurious collision would
/// silently drop a unique position, which breaks strong-solution coverage.
fn dedup_key(masked: u32, indices: &[usize]) -> (u64, u64) {
    let mut h1 = FxHasher::default();
    masked.hash(&mut h1);
    indices.hash(&mut h1);
    let mut h2 = FxHasher::default();
    0xdead_beef_cafe_babe_u64.hash(&mut h2);
    masked.hash(&mut h2);
    indices.hash(&mut h2);
    (h1.finish(), h2.finish())
}

/// Walk the game tree up to `max_depth` in DFS letter order, invoking
/// `callback` for each unique position. Dedup is inline via a single
/// `FxHashSet<(u64, u64)>`, so memory scales with *unique position count*
/// rather than pre-dedup enumeration count. At k=11 depth=4 this means
/// ~1 GB of dedup state instead of ~40 GB of materialized `Position`
/// structs (the previous implementation OOM'd on a 32 GB host).
///
/// DFS order replaces the old sort-largest-first ordering; a streaming
/// walk can't sort. Cross-depth order is still roughly largest-first
/// (depth-1 positions precede their depth-4 descendants in the subtree),
/// which preserves most of the TT-warming benefit.
///
/// Returns `true` if the walk ran to completion, `false` if `callback`
/// returned `false` (used by `--only-index` for early exit).
fn for_each_position<F>(
    words: &[Vec<u8>],
    all_indices: &[usize],
    max_depth: usize,
    mut callback: F,
) -> bool
where
    F: FnMut(&str, &[usize], u32) -> bool,
{
    // Depth 0: the root state (all words, no letters guessed). The server
    // looks this up to display the true minimax value; without it the
    // server falls back to a hardcoded estimate which can be materially
    // wrong (e.g., k=4 true minimax is 14 but the estimate was 16).
    let root_label = format!("d0 root {}", all_indices.len());
    if !callback(&root_label, all_indices, 0) {
        return false;
    }

    let mut dedup: FxHashSet<(u64, u64)> = FxHashSet::default();
    walk_tree(
        words,
        all_indices,
        0,
        max_depth,
        "",
        &mut dedup,
        &mut callback,
    )
}

fn walk_tree<F>(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    depth_remaining: usize,
    path: &str,
    dedup: &mut FxHashSet<(u64, u64)>,
    callback: &mut F,
) -> bool
where
    F: FnMut(&str, &[usize], u32) -> bool,
{
    if depth_remaining == 0 {
        return true;
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
            if !dedup.insert(dedup_key(new_masked, part_indices)) {
                continue;
            }
            let ch = letter as char;
            let kind = if *pmask == 0 { "miss" } else { "hit" };
            let label = if path.is_empty() {
                format!("d{depth} '{ch}' {kind}({pmask:#x}) {}", part_indices.len())
            } else {
                format!("d{depth} {path}+'{ch}'{kind} {}", part_indices.len())
            };
            if !callback(&label, part_indices, new_masked) {
                return false;
            }

            if depth_remaining > 1 {
                let sub_path = if path.is_empty() {
                    format!("'{ch}'{kind}")
                } else {
                    format!("{path}+'{ch}'{kind}")
                };
                if !walk_tree(
                    words,
                    part_indices,
                    new_masked,
                    depth_remaining - 1,
                    &sub_path,
                    dedup,
                    callback,
                ) {
                    return false;
                }
            }
        }
    }
    true
}

#[allow(clippy::cast_precision_loss)]
fn rss_gb() -> f64 {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0)
        * 4096;
    rss as f64 / (1024.0 * 1024.0 * 1024.0)
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
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
        println!(
            "Shard: {shard_i}/{shard_n} (processing positions where index % {shard_n} == {shard_i})\n"
        );
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

        // Pre-count pass: enumerate unique positions without allocating
        // Position structs or solving, so we can report % progress during
        // the real pass. The walk is CPU-bound on `partition_by_letter`
        // and typically finishes in seconds; memory use matches the real
        // pass (dedup set), so this dominates neither runtime nor RAM.
        let count_start = Instant::now();
        let mut total = 0usize;
        for_each_position(
            &words,
            &all_indices,
            cli.depth,
            |_label, _indices, _masked| {
                total += 1;
                true
            },
        );
        println!(
            "  {total} unique positions to check (depth 0..={}, enumerated in {:.1?})",
            cli.depth,
            count_start.elapsed(),
        );

        let solved = AtomicUsize::new(0);
        let skipped = AtomicUsize::new(0);
        let total_flushed = AtomicUsize::new(0);
        let too_large = AtomicUsize::new(0);
        let not_my_shard = AtomicUsize::new(0);
        let stdout_lock: Mutex<()> = Mutex::new(());
        let wall_start = Instant::now();

        let flush_every = cli.flush_every_n_positions.max(1);
        // Batch size trades enumeration-pause cost (small) against per-batch
        // parallel throughput (large). 10K positions × ~400B label+indices
        // ≈ 4 MB of transient memory per batch — negligible next to the
        // dedup set. Most batches complete in seconds; enumeration resumes.
        let batch_size: usize = 10_000;
        let num_workers = rayon::current_num_threads().max(1);
        println!(
            "  worker pool: {num_workers} threads, batch size {batch_size}, flush every {flush_every} positions per worker"
        );

        let mut batch: Vec<Task> = Vec::with_capacity(batch_size);
        let mut position_index: usize = 0;
        let mut should_abort = false;

        // Helper: drain the current batch through the worker pool.
        let process_batch = |batch: &mut Vec<Task>| {
            if batch.is_empty() {
                return;
            }
            let chunk_size = batch.len().div_ceil(num_workers);
            batch.par_chunks(chunk_size).for_each(|chunk| {
                let solver = MemoizedSolver::with_disk_cache(Arc::clone(&dc));
                let mut positions_since_flush = 0usize;
                for task in chunk {
                    if is_cached_exact(&dc, &words, &task.indices, task.masked) {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let start = Instant::now();
                    let value =
                        solver.solve_position_smp(&words, &task.indices, task.masked);
                    let elapsed = start.elapsed();
                    solved.fetch_add(1, Ordering::Relaxed);
                    positions_since_flush += 1;

                    let flushed = if positions_since_flush >= flush_every {
                        let n = match solver.flush_and_evict() {
                            Some(Ok(n)) => n,
                            Some(Err(e)) => {
                                eprintln!("  WARNING: flush_and_evict failed: {e:#}");
                                0
                            }
                            None => 0,
                        };
                        total_flushed.fetch_add(n, Ordering::Relaxed);
                        positions_since_flush = 0;
                        n
                    } else {
                        0
                    };

                    let pct = (task.idx + 1) as f64 / total as f64 * 100.0;
                    let tid = rayon::current_thread_index().unwrap_or(0);
                    {
                        let _g = stdout_lock.lock().unwrap();
                        if flushed > 0 || flush_every == 1 {
                            println!(
                                "  [{pct:>5.1}%] t{tid} {} => value={value}, {elapsed:.2?}, +{flushed} flushed (RSS {:.1}G)",
                                task.label,
                                rss_gb(),
                            );
                        } else {
                            println!(
                                "  [{pct:>5.1}%] t{tid} {} => value={value}, {elapsed:.2?} (flush in {})",
                                task.label,
                                flush_every - positions_since_flush,
                            );
                        }
                    }
                }
                // Final flush for this chunk.
                if positions_since_flush > 0
                    && let Some(res) = solver.flush_and_evict()
                {
                    match res {
                        Ok(n) => {
                            total_flushed.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(e) => eprintln!("  WARNING: per-chunk final flush failed: {e:#}"),
                    }
                }
            });
            batch.clear();
        };

        for_each_position(&words, &all_indices, cli.depth, |label, indices, masked| {
            let idx = position_index;
            position_index += 1;

            if let Some(target) = cli.only_index
                && idx != target
            {
                return true;
            }
            if shard_n > 1 && idx % shard_n != shard_i {
                not_my_shard.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            if let Some(max) = cli.max_words
                && indices.len() > max
            {
                too_large.fetch_add(1, Ordering::Relaxed);
                return true;
            }

            batch.push(Task {
                idx,
                label: label.to_string(),
                indices: indices.to_vec(),
                masked,
            });

            if batch.len() >= batch_size {
                process_batch(&mut batch);
            }

            if cli.only_index == Some(idx) {
                should_abort = true;
                return false;
            }
            true
        });

        // Final batch.
        if !batch.is_empty() {
            process_batch(&mut batch);
        }
        let _ = should_abort;

        let wall = wall_start.elapsed();
        let shard_note = if shard_n > 1 {
            format!(
                ", {} skipped (other shard)",
                not_my_shard.load(Ordering::Relaxed)
            )
        } else {
            String::new()
        };
        println!(
            "  Done: {} solved, {} cached, {} skipped (too large){shard_note}, {} entries flushed, {wall:.1?} total",
            solved.load(Ordering::Relaxed),
            skipped.load(Ordering::Relaxed),
            too_large.load(Ordering::Relaxed),
            total_flushed.load(Ordering::Relaxed),
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
            assert_eq!(
                covered, expected,
                "n={n}: shards did not cover all positions exactly once"
            );
        }
    }
}
