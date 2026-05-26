//! Probe: at pre-J state for k=6 (after misses ABCDFGEHILO), enumerate every
//! unguessed letter's partitions and FRESH-solve each partition with no disk
//! cache. Compare the computed V(pre-J) = min over L of max over partitions
//! of (miss_cost + V) against the live solver's direct V(pre-J).
//!
//! If the two agree, the solver is internally consistent. If they disagree,
//! there's a real solver bug.

use std::collections::HashMap;
use std::path::Path;

use hangman2::dictionary::Dictionary;
use hangman2::game::letter_bit;
use hangman2::solver::MemoizedSolver;
use hangman2::solver::serving::pos_mask;

fn main() -> anyhow::Result<()> {
    let dict = Dictionary::from_file(Path::new("enable1.txt"))?;
    let words: Vec<Vec<u8>> = dict.words_of_length(6).to_vec();
    println!("loaded {} k=6 words", words.len());

    // Walk path "abcdfgehilo", taking the miss partition each step.
    let mut indices: Vec<usize> = (0..words.len()).collect();
    let mut masked: u32 = 0;
    let path = "abcdfgehilo";
    for letter_ch in path.chars() {
        let letter = letter_ch as u8;
        masked |= letter_bit(letter);
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }
        let (_, chosen) = parts
            .into_iter()
            .max_by_key(|(_, ix)| ix.len())
            .unwrap();
        indices = chosen;
    }
    println!(
        "pre-J state: {} words, masked={masked:026b}",
        indices.len()
    );

    // Direct solve.
    let direct = MemoizedSolver::new()
        .solve_position_smp(&words, &indices, masked);
    println!("\nDirect V(pre-J), fresh solver: {direct}");

    // Per-letter enumeration with FRESH solvers per partition.
    println!("\nPer-letter (each partition solved with a fresh solver, no cache):");
    let mut overall_min: u32 = u32::MAX;
    for letter in b'a'..=b'z' {
        if masked & letter_bit(letter) != 0 {
            continue;
        }
        let mut parts: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let m = pos_mask(&words[idx], letter);
            parts.entry(m).or_default().push(idx);
        }
        let new_masked = masked | letter_bit(letter);

        let mut letter_worst: u32 = 0;
        let mut detail: Vec<(u32, usize, u32, u32)> = Vec::new();
        for (pmask, sub) in &parts {
            let miss_cost = u32::from(*pmask == 0);
            let v_sub = if sub.len() <= 1 {
                0
            } else {
                MemoizedSolver::new().solve_position_smp(&words, sub, new_masked)
            };
            letter_worst = letter_worst.max(miss_cost + v_sub);
            detail.push((*pmask, sub.len(), miss_cost, v_sub));
        }
        detail.sort_by_key(|d| std::cmp::Reverse(d.1));
        print!("  {} worst={letter_worst} | ", letter as char);
        for (pmask, n, mc, v) in &detail {
            print!("p{pmask:#x}n{n}m{mc}v{v} ");
        }
        println!();
        overall_min = overall_min.min(letter_worst);
    }
    println!("\nMin over letters of worst: {overall_min}");
    println!("Direct V(pre-J): {direct}");
    println!(
        "Match: {}",
        if overall_min == direct { "YES" } else { "NO — solver inconsistency" }
    );
    Ok(())
}
