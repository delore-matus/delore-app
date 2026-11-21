use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use provider_core::{
    detect_gpu, load_config, run_provider_managed, runtime_suitability, FeeConfig, GpuInfo,
    ProviderConfig, ProviderEvent, ProviderEventCallback, RuntimeDownloadPhase, RuntimeManager,
    RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
};
use serde::Serialize;
use tauri::{Emitter, Manager, State, WindowEvent};
use tokio::sync::{watch, Mutex as AsyncMutex};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CurrentJob {
    Idle,
    Running {
        job_id: String,
    },
    Finished {
        job_id: String,
        result_data: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct StatusSnapshot {
    provider_running: bool,
    runtime: RuntimeState,
    runtime_suitability: RuntimeSuitability,
    gpu: GpuInfo,
    connection: provider_core::ConnectionState,
    fee_config: Option<FeeConfig>,
    current_job: CurrentJob,
    payout_address: Option<String>,
    coordinator_ws: String,
    model: String,
    runtime_percent: u8,
    model_percent: u8,
    expected_total_download_bytes: Option<u64>,
    error_message: Option<String>,
}

struct ProviderTask {
    stop: watch::Sender<bool>,
    join: tauri::async_runtime::JoinHandle<()>,
}

struct StateInner {
    snapshot: Mutex<StatusSnapshot>,
    task: AsyncMutex<Option<ProviderTask>>,
    confirmation_granted: Mutex<bool>,
    close_started: AtomicBool,
    allow_window_close: AtomicBool,
}

#[derive(Clone)]
struct DesktopState {
    inner: Arc<StateInner>,
}

impl DesktopState {
    fn new() -> Self {
        let config_result = load_config();
        let config = match &config_result {
            Ok(config) => config.clone().unwrap_or_else(ProviderConfig::defaults),
            Err(_) => ProviderConfig::defaults(),
        };
        let gpu = detect_gpu();
        let runtime = match RuntimeManager::new() {
            Ok(manager) if manager.runtime_dir().exists() => match manager.executable_path() {
                Ok(_) => RuntimeState::Ready,
                Err(error) => RuntimeState::Error {
                    message: error.to_string(),
                },
            },
            Ok(_) => RuntimeState::NotInstalled,
            Err(error) => RuntimeState::Error {
                message: error.to_string(),
            },
        };
        let error_message = config_result
            .err()
            .map(|error| format!("Could not load provider configuration: {error:#}"));
        let runtime_error = match &runtime {
            RuntimeState::Error { message } => Some(message.clone()),
            _ => None,
        };
        let snapshot = StatusSnapshot {
            provider_running: false,
            runtime,
            runtime_suitability: runtime_suitability(),
            gpu,
            connection: provider_core::ConnectionState::Disconnected,
            fee_config: None,
            current_job: CurrentJob::Idle,
            payout_address: config.payout_address,
            coordinator_ws: config.coordinator_ws,
            model: config.ollama_model,
            runtime_percent: 0,
            model_percent: 0,
            expected_total_download_bytes: None,
            error_message: error_message.or(runtime_error),
        };
        Self {
            inner: Arc::new(StateInner {
                snapshot: Mutex::new(snapshot),
                task: AsyncMutex::new(None),
                confirmation_granted: Mutex::new(false),
                close_started: AtomicBool::new(false),
                allow_window_close: AtomicBool::new(false),
            }),
        }
    }
}

#[tauri::command]
fn get_status(state: State<'_, DesktopState>) -> Result<StatusSnapshot, String> {
    state
        .inner()
        .inner
        .snapshot
        .lock()
        .map(|snapshot| snapshot.clone())
        .map_err(|error| format!("Provider status is unavailable: {error}"))
}

#[tauri::command]
fn confirm_runtime_download(state: State<'_, DesktopState>) -> Result<(), String> {
    if let RuntimeSuitability::NotRecommended { .. } = runtime_suitability() {
        let mut granted = state
            .inner()
            .inner
            .confirmation_granted
            .lock()
            .map_err(|error| format!("Could not record runtime confirmation: {error}"))?;
        *granted = true;
    }
    Ok(())
}

#[tauri::command]
async fn start_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let config = load_config()
        .map_err(|error| format!("Could not load provider configuration: {error:#}"))?
        .unwrap_or_else(ProviderConfig::defaults);
    let runtime_manager =
        RuntimeManager::new().map_err(|error| format!("Runtime paths unavailable: {error:#}"))?;
    let requires_confirmation = !runtime_manager.runtime_dir().exists()
        && matches!(
            runtime_suitability(),
            RuntimeSuitability::NotRecommended { .. }
        );
    let confirmed = {
        let mut granted = state
            .inner()
            .inner
            .confirmation_granted
            .lock()
            .map_err(|error| format!("Could not check runtime confirmation: {error}"))?;
        if requires_confirmation && !*granted {
            return Err(
                "Runtime preparation on this hardware requires explicit confirmation.".into(),
            );
        }
        std::mem::take(&mut *granted)
    };

    let mut task_slot = state.inner().inner.task.lock().await;
    if task_slot
        .as_ref()
        .is_some_and(|task| !task.join.inner().is_finished())
    {
        return Err("Provider is already running.".into());
    }

    {
        let mut snapshot = state
            .inner()
            .inner
            .snapshot
            .lock()
            .map_err(|error| format!("Provider status is unavailable: {error}"))?;
        snapshot.provider_running = true;
        snapshot.current_job = CurrentJob::Idle;
        snapshot.error_message = None;
        snapshot.coordinator_ws = config.coordinator_ws.clone();
        snapshot.model = config.ollama_model.clone();
        snapshot.runtime_percent = 0;
        snapshot.model_percent = 0;
        snapshot.expected_total_download_bytes = None;
    }
    emit_status(&app, state.inner());

    let (stop, stop_rx) = watch::channel(false);
    let runtime_state = state.inner().inner.clone();
    let runtime_app = app.clone();
    let runtime_callback: RuntimeProgressCallback = Arc::new(move |runtime| {
        update_status(&runtime_state, &runtime_app, |snapshot| {
            if let RuntimeState::Downloading {
                phase,
                percent,
                expected_total_download_bytes,
                ..
            } = &runtime
            {
                match phase {
                    RuntimeDownloadPhase::Runtime => snapshot.runtime_percent = *percent,
                    RuntimeDownloadPhase::Model => snapshot.model_percent = *percent,
                }
                if *expected_total_download_bytes > 0 {
                    snapshot.expected_total_download_bytes = Some(*expected_total_download_bytes);
                }
            }
            if let RuntimeState::Error { message } = &runtime {
                snapshot.error_message = Some(message.clone());
            }
            snapshot.runtime = runtime;
        });
    });

    let event_state = state.inner().inner.clone();
    let event_app = app.clone();
    let event_callback: ProviderEventCallback = Arc::new(move |event| {
        update_status(&event_state, &event_app, |snapshot| match event {
            ProviderEvent::Connection { state, message } => {
                snapshot.connection = state;
                if let Some(message) = message {
                    snapshot.error_message = Some(message);
                }
            }
            ProviderEvent::FeeConfig { config } => snapshot.fee_config = Some(config),
            ProviderEvent::JobStarted { job_id } => {
                snapshot.current_job = CurrentJob::Running { job_id }
            }
            ProviderEvent::JobFinished { result } => {
                snapshot.current_job = CurrentJob::Finished {
                    job_id: result.job_id,
                    result_data: result.result_data,
                    error: result.error,
                }
            }
            ProviderEvent::Error { message } => snapshot.error_message = Some(message),
        });
    });

    let task_state = state.inner().inner.clone();
    let task_app = app.clone();
    let join = tauri::async_runtime::spawn(async move {
        let result =
            run_provider_managed(config, confirmed, stop_rx, runtime_callback, event_callback)
                .await;
        update_status(&task_state, &task_app, |snapshot| {
            snapshot.provider_running = false;
            snapshot.connection = provider_core::ConnectionState::Disconnected;
            if matches!(&snapshot.current_job, CurrentJob::Running { .. }) {
                snapshot.current_job = CurrentJob::Idle;
            }
            if matches!(&snapshot.runtime, RuntimeState::Stopping) {
                snapshot.runtime = match RuntimeManager::new() {
                    Ok(manager) if manager.runtime_dir().exists() => RuntimeState::Ready,
                    Ok(_) => RuntimeState::NotInstalled,
                    Err(error) => RuntimeState::Error {
                        message: error.to_string(),
                    },
                };
                snapshot.runtime_percent = 0;
                snapshot.model_percent = 0;
                snapshot.expected_total_download_bytes = None;
            }
            if let Err(error) = result {
                snapshot.error_message = Some(format!("{error:#}"));
            }
        });
    });
    *task_slot = Some(ProviderTask { stop, join });
    Ok(())
}

#[tauri::command]
async fn stop_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    stop_provider_task(&app, state.inner()).await
}

