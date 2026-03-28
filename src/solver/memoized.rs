use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use super::canon::{dedup_and_hash, dedup_only};
use super::disk_cache::DiskCache;
use crate::game::{LetterSet, letter_bit};

// ---------------------------------------------------------------------------
// Progress tracking — live ETA estimation during long solves.
// ---------------------------------------------------------------------------

/// A snapshot of one ply level in the search.
#[derive(Clone, Debug)]
pub struct ProgressFrame {
    /// When this ply started (seconds since solve start).
    pub start_secs: f64,
    /// Total letters to try at this level.
    pub total_moves: u32,
    /// Letters fully evaluated so far.
    pub completed_moves: u32,
}

/// Current solver progress, readable while the solve is running.
#[derive(Clone, Debug)]
pub struct ProgressSnapshot {
    /// Stack of active frames (index 0 = shallowest ply).
    pub frames: Vec<ProgressFrame>,
    /// Seconds since solve started.
    pub elapsed_secs: f64,
    /// Current MTD(f) iteration (0 = first, or non-MTD(f) mode).
    pub mtd_iteration: u32,
    /// Duration of completed non-trivial MTD(f) iterations (seconds).
    pub iter_durations: Vec<f64>,
}

/// Memoized minimax solver with structural caching, alpha-beta pruning,
/// move ordering, and position canonicalization.
///
/// ## Optimizations
///
/// 1. **Structural transposition table**: cache key is the canonical form of
///    the word set — letters relabeled by first appearance, positions sorted
///    into canonical order. Two word sets related by any letter permutation
///    AND/OR position permutation share a cache entry.
///
/// 2. **Alpha-beta pruning**: guesser minimizes, referee maximizes. Prune
///    when the referee's worst case already exceeds the guesser's best.
///    Transposition table stores bound types to avoid caching incorrect values.
///
/// 3. **Move ordering**: try letters that minimize the maximum partition size
///    first (most "informative" guesses). Better first moves = tighter alpha
///    = more pruning.
///
/// 4. **Word deduplication**: identical effective signatures collapse to one
///    representative index.
///
/// 5. **Lazy SMP parallelism**: for large word sets, multiple threads search
///    independently with different move orderings, sharing a transposition
///    table. Within each thread, YBWC-style parallelism evaluates the first
///    letter sequentially for a tight bound, then parallelizes the rest.
///
/// 6. **Compact cache key**: 128-bit hash of canonical form instead of full
///    Vec allocation. Collision probability ~2^-64 per pair is acceptable.
///
/// ## Representation
///
/// Words are stored once in the solver as `Vec<Vec<u8>>`. Each subproblem
/// is `(word_indices: &[usize], masked: u32)` where `masked` is a bitmask
/// of letters that have been guessed (treated as 0 in signatures). The
/// effective byte of word `i` at position `j` is:
/// `if masked & letter_bit(words[i][j]) != 0 { 0 } else { words[i][j] }`.
pub struct MemoizedSolver {
    cache: DashMap<u128, u32>,
    hash_calls: AtomicU64,
    cache_hits: AtomicU64,
    /// Reference to the currently active solve's data (for progress reading).
    active_data: std::sync::Mutex<Option<Arc<SolverData>>>,
    /// Optional on-disk cache (LMDB) for persisted EXACT entries.
    disk_cache: Option<Arc<DiskCache>>,
}

/// Parallelism threshold: nodes with at least this many words use rayon.
const PAR_THRESHOLD: usize = 50;

// Cache entry encoding:
//   Bits 10-11: bound type (0=exact, 1=lower, 2=upper)
//   Bits 5-9:   best letter index (0-25), 31 = none
//   Bits 0-4:   value (0-25)
// Values are always ≤ 25 (at most 26 letters), so 5 bits suffice.
const VALUE_MASK: u32 = 0x1F;
const LETTER_SHIFT: u32 = 5;
const BOUND_SHIFT: u32 = 10;
const BOUND_EXACT: u32 = 0;
const BOUND_LOWER: u32 = 1;
const BOUND_UPPER: u32 = 2;

#[inline]
fn cache_pack(value: u32, best_letter: u8, bound: u32) -> u32 {
    let letter_idx = u32::from(best_letter.wrapping_sub(b'a'));
    let letter_bits = if letter_idx < 26 { letter_idx } else { 31 };
    (value & VALUE_MASK) | (letter_bits << LETTER_SHIFT) | (bound << BOUND_SHIFT)
}

#[inline]
pub(super) fn cache_unpack(packed: u32) -> (u32, Option<u8>, u32) {
    let value = packed & VALUE_MASK;
    let letter_idx = (packed >> LETTER_SHIFT) & 0x1F;
    let best_letter = if letter_idx < 26 {
        Some(b'a' + letter_idx as u8)
    } else {
        None
    };
    let bound = (packed >> BOUND_SHIFT) & 0x3;
    (value, best_letter, bound)
}

