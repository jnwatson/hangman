//! Diagnostic: check cache hit rate for first-guess partitions.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::serving::{
    canonical_hash_for_words, decode_tt_entry, fold_required_letters, pos_mask,
};

#[derive(Parser)]
#[command(name = "cache-diag")]
struct Cli {
    #[arg(short, long)]
    dict: PathBuf,

    #[arg(short, long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    #[arg(short, long)]
    length: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    let words = dict.words_of_length(cli.length).to_vec();
    println!("k={}: {} words", cli.length, words.len());

    let dc = DiskCache::open_if_exists(&cli.cache_dir, cli.length, &words, 16 * 1024 * 1024 * 1024)?
        .expect("no disk cache found");
    println!("disk cache: {} entries", dc.entry_count());

    let all_indices: Vec<usize> = (0..words.len()).collect();
    let root_folded = fold_required_letters(&words, &all_indices, 0);
    let root_hash = canonical_hash_for_words(&words, &all_indices, root_folded);
    let root_entry = dc.get(root_hash).and_then(decode_tt_entry);
    println!(
        "root: hash={:#034x} value={:?}\n",
        root_hash,
        root_entry.map(|e| e.value)
    );

    let mut total_misses = 0u32;
    let mut total_lookups = 0u32;

    for li in 0..26u8 {
        let letter = b'a' + li;
        let new_masked = letter_bit(letter);

        let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &all_indices {
            let mask = pos_mask(&words[idx], letter);
            partitions.entry(mask).or_default().push(idx);
        }

        let mut misses = 0u32;
        let mut hits = 0u32;

        for (_, indices) in &partitions {
            if indices.len() <= 1 {
                continue;
            }
            total_lookups += 1;
            let folded = fold_required_letters(&words, indices, new_masked);
            let hash = canonical_hash_for_words(&words, indices, folded);
            if dc.get(hash).and_then(decode_tt_entry).is_some() {
                hits += 1;
            } else {
                misses += 1;
                total_misses += 1;
            }
        }

        let c = letter as char;
        if misses > 0 {
            println!("  '{}': {} hits, {} MISSES", c, hits, misses);
        } else if hits > 0 {
            println!("  '{}': all {} hits", c, hits);
        }
    }

    println!(
        "\nTotal: {} lookups, {} misses ({:.1}% hit rate)",
        total_lookups,
        total_misses,
        if total_lookups > 0 {
            (total_lookups - total_misses) as f64 / total_lookups as f64 * 100.0
        } else {
            100.0
        }
    );

    Ok(())
}
