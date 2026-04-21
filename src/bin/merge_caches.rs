#![deny(clippy::all, clippy::pedantic)]
#![allow(
    clippy::collapsible_if,
    clippy::doc_markdown,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cloned_ref_to_slice_refs,
)]

//! Merge multiple LMDB cache directories into a single destination.
//!
//! Use case: when sharding precompute across N machines, each machine produces
//! its own LMDB cache. After the run, rsync each shard's cache directory back
//! and merge them into the production cache with this binary.
//!
//! The merge iterates every entry in each source DB and writes it to the
//! destination. LMDB's default `put` overwrites on collision; for
//! hangman TT entries, same-key-same-value holds for EXACT entries (which
//! are deterministic), so the common case is a safe no-op on duplicates.
//!
//! `--prefer-exact` adds a safeguard: when the destination already contains
//! an EXACT entry for a key and the incoming value is a bound (LOWER/UPPER),
//! keep the existing EXACT. Without this flag, last-write-wins.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use saferlmdb as lmdb;

// TT entry packed format (must match src/solver/memoized.rs):
//   bits 0-4: value
//   bits 5-9: best letter index
//   bits 10-11: bound type (0 = EXACT, 1 = LOWER, 2 = UPPER)
const BOUND_SHIFT: u32 = 10;
const BOUND_EXACT: u32 = 0;

fn bound_of(packed: u32) -> u32 {
    (packed >> BOUND_SHIFT) & 0b11
}

fn is_exact(packed: u32) -> bool {
    bound_of(packed) == BOUND_EXACT
}

/// Statistics from a merge run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MergeStats {
    pub sources_scanned: usize,
    pub entries_read: usize,
    pub entries_written: usize,
    /// Count of times an EXACT destination entry was preserved over an
    /// incoming bound (only when `prefer_exact` is true).
    pub exact_preserved: usize,
}

/// Open an LMDB environment rooted at `path`.
///
/// Creates the directory if missing (read/write mode). Read-only mode
/// requires the directory to already exist.
fn open_env(path: &Path, map_size: usize, readonly: bool) -> Result<Arc<lmdb::Environment>> {
    if !readonly {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating dir {}", path.display()))?;
    } else if !path.exists() {
        anyhow::bail!("source dir does not exist: {}", path.display());
    }

    let env = unsafe {
        let mut builder = lmdb::EnvBuilder::new().context("EnvBuilder")?;
        builder.set_mapsize(map_size).context("set_mapsize")?;
        let path_str = path
            .to_str()
            .with_context(|| format!("non-UTF8 path: {}", path.display()))?;
        let flags = if readonly {
            lmdb::open::Flags::RDONLY
        } else {
            lmdb::open::Flags::empty()
        };
        builder
            .open(path_str, &flags, 0o644)
            .with_context(|| format!("opening LMDB env at {}", path.display()))?
    };
    Ok(Arc::new(env))
}

fn open_db(env: &Arc<lmdb::Environment>) -> Result<lmdb::Database<'static>> {
    lmdb::Database::open(env.clone(), None, &lmdb::DatabaseOptions::defaults())
        .context("opening LMDB database")
}

