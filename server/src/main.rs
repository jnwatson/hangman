use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Parser;
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc};
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
    /// Bumped on every committed guess. A queued slow-path job carries the
    /// generation it observed in Phase 1; if the session has advanced when
    /// the worker tries to commit, the job is rejected as stale (409).
    generation: u64,
}

/// Maximum number of concurrent sessions.
const MAX_SESSIONS: usize = 10_000;
/// Sessions expire after this duration of inactivity.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// How many guess solves may run concurrently. Each solve gets up to 2 cores
/// of partition-level parallelism, so 7 jobs × 2 cores = 14 cores active at
/// peak, leaving ~2 cores for OS / niced precompute on a 16-core box.
const SERVE_CONCURRENCY: usize = 7;
/// Per-user partition-level parallelism. Each user's job runs at most this
/// many partition solves concurrently.
const PER_JOB_PARALLELISM: usize = 2;
/// Job results live in memory for this long after completion before sweep.
const JOB_RESULT_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// Maximum queued+running jobs before we shed load with 503.
const MAX_INFLIGHT_JOBS: usize = 200;

/// Per-job state visible to the polling endpoint.
#[derive(Clone)]
enum JobState {
    /// Submitted, awaiting a worker permit.
    Queued { ticket: u64 },
    /// A worker is processing it.
    Running { ticket: u64 },
    /// Solve completed; result available, awaiting fetch by client.
    Done { resp: GuessResponse, finished_at: Instant },
    /// Solve failed with this message.
    Failed { msg: String, finished_at: Instant },
}

/// All data captured at handle_guess time so the worker can finish without
/// re-reading session state. `parts`, `values`, `unsolved` are the result of
/// Phase 1 (partition + cache lookup) which already ran synchronously.
struct JobSpec {
    request_id: String,
    ticket: u64,
    game_id: String,
    word_length: usize,
    letter: u8,
    new_masked: LetterSet,
    current_remaining: Vec<usize>,
    parts: Vec<(u32, Vec<usize>)>,
    values: Vec<Option<u32>>,
    unsolved: Vec<usize>,
    optimal_letter: Option<u8>,
    cache_hits: usize,
    cache_misses: usize,
    ms_session_read: u128,
    ms_partition: u128,
    ms_phase1_cache: u128,
    submitted_at: Instant,
    /// Session generation observed at Phase 1. If the session has moved on
    /// before the worker commits, the job returns 409 Conflict.
    session_gen: u64,
}

