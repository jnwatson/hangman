use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use dashmap::DashMap;
use saferlmdb as lmdb;

use super::memoized::{BOUND_EXACT, BOUND_LOWER, BOUND_UPPER, cache_unpack};

/// Cumulative outcomes of all `DiskCache::save` calls on a single `DiskCache`.
///
/// The `rejected_by_exact` counter is the diagnostic of interest for parallel
/// precompute: each increment means a worker computed a bound for a position
/// that another worker had already EXACT-resolved and flushed. That worker's
/// compute for that subtree was redundant — counts that climb fast indicate
/// the parallel layout is wasting significant CPU on overlapping solves.
#[derive(Debug, Default, Clone, Copy)]
pub struct SaveStats {
    /// New entries written (no prior disk entry for this key).
    pub inserted: u64,
    /// Existing entries overwritten because the new value dominated.
    pub overwritten: u64,
    /// Writes rejected because the existing entry was EXACT.
    pub rejected_by_exact: u64,
    /// Writes rejected because the existing bound was already tighter or of
    /// a different bound type (can't overwrite LOWER with UPPER, etc.).
    pub rejected_other: u64,
}

impl SaveStats {
    #[must_use]
    pub fn total_written(self) -> u64 {
        self.inserted + self.overwritten
    }

    #[must_use]
    pub fn total_considered(self) -> u64 {
        self.inserted + self.overwritten + self.rejected_by_exact + self.rejected_other
    }
}

/// Decide whether a new packed entry should overwrite an existing packed entry
/// in the disk cache. Mirrors the in-memory dominance rules in
/// `MemoizedSolverInner::cache_store`:
/// - EXACT always overwrites non-EXACT; EXACT is never overwritten by a bound.
/// - A tighter bound of the *same* type wins (LOWER with larger value,
///   UPPER with smaller value).
/// - A bound of one type never overwrites a bound of the other type.
///
/// This prevents a parallel worker's bound entry from clobbering an EXACT
/// that was written by another worker earlier in the same run.
#[inline]
fn new_dominates(new_packed: u32, old_packed: u32) -> bool {
    if new_packed == old_packed {
        return false;
    }
    let (new_val, _, new_bound) = cache_unpack(new_packed);
    let (old_val, _, old_bound) = cache_unpack(old_packed);
    if old_bound == BOUND_EXACT {
        return false;
    }
    if new_bound == BOUND_EXACT {
        return true;
    }
    if new_bound == BOUND_LOWER && old_bound == BOUND_LOWER && new_val > old_val {
        return true;
    }
    if new_bound == BOUND_UPPER && old_bound == BOUND_UPPER && new_val < old_val {
        return true;
    }
    false
}

/// On-disk transposition table backed by LMDB.
///
/// Stores `(u128 canonical_key → u32 packed_value)` entries in a memory-mapped
/// B+tree. The OS pages in only the accessed entries, so multi-GB databases
/// don't require multi-GB RAM.
pub struct DiskCache {
    // Arc lets Database hold a 'static reference to the Environment.
    env: Arc<lmdb::Environment>,
    db: lmdb::Database<'static>,
    /// Cumulative `save()` stats across all calls, updated atomically so
    /// parallel workers can contribute without locking.
    stat_inserted: AtomicU64,
    stat_overwritten: AtomicU64,
    stat_rejected_exact: AtomicU64,
    stat_rejected_other: AtomicU64,
}

