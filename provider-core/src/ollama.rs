use std::time::Instant;

use anyhow::{Context, Result};
use common::JobResult;
use serde::{Deserialize, Serialize};

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
) -> OllamaGenerateRequest {
    let mapping = settings.preset.mapping(
        logical_cpu_count,
        settings.mode == crate::resource::ResourceMode::AlwaysOnLowPriority,
    );
    OllamaGenerateRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
        stream: false,
        keep_alive: mapping.keep_alive.to_string(),
        options: OllamaModelOptions {
            num_thread: mapping.num_thread,
        },
    }
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
    let request = ollama_request(model, prompt, settings, logical_cpu_count);

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
    use super::ollama_request;
    use crate::resource::{ResourcePreset, ResourceSettings};

    #[test]
    fn inference_request_applies_threads_and_live_keep_alive_preset() {
        let settings = ResourceSettings {
            preset: ResourcePreset::Low,
            ..ResourceSettings::default()
        };
        let request =
            serde_json::to_value(ollama_request("model", "prompt", &settings, 8)).unwrap();
        assert_eq!(request["options"]["num_thread"], 1);
        assert_eq!(request["keep_alive"], "0");

        let balanced = ResourceSettings::default();
        let request =
            serde_json::to_value(ollama_request("model", "prompt", &balanced, 8)).unwrap();
        assert_eq!(request["options"]["num_thread"], 4);
        assert_eq!(request["keep_alive"], "5m");
    }
}
