use std::sync::Arc;

use anyhow::{Context, Result};
use common::{FeeConfig, JobResult, ProviderRegistration, WsMessage};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    detect_gpu, run_ollama, ProviderConfig, RunningRuntime, RuntimeManager,
    RuntimeProgressCallback, RuntimeState,
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
    Error {
        message: String,
    },
}

pub type ProviderEventCallback = Arc<dyn Fn(ProviderEvent) + Send + Sync + 'static>;

#[derive(Clone, Copy)]
struct ProviderRunOptions {
    confirmed_not_recommended: bool,
    stop_after_job: bool,
    handle_ctrl_c: bool,
    use_environment_overrides: bool,
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
    let result = run_provider_inner(
        config,
        ProviderRunOptions {
            confirmed_not_recommended,
            stop_after_job,
            handle_ctrl_c: true,
            use_environment_overrides: true,
        },
        stop_rx,
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
    run_provider_inner(
        config,
        ProviderRunOptions {
            confirmed_not_recommended,
            stop_after_job: false,
            handle_ctrl_c: false,
            use_environment_overrides: false,
        },
        stop_rx,
        runtime_callback,
        event_callback,
    )
    .await
}

async fn run_provider_inner(
    config: ProviderConfig,
    options: ProviderRunOptions,
    mut stop_rx: watch::Receiver<bool>,
    runtime_callback: RuntimeProgressCallback,
    event_callback: ProviderEventCallback,
) -> Result<()> {
    let model = if options.use_environment_overrides {
        std::env::var("OLLAMA_MODEL").unwrap_or(config.ollama_model.clone())
    } else {
        config.ollama_model.clone()
    };
    let runtime_manager = RuntimeManager::new()?;
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
            options.confirmed_not_recommended,
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
        result = runtime_manager.start(runtime_callback.clone()) => result?,
    };
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
            &runtime,
            options,
            &mut stop_rx,
            &event_callback,
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
    runtime: &RunningRuntime,
    options: ProviderRunOptions,
    stop_rx: &mut watch::Receiver<bool>,
    event_callback: &ProviderEventCallback,
) -> Result<()> {
    let coordinator_url = if options.use_environment_overrides {
        std::env::var("COORDINATOR_WS").unwrap_or(config.coordinator_ws.clone())
    } else {
        config.coordinator_ws.clone()
    };
    let provider_id = if options.use_environment_overrides {
        std::env::var("PROVIDER_ID").unwrap_or(config.provider_id.clone())
    } else {
        config.provider_id.clone()
    };
    let gpu = detect_gpu();
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
        !options.use_environment_overrides,
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
    });
    sender
        .send(Message::Text(
            serde_json::to_string(&registration).context("failed to serialize registration")?,
        ))
        .await
        .context("failed to send provider registration")?;

    'connection: loop {
        let message = tokio::select! {
            biased;
            _ = wait_for_stop(stop_rx) => {
                let _ = sender.send(Message::Close(None)).await;
                break;
            }
            _ = tokio::signal::ctrl_c(), if options.handle_ctrl_c => {
                println!("Stopping GPU Swarm provider.");
                break;
            }
            message = receiver.next() => {
                match message {
                    Some(message) => message.context("failed to read WebSocket message")?,
                    None => break,
                }
            }
        };

        match message {
            Message::Text(json) => {
                let message: WsMessage =
                    serde_json::from_str(&json).context("coordinator sent invalid JSON")?;
                match message {
                    WsMessage::Job(job) => {
                        println!("Received job");
                        event_callback(ProviderEvent::JobStarted {
                            job_id: job.id.clone(),
                        });
                        let inference = run_ollama(
                            runtime.api_url(),
                            &job.id,
                            &provider_id,
                            &model,
                            &job.prompt,
                        );
                        let result = tokio::select! {
                            biased;
                            _ = wait_for_stop(stop_rx) => {
                                let _ = sender.send(Message::Close(None)).await;
                                break 'connection;
                            }
                            result = inference => result,
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
        connect_with_retry, run_provider_managed, ConnectionState, ProviderConfig, ProviderEvent,
        ProviderEventCallback, RuntimeProgressCallback, RuntimeState,
    };
    use std::sync::{Arc, Mutex};
    use tokio::{net::TcpListener, sync::watch, time::timeout};
    use tokio_tungstenite::accept_async;

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