impl DiskCache {
    /// Open or create a disk cache for the given word length and dictionary.
    ///
    /// The database directory is `{dir}/tt_len{word_length}_{hash:016x}/`.
    /// If the directory doesn't exist, it is created.
    ///
    /// `map_size` is the maximum database size in bytes. LMDB requires this
    /// upfront but doesn't allocate the space — it's a virtual address space
    /// limit. Use a generous value (e.g., 16 GB).
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the LMDB
    /// environment cannot be opened.
    pub fn open(
        dir: &Path,
        word_length: usize,
        words: &[Vec<u8>],
        map_size: usize,
    ) -> Result<Self> {
        let db_dir = Self::db_dir(dir, word_length, words);
        std::fs::create_dir_all(&db_dir)
            .with_context(|| format!("creating cache dir {}", db_dir.display()))?;

        let env = unsafe {
            let mut builder = lmdb::EnvBuilder::new()
                .context("creating LMDB env builder")?;
            builder.set_mapsize(map_size)?;
            let path_str = db_dir
                .to_str()
                .with_context(|| format!("non-UTF8 path: {}", db_dir.display()))?;
            // NORDAHEAD: disable OS readahead on the mmap. Helps random-access
            // performance when the DB is larger than RAM (common for us) and
            // especially when multiple concurrent LMDB envs compete for page
            // cache — readahead wastes RAM on speculative pages that get
            // evicted before being used.
            builder
                .open(path_str, &lmdb::open::Flags::NORDAHEAD, 0o644)
                .with_context(|| format!("opening LMDB env at {}", db_dir.display()))?
        };
        let env = Arc::new(env);

        let db = lmdb::Database::open(
            env.clone(),
            None,
            &lmdb::DatabaseOptions::defaults(),
        )
        .context("opening LMDB database")?;

        Ok(Self {
            env,
            db,
            stat_inserted: AtomicU64::new(0),
            stat_overwritten: AtomicU64::new(0),
            stat_rejected_exact: AtomicU64::new(0),
            stat_rejected_other: AtomicU64::new(0),
        })
    }

    /// Snapshot of cumulative `save()` stats. Safe to call concurrently.
    #[must_use]
    pub fn save_stats(&self) -> SaveStats {
        SaveStats {
            inserted: self.stat_inserted.load(Ordering::Relaxed),
            overwritten: self.stat_overwritten.load(Ordering::Relaxed),
            rejected_by_exact: self.stat_rejected_exact.load(Ordering::Relaxed),
            rejected_other: self.stat_rejected_other.load(Ordering::Relaxed),
        }
    }

    /// Open an existing disk cache, returning `None` if the directory doesn't
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory exists but the LMDB environment
    /// cannot be opened.
    pub fn open_if_exists(
        dir: &Path,
        word_length: usize,
        words: &[Vec<u8>],
        map_size: usize,
    ) -> Result<Option<Self>> {
        let db_dir = Self::db_dir(dir, word_length, words);
        if !db_dir.exists() {
            return Ok(None);
        }
        Self::open(dir, word_length, words, map_size).map(Some)
    }

