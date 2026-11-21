use anyhow::{Context, Result};
use common::{FeeConfig, JobResult, ProviderRegistration, WsMessage};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{detect_gpu, run_ollama, ProviderConfig};

#[derive(Debug, Clone)]
pub struct JobHistoryEntry {
    pub job_id: String,
    pub provider_id: String,
    pub status: String,
    pub result_data: String,
    pub error: Option<String>,
    pub duration_ms: u128,
}

pub async fn run_provider(config: ProviderConfig) -> Result<()> {
    let coordinator_url =
        std::env::var("COORDINATOR_WS").unwrap_or(config.coordinator_ws.clone());
    let provider_id =
        std::env::var("PROVIDER_ID").unwrap_or(config.provider_id.clone());
    let model =
        std::env::var("OLLAMA_MODEL").unwrap_or(config.ollama_model.clone());
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

    println!("Connecting provider to {coordinator_url}");
    let (websocket, _response) = connect_async(coordinator_url)
        .await
        .context("failed to connect to coordinator; is it running?")?;
    println!("Provider connected");

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
            serde_json::to_string(&registration)
                .context("failed to serialize registration")?,
        ))
        .await
        .context("failed to send provider registration")?;

    loop {
        let message = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
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
                let message: WsMessage = serde_json::from_str(&json)
                    .context("coordinator sent invalid JSON")?;
                match message {
                    WsMessage::Job(job) => {
                        println!("Received job");
                        let result = run_ollama(
                            &job.id,
                            &provider_id,
                            &model,
                            &job.prompt,
                        )
                        .await;

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

                        let result_json =
                            serde_json::to_string(&WsMessage::Result(result))
                                .context("failed to serialize job result")?;
                        sender
                            .send(Message::Text(result_json))
                            .await
                            .context("failed to send job result")?;

                        if let Some(fees) = &fee_config {
                            println!(
                                "Job {} complete. You earned {} units (fee: {} units)",
                                job.id,
                                fees.provider_payout_units,
                                fees.job_price_units - fees.provider_payout_units
                            );
                        }
                        println!("Sent inference result for job {}", job.id);
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
                            fees.fee_bps,
                            fees.provider_payout_units
                        );
                        println!("Platform fee recipient: {}", fees.fee_address);
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
                    println!(
                        "Coordinator disconnected: code={} reason={}",
                        frame.code, frame.reason
                    );
                } else {
                    println!("Coordinator disconnected");
                }
                break;
            }
            Message::Frame(_) => {}
        }
    }

    Ok(())
}
