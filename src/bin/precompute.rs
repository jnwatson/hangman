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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;
use rustc_hash::FxHashSet;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{canonical_hash_for_words, decode_tt_entry, fold_required_letters};
use hangman2::solver::{DiskCache, MemoizedSolver};

/// One position queued for solving by the worker pool. Send/rayon-friendly.
///
/// We store raw label components (`depth`, `path`, `letter`, `pmask`, indices length)
/// rather than a pre-formatted label string. The worker only formats the
/// human-readable label in the `println!` branch — which only fires for
/// positions that weren't already cached. For deep precomputes where most
/// positions are cached skips, this avoids ~1B wasted string allocations
/// on the main (producer) thread.
///
/// `path` is an `Arc<String>` shared across sibling positions at the same
/// recursion level — cheap refcount bump per task instead of per-position
/// path clone.
struct Task {
    idx: usize,
    depth: usize,
    path: Arc<String>,
    letter: u8,
    pmask: u32,
    indices: Vec<usize>,
    masked: u32,
}

fn format_label(task: &Task) -> String {
    if task.depth == 0 {
        return format!("d0 root {}", task.indices.len());
    }
    let ch = task.letter as char;
    let kind = if task.pmask == 0 { "miss" } else { "hit" };
    if task.path.is_empty() {
        format!(
            "d{} '{}' {}({:#x}) {}",
            task.depth,
            ch,
            kind,
            task.pmask,
            task.indices.len(),
        )
    } else {
        format!(
            "d{} {}+'{}'{} {}",
            task.depth,
            task.path,
            ch,
            kind,
            task.indices.len(),
        )
    }
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

/// Dedup key = the canonical hash the LMDB cache is keyed on.
///
/// Two game states that are canonically equivalent (letter-permutation
/// symmetric, etc.) produce the same hash — walker skips them after the
/// first. This collapses the dedup set size by whatever the average
/// equivalence-class size is (typically 5–20× at deep depths) while
/// keeping dedup and cache-lookup consistent: if dedup says "seen",
/// LMDB agrees it's the same canonical position.
///
/// Collision semantics now match LMDB's — any u64 hash collision
/// already affects the storage layer identically, so using this single
/// hash for dedup doesn't introduce any new correctness risk beyond
/// what the system already tolerates.
fn dedup_key(words: &[Vec<u8>], indices: &[usize], masked: u32) -> u128 {
    let folded = fold_required_letters(words, indices, masked);
    canonical_hash_for_words(words, indices, folded)
}

/// Walk the game tree up to `max_depth` in DFS letter order, invoking
/// `callback` for each unique position. Dedup is inline via a single
/// `FxHashSet<u128>` keyed by canonical hash, so memory scales with the *unique canonical position count*
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
/// Signature: `callback(depth, path, letter, pmask, indices, masked) -> bool`.
///
/// Callback receives raw label components instead of a pre-formatted string.
/// This lets the producer (`walk_tree`) skip ~1B `format!()` calls per deep run;
/// consumers format on demand via `format_label(&Task)`. `path` is a shared
/// `Arc<String>` so siblings at the same recursion level reuse the same
/// allocation — consumer stores `Arc::clone(path)` (refcount bump) instead of
/// allocating a fresh owned copy.
///
/// For the root state, callback is invoked once with `depth=0, letter=0,
/// pmask=0, path=Arc::new(String::new())`. `format_label` special-cases
/// `depth == 0` to emit `"d0 root {count}"`.
fn for_each_position<F>(
    words: &[Vec<u8>],
    all_indices: &[usize],
    max_depth: usize,
    mut callback: F,
) -> bool
where
    F: FnMut(usize, &Arc<String>, u8, u32, &[usize], u32) -> bool,
{
    let root_path = Arc::new(String::new());
    if !callback(0, &root_path, 0, 0, all_indices, 0) {
        return false;
    }

    let mut dedup: FxHashSet<u128> = FxHashSet::default();
    walk_tree(
        words,
        all_indices,
        0,
        1,
        max_depth,
        &root_path,
        &mut dedup,
        &mut callback,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_tree<F>(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    depth: usize,
    depth_remaining: usize,
    path: &Arc<String>,
    dedup: &mut FxHashSet<u128>,
    callback: &mut F,
) -> bool
where
    F: FnMut(usize, &Arc<String>, u8, u32, &[usize], u32) -> bool,
{
    if depth_remaining == 0 {
        return true;
    }

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
            if !dedup.insert(dedup_key(words, part_indices, new_masked)) {
                continue;
            }
            if !callback(depth, path, letter, *pmask, part_indices, new_masked) {
                return false;
            }

            if depth_remaining > 1 {
                let ch = letter as char;
                let kind = if *pmask == 0 { "miss" } else { "hit" };
                let sub_path = Arc::new(if path.is_empty() {
                    format!("'{ch}'{kind}")
                } else {
                    format!("{path}+'{ch}'{kind}")
                });
                if !walk_tree(
                    words,
                    part_indices,
                    new_masked,
                    depth + 1,
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

        // 1 TB: sparse on 64-bit systems (no physical disk cost until used),
        // gives us multi-TB headroom so precomputes can't bust MDB_MAP_FULL.
        let map_size = 1024_usize * 1024 * 1024 * 1024;
        let dc = Arc::new(DiskCache::open(&cli.cache_dir, len, &words, map_size)?);
        println!("  Disk cache: {} entries", dc.entry_count());

        let all_indices: Vec<usize> = (0..words.len()).collect();

        // Pre-count pass removed: the walk itself takes hours for deep
        // precomputes (e.g., k=4 d=9 enumerates 1.4 B positions in ~2 h),
        // and a pre-count added a full second walk just to print the
        // "X unique positions" header. The main pass now prints running
        // `[pos=N]` counters so progress is visible without the duplicate
        // walk.

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
        // Chunk size matters for load balance. When chunks are batch/N (one
        // per worker), a single slow position in any chunk stalls its whole
        // worker while others finish early and idle. Smaller chunks give
        // rayon work-stealing granularity. At chunk_size=64, a 10K batch
        // produces ~156 chunks for 16 workers (~10 chunks per worker) — the
        // unluckiest worker's pain is bounded to one chunk's worth.
        let chunk_size: usize = 64;
        let num_workers = rayon::current_num_threads().max(1);
        println!(
            "  worker pool: {num_workers} threads, batch size {batch_size}, chunk size {chunk_size}, flush every {flush_every} positions per worker"
        );

        let mut batch: Vec<Task> = Vec::with_capacity(batch_size);
        let mut position_index: usize = 0;
        let mut should_abort = false;

        // Helper: drain the current batch through the worker pool.
        let process_batch = |batch: &mut Vec<Task>| {
            if batch.is_empty() {
                return;
            }
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

                    let tid = rayon::current_thread_index().unwrap_or(0);
                    let label = format_label(task);
                    {
                        let _g = stdout_lock.lock().unwrap();
                        if flushed > 0 || flush_every == 1 {
                            println!(
                                "  [pos={}] t{tid} {label} => value={value}, {elapsed:.2?}, +{flushed} flushed (RSS {:.1}G)",
                                task.idx,
                                rss_gb(),
                            );
                        } else {
                            println!(
                                "  [pos={}] t{tid} {label} => value={value}, {elapsed:.2?} (flush in {})",
                                task.idx,
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

        for_each_position(&words, &all_indices, cli.depth, |depth, path, letter, pmask, indices, masked| {
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
                depth,
                path: Arc::clone(path),
                letter,
                pmask,
                indices: indices.to_vec(),
                masked,
            });

            if batch.len() >= batch_size {
                process_batch(&mut batch);
                // After each batch drain, print a rolling save-stats
                // summary so we can gauge worker redundancy without
                // waiting for the whole k run to finish. rejected-by-exact
                // is the diagnostic of interest.
                let stats = dc.save_stats();
                let considered = stats.total_considered();
                if considered > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let rbe_pct =
                        stats.rejected_by_exact as f64 / considered as f64 * 100.0;
                    println!(
                        "  [stats] considered={} inserted={} overwritten={} rej-exact={} ({:.2}%) rej-other={}",
                        considered,
                        stats.inserted,
                        stats.overwritten,
                        stats.rejected_by_exact,
                        rbe_pct,
                        stats.rejected_other,
                    );
                }
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
        // Save-path stats: rejected_by_exact counts bounds that hit an
        // existing EXACT on disk — each one means the solve that produced
        // the bound was redundant work another worker had already done.
        // High rates indicate the parallel layout should share more state.
        let stats = dc.save_stats();
        let considered = stats.total_considered();
        if considered > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rejected_by_exact_pct = stats.rejected_by_exact as f64 / considered as f64 * 100.0;
            #[allow(clippy::cast_precision_loss)]
            let rejected_other_pct = stats.rejected_other as f64 / considered as f64 * 100.0;
            println!(
                "  Save stats: {} considered, {} inserted, {} overwritten, {} rejected-by-exact ({:.1}%), {} rejected-other ({:.1}%)",
                considered,
                stats.inserted,
                stats.overwritten,
                stats.rejected_by_exact,
                rejected_by_exact_pct,
                stats.rejected_other,
                rejected_other_pct,
            );
        }
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
