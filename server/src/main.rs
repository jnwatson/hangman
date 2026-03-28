use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Parser;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tracing::info;
use uuid::Uuid;

use hangman2::dictionary::Dictionary;
use hangman2::game::{letter_bit, LetterSet};
use hangman2::solver::disk_cache::DiskCache;
use hangman2::solver::serving::{canonical_hash_for_words, decode_tt_entry, fold_required_letters, pos_mask};
use hangman2::solver::MemoizedSolver;

#[derive(Parser)]
#[command(name = "dead-letters-server")]
struct Cli {
    /// Path to dictionary file
    #[arg(short, long, default_value = "enable1.txt")]
    dictionary: PathBuf,

    /// Path to precomputed TT cache directory
    #[arg(short, long, default_value = "./game_cache")]
    cache_dir: PathBuf,

    /// Port to listen on
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    /// Path to static files (Svelte build output)
    #[arg(short, long)]
    static_dir: Option<PathBuf>,
}

/// Per-length precomputed data.
struct LengthData {
    words: Vec<Vec<u8>>,
    disk_cache: Option<Arc<DiskCache>>,
    minimax_value: Option<u32>,
    /// Solver initialized for on-demand position evaluation.
    solver: Arc<MemoizedSolver>,
}

/// Active game session.
struct GameSession {
    word_length: usize,
    /// Indices into LengthData.words for remaining valid words.
    remaining: Vec<usize>,
    masked: LetterSet,
    pattern: Vec<u8>,
    wrong_letters: Vec<u8>,
    guesses_allowed: u32,
    guesses_left: u32,
    game_over: bool,
    won: bool,
}

struct AppState {
    lengths: HashMap<usize, LengthData>,
    sessions: DashMap<String, GameSession>,
}

// -- Request/Response types --

#[derive(Deserialize)]
struct NewGameQuery {
    length: usize,
}

#[derive(Serialize)]
struct NewGameResponse {
    game_id: String,
    word_length: usize,
    guesses_allowed: u32,
    minimax_value: Option<u32>,
    dictionary_size: usize,
}

#[derive(Deserialize)]
struct GuessRequest {
    game_id: String,
    letter: String,
}

#[derive(Serialize)]
struct GuessResponse {
    positions: Option<Vec<usize>>,
    pattern: String,
    guesses_left: u32,
    wrong_letters: Vec<String>,
    game_over: bool,
    won: bool,
    example_word: Option<String>,
    valid_words_bitvec: String,
    /// "solved", "degraded", or "unresolved"
    solve_status: &'static str,
}

#[derive(Deserialize)]
struct WordListQuery {
    length: usize,
}

#[derive(Serialize)]
struct WordListResponse {
    words: Vec<String>,
}

type AppError = (StatusCode, String);

fn err(status: StatusCode, msg: impl Into<String>) -> AppError {
    (status, msg.into())
}

// -- Handlers --

async fn handle_new_game(
    State(state): State<Arc<AppState>>,
    Query(params): Query<NewGameQuery>,
) -> Result<Json<NewGameResponse>, AppError> {
    let length_data = state
        .lengths
        .get(&params.length)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no words of length {}", params.length)))?;

    if length_data.words.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no words of that length"));
    }

    let guesses_allowed = length_data
        .minimax_value
        .map(|v| v.saturating_sub(1))
        .unwrap_or(6);

    let game_id = Uuid::new_v4().to_string();
    let remaining: Vec<usize> = (0..length_data.words.len()).collect();

    let session = GameSession {
        word_length: params.length,
        remaining,
        masked: 0,
        pattern: vec![b'_'; params.length],
        wrong_letters: Vec::new(),
        guesses_allowed,
        guesses_left: guesses_allowed,
        game_over: false,
        won: false,
    };

    state.sessions.insert(game_id.clone(), session);

    Ok(Json(NewGameResponse {
        game_id,
        word_length: params.length,
        guesses_allowed,
        minimax_value: length_data.minimax_value,
        dictionary_size: length_data.words.len(),
    }))
}

