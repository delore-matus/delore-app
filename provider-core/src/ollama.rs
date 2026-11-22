use std::time::Instant;

use anyhow::{Context, Result};
use common::JobResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::resource::ResourceSettings;

#[derive(Serialize)]
struct OllamaGenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
    keep_alive: String,
    options: OllamaModelOptions,
}

#[derive(Serialize)]
struct OllamaModelOptions {
    num_thread: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_gpu: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: Option<String>,
    eval_count: Option<u32>,
    eval_duration: Option<u64>,
}

fn ollama_request(
    model: &str,
    prompt: &str,
    settings: &ResourceSettings,
    logical_cpu_count: usize,
    num_gpu: Option<usize>,
) -> OllamaGenerateRequest {
    let mapping = settings.preset.mapping(
        logical_cpu_count,
        settings.mode == crate::resource::ResourceMode::AlwaysOnLowPriority
            || settings.below_normal_priority,
    );
    OllamaGenerateRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
        stream: false,
        keep_alive: mapping.keep_alive.to_string(),
        options: OllamaModelOptions {
            num_thread: mapping.num_thread,
            num_gpu,
        },
    }
}

fn gpu_layers_for_performance(total_layers: usize, performance_percent: u8) -> Option<usize> {
    if total_layers == 0 || performance_percent == 0 || performance_percent > 100 {
        return None;
    }
    let requested = total_layers
        .saturating_mul(usize::from(performance_percent))
        .saturating_add(99)
        / 100;
    Some(requested.clamp(1, total_layers))
}

async fn model_layer_count(
    http: &reqwest::Client,
    api_url: &str,
    model: &str,
) -> Result<Option<usize>> {
    let response = http
        .post(format!("{}/api/show", api_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .context("failed to query model layer metadata")?
        .error_for_status()
        .context("model layer metadata request was rejected")?;
    let details: Value = response
        .json()
        .await
        .context("failed to parse model layer metadata")?;
    let Some(model_info) = details.get("model_info").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(architecture) = model_info
        .get("general.architecture")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let block_count_key = format!("{architecture}.block_count");
    let block_count = model_info
        .get(&block_count_key)
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok());
    Ok(block_count)
}

pub async fn run_ollama(
    api_url: &str,
    job_id: &str,
    provider_id: &str,
    model: &str,
    prompt: &str,
) -> Result<JobResult> {
    let settings = ResourceSettings::default();
    run_ollama_with_settings(api_url, job_id, provider_id, model, prompt, &settings).await
}

pub async fn run_ollama_with_settings(
    api_url: &str,
    job_id: &str,
    provider_id: &str,
    model: &str,
    prompt: &str,
    settings: &ResourceSettings,
) -> Result<JobResult> {
    let started = Instant::now();
    let http = reqwest::Client::new();
    let logical_cpu_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let gpu_support = crate::detect_gpu().support;
    let num_gpu = if settings.performance_percent > 0
        && matches!(
            gpu_support.as_str(),
            "nvidia-supported" | "apple-silicon-best-effort"
        ) {
        match model_layer_count(&http, api_url, model).await {
            Ok(Some(total_layers)) => {
                gpu_layers_for_performance(total_layers, settings.performance_percent)
            }
            Ok(None) => {
                crate::technical_log!(
                    "No layer count was reported for model {model}; leaving GPU layer selection automatic."
                );
                None
            }
            Err(error) => {
                crate::technical_log!(
                    "Could not read layer count for model {model}; leaving GPU layer selection automatic: {error:#}"
                );
                None
            }
        }
    } else {
        None
    };
    let request = ollama_request(model, prompt, settings, logical_cpu_count, num_gpu);

    let response = http
        .post(format!("{}/api/generate", api_url.trim_end_matches('/')))
        .json(&request)
        .send()
        .await
        .context("Ollama is unavailable or the model is missing")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Ollama response")?;

    if !status.is_success() {
        return Ok(JobResult {
            job_id: job_id.to_string(),
            provider_id: provider_id.to_string(),
            result_data: String::new(),
            duration_ms: started.elapsed().as_millis(),
            eval_count: 0,
            tokens_per_sec: 0.0,
            error: Some(format!(
                "Ollama request failed (HTTP {}): {}",
                status,
                body.trim()
            )),
        });
    }

    match serde_json::from_str::<OllamaGenerateResponse>(&body) {
        Ok(ollama) => {
            let duration_ms = started.elapsed().as_millis();
            let eval_count = ollama.eval_count.unwrap_or(0);
            let eval_duration = ollama.eval_duration.unwrap_or(0);
            let tokens_per_sec = if eval_duration == 0 {
                0.0
            } else {
                eval_count as f64 / (eval_duration as f64 / 1_000_000_000.0)
            };
            Ok(JobResult {
                job_id: job_id.to_string(),
                provider_id: provider_id.to_string(),
                result_data: ollama.response.unwrap_or_default(),
                duration_ms,
                eval_count,
                tokens_per_sec,
                error: None,
            })
        }

        Err(error) => Ok(JobResult {
            job_id: job_id.to_string(),
            provider_id: provider_id.to_string(),
            result_data: String::new(),
            duration_ms: started.elapsed().as_millis(),
            eval_count: 0,
            tokens_per_sec: 0.0,
            error: Some(format!("Invalid Ollama response: {error}")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{gpu_layers_for_performance, ollama_request};
    use crate::resource::{ResourcePreset, ResourceSettings};

    #[test]
    fn inference_request_applies_threads_and_live_keep_alive_preset() {
        let settings = ResourceSettings {
            preset: ResourcePreset::Low,
            ..ResourceSettings::default()
        };
        let request =
            serde_json::to_value(ollama_request("model", "prompt", &settings, 8, None)).unwrap();
        assert_eq!(request["options"]["num_thread"], 1);
        assert!(request["options"].get("num_gpu").is_none());
        assert_eq!(request["keep_alive"], "0");

        let balanced = ResourceSettings::default();
        let request =
            serde_json::to_value(ollama_request("model", "prompt", &balanced, 8, Some(10)))
                .unwrap();
        assert_eq!(request["options"]["num_thread"], 4);
        assert_eq!(request["options"]["num_gpu"], 10);
        assert_eq!(request["keep_alive"], "5m");
    }

    #[test]
    fn gpu_layer_fraction_rounds_up_within_model_bounds() {
        assert_eq!(gpu_layers_for_performance(40, 50), Some(20));
        assert_eq!(gpu_layers_for_performance(13, 1), Some(1));
        assert_eq!(gpu_layers_for_performance(13, 10), Some(2));
        assert_eq!(gpu_layers_for_performance(13, 99), Some(13));
        assert_eq!(gpu_layers_for_performance(13, 100), Some(13));
        assert_eq!(gpu_layers_for_performance(0, 50), None);
        assert_eq!(gpu_layers_for_performance(13, 0), None);
        assert_eq!(gpu_layers_for_performance(13, 101), None);
    }
}
