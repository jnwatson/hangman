use std::hash::{Hash, Hasher};

/// Canonicalize a set of signatures under simultaneous letter permutation
/// AND position (column) permutation.
///
/// Two signature sets that are isomorphic under any combination of:
/// - Relabeling letters (e.g., every `a` becomes `x`, every `b` becomes `y`)
/// - Permuting positions (e.g., swapping column 0 and column 1 in all sigs)
///
/// will produce the same canonical form.
///
/// For word lengths ≤ 8, all column permutations are tried (exact).
/// For word lengths > 8, a heuristic approach is used.
///
/// Byte value 0 is treated as a fixed "blank" marker (used by the memoized
/// solver for masked/revealed positions). It is never relabeled.
#[cfg(test)]
pub(super) fn canonicalize(sigs: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if sigs.is_empty() {
        return vec![];
    }
    let k = sigs[0].len();
    if k == 0 {
        return vec![vec![]; sigs.len()];
    }

    if k <= 8 {
        canonicalize_exact(sigs, k)
    } else {
        canonicalize_heuristic(sigs, k)
    }
}

/// Compute a 128-bit hash of the canonical form.
///
/// Uses a fast heuristic canonicalization for performance.
/// This may produce slightly more cache entries than exact canonicalization
/// (missing some position isomorphisms) but is much faster.
pub(super) fn canonical_hash(sigs: &[Vec<u8>]) -> u128 {
    if sigs.is_empty() {
        return 0;
    }
    let k = sigs[0].len();
    if k == 0 {
        return 0;
    }
    canonical_hash_fast(sigs, k)
}

/// Combined dedup + canonical hash for the hot path.
///
/// Returns `(deduped_indices, canonical_hash)`. The deduped indices keep one
/// representative per unique effective signature. The canonical hash is
/// computed on the deduped effective signatures (sort→relabel→sort→relabel→hash).
///
/// This avoids computing effective signatures twice (once for dedup, once for
/// hashing).
pub(super) fn dedup_and_hash(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
) -> (Vec<usize>, u128) {
    if indices.is_empty() {
        return (vec![], 0);
    }
    let k = words[indices[0]].len();
    if k == 0 {
        return (indices.to_vec(), 0);
    }

    if k <= 8 {
        return dedup_and_hash_small_k(words, indices, masked, k);
    }

    if k <= 16 {
        return dedup_and_hash_medium_k(words, indices, masked, k);
    }

    dedup_and_hash_general(words, indices, masked, k)
}

/// Dedup only: collapse identical effective signatures without computing
/// the canonical hash. Used when the canonical key is already known from
/// the key_cache but the TT missed.
pub(super) fn dedup_only(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
) -> Vec<usize> {
    if indices.is_empty() {
        return vec![];
    }
    let k = words[indices[0]].len();
    if k == 0 {
        return indices.to_vec();
    }

    if k <= 8 {
        return dedup_only_small_k(words, indices, masked, k);
    }

    // For k > 8, fall back to the full function and discard the hash.
    dedup_and_hash(words, indices, masked).0
}

/// Fast dedup for k ≤ 8: encode each sig as u64, sort, and dedup.
/// Skips the expensive canonicalization step (relabel + column permutation).
fn dedup_only_small_k(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    k: usize,
) -> Vec<usize> {
    use crate::game::letter_bit;

    let n = indices.len();
    let mut pairs: Vec<(u64, usize)> = Vec::with_capacity(n);
    for &idx in indices {
        let word = &words[idx];
        let mut key = 0u64;
        for &b in word.iter().take(k) {
            let eff = if masked & letter_bit(b) != 0 { 0 } else { b };
            key = key << 8 | u64::from(eff);
        }
        pairs.push((key, idx));
    }

    pairs.sort_unstable();

    let mut deduped: Vec<usize> = Vec::new();
    let mut prev_key = u64::MAX;
    for &(key, idx) in &pairs {
        if key != prev_key {
            prev_key = key;
            deduped.push(idx);
        }
    }
    deduped
}