/// Merge entries from each source LMDB into the destination LMDB.
///
/// # Arguments
/// - `sources`: paths to source LMDB directories (opened read-only)
/// - `dest`: path to destination LMDB directory (created if missing)
/// - `prefer_exact`: when true, keep existing EXACT destination entries
///   even if an incoming value exists (unless that value is also EXACT)
/// - `map_size`: LMDB map size in bytes for both source and dest envs
/// - `batch_size`: number of (key, value) pairs to write per transaction
///
/// # Errors
/// - source path doesn't exist
/// - LMDB open/read/write failure
/// - non-UTF8 paths
/// - malformed entries (wrong key or value byte length) are skipped
pub fn merge_caches(
    sources: &[PathBuf],
    dest: &Path,
    prefer_exact: bool,
    map_size: usize,
    batch_size: usize,
) -> Result<MergeStats> {
    let batch_size = batch_size.max(1);
    let mut stats = MergeStats::default();

    let dest_env = open_env(dest, map_size, false)?;
    let dest_db = open_db(&dest_env)?;

    for src_path in sources {
        stats.sources_scanned += 1;
        let src_env = open_env(src_path, map_size, true)?;
        let src_db = open_db(&src_env)?;

        let rtxn = lmdb::ReadTransaction::new(&*src_env)
            .context("starting read txn on source")?;
        let racc = rtxn.access();
        let mut cursor = rtxn.cursor(&src_db).context("creating source cursor")?;

        let mut batch: Vec<([u8; 16], [u8; 4])> = Vec::with_capacity(batch_size);

        let mut pair: Option<(&[u8], &[u8])> = cursor.first::<[u8], [u8]>(&racc).ok();
        while let Some((k, v)) = pair {
            if k.len() == 16 && v.len() >= 4 {
                let mut kb = [0u8; 16];
                kb.copy_from_slice(k);
                let mut vb = [0u8; 4];
                vb.copy_from_slice(&v[..4]);
                batch.push((kb, vb));
                stats.entries_read += 1;

                if batch.len() >= batch_size {
                    flush_batch(&dest_env, &dest_db, &batch, prefer_exact, &mut stats)?;
                    batch.clear();
                }
            }
            pair = cursor.next::<[u8], [u8]>(&racc).ok();
        }
        if !batch.is_empty() {
            flush_batch(&dest_env, &dest_db, &batch, prefer_exact, &mut stats)?;
        }
    }
    Ok(stats)
}

fn flush_batch(
    env: &Arc<lmdb::Environment>,
    db: &lmdb::Database,
    batch: &[([u8; 16], [u8; 4])],
    prefer_exact: bool,
    stats: &mut MergeStats,
) -> Result<()> {
    let wtxn = lmdb::WriteTransaction::new(&**env).context("starting write txn")?;
    {
        let mut access = wtxn.access();
        for (kb, vb) in batch {
            let new_val = u32::from_le_bytes(*vb);
            if prefer_exact && !is_exact(new_val) {
                if let Ok(existing_bytes) = access.get::<[u8], [u8]>(db, &kb[..]) {
                    if existing_bytes.len() >= 4 {
                        let existing = u32::from_le_bytes([
                            existing_bytes[0],
                            existing_bytes[1],
                            existing_bytes[2],
                            existing_bytes[3],
                        ]);
                        if is_exact(existing) {
                            stats.exact_preserved += 1;
                            continue;
                        }
                    }
                }
            }
            access
                .put(db, &kb[..], &vb[..], &lmdb::put::Flags::empty())
                .context("put")?;
            stats.entries_written += 1;
        }
    }
    wtxn.commit().context("committing batch")?;
    Ok(())
}

#[derive(Parser)]
#[command(name = "merge-caches", about = "Merge multiple LMDB cache dirs into one")]
struct Cli {
    /// Source LMDB directories (each is a tt_len{K}_{hash:016x}/ folder)
    #[arg(required = true, num_args = 1..)]
    sources: Vec<PathBuf>,

    /// Destination LMDB directory (created if missing)
    #[arg(long, short)]
    dest: PathBuf,

    /// Max LMDB map size in GB
    #[arg(long, default_value = "256")]
    map_size_gb: usize,

    /// When the destination holds an EXACT entry, skip incoming non-EXACT bounds
    #[arg(long)]
    prefer_exact: bool,

    /// Write-transaction batch size
    #[arg(long, default_value = "50000")]
    batch_size: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let map_size = cli.map_size_gb.saturating_mul(1024 * 1024 * 1024);

    println!("merge-caches:");
    for src in &cli.sources {
        println!("  source: {}", src.display());
    }
    println!("  dest:   {}", cli.dest.display());
    println!("  map_size_gb={} prefer_exact={} batch_size={}",
             cli.map_size_gb, cli.prefer_exact, cli.batch_size);

    let stats = merge_caches(
        &cli.sources,
        &cli.dest,
        cli.prefer_exact,
        map_size,
        cli.batch_size,
    )?;

