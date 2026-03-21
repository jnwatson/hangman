#![allow(dead_code)]

/// Compact bitset representation of a subset of words.
/// Used as part of the transposition table key.
///
/// Words are assigned indices 0..N within a word-length group.
/// The bitset tracks which indices are still in the candidate set.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WordSet {
    bits: Vec<u64>,
    len: usize,
}

impl WordSet {
    /// Create a full set containing indices 0..n.
    #[must_use]
    pub fn full(n: usize) -> Self {
        let num_blocks = n.div_ceil(64);
        let mut bits = vec![u64::MAX; num_blocks];
        // Clear unused bits in the last block.
        let remainder = n % 64;
        if remainder != 0 && num_blocks > 0 {
            bits[num_blocks - 1] = (1u64 << remainder) - 1;
        }
        // Handle the edge case where n is an exact multiple of 64:
        // all bits in the last block are valid, no clearing needed.
        Self { bits, len: n }
    }

    /// Create an empty set with capacity for n words.
    #[must_use]
    pub fn empty(n: usize) -> Self {
        let num_blocks = n.div_ceil(64);
        Self {
            bits: vec![0; num_blocks],
            len: 0,
        }
    }

    /// Number of words in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Check if index i is in the set.
    #[must_use]
    pub fn contains(&self, i: usize) -> bool {
        let block = i / 64;
        let bit = i % 64;
        block < self.bits.len() && (self.bits[block] & (1u64 << bit)) != 0
    }

    /// Add index i to the set.
    pub fn insert(&mut self, i: usize) {
        let block = i / 64;
        let bit = i % 64;
        if !self.contains(i) {
            self.bits[block] |= 1u64 << bit;
            self.len += 1;
        }
    }

    /// Iterate over the indices in the set.
    pub fn iter(&self) -> WordSetIter<'_> {
        WordSetIter {
            bits: &self.bits,
            block: 0,
            remaining: self.bits.first().copied().unwrap_or(0),
        }
    }

    /// Raw bits for hashing.
    #[must_use]
    pub fn as_bits(&self) -> &[u64] {
        &self.bits
    }
}

pub struct WordSetIter<'a> {
    bits: &'a [u64],
    block: usize,
    remaining: u64,
}

impl Iterator for WordSetIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.remaining == 0 {
            self.block += 1;
            if self.block >= self.bits.len() {
                return None;
            }
            self.remaining = self.bits[self.block];
        }
        let bit = self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1; // clear lowest set bit
        Some(self.block * 64 + bit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_set() {
        let ws = WordSet::full(5);
        assert_eq!(ws.len(), 5);
        assert!(ws.contains(0));
        assert!(ws.contains(4));
        assert!(!ws.contains(5));
        assert_eq!(ws.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn full_set_exact_block() {
        let ws = WordSet::full(64);
        assert_eq!(ws.len(), 64);
        assert!(ws.contains(63));
        assert!(!ws.contains(64));
    }

    #[test]
    fn full_set_multi_block() {
        let ws = WordSet::full(100);
        assert_eq!(ws.len(), 100);
        assert!(ws.contains(99));
        assert!(!ws.contains(100));
        assert_eq!(ws.iter().count(), 100);
    }

    #[test]
    fn empty_set_and_insert() {
        let mut ws = WordSet::empty(10);
        assert_eq!(ws.len(), 0);
        assert!(!ws.contains(3));
        ws.insert(3);
        ws.insert(7);
        assert_eq!(ws.len(), 2);
        assert!(ws.contains(3));
        assert!(ws.contains(7));
        assert_eq!(ws.iter().collect::<Vec<_>>(), vec![3, 7]);
    }

    #[test]
    fn double_insert() {
        let mut ws = WordSet::empty(10);
        ws.insert(3);
        ws.insert(3);
        assert_eq!(ws.len(), 1);
    }
}
