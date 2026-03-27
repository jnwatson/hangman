#![deny(clippy::all, clippy::pedantic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::solver::{DagSolver, DiskCache, MemoizedSolver, NaiveSolver, ProgressSnapshot};

#[derive(Parser)]
#[command(name = "bench", about = "Benchmark the hangman solver")]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// Path to dictionary file
    #[arg(short, long)]
    dict: PathBuf,

    /// Word lengths to benchmark (comma-separated). Default: 2,3,4
    #[arg(short, long, value_delimiter = ',')]
    lengths: Option<Vec<usize>>,

    /// Also run the naive solver for oracle comparison (slow!)
    #[arg(long)]
    naive: bool,

    /// Also run the DAG solver for comparison
    #[arg(long)]
    dag: bool,

    /// Maximum word count to attempt (skip lengths with more words)
    #[arg(long, default_value = "10000")]
    max_words: usize,

    /// Sample N words per length (use full set if omitted)
    #[arg(short, long)]
    sample: Option<usize>,

    /// Estimate runtime by iterative deepening instead of full solve
    #[arg(long)]
    estimate: bool,

    /// Time budget in seconds for estimation (default: 60)
    #[arg(long, default_value = "60")]
    estimate_budget: u64,

    /// Save TT to disk (LMDB) after solving each length
    #[arg(long)]
    save_cache: bool,

    /// Load TT from disk (LMDB) before solving each length
    #[arg(long)]
    load_cache: bool,

    /// Directory for disk cache databases
    #[arg(long, default_value = "./tt_cache")]
    cache_dir: PathBuf,

    /// After solving, warm the cache for all reachable game positions
    #[arg(long)]
    warm_cache: bool,

    /// Stop solving at this miss depth instead of finding the exact value.
    /// Useful for k=4-7 where full solve is infeasible. Runs iterative
    /// deepening from 1..=N, warming the TT progressively. Combined with
    /// --save-cache, produces a partial TT for runtime serving.
    #[arg(long)]
    bounded: Option<u32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words\n", dict.total_words());

    let lengths = cli.lengths.clone().unwrap_or_else(|| vec![2, 3, 4]);

    if cli.estimate {
        run_estimate(&cli, &dict, &lengths);
    } else {
        run_benchmark(&cli, &dict, &lengths)?;
    }

    Ok(())
}

fn run_estimate(cli: &Cli, dict: &Dictionary, lengths: &[usize]) {
    for len in lengths {
        let all_words = dict.words_of_length(*len);
        if all_words.is_empty() {
            println!("Length {len}: no words");
            continue;
        }

        let words: Vec<Vec<u8>> = if let Some(n) = cli.sample {
            all_words.iter().take(n).cloned().collect()
        } else {
            all_words.to_vec()
        };

        if words.len() > cli.max_words {
            println!(
                "Length {len}: {} words (skipped, > {})",
                words.len(),
                cli.max_words
            );
            continue;
        }

        println!("Length {len}: {} words", words.len());
        println!(
            "  {:>5} {:>12} {:>12} {:>10} {:>10} {:>8}",
            "depth", "time", "cumul", "hash_call", "cache_hit", "result"
        );
        println!("  {}", "-".repeat(65));

        let solver = MemoizedSolver::new();
        let mut prev_secs = 0.0_f64;
        let mut cumul_time = 0.0_f64;
        let mut ratios: Vec<f64> = Vec::new();
        let mut is_solved = false;
        let mut last_depth = 0u32;
        let budget = std::time::Duration::from_secs(cli.estimate_budget);
        let wall_start = Instant::now();

        for depth in 1..=25 {
            let start = Instant::now();
            let result = solver.solve_bounded(&words, depth);
            let elapsed = start.elapsed();
            let secs = elapsed.as_secs_f64();
            cumul_time += secs;
            last_depth = depth;

            let hash_calls = solver.hash_calls();
            let cache_hits = solver.cache_hits();

            let status = if result <= depth {
                is_solved = true;
                format!("= {result}")
            } else {
                format!("> {depth}")
            };

            println!(
                "  {depth:>5} {secs:>12.3}s {cumul_time:>12.3}s {hash_calls:>10} {cache_hits:>10} {status:>8}",
            );

            if is_solved {
                println!("\n  Solved! Optimal misses = {result}");
                break;
            }

            if secs > 0.001 && prev_secs > 0.001 {
                ratios.push(secs / prev_secs);
            }
            prev_secs = secs;

            // Stop probing if we've exceeded the time budget.
            if wall_start.elapsed() >= budget {
                println!("  (time budget of {}s reached)", cli.estimate_budget);
                break;
            }
        }

        if !is_solved {
            print_estimate(&ratios, prev_secs, cumul_time, last_depth);
        }
        println!();
    }
}