/// Specialized path for k ≤ 8: encode each sig as a u64.
fn dedup_and_hash_small_k(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    k: usize,
) -> (Vec<usize>, u128) {
    use crate::game::letter_bit;

    let n = indices.len();

    // Encode each effective sig as u64 (up to 8 bytes).
    let mut pairs: Vec<(u64, usize)> = Vec::with_capacity(n);
    for &idx in indices {
        let word = &words[idx];
        let mut key = 0u64;
        for &b in word.iter().take(k) {
            let eff = if masked & letter_bit(b) != 0 { 0 } else { b };
            key = key << 8 | u64::from(eff);
        }
        pairs.push((key, idx));
    }

    // Sort by sig key.
    pairs.sort_unstable();

    // Dedup and collect unique sigs + representative indices.
    let mut deduped: Vec<usize> = Vec::new();
    let mut unique_keys: Vec<u64> = Vec::new();
    for &(key, idx) in &pairs {
        if unique_keys.last() != Some(&key) {
            unique_keys.push(key);
            deduped.push(idx);
        }
    }

    if deduped.len() <= 1 {
        return (deduped, 0);
    }

    // Canonicalize: decode unique u64 keys into flat buffer, then
    // relabel → sort → relabel → hash.
    let m = unique_keys.len();
    let total = m * k;
    let mut buf = vec![0u8; total];
    for (i, &key) in unique_keys.iter().enumerate() {
        for j in 0..k {
            buf[i * k + j] = ((key >> (8 * (k - 1 - j))) & 0xFF) as u8;
        }
    }

    // Rows are already sorted from dedup. Canonicalize with column awareness.
    (deduped, canonicalize_sorted_rows(&mut buf, m, k))
}

/// Specialized path for k=9-16: encode each sig as a u128.
fn dedup_and_hash_medium_k(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    k: usize,
) -> (Vec<usize>, u128) {
    use crate::game::letter_bit;

    let n = indices.len();

    // Encode each effective sig as u128 (up to 16 bytes).
    let mut pairs: Vec<(u128, usize)> = Vec::with_capacity(n);
    for &idx in indices {
        let word = &words[idx];
        let mut key = 0u128;
        for &b in word.iter().take(k) {
            let eff = if masked & letter_bit(b) != 0 { 0 } else { b };
            key = key << 8 | u128::from(eff);
        }
        pairs.push((key, idx));
    }

    // Sort by sig key.
    pairs.sort_unstable();

    // Dedup and collect unique sigs + representative indices.
    let mut deduped: Vec<usize> = Vec::new();
    let mut unique_keys: Vec<u128> = Vec::new();
    for &(key, idx) in &pairs {
        if unique_keys.last() != Some(&key) {
            unique_keys.push(key);
            deduped.push(idx);
        }
    }

    if deduped.len() <= 1 {
        return (deduped, 0);
    }

    // Canonicalize: decode unique u128 keys into flat buffer, then
    // relabel → sort → relabel → hash.
    let m = unique_keys.len();
    let total = m * k;
    let mut buf = vec![0u8; total];
    for (i, &key) in unique_keys.iter().enumerate() {
        for j in 0..k {
            buf[i * k + j] = ((key >> (8 * (k - 1 - j))) & 0xFF) as u8;
        }
    }

    // Rows are already sorted from dedup. Canonicalize with column awareness.
    (deduped, canonicalize_sorted_rows(&mut buf, m, k))
}