impl MemoizedSolver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            active_data: std::sync::Mutex::new(None),
            disk_cache: None,
        }
    }

    /// Create a solver backed by an on-disk LMDB cache.
    ///
    /// The disk cache is checked as an L2 fallback when the in-memory
    /// transposition table misses. Only EXACT entries are stored on disk.
    #[must_use]
    pub fn with_disk_cache(disk: Arc<DiskCache>) -> Self {
        Self {
            cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            active_data: std::sync::Mutex::new(None),
            disk_cache: Some(disk),
        }
    }

    /// Copy all entries from another solver's persistent cache into this one.
    pub fn copy_cache_from(&self, other: &MemoizedSolver) {
        for entry in other.cache.iter() {
            self.cache.insert(*entry.key(), *entry.value());
        }
    }

    /// Access the in-memory cache (for serialization to disk).
    #[must_use]
    pub fn cache(&self) -> &DashMap<u128, u32> {
        &self.cache
    }

    /// Flush all TT entries (EXACT, LOWER, UPPER) to the disk cache.
    ///
    /// After `solve()` returns, uses the persistent cache (`self.cache`).
    /// During an active solve, uses the live solve cache (`active_data`).
    ///
    /// Returns the number of entries saved, or `None` if no disk cache.
    pub fn flush_to_disk(&self) -> Option<anyhow::Result<usize>> {
        let dc = self.disk_cache.as_ref()?;
        // Try the active solve cache first (for mid-solve flushes).
        let guard = self.active_data.lock().unwrap();
        if let Some(data) = guard.as_ref() {
            return Some(dc.save(&data.cache));
        }
        drop(guard);
        // After solve() returns, active_data is None — use persistent cache.
        Some(dc.save(&self.cache))
    }

    /// Flush all TT entries to disk, then clear the in-memory cache.
    /// The disk cache continues to serve as L2 for subsequent lookups.
    ///
    /// Only works during an active solve (when `active_data` is set).
    /// Returns the number of entries flushed, or `None` if no disk cache
    /// or no active solve.
    pub fn flush_and_evict(&self) -> Option<anyhow::Result<usize>> {
        let dc = self.disk_cache.as_ref()?;
        let guard = self.active_data.lock().unwrap();
        let data = guard.as_ref()?;
        let result = dc.save(&data.cache);
        data.cache.clear();
        Some(result)
    }

    /// Create a solver pre-initialized with word data for serving.
    ///
    /// Unlike `new()` or `with_disk_cache()`, this eagerly builds the internal
    /// data structures (pos\_masks, word\_letters, etc.) so that subsequent
    /// `solve_position` calls are fast.
    #[must_use]
    pub fn for_serving(words: Vec<Vec<u8>>, disk: Option<Arc<DiskCache>>) -> Self {
        let data = Arc::new(SolverData::new(words, disk.clone()));
        Self {
            cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            active_data: std::sync::Mutex::new(Some(data)),
            disk_cache: disk,
        }
    }

    /// Solve a specific subproblem: given word indices and already-masked
    /// letters, compute the minimax value. Uses the disk cache as L2.
    ///
    /// This is designed for the game server: when a precomputed cache entry
    /// is missing (e.g., the user made a non-optimal guess), this runs the
    /// solver on-demand. With a well-populated disk cache, most internal
    /// lookups hit and the solve completes quickly.
    ///
    /// # Panics
    ///
    /// Panics if the solver was not initialized via `for_serving`.
    #[must_use]
    pub fn solve_position(&self, indices: &[usize], masked: LetterSet) -> u32 {
        if indices.len() <= 1 {
            return 0;
        }
        let guard = self.active_data.lock().unwrap();
        let data = Arc::clone(guard.as_ref().expect("solver not initialized for serving"));
        drop(guard);

        data.cancelled.store(false, Ordering::Relaxed);
        let inner = MemoizedSolverInner::new(data, 0, false);
        inner.solve_subset(indices, masked, 0, u32::MAX)
    }

    /// Warm the cache for serving: ensure every reachable position has an
    /// EXACT entry. After calling `solve()`, the TT only has EXACT entries
    /// for the optimal path. This method visits all positions reachable by
    /// any sequence of user guesses (with optimal referee responses) and
    /// solves any missing entries.
    ///
    /// Returns the number of new positions solved.
    pub fn warm_serving_cache(&self, words: &[Vec<u8>]) -> usize {
        let data = Arc::new(SolverData::new(words.to_vec(), self.disk_cache.clone()));
        // Seed with all entries from the persistent cache.
        for entry in &self.cache {
            data.cache.insert(*entry.key(), *entry.value());
        }
        data.cancelled.store(false, Ordering::Relaxed);
        *self.active_data.lock().unwrap() = Some(Arc::clone(&data));

        let inner = MemoizedSolverInner::new(Arc::clone(&data), 0, false);
        let indices: Vec<usize> = (0..words.len()).collect();
        let mut solved = 0usize;
        let mut visited = rustc_hash::FxHashSet::default();
        inner.warm_recursive(&indices, 0, &mut solved, &mut visited);

        // Transfer new EXACT entries to persistent cache.
        for entry in &data.cache {
            let (_, _, bound) = cache_unpack(*entry.value());
            if bound == BOUND_EXACT {
                self.cache.insert(*entry.key(), *entry.value());
            }
        }
        *self.active_data.lock().unwrap() = None;
        solved
    }

    /// Solve for the given word list. All words must be the same length.
    ///
    /// For large word sets (> 25K words), uses Lazy SMP: multiple threads
    /// search independently with different move orderings, sharing a
    /// transposition table. For smaller sets, uses MTD(f) iterative deepening
    /// with YBWC-style internal parallelism.
    ///
    /// # Panics
    ///
    /// Panics if words is empty.
    #[must_use]
    pub fn solve(&self, words: &[Vec<u8>]) -> u32 {
        assert!(!words.is_empty());
        let data = Arc::new(SolverData::new(words.to_vec(), self.disk_cache.clone()));
        *self.active_data.lock().unwrap() = Some(Arc::clone(&data));
        let indices: Vec<usize> = (0..words.len()).collect();

        let n_extra = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1)
            / 4;
        // MTD(f) path benefits from more helpers warming the TT; pure Lazy
        // SMP has diminishing returns (DashMap contention). Tune independently.
        let n_extra = if words.len() <= 25_000 {
            n_extra.clamp(1, 7)
        } else {
            n_extra.clamp(1, 2)
        };

        let result = if words.len() <= 25_000 {
            // MTD(f) + Lazy SMP hybrid: all threads do iterative deepening
            // with perturbed move ordering, sharing a transposition table.
            // Main thread uses rayon (YBWC) for internal parallelism.
            std::thread::scope(|s| {
                // Launch helpers with full-window search and perturbed ordering.
                // The main thread's iterative deepening warms the TT, which
                // helps helpers find cutoffs faster.
                #[allow(clippy::cast_possible_truncation)]
                let _helpers: Vec<_> = (1..=n_extra as u32)
                    .map(|tid| {
                        let data = Arc::clone(&data);
                        let indices = indices.clone();
                        s.spawn(move || {
                            let solver = MemoizedSolverInner::new(data, tid, false);
                            solver.solve_subset(&indices, 0, 0, u32::MAX)
                        })
                    })
                    .collect();

                // Main thread: MTD(f) iterative deepening with explicit alpha.
                let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
                let mut result = 0;
                let mut lo = 0u32;
                for (iter, target) in (0..26u32).enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    data.mtd_iteration.store(iter as u32, Ordering::Relaxed);
                    let iter_start = Instant::now();
                    result = solver.solve_subset(&indices, 0, lo, target + 1);
                    let iter_secs = iter_start.elapsed().as_secs_f64();
                    if iter_secs > 0.01 {
                        data.iter_durations.lock().unwrap().push(iter_secs);
                    }
                    if result <= target {
                        break;
                    }
                    lo = result;
                }

                // MTD(f) used narrow windows, so many TT entries are bounds
                // rather than EXACT. Do one final full-window pass to convert
                // them to EXACT (needed for disk cache persistence). The TT is
                // warm from MTD(f), so this is very fast.
                solver.solve_subset(&indices, 0, 0, result + 1);

                // Signal helpers to stop and wait for them.
                data.cancelled.store(true, Ordering::Relaxed);
                result
            })
        } else {
            // Pure Lazy SMP: primary thread uses rayon (YBWC) for internal
            // parallelism; extra sequential threads explore with perturbed
            // orderings, sharing the TT for mutual pruning.
            std::thread::scope(|s| {
                #[allow(clippy::cast_possible_truncation)]
                let handles: Vec<_> = (1..=n_extra as u32)
                    .map(|tid| {
                        let data = Arc::clone(&data);
                        let indices = indices.clone();
                        s.spawn(move || {
                            let solver = MemoizedSolverInner::new(data, tid, false);
                            solver.solve_subset(&indices, 0, 0, u32::MAX)
                        })
                    })
                    .collect();

                let main_result = {
                    let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
                    solver.solve_subset(&indices, 0, 0, u32::MAX)
                };

                let mut best = main_result;
                for h in handles {
                    best = best.min(h.join().unwrap());
                }
                best
            })
        };

        // Transfer instrumentation to outer solver.
        self.hash_calls
            .fetch_add(data.hash_calls.load(Ordering::Relaxed), Ordering::Relaxed);
        self.cache_hits
            .fetch_add(data.cache_hits.load(Ordering::Relaxed), Ordering::Relaxed);
        // Transfer all entries to the persistent cache. EXACT always wins;
        // for bounds, keep the tighter value.
        for entry in &data.cache {
            let key = *entry.key();
            let new_packed = *entry.value();
            let (new_val, _, new_bound) = cache_unpack(new_packed);
            if let Some(existing) = self.cache.get(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                let dominated = new_bound == BOUND_EXACT
                    || (old_bound != BOUND_EXACT
                        && ((new_bound == BOUND_LOWER && new_val > old_val)
                            || (new_bound == BOUND_UPPER && new_val < old_val)));
                drop(existing);
                if dominated {
                    self.cache.insert(key, new_packed);
                }
            } else {
                self.cache.insert(key, new_packed);
            }
        }

        *self.active_data.lock().unwrap() = None;
        result
    }

    /// Solve a specific game position with full SMP parallelism.
    ///
    /// Unlike `solve()` which starts from the root (all words, masked=0),
    /// this starts from an arbitrary position. Unlike `solve_position()` which
    /// is single-threaded for serving, this uses full SMP for precompute.
    ///
    /// `words` must be the full dictionary for this length (not a subset) so
    /// that TT entries are keyed identically to the server's lookup path.
    pub fn solve_position_smp(
        &self,
        words: &[Vec<u8>],
        indices: &[usize],
        masked: LetterSet,
    ) -> u32 {
        if indices.is_empty() {
            return 0;
        }
        let data = Arc::new(SolverData::new(words.to_vec(), self.disk_cache.clone()));
        *self.active_data.lock().unwrap() = Some(Arc::clone(&data));
        let indices: Vec<usize> = indices.to_vec();

        let n_words = indices.len();
        let n_extra = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1)
            / 4;
        let n_extra = if n_words <= 25_000 {
            n_extra.clamp(1, 7)
        } else {
            n_extra.clamp(1, 2)
        };

        let result = if n_words <= 25_000 {
            std::thread::scope(|s| {
                #[allow(clippy::cast_possible_truncation)]
                let _helpers: Vec<_> = (1..=n_extra as u32)
                    .map(|tid| {
                        let data = Arc::clone(&data);
                        let indices = indices.clone();
                        s.spawn(move || {
                            let solver = MemoizedSolverInner::new(data, tid, false);
                            solver.solve_subset(&indices, masked, 0, u32::MAX)
                        })
                    })
                    .collect();

                let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
                let mut result = 0;
                let mut lo = 0u32;
                for (iter, target) in (0..26u32).enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    data.mtd_iteration.store(iter as u32, Ordering::Relaxed);
                    let iter_start = Instant::now();
                    result = solver.solve_subset(&indices, masked, lo, target + 1);
                    let iter_secs = iter_start.elapsed().as_secs_f64();
                    if iter_secs > 0.01 {
                        data.iter_durations.lock().unwrap().push(iter_secs);
                    }
                    if result <= target {
                        break;
                    }
                    lo = result;
                }

                solver.solve_subset(&indices, masked, 0, result + 1);

                data.cancelled.store(true, Ordering::Relaxed);
                result
            })
        } else {
            std::thread::scope(|s| {
                #[allow(clippy::cast_possible_truncation)]
                let handles: Vec<_> = (1..=n_extra as u32)
                    .map(|tid| {
                        let data = Arc::clone(&data);
                        let indices = indices.clone();
                        s.spawn(move || {
                            let solver = MemoizedSolverInner::new(data, tid, false);
                            solver.solve_subset(&indices, masked, 0, u32::MAX)
                        })
                    })
                    .collect();

                let main_result = {
                    let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
                    solver.solve_subset(&indices, masked, 0, u32::MAX)
                };

                let mut best = main_result;
                for h in handles {
                    best = best.min(h.join().unwrap());
                }
                best
            })
        };

        // Force-store EXACT entry for the root position. The MTD(f)
        // verification pass may fail to produce EXACT because TT bounds
        // from earlier iterations narrow the window (e.g., UPPER_BOUND
        // tightens beta so best == beta → LOWER_BOUND stored instead).
        {
            let folded = crate::solver::serving::fold_required_letters(
                &data.words, &indices, masked,
            );
            let root_key = crate::solver::serving::canonical_hash_for_words(
                &data.words, &indices, folded,
            );
            let packed = cache_pack(result, 0, BOUND_EXACT);
            data.cache.insert(root_key, packed);
        }

        self.hash_calls
            .fetch_add(data.hash_calls.load(Ordering::Relaxed), Ordering::Relaxed);
        self.cache_hits
            .fetch_add(data.cache_hits.load(Ordering::Relaxed), Ordering::Relaxed);
        for entry in &data.cache {
            let key = *entry.key();
            let new_packed = *entry.value();
            let (new_val, _, new_bound) = cache_unpack(new_packed);
            if let Some(existing) = self.cache.get(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                let dominated = new_bound == BOUND_EXACT
                    || (old_bound != BOUND_EXACT
                        && ((new_bound == BOUND_LOWER && new_val > old_val)
                            || (new_bound == BOUND_UPPER && new_val < old_val)));
                drop(existing);
                if dominated {
                    self.cache.insert(key, new_packed);
                }
            } else {
                self.cache.insert(key, new_packed);
            }
        }

        *self.active_data.lock().unwrap() = None;
        result
    }

    /// Solve with a maximum miss budget. Returns the minimax value if it is
    /// `<= max_misses`, otherwise returns the budget value (meaning the true
    /// value is `> max_misses`).
    ///
    /// Uses the same parallelism strategy as `solve`. The transposition table
    /// is preserved across calls, so successive calls with increasing budgets
    /// benefit from prior iterations' cached moves.
    ///
    /// # Panics
    ///
    /// Panics if words is empty.
    #[must_use]
    pub fn solve_bounded(&self, words: &[Vec<u8>], max_misses: u32) -> u32 {
        assert!(!words.is_empty());
        let data = Arc::new(SolverData::new(words.to_vec(), self.disk_cache.clone()));
        // Seed with any exact entries from prior calls.
        for entry in &self.cache {
            data.cache.insert(*entry.key(), *entry.value());
        }
        *self.active_data.lock().unwrap() = Some(Arc::clone(&data));
        let indices: Vec<usize> = (0..words.len()).collect();

        let n_extra = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1)
            / 4;
        let n_extra = if words.len() <= 25_000 {
            n_extra.clamp(1, 7)
        } else {
            n_extra.clamp(1, 2)
        };

        let result = std::thread::scope(|s| {
            // Launch SMP helper threads with the same budget.
            #[allow(clippy::cast_possible_truncation)]
            let _helpers: Vec<_> = (1..=n_extra as u32)
                .map(|tid| {
                    let data = Arc::clone(&data);
                    let indices = indices.clone();
                    s.spawn(move || {
                        let solver = MemoizedSolverInner::new(data, tid, false);
                        solver.solve_subset(&indices, 0, 0, max_misses + 1)
                    })
                })
                .collect();

            let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
            let result = solver.solve_subset(&indices, 0, 0, max_misses + 1);

            // Signal helpers to stop.
            data.cancelled.store(true, Ordering::Relaxed);
            result
        });

        self.hash_calls
            .fetch_add(data.hash_calls.load(Ordering::Relaxed), Ordering::Relaxed);
        self.cache_hits
            .fetch_add(data.cache_hits.load(Ordering::Relaxed), Ordering::Relaxed);
        // Preserve all TT entries (including lower/upper bounds) across calls.
        // Lower bounds from prior iterations provide tighter alpha for future
        // iterations, and upper bounds provide tighter beta.
        for entry in &data.cache {
            let (val, _, bound) = cache_unpack(*entry.value());
            let key = *entry.key();
            if let Some(existing) = self.cache.get(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                // Exact always wins. Otherwise keep the tighter bound.
                if bound == BOUND_EXACT
                    || (old_bound != BOUND_EXACT
                        && ((bound == BOUND_LOWER && val > old_val)
                            || (bound == BOUND_UPPER && val < old_val)))
                {
                    drop(existing);
                    self.cache.insert(key, *entry.value());
                }
            } else {
                self.cache.insert(key, *entry.value());
            }
        }

        *self.active_data.lock().unwrap() = None;
        result
    }

    /// Read the current progress of an active solve. Returns `None` if no
    /// solve is in progress or the progress stack is empty.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn progress(&self) -> Option<ProgressSnapshot> {
        let guard = self.active_data.lock().unwrap();
        let data = guard.as_ref()?;
        let stack = data.progress.lock().unwrap();
        if stack.is_empty() {
            return None;
        }
        Some(ProgressSnapshot {
            frames: stack.clone(),
            elapsed_secs: data.solve_start.elapsed().as_secs_f64(),
            mtd_iteration: data.mtd_iteration.load(Ordering::Relaxed),
            iter_durations: data.iter_durations.lock().unwrap().clone(),
        })
    }

    #[must_use]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    #[must_use]
    pub fn hash_calls(&self) -> u64 {
        self.hash_calls.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }
}