fn print_estimate(ratios: &[f64], last_time: f64, cumul_time: f64, last_depth: u32) {
    if ratios.is_empty() {
        println!("\n  Not enough data to extrapolate (need at least 3 measured depths).");
        return;
    }

    // Use geometric mean of ratios as the branching factor estimate.
    let ln_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
    #[allow(clippy::cast_precision_loss)]
    let geo_mean = (ln_sum / ratios.len() as f64).exp();

    // Also show the last ratio for trend comparison.
    let last_ratio = ratios.last().copied().unwrap_or(geo_mean);

    println!("\n  Branching factor: {geo_mean:.2}x (geometric mean), last ratio: {last_ratio:.2}x");
    println!("  Extrapolated solve times (cumulative, from depth {last_depth}):");
    println!(
        "  {:>5} {:>14}  {:>14}",
        "depth", "conservative", "optimistic"
    );

    // Conservative: use the larger of geo_mean and last_ratio.
    let factor_conservative = geo_mean.max(last_ratio);
    // Optimistic: use the smaller.
    let factor_optimistic = geo_mean.min(last_ratio);

    let mut depth_t_con = last_time;
    let mut depth_t_opt = last_time;
    let mut cumul_con = cumul_time;
    let mut cumul_opt = cumul_time;

    for d in (last_depth + 1)..=(last_depth + 10) {
        depth_t_con *= factor_conservative;
        depth_t_opt *= factor_optimistic;
        cumul_con += depth_t_con;
        cumul_opt += depth_t_opt;
        println!(
            "  {:>5} {:>14} {:>14}",
            d,
            format_duration(cumul_con),
            format_duration(cumul_opt),
        );
    }
}

fn format_duration(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{:.1}m", secs / 60.0)
    } else if secs < 86400.0 {
        format!("{:.1}h", secs / 3600.0)
    } else {
        format!("{:.1}d", secs / 86400.0)
    }
}

