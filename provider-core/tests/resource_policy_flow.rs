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

#[path = "support/coordinator.rs"]
mod coordinator;

fn utc_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[tokio::test]
#[ignore = "requires the installed provider runtime and cached llama3.2:1b model"]
async fn performance_slider_controls_registration_and_job_flow() {
    let coordinator = coordinator::OwnedCoordinator::start()
        .await
        .expect("test coordinator should start on isolated ports");
    let coordinator_http = coordinator.http_url.clone();
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

    let expected_fee_address = "0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850";
    let provider_id = format!("performance-zero-provider-{}", uuid::Uuid::new_v4());
    let job_id = format!("performance-zero-{}", uuid::Uuid::new_v4());
    let mut config = ProviderConfig::defaults();
    config.payout_address = Some("0x1111111111111111111111111111111111111111".into());
    config.provider_id = provider_id.clone();
    config.coordinator_ws = coordinator.ws_url.clone();
    config.ollama_model = "llama3.2:3b".into();
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
                    assert_eq!(config.fee_address, expected_fee_address);
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

    let registration_deadline = Instant::now() + Duration::from_secs(10);
    let provider_entry = loop {
        let providers: Value = client
            .get(format!("{coordinator_http}/providers"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(provider) = providers
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["provider_id"] == provider_id)
        {
            break provider.clone();
        }
        assert!(
            Instant::now() < registration_deadline,
            "provider was not listed while paused at 0%: {providers}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(provider_entry["model"], "llama3.2:1b");
    assert_eq!(provider_entry["status"], "paused");
    assert_eq!(provider_entry["accepting_jobs"], false);
    println!(
        "GET /providers provider_id={} status=paused accepting_jobs=false at={}",
        provider_id,
        utc_timestamp()
    );

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
                "job while performance=0: status={} job_id={} at={}",
                job["status"],
                job_id,
                utc_timestamp()
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
    println!("slider changed to 50% at {}", utc_timestamp());

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
                        utc_timestamp()
                    );
                    job_started_before_run |= !saw_run;
                    saw_started |= started_id == job_id;
                }
                ProviderEvent::JobFinished { result } => {
                    println!(
                        "provider-status job=finished id={} error={:?} at={}",
                        result.job_id,
                        result.error,
                        utc_timestamp()
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
        utc_timestamp()
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

    let zero_settings = provider_core::ResourceSettings {
        mode: ResourceMode::AlwaysOnLowPriority,
        preset: ResourcePreset::Low,
        do_not_run_on_battery: false,
        not_recommended_confirmed: true,
        performance_percent: 0,
        ..provider_core::ResourceSettings::default()
    };
    control.update_resource_settings(zero_settings).unwrap();
    let pause_deadline = Instant::now() + Duration::from_secs(40);
    let mut saw_zero_pause = false;
    while !saw_zero_pause {
        assert!(
            Instant::now() < pause_deadline,
            "provider did not publish paused state after changing back to 0%"
        );
        if let Some(ProviderEvent::Policy {
            decision: PolicyDecision::Paused(ref reason),
            ..
        }) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap_or(None)
        {
            saw_zero_pause = reason == "Performance is set to 0%, no work is accepted";
        }
    }

    let paused_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let providers: Value = client
            .get(format!("{coordinator_http}/providers"))
            .send()
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
            .expect("provider should remain connected");
        if provider_entry["status"] == "paused" && provider_entry["accepting_jobs"] == false {
            println!(
                "GET /providers provider_id={} status=paused accepting_jobs=false at={}",
                provider_id,
                utc_timestamp()
            );
            break;
        }
        assert!(
            Instant::now() < paused_deadline,
            "coordinator did not mark provider paused: {provider_entry}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let second_job_id = format!("performance-zero-second-{}", uuid::Uuid::new_v4());
    let second_response = client
        .post(format!("{coordinator_http}/request-job"))
        .header("X-Payment", "mock")
        .json(&serde_json::json!({
            "id": second_job_id,
            "prompt": "Respond with exactly: ok"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second_response.status(), reqwest::StatusCode::ACCEPTED);
    let second_job_url = format!("{coordinator_http}/jobs/{second_job_id}");
    let second_queued_deadline = Instant::now() + Duration::from_secs(5);
    let mut second_queued = false;
    while Instant::now() < second_queued_deadline {
        let job: Value = client
            .get(&second_job_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if job["status"] == "queued" {
            println!(
                "job while performance=0: status=queued job_id={second_job_id} at={}",
                utc_timestamp()
            );
            second_queued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(second_queued, "second job must stay queued at 0%");

    let full_settings = provider_core::ResourceSettings {
        mode: ResourceMode::AlwaysOnLowPriority,
        preset: ResourcePreset::Low,
        do_not_run_on_battery: false,
        not_recommended_confirmed: true,
        performance_percent: 100,
        ..provider_core::ResourceSettings::default()
    };
    control.update_resource_settings(full_settings).unwrap();
    let run_deadline = Instant::now() + Duration::from_secs(8);
    let mut saw_full_run = false;
    while !saw_full_run {
        assert!(
            Instant::now() < run_deadline,
            "provider did not resume after changing to 100%"
        );
        if let Some(ProviderEvent::Policy {
            decision: PolicyDecision::Run,
            ..
        }) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap_or(None)
        {
            saw_full_run = true;
        }
    }
    println!(
        "provider-status policy=Run performance=100 at={}",
        utc_timestamp()
    );
    let full_job_ids = [
        format!("performance-full-a-{}", uuid::Uuid::new_v4()),
        format!("performance-full-b-{}", uuid::Uuid::new_v4()),
    ];
    for full_job_id in &full_job_ids {
        let response = client
            .post(format!("{coordinator_http}/request-job"))
            .header("X-Payment", "mock")
            .json(&serde_json::json!({
                "id": full_job_id,
                "prompt": "Respond with exactly: ok"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        println!(
            "job submitted at performance=100 id={full_job_id} at={}",
            utc_timestamp()
        );
    }

    let mut full_jobs_finished = Vec::new();
    let full_run_deadline = Instant::now() + Duration::from_secs(180);
    while full_jobs_finished.len() < full_job_ids.len() {
        assert!(
            Instant::now() < full_run_deadline,
            "100% jobs did not finish: {full_jobs_finished:?}"
        );
        if let Some(ProviderEvent::JobFinished { result }) =
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap_or(None)
        {
            if full_job_ids.contains(&result.job_id) {
                assert_eq!(result.error, None);
                full_jobs_finished.push(result.job_id.clone());
                println!(
                    "provider-status job=finished id={} at={}",
                    result.job_id,
                    utc_timestamp()
                );
            }
        }
    }

    let config_dir = provider_core::config_dir().unwrap();
    let log_path = config_dir.join("logs").join("delore.log");
    let technical_log = std::fs::read_to_string(&log_path).unwrap_or_default();
    for full_job_id in &full_job_ids {
        assert!(
            !technical_log.contains(&format!("Performance cooldown start: job_id={full_job_id}")),
            "100% must not emit a cooldown for {full_job_id}"
        );
    }
    for completed_id in std::iter::once(&second_job_id).chain(full_job_ids.iter()) {
        let job_url = format!("{coordinator_http}/jobs/{completed_id}");
        let completed_deadline = Instant::now() + Duration::from_secs(10);
        let job = loop {
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
                "coordinator did not complete job {completed_id}: {job}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(job["fee_breakdown"]["gross_price"], 10_000);
        assert_eq!(job["fee_breakdown"]["provider_payout"], 9_000);
        assert_eq!(job["fee_breakdown"]["platform_fee"], 1_000);
        println!(
            "GET /jobs/{completed_id}: status=completed payout=9000 fee=1000 at={}",
            utc_timestamp()
        );
    }

    let _ = stop.send(true);
    let provider_result = tokio::time::timeout(Duration::from_secs(20), provider)
        .await
        .expect("provider should stop promptly")
        .expect("provider task should join");
    assert!(provider_result.is_ok(), "{provider_result:?}");
}
