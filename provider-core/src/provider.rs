use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use common::{FeeConfig, JobPayload, JobResult, ProviderRegistration, WsMessage};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    confirm_not_recommended_runtime, coordinator_fees_url_with_override, decide_current_policy,
    detect_gpu, duty_cycle_pause_ms, effective_ollama_model, ensure_provider_confirmation,
    run_ollama_with_settings, runtime_suitability, validate_ollama_model, DutyCyclePause,
    NativeSystemProbe, OllamaModelSelection, PolicyDecision, ProviderConfig, ResourceMetrics,
    ResourceMonitor, ResourceSettings, RunningRuntime, RuntimeManager, RuntimeProgressCallback,
    RuntimeState, RuntimeSuitability, SystemProbe,
};

#[derive(Debug, Deserialize)]
struct CoordinatorFeesResponse {
    #[serde(default)]
    network_model: Option<String>,
}

async fn fetch_network_model(coordinator_ws: &str) -> Result<Option<String>> {
    let http_override = match std::env::var("COORDINATOR_HTTP") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("COORDINATOR_HTTP contains invalid non-Unicode data")
        }
    };
    let url = coordinator_fees_url_with_override(coordinator_ws, http_override.as_deref())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to configure coordinator settings client")?;
    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            log_model_selection_issue(&format!(
                "Could not fetch coordinator model selection from {url}: {error:#}"
            ));
            return Ok(None);
        }
    };
    if !response.status().is_success() {
        log_model_selection_issue(&format!(
            "Coordinator model selection request returned HTTP {}; using saved model",
            response.status()
        ));
        return Ok(None);
    }
    match response.json::<CoordinatorFeesResponse>().await {
        Ok(fees) => Ok(fees.network_model),
        Err(error) => {
            log_model_selection_issue(&format!(
                "Could not parse coordinator model selection: {error:#}; using saved model"
            ));
            Ok(None)
        }
    }
}

fn log_model_selection_issue(message: &str) {
    if let Err(error) = crate::config::append_technical_log(message) {
        eprintln!("Could not append technical log: {error:#}");
    }
}

fn select_network_model(
    selection: &OllamaModelSelection,
    network_model: Option<&str>,
) -> Result<String> {
    if selection.environment_override {
        return Ok(selection.effective.clone());
    }
    let Some(model) = network_model else {
        return Ok(selection.configured.clone());
    };
    validate_ollama_model(model).context("coordinator selected an invalid model name")?;
    Ok(model.to_string())
}

#[derive(Debug, Clone)]
pub struct JobHistoryEntry {
    pub job_id: String,
    pub provider_id: String,
    pub status: String,
    pub result_data: String,
    pub error: Option<String>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum ProviderEvent {
    Connection {
        state: ConnectionState,
        message: Option<String>,
    },
    FeeConfig {
        config: FeeConfig,
    },
    JobStarted {
        job_id: String,
    },
    JobFinished {
        result: JobResult,
    },
    Policy {
        decision: PolicyDecision,
        diagnostic: Option<String>,
    },
    ResourceMonitor {
        metrics: ResourceMetrics,
    },
    Error {
        message: String,
    },
}

pub type ProviderEventCallback = Arc<dyn Fn(ProviderEvent) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct ProviderControl {
    pause_tx: watch::Sender<bool>,
    resource_settings_tx: watch::Sender<ResourceSettings>,
}

impl ProviderControl {
    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.pause_tx
            .send(paused)
            .context("provider control channel is closed")
    }

    pub fn pause(&self) -> Result<()> {
        self.set_paused(true)
    }

    pub fn resume(&self) -> Result<()> {
        self.set_paused(false)
    }

    pub fn update_resource_settings(&self, settings: ResourceSettings) -> Result<()> {
        settings.validate()?;
        self.resource_settings_tx.send_replace(settings);
        Ok(())
    }

    pub fn subscribe_resource_settings(&self) -> watch::Receiver<ResourceSettings> {
        self.resource_settings_tx.subscribe()
    }
}

pub fn provider_control_channel() -> (ProviderControl, watch::Receiver<bool>) {
    let (pause_tx, pause_rx) = watch::channel(false);
    let (resource_settings_tx, _) = watch::channel(ResourceSettings::default());
    (
        ProviderControl {
            pause_tx,
            resource_settings_tx,
        },
        pause_rx,
    )
}

#[derive(Clone, Copy)]
struct ProviderRunOptions {
    confirmed_not_recommended: bool,
    stop_after_job: bool,
    handle_ctrl_c: bool,
    use_cli_environment_overrides: bool,
}

struct ProviderControlReceivers {
    stop: watch::Receiver<bool>,
    pause: watch::Receiver<bool>,
    resource_settings: watch::Receiver<ResourceSettings>,
}

