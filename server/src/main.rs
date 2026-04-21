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
    /// When this session was last accessed (for expiry).
    last_active: std::time::Instant,
}

/// Maximum number of concurrent sessions.
const MAX_SESSIONS: usize = 10_000;
/// Sessions expire after this duration of inactivity.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

struct AppState {
    lengths: HashMap<usize, LengthData>,
    sessions: DashMap<String, GameSession>,
    /// Only one solver runs at a time on this single-vCPU machine.
    solver_semaphore: tokio::sync::Semaphore,
    /// The currently active solver (if any), so new requests can cancel it.
    active_solver: std::sync::Mutex<Option<Arc<MemoizedSolver>>>,
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
struct HintQuery {
    game_id: String,
}

#[derive(Serialize)]
struct HintResponse {
    letter: String,
    /// Minimax value if this letter is guessed (from the referee's perspective).
    value: Option<u32>,
    /// "solved" or "degraded"
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

    // Reject if too many sessions (memory protection).
    if state.sessions.len() >= MAX_SESSIONS {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "server is busy, try again later"));
    }

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
        last_active: std::time::Instant::now(),
    };

    state.sessions.insert(game_id.clone(), session);

    info!(
        "GAME new id={} k={} dict={} minimax={:?} budget={}",
        game_id,
        params.length,
        length_data.words.len(),
        length_data.minimax_value,
        guesses_allowed,
    );

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
    // Snapshot session state into locals, then drop the DashMap guard
    // immediately. Holding a DashMap RefMut across `.await` deadlocks the
    // tokio runtime on a 1-worker setup, because the shard lock is sync.
    let (word_length, current_remaining, current_masked) = {
        let session = state
            .sessions
            .get(&req.game_id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

        if session.game_over {
            return Err(err(StatusCode::BAD_REQUEST, "game is over"));
        }
        (session.word_length, session.remaining.clone(), session.masked)
    };

    let letter = req
        .letter
        .bytes()
        .next()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "empty letter"))?
        .to_ascii_lowercase();

    if !letter.is_ascii_lowercase() {
        return Err(err(StatusCode::BAD_REQUEST, "invalid letter"));
    }

    if current_masked & letter_bit(letter) != 0 {
        return Err(err(StatusCode::BAD_REQUEST, "letter already guessed"));
    }

    let length_data = state
        .lengths
        .get(&word_length)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "length data missing"))?;

    // Look up the OPTIMAL letter for this pre-guess state from the cache.
    // Used only for post-hoc analysis logging — if the user deviates from
    // optimal play, we can trace which guesses were sub-optimal.
    let optimal_letter: Option<u8> = {
        let folded = fold_required_letters(&length_data.words, &current_remaining, current_masked);
        let hash = canonical_hash_for_words(&length_data.words, &current_remaining, folded);
        length_data
            .disk_cache
            .as_ref()
            .and_then(|dc| dc.get(hash))
            .and_then(decode_tt_entry)
            .and_then(|e| e.best_letter)
    };

    // Partition remaining words by this letter's positions.
    let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in &current_remaining {
        let mask = pos_mask(&length_data.words[idx], letter);
        partitions.entry(mask).or_default().push(idx);
    }

    // Pick the worst partition for the guesser (highest minimax value).
    let new_masked = current_masked | letter_bit(letter);

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

    // Phase 2: Solve cache misses on-demand with a hard 5-second deadline.
    // Only one solver runs at a time (1-vCPU machine). New guesses cancel
    // any active solver to avoid blocking.
    let mut any_timeout = false;
    if !unsolved.is_empty() {
        // Cancel any active solver so the semaphore frees up quickly.
        {
            let active = state.active_solver.lock().unwrap();
            if let Some(s) = active.as_ref() {
                s.cancel();
            }
        }

        // Wait for the semaphore (cancelled solver finishes within ms).
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            state.solver_semaphore.acquire(),
        ).await;

        if let Ok(Ok(_permit)) = permit {
            // Register ourselves as the active solver.
            {
                let mut active = state.active_solver.lock().unwrap();
                *active = Some(Arc::clone(&length_data.solver));
            }

            // Sort unsolved partitions by size (smallest first) so quick wins
            // happen before any slow position can starve the budget. Total
            // budget must stay below nginx's proxy_read_timeout (currently
            // 25s); we use 20s with an 8s per-partition cap.
            let mut order: Vec<usize> = unsolved.clone();
            order.sort_by_key(|&i| parts[i].1.len());

            let total_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(20);
            for &i in &order {
                let now = std::time::Instant::now();
                if now >= total_deadline {
                    any_timeout = true;
                    continue;
                }
                // Per-partition cap of 8s, but never exceed total deadline.
                let per_partition = std::time::Duration::from_secs(8);
                let remaining = total_deadline.saturating_duration_since(now);
                let dl = now + remaining.min(per_partition);

                let solver = Arc::clone(&length_data.solver);
                let indices = parts[i].1.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let v = solver.solve_position_with_deadline(&indices, new_masked, Some(dl));
                    let cancelled = solver.was_cancelled();
                    (v, cancelled)
                })
                .await;

                match result {
                    Ok((v, false)) => { values[i] = Some(v); }
                    _ => { any_timeout = true; }
                }
            }

            // Clear active solver.
            {
                let mut active = state.active_solver.lock().unwrap();
                *active = None;
            }
        } else {
            // Couldn't acquire semaphore — degrade all unsolved partitions.
            any_timeout = true;
        }
    }

    // Phase 3: Pick worst partition.
    // For unsolved partitions, assume worst case (u32::MAX) so the referee
    // never accidentally picks an easy partition over an unknown one.
    let mut best_idx = 0;
    let mut best_value = 0u32;

    for (i, (pmask, indices)) in parts.iter().enumerate() {
        let miss_cost = u32::from(*pmask == 0);
        let value = match values[i] {
            Some(v) => miss_cost + v,
            None => u32::MAX,
        };
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

    // Re-acquire the session under a write guard to apply updates.
    // No `.await` is held across this guard.
    let mut session = state
        .sessions
        .get_mut(&req.game_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

    session.last_active = std::time::Instant::now();
    session.masked = new_masked;
    session.remaining = best_indices.clone();

    let positions = if best_partition_mask == 0 {
        // Miss: lose if no budget remaining, otherwise decrement.
        session.wrong_letters.push(letter);
        if session.guesses_left == 0 {
            session.game_over = true;
            session.won = false;
        } else {
            session.guesses_left -= 1;
        }
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

    // Check win condition.
    if !session.game_over && !session.pattern.contains(&b'_') {
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

    // Log this guess. Includes optimal letter (per solver) vs actual letter
    // played, result of the guess, and state after.
    let opt_ch = optimal_letter
        .map(|b| (b as char).to_string())
        .unwrap_or_else(|| "?".into());
    let actual_ch = letter as char;
    let result_str = if best_partition_mask == 0 {
        "miss".to_string()
    } else {
        format!("hit({:#06x})", best_partition_mask)
    };
    let value_str = match values[best_idx] {
        Some(v) => v.to_string(),
        None => "?".into(),
    };
    info!(
        "GAME guess id={} letter={} optimal={} result={} remaining={} wrong={} left={} value={} status={}",
        req.game_id,
        actual_ch,
        opt_ch,
        result_str,
        session.remaining.len(),
        session.wrong_letters.len(),
        session.guesses_left,
        value_str,
        solve_status,
    );

    if session.game_over {
        let guessed_letters: Vec<char> = (0..26u8)
            .filter(|i| session.masked & (1u32 << i) != 0)
            .map(|i| (b'a' + i) as char)
            .collect();
        let guessed_str: String = guessed_letters.iter().collect();
        let wrong_str: String = session
            .wrong_letters
            .iter()
            .map(|&b| b as char)
            .collect();
        info!(
            "GAME end id={} won={} misses={} guessed=[{}] wrong=[{}] pattern={} word={:?}",
            req.game_id,
            session.won,
            session.wrong_letters.len(),
            guessed_str,
            wrong_str,
            pattern_str,
            example_word.as_deref().unwrap_or("?"),
        );
    }

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

async fn handle_hint(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HintQuery>,
) -> Result<Json<HintResponse>, AppError> {
    // Snapshot session state, then drop the DashMap guard before any
    // `.await` to avoid deadlocking the tokio runtime on the sync shard lock.
    let (word_length, masked, remaining) = {
        let session = state
            .sessions
            .get(&params.game_id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

        if session.game_over {
            return Err(err(StatusCode::BAD_REQUEST, "game is over"));
        }
        (session.word_length, session.masked, session.remaining.clone())
    };

    let length_data = state
        .lengths
        .get(&word_length)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "length data missing"))?;

    let solver = Arc::clone(&length_data.solver);
    let words = &length_data.words;
    let disk_cache = length_data.disk_cache.clone();

    // Evaluate each unguessed letter: compute the referee's best response value.
    struct LetterEval {
        letter: u8,
        value: Option<u32>,
    }

    let mut evals: Vec<LetterEval> = Vec::new();
    let mut needs_solve: Vec<(u8, Vec<(u32, Vec<usize>)>)> = Vec::new();

    for letter in b'a'..=b'z' {
        if masked & letter_bit(letter) != 0 {
            continue;
        }

        let mut partitions: HashMap<u32, Vec<usize>> = HashMap::new();
        for &idx in &remaining {
            let mask = pos_mask(&words[idx], letter);
            partitions.entry(mask).or_default().push(idx);
        }

        let new_masked = masked | letter_bit(letter);
        let mut worst_value: u32 = 0;
        let mut all_cached = true;

        for (&pmask, indices) in &partitions {
            if indices.len() <= 1 {
                let miss_cost = u32::from(pmask == 0);
                worst_value = worst_value.max(miss_cost);
                continue;
            }
            let miss_cost = u32::from(pmask == 0);
            let folded = fold_required_letters(words, indices, new_masked);
            let hash = canonical_hash_for_words(words, indices, folded);
            let cached = disk_cache
                .as_ref()
                .and_then(|dc| dc.get(hash))
                .and_then(decode_tt_entry)
                .map(|e| e.value);

            if let Some(v) = cached {
                worst_value = worst_value.max(miss_cost + v);
            } else {
                all_cached = false;
            }
        }

        if all_cached {
            evals.push(LetterEval { letter, value: Some(worst_value) });
        } else {
            let parts: Vec<(u32, Vec<usize>)> = partitions.into_iter().collect();
            needs_solve.push((letter, parts));
        }
    }

    // Solve uncached letters — but only if the solver is free.
    // Hints are optional; don't block if another request is solving.
    let mut any_timeout = false;
    if !needs_solve.is_empty() {
        let permit = state.solver_semaphore.try_acquire();
        if let Ok(_permit) = permit {
            // Register ourselves as the active solver.
            {
                let mut active = state.active_solver.lock().unwrap();
                *active = Some(Arc::clone(&solver));
            }

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            for (letter, parts) in needs_solve {
                let solver2 = Arc::clone(&solver);
                let new_masked = masked | letter_bit(letter);
                let dl = deadline;

                let result = tokio::task::spawn_blocking(move || {
                    let mut worst: u32 = 0;
                    let mut was_cancelled = false;
                    for (pmask, indices) in &parts {
                        let miss_cost = u32::from(*pmask == 0);
                        if indices.len() <= 1 {
                            worst = worst.max(miss_cost);
                            continue;
                        }
                        let value = solver2.solve_position_with_deadline(indices, new_masked, Some(dl));
                        if solver2.was_cancelled() {
                            was_cancelled = true;
                            break;
                        }
                        worst = worst.max(miss_cost + value);
                    }
                    (worst, was_cancelled)
                })
                .await;

                match result {
                    Ok((worst, false)) => { evals.push(LetterEval { letter, value: Some(worst) }); }
                    _ => { any_timeout = true; break; }
                }
            }

            // Clear active solver.
            {
                let mut active = state.active_solver.lock().unwrap();
                *active = None;
            }
        } else {
            // Solver is busy — return immediately with what we have.
            return Err(err(StatusCode::SERVICE_UNAVAILABLE, "solver busy"));
        }
    }

    // Pick the letter with the lowest worst-case value (best for guesser).
    let best = evals
        .iter()
        .filter(|e| e.value.is_some())
        .min_by_key(|e| e.value.unwrap());

    match best {
        Some(b) => Ok(Json(HintResponse {
            letter: (b.letter.to_ascii_uppercase() as char).to_string(),
            value: b.value,
            solve_status: if any_timeout { "degraded" } else { "solved" },
        })),
        None => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "no letters to evaluate")),
    }
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
        solver_semaphore: tokio::sync::Semaphore::new(1),
        active_solver: std::sync::Mutex::new(None),
    });

    // Background task: expire stale sessions every 5 minutes.
    let cleanup_state = Arc::clone(&state);

    let mut app = Router::new()
        .route("/api/health", get(handle_health))
        .route("/api/new", get(handle_new_game))
        .route("/api/guess", post(handle_guess))
        .route("/api/hint", get(handle_hint))
        .route("/api/wordlist", get(handle_wordlist))
        .with_state(state);

    // Serve static files if configured.
    if let Some(static_dir) = cli.static_dir {
        info!("serving static files from {:?}", static_dir);
        app = app.fallback_service(ServeDir::new(static_dir));
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let before = cleanup_state.sessions.len();
            cleanup_state.sessions.retain(|_, session| {
                session.last_active.elapsed() < SESSION_TTL
            });
            let expired = before - cleanup_state.sessions.len();
            if expired > 0 {
                info!("expired {} stale sessions ({} remaining)", expired, cleanup_state.sessions.len());
            }
        }
    });

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
/// Fallback minimax when the root entry isn't in the disk cache.
///
/// Values below are from cache probing (min-over-letters of max-over-partitions)
/// where available, otherwise from earlier heuristic estimates. Precompute now
/// writes the depth-0 root entry, so new cache dumps will make this fallback
/// obsolete for the k values that have been re-precomputed.
fn estimate_minimax(k: usize, _word_count: usize) -> u32 {
    match k {
        1 => 25,
        2 => 14,  // verified via cache probe
        3 => 17,  // verified via cache probe
        4 => 14,  // verified via cache probe (was 16)
        5 => 13,  // verified via cache probe (was 14)
        6 => 12,
        7 => 11,
        8 => 8,
        9 => 6,
        10 => 5,
        11 => 6,
        12 => 4,
        13 => 4,
        14 => 3,
        15..=17 => 2,
        18 => 3,  // verified via cache probe (was 2)
        19 => 3,  // verified via cache probe (was 2)
        20 => 2,  // verified via cache probe (was 1)
        21..=25 => 1,
        _ => 0,
    }
}
