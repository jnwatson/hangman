#![deny(clippy::all, clippy::pedantic)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::items_after_statements,
    clippy::manual_is_multiple_of,
    clippy::doc_markdown,
)]

//! Coverage evaluator: does the existing cache let the server handle any
//! position within `--deadline-secs` seconds?
//!
//! For each reachable position at `--target-depth`, look it up in the cache
//! (as the server would via `canonical_hash_for_words`). If cached → hit,
//! zero cost. If not → run the solver with a per-position deadline and time
//! it. Emit per-bucket counts, worst cases, and the list of positions that
//! exceed the timeout.
//!
//! Abandon any single position that exceeds the deadline (the solver returns
//! early), and abandon the whole run if --total-deadline-secs elapses.

use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::MemoizedSolver;
use hangman2::solver::serving::{canonical_hash_for_words, fold_required_letters, pos_mask};

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short = 'k', long)]
    length: usize,

    #[arg(long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// Enumerate positions at exactly this depth. Should be (cached_max_depth + 1).
    #[arg(long, default_value_t = 3)]
    target_depth: usize,

    /// Per-position deadline in seconds.
    #[arg(long, default_value_t = 10)]
    deadline_secs: u64,

    /// Abandon the whole evaluation after this many seconds.
    #[arg(long, default_value_t = 3600)]
    total_deadline_secs: u64,

    /// Warn loudly on positions that exceed this wall-time (seconds).
    #[arg(long, default_value_t = 3)]
    warn_secs: u64,

    /// Process every N-th position only (for sampling large sets).
    #[arg(long, default_value_t = 1)]
    stride: usize,

    /// Print progress every N positions.
    #[arg(long, default_value_t = 500)]
    progress_every: usize,
}

fn partition_by_letter(
    words: &[Vec<u8>],
    indices: &[usize],
    letter: u8,
) -> HashMap<u32, Vec<usize>> {
    let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in indices {
        let m = pos_mask(&words[idx], letter);
        parts.entry(m).or_default().push(idx);
    }
    parts
}

