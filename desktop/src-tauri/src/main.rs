use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use provider_core::{
    confirm_not_recommended_runtime, detect_gpu, effective_ollama_model, load_config,
    provider_control_channel, run_provider_managed_with_control, runtime_suitability,
    set_ollama_model as save_ollama_model, set_resource_settings as save_resource_settings,
    FeeConfig, GpuInfo, PolicyDecision, PresetMapping, ProviderConfig, ProviderControl,
    ProviderEvent, ProviderEventCallback, ResourceMetrics, ResourceSettings, RuntimeDownloadPhase,
    RuntimeManager, RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
};
use serde::Serialize;
use tauri::{Emitter, Manager, State, WindowEvent};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::time::{sleep, timeout, Duration};

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
    user_paused: bool,
    payout_address: Option<String>,
    coordinator_ws: String,
    configured_model: String,
    effective_model: String,
    model_override_active: bool,
    configured_model_download_bytes: Option<u64>,
    effective_model_download_bytes: Option<u64>,
    model_size_error: Option<String>,
    runtime_percent: u8,
    model_percent: u8,
    expected_total_download_bytes: Option<u64>,
    error_message: Option<String>,
    resource_settings: ResourceSettings,
    policy: PolicyDecision,
    policy_diagnostic: Option<String>,
    resource_metrics: ResourceMetrics,
    preset_mapping: PresetMapping,
    requires_restart: bool,
}

struct ProviderTask {
    stop: watch::Sender<bool>,
    control: ProviderControl,
    join: tauri::async_runtime::JoinHandle<()>,
}

struct StateInner {
    snapshot: Mutex<StatusSnapshot>,
    task: AsyncMutex<Option<ProviderTask>>,
    running_resource_settings: Mutex<Option<ResourceSettings>>,
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
            Ok(manager) => initial_runtime_state(&manager),
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
        let configured_model = config.ollama_model.clone();
        let resource_settings = config.resource_settings.clone();
        let preset_mapping = resource_preset_mapping(&resource_settings);
        let (effective_model, model_override_active, model_error) = model_status_selection(&config);
        let snapshot = StatusSnapshot {
            provider_running: false,
            runtime,
            runtime_suitability: runtime_suitability(),
            gpu,
            connection: provider_core::ConnectionState::Disconnected,
            fee_config: None,
            current_job: CurrentJob::Idle,
            user_paused: false,
            payout_address: config.payout_address,
            coordinator_ws: config.coordinator_ws,
            configured_model,
            effective_model,
            model_override_active,
            configured_model_download_bytes: None,
            effective_model_download_bytes: None,
            model_size_error: None,
            runtime_percent: 0,
            model_percent: 0,
            expected_total_download_bytes: None,
            error_message: error_message.or(runtime_error).or(model_error),
            resource_settings,
            policy: PolicyDecision::Paused("Provider is stopped".to_string()),
            policy_diagnostic: None,
            resource_metrics: ResourceMetrics {
                runtime_cpu_percent: None,
                runtime_ram_bytes: None,
                gpu_utilization_percent: None,
                gpu_vram_used_bytes: None,
                gpu_device_wide: false,
            },
            preset_mapping,
            requires_restart: false,
        };
        Self {
            inner: Arc::new(StateInner {
                snapshot: Mutex::new(snapshot),
                task: AsyncMutex::new(None),
                running_resource_settings: Mutex::new(None),
                confirmation_granted: Mutex::new(false),
                close_started: AtomicBool::new(false),
                allow_window_close: AtomicBool::new(false),
            }),
        }
    }
}