/// General path for k > 16.
fn dedup_and_hash_general(
    words: &[Vec<u8>],
    indices: &[usize],
    masked: u32,
    k: usize,
) -> (Vec<usize>, u128) {
    use crate::game::letter_bit;

    let n = indices.len();
    let mut buf = vec![0u8; n * k];
    for (i, &idx) in indices.iter().enumerate() {
        let word = &words[idx];
        let row = &mut buf[i * k..(i + 1) * k];
        for (j, &b) in word.iter().enumerate() {
            row[j] = if masked & letter_bit(b) != 0 { 0 } else { b };
        }
    }

    // Sort by sig content.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| buf[a * k..(a + 1) * k].cmp(&buf[b * k..(b + 1) * k]));

    // Reorder and dedup.
    let mut sorted_buf = vec![0u8; n * k];
    let mut deduped: Vec<usize> = Vec::new();
    let mut unique_count = 0;

    for (i, &src) in order.iter().enumerate() {
        let src_row = &buf[src * k..(src + 1) * k];
        let is_new = i == 0 || src_row != &sorted_buf[(unique_count - 1) * k..unique_count * k];
        if is_new {
            sorted_buf[unique_count * k..(unique_count + 1) * k].copy_from_slice(src_row);
            deduped.push(indices[src]);
            unique_count += 1;
        }
    }

    if deduped.len() <= 1 {
        return (deduped, 0);
    }

    sorted_buf.truncate(unique_count * k);

    // Rows are already sorted from dedup. Canonicalize with column awareness.
    (
        deduped,
        canonicalize_sorted_rows(&mut sorted_buf, unique_count, k),
    )
}

/// Canonicalize with column merging + column permutation + letter relabeling.
fn canonical_hash_fast(sigs: &[Vec<u8>], k: usize) -> u128 {
    let n = sigs.len();
    let total = n * k;
    let mut buf = vec![0u8; total];

    for (i, sig) in sigs.iter().enumerate() {
        buf[i * k..(i + 1) * k].copy_from_slice(sig);
    }

    // Sort rows first for consistent starting point.
    let mut tmp = vec![0u8; total];
    let mut indices: Vec<usize> = (0..n).collect();
    sort_flat_rows(&mut buf, &mut tmp, &mut indices, n, k);

    canonicalize_sorted_rows(&mut buf, n, k)
}

// ---------------------------------------------------------------------------
// Column operations infrastructure. Tested and found to provide negligible
// cache reduction for English dictionaries (verified: zero improvement for
// k=2 at 66.5M entries, 55% slowdown for k=3). Kept for future experiments.
// ---------------------------------------------------------------------------

/// Maximum budget for exact column permutation: factorial(ek) * m ≤ this.
#[allow(dead_code)]
const EXACT_COL_BUDGET: usize = 500_000;

#[allow(dead_code)]
fn factorial(n: usize) -> usize {
    match n {
        0 | 1 => 1,
        _ => (2..=n).product(),
    }
}

/// Merge identical columns in a flat buffer (m rows, k cols).
/// Returns `Some((new_buffer, new_k))` if columns were merged, `None` if
/// all columns are already distinct.
#[allow(dead_code)]
fn merge_identical_columns(buf: &[u8], m: usize, k: usize) -> Option<(Vec<u8>, usize)> {
    if k <= 1 {
        return None;
    }

    // Quick check: if first row values are all distinct, no merge possible.
    let first_row = &buf[..k];
    let mut seen = [false; 256];
    let mut has_dup = false;
    for &b in first_row {
        if seen[b as usize] {
            has_dup = true;
            break;
        }
        seen[b as usize] = true;
    }
    if !has_dup {
        return None;
    }

    // Full check: compare columns with matching first-row values.
    let mut keep = vec![true; k];
    let mut merged_any = false;
    for j2 in 1..k {
        if !keep[j2] {
            continue;
        }
        for j1 in 0..j2 {
            if !keep[j1] {
                continue;
            }
            // Quick reject: first-row values must match.
            if buf[j1] != buf[j2] {
                continue;
            }
            if (0..m).all(|i| buf[i * k + j1] == buf[i * k + j2]) {
                keep[j2] = false;
                merged_any = true;
                break;
            }
        }
    }

    if !merged_any {
        return None;
    }

    let cols: Vec<usize> = (0..k).filter(|&j| keep[j]).collect();
    let ek = cols.len();
    let mut out = vec![0u8; m * ek];
    for i in 0..m {
        for (nj, &oj) in cols.iter().enumerate() {
            out[i * ek + nj] = buf[i * k + oj];
        }
    }
    Some((out, ek))
}