#[tauri::command]
fn set_payout_address(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    address: String,
) -> Result<StatusSnapshot, String> {
    let config =
        provider_core::set_payout_address(&address).map_err(|error| format!("{error:#}"))?;
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.payout_address = config.payout_address;
        snapshot.coordinator_ws = config.coordinator_ws;
        snapshot.model = config.ollama_model;
        snapshot.error_message = None;
    });
    get_status(state)
}

async fn stop_provider_task(app: &tauri::AppHandle, state: &DesktopState) -> Result<(), String> {
    let mut task_slot = state.inner.task.lock().await;
    let result = if let Some(task) = task_slot.as_mut() {
        let _ = task.stop.send(true);
        Some((&mut task.join).await)
    } else {
        None
    };
    *task_slot = None;
    drop(task_slot);
    if let Some(Err(error)) = result {
        return Err(format!("Provider shutdown task failed: {error}"));
    }
    update_status(&state.inner, app, |snapshot| {
        snapshot.provider_running = false;
        snapshot.connection = provider_core::ConnectionState::Disconnected;
        if matches!(&snapshot.current_job, CurrentJob::Running { .. }) {
            snapshot.current_job = CurrentJob::Idle;
        }
    });
    Ok(())
}

fn update_status(
    state: &Arc<StateInner>,
    app: &tauri::AppHandle,
    update: impl FnOnce(&mut StatusSnapshot),
) {
    match state.snapshot.lock() {
        Ok(mut snapshot) => {
            update(&mut snapshot);
            let snapshot = snapshot.clone();
            if let Err(error) = app.emit("provider-status", snapshot) {
                eprintln!("Could not emit provider status: {error}");
            }
        }
        Err(error) => eprintln!("Provider status lock is unavailable: {error}"),
    }
}

fn emit_status(app: &tauri::AppHandle, state: &DesktopState) {
    match state.inner.snapshot.lock() {
        Ok(snapshot) => {
            let snapshot = snapshot.clone();
            if let Err(error) = app.emit("provider-status", snapshot) {
                eprintln!("Could not emit provider status: {error}");
            }
        }
        Err(error) => eprintln!("{error}"),
    }
}

fn main() {
    tauri::Builder::default()
        .manage(DesktopState::new())
        .invoke_handler(tauri::generate_handler![
            get_status,
            start_provider,
            stop_provider,
            confirm_runtime_download,
            set_payout_address
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<DesktopState>().inner().clone();
                if !state.inner.allow_window_close.load(Ordering::Acquire) {
                    api.prevent_close();
                    if !state.inner.close_started.swap(true, Ordering::AcqRel) {
                        let app = window.app_handle().clone();
                        let closing_window = window.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(error) = stop_provider_task(&app, &state).await {
                                eprintln!("Provider shutdown on window close failed: {error}");
                            }
                            state
                                .inner
                                .allow_window_close
                                .store(true, Ordering::Release);
                            if let Err(error) = closing_window.close() {
                                eprintln!("Could not close the app window: {error}");
                            }
                        });
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running DeLore");
}