struct ProviderConnectionContext<'a> {
    runtime: &'a RunningRuntime,
    options: ProviderRunOptions,
    stop_rx: &'a mut watch::Receiver<bool>,
    pause_rx: &'a mut watch::Receiver<bool>,
    resource_settings: ResourceSettings,
    settings_rx: &'a mut watch::Receiver<ResourceSettings>,
    system_probe: &'a dyn SystemProbe,
    event_callback: &'a ProviderEventCallback,
    backoff: &'a mut ReconnectBackoff,
}

pub async fn run_provider(config: ProviderConfig) -> Result<()> {
    run_provider_with_confirmation(config, false, false, Arc::new(|_| {})).await
}

pub async fn run_provider_with_confirmation(
    config: ProviderConfig,
    confirmed_not_recommended: bool,
    stop_after_job: bool,
    runtime_callback: RuntimeProgressCallback,
) -> Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (control, pause_rx) = provider_control_channel();
    control.update_resource_settings(config.resource_settings.clone())?;
    let settings_rx = control.subscribe_resource_settings();
    let result = run_provider_inner(
        config,
        ProviderRunOptions {
            confirmed_not_recommended,
            stop_after_job,
            handle_ctrl_c: true,
            use_cli_environment_overrides: true,
        },
        ProviderControlReceivers {
            stop: stop_rx,
            pause: pause_rx,
            resource_settings: settings_rx,
        },
        Arc::new(NativeSystemProbe),
        runtime_callback,
        Arc::new(|_| {}),
    )
    .await;
    drop(stop_tx);
    result
}

pub async fn run_provider_managed(
    config: ProviderConfig,
    confirmed_not_recommended: bool,
    stop_rx: watch::Receiver<bool>,
    runtime_callback: RuntimeProgressCallback,
    event_callback: ProviderEventCallback,
) -> Result<()> {
    let (control, pause_rx) = provider_control_channel();
    control.update_resource_settings(config.resource_settings.clone())?;
    let settings_rx = control.subscribe_resource_settings();
    run_provider_managed_with_control(
        config,
        confirmed_not_recommended,
        stop_rx,
        pause_rx,
        settings_rx,
        runtime_callback,
        event_callback,
    )
    .await
}

pub async fn run_provider_managed_with_control(
    config: ProviderConfig,
    confirmed_not_recommended: bool,
    stop_rx: watch::Receiver<bool>,
    pause_rx: watch::Receiver<bool>,
    settings_rx: watch::Receiver<ResourceSettings>,
    runtime_callback: RuntimeProgressCallback,
    event_callback: ProviderEventCallback,
) -> Result<()> {
    run_provider_inner(
        config,
        ProviderRunOptions {
            confirmed_not_recommended,
            stop_after_job: false,
            handle_ctrl_c: false,
            use_cli_environment_overrides: false,
        },
        ProviderControlReceivers {
            stop: stop_rx,
            pause: pause_rx,
            resource_settings: settings_rx,
        },
        Arc::new(NativeSystemProbe),
        runtime_callback,
        event_callback,
    )
    .await
}

pub async fn run_provider_managed_with_probe(
    config: ProviderConfig,
    confirmed_not_recommended: bool,
    stop_rx: watch::Receiver<bool>,
    pause_rx: watch::Receiver<bool>,
    system_probe: Arc<dyn SystemProbe>,
    runtime_callback: RuntimeProgressCallback,
    event_callback: ProviderEventCallback,
) -> Result<()> {
    let (control, _) = provider_control_channel();
    control.update_resource_settings(config.resource_settings.clone())?;
    let settings_rx = control.subscribe_resource_settings();
    run_provider_inner(
        config,
        ProviderRunOptions {
            confirmed_not_recommended,
            stop_after_job: false,
            handle_ctrl_c: false,
            use_cli_environment_overrides: false,
        },
        ProviderControlReceivers {
            stop: stop_rx,
            pause: pause_rx,
            resource_settings: settings_rx,
        },
        system_probe,
        runtime_callback,
        event_callback,
    )
    .await
}