struct AppState {
    lengths: HashMap<usize, LengthData>,
    sessions: DashMap<String, GameSession>,
    /// Job results, keyed by request_id.
    jobs: DashMap<String, JobState>,
    /// Sender into the worker dispatcher.
    job_tx: mpsc::UnboundedSender<JobSpec>,
    /// Monotonic ticket counter assigned at submission time.
    next_ticket: AtomicU64,
    /// Sorted set of in-flight ticket numbers (queued or running). Used to
    /// compute "jobs ahead of me" for queue-position responses.
    inflight: AsyncMutex<BTreeSet<u64>>,
    /// Caps simultaneous active solves to SERVE_CONCURRENCY.
    serve_semaphore: Arc<Semaphore>,
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

#[derive(Serialize, Clone)]
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

#[derive(Serialize)]
struct GuessQueuedResponse {
    request_id: String,
    queue_position: usize,
    /// Estimated seconds until result is ready (best effort).
    eta_seconds: u32,
}

#[derive(Deserialize)]
struct StatusQuery {
    request_id: String,
}

#[derive(Serialize)]
struct StatusResponse {
    /// "queued", "running", "done", or "failed".
    state: &'static str,
    queue_position: Option<usize>,
    result: Option<GuessResponse>,
    error: Option<String>,
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
        generation: 0,
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
) -> Result<Response, AppError> {
    let t_start = Instant::now();
    // Snapshot session state into locals, then drop the DashMap guard
    // immediately. Holding a DashMap RefMut across `.await` deadlocks the
    // tokio runtime on a 1-worker setup, because the shard lock is sync.
    let (word_length, current_remaining, current_masked, session_gen) = {
        let session = state
            .sessions
            .get(&req.game_id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

        if session.game_over {
            return Err(err(StatusCode::BAD_REQUEST, "game is over"));
        }
        (
            session.word_length,
            session.remaining.clone(),
            session.masked,
            session.generation,
        )
    };
    let ms_session_read = t_start.elapsed().as_millis();

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
    let ms_partition = t_start.elapsed().as_millis();

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

    let ms_phase1_cache = t_start.elapsed().as_millis();
    let cache_hits = parts.len() - unsolved.len();
    let cache_misses = unsolved.len();

    // ---- Slow path: any cache miss → enqueue and return 202 ----
    if !unsolved.is_empty() {
        let spec = JobSpec {
            request_id: String::new(),
            ticket: 0,
            game_id: req.game_id.clone(),
            word_length,
            letter,
            new_masked,
            current_remaining: current_remaining.clone(),
            parts: parts.clone(),
            values: values.clone(),
            unsolved: unsolved.clone(),
            optimal_letter,
            cache_hits,
            cache_misses,
            ms_session_read,
            ms_partition,
            ms_phase1_cache,
            submitted_at: t_start,
            session_gen,
        };
        return enqueue_slow_path(state, spec).await;
    }

    // ---- Fast path: all partitions cached, finalize inline ----
    let any_timeout = false;
    let ms_sem_acquire: u128 = 0;
    let ms_live_solve: u128 = 0;
    let ms_spawn_overhead: u128 = 0;
    let per_partition_ms: Vec<u128> = Vec::new();

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

    if session.generation != session_gen {
        return Err(err(StatusCode::CONFLICT, "guess is stale (session moved on)"));
    }

    session.last_active = std::time::Instant::now();
    session.masked = new_masked;
    session.remaining = best_indices.clone();
    session.generation += 1;

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
    let ms_total = t_start.elapsed().as_millis();
    let per_part_str = per_partition_ms
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
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
    let part_sizes_str = parts
        .iter()
        .map(|(_, ix)| ix.len().to_string())
        .collect::<Vec<_>>()
        .join(",");
    info!(
        "GAME timing id={} letter={} pre_remaining={} post_remaining={} total_ms={} session_read_ms={} partition_ms={} phase1_ms={} sem_acquire_ms={} live_solve_ms={} spawn_overhead_ms={} partitions={} cache_hits={} cache_misses={} per_partition_ms=[{}] partition_sizes=[{}]",
        req.game_id,
        actual_ch,
        current_remaining.len(),
        session.remaining.len(),
        ms_total,
        ms_session_read,
        ms_partition,
        ms_phase1_cache,
        ms_sem_acquire,
        ms_live_solve,
        ms_spawn_overhead,
        parts.len(),
        cache_hits,
        cache_misses,
        per_part_str,
        part_sizes_str,
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
    })
    .into_response())
}

/// Enqueue a slow-path solve and return a 202 with the request_id.
async fn enqueue_slow_path(state: Arc<AppState>, mut spec: JobSpec) -> Result<Response, AppError> {
    {
        let inflight = state.inflight.lock().await;
        if inflight.len() >= MAX_INFLIGHT_JOBS {
            return Err(err(StatusCode::SERVICE_UNAVAILABLE, "queue full"));
        }
    }
    let request_id = Uuid::new_v4().to_string();
    let ticket = state.next_ticket.fetch_add(1, Ordering::SeqCst);
    spec.request_id = request_id.clone();
    spec.ticket = ticket;
    let position = {
        let mut inflight = state.inflight.lock().await;
        inflight.insert(ticket);
        inflight.iter().take_while(|&&t| t < ticket).count() + 1
    };
    state
        .jobs
        .insert(request_id.clone(), JobState::Queued { ticket });
    state
        .job_tx
        .send(spec)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "queue closed"))?;
    let eta_seconds = (position as u32).saturating_mul(12);
    Ok((
        StatusCode::ACCEPTED,
        Json(GuessQueuedResponse {
            request_id,
            queue_position: position,
            eta_seconds,
        }),
    )
        .into_response())
}

/// Worker task: pulls JobSpecs and processes them under a concurrency cap.
fn spawn_worker_dispatcher(state: Arc<AppState>, mut rx: mpsc::UnboundedReceiver<JobSpec>) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let permit = state
                .serve_semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("serve_semaphore closed");
            let state2 = Arc::clone(&state);
            tokio::spawn(async move {
                let _permit = permit;
                process_job(state2, job).await;
            });
        }
    });
}