async fn handle_guess(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GuessRequest>,
) -> Result<Json<GuessResponse>, AppError> {
    let mut session = state
        .sessions
        .get_mut(&req.game_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

    if session.game_over {
        return Err(err(StatusCode::BAD_REQUEST, "game is over"));
    }

    let letter = req
        .letter
        .bytes()
        .next()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "empty letter"))?
        .to_ascii_lowercase();

    if !letter.is_ascii_lowercase() {
        return Err(err(StatusCode::BAD_REQUEST, "invalid letter"));
    }

    if session.masked & letter_bit(letter) != 0 {
        return Err(err(StatusCode::BAD_REQUEST, "letter already guessed"));
    }

    let length_data = state
        .lengths
        .get(&session.word_length)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "length data missing"))?;

    // Partition remaining words by this letter's positions.
    let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in &session.remaining {
        let mask = pos_mask(&length_data.words[idx], letter);
        partitions.entry(mask).or_default().push(idx);
    }

    // Pick the worst partition for the guesser (highest minimax value).
    let new_masked = session.masked | letter_bit(letter);

    // Collect partitions into a stable Vec for indexed access.
    let parts: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();

    // Phase 1: Look up all partitions in the cache, track which need solving.
    let mut values: Vec<Option<u32>> = Vec::with_capacity(parts.len());
    let mut unsolved: Vec<usize> = Vec::new();

    for (i, (_pmask, indices)) in parts.iter().enumerate() {
        if indices.len() <= 1 {
            values.push(Some(0));
            continue;
        }
        let folded = fold_required_letters(&length_data.words, indices, new_masked);
        let hash = canonical_hash_for_words(&length_data.words, indices, folded);
        let cached = length_data
            .disk_cache
            .as_ref()
            .and_then(|dc| dc.get(hash))
            .and_then(decode_tt_entry)
            .map(|e| e.value);

        values.push(cached);
        if cached.is_none() {
            unsolved.push(i);
        }
    }

    // Phase 2: Solve cache misses with a 10-second budget.
    // Run in spawn_blocking to avoid blocking the tokio runtime.
    let mut any_timeout = false;
    if !unsolved.is_empty() {
        let solver = Arc::clone(&length_data.solver);
        let unsolved_parts: Vec<(usize, Vec<usize>)> = unsolved
            .iter()
            .map(|&i| (i, parts[i].1.clone()))
            .collect();
        let word_length = session.word_length;

        let results = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut solved: Vec<(usize, Option<u32>)> = Vec::new();
            let mut timed_out = false;
            for (i, indices) in &unsolved_parts {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    for (j, inds) in &unsolved_parts {
                        if solved.iter().all(|(si, _)| si != j) {
                            tracing::error!(
                                "SOLVE TIMEOUT: partition of {} words (k={}, masked={:#x})",
                                inds.len(),
                                word_length,
                                new_masked,
                            );
                        }
                    }
                    break;
                }
                let value = solver.solve_position(indices, new_masked);
                solved.push((*i, Some(value)));
            }
            (solved, timed_out)
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("solve failed: {e}")))?;

        for (i, v) in results.0 {
            values[i] = v;
        }
        any_timeout = results.1;
    }

    // Phase 3: Pick worst partition.
    let mut best_idx = 0;
    let mut best_value = 0u32;

    for (i, (pmask, indices)) in parts.iter().enumerate() {
        let miss_cost = u32::from(*pmask == 0);
        let value = miss_cost + values[i].unwrap_or(0);
        if value > best_value || (value == best_value && indices.len() > parts[best_idx].1.len()) {
            best_value = value;
            best_idx = i;
        }
    }

    let best_partition_mask = parts[best_idx].0;
    let best_indices = &parts[best_idx].1;

    let solve_status = if unsolved.is_empty() {
        "solved"
    } else if any_timeout {
        "degraded"
    } else {
        "solved" // all misses resolved within budget
    };

    // Update session state.
    session.masked = new_masked;
    session.remaining = best_indices.clone();

    let positions = if best_partition_mask == 0 {
        // Miss.
        session.wrong_letters.push(letter);
        session.guesses_left = session.guesses_left.saturating_sub(1);
        None
    } else {
        // Hit: reveal letter at positions.
        let mut pos_list = Vec::new();
        for bit in 0..32 {
            if best_partition_mask & (1 << bit) != 0 {
                session.pattern[bit] = letter;
                pos_list.push(bit);
            }
        }
        Some(pos_list)
    };

    // Check game over conditions.
    if session.guesses_left == 0 {
        session.game_over = true;
        session.won = false;
    } else if !session.pattern.contains(&b'_') {
        session.game_over = true;
        session.won = true;
    }

    let example_word = if session.game_over {
        session
            .remaining
            .first()
            .map(|&idx| String::from_utf8_lossy(&length_data.words[idx]).to_string())
    } else {
        None
    };

    let pattern_str: String = session
        .pattern
        .iter()
        .map(|&b| if b == b'_' { '_' } else { b.to_ascii_uppercase() as char })
        .collect();

    let wrong: Vec<String> = session
        .wrong_letters
        .iter()
        .map(|&b| (b.to_ascii_uppercase() as char).to_string())
        .collect();

    Ok(Json(GuessResponse {
        positions,
        pattern: pattern_str,
        guesses_left: session.guesses_left,
        wrong_letters: wrong,
        game_over: session.game_over,
        won: session.won,
        example_word,
        valid_words_bitvec: encode_bitvec(&session.remaining, length_data.words.len()),
        solve_status,
    }))
}

