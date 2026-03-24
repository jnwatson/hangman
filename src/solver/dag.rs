use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use rustc_hash::FxHashMap;

use super::canon::dedup_and_hash;
use crate::game::{LetterSet, letter_bit};

// Cache entry encoding (matches memoized.rs format).
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

struct WordEntry {
    word_idx: usize,
    /// Letters present at this node's unrevealed positions.
    unrevealed_letters: LetterSet,
}

struct DagNode {
    /// Bitmask of position indices that are unrevealed at this pattern.
    unrevealed_mask: u32,
    /// Union of `unrevealed_letters` across all words at this node.
    useful_letters: LetterSet,
    /// Bitmask of letters revealed at this pattern's positions.
    revealed_letters: LetterSet,
    /// Words matching this pattern.
    word_entries: Vec<WordEntry>,
    /// For each letter (indexed by `letter - b'a'`): list of
    /// `(unrevealed_pos_mask, child_node_id)` for hit outcomes.
    hit_children: [Vec<(u32, u32)>; 26],
}

enum CacheLookup {
    Hit(u32),
    Miss {
        tt_move: Option<u8>,
        alpha: u32,
        beta: u32,
    },
}

/// Hybrid DAG + canonical-hash solver.
///
/// Precomputes a DAG of reveal patterns for O(1) state identification
/// and navigation. Uses canonical structural hashing (letter/position
/// isomorphism) for the transposition table, giving maximal state
/// compression.
///
/// State identification: `(node_id, imp_wrong)` is O(1) to compute.
/// A fast key cache maps this to the canonical 128-bit hash, avoiding
/// the expensive `dedup_and_hash` on repeat visits.
///
/// Navigation: precomputed successor edges give O(1) child lookup
/// per partition, with `useful_letters` coalescing that drops
/// irrelevant wrong guesses at each transition.
pub struct DagSolver {
    words: Vec<Vec<u8>>,
    nodes: Vec<DagNode>,
    root: u32,
    /// `pos_masks[letter_idx][word_idx]` = position bitmask.
    pos_masks: Vec<Vec<u32>>,
    /// Canonical hash → packed value.
    cache: DashMap<u128, u32>,
    /// `(node_id, imp_wrong)` → canonical hash. O(1) lookup avoids
    /// recomputing `dedup_and_hash` on repeat visits.
    key_cache: DashMap<u64, u128>,
    hash_calls: AtomicU64,
    cache_hits: AtomicU64,
}

impl DagSolver {
    /// Build a hybrid DAG solver for the given word list.
    ///
    /// # Panics
    ///
    /// Panics if words is empty or word length exceeds 16.
    #[must_use]
    pub fn new(words: &[Vec<u8>]) -> Self {
        assert!(!words.is_empty());
        let n = words.len();
        let word_len = words[0].len();
        assert!(word_len <= 16, "word length must be <= 16");

        let mut pos_masks = vec![vec![0u32; n]; 26];
        for (idx, word) in words.iter().enumerate() {
            for (j, &b) in word.iter().enumerate() {
                if b.is_ascii_lowercase() {
                    pos_masks[(b - b'a') as usize][idx] |= 1 << j;
                }
            }
        }

        let (nodes, root) = Self::build_dag(words, &pos_masks, word_len);

        Self {
            words: words.to_vec(),
            nodes,
            root,
            pos_masks,
            cache: DashMap::new(),
            key_cache: DashMap::new(),
            hash_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
        }
    }

