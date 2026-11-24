use provider_core::config::append_technical_log;
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use provider_core::{
    confirm_not_recommended_runtime, detect_gpu, load_config, load_config_with_recovery,
    provider_control_channel, run_provider_managed_with_control, runtime_suitability,
    set_desktop_preferences as save_desktop_preferences, set_payout_address as save_payout_address,
    set_performance_percent as save_performance_percent, DesktopPreferences, FeeConfig, GpuInfo,
    JobResult, PolicyDecision, ProviderConfig, ProviderControl, ProviderEvent,
    ProviderEventCallback, ResourceMode, ResourceSettings, RuntimeDownloadPhase, RuntimeManager,
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UiErrorCategory {
    Connection,
    TransferInterrupted,
    NotEnoughDiskSpace,
    UnsupportedSystem,
    InvalidSetting,
    SettingsUnreadable,
    SetupFailed,
}

impl UiErrorCategory {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::TransferInterrupted => "transfer_interrupted",
            Self::NotEnoughDiskSpace => "not_enough_disk_space",
            Self::UnsupportedSystem => "unsupported_system",
            Self::InvalidSetting => "invalid_setting",
            Self::SettingsUnreadable => "settings_unreadable",
            Self::SetupFailed => "setup_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct UiError {
    category: UiErrorCategory,
    message: String,
}

fn classify_error(technical: &str) -> UiError {
    let lower = technical.to_ascii_lowercase();
    let (category, message) = if lower.contains("disk")
        || lower.contains("space")
        || lower.contains("insufficient free")
    {
        (
            UiErrorCategory::NotEnoughDiskSpace,
            "There is not enough free disk space. Free some space, then try again.",
        )
    } else if lower.contains("failed to parse config")
        || lower.contains("unreadable config")
        || lower.contains("could not read config")
    {
        (
            UiErrorCategory::SettingsUnreadable,
            "Your settings file could not be read. DeLore will use default settings.",
        )
    } else if lower.contains("unsupported on this platform") || lower.contains("unsupported system")
    {
        (
            UiErrorCategory::UnsupportedSystem,
            "This system is not supported. Check the help page for supported systems.",
        )
    } else if lower.contains("payout address") {
        (
            UiErrorCategory::InvalidSetting,
            "Enter a valid payout address with a 0x prefix and 40 hexadecimal characters.",
        )
    } else if lower.contains("coordinator url") || lower.contains("unencrypted coordinator urls") {
        (
            UiErrorCategory::InvalidSetting,
            "The saved coordinator address is invalid. Update coordinator_ws in your settings file.",
        )
    } else if lower.contains("did not become ready within") {
        (
            UiErrorCategory::SetupFailed,
            "DeLore could not finish starting after a few minutes. Please try again or open the logs for details.",
        )
    } else if lower.contains("ollama_model")
        || lower.contains("invalid model")
        || lower.contains("performance must")
        || lower.contains("invalid resource_settings")
        || lower.contains("invalid active resource settings")
    {
        (
            UiErrorCategory::InvalidSetting,
            "A saved setting is invalid. Review your settings and try again.",
        )
    } else if lower.contains("coordinator")
        || lower.contains("connect")
        || lower.contains("network")
        || lower.contains("http")
        || lower.contains("offline")
        || lower.contains("registry")
    {
        (
            UiErrorCategory::Connection,
            "DeLore could not connect. Check your internet connection, then try again.",
        )
    } else if lower.contains("interrupted")
        || lower.contains("unexpected byte range")
        || lower.contains("download ended before")
        || lower.contains("download returned an http error")
    {
        (
            UiErrorCategory::TransferInterrupted,
            "Setup was interrupted while receiving files. Check your internet connection, then retry.",
        )
    } else {
        (
            UiErrorCategory::SetupFailed,
            "Setup could not finish. Check your internet connection and free disk space, then try again.",
        )
    };
    UiError {
        category,
        message: message.to_string(),
    }
}

fn map_error_with_logger(technical: &str, mut logger: impl FnMut(&str)) -> UiError {
    logger(technical);
    classify_error(technical)
}

fn map_error(technical: &str) -> UiError {
    map_error_with_logger(technical, |detail| {
        log_technical(&format!("Desktop error: {detail}"));
    })
}

fn settings_unreadable_error() -> UiError {
    UiError {
        category: UiErrorCategory::SettingsUnreadable,
        message: "Your settings file could not be read. DeLore will use default settings."
            .to_string(),
    }
}

fn map_invoke_error(technical: &str) -> String {
    let error = map_error(technical);
    format!("{}|{}", error.category.as_str(), error.message)
}

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
    #[serde(rename = "setup_state")]
    runtime: RuntimeState,
    #[serde(rename = "hardware_support")]
    runtime_suitability: RuntimeSuitability,
    gpu: GpuInfo,
    connection: provider_core::ConnectionState,
    fee_config: Option<FeeConfig>,
    current_job: CurrentJob,
    user_paused: bool,
    payout_address: Option<String>,
    coordinator_ws: String,
    #[serde(rename = "setup_progress_percent")]
    runtime_percent: u8,
    #[serde(rename = "artifact_progress_percent")]
    model_percent: u8,
    #[serde(rename = "expected_setup_bytes")]
    expected_total_download_bytes: Option<u64>,
    error_message: Option<UiError>,
    policy: PolicyDecision,
    policy_diagnostic: Option<String>,
    performance_percent: u8,
    hardware_confirmed: bool,
    desktop_preferences: DesktopPreferences,
    earnings: EarningsSnapshot,
    earnings_error: Option<UiError>,
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
        let (config, settings_error) = match load_config_with_recovery() {
            Ok(loaded) => (
                loaded.config.unwrap_or_else(ProviderConfig::defaults),
                loaded.recovered_unreadable.then(settings_unreadable_error),
            ),
            Err(error) => {
                log_technical(&format!(
                    "Could not back up unreadable provider configuration: {error:#}"
                ));
                (
                    ProviderConfig::defaults(),
                    Some(settings_unreadable_error()),
                )
            }
        };
        let gpu = detect_gpu();
        let (runtime, runtime_error) = match RuntimeManager::new() {
            Ok(manager) => (initial_runtime_state(&manager), None),
            Err(error) => {
                let mapped = map_error(&format!("{error:#}"));
                (
                    RuntimeState::Error {
                        message: mapped.message.clone(),
                    },
                    Some(mapped),
                )
            }
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
                            Some(map_error(&format!("{error:#}"))),
                        )
                    }
                }
            }
            Err(error) => {
                log_technical(&format!(
                    "Could not determine earnings history path: {error:#}"
                ));
                (
                    None,
                    EarningsSnapshot::default(),
                    Some(map_error(&format!("{error:#}"))),
                )
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
            coordinator_ws: std::env::var("COORDINATOR_WS")
                .unwrap_or_else(|_| config.coordinator_ws.clone()),
            runtime_percent: 0,
            model_percent: 0,
            expected_total_download_bytes: None,
            error_message: settings_error.or(runtime_error),
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
        .map_err(|error| map_invoke_error(&format!("Provider status is unavailable: {error}")))?
        .clone())
}

