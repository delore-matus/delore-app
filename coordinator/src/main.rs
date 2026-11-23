//! MVP HTTP/WebSocket coordinator.
//!
//! POST /request-job accepts a mock payment header and forwards the job over
//! an mpsc channel to the WebSocket connection handler.

use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
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
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch, Mutex};

mod storage;
use storage::JobStore;

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
type ProviderSenderMap = HashMap<String, (u64, mpsc::UnboundedSender<JobPayload>)>;

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
    /// Durable job state recovered from SQLite on coordinator startup.
    job_store: JobStore,
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
    network_model: String,
}

#[derive(Debug, Serialize)]
struct FeesResponse {
    fee_bps: u64,
    provider_share_percent: u64,
    platform_share_percent: u64,
    fee_address: String,
    network_model: String,
}

fn parse_network_model(value: Option<&str>) -> Result<String> {
    let model = value.unwrap_or("llama3.2:3b");
    if !common::is_valid_model_name(model) {
        anyhow::bail!("GPUSWARM_NETWORK_MODEL is not a valid model name");
    }
    Ok(model.to_string())
}

fn calculate_fee(price_units: u64, fee_bps: u64) -> (u64, u64) {
    let fee = ((price_units as u128 * fee_bps as u128) / 10_000) as u64;
    (fee, price_units - fee)
}

fn validate_fee_address(address: Option<&str>) -> Result<String> {
    let address = address.unwrap_or("").trim();
    if address.is_empty() {
        anyhow::bail!("GPUSWARM_FEE_ADDRESS is missing");
    }
    if !address.starts_with("0x") {
        anyhow::bail!("GPUSWARM_FEE_ADDRESS must start with 0x");
    }
    if address.len() != 42 {
        if address.len() < 42 {
            anyhow::bail!("GPUSWARM_FEE_ADDRESS is too short; expected 40 hex characters");
        }
        anyhow::bail!("GPUSWARM_FEE_ADDRESS is too long; expected 40 hex characters");
    }
    if !address[2..].chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("GPUSWARM_FEE_ADDRESS contains non-hex characters");
    }
    Ok(address.to_string())
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
    let configured_address = std::env::var("GPUSWARM_FEE_ADDRESS").ok();
    let platform_address = match validate_fee_address(configured_address.as_deref()) {
        Ok(address) => address,
        Err(error) => {
            if std::env::var("GPUSWARM_ALLOW_NO_FEE_ADDRESS").as_deref() == Ok("1") {
                eprintln!(
                    "WARNING: TESTNET ONLY - fee address is unavailable ({}); \
                     GPUSWARM_ALLOW_NO_FEE_ADDRESS=1 is in effect",
                    error
                );
                "0x0000000000000000000000000000000000000000".to_string()
            } else {
                return Err(error);
            }
        }
    };
    let network_model = match std::env::var("GPUSWARM_NETWORK_MODEL") {
        Ok(model) => parse_network_model(Some(&model))?,
        Err(std::env::VarError::NotPresent) => parse_network_model(None)?,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("GPUSWARM_NETWORK_MODEL must contain valid Unicode")
        }
    };

    Ok(FeeSettings {
        fee_bps,
        job_price_units,
        platform_address,
        network_model,
    })
}

fn configured_port(name: &str, default: u16) -> Result<u16> {
    let value = std::env::var(name).ok();
    parse_configured_port(name, value.as_deref(), default)
}

fn parse_configured_port(name: &str, value: Option<&str>, default: u16) -> Result<u16> {
    match value {
        Some(value) => value
            .parse::<u16>()
            .with_context(|| format!("{name} must be a port number between 0 and 65535")),
        None => Ok(default),
    }
}

