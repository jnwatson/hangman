//! Integration tests verifying that precomputed disk caches contain EXACT
//! entries for every partition the server would look up.
//!
//! These tests mirror the server's lookup path: partition words by a guessed
//! letter, compute `fold_required_letters` + `canonical_hash_for_words`, then
//! look up in the disk cache via `decode_tt_entry` (which filters EXACT only).
//!
//! Run with: cargo test --test cache_serving -- --ignored

use std::collections::HashMap;
use std::path::PathBuf;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry, fold_required_letters, pos_mask,
};
use hangman2::solver::DiskCache;

const DICT_PATH: &str = "enable1.txt";
const CACHE_DIR: &str = "game_cache";

/// Partition word indices by a guessed letter's position mask (same as server).
fn partition_by_letter(words: &[Vec<u8>], indices: &[usize], letter: u8) -> HashMap<u32, Vec<usize>> {
    let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in indices {
        let mask = pos_mask(&words[idx], letter);
        partitions.entry(mask).or_default().push(idx);
    }
    partitions
}

/// Verify that every depth-1 partition for a given first guess has an EXACT
/// entry in the disk cache, using the server's exact lookup path.
fn verify_first_guess(words: &[Vec<u8>], dc: &DiskCache, letter: u8, k: usize) {
    let all_indices: Vec<usize> = (0..words.len()).collect();
    let masked = letter_bit(letter);
    let partitions = partition_by_letter(words, &all_indices, letter);

    for (&pmask, indices) in &partitions {
        if indices.len() <= 1 {
            continue;
        }
        let folded = fold_required_letters(words, indices, masked);
        let hash = canonical_hash_for_words(words, indices, folded);

        let packed = dc.get(hash);
        assert!(
            packed.is_some(),
            "k={k}: no cache entry for '{letter}' partition (pmask={pmask:#x}, {n} words, hash={hash:#x})",
            letter = letter as char,
            n = indices.len(),
        );

        let entry = decode_tt_entry(packed.unwrap());
        assert!(
            entry.is_some(),
            "k={k}: cache entry for '{letter}' partition is not EXACT \
             (pmask={pmask:#x}, {n} words, hash={hash:#x}, packed={packed:#x})",
            letter = letter as char,
            n = indices.len(),
            packed = packed.unwrap(),
        );
    }
}

/// Verify depth-2 partitions: after guessing letter1, then letter2, every
/// resulting partition should have an EXACT entry.
fn verify_second_guess(
    words: &[Vec<u8>],
    dc: &DiskCache,
    letter1: u8,
    letter2: u8,
    k: usize,
) {
    let all_indices: Vec<usize> = (0..words.len()).collect();
    let masked1 = letter_bit(letter1);
    let partitions1 = partition_by_letter(words, &all_indices, letter1);

    for (_pmask1, indices1) in &partitions1 {
        if indices1.len() <= 1 {
            continue;
        }
        let masked2 = masked1 | letter_bit(letter2);
        let partitions2 = partition_by_letter(words, indices1, letter2);

        for (&pmask2, indices2) in &partitions2 {
            if indices2.len() <= 1 {
                continue;
            }
            let folded = fold_required_letters(words, indices2, masked2);
            let hash = canonical_hash_for_words(words, indices2, folded);

            let packed = dc.get(hash);
            assert!(
                packed.is_some(),
                "k={k}: no cache entry for '{l1}'+'{l2}' partition \
                 (pmask={pmask2:#x}, {n} words, hash={hash:#x})",
                l1 = letter1 as char,
                l2 = letter2 as char,
                n = indices2.len(),
            );

            let entry = decode_tt_entry(packed.unwrap());
            assert!(
                entry.is_some(),
                "k={k}: cache entry for '{l1}'+'{l2}' partition is not EXACT \
                 (pmask={pmask2:#x}, {n} words, hash={hash:#x}, packed={packed:#x})",
                l1 = letter1 as char,
                l2 = letter2 as char,
                n = indices2.len(),
                packed = packed.unwrap(),
            );
        }
    }
}

#[test]
#[ignore] // requires enable1.txt and game_cache/ with precomputed data
fn k13_all_first_guesses_have_exact_entries() {
    let dict = Dictionary::from_file(&PathBuf::from(DICT_PATH))
        .expect("dictionary not found — run from project root");
    let words: Vec<Vec<u8>> = dict.words_of_length(13).to_vec();
    assert!(!words.is_empty(), "no 13-letter words in dictionary");

    let dc = DiskCache::open(&PathBuf::from(CACHE_DIR), 13, &words, 16 * 1024 * 1024 * 1024)
        .expect("failed to open k=13 disk cache");

    for letter in b'a'..=b'z' {
        verify_first_guess(&words, &dc, letter, 13);
    }
}

#[test]
#[ignore] // requires enable1.txt and game_cache/ with precomputed data
fn k13_all_second_guesses_have_exact_entries() {
    let dict = Dictionary::from_file(&PathBuf::from(DICT_PATH))
        .expect("dictionary not found — run from project root");
    let words: Vec<Vec<u8>> = dict.words_of_length(13).to_vec();
    assert!(!words.is_empty(), "no 13-letter words in dictionary");

    let dc = DiskCache::open(&PathBuf::from(CACHE_DIR), 13, &words, 16 * 1024 * 1024 * 1024)
        .expect("failed to open k=13 disk cache");

    for l1 in b'a'..=b'z' {
        for l2 in b'a'..=b'z' {
            if l1 == l2 {
                continue;
            }
            verify_second_guess(&words, &dc, l1, l2, 13);
        }
    }
}

/// Verify that solve_position_smp produces EXACT entries retrievable by the
/// server's lookup path. This test does NOT require pre-existing caches —
/// it solves from scratch to validate the fix.
#[test]
fn solve_position_smp_root_entries_are_exact() {
    use hangman2::solver::MemoizedSolver;

    let words: Vec<Vec<u8>> = [
        "cat", "bat", "hat", "mat", "sat", "rat", "fat", "vat",
        "dog", "fog", "log", "hog", "bog", "cog", "jog", "tog",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    let all_indices: Vec<usize> = (0..words.len()).collect();

    // Test multiple first guesses to exercise different partition shapes.
    for letter in b'a'..=b'z' {
        let masked = letter_bit(letter);
        let partitions = partition_by_letter(&words, &all_indices, letter);

        let solver = MemoizedSolver::new();

        for (_pmask, indices) in &partitions {
            if indices.len() <= 1 {
                continue;
            }
            solver.solve_position_smp(&words, indices, masked);
        }

        // Verify every partition has an EXACT entry.
        for (_pmask, indices) in &partitions {
            if indices.len() <= 1 {
                continue;
            }
            let folded = fold_required_letters(&words, indices, masked);
            let hash = canonical_hash_for_words(&words, indices, folded);

            let packed = solver.cache().get(&hash).map(|v| *v);
            assert!(
                packed.is_some(),
                "no cache entry after solve_position_smp for '{}' partition ({} words)",
                letter as char,
                indices.len(),
            );

            let entry = decode_tt_entry(packed.unwrap());
            assert!(
                entry.is_some(),
                "entry for '{}' partition ({} words) is not EXACT (packed={:#x})",
                letter as char,
                indices.len(),
                packed.unwrap(),
            );
        }
    }
}