#[tauri::command]
fn get_earnings(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<EarningsSnapshot, String> {
    let history_store =
        state.inner().inner.history.lock().map_err(|error| {
            map_invoke_error(&format!("Earnings history is unavailable: {error}"))
        })?;
    let store = history_store
        .as_ref()
        .ok_or_else(|| map_invoke_error("Earnings history storage is unavailable."))?;
    let earnings = store.load().map_err(|error| {
        map_invoke_error(&format!("Could not load earnings history: {error:#}"))
    })?;
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
fn confirm_hardware_use(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    if let RuntimeSuitability::NotRecommended { .. } = runtime_suitability() {
        let loaded = load_config_with_recovery().map_err(|error| {
            map_invoke_error(&format!(
                "Could not back up unreadable provider configuration: {error:#}"
            ))
        })?;
        let hardware_confirmed = if loaded.recovered_unreadable {
            true
        } else {
            confirm_not_recommended_runtime()
                .map_err(|error| map_invoke_error(&format!("{error:#}")))?
                .resource_settings
                .not_recommended_confirmed
        };
        let mut granted = state
            .inner()
            .inner
            .confirmation_granted
            .lock()
            .map_err(|error| {
                map_invoke_error(&format!("Could not record hardware confirmation: {error}"))
            })?;
        *granted = true;
        update_status(&state.inner().inner, &app, |snapshot| {
            snapshot.hardware_confirmed = hardware_confirmed;
            if loaded.recovered_unreadable {
                snapshot.error_message = Some(settings_unreadable_error());
            }
        });
    }
    Ok(())
}

#[tauri::command]
async fn start_provider(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let loaded_config = load_config_with_recovery().map_err(|error| {
        map_invoke_error(&format!(
            "Could not back up unreadable provider configuration: {error:#}"
        ))
    })?;
    let settings_were_unreadable = loaded_config.recovered_unreadable;
    let mut config = loaded_config
        .config
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
            .map_err(|error| {
                map_invoke_error(&format!("Could not check hardware confirmation: {error}"))
            })?;
        if requires_confirmation && !*granted {
            return Err(map_invoke_error(
                "Provider use on this hardware requires explicit confirmation.",
            ));
        }
        std::mem::take(&mut *granted) || config.resource_settings.not_recommended_confirmed
    };
    if settings_were_unreadable && confirmed {
        config.resource_settings.not_recommended_confirmed = true;
    }

    let mut task_slot = state.inner().inner.task.lock().await;
    if task_slot
        .as_ref()
        .is_some_and(|task| !task.join.inner().is_finished())
    {
        return Err(map_invoke_error("Provider is already running."));
    }

    {
        let mut snapshot = state.inner().inner.snapshot.lock().map_err(|error| {
            map_invoke_error(&format!("Provider status is unavailable: {error}"))
        })?;
        snapshot.provider_running = true;
        snapshot.current_job = CurrentJob::Idle;
        snapshot.user_paused = false;
        snapshot.error_message = settings_were_unreadable.then(settings_unreadable_error);
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
        .map_err(|error| {
            map_invoke_error(&format!("Invalid active resource settings: {error:#}"))
        })?;
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
            let runtime = match runtime {
                RuntimeState::Error { message } => {
                    let mapped = map_error(&message);
                    snapshot.error_message = Some(mapped.clone());
                    RuntimeState::Error {
                        message: mapped.message,
                    }
                }
                other => other,
            };
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
                    snapshot.earnings_error = Some(map_error(&format!("{error:#}")));
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
                if matches!(&state, provider_core::ConnectionState::Disconnected)
                    && matches!(snapshot.current_job, CurrentJob::Running { .. })
                {
                    snapshot.current_job = CurrentJob::Idle;
                }
                snapshot.connection = state;
                if let Some(message) = message {
                    snapshot.error_message = Some(map_error(&message));
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
                snapshot.policy_diagnostic = diagnostic.map(|detail| map_error(&detail).message);
            }
            ProviderEvent::ResourceMonitor { .. } => {}
            ProviderEvent::Error { message } => snapshot.error_message = Some(map_error(&message)),
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
                        message: map_error(&format!("{error:#}")).message,
                    },
                };
                snapshot.runtime_percent = 0;
                snapshot.model_percent = 0;
                snapshot.expected_total_download_bytes = None;
            }
            if let Err(error) = result {
                snapshot.error_message = Some(map_error(&format!("{error:#}")));
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
        .ok_or_else(|| map_invoke_error("Provider is not running."))?;
    task.control
        .set_paused(paused)
        .map_err(|error| map_invoke_error(&format!("{error:#}")))?;
    drop(task_slot);
    let mut snapshot =
        state.inner.snapshot.lock().map_err(|error| {
            map_invoke_error(&format!("Provider status is unavailable: {error}"))
        })?;
    snapshot.user_paused = paused;
    Ok(())
}