impl Default for MemoizedSolver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Shared solver data — lives in an Arc, shared across Lazy SMP threads.
// ---------------------------------------------------------------------------

struct SolverData {
    words: Vec<Vec<u8>>,
    /// Precomputed position masks: `pos_masks[letter_idx][word_idx]` = bitmask
    /// of positions where word contains letter (b'a' + `letter_idx`).
    /// Avoids O(k) per-call work in `pos_mask_for`.
    pos_masks: Vec<Vec<u32>>, // 26 × n_words
    /// Precomputed letter sets: `word_letters[word_idx]` = bitmask of letters
    /// present in the word. Used for fast `present_letters` computation.
    word_letters: Vec<u32>,
    cache: DashMap<u128, u32>,
    /// Maps cheap index-based hash → canonical key. Avoids redundant
    /// `dedup_and_hash` calls (the main bottleneck for small word sets).
    key_cache: DashMap<u64, u128>,
    hash_calls: AtomicU64,
    cache_hits: AtomicU64,
    /// Frequency rank: `freq_rank[letter_idx]` = rank (0 = most frequent).
    /// Used as tiebreaker when history scores are equal (cold start).
    freq_rank: [u8; 26],
    /// History heuristic: tracks which letters have been empirically good
    /// (best move or cutoff-causing) across the search. Used for dynamic
    /// move ordering — letters with high history scores are tried earlier.
    history: [AtomicU32; 26],
    /// Cancellation flag: when set, helper threads should exit early.
    cancelled: AtomicBool,
    /// Progress stack for ETA estimation. Only written by the main OS thread,
    /// read by an external reporter. The Mutex is uncontested during solving
    /// and only briefly held when reading.
    progress: std::sync::Mutex<Vec<ProgressFrame>>,
    /// Solve start time for computing progress frame offsets.
    solve_start: Instant,
    /// ID of the thread that created this data (the main solve thread).
    /// Only this thread updates the progress stack, avoiding corruption
    /// from rayon workers that share the same perturbation value.
    main_thread_id: std::thread::ThreadId,
    /// Current MTD(f) iteration number (0-indexed).
    mtd_iteration: AtomicU32,
    /// Duration of each completed MTD(f) iteration (seconds).
    /// Only non-trivial iterations (> 10ms) are recorded.
    iter_durations: std::sync::Mutex<Vec<f64>>,
    /// Optional on-disk L2 cache.
    disk_cache: Option<Arc<DiskCache>>,
}