#[tauri::command]
async fn get_status(state: State<'_, DesktopState>) -> Result<StatusSnapshot, String> {
    let inner = state.inner().inner.clone();
    let mut snapshot = inner
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?
        .clone();
    populate_model_download_sizes(&mut snapshot).await;
    let mut stored = inner
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?;
    if stored.configured_model == snapshot.configured_model
        && stored.effective_model == snapshot.effective_model
    {
        stored.configured_model_download_bytes = snapshot.configured_model_download_bytes;
        stored.effective_model_download_bytes = snapshot.effective_model_download_bytes;
        stored.model_size_error = snapshot.model_size_error;
        Ok(stored.clone())
    } else {
        Ok(stored.clone())
    }
}

#[tauri::command]
fn confirm_runtime_download(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    if let RuntimeSuitability::NotRecommended { .. } = runtime_suitability() {
        let config = confirm_not_recommended_runtime()
            .map_err(|error| format!("Could not save hardware confirmation: {error:#}"))?;
        let mut granted = state
            .inner()
            .inner
            .confirmation_granted
            .lock()
            .map_err(|error| format!("Could not record runtime confirmation: {error}"))?;
        *granted = true;
        update_status(&state.inner().inner, &app, |snapshot| {
            snapshot.resource_settings = config.resource_settings;
        });
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
    let selection = effective_ollama_model(&config).map_err(|error| format!("{error:#}"))?;
    let requires_confirmation = requires_provider_confirmation(
        config.resource_settings.not_recommended_confirmed,
        &runtime_suitability(),
    );
    let confirmed = {
        let mut granted = state
            .inner()
            .inner
            .confirmation_granted
            .lock()
            .map_err(|error| format!("Could not check runtime confirmation: {error}"))?;
        if requires_confirmation && !*granted {
            return Err("Provider use on this hardware requires explicit confirmation.".into());
        }
        std::mem::take(&mut *granted) || config.resource_settings.not_recommended_confirmed
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
        snapshot.user_paused = false;
        snapshot.error_message = None;
        snapshot.coordinator_ws = config.coordinator_ws.clone();
        snapshot.configured_model = selection.configured;
        snapshot.effective_model = selection.effective;
        snapshot.model_override_active = selection.environment_override;
        snapshot.runtime_percent = 0;
        snapshot.model_percent = 0;
        snapshot.expected_total_download_bytes = None;
        snapshot.resource_settings = config.resource_settings.clone();
        snapshot.preset_mapping = resource_preset_mapping(&config.resource_settings);
        snapshot.requires_restart = false;
    }
    emit_status(&app, state.inner());

    let active_resource_settings = config.resource_settings.clone();
    let (stop, stop_rx) = watch::channel(false);
    let (control, pause_rx) = provider_control_channel();
    control
        .update_resource_settings(active_resource_settings.clone())
        .map_err(|error| format!("Invalid active resource settings: {error:#}"))?;
    let settings_rx = control.subscribe_resource_settings();
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
                let connected = matches!(&state, provider_core::ConnectionState::Connected);
                snapshot.connection = state;
                if let Some(message) = message {
                    snapshot.error_message = Some(message);
                } else if connected {
                    snapshot.error_message = None;
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
            ProviderEvent::Policy {
                decision,
                diagnostic,
            } => {
                snapshot.policy = decision;
                snapshot.policy_diagnostic = diagnostic;
            }
            ProviderEvent::ResourceMonitor { metrics } => {
                snapshot.resource_metrics = metrics;
            }
            ProviderEvent::Error { message } => snapshot.error_message = Some(message),
        });
    });

    let task_state = state.inner().inner.clone();
    let task_app = app.clone();
    *state
        .inner()
        .inner
        .running_resource_settings
        .lock()
        .map_err(|error| format!("Could not track active resource settings: {error}"))? =
        Some(active_resource_settings);
    let join = tauri::async_runtime::spawn(async move {
        let result = run_provider_managed_with_control(
            config,
            confirmed,
            stop_rx,
            pause_rx,
            settings_rx,
            runtime_callback,
            event_callback,
        )
        .await;
        update_status(&task_state, &task_app, |snapshot| {
            snapshot.provider_running = false;
            snapshot.connection = provider_core::ConnectionState::Disconnected;
            snapshot.user_paused = false;
            snapshot.policy = PolicyDecision::Paused("Provider is stopped".to_string());
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
        if let Ok(mut running) = task_state.running_resource_settings.lock() {
            *running = None;
        }
    });
    *task_slot = Some(ProviderTask {
        stop,
        control,
        join,
    });
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
async fn pause_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    set_provider_paused(state.inner(), true).await?;
    emit_status(&app, state.inner());
    Ok(())
}

#[tauri::command]
async fn resume_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    set_provider_paused(state.inner(), false).await?;
    emit_status(&app, state.inner());
    Ok(())
}

