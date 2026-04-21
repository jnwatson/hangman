//! Public API for the game server.
//!
//! Exposes canonical hashing and TT entry decoding so the server can look up
//! precomputed values without going through the full solver.

use crate::game::{LetterSet, letter_bit};

use super::canon::dedup_and_hash;

/// Fold non-splitting required letters into `masked`, matching the solver's
/// `analyze_required_letters` transform. This must be called before
/// `canonical_hash_for_words` to produce a key that matches the disk cache.
///
/// A "non-splitting required letter" is one that appears in every remaining
/// word at the same positions. The solver treats these as free guesses and
/// includes them in `masked` before hashing. The server must do the same.
#[must_use]
pub fn fold_required_letters(
    words: &[Vec<u8>],
    indices: &[usize],
    mut masked: LetterSet,
) -> LetterSet {
    if indices.is_empty() {
        return masked;
    }
    // Compute intersection of letter sets across all words.
    let mut intersection = u32::MAX;
    for &idx in indices {
        let mut letters = 0u32;
        for &b in &words[idx] {
            letters |= letter_bit(b);
        }
        intersection &= letters;
        if intersection & !masked == 0 {
            return masked;
        }
    }
    let candidates = intersection & !masked;

    let mut bits = candidates;
    while bits != 0 {
        #[allow(clippy::cast_possible_truncation)]
        let li = bits.trailing_zeros() as u8;
        bits &= bits - 1;
        let letter = b'a' + li;
        let first_mask = pos_mask(&words[indices[0]], letter);
        let all_same = indices[1..]
            .iter()
            .all(|&idx| pos_mask(&words[idx], letter) == first_mask);
        if all_same {
            masked |= letter_bit(letter);
        }
    }
    masked
}

/// Compute the canonical hash for a set of words with the given masked letters.
///
/// **Important:** `masked` must first be passed through `fold_required_letters`
/// to match the solver's key computation.
///
/// This is the same hash used as the transposition table key during solving.
/// The server uses it to look up precomputed minimax values in the disk cache.
#[must_use]
pub fn canonical_hash_for_words(words: &[Vec<u8>], indices: &[usize], masked: LetterSet) -> u128 {
    let (_, hash) = dedup_and_hash(words, indices, masked);
    hash
}

/// Decoded transposition table entry.
#[derive(Debug, Clone, Copy)]
pub struct TtEntry {
    /// Minimax value (number of misses).
    pub value: u32,
    /// Best letter for the guesser (if known).
    pub best_letter: Option<u8>,
}

/// Decode a packed TT entry from the disk cache.
///
/// Returns `None` if the entry is not an EXACT bound (only EXACT values are
/// reliable for serving).
#[must_use]
pub fn decode_tt_entry(packed: u32) -> Option<TtEntry> {
    let (value, best_letter, bound) = super::memoized::cache_unpack(packed);
    if bound != 0 {
        // Not EXACT — can't use for serving
        return None;
    }
    Some(TtEntry {
        value,
        best_letter,
    })
}

/// Decode a packed TT entry into (value, `best_letter`, bound) without filtering.
#[must_use]
pub fn decode_tt_entry_raw(packed: u32) -> (u32, Option<u8>, u32) {
    super::memoized::cache_unpack(packed)
}

/// Compute the position mask for a letter in a word.
///
/// Returns a bitmask where bit `i` is set if `word[i] == letter`.
/// A return value of 0 means the letter is not in the word (a miss).
#[must_use]
pub fn pos_mask(word: &[u8], letter: u8) -> u32 {
    let mut mask = 0u32;
    for (j, &b) in word.iter().enumerate() {
        if b == letter {
            mask |= 1 << j;
        }
    }
    mask
}