/// Stream positions at `target_depth` through `callback`. Dedup is done via
/// a HashSet of (masked, indices-hash) — memory is O(positions) in hashes,
/// not in index vectors, so this fits easily on a 1 GB box.
fn stream_at_depth<F: FnMut(&str, &[usize], u32)>(
    words: &[Vec<u8>],
    all_indices: &[usize],
    target_depth: usize,
    mut callback: F,
) {
    let mut seen: HashSet<u64> = HashSet::new();

    if target_depth == 0 {
        callback("d0 root", all_indices, 0);
        return;
    }

    fn recurse<F: FnMut(&str, &[usize], u32)>(
        words: &[Vec<u8>],
        indices: &[usize],
        masked: u32,
        depth: usize,
        target: usize,
        path: &str,
        seen: &mut HashSet<u64>,
        callback: &mut F,
    ) {
        if depth == target {
            let mut h = std::hash::DefaultHasher::new();
            masked.hash(&mut h);
            indices.hash(&mut h);
            let key = h.finish();
            if !seen.insert(key) {
                return;
            }
            callback(path, indices, masked);
            return;
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
                let ch = letter as char;
                let kind = if *pmask == 0 { "miss" } else { "hit" };
                let next_path = if path.is_empty() {
                    format!("'{ch}'{kind}")
                } else {
                    format!("{path}+'{ch}'{kind}")
                };
                recurse(
                    words,
                    part_indices,
                    new_masked,
                    depth + 1,
                    target,
                    &next_path,
                    seen,
                    callback,
                );
            }
        }
    }

    recurse(words, all_indices, 0, 0, target_depth, "", &mut seen, &mut callback);
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    let words: Vec<Vec<u8>> = dict.words_of_length(cli.length).to_vec();
    if words.is_empty() {
        anyhow::bail!("no words of length {}", cli.length);
    }
    println!("k={}: {} words", cli.length, words.len());

    let map_size = 256 * 1024 * 1024 * 1024;
    let dc = Arc::new(
        DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, map_size)?
            .ok_or_else(|| anyhow::anyhow!("no cache found"))?,
    );
    println!("cache entries: {}", dc.entry_count());

    let all_indices: Vec<usize> = (0..words.len()).collect();

    let solver = MemoizedSolver::for_serving(words.clone(), Some(Arc::clone(&dc)));

    // Per-bucket counts.
    let mut bucket_instant = 0u64; // cache hit
    let mut bucket_fast = 0u64;    // <1s
    let mut bucket_medium = 0u64;  // 1s .. warn_secs
    let mut bucket_slow = 0u64;    // warn_secs .. deadline_secs
    let mut bucket_timeout = 0u64; // == deadline
    let mut max_live_ms = 0u64;
    let mut offenders: Vec<(u64, String)> = Vec::new();

    let run_started = Instant::now();
    let total_deadline = run_started + Duration::from_secs(cli.total_deadline_secs);
    let aborted = Arc::new(AtomicBool::new(false));

    let aborted_watcher = Arc::clone(&aborted);
    std::thread::spawn(move || {
        let now = Instant::now();
        if total_deadline > now {
            std::thread::sleep(total_deadline - now);
        }
        aborted_watcher.store(true, Ordering::SeqCst);
    });

    let processed = AtomicU64::new(0);
    let warn_ms = cli.warn_secs * 1000;
    let timeout_ms = cli.deadline_secs * 1000;
    let stride = cli.stride.max(1);
    let mut stream_index = 0u64;

    println!("streaming positions at depth {} (stride {}) ...", cli.target_depth, stride);

    stream_at_depth(&words, &all_indices, cli.target_depth, |path, indices, masked| {
        if aborted.load(Ordering::SeqCst) {
            return;
        }
        let this_idx = stream_index;
        stream_index += 1;
        if this_idx % (stride as u64) != 0 {
            return;
        }

        let label = format!("d{} {} n={}", cli.target_depth, path, indices.len());

        // Cache lookup (mirrors server path).
        let folded = fold_required_letters(&words, indices, masked);
        let hash = canonical_hash_for_words(&words, indices, folded);
        let cache_hit = dc.get(hash).is_some();

        if cache_hit {
            bucket_instant += 1;
        } else {
            let deadline = Instant::now() + Duration::from_secs(cli.deadline_secs);
            let t0 = Instant::now();
            let (_, cancelled) =
                solver.solve_position_with_deadline(indices, masked, Some(deadline));
            let elapsed_ms = t0.elapsed().as_millis() as u64;

            if cancelled || elapsed_ms >= timeout_ms.saturating_sub(200) {
                bucket_timeout += 1;
                offenders.push((elapsed_ms, label.clone()));
            } else if elapsed_ms >= warn_ms {
                bucket_slow += 1;
                offenders.push((elapsed_ms, label.clone()));
            } else if elapsed_ms >= 1000 {
                bucket_medium += 1;
            } else {
                bucket_fast += 1;
            }
            if elapsed_ms > max_live_ms {
                max_live_ms = elapsed_ms;
            }
        }

        let p = processed.fetch_add(1, Ordering::SeqCst) + 1;
        if p % (cli.progress_every as u64) == 0 {
            println!(
                "  [{}] hits={} <1s={} 1-{}s={} {}-{}s={} timeouts={}  max_live={}ms  last: {}",
                p,
                bucket_instant, bucket_fast, cli.warn_secs,
                bucket_medium, cli.warn_secs, cli.deadline_secs,
                bucket_slow, bucket_timeout, max_live_ms, label,
            );
        }
    });

    println!("\n=== summary ===");
    println!("positions evaluated: {}", processed.load(Ordering::SeqCst));
    println!("cache hits:          {bucket_instant}");
    println!("live-solve <1s:      {bucket_fast}");
    println!("live-solve 1-{}s:    {bucket_medium}", cli.warn_secs);
    println!("live-solve {}-{}s:   {bucket_slow}   (slow — inspect)", cli.warn_secs, cli.deadline_secs);
    println!("timeouts (>= {}s):   {bucket_timeout}   (fail — must cover deeper)", cli.deadline_secs);
    println!("max live-solve ms:   {max_live_ms}");
    println!("wall time:           {:.1}s", run_started.elapsed().as_secs_f64());
    if aborted.load(Ordering::SeqCst) {
        println!("ABORTED: total-deadline hit before full enumeration completed");
    }

    if !offenders.is_empty() {
        offenders.sort_by(|a, b| b.0.cmp(&a.0));
        println!("\n=== worst positions (top 20) ===");
        for (ms, label) in offenders.iter().take(20) {
            let flag = if *ms >= timeout_ms.saturating_sub(200) { "FAIL" } else { "slow" };
            println!("  {flag} {ms:>6}ms  {label}");
        }
        println!("\ntotal positions > {}s: {}", cli.warn_secs, offenders.len());
    }

    Ok(())
}