#[tauri::command]
async fn set_resource_settings(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    settings: ResourceSettings,
) -> Result<StatusSnapshot, String> {
    let snapshot = apply_resource_settings(state.inner(), settings).await?;
    emit_status(&app, state.inner());
    Ok(snapshot)
}

async fn set_provider_paused(state: &DesktopState, paused: bool) -> Result<(), String> {
    let task_slot = state.inner.task.lock().await;
    let task = task_slot
        .as_ref()
        .ok_or_else(|| "Provider is not running.".to_string())?;
    task.control
        .set_paused(paused)
        .map_err(|error| format!("{error:#}"))?;
    drop(task_slot);
    let mut snapshot = state
        .inner
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?;
    snapshot.user_paused = paused;
    Ok(())
}

async fn apply_resource_settings(
    state: &DesktopState,
    settings: ResourceSettings,
) -> Result<StatusSnapshot, String> {
    settings
        .validate()
        .map_err(|error| format!("Invalid resource settings: {error:#}"))?;
    let requires_restart = state
        .inner
        .running_resource_settings
        .lock()
        .map_err(|error| format!("Could not inspect active resource settings: {error}"))?
        .as_ref()
        .is_some_and(|active| active.requires_restart(&settings));
    let config = save_resource_settings(settings).map_err(|error| format!("{error:#}"))?;
    {
        let task_slot = state.inner.task.lock().await;
        if let Some(task) = task_slot
            .as_ref()
            .filter(|task| !task.join.inner().is_finished())
        {
            task.control
                .update_resource_settings(config.resource_settings.clone())
                .map_err(|error| format!("Could not update active resource policy: {error:#}"))?;
        }
    }
    let mut snapshot = state
        .inner
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?;
    snapshot.resource_settings = config.resource_settings;
    snapshot.preset_mapping = resource_preset_mapping(&snapshot.resource_settings);
    snapshot.requires_restart = requires_restart;
    Ok(snapshot.clone())
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
        snapshot.error_message = None;
    });
    state
        .inner()
        .inner
        .snapshot
        .lock()
        .map(|snapshot| snapshot.clone())
        .map_err(|error| format!("Provider status is unavailable: {error}"))
}

#[tauri::command]
async fn set_ollama_model(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    model: String,
) -> Result<StatusSnapshot, String> {
    let config = save_ollama_model(&model).map_err(|error| format!("{error:#}"))?;
    let (effective_model, model_override_active, model_error) = model_status_selection(&config);
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.configured_model = config.ollama_model;
        snapshot.effective_model = effective_model;
        snapshot.model_override_active = model_override_active;
        snapshot.configured_model_download_bytes = None;
        snapshot.effective_model_download_bytes = None;
        snapshot.model_size_error = None;
        snapshot.error_message = model_error;
    });
    get_status(state).await
}

fn model_status_selection(config: &ProviderConfig) -> (String, bool, Option<String>) {
    match effective_ollama_model(config) {
        Ok(selection) => (selection.effective, selection.environment_override, None),
        Err(error) => (
            std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| config.ollama_model.clone()),
            std::env::var_os("OLLAMA_MODEL").is_some(),
            Some(error.to_string()),
        ),
    }
}