    println!("\ndone:");
    println!("  sources scanned:  {}", stats.sources_scanned);
    println!("  entries read:     {}", stats.entries_read);
    println!("  entries written:  {}", stats.entries_written);
    if cli.prefer_exact {
        println!("  exact preserved:  {}", stats.exact_preserved);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MAP_SIZE: usize = 10 * 1024 * 1024;

    // Bound types for testing (must match memoized.rs encoding).
    const EXACT: u32 = 0;
    const LOWER: u32 = 1;
    const UPPER: u32 = 2;

    /// Construct a packed u32 value for a TT entry.
    fn packed(value: u32, letter: u32, bound: u32) -> u32 {
        (bound << BOUND_SHIFT) | ((letter & 0x1F) << 5) | (value & 0x1F)
    }

    /// Write raw (u128 key → u32 value) entries into an LMDB at `path`.
    fn make_db(path: &Path, entries: &[(u128, u32)]) {
        let env = open_env(path, MAP_SIZE, false).unwrap();
        let db = open_db(&env).unwrap();
        let wtxn = lmdb::WriteTransaction::new(&*env).unwrap();
        {
            let mut a = wtxn.access();
            for (k, v) in entries {
                let kb = k.to_le_bytes();
                let vb = v.to_le_bytes();
                a.put(&db, &kb[..], &vb[..], &lmdb::put::Flags::empty())
                    .unwrap();
            }
        }
        wtxn.commit().unwrap();
    }

    /// Read all (key, value) pairs from an LMDB at `path`, sorted by key.
    fn read_all(path: &Path) -> Vec<(u128, u32)> {
        let env = open_env(path, MAP_SIZE, true).unwrap();
        let db = open_db(&env).unwrap();
        let rtxn = lmdb::ReadTransaction::new(&*env).unwrap();
        let acc = rtxn.access();
        let mut cursor = rtxn.cursor(&db).unwrap();
        let mut out = Vec::new();

        let mut pair: Option<(&[u8], &[u8])> = cursor.first::<[u8], [u8]>(&acc).ok();
        while let Some((k, v)) = pair {
            let key = u128::from_le_bytes(k.try_into().unwrap());
            let val = u32::from_le_bytes([v[0], v[1], v[2], v[3]]);
            out.push((key, val));
            pair = cursor.next::<[u8], [u8]>(&acc).ok();
        }
        out.sort_by_key(|&(k, _)| k);
        out
    }

    #[test]
    fn bound_of_recognizes_encoding() {
        assert!(is_exact(packed(5, 0, EXACT)));
        assert!(!is_exact(packed(5, 0, LOWER)));
        assert!(!is_exact(packed(5, 0, UPPER)));
        assert_eq!(bound_of(packed(5, 0, LOWER)), LOWER);
        assert_eq!(bound_of(packed(5, 0, UPPER)), UPPER);
    }

    #[test]
    fn merge_single_source_copies_all_entries() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        let seed = [(1u128, packed(5, 0, EXACT)), (2u128, packed(3, 1, EXACT))];
        make_db(&src, &seed);

        let stats = merge_caches(&[src], &dest, false, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.sources_scanned, 1);
        assert_eq!(stats.entries_read, 2);
        assert_eq!(stats.entries_written, 2);
        assert_eq!(stats.exact_preserved, 0);

        let out = read_all(&dest);
        assert_eq!(out, seed.to_vec());
    }

    #[test]
    fn merge_multiple_sources_takes_union() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let s2 = tmp.path().join("s2");
        let dest = tmp.path().join("dest");
        make_db(&s1, &[(1, packed(5, 0, EXACT)), (2, packed(3, 0, EXACT))]);
        make_db(&s2, &[(3, packed(7, 2, EXACT)), (4, packed(0, 1, EXACT))]);

        let stats = merge_caches(&[s1, s2], &dest, false, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.sources_scanned, 2);
        assert_eq!(stats.entries_read, 4);
        assert_eq!(stats.entries_written, 4);

        let out = read_all(&dest);
        assert_eq!(out.len(), 4);
        assert_eq!(out.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn duplicate_key_last_write_wins_without_prefer_exact() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let s2 = tmp.path().join("s2");
        let dest = tmp.path().join("dest");
        make_db(&s1, &[(1, packed(5, 0, EXACT))]);
        make_db(&s2, &[(1, packed(7, 3, EXACT))]);

        merge_caches(&[s1, s2], &dest, false, MAP_SIZE, 100).unwrap();
        let out = read_all(&dest);
        assert_eq!(out, vec![(1, packed(7, 3, EXACT))]);
    }