/// Sort columns lexicographically by their column profile (values top-to-bottom).
#[allow(dead_code)]
fn sort_flat_cols(buf: &mut [u8], m: usize, k: usize) {
    if k <= 1 {
        return;
    }

    let mut col_order: Vec<usize> = (0..k).collect();
    col_order.sort_by(|&a, &b| {
        for i in 0..m {
            match buf[i * k + a].cmp(&buf[i * k + b]) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
        }
        std::cmp::Ordering::Equal
    });

    // Check if already in order.
    if col_order.iter().enumerate().all(|(i, &c)| i == c) {
        return;
    }

    let old = buf[..m * k].to_vec();
    for i in 0..m {
        for (nj, &oj) in col_order.iter().enumerate() {
            buf[i * k + nj] = old[i * k + oj];
        }
    }
}

/// Canonicalize a flat buffer whose rows are already sorted from dedup.
///
/// Steps:
/// 1. Drop all-zero columns (dead positions). Two states that differ only in
///    which position is "dead" share game structure, so they should share a
///    TT entry. Specifically, when a precompute top-level guess hits on
///    letter L at position P for all N remaining words, position P becomes
///    permanently 0 for all sub-problems; canonicalizing to a (k-1)-column
///    form collapses this whole sub-tree with its symmetric peers.
/// 2. Letter-only canonicalization: relabel → sort → relabel → hash.
///
/// Column permutation was tested separately and found net-negative for real
/// English dictionaries, so we don't do it here — we only drop columns that
/// are provably dead across all rows.
fn canonicalize_sorted_rows(buf: &mut [u8], m: usize, k: usize) -> u128 {
    if m <= 1 || k == 0 {
        return 0;
    }

    // Drop all-zero columns if present. Operates on a slice because when all
    // columns are live we can skip allocation entirely.
    let (buf, k) = match collapse_zero_cols(buf, m, k) {
        Some((owned, ek)) => (owned, ek),
        None => (buf[..m * k].to_vec(), k),
    };
    if k == 0 {
        // All columns were dead — all sigs are empty. Distinct rows are
        // impossible here (they would have had the same u64 key and been
        // deduped). Return 0 as the canonical hash of "zero useful state."
        return 0;
    }

    let total = m * k;
    let mut buf = buf;
    let mut tmp = vec![0u8; total];
    let mut indices: Vec<usize> = (0..m).collect();
    relabel_flat(&mut buf);

    let needs_resort = (1..m).any(|i| buf[(i - 1) * k..i * k] > buf[i * k..(i + 1) * k]);
    if needs_resort {
        sort_flat_rows(&mut buf, &mut tmp, &mut indices, m, k);
        relabel_flat(&mut buf);
    }

    hash_flat(&buf)
}

