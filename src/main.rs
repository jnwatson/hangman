#![deny(clippy::all, clippy::pedantic)]

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

use hangman2::dictionary::Dictionary;
use hangman2::solver::MemoizedSolver;

#[derive(Parser)]
#[command(name = "hangman2", about = "Schrödinger's Hangman solver")]
struct Cli {
    /// Path to dictionary file (one word per line)
    #[arg(short, long)]
    dict: PathBuf,

    /// Word length to solve (if omitted, solves all available lengths)
    #[arg(short, long)]
    length: Option<usize>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dict = Dictionary::from_file(&cli.dict)?;
    println!("Loaded {} words", dict.total_words());

    let lengths = if let Some(len) = cli.length {
        if dict.words_of_length(len).is_empty() {
            bail!("No words of length {len} in dictionary");
        }
        vec![len]
    } else {
        dict.available_lengths()
    };

    for len in &lengths {
        let words = dict.words_of_length(*len);
        println!("Length {len}: {} words — solving...", words.len());
        let solver = MemoizedSolver::new();
        let misses = solver.solve(words);
        println!(
            "  optimal misses: {misses}  (cache entries: {})",
            solver.cache_size()
        );
    }

    Ok(())
}
