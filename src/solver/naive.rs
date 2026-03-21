use std::collections::HashMap;

use crate::game::{LetterSet, letter_bit};

/// Naive minimax solver for Schrödinger's Hangman.
///
/// No caching, no pruning, no state-space reduction.
/// This exists purely as a correctness oracle.
///
/// The game state is represented as:
/// - `words`: the set of words still compatible with all reveals and misses
/// - `guessed`: bitmask of letters already guessed
///
/// The guesser picks a letter to minimize worst-case misses.
/// The referee picks the response partition to maximize misses.
pub struct NaiveSolver;

/// A referee response to a guessed letter.
/// The key is the positions where the letter appears (empty = miss).
/// We encode this as a bitmask over word positions for efficiency,
/// but the naive solver treats it opaquely.
type PositionMask = u32;

impl NaiveSolver {
    /// Solve: returns the minimax-optimal number of misses for the given word set.
    ///
    /// `words` must all be the same length and non-empty.
    /// `guessed` is the set of letters already guessed.
    ///
    /// # Panics
    ///
    /// Panics if `words` is empty.
    #[must_use]
    pub fn solve(words: &[&[u8]], guessed: LetterSet) -> u32 {
        assert!(!words.is_empty(), "word set must not be empty");

        // Base case: if all words are fully determined (only one word left,
        // or all positions are revealed), no more misses needed.
        if words.len() == 1 {
            return 0;
        }

        // If every unguessed letter produces the same partition for all words,
        // then the game is effectively over — but this is subsumed by the
        // recursion naturally terminating.

        let word_len = words[0].len();
        let remaining = all_letters_mask() & !guessed;

        // Only consider letters that appear in at least one word.
        // Letters absent from ALL words are guaranteed misses with no information gain,
        // so a guesser would never optimally pick them.
        // NOTE: This is our first "optimization" but it's trivially safe —
        // guessing a letter not in any word is strictly dominated by any letter
        // that IS in some word (same +1 miss, but no partition refinement).
        let present_in_words = words
            .iter()
            .fold(0u32, |acc, w| w.iter().fold(acc, |a, &b| a | letter_bit(b)));
        let useful = remaining & present_in_words;

        if useful == 0 {
            // No unguessed letters appear in any word. The guesser cannot
            // distinguish between words. But the game still needs to guess the
            // remaining hidden letters. Every remaining guess is a miss except
            // we can't actually reveal anything. In Schrödinger's hangman, the
            // game ends when the word is fully revealed, but if multiple words
            // remain and no useful letters exist, that means all words share
            // the same revealed pattern but differ in... wait, that can't happen
            // because we filtered `present_in_words`. If useful == 0, it means
            // all letters in the remaining words have been guessed already,
            // which means the pattern fully determines the word. But then
            // words.len() should be 1 (handled above) or all words are identical
            // in their letter positions. Let's verify:
            // Actually if words share the same multiset of letters in the same
            // positions and all those letters are guessed, they must be identical.
            // So this branch shouldn't be reachable with len > 1.
            // We handle it defensively.
            return 0;
        }

        let mut best_for_guesser = u32::MAX;

        for letter_idx in 0..26u8 {
            let letter = b'a' + letter_idx;
            let letter_mask = 1u32 << letter_idx;
            if useful & letter_mask == 0 {
                continue;
            }

            let new_guessed = guessed | letter_mask;

            // Partition words by the referee's possible responses.
            let partitions = partition_by_letter(words, letter, word_len);

            // Referee picks the partition that maximizes misses.
            let worst_for_guesser = partitions
                .iter()
                .map(|(pos_mask, group)| {
                    let is_miss = *pos_mask == 0;
                    let miss_cost = u32::from(is_miss);
                    let group_refs = group.clone();
                    miss_cost + Self::solve(&group_refs, new_guessed)
                })
                .max()
                .unwrap_or(0);

            best_for_guesser = best_for_guesser.min(worst_for_guesser);
        }

        best_for_guesser
    }
}