/// Run a queued job: do Phase 2 (live solves, parallel) + Phase 3 + session
/// update + response build, then store the result for polling.
async fn process_job(state: Arc<AppState>, job: JobSpec) {
    state
        .jobs
        .insert(job.request_id.clone(), JobState::Running { ticket: job.ticket });

    let result = run_phases_2_3_finalize(&state, &job).await;

    let final_state = match result {
        Ok(resp) => JobState::Done {
            resp,
            finished_at: Instant::now(),
        },
        Err((status, msg)) => JobState::Failed {
            msg: format!("{status}: {msg}"),
            finished_at: Instant::now(),
        },
    };
    state.jobs.insert(job.request_id.clone(), final_state);
    let mut inflight = state.inflight.lock().await;
    inflight.remove(&job.ticket);
}

/// Phase 2 (parallel live solves) + Phase 3 (pick worst) + session update +
/// response build + logging. Mirrors the logic that used to be inline in
/// handle_guess for the slow path.
async fn run_phases_2_3_finalize(
    state: &AppState,
    job: &JobSpec,
) -> Result<GuessResponse, AppError> {
    let length_data = state
        .lengths
        .get(&job.word_length)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "length data missing"))?;

    let t_start_phase2 = Instant::now();

    // ---- Phase 2: solve unsolved partitions in parallel chunks of PER_JOB_PARALLELISM ----
    let mut order: Vec<usize> = job.unsolved.clone();
    order.sort_by_key(|&i| job.parts[i].1.len());

    let total_deadline = Instant::now() + Duration::from_secs(20);
    // Shared accumulator across concurrent partition solves.
    let solved: Arc<std::sync::Mutex<Vec<(usize, Option<u32>, u128, bool)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let any_timeout_arc = Arc::new(std::sync::atomic::AtomicBool::new(false));

    stream::iter(order)
        .for_each_concurrent(Some(PER_JOB_PARALLELISM), |i| {
            let solved = Arc::clone(&solved);
            let any_timeout_arc = Arc::clone(&any_timeout_arc);
            let solver = Arc::clone(&length_data.solver);
            let indices = job.parts[i].1.clone();
            let new_masked = job.new_masked;
            async move {
                let now = Instant::now();
                if now >= total_deadline {
                    any_timeout_arc.store(true, Ordering::Relaxed);
                    return;
                }
                let per_partition = Duration::from_secs(8);
                let remaining = total_deadline.saturating_duration_since(now);
                let dl = now + remaining.min(per_partition);
                // Hard outer bound — covers the case where the solver's
                // shared cancellation flag races between concurrent calls
                // and a solve overshoots its watchdog.
                let outer_budget = remaining.min(per_partition) + Duration::from_secs(2);
                let result = tokio::time::timeout(
                    outer_budget,
                    tokio::task::spawn_blocking(move || {
                        let t_solve = Instant::now();
                        let (v, cancelled) =
                            solver.solve_position_with_deadline(&indices, new_masked, Some(dl));
                        (v, cancelled, t_solve.elapsed().as_millis())
                    }),
                )
                .await;
                let mut s = solved.lock().unwrap();
                match result {
                    Ok(Ok((v, false, solve_ms))) => s.push((i, Some(v), solve_ms, false)),
                    Ok(Ok((_, true, solve_ms))) => {
                        s.push((i, None, solve_ms, true));
                        any_timeout_arc.store(true, Ordering::Relaxed);
                    }
                    Ok(Err(_)) | Err(_) => {
                        s.push((i, None, 0, true));
                        any_timeout_arc.store(true, Ordering::Relaxed);
                    }
                }
            }
        })
        .await;

    let mut values = job.values.clone();
    let mut per_partition_ms: Vec<u128> = Vec::new();
    let mut ms_live_solve: u128 = 0;
    {
        let s = solved.lock().unwrap();
        for &(i, v, ms, _) in s.iter() {
            values[i] = v;
            per_partition_ms.push(ms);
            ms_live_solve += ms;
        }
    }
    let any_timeout = any_timeout_arc.load(Ordering::Relaxed);
    let _ = t_start_phase2; // (kept for future timing detail)

    finalize_response(state, &length_data, job, &values, any_timeout, ms_live_solve, per_partition_ms)
}