    fn build_dag(
        words: &[Vec<u8>],
        pos_masks: &[Vec<u32>],
        word_len: usize,
    ) -> (Vec<DagNode>, u32) {
        let n = words.len();
        let mut nodes: Vec<DagNode> = Vec::new();
        let mut pattern_to_id: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
        let mut patterns: Vec<Vec<u8>> = Vec::new();

        let root_pattern = vec![0u8; word_len];
        #[allow(clippy::cast_possible_truncation)]
        let root_unrevealed = if word_len < 32 {
            (1u32 << word_len) - 1
        } else {
            u32::MAX
        };
        let root_entries: Vec<WordEntry> = (0..n)
            .map(|idx| WordEntry {
                word_idx: idx,
                unrevealed_letters: Self::unrevealed_letters(pos_masks, idx, root_unrevealed),
            })
            .collect();
        let root_useful = root_entries
            .iter()
            .fold(0u32, |acc, e| acc | e.unrevealed_letters);

        nodes.push(DagNode {
            unrevealed_mask: root_unrevealed,
            useful_letters: root_useful,
            revealed_letters: 0,
            word_entries: root_entries,
            hit_children: std::array::from_fn(|_| Vec::new()),
        });
        pattern_to_id.insert(root_pattern.clone(), 0);
        patterns.push(root_pattern);

        let mut queue_idx = 0;
        while queue_idx < nodes.len() {
            #[allow(clippy::cast_possible_truncation)]
            let node_id = queue_idx as u32;
            queue_idx += 1;

            let useful = nodes[node_id as usize].useful_letters;
            let unrevealed_mask = nodes[node_id as usize].unrevealed_mask;
            let word_indices: Vec<usize> = nodes[node_id as usize]
                .word_entries
                .iter()
                .map(|e| e.word_idx)
                .collect();
            let pattern = patterns[node_id as usize].clone();

            for li in 0..26u8 {
                if useful & (1u32 << li) == 0 {
                    continue;
                }

                let mut part_map: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
                for &word_idx in &word_indices {
                    let mask = pos_masks[li as usize][word_idx] & unrevealed_mask;
                    part_map.entry(mask).or_default().push(word_idx);
                }

                let mut hit_succs = Vec::new();
                for (&mask, child_word_indices) in &part_map {
                    if mask == 0 {
                        continue;
                    }

                    let mut child_pattern = pattern.clone();
                    for (j, slot) in child_pattern.iter_mut().enumerate() {
                        if mask & (1 << j) != 0 {
                            *slot = b'a' + li;
                        }
                    }

                    let child_id = if let Some(&id) = pattern_to_id.get(&child_pattern) {
                        id
                    } else {
                        let child_unrevealed = unrevealed_mask & !mask;
                        let child_revealed = pattern_letters(&child_pattern);
                        let child_entries: Vec<WordEntry> = child_word_indices
                            .iter()
                            .map(|&idx| WordEntry {
                                word_idx: idx,
                                unrevealed_letters: Self::unrevealed_letters(
                                    pos_masks,
                                    idx,
                                    child_unrevealed,
                                ),
                            })
                            .collect();
                        let child_useful = child_entries
                            .iter()
                            .fold(0u32, |acc, e| acc | e.unrevealed_letters);

                        #[allow(clippy::cast_possible_truncation)]
                        let id = nodes.len() as u32;
                        nodes.push(DagNode {
                            unrevealed_mask: child_unrevealed,
                            useful_letters: child_useful,
                            revealed_letters: child_revealed,
                            word_entries: child_entries,
                            hit_children: std::array::from_fn(|_| Vec::new()),
                        });
                        pattern_to_id.insert(child_pattern.clone(), id);
                        patterns.push(child_pattern);
                        id
                    };

                    hit_succs.push((mask, child_id));
                }

                nodes[node_id as usize].hit_children[li as usize] = hit_succs;
            }
        }

        (nodes, 0)
    }

    fn unrevealed_letters(
        pos_masks: &[Vec<u32>],
        word_idx: usize,
        unrevealed_mask: u32,
    ) -> LetterSet {
        let mut ul = 0u32;
        for (li, masks) in pos_masks.iter().enumerate() {
            if masks[word_idx] & unrevealed_mask != 0 {
                ul |= 1 << li;
            }
        }
        ul
    }

