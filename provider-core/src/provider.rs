use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use common::{FeeConfig, JobPayload, JobResult, ProviderRegistration, WsMessage};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    decide_current_policy, detect_gpu, effective_ollama_model, ensure_provider_confirmation,
    run_ollama_with_settings, runtime_suitability, save_config, NativeSystemProbe, PolicyDecision,
    ProviderConfig, ResourceMetrics, ResourceMonitor, ResourceSettings, RunningRuntime,
    RuntimeManager, RuntimeProgressCallback, RuntimeState, RuntimeSuitability, SystemProbe,
};

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
    let model = effective_ollama_model(&config)?.effective;
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
        save_config(&config)?;
    }
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
    let resource_settings = settings_rx.borrow_and_update().clone();
    println!(
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
        run_provider_connection(
            config,
            model,
            ProviderConnectionContext {
                runtime: &runtime,
                options,
                stop_rx: &mut stop_rx,
                pause_rx: &mut pause_rx,
                resource_settings,
                settings_rx: &mut settings_rx,
                system_probe: system_probe.as_ref(),
                event_callback: &event_callback,
            },
        )
        .await
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
    } = context;
    let coordinator_url = if options.use_cli_environment_overrides {
        std::env::var("COORDINATOR_WS").unwrap_or(config.coordinator_ws.clone())
    } else {
        config.coordinator_ws.clone()
    };
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
                eprintln!(
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
    sender
        .send(Message::Text(
            serde_json::to_string(&registration).context("failed to serialize registration")?,
        ))
        .await
        .context("failed to send provider registration")?;

    let mut deferred_job: Option<JobPayload> = None;
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
        let message = if let Some(job) = deferred {
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
                    println!("Stopping GPU Swarm provider.");
                    break;
                }
                _ = policy_refresh.tick() => {
                    refresh_policy(
                        &resource_settings,
                        pause_rx,
                        system_probe,
                        &mut policy_decision,
                        &mut sender,
                        event_callback,
                    )
                    .await?;
                    event_callback(ProviderEvent::ResourceMonitor {
                        metrics: resource_monitor.sample(runtime.process_id()),
                    });
                    continue;
                }
                changed = pause_rx.changed(), if pause_control_open => {
                    if changed.is_ok() {
                        refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?;
                    } else {
                        pause_control_open = false;
                    }
                    continue;
                }
                changed = settings_rx.changed(), if settings_channel_open => {
                    if changed.is_ok() {
                        resource_settings = settings_rx.borrow_and_update().clone();
                        resource_settings.validate()?;
                        refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?;
                    } else {
                        settings_channel_open = false;
                    }
                    continue;
                }
                message = receiver.next() => {
                    match message {
                        Some(message) => message.context("failed to read WebSocket message")?,
                        None => break,
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
                        refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?;
                        if !matches!(&policy_decision, PolicyDecision::Run) {
                            println!(
                                "Coordinator sent job {} while provider is paused; holding it until resume",
                                job.id
                            );
                            deferred_job = Some(job);
                            continue;
                        }
                        println!("Received job");
                        event_callback(ProviderEvent::JobStarted {
                            job_id: job.id.clone(),
                        });
                        let inference = run_ollama_with_settings(
                            runtime.api_url(),
                            &job.id,
                            &provider_id,
                            &model,
                            &job.prompt,
                            &resource_settings,
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
                                _ = policy_refresh.tick() => {
                                    refresh_policy(
                                        &resource_settings,
                                        pause_rx,
                                        system_probe,
                                        &mut policy_decision,
                                        &mut sender,
                                        event_callback,
                                    )
                                    .await?;
                                    event_callback(ProviderEvent::ResourceMonitor {
                                        metrics: resource_monitor.sample(runtime.process_id()),
                                    });
                                }
                                changed = pause_rx.changed(), if pause_control_open => {
                                    if changed.is_ok() {
                                        refresh_policy(
                                            &resource_settings,
                                            pause_rx,
                                            system_probe,
                                            &mut policy_decision,
                                            &mut sender,
                                            event_callback,
                                        )
                                        .await?;
                                    } else {
                                        pause_control_open = false;
                                    }
                                }
                            }
                        };

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

                        refresh_policy(
                            &resource_settings,
                            pause_rx,
                            system_probe,
                            &mut policy_decision,
                            &mut sender,
                            event_callback,
                        )
                        .await?;
                        let result_json = serde_json::to_string(&WsMessage::Result(result.clone()))
                            .context("failed to serialize job result")?;
                        sender
                            .send(Message::Text(result_json))
                            .await
                            .context("failed to send job result")?;
                        event_callback(ProviderEvent::JobFinished { result });

                        if let Some(fees) = &fee_config {
                            println!(
                                "Job {} complete. You earned {} units (fee: {} units)",
                                job.id,
                                fees.provider_payout_units,
                                fees.job_price_units - fees.provider_payout_units
                            );
                        }
                        println!("Sent inference result for job {}", job.id);
                        if options.stop_after_job {
                            println!("One-job mode complete; stopping provider.");
                            break;
                        }
                    }
                    WsMessage::Result(result) => {
                        println!("Ignoring unexpected result message: {result:?}");
                    }
                    WsMessage::Register(registration) => {
                        println!("Ignoring unexpected provider registration: {registration:?}");
                    }
                    WsMessage::FeeConfig(fees) => {
                        println!(
                            "Platform fee: {} bps — you receive {} units per job.",
                            fees.fee_bps, fees.provider_payout_units
                        );
                        println!("Platform fee recipient: {}", fees.fee_address);
                        event_callback(ProviderEvent::FeeConfig {
                            config: fees.clone(),
                        });
                        fee_config = Some(fees);
                    }
                    WsMessage::Availability { .. } => {
                        println!("Ignoring unexpected coordinator availability message");
                    }
                }
            }
            Message::Binary(_) => println!("Ignoring unexpected binary message"),
            Message::Ping(payload) => {
                sender
                    .send(Message::Pong(payload))
                    .await
                    .context("failed to send Pong")?;
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                if let Some(frame) = frame {
                    let message = format!(
                        "Coordinator disconnected: code={} reason={}",
                        frame.code, frame.reason
                    );
                    println!("{message}");
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
) -> Result<()> {
    let sample = system_probe.sample();
    let next = decide_current_policy(settings, sample.idle, sample.power, *pause_rx.borrow());
    if *current == next {
        return Ok(());
    }

    let accepting_jobs = matches!(&next, PolicyDecision::Run);
    let availability = WsMessage::Availability { accepting_jobs };
    sender
        .send(Message::Text(
            serde_json::to_string(&availability)
                .context("failed to serialize provider availability")?,
        ))
        .await
        .context("failed to send provider availability")?;
    match &next {
        PolicyDecision::Run => println!("Provider policy: Run; accepting jobs"),
        PolicyDecision::Paused(reason) => println!("Provider policy: Paused — {reason}"),
    }
    event_callback(ProviderEvent::Policy {
        decision: next.clone(),
        diagnostic: sample.diagnostic,
    });
    *current = next;
    Ok(())
}

async fn connect_with_retry(
    coordinator_url: &str,
    retry: bool,
    stop_rx: &mut watch::Receiver<bool>,
    event_callback: &ProviderEventCallback,
) -> Result<Option<CoordinatorSocket>> {
    loop {
        emit_connection(event_callback, ConnectionState::Connecting, None);
        println!("Connecting provider to {coordinator_url}");
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
                println!("Provider connected");
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
                eprintln!("{message}");
                emit_connection(event_callback, ConnectionState::Disconnected, Some(message));
                tokio::select! {
                    biased;
                    _ = wait_for_stop(stop_rx) => return Ok(None),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
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
        ProviderConfig, ProviderEvent, ProviderEventCallback, RuntimeProgressCallback,
        RuntimeState,
    };
    use crate::{ResourceMode, ResourceSettings};
    use std::sync::{Arc, Mutex};
    use tokio::{net::TcpListener, sync::watch, time::timeout};
    use tokio_tungstenite::accept_async;

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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            drop(first);

            let (second, _) = timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            accept_async(second).await.unwrap();
        });

        let events = Arc::new(Mutex::new(Vec::new()));
        let observed_events = events.clone();
        let event_callback: ProviderEventCallback =
            Arc::new(move |event| observed_events.lock().unwrap().push(event));
        let (_stop, mut stop_rx) = watch::channel(false);
        let socket = connect_with_retry(
            &format!("ws://{address}/ws"),
            true,
            &mut stop_rx,
            &event_callback,
        )
        .await
        .unwrap()
        .unwrap();
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
}