/// Phase 3 + session update + response build + logging — extracted so the
/// worker can call it after Phase 2 completes.
fn finalize_response(
    state: &AppState,
    length_data: &LengthData,
    job: &JobSpec,
    values: &[Option<u32>],
    any_timeout: bool,
    ms_live_solve: u128,
    per_partition_ms: Vec<u128>,
) -> Result<GuessResponse, AppError> {
    // Phase 3: pick worst partition.
    let mut best_idx = 0;
    let mut best_value = 0u32;
    for (i, (pmask, indices)) in job.parts.iter().enumerate() {
        let miss_cost = u32::from(*pmask == 0);
        let value = match values[i] {
            Some(v) => miss_cost + v,
            None => u32::MAX,
        };
        if value > best_value
            || (value == best_value && indices.len() > job.parts[best_idx].1.len())
        {
            best_value = value;
            best_idx = i;
        }
    }

    let best_partition_mask = job.parts[best_idx].0;
    let best_indices = &job.parts[best_idx].1;
    let unsolved_was_empty = job.unsolved.is_empty();

    let solve_status = if unsolved_was_empty {
        "solved"
    } else if any_timeout {
        "degraded"
    } else {
        "solved"
    };

    let mut session = state
        .sessions
        .get_mut(&job.game_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "game not found"))?;

    if session.generation != job.session_gen {
        return Err(err(StatusCode::CONFLICT, "guess is stale (session moved on)"));
    }

    session.last_active = Instant::now();
    session.masked = job.new_masked;
    session.remaining = best_indices.clone();
    session.generation += 1;

    let positions = if best_partition_mask == 0 {
        session.wrong_letters.push(job.letter);
        if session.guesses_left == 0 {
            session.game_over = true;
            session.won = false;
        } else {
            session.guesses_left -= 1;
        }
        None
    } else {
        let mut pos_list = Vec::new();
        for bit in 0..32 {
            if best_partition_mask & (1 << bit) != 0 {
                session.pattern[bit] = job.letter;
                pos_list.push(bit);
            }
        }
        Some(pos_list)
    };

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

    let opt_ch = job
        .optimal_letter
        .map(|b| (b as char).to_string())
        .unwrap_or_else(|| "?".into());
    let actual_ch = job.letter as char;
    let result_str = if best_partition_mask == 0 {
        "miss".to_string()
    } else {
        format!("hit({:#06x})", best_partition_mask)
    };
    let value_str = match values[best_idx] {
        Some(v) => v.to_string(),
        None => "?".into(),
    };
    let ms_total = job.submitted_at.elapsed().as_millis();
    let per_part_str = per_partition_ms
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    info!(
        "GAME guess id={} letter={} optimal={} result={} remaining={} wrong={} left={} value={} status={}",
        job.game_id,
        actual_ch,
        opt_ch,
        result_str,
        session.remaining.len(),
        session.wrong_letters.len(),
        session.guesses_left,
        value_str,
        solve_status,
    );
    let part_sizes_str = job
        .parts
        .iter()
        .map(|(_, ix)| ix.len().to_string())
        .collect::<Vec<_>>()
        .join(",");
    info!(
        "GAME timing id={} letter={} pre_remaining={} post_remaining={} total_ms={} session_read_ms={} partition_ms={} phase1_ms={} live_solve_ms={} partitions={} cache_hits={} cache_misses={} per_partition_ms=[{}] partition_sizes=[{}]",
        job.game_id,
        actual_ch,
        job.current_remaining.len(),
        session.remaining.len(),
        ms_total,
        job.ms_session_read,
        job.ms_partition,
        job.ms_phase1_cache,
        ms_live_solve,
        job.parts.len(),
        job.cache_hits,
        job.cache_misses,
        per_part_str,
        part_sizes_str,
    );

    if session.game_over {
        let guessed_letters: Vec<char> = (0..26u8)
            .filter(|i| session.masked & (1u32 << i) != 0)
            .map(|i| (b'a' + i) as char)
            .collect();
        let guessed_str: String = guessed_letters.iter().collect();
        let wrong_str: String = session.wrong_letters.iter().map(|&b| b as char).collect();
        info!(
            "GAME end id={} won={} misses={} guessed=[{}] wrong=[{}] pattern={} word={:?}",
            job.game_id,
            session.won,
            session.wrong_letters.len(),
            guessed_str,
            wrong_str,
            pattern_str,
            example_word.as_deref().unwrap_or("?"),
        );
    }

    Ok(GuessResponse {
        positions,
        pattern: pattern_str,
        guesses_left: session.guesses_left,
        wrong_letters: wrong,
        game_over: session.game_over,
        won: session.won,
        example_word,
        valid_words_bitvec: encode_bitvec(&session.remaining, length_data.words.len()),
        solve_status,
    })
}