/// Check for all-zero columns in a row-major m×k buffer. Returns `None` if
/// all columns have at least one non-zero byte. Otherwise returns an
/// `m × live_k` buffer with the dead columns removed (`live_k` = columns
/// that had at least one non-zero byte).
///
/// Preserves row order: since dead columns have byte 0 in every row, they
/// can't affect lex ordering of rows, so post-collapse rows are still sorted
/// if the pre-collapse rows were.
fn collapse_zero_cols(buf: &[u8], m: usize, k: usize) -> Option<(Vec<u8>, usize)> {
    if k == 0 || m == 0 {
        return None;
    }

    let mut live = 0u64;
    debug_assert!(k <= 64, "collapse_zero_cols assumes k fits in u64 bitmask");
    for i in 0..m {
        let row = &buf[i * k..(i + 1) * k];
        for (j, &b) in row.iter().enumerate() {
            if b != 0 {
                live |= 1u64 << j;
            }
        }
        // Early exit: every column already has a non-zero byte.
        if live.count_ones() as usize == k {
            return None;
        }
    }

    let new_k = live.count_ones() as usize;
    if new_k == k {
        return None;
    }

    let mut out = vec![0u8; m * new_k];
    for i in 0..m {
        let mut w = 0usize;
        for j in 0..k {
            if live & (1u64 << j) != 0 {
                out[i * new_k + w] = buf[i * k + j];
                w += 1;
            }
        }
    }
    Some((out, new_k))
}

/// Try all ek! column permutations, canonicalize each, return min hash.
#[allow(dead_code)]
fn exact_column_canon_hash(buf: &[u8], m: usize, k: usize) -> u128 {
    let total = m * k;
    let mut perm: Vec<usize> = (0..k).collect();
    let mut best = u128::MAX;
    let mut work = vec![0u8; total];
    let mut tmp = vec![0u8; total];
    let mut indices: Vec<usize> = (0..m).collect();

    loop {
        // Apply column permutation.
        for i in 0..m {
            for (j, &c) in perm.iter().enumerate() {
                work[i * k + j] = buf[i * k + c];
            }
        }

        // sort rows → relabel → sort rows → relabel.
        sort_flat_rows(&mut work, &mut tmp, &mut indices, m, k);
        relabel_flat(&mut work);
        sort_flat_rows(&mut work, &mut tmp, &mut indices, m, k);
        relabel_flat(&mut work);

        let h = hash_flat(&work);
        if h < best {
            best = h;
        }

        if !next_permutation(&mut perm) {
            break;
        }
    }

    best
}

/// Heuristic canonicalization for large k: iterate row sort, relabel,
/// column sort until convergence (up to 4 rounds).
#[allow(dead_code)]
fn heuristic_column_canon_hash(buf: &[u8], m: usize, k: usize) -> u128 {
    let total = m * k;
    let mut work = buf[..total].to_vec();
    let mut tmp = vec![0u8; total];
    let mut indices: Vec<usize> = (0..m).collect();

    for _ in 0..2 {
        sort_flat_rows(&mut work, &mut tmp, &mut indices, m, k);
        relabel_flat(&mut work);
        sort_flat_cols(&mut work, m, k);
    }
    // Final pass: sort rows and relabel.
    sort_flat_rows(&mut work, &mut tmp, &mut indices, m, k);
    relabel_flat(&mut work);

    hash_flat(&work)
}

/// No canonicalization at all — just sort and hash. For measuring baseline.
#[allow(dead_code)]
fn canonical_hash_none(sigs: &[Vec<u8>], _k: usize) -> u128 {
    let mut sorted = sigs.to_vec();
    sorted.sort();
    hash_vecs(&sorted)
}

fn hash_flat(data: &[u8]) -> u128 {
    let mut hasher = std::hash::DefaultHasher::new();
    data.hash(&mut hasher);
    let h1 = hasher.finish();

    let mut hasher2 = std::hash::DefaultHasher::new();
    h1.hash(&mut hasher2);
    data.hash(&mut hasher2);
    let h2 = hasher2.finish();

    u128::from(h1) | (u128::from(h2) << 64)
}

/// Exact canonicalization: try all k! column permutations, take lex-smallest.
#[cfg(test)]
fn canonicalize_exact(sigs: &[Vec<u8>], k: usize) -> Vec<Vec<u8>> {
    let n = sigs.len();
    let mut best: Option<Vec<u8>> = None;
    let mut perm: Vec<usize> = (0..k).collect();
    let mut buf = vec![0u8; n * k];
    let mut tmp = vec![0u8; n * k];
    let mut indices: Vec<usize> = (0..n).collect();

    loop {
        canonical_for_perm_flat(sigs, &perm, &mut buf, &mut tmp, &mut indices, n, k);
        if best.as_ref().is_none_or(|b| buf < *b) {
            best = Some(buf.clone());
        }
        if !next_permutation(&mut perm) {
            break;
        }
    }

    let flat = best.unwrap();
    flat.chunks_exact(k).map(<[u8]>::to_vec).collect()
}

