use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use provider_core::{
    run_provider_managed, ConnectionState, ProviderConfig, ProviderEvent, ProviderEventCallback,
    ResourceMode, ResourceSettings, RuntimeManager, RuntimeProgressCallback, RuntimeState,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

#[path = "support/coordinator.rs"]
mod coordinator;

const FEE_ADDRESS: &str = "0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850";

fn utc_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[tokio::test]
#[ignore = "requires the installed provider runtime and cached llama3.2:1b model"]
async fn managed_provider_runs_a_job_and_stops() {
    let coordinator = coordinator::OwnedCoordinator::start()
        .await
        .expect("test coordinator should start on isolated ports");
    let coordinator_http = coordinator.http_url.clone();
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

    let provider_id = format!("desktop-e2e-{}", uuid::Uuid::new_v4());
    let config = ProviderConfig {
        payout_address: Some("0x1111111111111111111111111111111111111111".to_string()),
        provider_id: provider_id.clone(),
        auth_token: uuid::Uuid::new_v4().to_string(),
        coordinator_ws: coordinator.ws_url.clone(),
        ollama_model: "llama3.2:1b".to_string(),
        resource_settings: ResourceSettings {
            mode: ResourceMode::AlwaysOnLowPriority,
            ..ResourceSettings::default()
        },
        desktop_preferences: provider_core::DesktopPreferences::default(),
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
    let mut provider = tokio::spawn(run_provider_managed(
        config,
        true,
        stop_rx,
        runtime_callback,
        event_callback,
    ));

    let startup_deadline = Instant::now() + Duration::from_secs(240);
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
                    println!(
                        "provider-status runtime={} at={}",
                        serde_json::to_string(&state).unwrap(),
                        utc_timestamp()
                    );
                    runtime_states.push(state);
                }
            }
            result = &mut provider => {
                panic!("provider exited before connecting: {result:?}");
            }
            _ = tokio::time::sleep_until(startup_deadline.into()) => {
                let log_path = provider_core::config_dir().unwrap().join("logs").join("engine.log");
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                let tail = log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                let _ = stop.send(true);
                let result = tokio::time::timeout(Duration::from_secs(10), &mut provider).await;
                panic!("provider did not connect before startup deadline; task={result:?}; engine log tail:\n{tail}");
            }
            event = event_rx.recv() => {
                if let Some(event) = event {
                    match event {
                        ProviderEvent::Connection { state, message } => {
                            println!(
                                "provider-status connection={state:?} message={message:?} at={}",
                                utc_timestamp()
                            );
                            connected = matches!(state, ConnectionState::Connected);
                        }
                        ProviderEvent::FeeConfig { config } => {
                            println!(
                                "provider-status fee_bps={} fee_address={} at={}",
                                config.fee_bps,
                                config.fee_address,
                                utc_timestamp()
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
    let providers: Value = reqwest::get(format!("{coordinator_http}/providers"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let provider_entry = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider_id"] == provider_id)
        .expect("provider should be registered before the job is submitted");
    println!("GET /providers registered-provider={provider_entry}");
    assert_eq!(provider_entry["status"], "idle");
    assert_eq!(provider_entry["accepting_jobs"], true);
    while let Ok(state) = runtime_rx.try_recv() {
        println!(
            "provider-status runtime={} at={}",
            serde_json::to_string(&state).unwrap(),
            utc_timestamp()
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
    let job_url = format!("{coordinator_http}/jobs/{job_id}");
    let dispatch_deadline = Instant::now() + Duration::from_secs(5);
    let job_status = loop {
        let status: Value = reqwest::Client::new()
            .get(&job_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["status"] != "queued" {
            println!("GET /jobs/{job_id} assigned status: {status}");
            break status;
        }
        assert!(
            Instant::now() < dispatch_deadline,
            "job remained queued after a registered accepting provider: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        matches!(job_status["status"].as_str(), Some("running" | "completed")),
        "job should have been assigned to the connected provider: {job_status}"
    );

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
                println!(
                    "provider-status job=running id={started_id} at={}",
                    utc_timestamp()
                );
            }
            ProviderEvent::JobFinished { result } if result.job_id == job_id => {
                assert_eq!(result.error, None);
                assert_eq!(result.result_data.trim(), "ok");
                finished = true;
                println!(
                    "provider-status job=finished id={} result={:?} at={}",
                    result.job_id,
                    result.result_data,
                    utc_timestamp()
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
    println!("GET /jobs/{job_id} {result} at={}", utc_timestamp());

    stop.send(true).unwrap();
    provider.await.unwrap().unwrap();
    println!("provider-status provider=stopped at={}", utc_timestamp());
}