/// Partition words by the positions where `letter` appears.
/// Returns a map from position-bitmask to the words that produce that pattern.
/// Position bitmask 0 means the letter doesn't appear (miss).
fn partition_by_letter<'a>(
    words: &[&'a [u8]],
    letter: u8,
    word_len: usize,
) -> HashMap<PositionMask, Vec<&'a [u8]>> {
    let mut map: HashMap<PositionMask, Vec<&'a [u8]>> = HashMap::new();
    for &word in words {
        let mut mask: PositionMask = 0;
        for (i, &ch) in word.iter().enumerate() {
            if ch == letter {
                assert!(i < 32, "word length must be < 32 for position bitmask");
                mask |= 1 << i;
            }
        }
        // Additional constraint: if the letter was already guessed and
        // is in the word, those positions are already revealed. We don't
        // need to check here because `guessed` filtering ensures we only
        // guess unguessed letters.
        let _ = word_len; // used only for documentation/assertion context
        map.entry(mask).or_default().push(word);
    }
    map
}

fn all_letters_mask() -> LetterSet {
    (1 << 26) - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: convert string slices to byte slices for the solver.
    fn w(words: &[&str]) -> Vec<Vec<u8>> {
        words.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    fn solve(words: &[&str]) -> u32 {
        let owned = w(words);
        let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        NaiveSolver::solve(&refs, 0)
    }

    #[test]
    fn single_word_zero_misses() {
        // Only one word — referee is committed, guesser can't miss.
        assert_eq!(solve(&["cat"]), 0);
    }

    #[test]
    fn two_identical_words() {
        // Deduplicated in practice, but solver should handle it.
        assert_eq!(solve(&["cat", "cat"]), 0);
    }

    #[test]
    fn two_words_one_letter_diff() {
        // "cat" vs "bat" — guesser guesses 'c' or 'b' first.
        // If guesser guesses 'c': referee says hit (-> "cat") or miss (-> "bat").
        //   Hit path: 1 word, done. 0 misses on this path.
        //   Miss path: 1 word, done. 1 miss on this path.
        //   Referee maximizes: picks miss path -> 1 miss.
        // If guesser guesses 'b': symmetric -> 1 miss.
        // If guesser guesses 'a': both words have 'a' at pos 1. Same partition.
        //   Then still need to distinguish, same as before -> 1 miss total.
        // If guesser guesses 't': both have 't' at pos 2. Same partition. -> 1 miss.
        // Best the guesser can do: 1 miss.
        assert_eq!(solve(&["cat", "bat"]), 1);
    }

    #[test]
    fn three_words_simple() {
        // "cat", "bat", "hat"
        // All share 'a' at pos 1 and 't' at pos 2. Differ at pos 0.
        // Guesser must identify pos 0: c, b, or h.
        // Any single guess splits into hit (1 word) vs miss (2 words).
        // Referee picks the 2-word group. Then we're back to a 2-word problem -> 1 more miss.
        // Total: 2 misses.
        assert_eq!(solve(&["cat", "bat", "hat"]), 2);
    }

    #[test]
    fn completely_different_words() {
        // "ab" vs "cd" — no letters in common.
        // Guesser guesses 'a': hit -> reveals a_, must be "ab". Miss -> must be "cd".
        // Either way, 1 word left after first guess. Worst case (miss) = 1 miss.
        assert_eq!(solve(&["ab", "cd"]), 1);
    }

    #[test]
    fn longer_example() {
        // "abc", "abd", "xyz"
        // Guesser might guess 'a': hit -> {"abc","abd"}, miss -> {"xyz"}
        //   Referee picks hit (harder subproblem): solve({"abc","abd"}, guessed={a})
        //     Sub: guess 'b' -> both have b at pos 1, same partition. Then guess 'c': hit->abc, miss->abd. 1 miss.
        //     Sub: guess 'c' -> hit=abc, miss=abd. 1 miss. Referee picks miss. Total sub = 1.
        //   So guess 'a' total = max(0+1, 1+0) = 1 miss.
        // Guesser could also try other letters. 'x': hit->xyz, miss->{abc,abd}
        //   Referee picks miss group: solve({abc,abd}, guessed={x}) -> same as 2 words differing in last letter = 1 miss.
        //   Total = max(0+..., 1+1) = 2. Worse.
        // Best: 1 miss.
        assert_eq!(solve(&["abc", "abd", "xyz"]), 1);
    }
}
