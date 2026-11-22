use provider_core::config::append_technical_log;
use std::{
    fs,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use provider_core::{
    confirm_not_recommended_runtime, detect_gpu, load_config, provider_control_channel,
    run_provider_managed_with_control, runtime_suitability,
    set_desktop_preferences as save_desktop_preferences, set_payout_address as save_payout_address,
    set_performance_percent as save_performance_percent, DesktopPreferences, FeeConfig, GpuInfo,
    JobResult, PolicyDecision, ProviderConfig, ProviderControl, ProviderEvent,
    ProviderEventCallback, ResourceSettings, RuntimeDownloadPhase, RuntimeManager,
    RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
};
use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{watch, Mutex as AsyncMutex};

mod history;
use history::{EarningsSnapshot, EarningsStore};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CurrentJob {
    Idle,
    Running { job_id: String },
    Finished { job_id: String, succeeded: bool },
}

fn tray_image() -> tauri::image::Image<'static> {
    let mut pixels = vec![0; 16 * 16 * 4];
    for y in 0..16 {
        for x in 0..16 {
            let dx = x as i32 - 8;
            let dy = y as i32 - 8;
            if dx * dx + dy * dy <= 36 {
                let offset = (y * 16 + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[5, 249, 20, 255]);
            }
        }
    }
    tauri::image::Image::new_owned(pixels, 16, 16)
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
    runtime_percent: u8,
    model_percent: u8,
    expected_total_download_bytes: Option<u64>,
    error_message: Option<String>,
    policy: PolicyDecision,
    policy_diagnostic: Option<String>,
    performance_percent: u8,
    hardware_confirmed: bool,
    desktop_preferences: DesktopPreferences,
    earnings: EarningsSnapshot,
    earnings_error: Option<String>,
}

struct ProviderTask {
    stop: watch::Sender<bool>,
    control: ProviderControl,
    join: tauri::async_runtime::JoinHandle<()>,
}

struct StateInner {
    snapshot: Mutex<StatusSnapshot>,
    task: AsyncMutex<Option<ProviderTask>>,
    confirmation_granted: Mutex<bool>,
    history: Mutex<Option<EarningsStore>>,
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
        let resource_settings = config.resource_settings.clone();
        let performance_percent = resource_settings.performance_percent;
        let history_path =
            provider_core::config_dir().map(|directory| directory.join("earnings.json"));
        let (history_store, earnings, earnings_error) = match history_path {
            Ok(path) => {
                let store = EarningsStore::new(path);
                match store.load() {
                    Ok(earnings) => {
                        if earnings.recovered_corruption {
                            log_technical("Recovered a corrupt local earnings history file.");
                        }
                        (Some(store), earnings, None)
                    }
                    Err(error) => {
                        log_technical(&format!("Could not load local earnings history: {error:#}"));
                        (
                            Some(store),
                            EarningsSnapshot::default(),
                            Some(error.to_string()),
                        )
                    }
                }
            }
            Err(error) => {
                log_technical(&format!(
                    "Could not determine earnings history path: {error:#}"
                ));
                (None, EarningsSnapshot::default(), Some(error.to_string()))
            }
        };
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
            runtime_percent: 0,
            model_percent: 0,
            expected_total_download_bytes: None,
            error_message: error_message.or(runtime_error),
            policy: PolicyDecision::Paused("Provider is stopped".to_string()),
            policy_diagnostic: None,
            performance_percent,
            hardware_confirmed: resource_settings.not_recommended_confirmed,
            desktop_preferences: config.desktop_preferences,
            earnings,
            earnings_error,
        };
        Self {
            inner: Arc::new(StateInner {
                snapshot: Mutex::new(snapshot),
                task: AsyncMutex::new(None),
                confirmation_granted: Mutex::new(false),
                history: Mutex::new(history_store),
                close_started: AtomicBool::new(false),
                allow_window_close: AtomicBool::new(false),
            }),
        }
    }
}

#[tauri::command]
fn get_status(state: State<'_, DesktopState>) -> Result<StatusSnapshot, String> {
    Ok(state
        .inner()
        .inner
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?
        .clone())
}

#[tauri::command]
fn get_earnings(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<EarningsSnapshot, String> {
    let history_store = state
        .inner()
        .inner
        .history
        .lock()
        .map_err(|error| format!("Earnings history is unavailable: {error}"))?;
    let store = history_store
        .as_ref()
        .ok_or_else(|| "Earnings history storage is unavailable.".to_string())?;
    let earnings = store
        .load()
        .map_err(|error| format!("Could not load earnings history: {error:#}"))?;
    if earnings.recovered_corruption {
        log_technical("Recovered a corrupt local earnings history file.");
    }
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.earnings = earnings.clone();
        snapshot.earnings_error = None;
    });
    Ok(earnings)
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
            snapshot.hardware_confirmed = config.resource_settings.not_recommended_confirmed;
        });
    }
    Ok(())
}

