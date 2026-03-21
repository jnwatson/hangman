/// A bitmask over the 26 lowercase ASCII letters.
/// Bit 0 = 'a', bit 1 = 'b', ..., bit 25 = 'z'.
pub type LetterSet = u32;

/// Convert a letter ('a'..='z') to its bitmask.
#[inline]
#[must_use]
pub fn letter_bit(c: u8) -> LetterSet {
    debug_assert!(c.is_ascii_lowercase());
    1 << (c - b'a')
}

/// A pattern representing the current revealed state of the word.
/// Each position is either `Some(letter)` if revealed, or `None` if still hidden.
/// Stored compactly: lowercase ASCII bytes, 0 for unrevealed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pattern {
    slots: Vec<u8>, // 0 = hidden, b'a'..=b'z' = revealed
}

impl Pattern {
    /// Create a fully-hidden pattern of the given length.
    #[must_use]
    pub fn blank(len: usize) -> Self {
        Self {
            slots: vec![0; len],
        }
    }

    /// Word length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the pattern has zero length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Access the raw slots.
    #[must_use]
    pub fn slots(&self) -> &[u8] {
        &self.slots
    }

    /// Return a new pattern with the given letter revealed at all specified positions.
    #[must_use]
    pub fn reveal(&self, letter: u8, positions: &[usize]) -> Self {
        let mut new = self.clone();
        for &pos in positions {
            debug_assert!(pos < new.slots.len());
            new.slots[pos] = letter;
        }
        new
    }

    /// Check whether a word matches this pattern (all revealed letters match,
    /// and hidden positions don't contain any revealed letter).
    #[must_use]
    pub fn matches_word(&self, word: &[u8]) -> bool {
        if word.len() != self.slots.len() {
            return false;
        }
        let mut revealed = 0u32;
        for &s in &self.slots {
            if s != 0 {
                revealed |= letter_bit(s);
            }
        }
        for (slot, &w) in self.slots.iter().zip(word.iter()) {
            if *slot != 0 {
                if *slot != w {
                    return false;
                }
            } else if revealed & letter_bit(w) != 0 {
                // Hidden position contains a letter that should be revealed everywhere
                return false;
            }
        }
        true
    }

    /// Display as a string like "_ a _ _ e".
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(self.slots.len() * 2);
        for (i, &s) in self.slots.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push(if s == 0 { '_' } else { s as char });
        }
        out
    }
}

/// The state of an ongoing game, from the guesser's perspective.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GameState {
    /// Current revealed pattern.
    pub pattern: Pattern,
    /// Letters guessed so far (bitmask).
    pub guessed: LetterSet,
    /// Number of misses so far.
    pub misses: u32,
}

impl GameState {
    /// Start a new game with the given word length.
    #[must_use]
    pub fn new(word_len: usize) -> Self {
        Self {
            pattern: Pattern::blank(word_len),
            guessed: 0,
            misses: 0,
        }
    }

    /// Letters not yet guessed.
    #[must_use]
    pub fn remaining_letters(&self) -> LetterSet {
        let all_letters: LetterSet = (1 << 26) - 1;
        all_letters & !self.guessed
    }

    /// Whether the word is fully revealed (no hidden slots).
    #[must_use]
    pub fn is_solved(&self) -> bool {
        self.pattern.slots().iter().all(|&s| s != 0)
    }
}

/// Final outcome of a game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Total misses in the game.
    pub misses: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_pattern() {
        let p = Pattern::blank(5);
        assert_eq!(p.len(), 5);
        assert_eq!(p.display(), "_ _ _ _ _");
    }

    #[test]
    fn reveal_pattern() {
        let p = Pattern::blank(4);
        let p2 = p.reveal(b'a', &[0, 3]);
        assert_eq!(p2.display(), "a _ _ a");
    }

    #[test]
    fn pattern_matches_word() {
        let p = Pattern::blank(4).reveal(b'a', &[0, 3]);
        assert!(p.matches_word(b"abca"));
        assert!(!p.matches_word(b"abcd")); // 'd' at position 3, not 'a'
        assert!(!p.matches_word(b"aaba")); // 'a' at position 1 should be revealed
    }

    #[test]
    fn pattern_rejects_hidden_revealed_letter() {
        // Pattern "a _ _ _" — word "abac" invalid because 'a' at pos 2 is hidden
        let p = Pattern::blank(4).reveal(b'a', &[0]);
        assert!(!p.matches_word(b"abac"));
        assert!(p.matches_word(b"abcd"));
    }

    #[test]
    fn game_state_basics() {
        let gs = GameState::new(5);
        assert_eq!(gs.misses, 0);
        assert!(!gs.is_solved());
        assert_eq!(gs.remaining_letters().count_ones(), 26);
    }

    #[test]
    fn letter_bit_values() {
        assert_eq!(letter_bit(b'a'), 1);
        assert_eq!(letter_bit(b'z'), 1 << 25);
    }
}
