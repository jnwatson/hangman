use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::canon::dedup_and_hash;
use crate::game::{LetterSet, letter_bit};

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
fn cache_unpack(packed: u32) -> (u32, Option<u8>, u32) {
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
        }
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
        let data = Arc::new(SolverData::new(words.to_vec()));
        let indices: Vec<usize> = (0..words.len()).collect();

        let result = if words.len() <= 25_000 {
            // MTD(f)-style iterative deepening for word lists with shallow
            // minimax trees. Each iteration fills the TT with best moves,
            // improving subsequent iterations' move ordering.
            let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
            let mut result = 0;
            for target in 0..26u32 {
                result = solver.solve_subset(&indices, 0, 0, target + 1);
                if result <= target {
                    break;
                }
            }
            result
        } else {
            // Lazy SMP: primary thread uses rayon (YBWC) for internal
            // parallelism; a few extra sequential threads explore with
            // perturbed orderings, sharing the TT for mutual pruning.
            let n_extra = std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(1)
                / 4;
            let n_extra = n_extra.clamp(1, 3);

            std::thread::scope(|s| {
                // Spawn extra sequential threads with perturbed orderings.
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

                // Primary thread uses rayon for YBWC parallelism.
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
        // Only transfer exact values to the persistent cache.
        for entry in &data.cache {
            let (_, _, bound) = cache_unpack(*entry.value());
            if bound == BOUND_EXACT {
                self.cache.insert(*entry.key(), *entry.value());
            }
        }

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
        let data = Arc::new(SolverData::new(words.to_vec()));
        // Seed with any exact entries from prior calls.
        for entry in &self.cache {
            data.cache.insert(*entry.key(), *entry.value());
        }
        let indices: Vec<usize> = (0..words.len()).collect();

        let result = if words.len() <= 25_000 {
            let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
            solver.solve_subset(&indices, 0, 0, max_misses + 1)
        } else {
            let n_extra = std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(1)
                / 4;
            let n_extra = n_extra.clamp(1, 3);

            std::thread::scope(|s| {
                #[allow(clippy::cast_possible_truncation)]
                let handles: Vec<_> = (1..=n_extra as u32)
                    .map(|tid| {
                        let data = Arc::clone(&data);
                        let indices = indices.clone();
                        s.spawn(move || {
                            let solver = MemoizedSolverInner::new(data, tid, false);
                            solver.solve_subset(&indices, 0, 0, max_misses + 1)
                        })
                    })
                    .collect();

                let main_result = {
                    let solver = MemoizedSolverInner::new(Arc::clone(&data), 0, true);
                    solver.solve_subset(&indices, 0, 0, max_misses + 1)
                };

                let mut best = main_result;
                for h in handles {
                    best = best.min(h.join().unwrap());
                }
                best
            })
        };

        self.hash_calls
            .fetch_add(data.hash_calls.load(Ordering::Relaxed), Ordering::Relaxed);
        self.cache_hits
            .fetch_add(data.cache_hits.load(Ordering::Relaxed), Ordering::Relaxed);
        for entry in &data.cache {
            let (_, _, bound) = cache_unpack(*entry.value());
            if bound == BOUND_EXACT {
                self.cache.insert(*entry.key(), *entry.value());
            }
        }

        result
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
    hash_calls: AtomicU64,
    cache_hits: AtomicU64,
    /// History heuristic: letters that are good moves get higher scores.
    /// Used to improve move ordering beyond partition-size heuristic.
    history: [AtomicU64; 26],
}

impl SolverData {
    fn new(words: Vec<Vec<u8>>) -> Self {
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
        Self {
            words,
            pos_masks,
            word_letters,
            cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            history: std::array::from_fn(|_| AtomicU64::new(0)),
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

    /// Deduplicate letters by partition structure and sort by max partition size.
    fn dedup_and_order_letters(&self, letters: &[u8], indices: &[usize]) -> Vec<u8> {
        let mut seen: FxHashSet<u64> = FxHashSet::default();
        // (letter, max_part_size, _unused, num_parts)
        let mut result: Vec<(u8, usize, usize, usize)> = Vec::new();
        let mut counts: FxHashMap<u32, usize> = FxHashMap::default();
        let mut fingerprint: Vec<(usize, bool)> = Vec::new();

        for &letter in letters {
            counts.clear();
            let mut max_part = 0usize;
            for &idx in indices {
                let mask = self.pos_mask_for(idx, letter);
                let count = counts.entry(mask).or_insert(0);
                *count += 1;
                max_part = max_part.max(*count);
            }
            let num_parts = counts.len();
            fingerprint.clear();
            fingerprint.extend(counts.iter().map(|(&mask, &count)| (count, mask == 0)));
            fingerprint.sort_unstable();

            // Hash the fingerprint to a u64 for fast set lookup.
            let fp_hash = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::hash::DefaultHasher::new();
                fingerprint.hash(&mut hasher);
                hasher.finish()
            };

            if seen.insert(fp_hash) {
                result.push((letter, max_part, 0, num_parts));
            }
        }

        // Sort by: smallest max partition first, most partitions as tiebreaker,
        // then history heuristic (perturbed for Lazy SMP diversity).
        let perturbation = self.perturbation;
        result.sort_by(|a, b| {
            // Primary: always sort by max partition size (ascending).
            let primary = a.1.cmp(&b.1);

            // Secondary: partition count. Reversed for some threads.
            let secondary = if perturbation < 2 {
                b.3.cmp(&a.3) // more partitions = better (default)
            } else {
                a.3.cmp(&b.3) // fewer partitions first (diversity)
            };

            // Tertiary: history heuristic, reversed for odd perturbation.
            let tertiary = {
                let ha = self.data.history[(a.0 - b'a') as usize].load(Ordering::Relaxed);
                let hb = self.data.history[(b.0 - b'a') as usize].load(Ordering::Relaxed);
                if perturbation.is_multiple_of(2) {
                    hb.cmp(&ha) // higher history = better (default)
                } else {
                    ha.cmp(&hb) // reversed for diversity
                }
            };

            primary.then(secondary).then(tertiary)
        });
        result.into_iter().map(|(letter, _, _, _)| letter).collect()
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
        let mut required_splitting: LetterSet = 0;
        for li in 0..26u8 {
            let letter = b'a' + li;
            if masked & letter_bit(letter) != 0 {
                continue;
            }
            let first_mask = self.pos_mask_for(indices[0], letter);
            if first_mask == 0 {
                continue; // not in first word → can't be in all
            }
            let all_match = indices[1..]
                .iter()
                .all(|&idx| self.pos_mask_for(idx, letter) != 0);
            if !all_match {
                continue; // not required
            }
            // Letter is required (in all words). Check if it splits.
            let all_same_mask = indices[1..]
                .iter()
                .all(|&idx| self.pos_mask_for(idx, letter) == first_mask);
            if all_same_mask {
                // Non-splitting: free guess, add to masked.
                masked |= letter_bit(letter);
            } else {
                // Splitting: evaluate this letter (and only required letters).
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
        } else {
            CacheLookup::Miss {
                tt_move: None,
                alpha,
                beta,
            }
        }
    }

    /// Choose which letters to evaluate. If splitting required letters exist,
    /// only those are returned (non-required letters are provably suboptimal).
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
        let raw: Vec<u8> = (0..26u8)
            .filter(|i| candidate_mask & (1u32 << i) != 0)
            .map(|i| b'a' + i)
            .collect();
        self.dedup_and_order_letters(&raw, indices)
    }

    /// Lower bound via the "miss chain" argument: when no letter is required,
    /// every guess risks a miss. The referee can always choose the miss
    /// partition, so `value >= min_L (1 + lb(miss_partition(L)))`.
    ///
    /// `max_depth` limits recursion for performance.
    fn miss_chain_lower_bound(
        &self,
        indices: &[usize],
        mut masked: LetterSet,
        max_depth: u32,
    ) -> u32 {
        if indices.len() <= 1 || max_depth == 0 {
            return 0;
        }

        // Collapse required letters (in all words) — free, no miss risk.
        for li in 0..26u8 {
            let letter = b'a' + li;
            if masked & letter_bit(letter) != 0 {
                continue;
            }
            let first_mask = self.pos_mask_for(indices[0], letter);
            if first_mask == 0 {
                continue;
            }
            if indices[1..]
                .iter()
                .all(|&idx| self.pos_mask_for(idx, letter) != 0)
            {
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
            let val =
                1 + self.miss_chain_lower_bound(&miss, masked | letter_bit(letter), max_depth - 1);
            best = best.min(val);
            if best <= 1 {
                break;
            }
        }
        if best == u32::MAX { 0 } else { best }
    }

    fn solve_subset(&self, indices: &[usize], masked: LetterSet, alpha: u32, beta: u32) -> u32 {
        if indices.len() <= 1 || beta == 0 {
            return 0;
        }

        // Analyze required letters: collapse non-splitting ones (free guesses)
        // and identify splitting ones (the only candidates we need to evaluate
        // when present, since non-required letters risk a miss).
        let (masked, required_splitting) = self.analyze_required_letters(indices, masked);

        // Quick pruning via miss-chain lower bound before expensive canonicalization.
        // When no letter is required, the referee can always force misses.
        if required_splitting == 0 && indices.len() >= 5000 {
            let depth = if indices.len() >= 40_000 { 3 } else { 2 };
            let lb = self.miss_chain_lower_bound(indices, masked, depth);
            if lb >= beta {
                return lb;
            }
        }

        // Combined dedup + canonical hash (avoids computing effective sigs twice).
        let (indices, cache_key) = dedup_and_hash(&self.data.words, indices, masked);
        if indices.len() <= 1 {
            return 0;
        }

        self.data.hash_calls.fetch_add(1, Ordering::Relaxed);

        // Tighten beta using worst-case bound: max misses = number of
        // non-required useful letters (letters in some but not all words).
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

        let (best, best_letter) = if self.use_rayon && indices.len() >= PAR_THRESHOLD {
            self.solve_parallel(&letters, &indices, masked, alpha, beta)
        } else {
            self.solve_sequential(&letters, &indices, masked, alpha, beta)
        };

        self.cache_store(cache_key, best, best_letter, alpha, beta);
        best
    }

    /// Store a result in the cache with correct bound type.
    fn cache_store(&self, key: u128, best: u32, best_letter: u8, alpha: u32, beta: u32) {
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
        let mut best = u32::MAX;
        let mut best_letter = letters[0];
        for &letter in letters {
            let cutoff = best.min(beta);
            let val = self.evaluate_letter(indices, masked, letter, cutoff);
            if val < best {
                best = val;
                best_letter = letter;
                // History heuristic: reward letters that improve the bound.
                self.data.history[(letter - b'a') as usize]
                    .fetch_add(indices.len() as u64, Ordering::Relaxed);
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
        // Evaluate first letter sequentially to get a tight initial bound.
        let first_val = self.evaluate_letter(indices, masked, letters[0], beta);
        let mut best = first_val;
        let mut best_letter = letters[0];

        if best <= alpha || best == 0 || letters.len() <= 1 {
            self.data.history[(best_letter - b'a') as usize]
                .fetch_add(indices.len() as u64, Ordering::Relaxed);
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

        if rest.0 < best {
            best = rest.0;
            best_letter = rest.1;
        }

        // Update history for the best letter found.
        if best_letter >= b'a' {
            self.data.history[(best_letter - b'a') as usize]
                .fetch_add(indices.len() as u64, Ordering::Relaxed);
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
        // Build (pos_mask, word_idx) pairs and sort by pos_mask.
        // Reuse a single sorted array — partitions are contiguous slices.
        let mut pairs: Vec<(u32, usize)> = Vec::with_capacity(indices.len());
        for &idx in indices {
            pairs.push((self.pos_mask_for(idx, letter), idx));
        }
        pairs.sort_unstable_by_key(|&(mask, _)| mask);

        // Identify partition boundaries.
        let mut boundaries: Vec<(u32, usize, usize)> = Vec::new(); // (mask, start, len)
        let mut i = 0;
        while i < pairs.len() {
            let mask = pairs[i].0;
            let start = i;
            while i < pairs.len() && pairs[i].0 == mask {
                i += 1;
            }
            boundaries.push((mask, start, i - start));
        }

        // Evaluate miss partition first (pos_mask == 0) since it has +1 miss cost,
        // making it most likely to exceed cutoff and trigger pruning. Then largest
        // partitions first among hit partitions.
        boundaries.sort_unstable_by(|a, b| {
            let a_is_miss = u32::from(a.0 != 0); // miss=0 sorts first
            let b_is_miss = u32::from(b.0 != 0);
            a_is_miss.cmp(&b_is_miss).then(b.2.cmp(&a.2))
        });

        let new_masked = masked | letter_bit(letter);
        let mut worst = 0u32;

        // Extract word indices from pairs into a single buffer, then pass
        // slices to solve_subset to avoid per-partition allocations.
        let word_indices: Vec<usize> = pairs.iter().map(|&(_, idx)| idx).collect();

        for &(pos_mask, start, len) in &boundaries {
            let miss_cost = u32::from(pos_mask == 0);

            // If miss_cost alone meets cutoff, prune without recursion.
            if miss_cost >= cutoff {
                return cutoff;
            }

            let subset = &word_indices[start..start + len];

            // ETC: for large partitions, probe the TT before full solve.
            // This avoids expensive search when the TT already has a bound.
            // child_alpha: referee already has `worst`; this partition must
            // give > worst - miss_cost to affect the max.
            let child_alpha = worst.saturating_sub(miss_cost);
            let child_beta = cutoff - miss_cost;
            let val = miss_cost + self.solve_subset(subset, new_masked, child_alpha, child_beta);

            worst = worst.max(val);
            if worst >= cutoff {
                return worst;
            }
        }

        worst
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
    fn cache_is_used() {
        let words: Vec<Vec<u8>> = ["cat", "bat", "hat", "mat"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let solver = MemoizedSolver::new();
        let _ = solver.solve(&words);
        assert!(solver.cache_size() > 0, "cache should have entries");
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
        let solver = MemoizedSolver::new();
        let r1 = solver.solve(&[b"ab".to_vec(), b"cd".to_vec()]);
        let r2 = solver.solve(&[b"ef".to_vec(), b"gh".to_vec()]);
        assert_eq!(r1, r2);
        assert_eq!(solver.cache_size(), 1);
    }

    #[test]
    fn position_isomorphic_share_cache() {
        // "ab","cd" and "ba","dc" differ only by column swap — same game.
        let solver = MemoizedSolver::new();
        let r1 = solver.solve(&[b"ab".to_vec(), b"cd".to_vec()]);
        let r2 = solver.solve(&[b"ba".to_vec(), b"dc".to_vec()]);
        assert_eq!(r1, r2);
        assert_eq!(solver.cache_size(), 1);
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
}
