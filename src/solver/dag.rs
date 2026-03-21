use std::collections::HashMap;

use dashmap::DashMap;

use super::canon::canonical_hash;
use crate::game::{LetterSet, letter_bit};

/// DAG-based minimax solver with structural canonicalization.
///
/// State transitions use the clean (pattern, missed) representation.
/// Caching uses the canonical structure of compatible words at each state,
/// so isomorphic game positions share cache entries.
///
/// ## Combining two ideas
///
/// 1. **DAG transitions**: the game state is (reveal pattern, missed letters).
///    Compatible words are derived from this, not tracked through recursion.
///
/// 2. **Structural caching**: the cache key is the canonical form of the
///    compatible words' unrevealed signatures. Two states with isomorphic
///    word structures (under letter/position permutation) share a cache entry.
///
/// This means `(___,missed={b,c})` and `(___,missed={d,e})` share a cache
/// entry if the remaining compatible words have the same structure.
pub struct DagSolver {
    words: Vec<Vec<u8>>,
    word_len: usize,
    /// Canonical structure hash -> minimax value.
    cache: DashMap<u128, u32>,
}

impl DagSolver {
    /// Create a solver for words of a given length.
    ///
    /// # Panics
    ///
    /// Panics if words is empty or word length exceeds 16.
    #[must_use]
    pub fn new(words: Vec<Vec<u8>>) -> Self {
        assert!(!words.is_empty());
        let word_len = words[0].len();
        assert!(word_len <= 16, "word length must be <= 16");
        Self {
            words,
            word_len,
            cache: DashMap::new(),
        }
    }

    /// Solve: returns the minimax-optimal number of misses.
    #[must_use]
    pub fn solve(&self) -> u32 {
        let pattern = vec![0u8; self.word_len];
        self.solve_state(&pattern, 0, u32::MAX)
    }

    fn solve_state(&self, pattern: &[u8], missed: LetterSet, _alpha: u32) -> u32 {
        let guessed = pattern_letters(pattern) | missed;
        let compat: Vec<usize> = self.compatible_word_indices(pattern, guessed);

        if compat.len() <= 1 {
            return 0;
        }

        // Build unrevealed signatures and canonicalize for cache key.
        let sigs = self.unrevealed_signatures(pattern, &compat);
        let cache_key = canonical_hash(&sigs);

        if let Some(val) = self.cache.get(&cache_key) {
            return *val;
        }

        // Valid guesses: unguessed letters present in compatible words.
        let present = compat.iter().fold(0u32, |acc, &idx| {
            self.words[idx].iter().fold(acc, |a, &b| a | letter_bit(b))
        });
        let useful = present & !guessed;

        if useful == 0 {
            self.cache.insert(cache_key, 0);
            return 0;
        }

        let mut letters: Vec<u8> = (0..26u8)
            .filter(|i| useful & (1u32 << i) != 0)
            .map(|i| b'a' + i)
            .collect();
        self.order_letters(&mut letters, &compat);

        let mut best = u32::MAX;

        for &letter in &letters {
            let val = self.evaluate_guess(pattern, missed, &compat, letter, best);
            if val < best {
                best = val;
            }
            if best == 0 {
                break;
            }
        }

        self.cache.insert(cache_key, best);
        best
    }

    fn evaluate_guess(
        &self,
        pattern: &[u8],
        missed: LetterSet,
        compat: &[usize],
        letter: u8,
        alpha: u32,
    ) -> u32 {
        let partitions = self.partition_compat(compat, letter);

        let mut parts: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();
        parts.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

        let mut worst = 0u32;

        for (pos_mask, _group) in &parts {
            let is_miss = *pos_mask == 0;
            let miss_cost = u32::from(is_miss);
            let sub_alpha = alpha.saturating_sub(miss_cost);

            let val = if is_miss {
                let new_missed = missed | letter_bit(letter);
                miss_cost + self.solve_state(pattern, new_missed, sub_alpha)
            } else {
                let mut new_pattern = pattern.to_vec();
                for (i, slot) in new_pattern.iter_mut().enumerate() {
                    if pos_mask & (1 << i) != 0 {
                        *slot = letter;
                    }
                }
                miss_cost + self.solve_state(&new_pattern, missed, sub_alpha)
            };

            worst = worst.max(val);
            if worst >= alpha {
                return worst;
            }
        }

        worst
    }

    /// Extract unrevealed signatures: for each compatible word, the letters
    /// at unrevealed positions only. Revealed positions are dropped since
    /// they're constant across all compatible words at this node.
    fn unrevealed_signatures(&self, pattern: &[u8], compat: &[usize]) -> Vec<Vec<u8>> {
        let mut sigs: Vec<Vec<u8>> = compat
            .iter()
            .map(|&idx| {
                self.words[idx]
                    .iter()
                    .zip(pattern.iter())
                    .filter(|(_, p)| **p == 0) // unrevealed positions only
                    .map(|(ch, _)| *ch)
                    .collect()
            })
            .collect();
        sigs.sort();
        sigs.dedup();
        sigs
    }

    fn compatible_word_indices(&self, pattern: &[u8], guessed: LetterSet) -> Vec<usize> {
        self.words
            .iter()
            .enumerate()
            .filter(|(_, word)| {
                word.iter().enumerate().all(|(i, &ch)| {
                    if pattern[i] != 0 {
                        ch == pattern[i]
                    } else {
                        guessed & letter_bit(ch) == 0
                    }
                })
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn partition_compat(&self, compat: &[usize], letter: u8) -> HashMap<u32, Vec<usize>> {
        let mut map: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in compat {
            let word = &self.words[idx];
            let mut mask = 0u32;
            for (i, &ch) in word.iter().enumerate() {
                if ch == letter {
                    mask |= 1 << i;
                }
            }
            map.entry(mask).or_default().push(idx);
        }
        map
    }

    fn order_letters(&self, letters: &mut [u8], compat: &[usize]) {
        letters.sort_by_cached_key(|&letter| {
            let mut counts: HashMap<u32, usize> = HashMap::new();
            let mut max_part = 0usize;
            for &idx in compat {
                let word = &self.words[idx];
                let mut mask = 0u32;
                for (i, &ch) in word.iter().enumerate() {
                    if ch == letter {
                        mask |= 1 << i;
                    }
                }
                let count = counts.entry(mask).or_insert(0);
                *count += 1;
                max_part = max_part.max(*count);
            }
            max_part
        });
    }

    #[must_use]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }
}

fn pattern_letters(pattern: &[u8]) -> LetterSet {
    pattern.iter().fold(
        0u32,
        |acc, &b| {
            if b == 0 { acc } else { acc | letter_bit(b) }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::naive::NaiveSolver;
    use super::*;

    fn solve_both(words: &[&str]) -> u32 {
        let owned: Vec<Vec<u8>> = words.iter().map(|s| s.as_bytes().to_vec()).collect();
        let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let naive_result = NaiveSolver::solve(&refs, 0);

        let solver = DagSolver::new(owned);
        let dag_result = solver.solve();

        assert_eq!(
            naive_result, dag_result,
            "naive={naive_result} != dag={dag_result} for {words:?}"
        );
        dag_result
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
        let solver = DagSolver::new(words);
        let _ = solver.solve();
        assert!(solver.cache_size() > 0);
    }
}
