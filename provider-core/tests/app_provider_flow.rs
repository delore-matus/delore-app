use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use provider_core::{
    run_provider_managed, ConnectionState, ProviderConfig, ProviderEvent, ProviderEventCallback,
    ResourceMode, ResourceSettings, RuntimeManager, RuntimeProgressCallback, RuntimeState,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

const FEE_ADDRESS: &str = "0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850";

#[tokio::test]
#[ignore = "requires a running local coordinator and cached llama3.2:1b runtime model"]
async fn app_managed_provider_runs_a_job_and_stops() {
    let runtime = RuntimeManager::new().unwrap();
    assert!(
        runtime.runtime_dir().exists(),
        "managed runtime is not installed"
    );
    assert!(
        runtime
            .models_dir()
            .join("manifests/registry.ollama.ai/library/llama3.2/1b")
            .exists(),
        "cached llama3.2:1b model is required"
    );
    println!(
        "managed-runtime executable={}",
        runtime.executable_path().unwrap().display()
    );

    let coordinator_http =
        std::env::var("GPUSWARM_E2E_HTTP").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let coordinator_ws =
        std::env::var("GPUSWARM_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081/ws".to_string());
    let provider_id = format!("desktop-e2e-{}", uuid::Uuid::new_v4());
    let config = ProviderConfig {
        payout_address: Some("0x1111111111111111111111111111111111111111".to_string()),
        provider_id,
        coordinator_ws,
        ollama_model: "llama3.2:1b".to_string(),
        resource_settings: ResourceSettings {
            mode: ResourceMode::AlwaysOnLowPriority,
            ..ResourceSettings::default()
        },
    };

    let (stop, stop_rx) = watch::channel(false);
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let runtime_callback: RuntimeProgressCallback = Arc::new(move |state| {
        let _ = runtime_tx.send(state);
    });
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_callback: ProviderEventCallback = Arc::new(move |event| {
        let _ = event_tx.send(event);
    });
    let provider = tokio::spawn(run_provider_managed(
        config,
        true,
        stop_rx,
        runtime_callback,
        event_callback,
    ));

    let startup_deadline = Instant::now() + Duration::from_secs(120);
    let mut runtime_states = Vec::new();
    let mut connected = false;
    let mut fee_config = None;
    while !connected || fee_config.is_none() {
        assert!(
            Instant::now() < startup_deadline,
            "provider did not connect and receive FeeConfig"
        );
        tokio::select! {
            state = runtime_rx.recv() => {
                if let Some(state) = state {
                    println!("provider-status runtime={}", serde_json::to_string(&state).unwrap());
                    runtime_states.push(state);
                }
            }
            event = event_rx.recv() => {
                if let Some(event) = event {
                    match event {
                        ProviderEvent::Connection { state, message } => {
                            println!("provider-status connection={state:?} message={message:?}");
                            connected = matches!(state, ConnectionState::Connected);
                        }
                        ProviderEvent::FeeConfig { config } => {
                            println!(
                                "provider-status fee_bps={} fee_address={}",
                                config.fee_bps, config.fee_address
                            );
                            fee_config = Some(config);
                        }
                        event => println!("provider-status event={event:?}"),
                    }
                }
            }
        }
    }

    let fee_config = fee_config.unwrap();
    assert_eq!(fee_config.fee_bps, 1000);
    assert_eq!(fee_config.fee_address, FEE_ADDRESS);
    while let Ok(state) = runtime_rx.try_recv() {
        println!(
            "provider-status runtime={}",
            serde_json::to_string(&state).unwrap()
        );
        runtime_states.push(state);
    }
    assert!(runtime_states
        .iter()
        .any(|state| matches!(state, RuntimeState::Ready)));
    assert!(runtime_states
        .iter()
        .any(|state| matches!(state, RuntimeState::Starting)));
    assert!(runtime_states
        .iter()
        .any(|state| matches!(state, RuntimeState::Running)));

    let job_id = format!("desktop-e2e-job-{}", uuid::Uuid::new_v4());
    let earnings_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("earnings.log");
    let earnings_start = fs::metadata(&earnings_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let response = reqwest::Client::new()
        .post(format!("{coordinator_http}/request-job"))
        .header("X-Payment", "mock")
        .json(&serde_json::json!({
            "id": &job_id,
            "prompt": "Respond with exactly: ok"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(accepted["job_id"], job_id);

    let job_deadline = Instant::now() + Duration::from_secs(180);
    let mut saw_running = false;
    let mut finished = false;
    while !finished {
        assert!(
            Instant::now() < job_deadline,
            "provider did not finish the job"
        );
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await
        else {
            continue;
        };
        match event {
            ProviderEvent::JobStarted { job_id: started_id } if started_id == job_id => {
                saw_running = true;
                println!("provider-status job=running id={started_id}");
            }
            ProviderEvent::JobFinished { result } if result.job_id == job_id => {
                assert_eq!(result.error, None);
                assert_eq!(result.result_data.trim(), "ok");
                finished = true;
                println!(
                    "provider-status job=finished id={} result={:?}",
                    result.job_id, result.result_data
                );
            }
            event => println!("provider-status event={event:?}"),
        }
    }
    assert!(saw_running);

    let result: Value = reqwest::Client::new()
        .get(format!("{coordinator_http}/jobs/{job_id}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(
        result["result"]["result_data"].as_str().unwrap().trim(),
        "ok"
    );
    println!("GET /jobs/{job_id} {result}");

    let earnings_deadline = Instant::now() + Duration::from_secs(5);
    let earnings_line = loop {
        let contents = fs::read_to_string(&earnings_path).unwrap_or_default();
        let appended = contents
            .as_bytes()
            .get(earnings_start as usize..)
            .map(String::from_utf8_lossy)
            .map(|text| text.into_owned())
            .unwrap_or_default();
        if let Some(line) = appended.lines().find(|line| line.contains(&job_id)) {
            break line.to_string();
        }
        assert!(
            Instant::now() < earnings_deadline,
            "earnings.log did not record the completed job"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(earnings_line.contains(&format!("fee_address={FEE_ADDRESS}")));
    assert!(earnings_line.contains("gross_price=10000"));
    assert!(earnings_line.contains("fee_amount=1000"));
    println!("earnings.log {earnings_line}");

    stop.send(true).unwrap();
    provider.await.unwrap().unwrap();
    println!("provider-status provider=stopped");
}