impl SolverData {
    fn new(words: Vec<Vec<u8>>, disk_cache: Option<Arc<DiskCache>>) -> Self {
        let n = words.len();
        let mut pos_masks = vec![vec![0u32; n]; 26];
        let mut word_letters = vec![0u32; n];
        for (idx, word) in words.iter().enumerate() {
            for (j, &b) in word.iter().enumerate() {
                if b.is_ascii_lowercase() {
                    let li = (b - b'a') as usize;
                    pos_masks[li][idx] |= 1 << j;
                    word_letters[idx] |= 1 << li;
                }
            }
        }
        // Rank letters by how many words contain them (descending).
        let mut letter_counts = [0u32; 26];
        for &wl in &word_letters {
            for li in 0..26u32 {
                if wl & (1 << li) != 0 {
                    letter_counts[li as usize] += 1;
                }
            }
        }
        // Compute frequency ranks: sort letters by count descending,
        // then assign rank 0 to most frequent.
        #[allow(clippy::cast_possible_truncation)]
        let mut freq_order: Vec<(usize, u32)> = letter_counts.iter().copied().enumerate().collect();
        freq_order.sort_by(|a, b| b.1.cmp(&a.1));
        let mut freq_rank = [0u8; 26];
        for (rank, &(li, _)) in freq_order.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            { freq_rank[li] = rank as u8; }
        }