fn resource_preset_mapping(settings: &ResourceSettings) -> PresetMapping {
    let logical_cpu_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    settings.preset.mapping(
        logical_cpu_count,
        settings.mode == provider_core::ResourceMode::AlwaysOnLowPriority,
    )
}

async fn populate_model_download_sizes(snapshot: &mut StatusSnapshot) {
    let manager = match RuntimeManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            snapshot.model_size_error = Some(error.to_string());
            return;
        }
    };
    let configured = manager
        .model_download_size(&snapshot.configured_model)
        .await;
    snapshot.configured_model_download_bytes = configured.as_ref().ok().copied();
    let mut errors = Vec::new();
    if let Err(error) = &configured {
        errors.push(format!("saved model: {error:#}"));
    }
    if snapshot.effective_model == snapshot.configured_model {
        snapshot.effective_model_download_bytes = snapshot.configured_model_download_bytes;
    } else {
        let effective = manager.model_download_size(&snapshot.effective_model).await;
        snapshot.effective_model_download_bytes = effective.as_ref().ok().copied();
        if let Err(error) = effective {
            errors.push(format!("OLLAMA_MODEL override: {error:#}"));
        }
    }
    snapshot.model_size_error = if errors.is_empty() {
        None
    } else {
        Some(format!(
            "Could not estimate model download size: {}",
            errors.join("; ")
        ))
    };
}

async fn stop_provider_task(app: &tauri::AppHandle, state: &DesktopState) -> Result<(), String> {
    stop_provider_task_and_join(&state.inner).await?;
    update_status(&state.inner, app, |snapshot| {
        snapshot.provider_running = false;
        snapshot.connection = provider_core::ConnectionState::Disconnected;
        snapshot.user_paused = false;
        snapshot.policy = PolicyDecision::Paused("Provider is stopped".to_string());
        if matches!(&snapshot.current_job, CurrentJob::Running { .. }) {
            snapshot.current_job = CurrentJob::Idle;
        }
    });
    Ok(())
}