    /// Solve: returns the minimax-optimal number of misses.
    #[must_use]
    pub fn solve(&self) -> u32 {
        self.solve_state(self.root, 0, 0, u32::MAX)
    }

    #[inline]
    fn fast_key(node_id: u32, imp_wrong: LetterSet) -> u64 {
        (u64::from(node_id) << 32) | u64::from(imp_wrong)
    }

    fn solve_state(
        &self,
        node_id: u32,
        imp_wrong: LetterSet,
        alpha: u32,
        beta: u32,
    ) -> u32 {
        if beta == 0 {
            return 0;
        }

        // O(1) fast path: (node_id, imp_wrong) → canonical key → TT hit.
        let fast_key = Self::fast_key(node_id, imp_wrong);
        if let Some(entry) = self.key_cache.get(&fast_key) {
            let canon_key = *entry;
            drop(entry);
            self.hash_calls.fetch_add(1, Ordering::Relaxed);
            if let CacheLookup::Hit(val) = self.cache_lookup(canon_key, alpha, beta) {
                return val;
            }
        }

        // Slow path: filter valid words, compute canonical hash.
        let node = &self.nodes[node_id as usize];
        let mut valid_count = 0u32;
        let mut union_ul: LetterSet = 0;
        let mut inter_ul: LetterSet = !0;
        let mut valid_indices: Vec<usize> = Vec::new();

        for entry in &node.word_entries {
            if entry.unrevealed_letters & imp_wrong == 0 {
                valid_count += 1;
                union_ul |= entry.unrevealed_letters;
                inter_ul &= entry.unrevealed_letters;
                valid_indices.push(entry.word_idx);
            }
        }

        if valid_count <= 1 {
            self.key_cache.insert(fast_key, 0);
            return 0;
        }

        let actual_useful = union_ul & !imp_wrong;
        if actual_useful == 0 {
            self.key_cache.insert(fast_key, 0);
            return 0;
        }

        // Required/useful letter analysis (needed before canonical hash).
        let required = inter_ul & actual_useful;
        let required_splitting = self.find_splitting_required(node, imp_wrong, required);
        let non_splitting_required = required & !required_splitting;

        // Canonical key via structural dedup+hash.
        // Include non-splitting required letters in masked for maximal
        // canonical compression (matches memoized solver behavior).
        let masked = node.revealed_letters | imp_wrong | non_splitting_required;
        let (_deduped, canon_key) = dedup_and_hash(&self.words, &valid_indices, masked);
        self.key_cache.insert(fast_key, canon_key);

        self.hash_calls.fetch_add(1, Ordering::Relaxed);
        let n_useful = actual_useful.count_ones();
        let n_required = required.count_ones();
        let max_misses = n_useful - n_required;
        let beta = beta.min(max_misses);
        if alpha >= beta {
            return alpha;
        }

        // TT lookup with canonical key.
        let (tt_move, alpha, beta) = match self.cache_lookup(canon_key, alpha, beta) {
            CacheLookup::Hit(val) => return val,
            CacheLookup::Miss {
                tt_move,
                alpha,
                beta,
            } => (tt_move, alpha, beta),
        };

        // Select and order candidate letters.
        let candidate_mask = if required_splitting != 0 {
            required_splitting
        } else {
            actual_useful
        };
        let mut letters = self.order_letters(node, imp_wrong, candidate_mask);

        if let Some(tt_letter) = tt_move
            && let Some(pos) = letters.iter().position(|&l| l == tt_letter)
            && pos > 0
        {
            letters[..=pos].rotate_right(1);
        }

        // Evaluate: guesser minimizes.
        let mut best = u32::MAX;
        let mut best_letter = letters[0];
        for &letter in &letters {
            let cutoff = best.min(beta);
            let val = self.evaluate_letter(node_id, imp_wrong, letter, cutoff);
            if val < best {
                best = val;
                best_letter = letter;
            }
            if best <= alpha || best == 0 {
                break;
            }
        }

        self.cache_store(canon_key, best, best_letter, alpha, beta);
        best
    }