        Self {
            words,
            pos_masks,
            word_letters,
            cache: DashMap::new(),
            key_cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            freq_rank,
            history: std::array::from_fn(|_| AtomicU32::new(0)),
            cancelled: AtomicBool::new(false),
            progress: std::sync::Mutex::new(Vec::new()),
            solve_start: Instant::now(),
            main_thread_id: std::thread::current().id(),
            mtd_iteration: AtomicU32::new(0),
            iter_durations: std::sync::Mutex::new(Vec::new()),
            disk_cache,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-thread solver — references shared data via Arc, has own perturbation.
// ---------------------------------------------------------------------------

enum CacheLookup {
    Hit(u32),
    Miss {
        tt_move: Option<u8>,
        alpha: u32,
        beta: u32,
    },
}

struct MemoizedSolverInner {
    data: Arc<SolverData>,
    /// 0 = primary thread (optimal TT move ordering).
    /// > 0 = Lazy SMP secondary thread (perturbed ordering for diversity).
    perturbation: u32,
    /// Whether to use rayon for internal parallelism (YBWC).
    /// False for Lazy SMP threads (parallelism comes from multiple threads).
    use_rayon: bool,
}

impl MemoizedSolverInner {
    fn new(data: Arc<SolverData>, perturbation: u32, use_rayon: bool) -> Self {
        Self {
            data,
            perturbation,
            use_rayon,
        }
    }

    /// Compute the position mask of `letter` in word `idx`.
    #[inline]
    fn pos_mask_for(&self, idx: usize, letter: u8) -> u32 {
        self.data.pos_masks[(letter - b'a') as usize][idx]
    }

    /// Collect all non-masked letters present in the word subset.
    fn present_letters(&self, indices: &[usize], masked: LetterSet) -> LetterSet {
        let mut present = 0u32;
        for &idx in indices {
            present |= self.data.word_letters[idx];
        }
        present & !masked
    }



    /// Fast path for exactly 2 distinct words.
    ///
    /// The answer is 0 if there exists an unmasked letter whose position
    /// masks differ between the two words AND both are non-zero (both hit,
    /// distinct partitions). Otherwise the answer is 1.
    fn solve_two_words(&self, idx_a: usize, idx_b: usize, masked: LetterSet) -> u32 {
        for letter_idx in 0..26u8 {
            let letter = b'a' + letter_idx;
            if masked & letter_bit(letter) != 0 {
                continue;
            }
            let mask_a = self.pos_mask_for(idx_a, letter);
            let mask_b = self.pos_mask_for(idx_b, letter);
            // Both non-zero and different → 2 distinct hit partitions, 0 misses.
            if mask_a != 0 && mask_b != 0 && mask_a != mask_b {
                return 0;
            }
        }
        1
    }

    /// Fast path for exactly 3 distinct words.
    ///
    /// Directly evaluates all letters without the overhead of partition
    /// sorting, deduplication, or move ordering. For 3 words, each letter
    /// creates at most 3 partitions, each resolvable via `solve_two_words`.
    fn solve_three_words(
        &self,
        idx_a: usize,
        idx_b: usize,
        idx_c: usize,
        masked: LetterSet,
    ) -> u32 {
        let mut best = u32::MAX;
        for li in 0..26u8 {
            let letter = b'a' + li;
            if masked & letter_bit(letter) != 0 {
                continue;
            }

            let ma = self.pos_mask_for(idx_a, letter);
            let mb = self.pos_mask_for(idx_b, letter);
            let mc = self.pos_mask_for(idx_c, letter);

            // Skip letters not present in any word.
            if ma == 0 && mb == 0 && mc == 0 {
                continue;
            }

            let new_masked = masked | letter_bit(letter);

            let worst = if ma == mb && mb == mc {
                // All 3 same mask — letter doesn't discriminate.
                u32::from(ma == 0) + self.solve_three_words(idx_a, idx_b, idx_c, new_masked)
            } else if ma == mb {
                // {a,b} together, {c} alone.
                let val_ab = u32::from(ma == 0) + self.solve_two_words(idx_a, idx_b, new_masked);
                val_ab.max(u32::from(mc == 0))
            } else if ma == mc {
                let val_ac = u32::from(ma == 0) + self.solve_two_words(idx_a, idx_c, new_masked);
                val_ac.max(u32::from(mb == 0))
            } else if mb == mc {
                let val_bc = u32::from(mb == 0) + self.solve_two_words(idx_b, idx_c, new_masked);
                val_bc.max(u32::from(ma == 0))
            } else {
                // All 3 distinct masks — each word in its own partition.
                u32::from(ma == 0)
                    .max(u32::from(mb == 0))
                    .max(u32::from(mc == 0))
            };

            best = best.min(worst);
            if best == 0 {
                return 0;
            }
        }
        best
    }

    /// Fast path for exactly 4 distinct words.
    ///
    /// Directly evaluates all letters without the overhead of partition
    /// sorting, deduplication, or move ordering.
    fn solve_four_words(&self, idxs: [usize; 4], masked: LetterSet) -> u32 {
        let mut best = u32::MAX;
        for li in 0..26u8 {
            let letter = b'a' + li;
            if masked & letter_bit(letter) != 0 {
                continue;
            }

            let masks: [u32; 4] = std::array::from_fn(|i| self.pos_mask_for(idxs[i], letter));

            // Skip letters not present in any word.
            if masks[0] == 0 && masks[1] == 0 && masks[2] == 0 && masks[3] == 0 {
                continue;
            }

            let new_masked = masked | letter_bit(letter);

            // Group words by their pos_mask to find partitions.
            // With 4 words, there are at most 4 distinct masks.
            let worst = self.evaluate_four_word_partitions(&idxs, &masks, new_masked);

            best = best.min(worst);
            if best == 0 {
                return 0;
            }
        }
        best
    }

    /// Helper: evaluate partitions formed by 4 words with given masks.
    fn evaluate_four_word_partitions(
        &self,
        idxs: &[usize; 4],
        masks: &[u32; 4],
        masked: LetterSet,
    ) -> u32 {
        // Group by mask value. At most 4 groups.
        let mut groups: [(u32, [usize; 4], u8); 4] = [(0, [0; 4], 0); 4];
        let mut n_groups: usize = 0;

        for i in 0..4 {
            let mut found = false;
            for group in groups.iter_mut().take(n_groups) {
                if group.0 == masks[i] {
                    let cnt = group.2 as usize;
                    group.1[cnt] = idxs[i];
                    group.2 += 1;
                    found = true;
                    break;
                }
            }
            if !found {
                groups[n_groups].0 = masks[i];
                groups[n_groups].1[0] = idxs[i];
                groups[n_groups].2 = 1;
                n_groups += 1;
            }
        }

        let mut worst = 0u32;
        for group in groups.iter().take(n_groups) {
            let miss_cost = u32::from(group.0 == 0);
            let cnt = group.2 as usize;
            let val = miss_cost
                + match cnt {
                    1 => 0,
                    2 => self.solve_two_words(group.1[0], group.1[1], masked),
                    3 => self.solve_three_words(group.1[0], group.1[1], group.1[2], masked),
                    4 => self
                        .solve_four_words([group.1[0], group.1[1], group.1[2], group.1[3]], masked),
                    _ => unreachable!(),
                };
            worst = worst.max(val);
        }
        worst
    }

    /// Solve a subproblem with alpha-beta pruning.
    ///
    /// `beta` is an upper bound from the caller: if our value is >= beta, the
    /// caller will prune, so we can return early. Only exact values (those
    /// proven < beta) are cached.
    /// Analyze required letters (present in ALL words).
    ///
    /// Returns `(new_masked, required_splitting)` where:
    /// - `new_masked` has non-splitting required letters added (free guesses)
    /// - `required_splitting` is a bitmask of required letters that DO split
    ///   (different `pos_mask` across words). When non-zero, the guesser should
    ///   ONLY evaluate these letters — non-required letters are provably
    ///   suboptimal because they risk a miss while required letters don't.
    fn analyze_required_letters(
        &self,
        indices: &[usize],
        mut masked: LetterSet,
    ) -> (LetterSet, LetterSet) {
        // Fast path: compute word_letters intersection to find required letters.
        // Bail early when intersection drops to zero (no required letters) —
        // this is the common case and avoids the O(26) letter-by-letter scan.
        let mut intersection = u32::MAX;
        for &idx in indices {
            intersection &= self.data.word_letters[idx];
            if intersection & !masked == 0 {
                return (masked, 0);
            }
        }
        let candidates = intersection & !masked;

        // Only check letters that are in ALL words (typically 0-5).
        let mut required_splitting: LetterSet = 0;
        let mut bits = candidates;
        while bits != 0 {
            #[allow(clippy::cast_possible_truncation)]
            let li = bits.trailing_zeros() as u8;
            bits &= bits - 1;
            let letter = b'a' + li;
            let first_mask = self.pos_mask_for(indices[0], letter);
            let all_same_mask = indices[1..]
                .iter()
                .all(|&idx| self.pos_mask_for(idx, letter) == first_mask);
            if all_same_mask {
                masked |= letter_bit(letter);
            } else {
                required_splitting |= letter_bit(letter);
            }
        }
        (masked, required_splitting)
    }

    /// Result of a transposition table lookup.
    fn cache_lookup(&self, key: u128, mut alpha: u32, mut beta: u32) -> CacheLookup {
        if let Some(packed) = self.data.cache.get(&key) {
            let (val, best_letter, bound) = cache_unpack(*packed);
            match bound {
                BOUND_EXACT => {
                    self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return CacheLookup::Hit(val);
                }
                BOUND_LOWER => {
                    if val >= beta {
                        self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    // Tighten alpha with lower bound.
                    alpha = alpha.max(val);
                }
                BOUND_UPPER => {
                    if val <= alpha {
                        self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    // Tighten beta with upper bound.
                    beta = beta.min(val);
                }
                _ => {}
            }
            if alpha >= beta {
                self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                return CacheLookup::Hit(val);
            }
            CacheLookup::Miss {
                tt_move: best_letter,
                alpha,
                beta,
            }
        } else if let Some(packed) = self.data.disk_cache.as_ref().and_then(|dc| dc.get(key)) {
            // Promote to in-memory cache for fast subsequent lookups.
            self.data.cache.insert(key, packed);
            let (val, best_letter, bound) = cache_unpack(packed);
            match bound {
                BOUND_EXACT => {
                    self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return CacheLookup::Hit(val);
                }
                BOUND_LOWER => {
                    if val >= beta {
                        self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    alpha = alpha.max(val);
                }
                BOUND_UPPER => {
                    if val <= alpha {
                        self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    beta = beta.min(val);
                }
                _ => {}
            }
            if alpha >= beta {
                self.data.cache_hits.fetch_add(1, Ordering::Relaxed);
                return CacheLookup::Hit(val);
            }
            CacheLookup::Miss {
                tt_move: best_letter,
                alpha,
                beta,
            }
        } else {
            CacheLookup::Miss {
                tt_move: None,
                alpha,
                beta,
            }
        }
    }

    /// Choose which letters to evaluate. Computes partition sizes lazily
    /// in batches: scores the first 8 letters by frequency, sorts
    /// them, and returns them. Remaining letters are appended in frequency
    /// order without scoring — if the first batch doesn't produce a cutoff,
    /// the remaining letters are unlikely to either.
    fn select_letters(
        &self,
        indices: &[usize],
        masked: LetterSet,
        required_splitting: LetterSet,
    ) -> Vec<u8> {
        let candidate_mask = if required_splitting != 0 {
            required_splitting
        } else {
            self.present_letters(indices, masked)
        };
        if candidate_mask == 0 {
            return vec![];
        }
        let n_candidates = candidate_mask.count_ones();
        // Few candidates: sort by history score with frequency tiebreaker.
        if n_candidates <= 6 {
            let mut letters: Vec<u8> = (0..26u8)
                .filter(|&i| candidate_mask & (1 << i) != 0)
                .map(|i| b'a' + i)
                .collect();
            letters.sort_by(|&a, &b| {
                let ha = self.data.history[(a - b'a') as usize].load(Ordering::Relaxed);
                let hb = self.data.history[(b - b'a') as usize].load(Ordering::Relaxed);
                hb.cmp(&ha).then_with(|| {
                    self.data.freq_rank[(a - b'a') as usize]
                        .cmp(&self.data.freq_rank[(b - b'a') as usize])
                })
            });
            return letters;
        }
        // Score candidates by partition size. For small word sets, score all
        // candidates (cost is O(n_candidates × n), negligible when n is small).
        // For larger sets, only score the top 8 by history.
        let score_all = (n_candidates as usize) * indices.len() < 50_000;
        let batch_size = if score_all {
            n_candidates as usize
        } else {
            8
        };

        // Build candidate list sorted by history (descending), freq tiebreaker.
        let mut candidates: Vec<(u8, u32, u8)> = (0..26u8)
            .filter(|&i| candidate_mask & (1 << i) != 0)
            .map(|i| {
                let letter = b'a' + i;
                let hist = self.data.history[i as usize].load(Ordering::Relaxed);
                let rank = self.data.freq_rank[i as usize];
                (letter, hist, rank)
            })
            .collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));

        let mut counts: FxHashMap<u32, usize> = FxHashMap::default();
        let mut scored: Vec<(u8, usize)> = Vec::with_capacity(batch_size);
        let mut remaining: Vec<u8> = Vec::new();
        for (letter, _, _) in &candidates {
            if scored.len() < batch_size {
                counts.clear();
                let mut max_part = 0usize;
                for &idx in indices {
                    let mask = self.pos_mask_for(idx, *letter);
                    let count = counts.entry(mask).or_insert(0);
                    *count += 1;
                    max_part = max_part.max(*count);
                }
                scored.push((*letter, max_part));
            } else {
                remaining.push(*letter);
            }
        }
        scored.sort_by_key(|&(_, mp)| mp);
        // remaining is already in history order (descending) from candidates.
        let mut result: Vec<u8> = scored.into_iter().map(|(letter, _)| letter).collect();
        result.extend(remaining);
        result
    }

    /// Lower bound via the "miss chain" argument: when no letter is required,
    /// every guess risks a miss. The referee can always choose the miss
    /// partition, so `value >= min_L (1 + lb(miss_partition(L)))`.
    ///
    /// `max_depth` limits recursion for performance.
    fn miss_chain_lower_bound(
        &self,
        indices: &[usize],
        masked: LetterSet,
        max_depth: u32,
    ) -> u32 {
        // At the top level, `masked` already has non-splitting required letters
        // folded in (from analyze_required_letters), so we skip the redundant
        // required-letter scan. Recursive calls still need it since we haven't
        // pre-analyzed those subsets.
        self.miss_chain_lb_inner(indices, masked, max_depth, true)
    }

    fn miss_chain_lb_inner(
        &self,
        indices: &[usize],
        mut masked: LetterSet,
        max_depth: u32,
        skip_required_scan: bool,
    ) -> u32 {
        if indices.len() <= 1 || max_depth == 0 {
            return 0;
        }

        if !skip_required_scan {
            // Collapse required letters (in all words) — free, no miss risk.
            let mut intersection = u32::MAX;
            for &idx in indices {
                intersection &= self.data.word_letters[idx];
                if intersection & !masked == 0 {
                    break;
                }
            }
            let candidates = intersection & !masked;
            let mut bits = candidates;
            while bits != 0 {
                #[allow(clippy::cast_possible_truncation)]
                let li = bits.trailing_zeros() as u8;
                bits &= bits - 1;
                let letter = b'a' + li;
                // Check if it's actually required (present in all words, not
                // just in the intersection of word_letters which could include
                // masked letters). Since we filtered by !masked, all
                // intersection bits are unmasked. Just mask them.
                masked |= letter_bit(letter);
            }
        }

        let present = self.present_letters(indices, masked);
        if present == 0 {
            return 0;
        }

        // All useful letters are non-required → at least 1 miss is guaranteed.
        // Find the letter that minimizes (1 + lb(miss_partition)).
        let mut best = u32::MAX;
        for li in 0..26u8 {
            if present & (1u32 << li) == 0 {
                continue;
            }
            let letter = b'a' + li;
            let miss: Vec<usize> = indices
                .iter()
                .filter(|&&idx| self.pos_mask_for(idx, letter) == 0)
                .copied()
                .collect();
            if miss.is_empty() {
                continue;
            }
            if miss.len() <= 1 {
                return 1;
            }
            let val = 1
                + self.miss_chain_lb_inner(&miss, masked | letter_bit(letter), max_depth - 1, false);
            best = best.min(val);
            if best <= 1 {
                break;
            }
        }
        if best == u32::MAX { 0 } else { best }
    }

    /// Compute a cheap order-independent hash of (indices, masked) for the
    /// key cache. Uses a commutative mixing function so that different
    /// orderings of the same index set produce the same hash.
    fn fast_index_key(indices: &[usize], masked: LetterSet) -> u64 {
        let mut h = 0u64;
        let mut xor = 0u64;
        for &idx in indices {
            let mix = (idx as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
            h = h.wrapping_add(mix);
            xor ^= mix;
        }
        h = h.wrapping_add(xor.rotate_left(17));
        h ^= u64::from(masked).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= (indices.len() as u64) << 48;
        h ^ (h >> 33)
    }

    #[allow(clippy::too_many_lines)]
    fn solve_subset(&self, indices: &[usize], masked: LetterSet, alpha: u32, beta: u32) -> u32 {
        if indices.len() <= 1 || beta == 0 {
            return 0;
        }
        // Early exit if another thread has found the answer.
        // Return beta (pessimistic) so this value doesn't get cached as a
        // good result that poisons the TT.
        if self.data.cancelled.load(Ordering::Relaxed) {
            return beta;
        }

        // Analyze required letters: collapse non-splitting ones (free guesses)
        // and identify splitting ones (the only candidates we need to evaluate
        // when present, since non-required letters risk a miss).
        let (masked, required_splitting) = self.analyze_required_letters(indices, masked);

        // Quick pruning via miss-chain lower bound before expensive canonicalization.
        // When no required-splitting letters exist, every unmasked letter risks a
        // miss — the referee can always pick the miss partition.
        //
        // Depth=1 proves lb >= 1 for free (no allocation needed): after
        // analyze_required_letters folded non-splitting required letters into
        // masked, any remaining present letter must miss on some word. So if
        // present != 0, the guesser faces at least 1 miss no matter what.
        //
        // Depth=2-3 for large nodes catches deeper miss chains but is expensive.
        if required_splitting == 0 && indices.len() >= 5 {
            if indices.len() >= 5000 {
                let depth = if indices.len() >= 40_000 { 3 } else { 2 };
                let lb = self.miss_chain_lower_bound(indices, masked, depth);
                if lb >= beta {
                    return lb;
                }
            }
            // Depth=1 fast path: lb is exactly 1 if any present letter exists.
            if beta <= 1 {
                let present = self.present_letters(indices, masked);
                if present != 0 {
                    return 1;
                }
            }
        }

        // Fast key cache: map (indices, masked) → canonical_key. On cache
        // hit, check TT immediately — skip expensive dedup_and_hash entirely
        // for TT hits. Only recompute on TT miss.
        let fast_key = Self::fast_index_key(indices, masked);
        let (indices, cache_key) = if let Some(entry) = self.data.key_cache.get(&fast_key) {
            let canon_key = *entry;
            drop(entry);
            self.data.hash_calls.fetch_add(1, Ordering::Relaxed);
            // Check TT — most visits are hits, avoiding dedup_and_hash entirely.
            if let CacheLookup::Hit(val) = self.cache_lookup(canon_key, alpha, beta) {
                return val;
            }
            // TT miss: only need deduped indices (skip canonicalization).
            let deduped = dedup_only(&self.data.words, indices, masked);
            (deduped, canon_key)
        } else {
            let (deduped, canon_key) = dedup_and_hash(&self.data.words, indices, masked);
            self.data.key_cache.insert(fast_key, canon_key);
            (deduped, canon_key)
        };

        if indices.len() <= 1 {
            return 0;
        }

        self.data.hash_calls.fetch_add(1, Ordering::Relaxed);

        // Tighten beta using worst-case bound.
        let present = self.present_letters(&indices, masked);
        let n_useful = present.count_ones();
        let n_required = required_splitting.count_ones();
        let max_misses = n_useful - n_required;
        let beta = beta.min(max_misses);
        if alpha >= beta {
            return alpha;
        }

        let (tt_move, alpha, beta) = match self.cache_lookup(cache_key, alpha, beta) {
            CacheLookup::Hit(val) => return val,
            CacheLookup::Miss {
                tt_move,
                alpha,
                beta,
            } => (tt_move, alpha, beta),
        };

        // Fast path for exactly 2 words.
        if indices.len() == 2 {
            let val = self.solve_two_words(indices[0], indices[1], masked);
            self.data
                .cache
                .insert(cache_key, cache_pack(val, 0, BOUND_EXACT));
            return val;
        }

        // Fast path for exactly 3 words — avoids partition/ordering overhead.
        if indices.len() == 3 {
            let val = self.solve_three_words(indices[0], indices[1], indices[2], masked);
            self.data
                .cache
                .insert(cache_key, cache_pack(val, 0, BOUND_EXACT));
            return val;
        }

        // Fast path for exactly 4 words.
        if indices.len() == 4 {
            let val =
                self.solve_four_words([indices[0], indices[1], indices[2], indices[3]], masked);
            self.data
                .cache
                .insert(cache_key, cache_pack(val, 0, BOUND_EXACT));
            return val;
        }

        let mut letters = self.select_letters(&indices, masked, required_splitting);
        if letters.is_empty() {
            self.data.cache.insert(cache_key, 0);
            return 0;
        }

        // TT move ordering: primary thread uses TT move first for optimal
        // pruning; secondary threads skip it for search diversity.
        if self.perturbation == 0
            && let Some(tt_letter) = tt_move
            && let Some(pos) = letters.iter().position(|&l| l == tt_letter)
            && pos > 0
        {
            letters[..=pos].rotate_right(1);
        }

        // Progress tracking: only the main OS thread pushes frames, at large
        // nodes only. Rayon workers share perturbation==0 but run on different
        // threads, so checking the thread ID prevents stack corruption.
        let tracking = self.perturbation == 0
            && indices.len() >= PAR_THRESHOLD
            && std::thread::current().id() == self.data.main_thread_id;
        if tracking {
            let frame = ProgressFrame {
                start_secs: self.data.solve_start.elapsed().as_secs_f64(),
                #[allow(clippy::cast_possible_truncation)]
                total_moves: letters.len() as u32,
                completed_moves: 0,
            };
            self.data.progress.lock().unwrap().push(frame);
        }

        let (best, best_letter) = if self.use_rayon && indices.len() >= PAR_THRESHOLD {
            self.solve_parallel(&letters, &indices, masked, alpha, beta)
        } else {
            self.solve_sequential(&letters, &indices, masked, alpha, beta)
        };

        if tracking {
            self.data.progress.lock().unwrap().pop();
        }

        // History heuristic: boost the best letter's score, weighted by
        // word count (larger nodes → more pruning value from good ordering).
        if best_letter != 0 {
            #[allow(clippy::cast_possible_truncation)]
            let bonus = (indices.len() / 10).clamp(1, 100) as u32;
            self.data.history[(best_letter - b'a') as usize]
                .fetch_add(bonus, Ordering::Relaxed);
        }

        self.cache_store(cache_key, best, best_letter, alpha, beta);
        best
    }

    /// Store a result in the cache with correct bound type.
    fn cache_store(&self, key: u128, best: u32, best_letter: u8, alpha: u32, beta: u32) {
        // Don't cache results from cancelled searches — values are unreliable.
        if self.data.cancelled.load(Ordering::Relaxed) {
            return;
        }
        if best <= alpha {
            // Upper bound — value is at most `best`. Never overwrite exact.
            let packed = cache_pack(best, best_letter, BOUND_UPPER);
            if let Some(mut existing) = self.data.cache.get_mut(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                if old_bound == BOUND_UPPER && best < old_val {
                    *existing = packed; // tighter upper bound
                }
                // Don't overwrite exact or lower bounds.
            } else {
                self.data.cache.insert(key, packed);
            }
        } else if best >= beta {
            // Lower bound — value is at least `best`. Never overwrite exact.
            let packed = cache_pack(best, best_letter, BOUND_LOWER);
            if let Some(mut existing) = self.data.cache.get_mut(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                if old_bound == BOUND_LOWER && best > old_val {
                    *existing = packed; // tighter lower bound
                }
            } else {
                self.data.cache.insert(key, packed);
            }
        } else {
            // Exact value — always store (overwrites any bound).
            self.data
                .cache
                .insert(key, cache_pack(best, best_letter, BOUND_EXACT));
        }
    }

    fn solve_sequential(
        &self,
        letters: &[u8],
        indices: &[usize],
        masked: LetterSet,
        alpha: u32,
        beta: u32,
    ) -> (u32, u8) {
        let tracking = self.perturbation == 0
            && std::thread::current().id() == self.data.main_thread_id;
        let mut best = u32::MAX;
        let mut best_letter = letters[0];
        for &letter in letters {
            let cutoff = best.min(beta);
            let val = self.evaluate_letter(indices, masked, letter, cutoff);
            if val < best {
                best = val;
                best_letter = letter;
            }
            if tracking
                && let Some(frame) = self.data.progress.lock().unwrap().last_mut()
            {
                frame.completed_moves += 1;
            }
            if best <= alpha || best == 0 {
                break;
            }
        }
        (best, best_letter)
    }

    /// YBWC-style parallel search: evaluate the first (best-ordered) letter
    /// sequentially to establish a tight bound, then parallelize the rest.
    fn solve_parallel(
        &self,
        letters: &[u8],
        indices: &[usize],
        masked: LetterSet,
        alpha: u32,
        beta: u32,
    ) -> (u32, u8) {
        let tracking = self.perturbation == 0
            && std::thread::current().id() == self.data.main_thread_id;
        // Evaluate first letter sequentially to get a tight initial bound.
        let first_val = self.evaluate_letter(indices, masked, letters[0], beta);
        let mut best = first_val;
        let mut best_letter = letters[0];

        if tracking
            && let Some(frame) = self.data.progress.lock().unwrap().last_mut()
        {
            frame.completed_moves += 1;
        }

        if best <= alpha || best == 0 || letters.len() <= 1 {
            return (best, best_letter);
        }

        // Parallelize remaining letters with the tighter bound from first.
        let shared_cutoff = std::sync::atomic::AtomicU32::new(best.min(beta));
        let rest = letters[1..]
            .par_iter()
            .map(|&letter| {
                let current_cutoff = shared_cutoff.load(Ordering::Relaxed);
                if current_cutoff == 0 || current_cutoff <= alpha {
                    return (u32::MAX, letter);
                }
                let val = self.evaluate_letter(indices, masked, letter, current_cutoff);
                shared_cutoff.fetch_min(val, Ordering::Relaxed);
                (val, letter)
            })
            .reduce(
                || (u32::MAX, 0),
                |(v1, l1), (v2, l2)| if v1 <= v2 { (v1, l1) } else { (v2, l2) },
            );

        // Mark all remaining letters as completed after parallel section.
        if tracking
            && let Some(frame) = self.data.progress.lock().unwrap().last_mut()
        {
            frame.completed_moves = frame.total_moves;
        }

        if rest.0 < best {
            best = rest.0;
            best_letter = rest.1;
        }

        (best, best_letter)
    }

    fn evaluate_letter(
        &self,
        indices: &[usize],
        masked: LetterSet,
        letter: u8,
        cutoff: u32,
    ) -> u32 {
        // Group word indices by pos_mask using a hash map (O(n)) instead
        // of sorting (O(n log n)). Each partition is a (pos_mask, Vec<idx>).
        // Pre-size to avoid rehashing — at most word_length+1 distinct masks.
        let mut partitions: FxHashMap<u32, Vec<usize>> =
            FxHashMap::with_capacity_and_hasher(32, Default::default());
        for &idx in indices {
            let mask = self.pos_mask_for(idx, letter);
            partitions.entry(mask).or_default().push(idx);
        }

        // Order: miss partition first (pos_mask == 0), then largest hit
        // partitions first. Collect into a vec of (pos_mask, indices).
        let mut ordered: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();
        ordered.sort_unstable_by(|a, b| {
            let a_is_miss = u32::from(a.0 != 0);
            let b_is_miss = u32::from(b.0 != 0);
            a_is_miss.cmp(&b_is_miss).then(b.1.len().cmp(&a.1.len()))
        });

        let new_masked = masked | letter_bit(letter);
        let mut worst = 0u32;

        let mut established = false;
        for &(pos_mask, ref subset) in &ordered {
            let miss_cost = u32::from(pos_mask == 0);

            // If miss_cost alone meets cutoff, prune without recursion.
            if miss_cost >= cutoff {
                return cutoff;
            }

            // Fast inline resolution for tiny partitions (avoids solve_subset
            // overhead: analyze_required, fast_index_key, dedup_and_hash, TT).
            if subset.len() <= 1 {
                worst = worst.max(miss_cost);
                established = true;
                if worst >= cutoff {
                    return worst;
                }
                continue;
            }
            if subset.len() == 2 {
                let val = miss_cost + self.solve_two_words(subset[0], subset[1], new_masked);
                worst = worst.max(val);
                established = true;
                if worst >= cutoff {
                    return worst;
                }
                continue;
            }

            // Cheap upper bound: partition value ≤ number of present unmasked
            // letters. Skip if that can't raise worst.
            let n_useful = self.present_letters(subset, new_masked).count_ones();
            if miss_cost + n_useful <= worst {
                continue;
            }

            let child_alpha = worst.saturating_sub(miss_cost);
            let child_beta = cutoff - miss_cost;

            // Null-window scout: after the first partition establishes a
            // bound, probe remaining partitions with a width-1 window to
            // test whether they exceed `worst`. Re-search on fail-high.
            if established && child_beta > child_alpha + 3 {
                let scout =
                    miss_cost + self.solve_subset(subset, new_masked, child_alpha, child_alpha + 1);
                if scout <= worst {
                    continue; // partition doesn't raise worst
                }
                // Fall through to full-window re-search.
            }

            let val = miss_cost + self.solve_subset(subset, new_masked, child_alpha, child_beta);

            worst = worst.max(val);
            established = true;
            if worst >= cutoff {
                return worst;
            }
        }

        worst
    }

    /// Recursively warm the cache for all reachable positions.
    ///
    /// For each unguessed letter, partitions the words and ensures each
    /// partition has an EXACT entry. Recurses into partitions so that
    /// the referee can respond optimally to any sequence of user guesses.
    ///
    /// Uses `visited` to avoid re-traversing positions reached via
    /// different transposition paths (same canonical key).
    fn warm_recursive(
        &self,
        indices: &[usize],
        masked: LetterSet,
        solved: &mut usize,
        visited: &mut rustc_hash::FxHashSet<u128>,
    ) {
        if indices.len() <= 1 {
            return;
        }

        // Ensure this position itself has an EXACT entry.
        let (masked, _) = self.analyze_required_letters(indices, masked);
        let (deduped, cache_key) = dedup_and_hash(&self.data.words, indices, masked);
        if deduped.len() <= 1 {
            return;
        }

        // Skip if we've already fully warmed this canonical position.
        if !visited.insert(cache_key) {
            return;
        }

        let indices = &deduped;

        // Check if we already have an EXACT entry.
        let need_solve = if let Some(packed) = self.data.cache.get(&cache_key) {
            let (_, _, bound) = cache_unpack(*packed);
            bound != BOUND_EXACT
        } else if let Some(packed) = self.data.disk_cache.as_ref().and_then(|dc| dc.get(cache_key)) {
            let (_, _, bound) = cache_unpack(packed);
            if bound == BOUND_EXACT {
                // Promote to in-memory cache.
                self.data.cache.insert(cache_key, packed);
                false
            } else {
                true
            }
        } else {
            true
        };

        if need_solve {
            self.solve_subset(indices, masked, 0, u32::MAX);
            *solved += 1;
        }

        // Now enumerate all 26 letters and recurse into each partition.
        let present = self.present_letters(indices, masked);
        for li in 0..26u8 {
            if masked & (1 << li) != 0 {
                continue; // Already guessed.
            }
            if present & (1 << li) == 0 {
                continue; // Not present in any word — miss with no split.
            }
            let letter = b'a' + li;
            let new_masked = masked | letter_bit(letter);

            // Partition by pos_mask.
            let mut partitions: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
            for &idx in indices {
                let mask = self.pos_mask_for(idx, letter);
                partitions.entry(mask).or_default().push(idx);
            }

            for (_, subset) in &partitions {
                if subset.len() > 1 {
                    self.warm_recursive(subset, new_masked, solved, visited);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Send + Sync for rayon
// ---------------------------------------------------------------------------

// MemoizedSolverInner is Send+Sync: Arc<SolverData> is Send+Sync,
// u32 and bool are Send+Sync.
// SolverData is Send+Sync: DashMap is Send+Sync, words is Vec<Vec<u8>>
// (Send+Sync), atomics are Send+Sync.

#[cfg(test)]
mod tests {
    use super::super::canon::canonicalize;
    use super::super::naive::NaiveSolver;
    use super::*;

    fn solve_both(words: &[&str]) -> u32 {
        let owned: Vec<Vec<u8>> = words.iter().map(|s| s.as_bytes().to_vec()).collect();
        let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let naive_result = NaiveSolver::solve(&refs, 0);

        let solver = MemoizedSolver::new();
        let memo_result = solver.solve(&owned);

        assert_eq!(
            naive_result, memo_result,
            "naive={naive_result} != memoized={memo_result} for {words:?}"
        );
        memo_result
    }

    #[test]
    fn single_word() {
        assert_eq!(solve_both(&["cat"]), 0);
    }

    #[test]
    fn two_words_one_diff() {
        assert_eq!(solve_both(&["cat", "bat"]), 1);
    }

    #[test]
    fn three_words() {
        assert_eq!(solve_both(&["cat", "bat", "hat"]), 2);
    }

    #[test]
    fn disjoint_words() {
        assert_eq!(solve_both(&["ab", "cd"]), 1);
    }

    #[test]
    fn longer_example() {
        assert_eq!(solve_both(&["abc", "abd", "xyz"]), 1);
    }

    #[test]
    fn four_words_same_suffix() {
        solve_both(&["cat", "bat", "hat", "mat"]);
    }

    #[test]
    fn two_letter_words() {
        solve_both(&["ab", "ac", "ad", "bc", "bd", "cd"]);
    }

    #[test]
    fn all_same_structure() {
        solve_both(&["ba", "ca", "da", "fa", "ga"]);
    }

    #[test]
    fn solve_returns_correct_value() {
        let words: Vec<Vec<u8>> = ["cat", "bat", "hat", "mat"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let solver = MemoizedSolver::new();
        let result = solver.solve(&words);
        // Verified against naive solver in `four_words_same_suffix`.
        assert!(result <= 25, "result should be a valid miss count");
    }

    #[test]
    fn two_words_shared_letter_diff_pos() {
        // "ab" and "ba" share 'a' and 'b' at different positions.
        // Guessing 'a' splits into two hits → 0 misses.
        assert_eq!(solve_both(&["ab", "ba"]), 0);
    }

    #[test]
    fn two_words_no_shared_letters() {
        // "ab" and "cd" share no letters → must miss on one → 1 miss.
        assert_eq!(solve_both(&["ab", "cd"]), 1);
    }

    #[test]
    fn two_words_same_pos_diff_letter() {
        // "ab" and "ac" differ only at pos 1. Guessing 'b': hit+miss. 1 miss.
        assert_eq!(solve_both(&["ab", "ac"]), 1);
    }

    #[test]
    fn isomorphic_sets_share_cache() {
        // Verify isomorphic word sets produce the same result.
        let solver = MemoizedSolver::new();
        let r1 = solver.solve(&[b"ab".to_vec(), b"cd".to_vec()]);
        let r2 = solver.solve(&[b"ef".to_vec(), b"gh".to_vec()]);
        assert_eq!(r1, r2);
    }

    #[test]
    fn position_isomorphic_share_cache() {
        // "ab","cd" and "ba","dc" differ only by column swap — same game.
        let solver = MemoizedSolver::new();
        let r1 = solver.solve(&[b"ab".to_vec(), b"cd".to_vec()]);
        let r2 = solver.solve(&[b"ba".to_vec(), b"dc".to_vec()]);
        assert_eq!(r1, r2);
    }

    #[test]
    fn canonicalize_isomorphic() {
        let a = vec![b"ab".to_vec(), b"cd".to_vec()];
        let b = vec![b"ef".to_vec(), b"gh".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn canonicalize_position_swap() {
        let a = vec![b"ab".to_vec(), b"cd".to_vec()];
        let b = vec![b"ba".to_vec(), b"dc".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn canonicalize_non_isomorphic() {
        let a = vec![b"ab".to_vec(), b"ac".to_vec()];
        let b = vec![b"de".to_vec(), b"fg".to_vec()];
        assert_ne!(canonicalize(&a), canonicalize(&b));
    }

    /// Verify that `solve_position_smp` stores EXACT entries that the server
    /// can look up via `fold_required_letters` + `canonical_hash_for_words`.
    ///
    /// Regression test: MTD(f)'s verification pass had its window narrowed by
    /// UPPER_BOUND TT entries from earlier iterations, causing the root entry
    /// to be stored as LOWER_BOUND instead of EXACT. The server's
    /// `decode_tt_entry` filters non-EXACT entries, causing cache misses.
    #[test]
    fn solve_position_smp_stores_exact_entries() {
        use super::super::serving::{
            canonical_hash_for_words, decode_tt_entry, fold_required_letters,
        };
        use crate::game::letter_bit;

        // Words that produce non-trivial partitions after guessing 'a'.
        let words: Vec<Vec<u8>> = ["cat", "bat", "hat", "mat", "dog", "fog", "log", "hog"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let all_indices: Vec<usize> = (0..words.len()).collect();

        // Partition by letter 'a': miss partition (dog/fog/log/hog) and
        // hit partition (cat/bat/hat/mat).
        let masked = letter_bit(b'a');
        let mut partitions: std::collections::HashMap<u32, Vec<usize>> =
            std::collections::HashMap::new();
        for &i in &all_indices {
            let mut pmask = 0u32;
            for (j, &b) in words[i].iter().enumerate() {
                if b == b'a' {
                    pmask |= 1 << j;
                }
            }
            partitions.entry(pmask).or_default().push(i);
        }

        let solver = MemoizedSolver::new();

        for (_pmask, indices) in &partitions {
            if indices.len() <= 1 {
                continue;
            }
            solver.solve_position_smp(&words, indices, masked);
        }

        // Verify: every non-trivial partition has an EXACT entry in the
        // solver's cache, retrievable via the server's lookup path.
        for (_pmask, indices) in &partitions {
            if indices.len() <= 1 {
                continue;
            }
            let folded = fold_required_letters(&words, indices, masked);
            let hash = canonical_hash_for_words(&words, indices, folded);

            let packed = solver
                .cache
                .get(&hash)
                .map(|v| *v)
                .expect("root entry should be in cache");
            let entry = decode_tt_entry(packed);
            assert!(
                entry.is_some(),
                "root entry for partition of {} words should be EXACT, \
                 got packed={:#x} (bound={})",
                indices.len(),
                packed,
                cache_unpack(packed).2,
            );
        }
    }
}