/// Heuristic canonicalization for large k (> 8).
///
/// Sort rows → relabel → sort columns → re-sort → re-relabel.
/// Not perfect (misses some position isomorphisms) but polynomial.
#[cfg(test)]
fn canonicalize_heuristic(sigs: &[Vec<u8>], k: usize) -> Vec<Vec<u8>> {
    let mut matrix = sigs.to_vec();
    matrix.sort();
    matrix = relabel(&matrix);

    // Sort columns by their content vector.
    let mut col_order: Vec<usize> = (0..k).collect();
    col_order.sort_by(|&a, &b| {
        for row in &matrix {
            let cmp = row[a].cmp(&row[b]);
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });

    let permuted: Vec<Vec<u8>> = matrix
        .iter()
        .map(|row| col_order.iter().map(|&c| row[c]).collect())
        .collect();

    let mut result = permuted;
    result.sort();
    result = relabel(&result);
    result.sort();
    result
}

/// For a fixed column permutation, compute the canonical letter-relabeled form.
///
/// sort → relabel → sort → relabel converges to a fixed point where:
/// - rows are lex-sorted
/// - letter labels are in first-appearance order (in row-major traversal)
#[cfg(test)]
#[allow(dead_code)]
fn canonical_for_perm(sigs: &[Vec<u8>], perm: &[usize]) -> Vec<Vec<u8>> {
    let mut matrix: Vec<Vec<u8>> = sigs
        .iter()
        .map(|row| perm.iter().map(|&c| row[c]).collect())
        .collect();

    matrix.sort();
    matrix = relabel(&matrix);
    matrix.sort();
    matrix = relabel(&matrix);

    matrix
}

/// Flat-buffer version: writes result into `buf` (len = n*k) in row-major order.
/// `tmp` and `indices` are reusable scratch buffers to avoid allocation.
#[cfg(test)]
fn canonical_for_perm_flat(
    sigs: &[Vec<u8>],
    perm: &[usize],
    buf: &mut [u8],
    tmp: &mut [u8],
    indices: &mut [usize],
    n: usize,
    k: usize,
) {
    // Apply column permutation into buf.
    for (i, sig) in sigs.iter().enumerate() {
        let row = &mut buf[i * k..(i + 1) * k];
        for (j, &c) in perm.iter().enumerate() {
            row[j] = sig[c];
        }
    }

    // sort → relabel → sort → relabel (all in-place on buf)
    sort_flat_rows(buf, tmp, indices, n, k);
    relabel_flat(buf);
    sort_flat_rows(buf, tmp, indices, n, k);
    relabel_flat(buf);
}

fn sort_flat_rows(buf: &mut [u8], tmp: &mut [u8], indices: &mut [usize], n: usize, k: usize) {
    for (i, idx) in indices.iter_mut().enumerate() {
        *idx = i;
    }

    if k <= 8 {
        // For short rows, encode each as u64 for O(1) comparison.
        let mut keys = vec![0u64; n];
        for i in 0..n {
            let mut key = 0u64;
            for &b in &buf[i * k..(i + 1) * k] {
                key = key << 8 | u64::from(b);
            }
            keys[i] = key;
        }
        indices.sort_unstable_by_key(|&i| keys[i]);
    } else {
        // Hash-accelerated sort: O(1) hash comparison with O(k) tiebreaker.
        let mut keys = vec![0u64; n];
        for i in 0..n {
            let row = &buf[i * k..(i + 1) * k];
            let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
            for &b in row {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3); // FNV prime
            }
            keys[i] = h;
        }
        indices.sort_unstable_by(|&a, &b| {
            keys[a]
                .cmp(&keys[b])
                .then_with(|| buf[a * k..(a + 1) * k].cmp(&buf[b * k..(b + 1) * k]))
        });
    }

    tmp[..n * k].copy_from_slice(&buf[..n * k]);
    for (dst, &src) in indices.iter().enumerate() {
        buf[dst * k..(dst + 1) * k].copy_from_slice(&tmp[src * k..(src + 1) * k]);
    }
}