#[allow(clippy::too_many_lines)]
fn run_benchmark(cli: &Cli, dict: &Dictionary, lengths: &[usize]) -> Result<()> {
    if cli.dag {
        println!(
            "{:>6} {:>8} {:>8} {:>12} {:>8} {:>12} {:>8}",
            "length", "words", "misses", "dag_time", "dag_$", "memo_time", "memo_$"
        );
    } else {
        println!(
            "{:>6} {:>8} {:>8} {:>12} {:>8} {:>10} {:>10}",
            "length", "words", "misses", "memo_time", "memo_$", "hash_call", "cache_hit"
        );
    }
    println!("{}", "-".repeat(78));

    for len in lengths {
        let all_words = dict.words_of_length(*len);
        if all_words.is_empty() {
            println!("{len:>6} {:>8} (no words)", 0);
            continue;
        }

        let words: Vec<Vec<u8>> = if let Some(n) = cli.sample {
            all_words.iter().take(n).cloned().collect()
        } else {
            all_words.to_vec()
        };

        if words.len() > cli.max_words {
            println!(
                "{:>6} {:>8} (skipped, > {} words)",
                len,
                words.len(),
                cli.max_words
            );
            continue;
        }

        // DAG solver (optional)
        let mut dag_misses = None;
        let mut dag_elapsed = std::time::Duration::ZERO;
        let mut dag_cache = 0;
        if cli.dag {
            let start = Instant::now();
            let dag = DagSolver::new(&words);
            let misses = dag.solve();
            dag_elapsed = start.elapsed();
            dag_cache = dag.cache_size();
            dag_misses = Some(misses);
        }

        // Memoized solver — run in a thread so we can report progress.
        // When --save-cache is set, always open/create the disk cache upfront
        // so that flush_to_disk can write intermediate entries during the solve.
        let disk = if cli.load_cache || cli.save_cache {
            let map_size = 16 * 1024 * 1024 * 1024; // 16 GB virtual
            let existing = if cli.load_cache {
                DiskCache::open_if_exists(&cli.cache_dir, *len, &words, map_size)?
            } else {
                None
            };
            if let Some(dc) = existing {
                Some(dc)
            } else if cli.save_cache {
                Some(DiskCache::open(&cli.cache_dir, *len, &words, map_size)?)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(ref dc) = disk {
            eprintln!("  loaded disk cache: {} entries", dc.entry_count());
        }
        let memo = Arc::new(if let Some(dc) = disk {
            MemoizedSolver::with_disk_cache(Arc::new(dc))
        } else {
            MemoizedSolver::new()
        });
        let start = Instant::now();
        let memo_misses = if let Some(max_misses) = cli.bounded {
            solve_bounded_with_progress(&memo, &words, max_misses)
        } else {
            solve_with_progress(&memo, &words)
        };
        let memo_elapsed = start.elapsed();
        let memo_cache = memo.cache_size();

        if cli.warm_cache {
            let warm_start = Instant::now();
            let new_positions = memo.warm_serving_cache(&words);
            eprintln!(
                "  warmed cache: {} new positions in {:.2}s",
                new_positions,
                warm_start.elapsed().as_secs_f64()
            );
        }

        if cli.save_cache {
            // Final flush of any remaining exact entries.
            let saved = memo.flush_to_disk().unwrap_or(Ok(0))?;
            eprintln!("  saved {saved} entries to disk cache");
        }

        if let Some(dm) = dag_misses {
            assert_eq!(
                dm, memo_misses,
                "MISMATCH at length {len}: dag={dm} != memo={memo_misses}"
            );
        }

        // Naive solver (optional)
        if cli.naive {
            let refs: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
            let naive_misses = NaiveSolver::solve(&refs, 0);
            assert_eq!(
                memo_misses, naive_misses,
                "MISMATCH at length {len}: memo={memo_misses} != naive={naive_misses}"
            );
        }

        let hash_calls = memo.hash_calls();
        let cache_hits = memo.cache_hits();

        // For bounded mode, show ">N" when the solve didn't converge.
        let misses_str = if let Some(max) = cli.bounded {
            if memo_misses > max {
                format!(">{max}")
            } else {
                format!("{memo_misses}")
            }
        } else {
            format!("{memo_misses}")
        };

        if cli.dag {
            println!(
                "{:>6} {:>8} {:>8} {:>12.2?} {:>8} {:>12.2?} {:>8}",
                len,
                words.len(),
                misses_str,
                dag_elapsed,
                dag_cache,
                memo_elapsed,
                memo_cache,
            );
        } else {
            println!(
                "{:>6} {:>8} {:>8} {:>12.2?} {:>8} {:>10} {:>10}",
                len,
                words.len(),
                misses_str,
                memo_elapsed,
                memo_cache,
                hash_calls,
                cache_hits,
            );
        }
    }
    Ok(())
}

/// Read this process's resident set size in bytes from /proc/self/statm.
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(rss_pages * 4096)
}

/// Run solver on the current thread, with a background reporter printing
/// progress every few seconds. Periodically flushes EXACT entries to disk.
fn solve_with_progress(solver: &Arc<MemoizedSolver>, words: &[Vec<u8>]) -> u32 {
    let pair = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    // Spawn background reporter thread.
    let reporter = {
        let solver = Arc::clone(solver);
        let pair = Arc::clone(&pair);
        std::thread::spawn(move || {
            let (lock, cvar) = &*pair;
            let mut ticks_since_flush = 0u32;
            loop {
                let done = lock.lock().unwrap();
                let (done, _) = cvar
                    .wait_timeout(done, std::time::Duration::from_secs(5))
                    .unwrap();
                if *done {
                    break;
                }
                drop(done);

                if let Some(snap) = solver.progress() {
                    let rss = rss_bytes().unwrap_or(0);
                    let rss_gb = rss as f64 / (1024.0 * 1024.0 * 1024.0);
                    let eta = estimate_remaining(&snap);
                    print_live_progress(&snap, eta, rss_gb);
                }

                // Check RSS and evict if too high (35 GB).
                let rss = rss_bytes().unwrap_or(0);
                const EVICT_THRESHOLD: u64 = 35 * 1024 * 1024 * 1024;
                if rss > EVICT_THRESHOLD {
                    if let Some(result) = solver.flush_and_evict() {
                        match result {
                            Ok(n) => eprintln!(
                                "  (RSS {:.1}G > 35G: flushed {n} entries to disk, evicted in-memory cache)",
                                rss as f64 / (1024.0 * 1024.0 * 1024.0)
                            ),
                            Err(e) => eprintln!("  (flush+evict error: {e})"),
                        }
                    }
                    ticks_since_flush = 0;
                } else {
                    // Flush to disk every 60s (12 ticks × 5s).
                    ticks_since_flush += 1;
                    if ticks_since_flush >= 12 {
                        ticks_since_flush = 0;
                        if let Some(result) = solver.flush_to_disk() {
                            match result {
                                Ok(n) => eprintln!("  (flushed {n} exact entries to disk)"),
                                Err(e) => eprintln!("  (flush error: {e})"),
                            }
                        }
                    }
                }
            }
        })
    };

    let result = solver.solve(words);

    // Signal reporter to stop immediately.
    let (lock, cvar) = &*pair;
    *lock.lock().unwrap() = true;
    cvar.notify_one();
    let _ = reporter.join();
    result
}

/// Run iterative bounded solves from depth 1..=max_misses, with a background
/// progress reporter. Each iteration warms the TT, so later iterations benefit
/// from prior cached results. Flushes to disk between iterations.
fn solve_bounded_with_progress(
    solver: &Arc<MemoizedSolver>,
    words: &[Vec<u8>],
    max_misses: u32,
) -> u32 {
    let pair = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    // Spawn background reporter thread (same as solve_with_progress).
    let reporter = {
        let solver = Arc::clone(solver);
        let pair = Arc::clone(&pair);
        std::thread::spawn(move || {
            let (lock, cvar) = &*pair;
            let mut ticks_since_flush = 0u32;
            loop {
                let done = lock.lock().unwrap();
                let (done, _) = cvar
                    .wait_timeout(done, std::time::Duration::from_secs(5))
                    .unwrap();
                if *done {
                    break;
                }
                drop(done);

                if let Some(snap) = solver.progress() {
                    let rss = rss_bytes().unwrap_or(0);
                    let rss_gb = rss as f64 / (1024.0 * 1024.0 * 1024.0);
                    let eta = estimate_remaining(&snap);
                    print_live_progress(&snap, eta, rss_gb);
                }

                // RSS-based eviction (35 GB threshold).
                let rss = rss_bytes().unwrap_or(0);
                const EVICT_THRESHOLD: u64 = 35 * 1024 * 1024 * 1024;
                if rss > EVICT_THRESHOLD {
                    if let Some(result) = solver.flush_and_evict() {
                        match result {
                            Ok(n) => eprintln!(
                                "  (RSS {:.1}G > 35G: flushed {n} entries to disk, evicted in-memory cache)",
                                rss as f64 / (1024.0 * 1024.0 * 1024.0)
                            ),
                            Err(e) => eprintln!("  (flush+evict error: {e})"),
                        }
                    }
                    ticks_since_flush = 0;
                } else {
                    ticks_since_flush += 1;
                    if ticks_since_flush >= 12 {
                        ticks_since_flush = 0;
                        if let Some(result) = solver.flush_to_disk() {
                            match result {
                                Ok(n) => eprintln!("  (flushed {n} entries to disk)"),
                                Err(e) => eprintln!("  (flush error: {e})"),
                            }
                        }
                    }
                }
            }
        })
    };

    let wall_start = Instant::now();
    let mut result = 0;
    for target in 1..=max_misses {
        let iter_start = Instant::now();
        result = solver.solve_bounded(words, target);
        let iter_secs = iter_start.elapsed().as_secs_f64();
        let cache_sz = solver.cache_size();

        if result <= target {
            eprintln!(
                "  bounded depth={target}: SOLVED (value={result}), cache={cache_sz}, {:.1}s (total {:.1}s)",
                iter_secs,
                wall_start.elapsed().as_secs_f64()
            );
            break;
        }
        eprintln!(
            "  bounded depth={target}: value>{target}, cache={cache_sz}, {:.1}s (total {:.1}s)",
            iter_secs,
            wall_start.elapsed().as_secs_f64()
        );

        // Flush to disk between iterations.
        if let Some(flush_result) = solver.flush_to_disk() {
            match flush_result {
                Ok(n) => eprintln!("  (inter-iteration flush: {n} entries to disk)"),
                Err(e) => eprintln!("  (flush error: {e})"),
            }
        }
    }

    // Signal reporter to stop.
    let (lock, cvar) = &*pair;
    *lock.lock().unwrap() = true;
    cvar.notify_one();
    let _ = reporter.join();
    result
}

/// Estimate remaining time using MTD(f) iteration growth rates.
///
/// Each non-trivial MTD(f) iteration takes longer than the previous (wider
/// window → more search). Given ≥2 completed iteration times, we compute
/// the geometric mean of consecutive ratios as a growth factor, then
/// estimate the current (in-progress) iteration's remaining time plus
/// a few future iterations.
///
/// Falls back to frame-based estimate for non-MTD(f) solves (>25K words)
/// or when insufficient iteration data exists.
fn estimate_remaining(snap: &ProgressSnapshot) -> Option<f64> {
    // For MTD(f): use iteration durations if available.
    if snap.iter_durations.len() >= 2 {
        return estimate_from_iterations(snap);
    }

    // Fallback: frame-based estimate for non-MTD(f) or early iterations.
    let frame = snap.frames.first()?;
    let done = f64::from(frame.completed_moves);
    if done < 1.0 {
        return None;
    }
    let total = f64::from(frame.total_moves);
    let elapsed = snap.elapsed_secs - frame.start_secs;
    if elapsed <= 0.0 {
        return None;
    }
    Some(elapsed / done * (total - done))
}

/// Estimate remaining time from MTD(f) iteration growth pattern.
///
/// Estimates how long the current in-progress iteration will take based on
/// the growth rate of previous iterations, then reports remaining time in
/// this iteration. The last iteration dominates total solve time, so this
/// is most accurate when it matters most.
fn estimate_from_iterations(snap: &ProgressSnapshot) -> Option<f64> {
    let durations = &snap.iter_durations;
    if durations.len() < 2 {
        return None;
    }

    // Compute growth factor from last ratio (most recent is most representative).
    let last = durations[durations.len() - 1];
    let prev = durations[durations.len() - 2];
    if prev < 0.001 {
        return None;
    }
    let growth = (last / prev).max(1.0);

    // Estimate current iteration's total time.
    let current_estimated = last * growth;

    // Time spent in current iteration = elapsed - sum(completed iterations + trivial gaps).
    let completed_sum: f64 = durations.iter().sum();
    let current_elapsed = (snap.elapsed_secs - completed_sum).max(0.0);
    let current_remaining = (current_estimated - current_elapsed).max(0.0);

    Some(current_remaining)
}

fn print_live_progress(snap: &ProgressSnapshot, ema: Option<f64>, rss_gb: f64) {
    if snap.frames.is_empty() {
        return;
    }

    // Show the progress stack: ply-by-ply [done/total].
    let mut parts: Vec<String> = Vec::new();
    for (i, frame) in snap.frames.iter().enumerate() {
        parts.push(format!(
            "{}[{}/{}]",
            i, frame.completed_moves, frame.total_moves
        ));
        if i >= 7 {
            parts.push("...".to_string());
            break;
        }
    }

    let elapsed = format_duration(snap.elapsed_secs);
    let iter_str = if snap.mtd_iteration > 0 {
        format!(" iter={}", snap.mtd_iteration)
    } else {
        String::new()
    };
    let eta = ema
        .map(|r| format!("  ETA: {}", format_duration(r)))
        .unwrap_or_default();

    eprintln!(
        "  [{elapsed}]{iter_str} {} RSS:{rss_gb:.1}G{eta}",
        parts.join(" ")
    );
}