fn configured_database_path(value: Option<PathBuf>) -> Result<PathBuf> {
    let path = value.unwrap_or_else(|| PathBuf::from("coordinator.sqlite3"));
    if path.as_os_str().is_empty() {
        anyhow::bail!("GPUSWARM_DB_PATH cannot be empty");
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize)]
struct ProviderInfo {
    provider_id: String,
    payout_address: String,
    gpu_name: String,
    gpu_memory: String,
    model: String,
    status: String,
    accepting_jobs: bool,
    jobs_completed: u64,
    total_earned_units: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeeBreakdown {
    gross_price: u64,
    platform_fee: u64,
    provider_payout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let (platform_fee, provider_payout) = calculate_fee(fees.job_price_units, fees.fee_bps);
    let record = jobs
        .entry(result.job_id.clone())
        .or_insert_with(|| JobRecord {
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
) -> Vec<JobRecord> {
    let mut failed_jobs = Vec::new();
    for record in jobs.values_mut() {
        if record.status == "running" && record.provider_id.as_deref() == Some(provider_id) {
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
            failed_jobs.push(record.clone());
        }
    }
    failed_jobs
}

async fn persist_job_records(state: &AppState, records: Vec<JobRecord>) {
    for record in records {
        if let Err(error) = state.job_store.save_job(record.clone()).await {
            eprintln!("Failed to persist job {}: {error:#}", record.id);
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

    let record = JobRecord {
        id: job.id.clone(),
        prompt: job.prompt.clone(),
        status: "queued".to_string(),
        provider_id: None,
        result: None,
        fee_breakdown: None,
        error: None,
    };
    if let Err(error) = state.job_store.save_job(record.clone()).await {
        eprintln!("Failed to persist queued job {}: {error:#}", job.id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Coordinator could not store the job".to_string(),
            }),
        )
            .into_response();
    }
    state.jobs.lock().await.insert(job.id.clone(), record);
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
                .find(|(_, provider)| provider.status == "idle" && provider.accepting_jobs)
                .map(|(provider_id, _)| provider_id.clone())
        };
        let Some(provider_id) = idle_provider else {
            break;
        };
        let sender = {
            let sender_map = state.provider_senders.lock().await;
            sender_map
                .get(&provider_id)
                .map(|(_, sender)| sender.clone())
        };
        let Some(sender) = sender else {
            break;
        };

        let record = {
            let jobs = state.jobs.lock().await;
            jobs.get(&job_id).cloned()
        };
        let Some(record) = record else {
            state.pending_jobs.lock().await.pop_front();
            continue;
        };

        let job = JobPayload {
            id: record.id.clone(),
            prompt: record.prompt.clone(),
        };
        let mut running_record = record.clone();
        running_record.status = "running".to_string();
        running_record.provider_id = Some(provider_id.clone());
        if let Err(error) = state.job_store.save_job(running_record.clone()).await {
            eprintln!("Failed to persist running job {job_id}: {error:#}");
            break;
        }
        if sender.send(job).is_err() {
            state.pending_jobs.lock().await.pop_front();
            if let Err(error) = state.job_store.save_job(record).await {
                eprintln!("Failed to restore queued job {job_id}: {error:#}");
            }
            continue;
        }
        state.pending_jobs.lock().await.pop_front();

        if let Some(provider) = state.providers.lock().await.get_mut(&provider_id) {
            provider.status = "busy".to_string();
        }
        state
            .jobs
            .lock()
            .await
            .insert(job_id.clone(), running_record);

        let timeout_state = state.clone();
        let timeout_job_id = job_id.clone();
        let timeout_provider_id = provider_id;
        tokio::spawn(async move {
            let timeout_secs = timeout_state.job_timeout_secs;
            tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
            let mut jobs = timeout_state.jobs.lock().await;
            let mut failed_job = None;
            if let Some(record) = jobs.get_mut(&timeout_job_id) {
                if record.status == "running"
                    && record.provider_id.as_deref() == Some(&timeout_provider_id)
                {
                    record.status = "failed".to_string();
                    record.error = Some(format!("Job timed out after {timeout_secs} seconds"));
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
                    failed_job = Some(record.clone());
                }
            }
            drop(jobs);
            if let Some(job) = failed_job {
                if let Err(error) = timeout_state.job_store.save_job(job).await {
                    eprintln!("Failed to persist timed-out job {timeout_job_id}: {error:#}");
                }
            }
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
        fee_address: state.fees.platform_address.clone(),
        network_model: state.fees.network_model.clone(),
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
async fn ws_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
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
            provider_payout_units: calculate_fee(send_fees.job_price_units, send_fees.fee_bps).1,
            fee_address: send_fees.platform_address.clone(),
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
                            let completed_job = {
                                let mut jobs = receive_state.jobs.lock().await;
                                apply_completed_job_result(&mut jobs, &result, &receive_fees);
                                jobs.get(&result.job_id).cloned()
                            };
                            if let Some(job) = completed_job {
                                if let Err(error) = receive_state.job_store.save_job(job).await {
                                    eprintln!(
                                        "Failed to persist completed job {}: {error:#}",
                                        result.job_id
                                    );
                                }
                            }
                            append_line(
                                "completed_jobs.log",
                                &format!(
                                    "[SUCCESS] Job {} completed by {}. Result: {}",
                                    result.job_id, result.provider_id, result.result_data
                                ),
                            );
                            {
                                let mut providers = receive_providers.lock().await;
                                if let Some(provider) = providers.get_mut(&result.provider_id) {
                                    provider.status = if provider.accepting_jobs {
                                        "idle".to_string()
                                    } else {
                                        "paused".to_string()
                                    };
                                    provider.jobs_completed =
                                        provider.jobs_completed.saturating_add(1);
                                    provider.total_earned_units =
                                        provider.total_earned_units.saturating_add(provider_payout);
                                }
                            }
                            assign_pending_job(receive_state.clone()).await;
                            append_line(
                                "earnings.log",
                                &format!(
                                    "timestamp={} job_id={} provider_id={} payout_address={} gross_price={} platform_fee={} provider_payout={} fee_bps={} fee_address={} fee_amount={}",
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
                                    receive_fees.fee_bps,
                                    receive_fees.platform_address,
                                    platform_fee
                                ),
                            );
                        } else {
                            let failed_job = {
                                let mut jobs = receive_state.jobs.lock().await;
                                if let Some(record) = jobs.get_mut(&result.job_id) {
                                    record.status = "failed".to_string();
                                    record.result = Some(result.clone());
                                    record.error = result.error.clone();
                                    Some(record.clone())
                                } else {
                                    None
                                }
                            };
                            if let Some(job) = failed_job {
                                if let Err(error) = receive_state.job_store.save_job(job).await {
                                    eprintln!(
                                        "Failed to persist failed job {}: {error:#}",
                                        result.job_id
                                    );
                                }
                            }
                            if let Some(provider) =
                                receive_providers.lock().await.get_mut(&result.provider_id)
                            {
                                provider.status = if provider.accepting_jobs {
                                    "idle".to_string()
                                } else {
                                    "paused".to_string()
                                };
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
                            let failed_jobs = {
                                let mut jobs = receive_state.jobs.lock().await;
                                fail_running_jobs_for_provider(
                                    &mut jobs,
                                    &provider_id,
                                    &format!(
                                        "Provider '{}' reconnected with the same provider_id",
                                        provider_id
                                    ),
                                )
                            };
                            persist_job_records(&receive_state, failed_jobs).await;
                        }
                        let mut providers = receive_providers.lock().await;
                        let (jobs_completed, total_earned_units) = providers
                            .get(&provider_id)
                            .map(|provider| (provider.jobs_completed, provider.total_earned_units))
                            .unwrap_or((0, 0));
                        providers.insert(
                            provider_id.clone(),
                            ProviderInfo {
                                provider_id: provider_id.clone(),
                                payout_address: registration.payout_address,
                                gpu_name: registration.gpu_name,
                                gpu_memory: registration.gpu_memory,
                                model: registration.model,
                                status: if registration.accepting_jobs {
                                    "idle".to_string()
                                } else {
                                    "paused".to_string()
                                },
                                accepting_jobs: registration.accepting_jobs,
                                jobs_completed,
                                total_earned_units,
                            },
                        );
                        *receive_provider_id.lock().await = Some(provider_id);
                        state.provider_senders.lock().await.insert(
                            registration.provider_id.clone(),
                            (connection_id, job_tx.clone()),
                        );
                        println!("Registered provider: {}", registration.provider_id);
                        drop(providers);
                        assign_pending_job(state.clone()).await;
                    }
                    Ok(WsMessage::Availability { accepting_jobs }) => {
                        let provider_id = receive_provider_id.lock().await.clone();
                        if let Some(provider_id) = provider_id {
                            let is_active_connection = receive_state
                                .provider_senders
                                .lock()
                                .await
                                .get(&provider_id)
                                .is_some_and(|(active_id, _)| *active_id == connection_id);
                            if is_active_connection {
                                if let Some(provider) =
                                    receive_providers.lock().await.get_mut(&provider_id)
                                {
                                    provider.accepting_jobs = accepting_jobs;
                                    if provider.status != "busy" {
                                        provider.status = if accepting_jobs {
                                            "idle".to_string()
                                        } else {
                                            "paused".to_string()
                                        };
                                    }
                                }
                                if accepting_jobs {
                                    assign_pending_job(receive_state.clone()).await;
                                }
                            }
                        }
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
                        println!(
                            "Provider disconnected: code={} reason={}",
                            frame.code, frame.reason
                        );
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
                            receive_state
                                .provider_senders
                                .lock()
                                .await
                                .remove(&provider_id);
                            let failed_jobs = {
                                let mut jobs = receive_state.jobs.lock().await;
                                fail_running_jobs_for_provider(
                                    &mut jobs,
                                    &provider_id,
                                    "Provider disconnected while job was running",
                                )
                            };
                            persist_job_records(&receive_state, failed_jobs).await;
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
                            receive_state
                                .provider_senders
                                .lock()
                                .await
                                .remove(&provider_id);
                            let failed_jobs = {
                                let mut jobs = receive_state.jobs.lock().await;
                                fail_running_jobs_for_provider(
                                    &mut jobs,
                                    &provider_id,
                                    "Provider disconnected while job was running",
                                )
                            };
                            persist_job_records(&receive_state, failed_jobs).await;
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
    let database_path =
        configured_database_path(std::env::var_os("GPUSWARM_DB_PATH").map(PathBuf::from))?;
    let job_store = JobStore::open(&database_path)?;
    let recovered_jobs = job_store.load_and_recover()?;
    let pending_jobs = recovered_jobs
        .iter()
        .filter(|job| job.status == "queued")
        .map(|job| job.id.clone())
        .collect::<VecDeque<_>>();
    let jobs = recovered_jobs
        .into_iter()
        .map(|job| (job.id.clone(), job))
        .collect::<HashMap<_, _>>();
    println!(
        "Coordinator database: {} ({} jobs loaded; {} queued)",
        database_path.display(),
        jobs.len(),
        pending_jobs.len()
    );
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
        pending_jobs: Arc::new(Mutex::new(pending_jobs)),
        jobs: Arc::new(Mutex::new(jobs)),
        job_store,
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

    let http_port = configured_port("GPUSWARM_HTTP_PORT", 8080)?;
    let http_address = SocketAddr::from(([0, 0, 0, 0], http_port));
    let http_listener = tokio::net::TcpListener::bind(http_address)
        .await
        .with_context(|| format!("failed to bind coordinator HTTP server to {http_address}"))?;
    let ws_port = configured_port("GPUSWARM_WS_PORT", 8081)?;
    let ws_address = SocketAddr::from(([0, 0, 0, 0], ws_port));
    let ws_listener = tokio::net::TcpListener::bind(ws_address)
        .await
        .with_context(|| format!("failed to bind coordinator WebSocket server to {ws_address}"))?;

    println!(
        "Coordinator listening on http://{}/request-job",
        http_listener.local_addr()?
    );
    println!(
        "Coordinator listening on ws://{}/ws",
        ws_listener.local_addr()?
    );
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
    use super::storage::JobStore;
    use super::{
        apply_completed_job_result, assign_pending_job, calculate_fee, configured_database_path,
        fail_running_jobs_for_provider, parse_configured_port, parse_network_model,
        validate_fee_address, AppState, FeeSettings, JobPayload, JobRecord, JobResult,
        ProviderInfo,
    };
    use std::{
        collections::{HashMap, VecDeque},
        path::PathBuf,
        sync::Arc,
        time::Duration,
    };
    use tokio::sync::{mpsc, Mutex};

    #[test]
    fn network_model_defaults_and_rejects_invalid_values() {
        assert_eq!(parse_network_model(None).unwrap(), "llama3.2:3b");
        assert_eq!(
            parse_network_model(Some("llama3.2:1b")).unwrap(),
            "llama3.2:1b"
        );
        assert!(parse_network_model(Some("../invalid")).is_err());
    }

    #[test]
    fn coordinator_database_path_has_a_safe_default_and_rejects_empty_values() {
        assert_eq!(
            configured_database_path(None).unwrap(),
            PathBuf::from("coordinator.sqlite3")
        );
        assert_eq!(
            configured_database_path(Some(PathBuf::from("custom.sqlite3"))).unwrap(),
            PathBuf::from("custom.sqlite3")
        );
        assert!(configured_database_path(Some(PathBuf::new())).is_err());
    }

    #[test]
    fn configured_ports_support_defaults_and_ephemeral_ports() {
        assert_eq!(parse_configured_port("PORT", None, 8080).unwrap(), 8080);
        assert_eq!(parse_configured_port("PORT", Some("0"), 8080).unwrap(), 0);
        assert_eq!(
            parse_configured_port("PORT", Some("65535"), 8080).unwrap(),
            65535
        );
        assert!(parse_configured_port("PORT", Some("65536"), 8080).is_err());
        assert!(parse_configured_port("PORT", Some("invalid"), 8080).is_err());
    }

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
            network_model: "llama3.2:3b".to_string(),
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

    #[test]
    fn fee_address_accepts_valid_address() {
        let address = "0x1111111111111111111111111111111111111111";
        assert_eq!(validate_fee_address(Some(address)).unwrap(), address);
    }

    #[test]
    fn fee_address_rejects_empty_address() {
        assert!(validate_fee_address(None).is_err());
        assert!(validate_fee_address(Some("")).is_err());
    }

    #[test]
    fn fee_address_rejects_missing_0x_prefix() {
        assert!(validate_fee_address(Some("1111111111111111111111111111111111111111")).is_err());
    }

    #[test]
    fn fee_address_rejects_too_short_address() {
        assert!(validate_fee_address(Some("0x1234")).is_err());
    }

    #[test]
    fn fee_address_rejects_too_long_address() {
        assert!(
            validate_fee_address(Some("0x11111111111111111111111111111111111111111111")).is_err()
        );
    }

    #[test]
    fn fee_address_rejects_non_hex_characters() {
        assert!(validate_fee_address(Some("0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")).is_err());
    }

    #[tokio::test]
    async fn concurrent_assignment_sends_queued_job_only_once() {
        let (sender, mut receiver) = mpsc::unbounded_channel::<JobPayload>();
        let state = AppState {
            assignment_lock: Arc::new(Mutex::new(())),
            pending_jobs: Arc::new(Mutex::new(VecDeque::from([
                "job-single-assignment".to_string()
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
            job_store: JobStore::in_memory().unwrap(),
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
                    status: "paused".to_string(),
                    accepting_jobs: false,
                    jobs_completed: 0,
                    total_earned_units: 0,
                },
            )]))),
            fees: FeeSettings {
                fee_bps: 1_000,
                job_price_units: 10_000,
                platform_address: "0xPLATFORM_PLACEHOLDER".to_string(),
                network_model: "llama3.2:3b".to_string(),
            },
            job_timeout_secs: 120,
        };

        assign_pending_job(state.clone()).await;
        assert!(receiver.try_recv().is_err());
        assert_eq!(state.pending_jobs.lock().await.len(), 1);
        assert_eq!(
            state.jobs.lock().await["job-single-assignment"].status,
            "queued"
        );
        {
            let mut providers = state.providers.lock().await;
            let provider = providers.get_mut("test-provider-A").unwrap();
            provider.accepting_jobs = true;
            provider.status = "idle".to_string();
        }

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