#[tauri::command]
async fn set_performance(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    performance_percent: i16,
) -> Result<StatusSnapshot, String> {
    let config = save_performance_percent(performance_percent)
        .map_err(|error| map_invoke_error(&format!("{error:#}")))?;
    let active_settings = desktop_resource_settings(&config.resource_settings);
    {
        let task_slot = state.inner().inner.task.lock().await;
        if let Some(task) = task_slot
            .as_ref()
            .filter(|task| !task.join.inner().is_finished())
        {
            task.control
                .update_resource_settings(active_settings.clone())
                .map_err(|error| map_invoke_error(&format!("{error:#}")))?;
        }
    }
    update_status(&state.inner().inner, &app, |snapshot| {
        snapshot.performance_percent = config.resource_settings.performance_percent;
    });
    state
        .inner()
        .inner
        .snapshot
        .lock()
        .map(|snapshot| snapshot.clone())
        .map_err(|error| map_invoke_error(&format!("Provider status is unavailable: {error}")))
}

#[tauri::command]
fn set_desktop_preferences(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    preferences: DesktopPreferences,
) -> Result<StatusSnapshot, String> {
    let previous = load_config()
        .map_err(|error| map_invoke_error(&format!("Could not load preferences: {error:#}")))?
        .unwrap_or_else(ProviderConfig::defaults)
        .desktop_preferences;
    let autolaunch = app.autolaunch();
    let config = save_desktop_preferences(preferences.clone())
        .map_err(|error| map_invoke_error(&format!("{error:#}")))?;
    if preferences.start_on_login != previous.start_on_login {
        let result = if preferences.start_on_login {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        if let Err(error) = result {
            if let Err(rollback_error) = save_desktop_preferences(previous) {
                return Err(map_invoke_error(&format!(
                    "Could not update start-on-login: {error}; preference rollback failed: {rollback_error:#}"
                )));
            }
            return Err(map_invoke_error(&format!(
                "Could not update start-on-login: {error}"
            )));
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
        .map_err(|error| map_invoke_error(&format!("Provider status is unavailable: {error}")))
}

#[tauri::command]
fn open_logs() -> Result<(), String> {
    open_logs_directory().map_err(|error| map_invoke_error(&error))
}

#[tauri::command]
fn open_data_folder() -> Result<(), String> {
    open_data_directory().map_err(|error| map_invoke_error(&error))
}

#[tauri::command]
fn set_payout_address(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    address: String,
) -> Result<StatusSnapshot, String> {
    let config =
        save_payout_address(&address).map_err(|error| map_invoke_error(&format!("{error:#}")))?;
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
        .map_err(|error| map_invoke_error(&format!("Provider status is unavailable: {error}")))
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
        return Err(map_invoke_error(&format!(
            "Provider shutdown task failed: {error}"
        )));
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
        mode: ResourceMode::AlwaysOnLowPriority,
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
    open_directory(&logs_dir, "logs directory")
}

fn open_data_directory() -> Result<(), String> {
    let data_dir = provider_core::config_dir()
        .map_err(|error| format!("Could not locate data directory: {error:#}"))?;
    fs::create_dir_all(&data_dir)
        .map_err(|error| format!("Could not create data directory: {error}"))?;
    open_directory(&data_dir, "data folder")
}

fn open_directory(directory: &Path, description: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(directory);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(directory);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(directory);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open {description}: {error}"))
}

fn initial_runtime_state(manager: &RuntimeManager) -> RuntimeState {
    if !manager.runtime_dir().exists() {
        return RuntimeState::NotInstalled;
    }
    match manager.executable_path() {
        Ok(_) => RuntimeState::Ready,
        Err(error) => RuntimeState::Error {
            message: map_error(&format!("{error:#}")).message,
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
                log_technical(&format!("Could not emit provider status: {error}"));
            }
        }
        Err(error) => log_technical(&format!("Provider status lock is unavailable: {error}")),
    }
}

fn emit_status(app: &tauri::AppHandle, state: &DesktopState) {
    match state.inner.snapshot.lock() {
        Ok(snapshot) => {
            let snapshot = snapshot.clone();
            if let Err(error) = app.emit("provider-status", snapshot) {
                log_technical(&format!("Could not emit provider status: {error}"));
            }
        }
        Err(error) => log_technical(&format!("Provider status lock is unavailable: {error}")),
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
        .plugin(tauri_plugin_single_instance::init(
            |app, _arguments, _working_directory| {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(error) = window
                        .unminimize()
                        .and_then(|_| window.show())
                        .and_then(|_| window.set_focus())
                    {
                        log_technical(&format!(
                            "Could not focus the existing application window: {error}"
                        ));
                    }
                }
            },
        ))
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
                            if let Err(error) = window
                                .unminimize()
                                .and_then(|_| window.show())
                                .and_then(|_| window.set_focus())
                            {
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
            open_data_folder,
            confirm_hardware_use,
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
                                log_technical(&format!(
                                    "Provider shutdown on window close failed: {error}"
                                ));
                            }
                            state
                                .inner
                                .allow_window_close
                                .store(true, Ordering::Release);
                            if let Err(error) = closing_window.close() {
                                log_technical(&format!("Could not close the app window: {error}"));
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
        classify_error, desktop_resource_settings, initial_runtime_state, map_error_with_logger,
        provider_control_channel, requires_provider_confirmation, set_provider_paused,
        settings_unreadable_error, stop_provider_task_and_join, DesktopState, ProviderTask,
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

    struct IsolatedConfigDir(std::path::PathBuf);

    impl IsolatedConfigDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gpuswarm-close-test-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            std::env::set_var("GPUSWARM_CONFIG_DIR", &path);
            Self(path)
        }
    }

    impl Drop for IsolatedConfigDir {
        fn drop(&mut self) {
            std::env::remove_var("GPUSWARM_CONFIG_DIR");
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn technical_provider_errors_are_mapped_to_safe_messages() {
        let errors = [
            "OLLAMA_MODEL contains invalid non-Unicode data.",
            "managed Ollama process exited during startup",
            "managed Ollama runtime did not become ready within 180 seconds",
            "model download ended before Ollama reported completion",
            "Ollama model download failed",
            "runtime download requires explicit confirmation",
            "resumed runtime download returned an unexpected byte range",
            "runtime download exceeded the pinned archive size",
            "runtime archive extraction is unsupported on this platform",
        ];
        let forbidden = ["ollama", "runtime", "sidecar", "model", "pull", "download"];
        for technical in errors {
            let mut logged = Vec::new();
            let mapped = map_error_with_logger(technical, |detail| logged.push(detail.to_string()));
            let message = mapped.message.to_ascii_lowercase();
            assert!(
                forbidden.iter().all(|word| !message.contains(word)),
                "unsafe mapped message for {technical:?}: {}",
                mapped.message
            );
            assert_eq!(logged, [technical]);
        }
    }

    #[test]
    fn unknown_error_is_generic_and_technical_detail_is_logged() {
        let technical = "unexpected private engine failure detail";
        let mut logged = Vec::new();
        let mapped = map_error_with_logger(technical, |detail| logged.push(detail.to_string()));

        assert_eq!(mapped.category, super::UiErrorCategory::SetupFailed);
        assert_eq!(
            mapped.message,
            "Setup could not finish. Check your internet connection and free disk space, then try again."
        );
        assert_eq!(logged, [technical]);
        let message = mapped.message.to_ascii_lowercase();
        assert!(
            ["ollama", "runtime", "sidecar", "model", "pull", "download"]
                .iter()
                .all(|word| !message.contains(word))
        );
    }

    #[test]
    fn long_startup_timeout_maps_to_a_friendly_message() {
        let mapped =
            classify_error("managed Ollama runtime did not become ready within 180 seconds");
        assert_eq!(mapped.category, super::UiErrorCategory::SetupFailed);
        assert_eq!(
            mapped.message,
            "DeLore could not finish starting after a few minutes. Please try again or open the logs for details."
        );
    }

    #[test]
    fn error_categories_cover_connection_disk_and_invalid_settings() {
        assert_eq!(
            classify_error("coordinator connection refused").category,
            super::UiErrorCategory::Connection
        );
        assert_eq!(
            classify_error("insufficient free disk space").category,
            super::UiErrorCategory::NotEnoughDiskSpace
        );
        assert_eq!(
            classify_error("Payout address must be 0x followed by 40 hex characters").category,
            super::UiErrorCategory::InvalidSetting
        );
        assert_eq!(
            classify_error("coordinator URL path must be /ws").category,
            super::UiErrorCategory::InvalidSetting
        );
    }

    #[test]
    fn unreadable_settings_use_a_dedicated_friendly_category() {
        let mapped = classify_error("failed to parse config: key must be a string");
        assert_eq!(mapped.category, super::UiErrorCategory::SettingsUnreadable);
        assert_eq!(
            mapped.message,
            "Your settings file could not be read. DeLore will use default settings."
        );
        assert_eq!(settings_unreadable_error().message, mapped.message);
    }

    #[test]
    fn closed_local_port_maps_to_a_safe_connection_message() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let technical = std::net::TcpStream::connect(address)
            .unwrap_err()
            .to_string();
        let mut logged = String::new();
        let mapped = map_error_with_logger(&technical, |detail| logged = detail.to_string());

        assert_eq!(mapped.category, super::UiErrorCategory::Connection);
        assert_eq!(
            mapped.message,
            "DeLore could not connect. Check your internet connection, then try again."
        );
        assert_eq!(logged, technical);
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
        assert_eq!(active.mode, ResourceMode::AlwaysOnLowPriority);
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
        let _isolated_config = IsolatedConfigDir::new();
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
