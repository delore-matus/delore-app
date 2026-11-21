//! MVP HTTP/WebSocket coordinator.
//!
//! POST /request-job accepts a mock payment header and forwards the job over
//! an mpsc channel to the WebSocket connection handler.

use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions,
    io::Write,
    net::SocketAddr,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use common::{FeeConfig as SharedFeeConfig, JobPayload, JobResult, WsMessage};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, watch, Mutex};

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
type ProviderSenderMap =
    HashMap<String, (u64, mpsc::UnboundedSender<JobPayload>)>;

/// Application state shared by HTTP and WebSocket handlers.
#[derive(Clone)]
struct AppState {
    /// Serializes pending-job assignment so concurrent callers cannot send
    /// the same job to multiple providers.
    assignment_lock: Arc<Mutex<()>>,
    /// Jobs waiting for an idle provider.
    pending_jobs: Arc<Mutex<VecDeque<String>>>,
    /// Every job in one in-memory lifecycle map.
    jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    /// Per-provider inboxes used to assign jobs directly.
    provider_senders: Arc<Mutex<ProviderSenderMap>>,
    /// Connected provider details and mock earning counters.
    providers: Arc<Mutex<HashMap<String, ProviderInfo>>>,
    fees: FeeSettings,
    job_timeout_secs: u64,
}

#[derive(Debug, Clone)]
struct FeeSettings {
    fee_bps: u64,
    job_price_units: u64,
    platform_address: String,
}

#[derive(Debug, Serialize)]
struct FeesResponse {
    fee_bps: u64,
    provider_share_percent: u64,
    platform_share_percent: u64,
}

fn calculate_fee(price_units: u64, fee_bps: u64) -> (u64, u64) {
    let fee = ((price_units as u128 * fee_bps as u128) / 10_000) as u64;
    (fee, price_units - fee)
}

fn parse_fee_settings() -> Result<FeeSettings> {
    let fee_bps = std::env::var("PLATFORM_FEE_BPS")
        .unwrap_or_else(|_| "1000".to_string())
        .parse::<u64>()
        .context("PLATFORM_FEE_BPS must be an integer between 0 and 5000")?;
    if fee_bps > 5_000 {
        anyhow::bail!("PLATFORM_FEE_BPS must be between 0 and 5000 basis points");
    }

    let job_price_units = std::env::var("JOB_PRICE_UNITS")
        .unwrap_or_else(|_| "10000".to_string())
        .parse::<u64>()
        .context("JOB_PRICE_UNITS must be a non-negative integer")?;
    let platform_address =
        std::env::var("PLATFORM_ADDRESS").unwrap_or_else(|_| "0xPLATFORM_PLACEHOLDER".to_string());
    if platform_address != "0xPLATFORM_PLACEHOLDER"
        && (platform_address.len() != 42
            || !platform_address.starts_with("0x")
            || !platform_address[2..].chars().all(|c| c.is_ascii_hexdigit()))
    {
        anyhow::bail!(
            "PLATFORM_ADDRESS must be a public 0x address; never configure a private key"
        );
    }

    Ok(FeeSettings {
        fee_bps,
        job_price_units,
        platform_address,
    })
}

#[derive(Debug, Clone, Serialize)]
struct ProviderInfo {
    provider_id: String,
    payout_address: String,
    gpu_name: String,
    gpu_memory: String,
    model: String,
    status: String,
    jobs_completed: u64,
    total_earned_units: u64,
}

#[derive(Debug, Clone, Serialize)]
struct FeeBreakdown {
    gross_price: u64,
    platform_fee: u64,
    provider_payout: u64,
}

#[derive(Debug, Clone, Serialize)]
struct JobRecord {
    id: String,
    prompt: String,
    status: String,
    provider_id: Option<String>,
    result: Option<JobResult>,
    fee_breakdown: Option<FeeBreakdown>,
    error: Option<String>,
}