    #[test]
    fn prefer_exact_keeps_dest_exact_over_incoming_bound() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let s2 = tmp.path().join("s2");
        let dest = tmp.path().join("dest");
        // s1 writes an EXACT entry.
        make_db(&s1, &[(1, packed(5, 0, EXACT))]);
        // s2 has a LOWER bound for the same key. Should NOT overwrite.
        make_db(&s2, &[(1, packed(3, 0, LOWER))]);

        let stats = merge_caches(&[s1, s2], &dest, true, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.exact_preserved, 1);
        // One write (the EXACT from s1), one preserved (the LOWER from s2 skipped).
        assert_eq!(stats.entries_written, 1);

        let out = read_all(&dest);
        assert_eq!(out, vec![(1, packed(5, 0, EXACT))]);
    }

    #[test]
    fn prefer_exact_allows_incoming_exact_to_overwrite_bound() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let s2 = tmp.path().join("s2");
        let dest = tmp.path().join("dest");
        make_db(&s1, &[(1, packed(3, 0, LOWER))]);
        make_db(&s2, &[(1, packed(7, 0, EXACT))]);

        let stats = merge_caches(&[s1, s2], &dest, true, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.exact_preserved, 0);

        let out = read_all(&dest);
        assert_eq!(out, vec![(1, packed(7, 0, EXACT))]);
    }

    #[test]
    fn prefer_exact_bound_vs_bound_overwrites_normally() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let s2 = tmp.path().join("s2");
        let dest = tmp.path().join("dest");
        make_db(&s1, &[(1, packed(3, 0, LOWER))]);
        make_db(&s2, &[(1, packed(4, 0, UPPER))]);

        let stats = merge_caches(&[s1, s2], &dest, true, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.exact_preserved, 0);

        let out = read_all(&dest);
        assert_eq!(out, vec![(1, packed(4, 0, UPPER))]);
    }

    #[test]
    fn empty_source_produces_no_writes() {
        let tmp = TempDir::new().unwrap();
        let s1 = tmp.path().join("s1");
        let dest = tmp.path().join("dest");
        make_db(&s1, &[]);

        let stats = merge_caches(&[s1], &dest, false, MAP_SIZE, 100).unwrap();
        assert_eq!(stats.entries_read, 0);
        assert_eq!(stats.entries_written, 0);
        assert!(read_all(&dest).is_empty());
    }

    #[test]
    fn merge_into_populated_dest_merges_rather_than_replaces() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        // Pre-populate dest.
        make_db(&dest, &[(10, packed(2, 0, EXACT)), (20, packed(4, 0, EXACT))]);
        make_db(&src, &[(20, packed(9, 0, EXACT)), (30, packed(1, 0, EXACT))]);

        merge_caches(&[src], &dest, false, MAP_SIZE, 100).unwrap();
        let out = read_all(&dest);
        let keys: Vec<u128> = out.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![10, 20, 30]);
        // Key 20 overwritten by the source value.
        let v20 = out.iter().find(|(k, _)| *k == 20).unwrap().1;
        assert_eq!(v20, packed(9, 0, EXACT));
    }

    #[test]
    fn missing_source_fails_cleanly() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does_not_exist");
        let dest = tmp.path().join("dest");
        let result = merge_caches(&[nonexistent], &dest, false, MAP_SIZE, 100);
        assert!(result.is_err());
    }

    #[test]
    fn batched_writes_produce_same_result_as_single_batch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest_small = tmp.path().join("dest_small");
        let dest_big = tmp.path().join("dest_big");
        let entries: Vec<(u128, u32)> = (0..250)
            .map(|i| (i as u128, packed(i as u32 % 26, 0, EXACT)))
            .collect();
        make_db(&src, &entries);

        merge_caches(&[src.clone()], &dest_small, false, MAP_SIZE, 17).unwrap();
        merge_caches(&[src], &dest_big, false, MAP_SIZE, 100_000).unwrap();

        assert_eq!(read_all(&dest_small), read_all(&dest_big));
        assert_eq!(read_all(&dest_small).len(), 250);
    }
}