    fn find_splitting_required(
        &self,
        node: &DagNode,
        imp_wrong: LetterSet,
        required: LetterSet,
    ) -> LetterSet {
        let mut splitting = 0u32;
        for li in 0..26u8 {
            let bit = 1u32 << li;
            if required & bit == 0 {
                continue;
            }
            let mut first_mask: Option<u32> = None;
            let mut splits = false;
            for entry in &node.word_entries {
                if entry.unrevealed_letters & imp_wrong != 0 {
                    continue;
                }
                let mask =
                    self.pos_masks[li as usize][entry.word_idx] & node.unrevealed_mask;
                match first_mask {
                    None => first_mask = Some(mask),
                    Some(fm) => {
                        if mask != fm {
                            splits = true;
                            break;
                        }
                    }
                }
            }
            if splits {
                splitting |= bit;
            }
        }
        splitting
    }

    fn cache_lookup(&self, key: u128, mut alpha: u32, mut beta: u32) -> CacheLookup {
        if let Some(packed) = self.cache.get(&key) {
            let (val, best_letter, bound) = cache_unpack(*packed);
            match bound {
                BOUND_EXACT => {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return CacheLookup::Hit(val);
                }
                BOUND_LOWER => {
                    if val >= beta {
                        self.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    alpha = alpha.max(val);
                }
                BOUND_UPPER => {
                    if val <= alpha {
                        self.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return CacheLookup::Hit(val);
                    }
                    beta = beta.min(val);
                }
                _ => {}
            }
            if alpha >= beta {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
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

    fn cache_store(&self, key: u128, best: u32, best_letter: u8, alpha: u32, beta: u32) {
        if best <= alpha {
            let packed = cache_pack(best, best_letter, BOUND_UPPER);
            if let Some(mut existing) = self.cache.get_mut(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                if old_bound == BOUND_UPPER && best < old_val {
                    *existing = packed;
                }
            } else {
                self.cache.insert(key, packed);
            }
        } else if best >= beta {
            let packed = cache_pack(best, best_letter, BOUND_LOWER);
            if let Some(mut existing) = self.cache.get_mut(&key) {
                let (old_val, _, old_bound) = cache_unpack(*existing);
                if old_bound == BOUND_LOWER && best > old_val {
                    *existing = packed;
                }
            } else {
                self.cache.insert(key, packed);
            }
        } else {
            self.cache
                .insert(key, cache_pack(best, best_letter, BOUND_EXACT));
        }
    }

    fn order_letters(
        &self,
        node: &DagNode,
        imp_wrong: LetterSet,
        candidate_mask: LetterSet,
    ) -> Vec<u8> {
        if candidate_mask.count_ones() <= 1 {
            return (0..26u8)
                .filter(|&i| candidate_mask & (1 << i) != 0)
                .map(|i| b'a' + i)
                .collect();
        }

        let mut scored: Vec<(u8, usize)> = Vec::new();
        let mut sizes: FxHashMap<u32, usize> = FxHashMap::default();

        for li in 0..26u8 {
            if candidate_mask & (1u32 << li) == 0 {
                continue;
            }
            sizes.clear();
            for entry in &node.word_entries {
                if entry.unrevealed_letters & imp_wrong != 0 {
                    continue;
                }
                let mask = self.pos_masks[li as usize][entry.word_idx] & node.unrevealed_mask;
                *sizes.entry(mask).or_insert(0) += 1;
            }
            let max_part = sizes.values().max().copied().unwrap_or(0);
            scored.push((b'a' + li, max_part));
        }

        scored.sort_by_key(|&(_, mp)| mp);
        scored.into_iter().map(|(letter, _)| letter).collect()
    }

    /// Evaluate a letter guess. Partitions valid words by position mask,
    /// then recurses via DAG successor edges.
    fn evaluate_letter(
        &self,
        node_id: u32,
        imp_wrong: LetterSet,
        letter: u8,
        cutoff: u32,
    ) -> u32 {
        let node = &self.nodes[node_id as usize];
        let li = (letter - b'a') as usize;

        let mut has_miss = false;
        let mut hit_masks: FxHashMap<u32, usize> = FxHashMap::default();
        for entry in &node.word_entries {
            if entry.unrevealed_letters & imp_wrong != 0 {
                continue;
            }
            let mask = self.pos_masks[li][entry.word_idx] & node.unrevealed_mask;
            if mask == 0 {
                has_miss = true;
            } else {
                *hit_masks.entry(mask).or_insert(0) += 1;
            }
        }

        let mut worst = 0u32;

        // Miss partition: stay at same node, imp_wrong grows.
        if has_miss {
            if cutoff <= 1 {
                return cutoff;
            }
            let child_beta = cutoff - 1;
            let miss_val = 1 + self.solve_state(
                node_id,
                imp_wrong | letter_bit(letter),
                worst.saturating_sub(1),
                child_beta,
            );
            worst = worst.max(miss_val);
            if worst >= cutoff {
                return worst;
            }
        }

        // Hit partitions: advance to child via DAG edges. Largest first.
        let mut hit_parts: Vec<(u32, usize)> = hit_masks.into_iter().collect();
        hit_parts.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        for (mask, _count) in hit_parts {
            let Some(&(_, child_id)) = node.hit_children[li]
                .iter()
                .find(|&&(m, _)| m == mask)
            else {
                continue;
            };

            let child_imp = imp_wrong & self.nodes[child_id as usize].useful_letters;
            let val = self.solve_state(child_id, child_imp, worst, cutoff);

            worst = worst.max(val);
            if worst >= cutoff {
                return worst;
            }
        }

        worst
    }

    #[must_use]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// Number of EXACT entries in the transposition table.
    #[must_use]
    pub fn exact_cache_size(&self) -> usize {
        self.cache
            .iter()
            .filter(|e| cache_unpack(*e.value()).2 == BOUND_EXACT)
            .count()
    }

    #[must_use]
    pub fn hash_calls(&self) -> u64 {
        self.hash_calls.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn key_cache_size(&self) -> usize {
        self.key_cache.len()
    }
}

fn pattern_letters(pattern: &[u8]) -> LetterSet {
    pattern.iter().fold(0u32, |acc, &b| {
        if b == 0 {
            acc
        } else {
            acc | letter_bit(b)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::naive::NaiveSolver;
    use super::*;

    fn solve_both(words: &[&str]) -> u32 {
        let owned: Vec<Vec<u8>> = words.iter().map(|s| s.as_bytes().to_vec()).collect();
        let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let naive_result = NaiveSolver::solve(&refs, 0);

        let solver = DagSolver::new(&owned);
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
        let solver = DagSolver::new(&words);
        let _ = solver.solve();
        assert!(solver.cache_size() > 0);
    }

    #[test]
    fn dag_node_count_reasonable() {
        let words: Vec<Vec<u8>> = ["cat", "bat", "hat", "mat", "cab", "tab"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let solver = DagSolver::new(&words);
        assert!(solver.node_count() > 1);
        assert!(solver.node_count() < 1000);
    }

    #[test]
    fn canonical_isomorphism_shares_cache() {
        // Two isomorphic word sets should produce the same canonical hash.
        let words: Vec<Vec<u8>> = ["ab", "cd", "ef", "gh"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let solver = DagSolver::new(&words);
        let _ = solver.solve();
        // With canonical hashing, isomorphic subproblems share TT entries.
        // The cache should be much smaller than key_cache.
        assert!(solver.cache_size() <= solver.key_cache_size());
    }
}