/// Polling endpoint for slow-path guess results.
async fn handle_guess_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatusQuery>,
) -> Result<Response, AppError> {
    let entry = state
        .jobs
        .get(&params.request_id)
        .map(|e| e.value().clone());
    match entry {
        Some(JobState::Done { resp, .. }) => {
            state.jobs.remove(&params.request_id);
            Ok(Json(StatusResponse {
                state: "done",
                queue_position: None,
                result: Some(resp),
                error: None,
            })
            .into_response())
        }
        Some(JobState::Failed { msg, .. }) => {
            state.jobs.remove(&params.request_id);
            Ok(Json(StatusResponse {
                state: "failed",
                queue_position: None,
                result: None,
                error: Some(msg),
            })
            .into_response())
        }
        Some(JobState::Queued { ticket }) => {
            let position = {
                let inflight = state.inflight.lock().await;
                inflight.iter().take_while(|&&t| t < ticket).count() + 1
            };
            Ok(Json(StatusResponse {
                state: "queued",
                queue_position: Some(position),
                result: None,
                error: None,
            })
            .into_response())
        }
        Some(JobState::Running { ticket }) => {
            let position = {
                let inflight = state.inflight.lock().await;
                inflight.iter().take_while(|&&t| t < ticket).count() + 1
            };
            Ok(Json(StatusResponse {
                state: "running",
                queue_position: Some(position),
                result: None,
                error: None,
            })
            .into_response())
        }
        None => Err(err(StatusCode::NOT_FOUND, "request not found")),
    }
}

/// Background task: remove Done/Failed entries older than JOB_RESULT_TTL.
fn spawn_job_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = Instant::now();
            state.jobs.retain(|_, v| match v {
                JobState::Done { finished_at, .. } | JobState::Failed { finished_at, .. } => {
                    now.duration_since(*finished_at) < JOB_RESULT_TTL
                }
                _ => true,
            });
        }
    });
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

    // Solve uncached letters — best effort. If no permit is free, return
    // 503 since hints are optional.
    let mut any_timeout = false;
    if !needs_solve.is_empty() {
        let permit = state.serve_semaphore.clone().try_acquire_owned();
        if let Ok(_permit) = permit {
            let deadline = Instant::now() + Duration::from_secs(5);
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
                        let (value, cancelled) = solver2
                            .solve_position_with_deadline(indices, new_masked, Some(dl));
                        if cancelled {
                            was_cancelled = true;
                            break;
                        }
                        worst = worst.max(miss_cost + value);
                    }
                    (worst, was_cancelled)
                })
                .await;

                match result {
                    Ok((worst, false)) => {
                        evals.push(LetterEval { letter, value: Some(worst) });
                    }
                    _ => {
                        any_timeout = true;
                        break;
                    }
                }
            }
        } else {
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
            match DiskCache::open_if_exists(&cli.cache_dir, k, &words, 1024_usize * 1024 * 1024 * 1024) {
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

    let (job_tx, job_rx) = mpsc::unbounded_channel::<JobSpec>();

    let state = Arc::new(AppState {
        lengths,
        sessions: DashMap::new(),
        jobs: DashMap::new(),
        job_tx,
        next_ticket: AtomicU64::new(0),
        inflight: AsyncMutex::new(BTreeSet::new()),
        serve_semaphore: Arc::new(Semaphore::new(SERVE_CONCURRENCY)),
    });

    spawn_worker_dispatcher(Arc::clone(&state), job_rx);
    spawn_job_sweeper(Arc::clone(&state));

    // Background task: expire stale sessions every 5 minutes.
    let cleanup_state = Arc::clone(&state);

    let mut app = Router::new()
        .route("/api/health", get(handle_health))
        .route("/api/new", get(handle_new_game))
        .route("/api/guess", post(handle_guess))
        .route("/api/guess_status", get(handle_guess_status))
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
