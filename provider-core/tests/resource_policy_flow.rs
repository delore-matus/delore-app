use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use provider_core::{
    provider_control_channel, run_provider_managed_with_control, PolicyDecision, ProviderConfig,
    ProviderEvent, ProviderEventCallback, ResourceMode, ResourcePreset, RuntimeManager,
    RuntimeProgressCallback,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

#[tokio::test]
#[ignore = "requires a running coordinator, installed runtime, and cached model"]
async fn zero_performance_keeps_job_queued_until_slider_increases() {
    let runtime = RuntimeManager::new().unwrap();
    assert!(
        runtime.runtime_dir().exists(),
        "managed runtime is not installed"
    );
    let model_manifest = runtime
        .models_dir()
        .join("manifests/registry.ollama.ai/library/llama3.2/1b");
    assert!(
        model_manifest.exists(),
        "cached llama3.2:1b model is required"
    );

    let coordinator_http =
        std::env::var("GPUSWARM_E2E_HTTP").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let coordinator_ws =
        std::env::var("GPUSWARM_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081/ws".into());
    let expected_fee_address = std::env::var("GPUSWARM_EXPECTED_FEE_ADDRESS").ok();
    let job_id = format!("performance-zero-{}", uuid::Uuid::new_v4());
    let mut config = ProviderConfig::defaults();
    config.provider_id = format!("performance-zero-provider-{}", uuid::Uuid::new_v4());
    config.coordinator_ws = coordinator_ws;
    config.ollama_model = "llama3.2:1b".into();
    config.resource_settings = provider_core::ResourceSettings {
        mode: ResourceMode::AlwaysOnLowPriority,
        preset: ResourcePreset::Low,
        do_not_run_on_battery: false,
        not_recommended_confirmed: true,
        performance_percent: 0,
        ..provider_core::ResourceSettings::default()
    };

    let (control, pause_rx) = provider_control_channel();
    control
        .update_resource_settings(config.resource_settings.clone())
        .unwrap();
    let settings_rx = control.subscribe_resource_settings();
    let (stop, stop_rx) = watch::channel(false);
    let runtime_callback: RuntimeProgressCallback = Arc::new(|_| {});
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_callback: ProviderEventCallback = Arc::new(move |event| {
        let _ = event_tx.send(event);
    });
    let provider = tokio::spawn(run_provider_managed_with_control(
        config,
        false,
        stop_rx,
        pause_rx,
        settings_rx,
        runtime_callback,
        event_callback,
    ));

    let client = reqwest::Client::new();
    let mut saw_initial_paused = false;
    let mut saw_fee_config = false;
    let startup_deadline = Instant::now() + Duration::from_secs(90);
    while !saw_initial_paused || !saw_fee_config {
        assert!(
            Instant::now() < startup_deadline,
            "provider did not register paused and receive fee config"
        );
        if let Some(event) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap_or(None)
        {
            match event {
                ProviderEvent::Policy { decision, .. } => {
                    println!("provider-status policy={decision:?}");
                    saw_initial_paused = matches!(
                        decision,
                        PolicyDecision::Paused(ref reason)
                            if reason == "Performance is set to 0%, no work is accepted"
                    );
                }
                ProviderEvent::FeeConfig { config } => {
                    let fee_units = config.job_price_units - config.provider_payout_units;
                    println!(
                        "fee_config fee_bps={} provider_payout={} fee={} recipient={}",
                        config.fee_bps, config.provider_payout_units, fee_units, config.fee_address
                    );
                    if let Some(expected) = &expected_fee_address {
                        assert_eq!(&config.fee_address, expected);
                    }
                    assert_eq!(config.provider_payout_units, 9_000);
                    assert_eq!(fee_units, 1_000);
                    saw_fee_config = true;
                }
                ProviderEvent::Connection { state, message } => {
                    println!("provider-status connection={state:?} message={message:?}");
                }
                event => println!("provider-status event={event:?}"),
            }
        }
    }

    let response = client
        .post(format!("{coordinator_http}/request-job"))
        .header("X-Payment", "mock")
        .json(&serde_json::json!({
            "id": job_id,
            "prompt": "Respond with exactly: ok"
        }))
        .send()
        .await
        .expect("coordinator should accept the queued mock job");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    let job_url = format!("{coordinator_http}/jobs/{job_id}");
    let queued_deadline = Instant::now() + Duration::from_secs(10);
    let mut queued = false;
    while Instant::now() < queued_deadline {
        let job: Value = client
            .get(&job_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if job["status"] == "queued" {
            println!(
                "job while performance=0: status={} job_id={} at={:?}",
                job["status"],
                job_id,
                chrono::Local::now()
            );
            queued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(queued, "job must remain queued at 0% performance");

    let mut resumed_settings = provider_core::ResourceSettings {
        performance_percent: 50,
        ..provider_core::ResourceSettings::default()
    };
    resumed_settings.mode = ResourceMode::AlwaysOnLowPriority;
    resumed_settings.preset = ResourcePreset::Low;
    resumed_settings.do_not_run_on_battery = false;
    resumed_settings.not_recommended_confirmed = true;
    control.update_resource_settings(resumed_settings).unwrap();
    println!("slider changed to 50% at {}", chrono::Local::now());

    let run_deadline = Instant::now() + Duration::from_secs(180);
    let mut saw_run = false;
    let mut saw_started = false;
    let mut job_started_before_run = false;
    let mut saw_finished = false;
    while !saw_finished {
        assert!(
            Instant::now() < run_deadline,
            "provider did not run and finish the queued job"
        );
        if let Some(event) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap_or(None)
        {
            match event {
                ProviderEvent::Policy { decision, .. } => {
                    println!("provider-status policy={decision:?}");
                    saw_run |= matches!(decision, PolicyDecision::Run);
                }
                ProviderEvent::JobStarted { job_id: started_id } => {
                    println!(
                        "provider-status job=running id={started_id} at={}",
                        chrono::Local::now()
                    );
                    job_started_before_run |= !saw_run;
                    saw_started |= started_id == job_id;
                }
                ProviderEvent::JobFinished { result } => {
                    println!(
                        "provider-status job=finished id={} error={:?} at={}",
                        result.job_id,
                        result.error,
                        chrono::Local::now()
                    );
                    saw_finished |= result.job_id == job_id;
                }
                ProviderEvent::Connection { state, message } => {
                    println!("provider-status connection={state:?} message={message:?}");
                }
                ProviderEvent::FeeConfig { .. } => {}
                event => println!("provider-status event={event:?}"),
            }
        }
    }

    let _ = stop.send(true);
    let provider_result = tokio::time::timeout(Duration::from_secs(20), provider)
        .await
        .expect("provider should stop promptly")
        .expect("provider task should join");
    assert!(provider_result.is_ok(), "{provider_result:?}");

    let completed_deadline = Instant::now() + Duration::from_secs(10);
    let job_result = loop {
        let job: Value = client
            .get(&job_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if job["status"] == "completed" {
            break job;
        }
        assert!(
            Instant::now() < completed_deadline,
            "coordinator did not persist the completed result: {job}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    println!(
        "GET /jobs/{job_id}: status={} result={:?} at={}",
        job_result["status"],
        job_result["result"]["result_data"],
        chrono::Local::now()
    );
    assert!(saw_initial_paused);
    assert!(saw_fee_config, "coordinator did not send its FeeConfig");
    assert!(queued, "job must remain queued while performance is zero");
    assert!(saw_run);
    assert!(saw_started);
    assert!(!job_started_before_run, "job started before policy Run");
    assert_eq!(job_result["status"], "completed");
    assert_eq!(
        job_result["result"]["result_data"]
            .as_str()
            .unwrap_or("")
            .trim(),
        "ok"
    );
}
