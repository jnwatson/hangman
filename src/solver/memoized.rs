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
/// 5. **Parallelism**: rayon at top levels with shared atomic alpha.
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
    hash_calls: std::sync::atomic::AtomicU64,
    cache_hits: std::sync::atomic::AtomicU64,
}

const PAR_THRESHOLD: usize = 100;

// Cache entry encoding: bit 31 = lower-bound flag, bits 0..30 = value.
// Values are always ≤ 25 (at most 26 letters), so this is safe.
const LOWER_BOUND_BIT: u32 = 1 << 31;

#[inline]
fn cache_exact(v: u32) -> u32 {
    v
}

#[inline]
fn cache_lower_bound(v: u32) -> u32 {
    v | LOWER_BOUND_BIT
}

#[inline]
fn cache_unpack(packed: u32) -> (u32, bool) {
    (packed & !LOWER_BOUND_BIT, packed & LOWER_BOUND_BIT != 0)
}

impl MemoizedSolver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
            hash_calls: std::sync::atomic::AtomicU64::new(0),
            cache_hits: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Solve for the given word list. All words must be the same length.
    ///
    /// # Panics
    ///
    /// Panics if words is empty.
    #[must_use]
    pub fn solve(&self, words: &[Vec<u8>]) -> u32 {
        assert!(!words.is_empty());
        let fresh = MemoizedSolverInner::new(words.to_vec());
        let indices: Vec<usize> = (0..words.len()).collect();
        let result = fresh.solve_subset(&indices, 0, u32::MAX);

        // Transfer instrumentation to self.
        self.hash_calls.fetch_add(
            fresh.hash_calls.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.cache_hits.fetch_add(
            fresh.cache_hits.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        // Only transfer exact values to the outer cache.
        for entry in &fresh.cache {
            let (_, is_lb) = cache_unpack(*entry.value());
            if !is_lb {
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
        self.hash_calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[must_use]
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for MemoizedSolver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Inner solver — owns the word list and the cache for one solve call.
// ---------------------------------------------------------------------------

struct MemoizedSolverInner {
    words: Vec<Vec<u8>>,
    cache: DashMap<u128, u32>,
    hash_calls: std::sync::atomic::AtomicU64,
    cache_hits: std::sync::atomic::AtomicU64,
}

impl MemoizedSolverInner {
    fn new(words: Vec<Vec<u8>>) -> Self {
        Self {
            words,
            cache: DashMap::new(),
            hash_calls: std::sync::atomic::AtomicU64::new(0),
            cache_hits: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Compute the position mask of `letter` in word `idx` under `masked`.
    ///
    /// Bit `j` is set if position `j` is not already masked AND contains
    /// `letter`. (Already-masked positions read as 0, so they can't match a
    /// real letter.)
    #[inline]
    fn pos_mask_for(&self, idx: usize, letter: u8, masked: LetterSet) -> u32 {
        let word = &self.words[idx];
        let mut mask = 0u32;
        for (j, &b) in word.iter().enumerate() {
            if b == letter && masked & letter_bit(b) == 0 {
                mask |= 1 << j;
            }
        }
        mask
    }

    /// Collect all non-zero, non-masked letters present in the word subset.
    fn present_letters(&self, indices: &[usize], masked: LetterSet) -> LetterSet {
        let mut present = 0u32;
        for &idx in indices {
            for &b in &self.words[idx] {
                if b != 0 && masked & letter_bit(b) == 0 {
                    present |= letter_bit(b);
                }
            }
        }
        present
    }

    /// Deduplicate letters by partition structure and sort by max partition size.
    ///
    /// Combines dedup and ordering in a single pass: compute partition fingerprint
    /// for each letter, dedup by fingerprint, then sort by max partition size.
    fn dedup_and_order_letters(
        &self,
        letters: &[u8],
        indices: &[usize],
        masked: LetterSet,
    ) -> Vec<u8> {
        let mut seen: FxHashSet<Vec<(usize, bool)>> = FxHashSet::default();
        let mut result: Vec<(u8, usize, usize)> = Vec::new(); // (letter, max_partition, num_parts)
        let mut counts: FxHashMap<u32, usize> = FxHashMap::default();
        let mut fingerprint: Vec<(usize, bool)> = Vec::new();

        for &letter in letters {
            counts.clear();
            let mut max_part = 0usize;
            for &idx in indices {
                let mask = self.pos_mask_for(idx, letter, masked);
                let count = counts.entry(mask).or_insert(0);
                *count += 1;
                max_part = max_part.max(*count);
            }
            let num_parts = counts.len();
            fingerprint.clear();
            fingerprint.extend(counts.iter().map(|(&mask, &count)| (count, mask == 0)));
            fingerprint.sort_unstable();

            if seen.insert(fingerprint.clone()) {
                // Order by: smallest max partition first, then most partitions first
                // (as tiebreaker). Encoding: (max_part, -num_parts) as sort key.
                result.push((letter, max_part, num_parts));
            }
        }

        result.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)));
        result.into_iter().map(|(letter, _, _)| letter).collect()
    }

    /// Fast path for exactly 2 distinct words.
    ///
    /// The answer is 0 if there exists an unmasked letter whose position
    /// masks differ between the two words AND both are non-zero (both hit,
    /// distinct partitions). Otherwise the answer is 1.
    fn solve_two_words(&self, idx_a: usize, idx_b: usize, masked: LetterSet) -> u32 {
        let word_a = &self.words[idx_a];
        let word_b = &self.words[idx_b];

        // Collect position masks for each unmasked letter in both words.
        for letter_idx in 0..26u8 {
            let letter = b'a' + letter_idx;
            if masked & letter_bit(letter) != 0 {
                continue;
            }
            let mask_a = self.pos_mask_for(idx_a, letter, masked);
            let mask_b = self.pos_mask_for(idx_b, letter, masked);
            // Both non-zero and different → 2 distinct hit partitions, 0 misses.
            if mask_a != 0 && mask_b != 0 && mask_a != mask_b {
                return 0;
            }
        }
        // No letter produces 2 distinct hit partitions.
        // Any distinguishing letter creates a hit+miss split → 1 miss.
        // (The words are distinct, so such a letter must exist.)
        let _ = (word_a, word_b);
        1
    }

    // ------------------------------------------------------------------

    /// Solve a subproblem with alpha-beta pruning.
    ///
    /// `beta` is an upper bound from the caller: if our value is >= beta, the
    /// caller will prune, so we can return early. Only exact values (those
    /// proven < beta) are cached.
    fn solve_subset(&self, indices: &[usize], masked: LetterSet, beta: u32) -> u32 {
        if indices.len() <= 1 || beta == 0 {
            return 0;
        }

        // Combined dedup + canonical hash (avoids computing effective sigs twice).
        let (indices, cache_key) = dedup_and_hash(&self.words, indices, masked);
        if indices.len() <= 1 {
            return 0;
        }

        self.hash_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if let Some(packed) = self.cache.get(&cache_key) {
            let (val, is_lower_bound) = cache_unpack(*packed);
            if !is_lower_bound {
                // Exact value — always usable.
                self.cache_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return val;
            }
            if val >= beta {
                // Lower bound that already meets/exceeds beta — prune.
                self.cache_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return val;
            }
            // Lower bound but val < beta — need to recompute with wider window.
        }

        // Fast path for exactly 2 words.
        if indices.len() == 2 {
            let val = self.solve_two_words(indices[0], indices[1], masked);
            self.cache.insert(cache_key, cache_exact(val));
            return val;
        }

        let present = self.present_letters(&indices, masked);
        if present == 0 {
            self.cache.insert(cache_key, 0);
            return 0;
        }

        let raw_letters: Vec<u8> = (0..26u8)
            .filter(|i| present & (1u32 << i) != 0)
            .map(|i| b'a' + i)
            .collect();
        let letters = self.dedup_and_order_letters(&raw_letters, &indices, masked);
        if letters.is_empty() {
            self.cache.insert(cache_key, 0);
            return 0;
        }

        let best = if indices.len() >= PAR_THRESHOLD {
            self.solve_parallel(&letters, &indices, masked, beta)
        } else {
            self.solve_sequential(&letters, &indices, masked, beta)
        };

        if best < beta {
            // Exact value — all letters evaluated without beta truncation.
            self.cache.insert(cache_key, cache_exact(best));
        } else {
            // Lower bound — store it so future queries with tight beta can prune.
            // Never overwrite an exact value with a lower bound.
            let packed = cache_lower_bound(best);
            if let Some(mut existing) = self.cache.get_mut(&cache_key) {
                let (old_val, old_is_lb) = cache_unpack(*existing);
                if old_is_lb && best > old_val {
                    *existing = packed;
                }
                // If exact (!old_is_lb), don't overwrite.
            } else {
                self.cache.insert(cache_key, packed);
            }
        }
        best
    }

    fn solve_sequential(
        &self,
        letters: &[u8],
        indices: &[usize],
        masked: LetterSet,
        beta: u32,
    ) -> u32 {
        let mut best = u32::MAX;
        for &letter in letters {
            let alpha = best.min(beta);
            let val = self.evaluate_letter(indices, masked, letter, alpha);
            if val < best {
                best = val;
            }
            if best == 0 {
                break;
            }
        }
        best
    }

    fn solve_parallel(
        &self,
        letters: &[u8],
        indices: &[usize],
        masked: LetterSet,
        beta: u32,
    ) -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        let shared_alpha = AtomicU32::new(beta);
        letters
            .par_iter()
            .map(|&letter| {
                let current_alpha = shared_alpha.load(Ordering::Relaxed);
                let val = self.evaluate_letter(indices, masked, letter, current_alpha);
                shared_alpha.fetch_min(val, Ordering::Relaxed);
                val
            })
            .min()
            .unwrap_or(0)
    }

    fn evaluate_letter(&self, indices: &[usize], masked: LetterSet, letter: u8, alpha: u32) -> u32 {
        // Build (pos_mask, word_idx) pairs and sort by pos_mask.
        // This avoids HashMap allocation for partitioning.
        let mut pairs: Vec<(u32, usize)> = indices
            .iter()
            .map(|&idx| (self.pos_mask_for(idx, letter, masked), idx))
            .collect();
        pairs.sort_unstable_by_key(|&(mask, _)| mask);

        // Identify partition boundaries and sizes, then sort by size (descending).
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
        // Evaluate largest partitions first for better alpha-beta cutoff.
        boundaries.sort_unstable_by(|a, b| b.2.cmp(&a.2));

        let new_masked = masked | letter_bit(letter);
        let mut worst = 0u32;

        for &(pos_mask, start, len) in &boundaries {
            let is_miss = pos_mask == 0;
            let miss_cost = u32::from(is_miss);

            // If miss_cost alone meets alpha, prune without recursion.
            if miss_cost >= alpha {
                return alpha;
            }

            let subset: Vec<usize> = pairs[start..start + len]
                .iter()
                .map(|&(_, idx)| idx)
                .collect();
            // Pass beta to child: caller prunes if miss_cost + child >= alpha,
            // so child only needs exact value when child < alpha - miss_cost.
            let child_beta = alpha - miss_cost;
            let val = miss_cost + self.solve_subset(&subset, new_masked, child_beta);

            worst = worst.max(val);
            if worst >= alpha {
                return worst;
            }
        }

        worst
    }
}

// ---------------------------------------------------------------------------
// Send + Sync for rayon
// ---------------------------------------------------------------------------

// MemoizedSolverInner is Send+Sync: DashMap is Send+Sync,
// words is Vec<Vec<u8>> (Send+Sync), atomics are Send+Sync.
// Rust derives Send/Sync automatically for all these fields.

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
