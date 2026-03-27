use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use dashmap::DashMap;
use saferlmdb as lmdb;

/// On-disk transposition table backed by LMDB.
///
/// Stores `(u128 canonical_key → u32 packed_value)` entries in a memory-mapped
/// B+tree. The OS pages in only the accessed entries, so multi-GB databases
/// don't require multi-GB RAM.
pub struct DiskCache {
    // Arc lets Database hold a 'static reference to the Environment.
    env: Arc<lmdb::Environment>,
    db: lmdb::Database<'static>,
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
            builder
                .open(path_str, &lmdb::open::Flags::empty(), 0o644)
                .with_context(|| format!("opening LMDB env at {}", db_dir.display()))?
        };
        let env = Arc::new(env);

        let db = lmdb::Database::open(
            env.clone(),
            None,
            &lmdb::DatabaseOptions::defaults(),
        )
        .context("opening LMDB database")?;

        Ok(Self { env, db })
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

    /// Write all entries (EXACT, LOWER, UPPER) from an in-memory cache to LMDB.
    ///
    /// When an entry already exists on disk, the new value wins if it is:
    /// - EXACT (always overwrites), or
    /// - a tighter bound of the same type.
    ///
    /// Returns the number of entries written.
    ///
    /// # Errors
    ///
    /// Returns an error if a write transaction cannot be created or committed.
    pub fn save(&self, cache: &DashMap<u128, u32>) -> Result<usize> {
        const BATCH_SIZE: usize = 100_000;
        let mut count = 0usize;
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
                    access
                        .put(
                            &self.db,
                            &key_bytes[..],
                            &val_bytes[..],
                            &lmdb::put::Flags::empty(),
                        )
                        .context("putting entry")?;
                }
            }
            txn.commit().context("committing batch")?;
            count += chunk.len();
        }
        Ok(count)
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