#[tauri::command]
async fn start_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let mut config = load_config()
        .map_err(|error| format!("Could not load provider configuration: {error:#}"))?
        .unwrap_or_else(ProviderConfig::defaults);
    let requires_confirmation = requires_provider_confirmation(
        config.resource_settings.not_recommended_confirmed,
        &runtime_suitability(),
    );
    config.resource_settings = desktop_resource_settings(&config.resource_settings);
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
        snapshot.runtime_percent = 0;
        snapshot.model_percent = 0;
        snapshot.expected_total_download_bytes = None;
        snapshot.performance_percent = config.resource_settings.performance_percent;
        snapshot.hardware_confirmed = config.resource_settings.not_recommended_confirmed;
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
        log_provider_event(&event);
        if let ProviderEvent::JobFinished { result } = &event {
            if let Err(error) = record_job_history(&event_state, &event_app, result) {
                log_technical(&format!(
                    "Could not update local earnings history: {error:#}"
                ));
                update_status(&event_state, &event_app, |snapshot| {
                    snapshot.earnings_error = Some(error.to_string());
                });
            }
            if result.error.is_none() {
                let notifications_enabled = event_state
                    .snapshot
                    .lock()
                    .map(|snapshot| snapshot.desktop_preferences.notifications_enabled)
                    .unwrap_or(false);
                if notifications_enabled {
                    if let Err(error) = event_app
                        .notification()
                        .builder()
                        .title("DeLore")
                        .body("A job has finished.")
                        .show()
                    {
                        log_technical(&format!("Could not show job notification: {error:#}"));
                    }
                }
            }
        }
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
                    succeeded: result.error.is_none(),
                }
            }
            ProviderEvent::Policy {
                decision,
                diagnostic,
            } => {
                snapshot.policy = decision;
                snapshot.policy_diagnostic = diagnostic;
            }
            ProviderEvent::ResourceMonitor { .. } => {}
            ProviderEvent::Error { message } => snapshot.error_message = Some(message),
        });
    });

    let task_state = state.inner().inner.clone();
    let task_app = app.clone();
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