async fn handle_wordlist(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WordListQuery>,
) -> Result<Json<WordListResponse>, AppError> {
    let length_data = state
        .lengths
        .get(&params.length)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no words of length {}", params.length)))?;

    let words: Vec<String> = length_data
        .words
        .iter()
        .map(|w| String::from_utf8_lossy(w).to_string())
        .collect();

    Ok(Json(WordListResponse { words }))
}

async fn handle_health() -> impl IntoResponse {
    "ok"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!("loading dictionary from {:?}", cli.dictionary);
    let dict = Dictionary::from_file(&cli.dictionary)?;
    info!("loaded {} words", dict.total_words());

    // Load precomputed data for each word length.
    let mut lengths = HashMap::new();
    for k in dict.available_lengths() {
        let words = dict.words_of_length(k).to_vec();
        if words.is_empty() {
            continue;
        }

        let disk_cache: Option<Arc<DiskCache>> = if cli.cache_dir.exists() {
            match DiskCache::open_if_exists(&cli.cache_dir, k, &words, 16 * 1024 * 1024 * 1024) {
                Ok(Some(dc)) => {
                    info!("k={}: loaded disk cache ({} entries)", k, dc.entry_count());
                    Some(Arc::new(dc))
                }
                Ok(None) => {
                    info!("k={}: no disk cache found", k);
                    None
                }
                Err(e) => {
                    tracing::warn!("k={}: failed to open disk cache: {}", k, e);
                    None
                }
            }
        } else {
            None
        };

        let minimax_value = disk_cache.as_ref().and_then(|dc| {
            let all_indices: Vec<usize> = (0..words.len()).collect();
            let root_masked = fold_required_letters(&words, &all_indices, 0);
            let root_hash = canonical_hash_for_words(&words, &all_indices, root_masked);
            let v = dc.get(root_hash).and_then(decode_tt_entry).map(|e| e.value);
            if let Some(val) = v {
                info!("k={}: minimax value = {}", k, val);
            }
            v
        }).or_else(|| Some(estimate_minimax(k, words.len())));

        let solver = Arc::new(MemoizedSolver::for_serving(words.clone(), disk_cache.clone()));

        lengths.insert(k, LengthData {
            words,
            disk_cache,
            minimax_value,
            solver,
        });
    }

    info!("serving {} word lengths", lengths.len());

    let state = Arc::new(AppState {
        lengths,
        sessions: DashMap::new(),
    });

    let mut app = Router::new()
        .route("/api/health", get(handle_health))
        .route("/api/new", get(handle_new_game))
        .route("/api/guess", post(handle_guess))
        .route("/api/wordlist", get(handle_wordlist))
        .with_state(state);

    // Serve static files if configured.
    if let Some(static_dir) = cli.static_dir {
        info!("serving static files from {:?}", static_dir);
        app = app.fallback_service(ServeDir::new(static_dir));
    }

    let addr = format!("0.0.0.0:{}", cli.port);
    info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Encode a set of word indices as a base64-encoded bitvector.
fn encode_bitvec(indices: &[usize], total_words: usize) -> String {
    let num_bytes = (total_words + 7) / 8;
    let mut bits = vec![0u8; num_bytes];
    for &i in indices {
        bits[i / 8] |= 1 << (i % 8);
    }
    BASE64.encode(&bits)
}

/// Rough minimax estimates for word lengths without precomputed data.
fn estimate_minimax(k: usize, _word_count: usize) -> u32 {
    match k {
        1 => 25,
        2 => 14,
        3 => 17,
        4 => 16,
        5 => 14,
        6 => 12,
        7 => 11,
        8 => 8,
        9 => 6,
        10 => 5,
        11 => 6,
        12 => 4,
        13 => 4,
        14 => 3,
        15..=19 => 2,
        20..=25 => 1,
        _ => 0,
    }
}
