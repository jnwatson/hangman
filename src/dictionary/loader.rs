use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// A loaded dictionary, with words grouped by length.
#[derive(Clone, Debug)]
pub struct Dictionary {
    /// Words grouped by length. Each word stored as lowercase ASCII bytes.
    by_length: HashMap<usize, Vec<Vec<u8>>>,
}

impl Dictionary {
    /// Load a dictionary from a file (one word per line).
    /// Filters to lowercase ASCII-only words.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("reading dictionary: {}", path.display()))?;
        Ok(Self::from_words(content.lines()))
    }

    /// Build a dictionary from an iterator of word strings.
    pub fn from_words<'a>(words: impl Iterator<Item = &'a str>) -> Self {
        let mut by_length: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for word in words {
            let lower = word.trim().to_ascii_lowercase();
            if lower.is_empty() || !lower.bytes().all(|b| b.is_ascii_lowercase()) {
                continue;
            }
            by_length
                .entry(lower.len())
                .or_default()
                .push(lower.into_bytes());
        }
        // Deduplicate each group
        for words in by_length.values_mut() {
            words.sort();
            words.dedup();
        }
        Self { by_length }
    }

    /// Get all words of a given length.
    #[must_use]
    pub fn words_of_length(&self, len: usize) -> &[Vec<u8>] {
        self.by_length.get(&len).map_or(&[], Vec::as_slice)
    }

    /// Available word lengths (sorted).
    #[must_use]
    pub fn available_lengths(&self) -> Vec<usize> {
        let mut lengths: Vec<usize> = self.by_length.keys().copied().collect();
        lengths.sort_unstable();
        lengths
    }

    /// Total number of words.
    #[must_use]
    pub fn total_words(&self) -> usize {
        self.by_length.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_words_filters_and_groups() {
        let dict = Dictionary::from_words(
            ["hello", "world", "hi", "RUST", "123", "a-b", ""]
                .iter()
                .copied(),
        );
        assert_eq!(dict.words_of_length(5).len(), 2); // hello, world
        assert_eq!(dict.words_of_length(2).len(), 1); // hi
        assert_eq!(dict.words_of_length(4).len(), 1); // rust (lowered)
        assert_eq!(dict.words_of_length(3).len(), 0); // "a-b" filtered out
    }

    #[test]
    fn deduplicates() {
        let dict = Dictionary::from_words(["cat", "cat", "CAT"].iter().copied());
        assert_eq!(dict.words_of_length(3).len(), 1);
    }

    #[test]
    fn available_lengths_sorted() {
        let dict = Dictionary::from_words(["zoo", "hi", "hello"].iter().copied());
        assert_eq!(dict.available_lengths(), vec![2, 3, 5]);
    }
}