fn apply_completed_job_result(
    jobs: &mut HashMap<String, JobRecord>,
    result: &JobResult,
    fees: &FeeSettings,
) {
    let (platform_fee, provider_payout) =
        calculate_fee(fees.job_price_units, fees.fee_bps);
    let record = jobs.entry(result.job_id.clone()).or_insert_with(|| JobRecord {
        id: result.job_id.clone(),
        prompt: String::new(),
        status: "queued".to_string(),
        provider_id: None,
        result: None,
        fee_breakdown: None,
        error: None,
    });
    record.status = "completed".to_string();
    record.provider_id = Some(result.provider_id.clone());
    record.result = Some(result.clone());
    record.error = None;
    record.fee_breakdown = Some(FeeBreakdown {
        gross_price: fees.job_price_units,
        platform_fee,
        provider_payout,
    });
}

fn fail_running_jobs_for_provider(
    jobs: &mut HashMap<String, JobRecord>,
    provider_id: &str,
    error: &str,
) {
    for record in jobs.values_mut() {
        if record.status == "running"
            && record.provider_id.as_deref() == Some(provider_id)
        {
            record.status = "failed".to_string();
            record.error = Some(error.to_string());
            append_line(
                "failed_jobs.log",
                &format!(
                    "timestamp={} job_id={} provider_id={} error={}",
                    timestamp_millis(),
                    record.id,
                    provider_id,
                    error
                ),
            );
        }
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct AcceptedResponse {
    status: String,
    job_id: String,
}

/// POST /request-job: require X-Payment, then enqueue a valid JobPayload.
async fn request_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(job): Json<JobPayload>,
) -> impl IntoResponse {
    // Header presence is intentionally treated as the entire mock payment
    // check. This prototype does not validate currency or perform any payment.
    if !headers.contains_key("x-payment") {
        let body = ErrorResponse {
            error: "Payment required: include X-Payment".to_string(),
        };
        return (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response();
    }

    state.jobs.lock().await.insert(
        job.id.clone(),
        JobRecord {
            id: job.id.clone(),
            prompt: job.prompt.clone(),
            status: "queued".to_string(),
            provider_id: None,
            result: None,
            fee_breakdown: None,
            error: None,
        },
    );
    state.pending_jobs.lock().await.push_back(job.id.clone());
    assign_pending_job(state.clone()).await;

    let body = AcceptedResponse {
        status: "accepted".to_string(),
        job_id: job.id,
    };
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

async fn assign_pending_job(state: AppState) {
    let _assignment = state.assignment_lock.lock().await;
    loop {
        let job_id = {
            let pending = state.pending_jobs.lock().await;
            pending.front().cloned()
        };
        let Some(job_id) = job_id else {
            break;
        };

        let idle_provider = {
            let providers = state.providers.lock().await;
            providers
                .iter()
                .find(|(_, provider)| provider.status == "idle")
                .map(|(provider_id, _)| provider_id.clone())
        };
        let Some(provider_id) = idle_provider else {
            break;
        };
        let sender = {
            let sender_map = state.provider_senders.lock().await;
            sender_map.get(&provider_id).map(|(_, sender)| sender.clone())
        };
        let Some(sender) = sender else {
            break;
        };

        let job = {
            let jobs = state.jobs.lock().await;
            jobs.get(&job_id).map(|record| JobPayload {
                id: record.id.clone(),
                prompt: record.prompt.clone(),
            })
        };
        let Some(job) = job else {
            state.pending_jobs.lock().await.pop_front();
            continue;
        };

        if sender.send(job).is_err() {
            state.pending_jobs.lock().await.pop_front();
            continue;
        }
        state.pending_jobs.lock().await.pop_front();

        if let Some(provider) = state.providers.lock().await.get_mut(&provider_id) {
            provider.status = "busy".to_string();
        }
        if let Some(record) = state.jobs.lock().await.get_mut(&job_id) {
            record.status = "running".to_string();
            record.provider_id = Some(provider_id.clone());
        }

        let timeout_state = state.clone();
        let timeout_job_id = job_id.clone();
        let timeout_provider_id = provider_id;
        tokio::spawn(async move {
            let timeout_secs = timeout_state.job_timeout_secs;
            tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
            let mut jobs = timeout_state.jobs.lock().await;
            if let Some(record) = jobs.get_mut(&timeout_job_id) {
                if record.status == "running"
                    && record.provider_id.as_deref() == Some(&timeout_provider_id)
                {
                    record.status = "failed".to_string();
                    record.error =
                        Some(format!("Job timed out after {timeout_secs} seconds"));
                    append_line(
                        "failed_jobs.log",
                        &format!(
                            "timestamp={} job_id={} provider_id={} error=Job timed out after {} seconds",
                            timestamp_millis(),
                            timeout_job_id,
                            timeout_provider_id,
                            timeout_secs
                        ),
                    );
                }
            }
            drop(jobs);
            if let Some(provider) = timeout_state
                .providers
                .lock()
                .await
                .get_mut(&timeout_provider_id)
            {
                provider.status = "idle".to_string();
            }
        });
    }
}

async fn get_job(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.jobs.lock().await.get(&job_id).cloned() {
        Some(job) => (StatusCode::OK, Json(job)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Job not found".to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_job_fallback(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    let path = uri.path();
    let Some(job_id) = path.strip_prefix("/jobs/") else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Route not found".to_string(),
            }),
        )
            .into_response();
    };
    get_job(State(state), axum::extract::Path(job_id.to_string()))
        .await
        .into_response()
}

async fn list_providers(State(state): State<AppState>) -> impl IntoResponse {
    let providers = state.providers.lock().await;
    let mut providers: Vec<ProviderInfo> = providers.values().cloned().collect();
    providers.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Json(providers)
}

async fn get_fees(State(state): State<AppState>) -> impl IntoResponse {
    let platform_share_percent = state.fees.fee_bps / 100;
    Json(FeesResponse {
        fee_bps: state.fees.fee_bps,
        provider_share_percent: 100 - platform_share_percent,
        platform_share_percent,
    })
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn append_line(path: &str, line: &str) {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut file) => {
            if let Err(error) = writeln!(file, "{line}") {
                eprintln!("Failed to append {path}: {error}");
            }
        }
        Err(error) => eprintln!("Failed to open {path}: {error}"),
    }
}

