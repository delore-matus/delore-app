use std::time::Instant;

use anyhow::{Context, Result};
use common::JobResult;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct OllamaGenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: Option<String>,
    eval_count: Option<u32>,
    eval_duration: Option<u64>,
}

pub async fn run_ollama(
    api_url: &str,
    job_id: &str,
    provider_id: &str,
    model: &str,
    prompt: &str,
) -> Result<JobResult> {
    let started = Instant::now();
    let http = reqwest::Client::new();
    let request = OllamaGenerateRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
        stream: false,
    };

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