#[tauri::command]
async fn restart_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let (provider_running, current_job, requires_restart, connected) = {
        let snapshot = state
            .inner()
            .inner
            .snapshot
            .lock()
            .map_err(|error| format!("Provider status is unavailable: {error}"))?;
        (
            snapshot.provider_running,
            snapshot.current_job.clone(),
            snapshot.requires_restart,
            snapshot.connection == provider_core::ConnectionState::Connected,
        )
    };
    if !provider_running {
        return Err(
            "The provider is not running; changed settings will apply on its next start.".into(),
        );
    }
    if !requires_restart {
        return Err("No runtime restart is currently required.".into());
    }
    if matches!(current_job, CurrentJob::Running { .. }) {
        return Err("Wait for the current job to finish before restarting the provider.".into());
    }

    if connected {
        {
            let task_slot = state.inner().inner.task.lock().await;
            let task = task_slot
                .as_ref()
                .ok_or_else(|| "Provider is not running.".to_string())?;
            task.control
                .pause()
                .map_err(|error| format!("Could not pause provider before restart: {error:#}"))?;
        }
        update_status(&state.inner().inner, &app, |snapshot| {
            snapshot.user_paused = true;
        });

        timeout(Duration::from_secs(5), async {
            loop {
                let is_paused =
                    {
                        let snapshot =
                            state.inner().inner.snapshot.lock().map_err(|error| {
                                format!("Provider status is unavailable: {error}")
                            })?;
                        snapshot.user_paused
                            && matches!(
                                snapshot.policy,
                                PolicyDecision::Paused(ref reason) if reason == "Paused by user"
                            )
                            && !matches!(snapshot.current_job, CurrentJob::Running { .. })
                    };
                if is_paused {
                    return Ok::<(), String>(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| "Timed out waiting for the provider to pause safely.".to_string())??;
    }

    stop_provider_task(&app, state.inner()).await?;
    start_provider(app, state).await
}

async fn stop_provider_task_and_join(state: &StateInner) -> Result<(), String> {
    let mut task_slot = state.task.lock().await;
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
    Ok(())
}

fn requires_provider_confirmation(
    already_confirmed: bool,
    suitability: &RuntimeSuitability,
) -> bool {
    !already_confirmed && matches!(suitability, RuntimeSuitability::NotRecommended { .. })
}

fn initial_runtime_state(manager: &RuntimeManager) -> RuntimeState {
    if !manager.runtime_dir().exists() {
        return RuntimeState::NotInstalled;
    }
    match manager.executable_path() {
        Ok(_) => RuntimeState::Ready,
        Err(error) => RuntimeState::Error {
            message: error.to_string(),
        },
    }
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
            pause_provider,
            resume_provider,
            set_resource_settings,
            restart_provider,
            confirm_runtime_download,
            set_payout_address,
            set_ollama_model
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

#[cfg(test)]
mod tests {
    use super::{
        apply_resource_settings, initial_runtime_state, provider_control_channel,
        requires_provider_confirmation, set_provider_paused, stop_provider_task_and_join,
        DesktopState, ProviderTask,
    };
    use provider_core::{
        ResourceMode, ResourcePreset, ResourceSchedule, ResourceSettings, RuntimeManager,
        RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::{mpsc, watch};

    struct IsolatedConfig {
        key: &'static str,
        previous: Option<OsString>,
        root: PathBuf,
    }

    impl IsolatedConfig {
        fn new() -> Self {
            let key = if cfg!(windows) {
                "APPDATA"
            } else if cfg!(target_os = "macos") {
                "HOME"
            } else {
                "XDG_CONFIG_HOME"
            };
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "gpuswarm-desktop-settings-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let previous = std::env::var_os(key);
            std::env::set_var(key, &root);
            Self {
                key,
                previous,
                root,
            }
        }
    }

    impl Drop for IsolatedConfig {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn pause_and_resume_commands_update_provider_control() {
        tauri::async_runtime::block_on(async {
            let state = DesktopState::new();
            let (control, mut pause_rx) = provider_control_channel();
            let (stop, mut stop_rx) = watch::channel(false);
            let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
            let join = tauri::async_runtime::spawn(async move {
                loop {
                    tokio::select! {
                        changed = pause_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            if observed_tx.send(*pause_rx.borrow()).is_err() {
                                break;
                            }
                        }
                        changed = stop_rx.changed() => {
                            if changed.is_ok() && *stop_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            });
            *state.inner.task.lock().await = Some(ProviderTask {
                stop,
                control,
                join,
            });

            set_provider_paused(&state, true).await.unwrap();
            assert_eq!(observed_rx.recv().await, Some(true));
            assert!(state.inner.snapshot.lock().unwrap().user_paused);

            set_provider_paused(&state, false).await.unwrap();
            assert_eq!(observed_rx.recv().await, Some(false));
            assert!(!state.inner.snapshot.lock().unwrap().user_paused);

            stop_provider_task_and_join(&state.inner).await.unwrap();
        });
    }

    #[test]
    fn resource_settings_command_validates_persists_and_sets_restart_required() {
        tauri::async_runtime::block_on(async {
            let _config = IsolatedConfig::new();
            let state = DesktopState::new();
            *state.inner.running_resource_settings.lock().unwrap() =
                Some(ResourceSettings::default());
            let (control, _) = provider_control_channel();
            let mut active_settings = control.subscribe_resource_settings();
            let (stop, mut stop_rx) = watch::channel(false);
            let join = tauri::async_runtime::spawn(async move {
                let _ = stop_rx.changed().await;
            });
            *state.inner.task.lock().await = Some(ProviderTask {
                stop,
                control,
                join,
            });

            let settings = ResourceSettings {
                mode: ResourceMode::ScheduledHours,
                schedule: Some(ResourceSchedule {
                    weekdays: vec![1, 2, 3, 4, 5],
                    start: "20:00".into(),
                    end: "06:00".into(),
                }),
                preset: ResourcePreset::High,
                ..ResourceSettings::default()
            };
            let saved = apply_resource_settings(&state, settings.clone())
                .await
                .unwrap();
            assert!(saved.requires_restart);
            assert_eq!(saved.resource_settings, settings);
            active_settings.changed().await.unwrap();
            assert_eq!(*active_settings.borrow_and_update(), settings);

            let config_path = provider_core::config_path().unwrap();
            let before_invalid_update = fs::read(&config_path).unwrap();
            let invalid = ResourceSettings {
                idle_threshold_minutes: 61,
                ..settings
            };
            assert!(apply_resource_settings(&state, invalid).await.is_err());
            assert_eq!(fs::read(config_path).unwrap(), before_invalid_update);

            let restored = apply_resource_settings(&state, ResourceSettings::default())
                .await
                .unwrap();
            assert!(!restored.requires_restart);
            stop_provider_task_and_join(&state.inner).await.unwrap();
        });
    }

    #[test]
    fn close_shutdown_helper_signals_and_joins_provider_task() {
        tauri::async_runtime::block_on(async {
            let state = DesktopState::new();
            let (stop, mut stop_rx) = watch::channel(false);
            let (control, _) = provider_control_channel();
            let task_stopped = Arc::new(AtomicBool::new(false));
            let observed_stop = task_stopped.clone();
            let join = tauri::async_runtime::spawn(async move {
                if stop_rx.changed().await.is_ok() && *stop_rx.borrow() {
                    observed_stop.store(true, Ordering::Release);
                }
            });
            *state.inner.task.lock().await = Some(ProviderTask {
                stop,
                control,
                join,
            });

            stop_provider_task_and_join(&state.inner).await.unwrap();

            assert!(task_stopped.load(Ordering::Acquire));
            assert!(state.inner.task.lock().await.is_none());
        });
    }

    #[test]
    #[ignore = "requires an installed managed Ollama runtime"]
    fn close_shutdown_helper_stops_the_managed_runtime() {
        tauri::async_runtime::block_on(async {
            let manager = RuntimeManager::new().unwrap();
            let executable = manager.executable_path().unwrap();
            let runtime_callback: RuntimeProgressCallback =
                Arc::new(|state| println!("close-test runtime={state:?}"));
            println!("close-test managed executable={}", executable.display());
            let runtime = manager.start(runtime_callback.clone()).await.unwrap();

            let state = DesktopState::new();
            let (stop, mut stop_rx) = watch::channel(false);
            let (control, _) = provider_control_channel();
            let join = tauri::async_runtime::spawn(async move {
                if stop_rx.changed().await.is_ok() && *stop_rx.borrow() {
                    runtime.shutdown(runtime_callback).await.unwrap();
                }
            });
            *state.inner.task.lock().await = Some(ProviderTask {
                stop,
                control,
                join,
            });

            stop_provider_task_and_join(&state.inner).await.unwrap();
            println!("close-test managed runtime stopped");
        });
    }

    #[test]
    fn missing_runtime_on_not_recommended_hardware_requires_confirmation() {
        let data_dir = std::env::temp_dir().join(format!(
            "gpuswarm-missing-runtime-test-{}",
            std::process::id()
        ));
        let manager = RuntimeManager::with_data_dir(data_dir.clone()).unwrap();
        assert!(!manager.runtime_dir().exists());
        assert_eq!(initial_runtime_state(&manager), RuntimeState::NotInstalled);
        assert!(requires_provider_confirmation(
            false,
            &RuntimeSuitability::NotRecommended {
                reason: "integrated Intel GPU".to_string()
            }
        ));
        assert!(!requires_provider_confirmation(
            true,
            &RuntimeSuitability::NotRecommended {
                reason: "integrated Intel GPU".to_string()
            }
        ));
        assert!(!requires_provider_confirmation(
            false,
            &RuntimeSuitability::Suitable
        ));
    }
}