#[tauri::command]
async fn set_performance(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    performance_percent: u8,
) -> Result<StatusSnapshot, String> {
    let config =
        save_performance_percent(performance_percent).map_err(|error| format!("{error:#}"))?;
    let active_settings = desktop_resource_settings(&config.resource_settings);
    {
        let task_slot = state.inner().inner.task.lock().await;
        if let Some(task) = task_slot
            .as_ref()
            .filter(|task| !task.join.inner().is_finished())
        {
            task.control
                .update_resource_settings(active_settings.clone())
                .map_err(|error| format!("{error:#}"))?;
        }
    }
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.performance_percent = performance_percent;
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
fn set_desktop_preferences(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    preferences: DesktopPreferences,
) -> Result<StatusSnapshot, String> {
    let previous = load_config()
        .map_err(|error| format!("Could not load preferences: {error:#}"))?
        .unwrap_or_else(ProviderConfig::defaults)
        .desktop_preferences;
    let autolaunch = app.autolaunch();
    let config =
        save_desktop_preferences(preferences.clone()).map_err(|error| format!("{error:#}"))?;
    if preferences.start_on_login != previous.start_on_login {
        let result = if preferences.start_on_login {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        if let Err(error) = result {
            if let Err(rollback_error) = save_desktop_preferences(previous) {
                return Err(format!(
                    "Could not update start-on-login: {error}; preference rollback failed: {rollback_error:#}"
                ));
            }
            return Err(format!("Could not update start-on-login: {error}"));
        }
    }
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.desktop_preferences = config.desktop_preferences;
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
fn open_logs() -> Result<(), String> {
    open_logs_directory()
}

#[tauri::command]
fn set_payout_address(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    address: String,
) -> Result<StatusSnapshot, String> {
    let config = save_payout_address(&address).map_err(|error| format!("{error:#}"))?;
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.payout_address = config.payout_address;
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

fn desktop_resource_settings(saved: &ResourceSettings) -> ResourceSettings {
    ResourceSettings {
        performance_percent: saved.performance_percent,
        not_recommended_confirmed: saved.not_recommended_confirmed,
        ..ResourceSettings::default()
    }
}

fn record_job_history(
    state: &Arc<StateInner>,
    app: &tauri::AppHandle,
    result: &JobResult,
) -> Result<(), String> {
    let fees = state
        .snapshot
        .lock()
        .map_err(|error| format!("Provider status is unavailable: {error}"))?
        .fee_config
        .clone();
    let earnings = {
        let history = state
            .history
            .lock()
            .map_err(|error| format!("Earnings history is unavailable: {error}"))?;
        let store = history
            .as_ref()
            .ok_or_else(|| "Earnings history storage is unavailable.".to_string())?;
        store
            .record_result(&result.job_id, result.error.is_none(), fees.as_ref())
            .map_err(|error| format!("{error:#}"))?
    };
    if earnings.recovered_corruption {
        log_technical("Recovered a corrupt local earnings history file.");
    }
    update_status(state, app, |snapshot| {
        snapshot.earnings = earnings;
        snapshot.earnings_error = None;
    });
    Ok(())
}

fn log_provider_event(event: &ProviderEvent) {
    match event {
        ProviderEvent::Connection { state, message } => {
            log_technical(&format!("Coordinator connection: {state:?}; {message:?}"));
        }
        ProviderEvent::FeeConfig { config } => {
            log_technical(&format!("Fee configuration received: {config:?}"));
        }
        ProviderEvent::JobStarted { job_id } => {
            log_technical(&format!("Job started: {job_id}"));
        }
        ProviderEvent::JobFinished { result } => {
            log_technical(&format!(
                "Job finished: id={} status={} duration_ms={}",
                result.job_id,
                if result.error.is_some() {
                    "failed"
                } else {
                    "completed"
                },
                result.duration_ms
            ));
        }
        ProviderEvent::Policy {
            decision,
            diagnostic,
        } => log_technical(&format!(
            "Provider policy changed: {decision:?}; diagnostic={diagnostic:?}"
        )),
        ProviderEvent::ResourceMonitor { metrics } => {
            log_technical(&format!("Resource sample: {metrics:?}"));
        }
        ProviderEvent::Error { message } => log_technical(&format!("Provider error: {message}")),
    }
}

fn log_technical(message: &str) {
    if let Err(error) = append_technical_log(message) {
        eprintln!("Could not write technical log: {error:#}");
    }
}

fn open_logs_directory() -> Result<(), String> {
    let logs_dir = provider_core::config_dir()
        .map_err(|error| format!("Could not locate logs directory: {error:#}"))?
        .join("logs");
    fs::create_dir_all(&logs_dir)
        .map_err(|error| format!("Could not create logs directory: {error}"))?;
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(&logs_dir);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(&logs_dir);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(&logs_dir);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open logs directory: {error}"))
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
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
            let provider = MenuItem::with_id(app, "provider", "Start / Stop", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "Pause / Resume", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &provider, &pause, &quit])?;
            let icon = tray_image();
            TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            if let Err(error) = window.show().and_then(|_| window.set_focus()) {
                                log_technical(&format!("Could not open the main window: {error}"));
                            }
                        }
                    }
                    "provider" => {
                        if let Err(error) = app.emit("tray-action", "toggle-provider") {
                            log_technical(&format!(
                                "Could not route the tray provider action: {error}"
                            ));
                        }
                    }
                    "pause" => {
                        if let Err(error) = app.emit("tray-action", "toggle-pause") {
                            log_technical(&format!(
                                "Could not route the tray pause action: {error}"
                            ));
                        }
                    }
                    "quit" => {
                        let app = app.clone();
                        let state = app.state::<DesktopState>().inner().clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(error) = stop_provider_task(&app, &state).await {
                                log_technical(&format!(
                                    "Provider shutdown from tray failed: {error}"
                                ));
                            }
                            state
                                .inner
                                .allow_window_close
                                .store(true, Ordering::Release);
                            app.exit(0);
                        });
                    }
                    _ => {}
                })
                .build(app)?;

            let preferences = load_config()
                .ok()
                .flatten()
                .map(|config| config.desktop_preferences)
                .unwrap_or_default();
            if preferences.start_minimized_to_tray {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_earnings,
            start_provider,
            stop_provider,
            pause_provider,
            resume_provider,
            set_performance,
            set_desktop_preferences,
            open_logs,
            confirm_runtime_download,
            set_payout_address
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<DesktopState>().inner().clone();
                if !state.inner.allow_window_close.load(Ordering::Acquire) {
                    let keep_in_tray = state
                        .inner
                        .snapshot
                        .lock()
                        .map(|snapshot| snapshot.desktop_preferences.keep_running_in_tray)
                        .unwrap_or(false);
                    api.prevent_close();
                    if keep_in_tray {
                        if let Err(error) = window.hide() {
                            log_technical(&format!("Could not hide the main window: {error}"));
                        }
                    } else if !state.inner.close_started.swap(true, Ordering::AcqRel) {
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
        desktop_resource_settings, initial_runtime_state, provider_control_channel,
        requires_provider_confirmation, set_provider_paused, stop_provider_task_and_join,
        DesktopState, ProviderTask,
    };
    use provider_core::{
        ResourceMode, ResourceSettings, RuntimeManager, RuntimeProgressCallback, RuntimeState,
        RuntimeSuitability,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, watch};

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
    fn desktop_policy_uses_fixed_defaults_and_keeps_user_choices() {
        let saved = ResourceSettings {
            mode: ResourceMode::AlwaysOnLowPriority,
            performance_percent: 72,
            not_recommended_confirmed: true,
            ..ResourceSettings::default()
        };
        let active = desktop_resource_settings(&saved);
        assert_eq!(active.performance_percent, 72);
        assert!(active.not_recommended_confirmed);
        assert_eq!(active.mode, ResourceSettings::default().mode);
        assert_eq!(
            active.do_not_run_on_battery,
            ResourceSettings::default().do_not_run_on_battery
        );
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