/// Relabel non-zero bytes by order of first appearance (row-major), in-place.
fn relabel_flat(buf: &mut [u8]) {
    let mut label_map: [u8; 256] = [0; 256];
    let mut next_id: u8 = 1;

    for &b in buf.iter() {
        if b != 0 && label_map[b as usize] == 0 {
            label_map[b as usize] = next_id;
            next_id += 1;
        }
    }

    for b in buf.iter_mut() {
        if *b != 0 {
            *b = label_map[*b as usize];
        }
    }
}

/// Relabel non-zero bytes by order of first appearance (row-major).
#[cfg(test)]
fn relabel(sigs: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut label_map: [u8; 256] = [0; 256];
    let mut next_id: u8 = 1;

    for sig in sigs {
        for &b in sig {
            if b != 0 && label_map[b as usize] == 0 {
                label_map[b as usize] = next_id;
                next_id += 1;
            }
        }
    }

    sigs.iter()
        .map(|sig| {
            sig.iter()
                .map(|&b| if b == 0 { 0 } else { label_map[b as usize] })
                .collect()
        })
        .collect()
}

fn hash_vecs(data: &[Vec<u8>]) -> u128 {
    let mut hasher = std::hash::DefaultHasher::new();
    data.hash(&mut hasher);
    let h1 = hasher.finish();

    let mut hasher2 = std::hash::DefaultHasher::new();
    h1.hash(&mut hasher2);
    data.hash(&mut hasher2);
    let h2 = hasher2.finish();

    u128::from(h1) | (u128::from(h2) << 64)
}