    /// Write all entries (EXACT, LOWER, UPPER) from an in-memory cache to LMDB
    /// with dominance-aware overwrite semantics.
    ///
    /// When an entry already exists on disk, the new value wins if it is:
    /// - EXACT and the old isn't EXACT, or
    /// - a tighter bound of the *same* type (tighter LOWER has larger value;
    ///   tighter UPPER has smaller value).
    ///
    /// Otherwise the old entry is preserved. This is critical in parallel
    /// precompute: without dominance, two workers writing the same canonical
    /// key can clobber an EXACT entry with a bound, hiding optimal moves from
    /// the serving path (which filters to EXACT-only lookups).
    ///
    /// Returns the number of entries actually written (new or dominating).
    ///
    /// # Errors
    ///
    /// Returns an error if a write transaction cannot be created or committed.
    pub fn save(&self, cache: &DashMap<u128, u32>) -> Result<usize> {
        const BATCH_SIZE: usize = 100_000;
        let mut inserted = 0u64;
        let mut overwritten = 0u64;
        let mut rejected_exact = 0u64;
        let mut rejected_other = 0u64;
        let entries: Vec<(u128, u32)> = cache
            .iter()
            .map(|entry| (*entry.key(), *entry.value()))
            .collect();

        for chunk in entries.chunks(BATCH_SIZE) {
            let txn = lmdb::WriteTransaction::new(&*self.env)
                .context("starting write transaction")?;
            {
                let mut access = txn.access();
                for &(key, value) in chunk {
                    let key_bytes = key.to_le_bytes();
                    let val_bytes = value.to_le_bytes();
                    // Read existing entry in the same transaction. If it
                    // exists and dominates (or equals) the new value, skip
                    // the write.
                    let existing = access
                        .get::<[u8], [u8]>(&self.db, &key_bytes[..])
                        .ok()
                        .and_then(|b| {
                            if b.len() >= 4 {
                                Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            } else {
                                None
                            }
                        });
                    match existing {
                        None => {
                            access
                                .put(
                                    &self.db,
                                    &key_bytes[..],
                                    &val_bytes[..],
                                    &lmdb::put::Flags::empty(),
                                )
                                .context("putting entry")?;
                            inserted += 1;
                        }
                        Some(old) => {
                            if new_dominates(value, old) {
                                access
                                    .put(
                                        &self.db,
                                        &key_bytes[..],
                                        &val_bytes[..],
                                        &lmdb::put::Flags::empty(),
                                    )
                                    .context("putting entry")?;
                                overwritten += 1;
                            } else {
                                let (_, _, old_bound) = cache_unpack(old);
                                if old_bound == BOUND_EXACT {
                                    rejected_exact += 1;
                                } else {
                                    rejected_other += 1;
                                }
                            }
                        }
                    }
                }
            }
            txn.commit().context("committing batch")?;
        }
        self.stat_inserted.fetch_add(inserted, Ordering::Relaxed);
        self.stat_overwritten.fetch_add(overwritten, Ordering::Relaxed);
        self.stat_rejected_exact
            .fetch_add(rejected_exact, Ordering::Relaxed);
        self.stat_rejected_other
            .fetch_add(rejected_other, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)]
        Ok((inserted + overwritten) as usize)
    }

    /// Look up a canonical key. Returns the packed u32 value.
    #[must_use]
    pub fn get(&self, key: u128) -> Option<u32> {
        let key_bytes = key.to_le_bytes();
        let txn = lmdb::ReadTransaction::new(&*self.env).ok()?;
        let access = txn.access();
        let val_bytes: &[u8] = access.get(&self.db, &key_bytes[..]).ok()?;
        if val_bytes.len() < 4 {
            return None;
        }
        Some(u32::from_le_bytes([
            val_bytes[0],
            val_bytes[1],
            val_bytes[2],
            val_bytes[3],
        ]))
    }

    /// Count the number of entries in the database.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.env.stat().map_or(0, |s| s.entries)
    }

    fn db_dir(dir: &Path, word_length: usize, words: &[Vec<u8>]) -> PathBuf {
        let hash = hash_word_set(words);
        dir.join(format!("tt_len{word_length}_{hash:016x}"))
    }
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
    use tempfile::TempDir;

    fn test_words() -> Vec<Vec<u8>> {
        vec![
            b"cat".to_vec(),
            b"bat".to_vec(),
            b"hat".to_vec(),
        ]
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let words = test_words();

        let cache: DashMap<u128, u32> = DashMap::new();
        // EXACT entries (bound bits 10-11 = 0).
        cache.insert(42, 0x0000_0005); // value=5
        cache.insert(99, 0x0000_0003); // value=3
        // Non-EXACT entry (bound=LOWER, bit 10 set).
        cache.insert(200, 0x0000_0005 | (1 << 10));

        let dc = DiskCache::open(dir.path(), 3, &words, 10 * 1024 * 1024).unwrap();
        let saved = dc.save(&cache).unwrap();
        assert_eq!(saved, 3); // All entries including bounds

        assert_eq!(dc.get(42), Some(0x0000_0005));
        assert_eq!(dc.get(99), Some(0x0000_0003));
        assert_eq!(dc.get(200), Some(0x0000_0005 | (1 << 10))); // LOWER bound
        assert_eq!(dc.get(12345), None);
        assert_eq!(dc.entry_count(), 3);
    }

    #[test]
    fn open_if_exists_returns_none() {
        let dir = TempDir::new().unwrap();
        let words = test_words();
        let result = DiskCache::open_if_exists(dir.path(), 3, &words, 10 * 1024 * 1024).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn save_preserves_existing_exact_against_later_bound() {
        use super::super::memoized::cache_pack;
        let dir = TempDir::new().unwrap();
        let words = test_words();
        let dc = DiskCache::open(dir.path(), 3, &words, 10 * 1024 * 1024).unwrap();

        // First write: EXACT value=5 for key 42.
        let first: DashMap<u128, u32> = DashMap::new();
        first.insert(42, cache_pack(5, b'a', BOUND_EXACT));
        assert_eq!(dc.save(&first).unwrap(), 1);

        // Second write: UPPER bound value=7 for same key — simulates a
        // parallel worker computing a looser bound for a position an earlier
        // worker already EXACT-resolved. Must NOT overwrite.
        let second: DashMap<u128, u32> = DashMap::new();
        second.insert(42, cache_pack(7, b'a', BOUND_UPPER));
        let written = dc.save(&second).unwrap();
        assert_eq!(written, 0, "EXACT must not be overwritten by a bound");

        let got = dc.get(42).unwrap();
        let (val, _, bound) = cache_unpack(got);
        assert_eq!(bound, BOUND_EXACT);
        assert_eq!(val, 5);
    }

    #[test]
    fn save_overwrites_bound_with_tighter_same_type() {
        use super::super::memoized::cache_pack;
        let dir = TempDir::new().unwrap();
        let words = test_words();
        let dc = DiskCache::open(dir.path(), 3, &words, 10 * 1024 * 1024).unwrap();

        // First: UPPER value=10 (value ≤ 10).
        let first: DashMap<u128, u32> = DashMap::new();
        first.insert(42, cache_pack(10, b'a', BOUND_UPPER));
        dc.save(&first).unwrap();

        // Second: UPPER value=7 (tighter upper bound; dominates).
        let second: DashMap<u128, u32> = DashMap::new();
        second.insert(42, cache_pack(7, b'a', BOUND_UPPER));
        let written = dc.save(&second).unwrap();
        assert_eq!(written, 1);
        let (val, _, bound) = cache_unpack(dc.get(42).unwrap());
        assert_eq!((val, bound), (7, BOUND_UPPER));

        // Third: UPPER value=12 (looser; should not overwrite).
        let third: DashMap<u128, u32> = DashMap::new();
        third.insert(42, cache_pack(12, b'a', BOUND_UPPER));
        let written = dc.save(&third).unwrap();
        assert_eq!(written, 0);
        let (val, _, _) = cache_unpack(dc.get(42).unwrap());
        assert_eq!(val, 7, "looser upper bound must not overwrite tighter");

        // Fourth: LOWER value=3 — different bound type, should not overwrite.
        let fourth: DashMap<u128, u32> = DashMap::new();
        fourth.insert(42, cache_pack(3, b'a', BOUND_LOWER));
        let written = dc.save(&fourth).unwrap();
        assert_eq!(written, 0);

        // Fifth: EXACT value=5 — always wins over bound.
        let fifth: DashMap<u128, u32> = DashMap::new();
        fifth.insert(42, cache_pack(5, b'a', BOUND_EXACT));
        let written = dc.save(&fifth).unwrap();
        assert_eq!(written, 1);
        let (val, _, bound) = cache_unpack(dc.get(42).unwrap());
        assert_eq!((val, bound), (5, BOUND_EXACT));
    }

    #[test]
    fn different_words_different_db() {
        let dir = TempDir::new().unwrap();
        let words1 = test_words();
        let words2 = vec![b"dog".to_vec(), b"log".to_vec()];

        let cache: DashMap<u128, u32> = DashMap::new();
        cache.insert(42, 5);

        let dc1 = DiskCache::open(dir.path(), 3, &words1, 10 * 1024 * 1024).unwrap();
        dc1.save(&cache).unwrap();

        let dc2 = DiskCache::open(dir.path(), 3, &words2, 10 * 1024 * 1024).unwrap();
        assert_eq!(dc2.get(42), None);
    }
}
