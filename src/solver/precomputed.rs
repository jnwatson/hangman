use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::game::letter_bit;

const DAG_VERSION: u32 = 1;
const DAG_DIR: &str = "dag_cache";

/// Pre-computed DAG of reachable game patterns for a fixed word list and length.
///
/// The DAG captures the structure of all possible reveal patterns reachable
/// from the initial blank state via hit transitions. Each node stores the
/// indices of words that match the pattern at revealed positions.
///
/// This is independent of the missed-letter set: at runtime, the solver
/// filters a node's matching words by the current missed letters.
///
/// ## Persistence
///
/// DAGs are stored in the `dag_cache/` directory. Filename encodes version,
/// word length, and a hash of the word set, so multiple DAGs coexist safely
/// and stale caches are never loaded.
#[derive(Serialize, Deserialize)]
pub struct PrecomputedDag {
    version: u32,
    word_length: usize,
    word_set_hash: u64,
    word_count: usize,
    words: Vec<Vec<u8>>,
    /// Pattern → indices of words matching at all revealed positions.
    /// Pattern encoding: 0 = unrevealed, `b'a'`..=`b'z'` = revealed letter.
    pattern_matches: HashMap<Vec<u8>, Vec<usize>>,
}

impl PrecomputedDag {
    /// Build a DAG by BFS from the root (all-blank) pattern.
    ///
    /// Discovers all reachable patterns via hit transitions and records
    /// which words match each pattern.
    ///
    /// # Panics
    ///
    /// Panics if `words` is empty or contains words of different lengths.
    #[must_use]
    pub fn build(words: Vec<Vec<u8>>) -> Self {
        assert!(!words.is_empty());
        let word_length = words[0].len();
        assert!(
            words.iter().all(|w| w.len() == word_length),
            "all words must have the same length"
        );

        let word_set_hash = hash_word_set(&words);
        let word_count = words.len();

        let mut pattern_matches: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        let root = vec![0u8; word_length];
        let all_indices: Vec<usize> = (0..words.len()).collect();
        pattern_matches.insert(root.clone(), all_indices);

        let mut queue = vec![root];

        while let Some(pattern) = queue.pop() {
            let matching = pattern_matches[&pattern].clone();

            // Collect letters appearing at unrevealed positions in matching words.
            let revealed = pattern_letters(&pattern);
            let mut present = 0u32;
            for &idx in &matching {
                for (i, &ch) in words[idx].iter().enumerate() {
                    if pattern[i] == 0 {
                        present |= letter_bit(ch);
                    }
                }
            }
            let useful = present & !revealed;

            for letter_idx in 0..26u8 {
                if useful & (1u32 << letter_idx) == 0 {
                    continue;
                }
                let letter = b'a' + letter_idx;

                // Partition by position mask.
                let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
                for &idx in &matching {
                    let mut mask = 0u32;
                    for (i, &ch) in words[idx].iter().enumerate() {
                        if ch == letter {
                            mask |= 1 << i;
                        }
                    }
                    partitions.entry(mask).or_default().push(idx);
                }

                // Hit partitions create new patterns.
                for (&mask, indices) in &partitions {
                    if mask == 0 {
                        continue;
                    }

                    let mut new_pattern = pattern.clone();
                    for (i, slot) in new_pattern.iter_mut().enumerate() {
                        if mask & (1 << i) != 0 {
                            *slot = letter;
                        }
                    }

                    #[allow(clippy::map_entry)]
                    if !pattern_matches.contains_key(&new_pattern) {
                        pattern_matches.insert(new_pattern.clone(), indices.clone());
                        queue.push(new_pattern);
                    }
                }
            }
        }

        Self {
            version: DAG_VERSION,
            word_length,
            word_set_hash,
            word_count,
            words,
            pattern_matches,
        }
    }

    /// Save to the `dag_cache/` directory under `base_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file
    /// cannot be written.
    pub fn save(&self, base_dir: &Path) -> Result<PathBuf> {
        let dir = base_dir.join(DAG_DIR);
        fs::create_dir_all(&dir).context("creating dag_cache directory")?;

        let path = dir.join(self.filename());
        let data = bincode::serialize(self).context("serializing DAG")?;
        fs::write(&path, data).context("writing DAG cache file")?;

        Ok(path)
    }

    /// Try to load a DAG matching the given word length and word set.
    ///
    /// Returns `Ok(None)` if no matching cache file exists.
    ///
    /// # Errors
    ///
    /// Returns an error if a matching file exists but cannot be read
    /// or deserialized.
    pub fn load(base_dir: &Path, word_length: usize, words: &[Vec<u8>]) -> Result<Option<Self>> {
        let dir = base_dir.join(DAG_DIR);
        if !dir.exists() {
            return Ok(None);
        }

        let word_set_hash = hash_word_set(words);
        let filename = format!("v{DAG_VERSION}_len{word_length}_{word_set_hash:016x}.bin");
        let path = dir.join(filename);

        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read(&path).context("reading DAG cache file")?;
        let dag: Self = bincode::deserialize(&data).context("deserializing DAG")?;

        // Verify all metadata fields.
        if dag.version != DAG_VERSION
            || dag.word_length != word_length
            || dag.word_set_hash != word_set_hash
            || dag.word_count != words.len()
        {
            return Ok(None);
        }

        Ok(Some(dag))
    }