/// Advance to the next lexicographic permutation. Returns false if already
/// at the last permutation.
fn next_permutation(arr: &mut [usize]) -> bool {
    let n = arr.len();
    if n <= 1 {
        return false;
    }

    let mut i = n - 1;
    while i > 0 && arr[i - 1] >= arr[i] {
        i -= 1;
    }
    if i == 0 {
        return false;
    }

    let mut j = n - 1;
    while arr[j] <= arr[i - 1] {
        j -= 1;
    }
    arr.swap(i - 1, j);
    arr[i..].reverse();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_isomorphic() {
        let a = vec![b"ab".to_vec(), b"cd".to_vec()];
        let b = vec![b"ef".to_vec(), b"gh".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn position_swap() {
        let a = vec![b"ab".to_vec(), b"cd".to_vec()];
        let b = vec![b"ba".to_vec(), b"dc".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn position_and_letter_combined() {
        // {[b,a], [c,a]} ↔ {[a,b], [a,c]} via column swap + letter relabel.
        // This is the case that the old algorithm got WRONG.
        let a = vec![b"ba".to_vec(), b"ca".to_vec()];
        let b = vec![b"ab".to_vec(), b"ac".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn collapse_zero_cols_identifies_all_dead_columns() {
        // 3 rows, 4 cols, col 1 is all-zero.
        let buf = vec![
            1, 0, 2, 3,
            1, 0, 4, 5,
            6, 0, 7, 8,
        ];
        let (out, new_k) = collapse_zero_cols(&buf, 3, 4).unwrap();
        assert_eq!(new_k, 3);
        assert_eq!(out, vec![1, 2, 3, 1, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn collapse_zero_cols_no_dead_returns_none() {
        let buf = vec![1, 2, 3, 4, 5, 6];
        assert!(collapse_zero_cols(&buf, 2, 3).is_none());
    }

    #[test]
    fn collapse_zero_cols_multiple_dead() {
        // 2 rows, 4 cols, cols 0 and 2 are all-zero.
        let buf = vec![
            0, 1, 0, 2,
            0, 3, 0, 4,
        ];
        let (out, new_k) = collapse_zero_cols(&buf, 2, 4).unwrap();
        assert_eq!(new_k, 2);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }

    #[test]
    fn collapse_zero_cols_all_dead() {
        let buf = vec![0, 0, 0, 0, 0, 0];
        let (out, new_k) = collapse_zero_cols(&buf, 2, 3).unwrap();
        assert_eq!(new_k, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn canonicalize_dead_col_merges_with_k_minus_one() {
        // Two word sets with the same effective structure but in one, the dead
        // column is at position 2 (k=4) and in the other it's a genuine k=3
        // set. These should hash to the same canonical form since the dead
        // column contributes no game-relevant information.
        let with_dead_col = vec![
            vec![1, 2, 0, 3],
            vec![1, 2, 0, 4],
            vec![5, 2, 0, 3],
        ];
        let no_dead_col = vec![
            vec![1, 2, 3],
            vec![1, 2, 4],
            vec![5, 2, 3],
        ];
        assert_eq!(canonical_hash(&with_dead_col), canonical_hash(&no_dead_col));
    }

    #[test]
    fn canonicalize_dead_col_position_invariance() {
        // Same 3-column structure, but in one the dead col is at position 0,
        // in the other at position 2. Both should collapse to the same hash.
        let dead_at_0 = vec![
            vec![0, 1, 2, 3],
            vec![0, 1, 2, 4],
        ];
        let dead_at_2 = vec![
            vec![1, 2, 0, 3],
            vec![1, 2, 0, 4],
        ];
        assert_eq!(canonical_hash(&dead_at_0), canonical_hash(&dead_at_2));
    }

    #[test]
    fn non_isomorphic() {
        // {ab, ac} has shared first letter; {de, fg} does not.
        let a = vec![b"ab".to_vec(), b"ac".to_vec()];
        let b = vec![b"de".to_vec(), b"fg".to_vec()];
        assert_ne!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn with_zeros() {
        let a = vec![vec![0, 1, 2], vec![0, 3, 4]];
        let b = vec![vec![1, 0, 2], vec![3, 0, 4]];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn three_col_rotation() {
        let a = vec![b"abc".to_vec(), b"def".to_vec()];
        let b = vec![b"bca".to_vec(), b"efd".to_vec()];
        let c = vec![b"cab".to_vec(), b"fde".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
        assert_eq!(canonicalize(&a), canonicalize(&c));
    }

    #[test]
    fn hash_isomorphic() {
        let a = vec![b"ab".to_vec(), b"cd".to_vec()];
        let b = vec![b"ba".to_vec(), b"dc".to_vec()];
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn hash_position_and_letter_exact() {
        // canonical_hash uses heuristic (no position isomorphism guaranteed).
        // This test verifies the exact canonicalize function handles it.
        let a = vec![b"ba".to_vec(), b"ca".to_vec()];
        let b = vec![b"ab".to_vec(), b"ac".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn empty_sigs() {
        assert_eq!(canonicalize(&[]), Vec::<Vec<u8>>::new());
        assert_eq!(canonical_hash(&[]), 0);
    }

    #[test]
    fn single_sig() {
        let a = vec![b"abc".to_vec()];
        let b = vec![b"xyz".to_vec()];
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn next_perm_exhaustive() {
        let mut p = vec![0, 1, 2];
        let mut count = 1;
        while next_permutation(&mut p) {
            count += 1;
        }
        assert_eq!(count, 6); // 3! = 6
    }
}