/// GET /ws: upgrade the HTTP connection to a WebSocket.
async fn ws_upgrade(
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_websocket(socket, state))
}

/// Handle the lifetime of one provider WebSocket connection.
async fn handle_websocket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let active_provider_id = Arc::new(Mutex::new(None::<String>));
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<JobPayload>();
    // Closing this watch channel tells the send task to stop if the provider
    // disappears or the receive side of the socket closes.
    let (close_tx, close_rx) = watch::channel(false);

    // This task waits for jobs from the HTTP handler and sends each job as
    // a JSON text WebSocket message.
    let send_close_tx = close_tx.clone();
    let receive_close_tx = close_tx.clone();
    let send_providers = state.providers.clone();
    let send_provider_id = active_provider_id.clone();
    let send_fees = state.fees.clone();
    let send_task = tokio::spawn(async move {
        let mut close_rx = close_rx;
        let mut keepalive = tokio::time::interval(Duration::from_secs(30));
        #[allow(clippy::while_let_loop)]
        if let Ok(json) = serde_json::to_string(&WsMessage::FeeConfig(SharedFeeConfig {
            fee_bps: send_fees.fee_bps,
            job_price_units: send_fees.job_price_units,
            provider_payout_units: calculate_fee(
                send_fees.job_price_units,
                send_fees.fee_bps,
            )
            .1,
        })) {
            if sender.send(Message::Text(json)).await.is_err() {
                let _ = send_close_tx.send(true);
                return;
            }
        }
        loop {
            let job = match tokio::select! {
                changed = close_rx.changed() => {
                    let _ = changed;
                    break;
                }
                received = job_rx.recv() => received,
                _ = keepalive.tick() => {
                    if sender.send(Message::Ping(Vec::new())).await.is_err() {
                        let _ = send_close_tx.send(true);
                        break;
                    }
                    continue;
                },
            } {
                Some(job) => job,
                None => break,
            };

            match serde_json::to_string(&WsMessage::Job(job)) {
                Ok(json) => {
                    if sender.send(Message::Text(json)).await.is_err() {
                        let _ = send_close_tx.send(true);
                        break;
                    }
                    if let Some(provider_id) = send_provider_id.lock().await.clone() {
                        if let Some(provider) = send_providers.lock().await.get_mut(&provider_id) {
                            provider.status = "busy".to_string();
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Failed to serialize job: {error}");
                }
            }
        }
        // The WebSocket stream ended even when no explicit Close frame was
        // received. Wake the send task so this connection handler can finish.
        let _ = send_close_tx.send(true);
    });

    // This task receives provider results and prints them for the MVP demo.
    let receive_providers = state.providers.clone();
    let receive_provider_id = active_provider_id.clone();
    let receive_fees = state.fees.clone();
    let receive_state = state.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(message) = receiver.next().await {
            match message {
                Ok(Message::Text(json)) => match serde_json::from_str::<WsMessage>(&json) {
                    Ok(WsMessage::Result(result)) => {
                        println!("Received JobResult: {result:?}");
                        if result.error.is_none() {
                            let (platform_fee, provider_payout) =
                                calculate_fee(receive_fees.job_price_units, receive_fees.fee_bps);
                            apply_completed_job_result(
                                &mut *receive_state.jobs.lock().await,
                                &result,
                                &receive_fees,
                            );
                            append_line(
                                "completed_jobs.log",
                                &format!(
                                    "[SUCCESS] Job {} completed by {}. Result: {}",
                                    result.job_id,
                                    result.provider_id,
                                    result.result_data
                                ),
                            );
                            {
                                let mut providers = receive_providers.lock().await;
                                if let Some(provider) = providers.get_mut(&result.provider_id) {
                                    provider.status = "idle".to_string();
                                    provider.jobs_completed = provider.jobs_completed.saturating_add(1);
                                    provider.total_earned_units =
                                        provider
                                            .total_earned_units
                                            .saturating_add(provider_payout);
                                }
                            }
                            assign_pending_job(receive_state.clone()).await;
                            append_line(
                                "earnings.log",
                                &format!(
                                    "timestamp={} job_id={} provider_id={} payout_address={} gross_price={} platform_fee={} provider_payout={} fee_bps={}",
                                    timestamp_millis(),
                                    result.job_id,
                                    result.provider_id,
                                    receive_providers
                                        .lock()
                                        .await
                                        .get(&result.provider_id)
                                        .map(|provider| provider.payout_address.as_str())
                                        .unwrap_or("not-set"),
                                    receive_fees.job_price_units,
                                    platform_fee,
                                    provider_payout,
                                    receive_fees.fee_bps
                                ),
                            );
                        } else {
                            if let Some(record) = receive_state.jobs.lock().await.get_mut(&result.job_id) {
                                record.status = "failed".to_string();
                                record.result = Some(result.clone());
                                record.error = result.error.clone();
                            }
                            if let Some(provider) = receive_providers
                                .lock()
                                .await
                                .get_mut(&result.provider_id)
                            {
                                provider.status = "idle".to_string();
                            }
                            assign_pending_job(receive_state.clone()).await;
                            append_line(
                                "failed_jobs.log",
                                &format!(
                                    "timestamp={} job_id={} provider_id={} error={}",
                                    timestamp_millis(),
                                    result.job_id,
                                    result.provider_id,
                                    result.error.as_deref().unwrap_or("unknown error")
                                ),
                            );
                        }
                    }
                    Ok(WsMessage::Job(job)) => {
                        eprintln!("Unexpected JobPayload from provider: {job:?}");
                    }
                    Ok(WsMessage::Register(registration)) => {
                        let provider_id = registration.provider_id.clone();
                        let duplicate_connection = receive_state
                            .provider_senders
                            .lock()
                            .await
                            .contains_key(&provider_id);
                        if duplicate_connection {
                            eprintln!(
                                "WARNING: provider_id '{}' reconnected with a duplicate ID; replacing the old connection",
                                provider_id
                            );
                            {
                                fail_running_jobs_for_provider(
                                    &mut *receive_state.jobs.lock().await,
                                    &provider_id,
                                    &format!(
                                        "Provider '{}' reconnected with the same provider_id",
                                        provider_id
                                    ),
                                );
                            }
                        }
                        let mut providers = receive_providers.lock().await;
                        let (jobs_completed, total_earned_units) = providers
                            .get(&provider_id)
                            .map(|provider| {
                                (provider.jobs_completed, provider.total_earned_units)
                            })
                            .unwrap_or((0, 0));
                        providers.insert(
                            provider_id.clone(),
                            ProviderInfo {
                                provider_id: provider_id.clone(),
                                payout_address: registration.payout_address,
                                gpu_name: registration.gpu_name,
                                gpu_memory: registration.gpu_memory,
                                model: registration.model,
                                status: "idle".to_string(),
                                jobs_completed,
                                total_earned_units,
                            },
                        );
                        *receive_provider_id.lock().await = Some(provider_id);
                        state
                            .provider_senders
                            .lock()
                            .await
                            .insert(
                                registration.provider_id.clone(),
                                (connection_id, job_tx.clone()),
                            );
                        println!("Registered provider: {}", registration.provider_id);
                        drop(providers);
                        assign_pending_job(state.clone()).await;
                    }
                    Ok(WsMessage::FeeConfig(config)) => {
                        eprintln!("Unexpected fee config from provider: {config:?}");
                    }
                    Err(error) => {
                        eprintln!("Invalid WebSocket JSON: {error}");
                    }
                },
                Ok(Message::Close(frame)) => {
                    if let Some(frame) = frame {
                        println!("Provider disconnected: code={} reason={}", frame.code, frame.reason);
                    } else {
                        println!("Provider disconnected");
                    }
                    if let Some(provider_id) = receive_provider_id.lock().await.clone() {
                        let is_active_connection = receive_state
                            .provider_senders
                            .lock()
                            .await
                            .get(&provider_id)
                            .is_some_and(|(active_id, _)| *active_id == connection_id);
                        if is_active_connection {
                            receive_providers.lock().await.remove(&provider_id);
                            receive_state.provider_senders.lock().await.remove(&provider_id);
                            {
                                fail_running_jobs_for_provider(
                                    &mut *receive_state.jobs.lock().await,
                                    &provider_id,
                                    "Provider disconnected while job was running",
                                );
                            }
                            assign_pending_job(receive_state.clone()).await;
                        }
                    }
                    let _ = receive_close_tx.send(true);
                    break;
                }
                Ok(Message::Ping(payload)) => {
                    // Most WebSocket libraries answer Ping frames automatically,
                    // but this branch documents and observes Ping/Pong frames.
                    println!("Received WebSocket ping ({} bytes)", payload.len());
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Binary(_)) => {
                    eprintln!("Ignoring unexpected binary WebSocket message");
                }
                Err(error) => {
                    eprintln!("WebSocket receive error: {error}");
                    if let Some(provider_id) = receive_provider_id.lock().await.clone() {
                        let is_active_connection = receive_state
                            .provider_senders
                            .lock()
                            .await
                            .get(&provider_id)
                            .is_some_and(|(active_id, _)| *active_id == connection_id);
                        if is_active_connection {
                            receive_providers.lock().await.remove(&provider_id);
                            receive_state.provider_senders.lock().await.remove(&provider_id);
                            {
                                fail_running_jobs_for_provider(
                                    &mut *receive_state.jobs.lock().await,
                                    &provider_id,
                                    "Provider disconnected while job was running",
                                );
                            }
                            assign_pending_job(receive_state.clone()).await;
                        }
                    }
                    let _ = receive_close_tx.send(true);
                    break;
                }
            }
        }
    });

    // Keep the connection task alive until sending or receiving finishes.
    let _ = tokio::join!(send_task, receive_task);
}