async fn run_provider_inner(
    mut config: ProviderConfig,
    options: ProviderRunOptions,
    control_receivers: ProviderControlReceivers,
    system_probe: Arc<dyn SystemProbe>,
    runtime_callback: RuntimeProgressCallback,
    event_callback: ProviderEventCallback,
) -> Result<()> {
    let ProviderControlReceivers {
        stop: mut stop_rx,
        pause: mut pause_rx,
        resource_settings: mut settings_rx,
    } = control_receivers;
    config.resource_settings.validate()?;
    let coordinator_url = std::env::var("COORDINATOR_WS").unwrap_or(config.coordinator_ws.clone());
    crate::validate_coordinator_ws_url(&coordinator_url)?;
    let model_selection = effective_ollama_model(&config)?;
    let runtime_manager = RuntimeManager::new()?;
    if *stop_rx.borrow() {
        runtime_callback(RuntimeState::Stopping);
        runtime_callback(if runtime_manager.runtime_dir().exists() {
            RuntimeState::Ready
        } else {
            RuntimeState::NotInstalled
        });
        emit_connection(&event_callback, ConnectionState::Disconnected, None);
        return Ok(());
    }
    let confirmed_not_recommended =
        options.confirmed_not_recommended || config.resource_settings.not_recommended_confirmed;
    let suitability = runtime_suitability();
    ensure_provider_confirmation(&suitability, confirmed_not_recommended)?;
    if options.confirmed_not_recommended
        && !config.resource_settings.not_recommended_confirmed
        && matches!(suitability, RuntimeSuitability::NotRecommended { .. })
    {
        config.resource_settings.not_recommended_confirmed = true;
        confirm_not_recommended_runtime()?;
    }
    runtime_callback(RuntimeState::Starting);
    let network_model = if model_selection.environment_override {
        None
    } else {
        tokio::select! {
            biased;
            _ = wait_for_stop(&mut stop_rx) => {
                runtime_callback(RuntimeState::Stopping);
                runtime_callback(if runtime_manager.runtime_dir().exists() {
                    RuntimeState::Ready
                } else {
                    RuntimeState::NotInstalled
                });
                emit_connection(&event_callback, ConnectionState::Disconnected, None);
                return Ok(());
            }
            result = fetch_network_model(&coordinator_url) => result?,
        }
    };
    let model = select_network_model(&model_selection, network_model.as_deref())?;
    let runtime_install = tokio::select! {
        biased;
        _ = wait_for_stop(&mut stop_rx) => {
            runtime_callback(RuntimeState::Stopping);
            runtime_callback(if runtime_manager.runtime_dir().exists() {
                RuntimeState::Ready
            } else {
                RuntimeState::NotInstalled
            });
            emit_connection(&event_callback, ConnectionState::Disconnected, None);
            return Ok(());
        }
        result = runtime_manager.install(
            &model,
            confirmed_not_recommended,
            runtime_callback.clone(),
        ) => result?,
    };
    let runtime = tokio::select! {
        biased;
        _ = wait_for_stop(&mut stop_rx) => {
            runtime_callback(RuntimeState::Stopping);
            runtime_callback(if runtime_manager.runtime_dir().exists() {
                RuntimeState::Ready
            } else {
                RuntimeState::NotInstalled
            });
            emit_connection(&event_callback, ConnectionState::Disconnected, None);
            return Ok(());
        }
        result = runtime_manager.start_with_settings(
            &config.resource_settings,
            runtime_callback.clone(),
        ) => result?,
    };
    let mut resource_settings = settings_rx.borrow_and_update().clone();
    crate::technical_log!(
        "Managed Ollama runtime listening on 127.0.0.1:{}",
        runtime.port()
    );

    let operation = async {
        let ensure_model = runtime.ensure_model_with_total(
            &model,
            runtime_install.model_download_bytes,
            runtime_install.expected_total_download_bytes,
            runtime_callback.clone(),
        );
        tokio::select! {
            biased;
            _ = wait_for_stop(&mut stop_rx) => return Ok(()),
            result = ensure_model => result?,
        }
        if *stop_rx.borrow() {
            return Ok(());
        }
        let mut backoff = ReconnectBackoff::default();
        loop {
            resource_settings = settings_rx.borrow_and_update().clone();
            resource_settings.validate()?;
            run_provider_connection(
                config.clone(),
                model.clone(),
                coordinator_url.clone(),
                ProviderConnectionContext {
                    runtime: &runtime,
                    options,
                    stop_rx: &mut stop_rx,
                    pause_rx: &mut pause_rx,
                    resource_settings: resource_settings.clone(),
                    settings_rx: &mut settings_rx,
                    system_probe: system_probe.as_ref(),
                    event_callback: &event_callback,
                    backoff: &mut backoff,
                },
            )
            .await?;

            if options.stop_after_job || options.use_cli_environment_overrides {
                return Ok(());
            }
            if *stop_rx.borrow() {
                return Ok(());
            }

            let delay = backoff.next_delay();
            crate::technical_log!(
                "Retrying coordinator connection in {} ms",
                delay.as_millis()
            );
            tokio::select! {
                biased;
                _ = wait_for_stop(&mut stop_rx) => return Ok(()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
    .await;

    let shutdown = runtime.shutdown(runtime_callback).await;
    match (operation, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => {
            emit_failure(&event_callback, &error);
            Err(error)
        }
        (Ok(()), Err(error)) => {
            emit_failure(&event_callback, &error);
            Err(error)
        }
        (Err(error), Err(shutdown_error)) => {
            emit_failure(&event_callback, &error);
            Err(error.context(format!(
                "managed runtime shutdown also failed: {shutdown_error}"
            )))
        }
    }
}

async fn run_provider_connection(
    config: ProviderConfig,
    model: String,
    coordinator_url: String,
    context: ProviderConnectionContext<'_>,
) -> Result<()> {
    let ProviderConnectionContext {
        runtime,
        options,
        stop_rx,
        pause_rx,
        settings_rx,
        mut resource_settings,
        system_probe,
        event_callback,
        backoff,
    } = context;
    let provider_id = if options.use_cli_environment_overrides {
        std::env::var("PROVIDER_ID").unwrap_or(config.provider_id.clone())
    } else {
        config.provider_id.clone()
    };
    let gpu = detect_gpu();
    let initial_sample = system_probe.sample();
    let mut policy_decision = decide_current_policy(
        &resource_settings,
        initial_sample.idle,
        initial_sample.power,
        *pause_rx.borrow(),
    );
    let accepting_jobs = matches!(&policy_decision, PolicyDecision::Run);
    event_callback(ProviderEvent::Policy {
        decision: policy_decision.clone(),
        diagnostic: initial_sample.diagnostic.clone(),
    });
    let payout_address = match config.payout_address.as_deref() {
        Some(address) => match crate::validate_payout_address(address) {
            Ok(()) => address.to_string(),
            Err(error) => {
                crate::technical_log!(
                    "WARNING: stored payout_address is invalid and will be sent as not-set: {}",
                    error
                );
                "not-set".to_string()
            }
        },
        None => "not-set".to_string(),
    };

    let Some(websocket) = connect_with_retry(
        &coordinator_url,
        !options.use_cli_environment_overrides,
        stop_rx,
        event_callback,
        backoff,
    )
    .await?
    else {
        return Ok(());
    };

    let (mut sender, mut receiver) = websocket.split();
    let mut fee_config: Option<FeeConfig> = None;
    let registration = WsMessage::Register(ProviderRegistration {
        provider_id: provider_id.clone(),
        payout_address,
        gpu_name: gpu.gpu_name,
        gpu_memory: gpu.gpu_memory,
        model: model.clone(),
        accepting_jobs,
    });
    let registration = Message::Text(
        serde_json::to_string(&registration).context("failed to serialize registration")?,
    );
    if let Err(error) = sender.send(registration).await {
        let message = format!("Coordinator connection was lost: {error}");
        crate::technical_log!("{message}");
        emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
        return Ok(());
    }

    let mut deferred_job: Option<JobPayload> = None;
    let mut pending_messages = VecDeque::new();
    let mut policy_refresh = tokio::time::interval(Duration::from_secs(2));
    let mut resource_monitor = ResourceMonitor::default();
    let mut pause_control_open = true;
    let mut settings_channel_open = true;
    'connection: loop {
        let deferred = if matches!(&policy_decision, PolicyDecision::Run) {
            deferred_job.take()
        } else {
            None
        };
        let message = if let Some(message) = pending_messages.pop_front() {
            message
        } else if let Some(job) = deferred {
            Message::Text(
                serde_json::to_string(&WsMessage::Job(job))
                    .context("failed to serialize deferred provider job")?,
            )
        } else {
            tokio::select! {
                biased;
                _ = wait_for_stop(stop_rx) => {
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
                _ = tokio::signal::ctrl_c(), if options.handle_ctrl_c => {
                    crate::technical_log!("Stopping GPU Swarm provider.");
                    break;
                }
                _ = policy_refresh.tick() => {
                    if !refresh_policy(
                        &resource_settings,
                        pause_rx,
                        system_probe,
                        &mut policy_decision,
                        &mut sender,
                        event_callback,
                    )
                    .await?
                    {
                        break 'connection;
                    }
                    event_callback(ProviderEvent::ResourceMonitor {
                        metrics: resource_monitor.sample(runtime.process_id()),
                    });
                    continue;
                }
                changed = pause_rx.changed(), if pause_control_open => {
                    if changed.is_ok() {
                        if !refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?
                        {
                            break 'connection;
                        }
                    } else {
                        pause_control_open = false;
                    }
                    continue;
                }
                changed = settings_rx.changed(), if settings_channel_open => {
                    if changed.is_ok() {
                        resource_settings = settings_rx.borrow_and_update().clone();
                        resource_settings.validate()?;
                        if !refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?
                        {
                            break 'connection;
                        }
                    } else {
                        settings_channel_open = false;
                    }
                    continue;
                }
                message = receiver.next() => {
                    match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => {
                            let message = format!("Coordinator connection was lost: {error}");
                            crate::technical_log!("{message}");
                            emit_connection(
                                event_callback,
                                ConnectionState::Disconnected,
                                Some(message),
                            );
                            break 'connection;
                        }
                        None => {
                            crate::technical_log!("Coordinator closed the WebSocket connection");
                            emit_connection(
                                event_callback,
                                ConnectionState::Disconnected,
                                None,
                            );
                            break 'connection;
                        }
                    }
                }
            }
        };

        match message {
            Message::Text(json) => {
                let message: WsMessage =
                    serde_json::from_str(&json).context("coordinator sent invalid JSON")?;
                match message {
                    WsMessage::Job(job) => {
                        if !refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?
                        {
                            break 'connection;
                        }
                        if !matches!(&policy_decision, PolicyDecision::Run) {
                            crate::technical_log!(
                                "Coordinator sent job {} while provider is paused; holding it until resume",
                                job.id
                            );
                            deferred_job = Some(job);
                            continue;
                        }
                        crate::technical_log!("Received job {}", job.id);
                        event_callback(ProviderEvent::JobStarted {
                            job_id: job.id.clone(),
                        });
                        let inference_settings = resource_settings.clone();
                        let inference = run_ollama_with_settings(
                            runtime.api_url(),
                            &job.id,
                            &provider_id,
                            &model,
                            &job.prompt,
                            &inference_settings,
                        );
                        tokio::pin!(inference);
                        let result = loop {
                            tokio::select! {
                                biased;
                                _ = wait_for_stop(stop_rx) => {
                                    let _ = sender.send(Message::Close(None)).await;
                                    break 'connection;
                                }
                                result = &mut inference => break result,
                                message = receiver.next() => {
                                    match message {
                                        Some(Ok(Message::Ping(payload))) => {
                                            if let Err(error) = sender.send(Message::Pong(payload)).await {
                                                let message = format!("Coordinator connection was lost: {error}");
                                                crate::technical_log!("{message}");
                                                emit_connection(
                                                    event_callback,
                                                    ConnectionState::Disconnected,
                                                    Some(message),
                                                );
                                                break 'connection;
                                            }
                                        }
                                        Some(Ok(Message::Pong(_))) => {}
                                        Some(Ok(Message::Close(frame))) => {
                                            let message = frame.map(|frame| {
                                                format!(
                                                    "Coordinator disconnected: code={} reason={}",
                                                    frame.code, frame.reason
                                                )
                                            });
                                            crate::technical_log!(
                                                "Coordinator disconnected during job {}; discarding its result",
                                                job.id
                                            );
                                            emit_connection(
                                                event_callback,
                                                ConnectionState::Disconnected,
                                                message,
                                            );
                                            break 'connection;
                                        }
                                        Some(Ok(message)) => pending_messages.push_back(message),
                                        Some(Err(error)) => {
                                            let message =
                                                format!("Coordinator connection was lost: {error}");
                                            crate::technical_log!(
                                                "Coordinator disconnected during job {}; discarding its result: {message}",
                                                job.id
                                            );
                                            emit_connection(
                                                event_callback,
                                                ConnectionState::Disconnected,
                                                Some(message),
                                            );
                                            break 'connection;
                                        }
                                        None => {
                                            crate::technical_log!(
                                                "Coordinator disconnected during job {}; discarding its result",
                                                job.id
                                            );
                                            emit_connection(
                                                event_callback,
                                                ConnectionState::Disconnected,
                                                None,
                                            );
                                            break 'connection;
                                        }
                                    }
                                }
                                _ = policy_refresh.tick() => {
                                    if !refresh_policy(
                                        &resource_settings,
                                        pause_rx,
                                        system_probe,
                                        &mut policy_decision,
                                        &mut sender,
                                        event_callback,
                                    )
                                    .await?
                                    {
                                        crate::technical_log!(
                                            "Coordinator disconnected during job {}; discarding its result",
                                            job.id
                                        );
                                        break 'connection;
                                    }
                                    event_callback(ProviderEvent::ResourceMonitor {
                                        metrics: resource_monitor.sample(runtime.process_id()),
                                    });
                                }
                                changed = pause_rx.changed(), if pause_control_open => {
                                    if changed.is_ok() {
                                        if !refresh_policy(
                                            &resource_settings,
                                            pause_rx,
                                            system_probe,
                                            &mut policy_decision,
                                            &mut sender,
                                            event_callback,
                                        )
                                        .await?
                                        {
                                            crate::technical_log!(
                                                "Coordinator disconnected during job {}; discarding its result",
                                                job.id
                                            );
                                            break 'connection;
                                        }
                                    } else {
                                        pause_control_open = false;
                                    }
                                }
                            }
                        };
                        if settings_channel_open {
                            match settings_rx.has_changed() {
                                Ok(true) => {
                                    resource_settings = settings_rx.borrow_and_update().clone();
                                    resource_settings.validate()?;
                                }
                                Ok(false) => {}
                                Err(_) => settings_channel_open = false,
                            }
                        }

                        let result: JobResult = match result {
                            Ok(result) => result,
                            Err(error) => JobResult {
                                job_id: job.id.clone(),
                                provider_id: provider_id.clone(),
                                result_data: String::new(),
                                duration_ms: 0,
                                eval_count: 0,
                                tokens_per_sec: 0.0,
                                error: Some(error.to_string()),
                            },
                        };

                        if !refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?
                        {
                            break 'connection;
                        }
                        let cooldown = if options.stop_after_job {
                            DutyCyclePause::PauseMs(0)
                        } else {
                            duty_cycle_pause_ms(
                                result.duration_ms,
                                i16::from(resource_settings.performance_percent),
                            )?
                        };
                        let cooldown_ms = match cooldown {
                            DutyCyclePause::NoWork => 0,
                            DutyCyclePause::PauseMs(pause_ms) => pause_ms,
                        };
                        if cooldown_ms > 0 && matches!(&policy_decision, PolicyDecision::Run) {
                            let decision = PolicyDecision::Paused(
                                "Taking a short break to limit GPU workload".to_string(),
                            );
                            let availability = Message::Text(
                                serde_json::to_string(&WsMessage::Availability {
                                    accepting_jobs: false,
                                })
                                .context("failed to serialize provider availability")?,
                            );
                            if let Err(error) = sender.send(availability).await {
                                let message = format!("Coordinator connection was lost: {error}");
                                crate::technical_log!("{message}");
                                emit_connection(
                                    event_callback,
                                    ConnectionState::Disconnected,
                                    Some(message),
                                );
                                break 'connection;
                            }
                            event_callback(ProviderEvent::Policy {
                                decision: decision.clone(),
                                diagnostic: None,
                            });
                            policy_decision = decision;
                        }
                        let result_json = serde_json::to_string(&WsMessage::Result(result.clone()))
                            .context("failed to serialize job result")?;
                        if let Err(error) = sender.send(Message::Text(result_json)).await {
                            let message = format!("Coordinator connection was lost: {error}");
                            crate::technical_log!(
                                "Discarding job {} result after send failure: {message}",
                                job.id
                            );
                            emit_connection(
                                event_callback,
                                ConnectionState::Disconnected,
                                Some(message),
                            );
                            break 'connection;
                        }
                        event_callback(ProviderEvent::JobFinished {
                            result: result.clone(),
                        });

                        if let Some(fees) = &fee_config {
                            crate::technical_log!(
                                "Job {} complete. You earned {} units (fee: {} units)",
                                job.id,
                                fees.provider_payout_units,
                                fees.job_price_units - fees.provider_payout_units
                            );
                        }
                        crate::technical_log!("Sent inference result for job {}", job.id);
                        if cooldown_ms > 0 {
                            crate::technical_log!(
                                "Performance cooldown start: job_id={} duration_ms={} pause_ms={}",
                                job.id,
                                result.duration_ms,
                                cooldown_ms
                            );
                            tokio::select! {
                                biased;
                                _ = wait_for_stop(stop_rx) => {
                                    let _ = sender.send(Message::Close(None)).await;
                                    break 'connection;
                                }
                                _ = tokio::time::sleep(Duration::from_millis(cooldown_ms)) => {}
                            }
                            crate::technical_log!("Performance cooldown end: job_id={}", job.id);
                            if !refresh_policy(
                                &resource_settings,
                                pause_rx,
                                system_probe,
                                &mut policy_decision,
                                &mut sender,
                                event_callback,
                            )
                            .await?
                            {
                                break 'connection;
                            }
                        }
                        if options.stop_after_job {
                            crate::technical_log!("One-job mode complete; stopping provider.");
                            break;
                        }
                    }
                    WsMessage::Result(result) => {
                        crate::technical_log!("Ignoring unexpected result message: {result:?}");
                    }
                    WsMessage::Register(registration) => {
                        crate::technical_log!(
                            "Ignoring unexpected provider registration: {registration:?}"
                        );
                    }
                    WsMessage::FeeConfig(fees) => {
                        crate::technical_log!(
                            "Platform fee: {} bps — you receive {} units per job.",
                            fees.fee_bps,
                            fees.provider_payout_units
                        );
                        crate::technical_log!("Platform fee recipient: {}", fees.fee_address);
                        event_callback(ProviderEvent::FeeConfig {
                            config: fees.clone(),
                        });
                        fee_config = Some(fees);
                    }
                    WsMessage::Availability { .. } => {
                        crate::technical_log!(
                            "Ignoring unexpected coordinator availability message"
                        );
                    }
                }
            }
            Message::Binary(_) => {
                crate::technical_log!("Ignoring unexpected binary message")
            }
            Message::Ping(payload) => {
                if let Err(error) = sender.send(Message::Pong(payload)).await {
                    let message = format!("Coordinator connection was lost: {error}");
                    crate::technical_log!("{message}");
                    emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
                    break 'connection;
                }
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                if let Some(frame) = frame {
                    let message = format!(
                        "Coordinator disconnected: code={} reason={}",
                        frame.code, frame.reason
                    );
                    crate::technical_log!("{message}");
                    emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
                }
                break;
            }
            Message::Frame(_) => {}
        }
    }

    emit_connection(event_callback, ConnectionState::Disconnected, None);
    Ok(())
}

type CoordinatorSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn refresh_policy(
    settings: &ResourceSettings,
    pause_rx: &watch::Receiver<bool>,
    system_probe: &dyn SystemProbe,
    current: &mut PolicyDecision,
    sender: &mut futures_util::stream::SplitSink<CoordinatorSocket, Message>,
    event_callback: &ProviderEventCallback,
) -> Result<bool> {
    let sample = system_probe.sample();
    let next = decide_current_policy(settings, sample.idle, sample.power, *pause_rx.borrow());
    if *current == next {
        return Ok(true);
    }

    let accepting_jobs = matches!(&next, PolicyDecision::Run);
    let availability = WsMessage::Availability { accepting_jobs };
    let message = Message::Text(
        serde_json::to_string(&availability)
            .context("failed to serialize provider availability")?,
    );
    if let Err(error) = sender.send(message).await {
        let message = format!("Coordinator connection was lost: {error}");
        crate::technical_log!("{message}");
        emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
        return Ok(false);
    }
    match &next {
        PolicyDecision::Run => crate::technical_log!("Provider policy: Run; accepting jobs"),
        PolicyDecision::Paused(reason) => {
            crate::technical_log!("Provider policy: Paused — {reason}")
        }
    }
    event_callback(ProviderEvent::Policy {
        decision: next.clone(),
        diagnostic: sample.diagnostic,
    });
    *current = next;
    Ok(true)
}

#[derive(Debug, Default)]
struct ReconnectBackoff {
    failed_attempts: u32,
    connected_at: Option<Instant>,
}

impl ReconnectBackoff {
    fn mark_connected(&mut self) {
        self.connected_at = Some(Instant::now());
    }

    fn next_delay(&mut self) -> Duration {
        if self
            .connected_at
            .take()
            .is_some_and(|connected_at| connected_at.elapsed() >= Duration::from_secs(30))
        {
            self.failed_attempts = 0;
        }

        let base_seconds = (1_u64 << self.failed_attempts.min(6)).min(60);
        self.failed_attempts = self.failed_attempts.saturating_add(1).min(6);

        let base_ms = base_seconds * 1_000;
        let spread_ms = base_ms / 5;
        let jitter_width = spread_ms * 2 + 1;
        let jittered_ms = base_ms - spread_ms + rand::random::<u64>() % jitter_width;
        Duration::from_millis(jittered_ms.clamp(1_000, 60_000))
    }
}

async fn connect_with_retry(
    coordinator_url: &str,
    retry: bool,
    stop_rx: &mut watch::Receiver<bool>,
    event_callback: &ProviderEventCallback,
    backoff: &mut ReconnectBackoff,
) -> Result<Option<CoordinatorSocket>> {
    loop {
        emit_connection(event_callback, ConnectionState::Connecting, None);
        crate::technical_log!("Connecting provider to {coordinator_url}");
        let result = tokio::select! {
            biased;
            _ = wait_for_stop(stop_rx) => {
                emit_connection(event_callback, ConnectionState::Disconnected, None);
                return Ok(None);
            }
            result = connect_async(coordinator_url) => result,
        };
        match result {
            Ok((websocket, _response)) => {
                crate::technical_log!("Provider connected");
                backoff.mark_connected();
                emit_connection(event_callback, ConnectionState::Connected, None);
                return Ok(Some(websocket));
            }
            Err(error) => {
                let error = anyhow::Error::from(error)
                    .context("failed to connect to coordinator; is it running?");
                if !retry {
                    emit_failure(event_callback, &error);
                    return Err(error);
                }
                let message = format!("{error:#}");
                crate::technical_log!("{message}");
                emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
                let delay = backoff.next_delay();
                crate::technical_log!(
                    "Retrying coordinator connection in {} ms",
                    delay.as_millis()
                );
                tokio::select! {
                    biased;
                    _ = wait_for_stop(stop_rx) => return Ok(None),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn wait_for_stop(stop_rx: &mut watch::Receiver<bool>) {
    loop {
        if *stop_rx.borrow() || stop_rx.changed().await.is_err() {
            return;
        }
    }
}

fn emit_connection(
    callback: &ProviderEventCallback,
    state: ConnectionState,
    message: Option<String>,
) {
    callback(ProviderEvent::Connection { state, message });
}

fn emit_failure(callback: &ProviderEventCallback, error: &anyhow::Error) {
    callback(ProviderEvent::Error {
        message: format!("{error:#}"),
    });
    emit_connection(
        callback,
        ConnectionState::Disconnected,
        Some(format!("{error:#}")),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        connect_with_retry, provider_control_channel, run_provider_managed, ConnectionState,
        ProviderConfig, ProviderEvent, ProviderEventCallback, ReconnectBackoff,
        RuntimeProgressCallback, RuntimeState,
    };
    use super::{select_network_model, CoordinatorFeesResponse};
    use crate::{ResourceMode, ResourceSettings};
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use tokio::{net::TcpListener, sync::watch, time::timeout};
    use tokio_tungstenite::accept_async;

    #[test]
    fn coordinator_model_selection_wins_unless_explicitly_overridden() {
        let configured = crate::OllamaModelSelection {
            configured: "llama3.2:3b".to_string(),
            effective: "llama3.2:3b".to_string(),
            environment_override: false,
        };
        assert_eq!(
            select_network_model(&configured, Some("llama3.2:1b")).unwrap(),
            "llama3.2:1b"
        );
        assert_eq!(
            select_network_model(&configured, None).unwrap(),
            "llama3.2:3b"
        );
        assert!(select_network_model(&configured, Some("../invalid")).is_err());

        let explicit_override = crate::OllamaModelSelection {
            configured: "llama3.2:3b".to_string(),
            effective: "llama3.2:1b".to_string(),
            environment_override: true,
        };
        assert_eq!(
            select_network_model(&explicit_override, Some("llama3.2:3b")).unwrap(),
            "llama3.2:1b"
        );
    }

    #[test]
    fn older_coordinator_fees_response_uses_saved_model() {
        let fees: CoordinatorFeesResponse = serde_json::from_str(r#"{"fee_bps":1000}"#).unwrap();
        assert_eq!(fees.network_model, None);
    }

    #[tokio::test]
    async fn provider_control_publishes_valid_resource_settings_updates() {
        let (control, _) = provider_control_channel();
        let mut settings_rx = control.subscribe_resource_settings();
        let settings = ResourceSettings {
            mode: ResourceMode::AlwaysOnLowPriority,
            idle_threshold_minutes: 1,
            ..ResourceSettings::default()
        };

        control.update_resource_settings(settings.clone()).unwrap();
        settings_rx.changed().await.unwrap();

        assert_eq!(*settings_rx.borrow_and_update(), settings);
        assert!(control
            .update_resource_settings(ResourceSettings {
                idle_threshold_minutes: 0,
                ..ResourceSettings::default()
            })
            .is_err());
    }

    #[tokio::test]
    async fn managed_provider_honors_stop_before_runtime_install() {
        let (stop, stop_rx) = watch::channel(true);
        let runtime_states = Arc::new(Mutex::new(Vec::new()));
        let observed_runtime = runtime_states.clone();
        let runtime_callback: RuntimeProgressCallback =
            Arc::new(move |state| observed_runtime.lock().unwrap().push(state));
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed_events = events.clone();
        let event_callback: ProviderEventCallback =
            Arc::new(move |event| observed_events.lock().unwrap().push(event));

        run_provider_managed(
            ProviderConfig::defaults(),
            false,
            stop_rx,
            runtime_callback,
            event_callback,
        )
        .await
        .unwrap();
        drop(stop);

        let runtime_states = runtime_states.lock().unwrap();
        assert_eq!(runtime_states.first(), Some(&RuntimeState::Stopping));
        assert!(matches!(
            runtime_states.last(),
            Some(RuntimeState::Ready | RuntimeState::NotInstalled)
        ));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            ProviderEvent::Connection {
                state: ConnectionState::Disconnected,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn managed_connection_recovers_when_coordinator_returns() {
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);

        let events = Arc::new(Mutex::new(Vec::new()));
        let observed_events = events.clone();
        let event_callback: ProviderEventCallback =
            Arc::new(move |event| observed_events.lock().unwrap().push(event));
        let url = format!("ws://{address}/ws");
        let connector = tokio::spawn(async move {
            let (_stop, mut stop_rx) = watch::channel(false);
            connect_with_retry(
                &url,
                true,
                &mut stop_rx,
                &event_callback,
                &mut ReconnectBackoff::default(),
            )
            .await
        });

        timeout(Duration::from_secs(3), async {
            loop {
                if events.lock().unwrap().iter().any(|event| {
                    matches!(
                        event,
                        ProviderEvent::Connection {
                            state: ConnectionState::Disconnected,
                            ..
                        }
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed port should produce an initial connection failure");

        let listener = TcpListener::bind(address).await.unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            accept_async(socket).await.unwrap();
        });
        let socket = connector.await.unwrap().unwrap().unwrap();
        drop(socket);
        server.await.unwrap();

        let states: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::Connection { state, .. } => Some(state.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            [
                ConnectionState::Connecting,
                ConnectionState::Disconnected,
                ConnectionState::Connecting,
                ConnectionState::Connected,
            ]
        );
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            ProviderEvent::Connection {
                state: ConnectionState::Disconnected,
                message: Some(message),
            } if message.contains("failed to connect to coordinator")
        )));
    }

    #[test]
    fn reconnect_backoff_grows_caps_and_resets_after_a_stable_connection() {
        let mut backoff = ReconnectBackoff::default();
        let ranges = [
            (1_000, 1_200),
            (1_600, 2_400),
            (3_200, 4_800),
            (6_400, 9_600),
            (12_800, 19_200),
            (25_600, 38_400),
            (48_000, 60_000),
            (48_000, 60_000),
        ];

        for (minimum, maximum) in ranges {
            let delay_ms = backoff.next_delay().as_millis() as u64;
            assert!((minimum..=maximum).contains(&delay_ms));
        }

        backoff.connected_at = Some(Instant::now() - Duration::from_secs(31));
        let reset_delay_ms = backoff.next_delay().as_millis() as u64;
        assert!((1_000..=1_200).contains(&reset_delay_ms));
    }
}