    /// Get word indices matching a pattern (at revealed positions only).
    ///
    /// Returns `None` if the pattern was not reachable during the BFS build.
    #[must_use]
    pub fn matching_words(&self, pattern: &[u8]) -> Option<&[usize]> {
        self.pattern_matches.get(pattern).map(Vec::as_slice)
    }

    /// Number of distinct pattern nodes in the DAG.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.pattern_matches.len()
    }

    /// The stored word list.
    #[must_use]
    pub fn words(&self) -> &[Vec<u8>] {
        &self.words
    }

    /// Word length this DAG was built for.
    #[must_use]
    pub fn word_length(&self) -> usize {
        self.word_length
    }

    fn filename(&self) -> String {
        format!(
            "v{}_len{}_{:016x}.bin",
            self.version, self.word_length, self.word_set_hash
        )
    }
}

fn pattern_letters(pattern: &[u8]) -> u32 {
    pattern.iter().fold(
        0u32,
        |acc, &b| {
            if b == 0 { acc } else { acc | letter_bit(b) }
        },
    )
}

fn hash_word_set(words: &[Vec<u8>]) -> u64 {
    let mut sorted = words.to_vec();
    sorted.sort();
    let mut hasher = std::hash::DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn test_words() -> Vec<Vec<u8>> {
        ["cat", "bat", "hat", "mat", "cab", "tab"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect()
    }

    #[test]
    fn build_discovers_root() {
        let dag = PrecomputedDag::build(test_words());
        let root = vec![0u8; 3];
        assert_eq!(dag.matching_words(&root).unwrap().len(), 6);
    }

    #[test]
    fn build_discovers_child_patterns() {
        let dag = PrecomputedDag::build(test_words());
        // After revealing 'a' at position 1: pattern [0, b'a', 0]
        let pattern = vec![0, b'a', 0];
        let matching = dag.matching_words(&pattern);
        assert!(matching.is_some());
        // cat, bat, hat, mat, cab, tab all have 'a' at position 1
        assert_eq!(matching.unwrap().len(), 6);
    }

    #[test]
    fn build_leaf_pattern() {
        let words: Vec<Vec<u8>> = ["ab", "cd"].iter().map(|s| s.as_bytes().to_vec()).collect();
        let dag = PrecomputedDag::build(words);

        // Fully revealed patterns should exist.
        assert!(dag.matching_words(b"ab").is_some());
        assert!(dag.matching_words(b"cd").is_some());
    }

    #[test]
    fn unreachable_pattern_returns_none() {
        let dag = PrecomputedDag::build(test_words());
        // "zzz" is not reachable from any word in test_words.
        assert!(dag.matching_words(b"zzz").is_none());
    }

    #[test]
    fn node_count_reasonable() {
        let dag = PrecomputedDag::build(test_words());
        // Should have at least root + some children, but bounded.
        assert!(dag.node_count() > 1);
        assert!(dag.node_count() < 1000);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let words = test_words();
        let dag = PrecomputedDag::build(words.clone());

        let tmp = env::temp_dir().join("hangman2_test_dag");
        let _ = fs::remove_dir_all(&tmp);

        let path = dag.save(&tmp).unwrap();
        assert!(path.exists());

        let loaded = PrecomputedDag::load(&tmp, 3, &words).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.node_count(), dag.node_count());
        assert_eq!(loaded.word_length(), 3);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_rejects_wrong_word_set() {
        let words = test_words();
        let dag = PrecomputedDag::build(words);

        let tmp = env::temp_dir().join("hangman2_test_dag_mismatch");
        let _ = fs::remove_dir_all(&tmp);

        dag.save(&tmp).unwrap();

        // Different word set should not match.
        let other_words: Vec<Vec<u8>> = ["dog", "fog"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let loaded = PrecomputedDag::load(&tmp, 3, &other_words).unwrap();
        assert!(loaded.is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_rejects_wrong_length() {
        let words = test_words();
        let dag = PrecomputedDag::build(words.clone());

        let tmp = env::temp_dir().join("hangman2_test_dag_len");
        let _ = fs::remove_dir_all(&tmp);

        dag.save(&tmp).unwrap();

        // Wrong word length should not match.
        let loaded = PrecomputedDag::load(&tmp, 4, &words).unwrap();
        assert!(loaded.is_none());

        let _ = fs::remove_dir_all(&tmp);
    }
}