#[tokio::main]
async fn main() -> Result<()> {
    let fees = parse_fee_settings()?;
    let job_timeout_secs = std::env::var("JOB_TIMEOUT_SECS")
        .unwrap_or_else(|_| "120".to_string())
        .parse::<u64>()
        .context("JOB_TIMEOUT_SECS must be a positive integer")?;
    if job_timeout_secs == 0 {
        anyhow::bail!("JOB_TIMEOUT_SECS must be a positive integer");
    }
    println!(
        "Platform fee: {} bps; provider receives {} units per {}-unit job; platform address: {}",
        fees.fee_bps,
        calculate_fee(fees.job_price_units, fees.fee_bps).1,
        fees.job_price_units,
        fees.platform_address
    );
    let state = AppState {
        assignment_lock: Arc::new(Mutex::new(())),
        pending_jobs: Arc::new(Mutex::new(VecDeque::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        provider_senders: Arc::new(Mutex::new(HashMap::new())),
        providers: Arc::new(Mutex::new(HashMap::new())),
        fees,
        job_timeout_secs,
    };

    let http_app = Router::new()
        .route("/request-job", post(request_job))
        .route("/providers", get(list_providers))
        .route("/fees", get(get_fees))
        .fallback(get_job_fallback)
        .with_state(state.clone());
    let ws_app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/fees", get(get_fees))
        .with_state(state.clone());

    let http_address: SocketAddr = "0.0.0.0:8080".parse()?;
    let http_listener = tokio::net::TcpListener::bind(http_address)
        .await
        .context("failed to bind coordinator HTTP server to 0.0.0.0:8080")?;
    let ws_address: SocketAddr = "0.0.0.0:8081".parse()?;
    let ws_listener = tokio::net::TcpListener::bind(ws_address)
        .await
        .context("failed to bind coordinator WebSocket server to 0.0.0.0:8081")?;

    println!("Coordinator listening on http://0.0.0.0:8080/request-job");
    println!("Coordinator listening on ws://0.0.0.0:8081/ws");
    let dispatcher_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            assign_pending_job(dispatcher_state.clone()).await;
        }
    });
    let http_task = tokio::spawn(async move {
        axum::serve(http_listener, http_app)
            .await
            .context("coordinator HTTP server stopped unexpectedly")
    });
    let ws_task = tokio::spawn(async move {
        axum::serve(ws_listener, ws_app)
            .await
            .context("coordinator WebSocket server stopped unexpectedly")
    });

    let (http_result, ws_result) = tokio::join!(http_task, ws_task);
    if let Ok(Err(error)) = http_result {
        eprintln!("Coordinator HTTP task stopped: {error}");
    } else if let Err(error) = http_result {
        eprintln!("Coordinator HTTP task failed: {error}");
    }
    if let Ok(Err(error)) = ws_result {
        eprintln!("Coordinator WebSocket task stopped: {error}");
    } else if let Err(error) = ws_result {
        eprintln!("Coordinator WebSocket task panicked or was cancelled: {error}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_completed_job_result, calculate_fee, fail_running_jobs_for_provider,
        assign_pending_job, AppState, FeeSettings, JobPayload, JobRecord, JobResult,
        ProviderInfo,
    };
    use std::{
        collections::{HashMap, VecDeque},
        sync::Arc,
        time::Duration,
    };
    use tokio::sync::{mpsc, Mutex};

    #[test]
    fn fee_is_zero_at_zero_bps() {
        assert_eq!(calculate_fee(10_000, 0), (0, 10_000));
    }

    #[test]
    fn fee_is_ten_percent_at_1000_bps() {
        assert_eq!(calculate_fee(10_000, 1_000), (1_000, 9_000));
    }

    #[test]
    fn fee_is_half_at_5000_bps() {
        assert_eq!(calculate_fee(10_000, 5_000), (5_000, 5_000));
    }

    #[test]
    fn fee_rounds_down_for_non_divisible_prices() {
        assert_eq!(calculate_fee(101, 1_000), (10, 91));
    }

    #[test]
    fn fee_and_provider_payout_sum_to_price() {
        for price in [1, 101, 10_000, u64::MAX / 2] {
            let (fee, provider_payout) = calculate_fee(price, 1_337);
            assert_eq!(fee + provider_payout, price);
        }
    }

    #[tokio::test]
    async fn completed_job_result_is_visible_by_job_id_lookup() {
        let jobs = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = jobs.lock().await;
            map.insert(
                "job-401".to_string(),
                JobRecord {
                    id: "job-401".to_string(),
                    prompt: "Napíš slovenskú básničku".to_string(),
                    status: "queued".to_string(),
                    provider_id: None,
                    result: None,
                    fee_breakdown: None,
                    error: None,
                },
            );
        }

        let result = JobResult {
            job_id: "job-401".to_string(),
            provider_id: "provider-node-01".to_string(),
            result_data: "Hotová slovenská básnička".to_string(),
            duration_ms: 123,
            eval_count: 45,
            tokens_per_sec: 12.5,
            error: None,
        };
        let fees = FeeSettings {
            fee_bps: 1_000,
            job_price_units: 10_000,
            platform_address: "0xPLATFORM_PLACEHOLDER".to_string(),
        };

        apply_completed_job_result(&mut *jobs.lock().await, &result, &fees);

        let found = jobs.lock().await.get("job-401").cloned();
        let found = found.expect("completed job should remain in the same jobs map");
        assert_eq!(found.status, "completed");
        assert_eq!(found.result.unwrap().job_id, "job-401");
        assert_eq!(found.fee_breakdown.unwrap().gross_price, 10_000);
    }

    #[test]
    fn provider_disconnect_marks_running_job_failed() {
        let mut jobs = HashMap::new();
        jobs.insert(
            "job-disconnect".to_string(),
            JobRecord {
                id: "job-disconnect".to_string(),
                prompt: "test prompt".to_string(),
                status: "running".to_string(),
                provider_id: Some("test-provider-A".to_string()),
                result: None,
                fee_breakdown: None,
                error: None,
            },
        );

        fail_running_jobs_for_provider(
            &mut jobs,
            "test-provider-A",
            "Provider disconnected while job was running",
        );

        let record = jobs.get("job-disconnect").unwrap();
        assert_eq!(record.status, "failed");
        assert_eq!(
            record.error.as_deref(),
            Some("Provider disconnected while job was running")
        );
    }

    #[tokio::test]
    async fn concurrent_assignment_sends_queued_job_only_once() {
        let (sender, mut receiver) = mpsc::unbounded_channel::<JobPayload>();
        let state = AppState {
            assignment_lock: Arc::new(Mutex::new(())),
            pending_jobs: Arc::new(Mutex::new(VecDeque::from([
                "job-single-assignment".to_string(),
            ]))),
            jobs: Arc::new(Mutex::new(HashMap::from([(
                "job-single-assignment".to_string(),
                JobRecord {
                    id: "job-single-assignment".to_string(),
                    prompt: "test prompt".to_string(),
                    status: "queued".to_string(),
                    provider_id: None,
                    result: None,
                    fee_breakdown: None,
                    error: None,
                },
            )]))),
            provider_senders: Arc::new(Mutex::new(HashMap::from([(
                "test-provider-A".to_string(),
                (1_u64, sender),
            )]))),
            providers: Arc::new(Mutex::new(HashMap::from([(
                "test-provider-A".to_string(),
                ProviderInfo {
                    provider_id: "test-provider-A".to_string(),
                    payout_address: "0xPLATFORM_PLACEHOLDER".to_string(),
                    gpu_name: "test GPU".to_string(),
                    gpu_memory: "test memory".to_string(),
                    model: "test model".to_string(),
                    status: "idle".to_string(),
                    jobs_completed: 0,
                    total_earned_units: 0,
                },
            )]))),
            fees: FeeSettings {
                fee_bps: 1_000,
                job_price_units: 10_000,
                platform_address: "0xPLATFORM_PLACEHOLDER".to_string(),
            },
            job_timeout_secs: 120,
        };

        let first = tokio::spawn(assign_pending_job(state.clone()));
        let second = tokio::spawn(assign_pending_job(state.clone()));
        let _ = tokio::join!(first, second);

        let delivered = receiver.recv().await.expect("job should be delivered");
        assert_eq!(delivered.id, "job-single-assignment");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.recv())
                .await
                .is_err(),
            "the same queued job must not be delivered twice"
        );
    }
}

