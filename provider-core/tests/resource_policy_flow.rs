use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use provider_core::{
    run_provider_managed_with_probe, IdleState, NativeSystemProbe, PolicyDecision, PowerState,
    ProviderConfig, ProviderEvent, ProviderEventCallback, ResourceMode, ResourcePreset,
    ResourceSettings, RuntimeManager, RuntimeProgressCallback, SystemProbe, SystemSample,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

struct TimedIdleProbe {
    started: Instant,
    power: PowerState,
}

impl SystemProbe for TimedIdleProbe {
    fn sample(&self) -> SystemSample {
        let elapsed = self.started.elapsed();
        let idle = if elapsed >= Duration::from_secs(60) {
            IdleState::IdleFor(elapsed)
        } else {
            IdleState::Active
        };
        SystemSample {
            idle,
            power: self.power,
            diagnostic: None,
        }
    }
}

#[tokio::test]
#[ignore = "requires a running coordinator, installed runtime, cached model, and one-minute policy wait"]
async fn paused_provider_leaves_job_queued_until_idle_threshold_then_runs_it() {
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

    let system = NativeSystemProbe.sample();
    println!(
        "native Windows probe: idle={:?}, power={:?}, diagnostic={:?}",
        system.idle, system.power, system.diagnostic
    );
    if system.power == PowerState::OnBattery {
        panic!("flow requires AC power; do-not-run-on-battery is enabled");
    }

    let coordinator_http =
        std::env::var("GPUSWARM_E2E_HTTP").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let coordinator_ws =
        std::env::var("GPUSWARM_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081/ws".into());
    let expected_fee_address = std::env::var("GPUSWARM_EXPECTED_FEE_ADDRESS").ok();
    let job_id = format!("resource-policy-{}", uuid::Uuid::new_v4());
    let mut config = ProviderConfig::defaults();
    config.provider_id = format!("resource-policy-provider-{}", uuid::Uuid::new_v4());
    config.coordinator_ws = coordinator_ws;
    config.ollama_model = "llama3.2:1b".into();
    config.resource_settings = ResourceSettings {
        mode: ResourceMode::OnlyWhenIdle,
        idle_threshold_minutes: 1,
        preset: ResourcePreset::Low,
        do_not_run_on_battery: true,
        not_recommended_confirmed: true,
        ..ResourceSettings::default()
    };

    let (stop, stop_rx) = watch::channel(false);
    let (_control, pause_rx) = provider_core::provider_control_channel();
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let runtime_callback: RuntimeProgressCallback = Arc::new(move |state| {
        let _ = runtime_tx.send(state);
    });
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_callback: ProviderEventCallback = Arc::new(move |event| {
        let _ = event_tx.send(event);
    });
    let probe = Arc::new(TimedIdleProbe {
        started: Instant::now(),
        power: system.power,
    });
    println!("test probe: mode=OnlyWhenIdle threshold=1 minute; reports Active for 60 seconds");
    let provider = tokio::spawn(run_provider_managed_with_probe(
        config,
        false,
        stop_rx,
        pause_rx,
        probe,
        runtime_callback,
        event_callback,
    ));

    let client = reqwest::Client::new();
    let mut saw_initial_paused = false;
    let mut saw_fee_config = false;
    let startup_deadline = Instant::now() + Duration::from_secs(90);
    while !saw_initial_paused {
        assert!(
            Instant::now() < startup_deadline,
            "provider did not reach Paused"
        );
        tokio::select! {
            state = runtime_rx.recv() => {
                if let Some(state) = state {
                    println!("runtime={state:?}");
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some(ProviderEvent::Policy { decision, diagnostic }) => {
                        println!("provider-status policy={decision:?} diagnostic={diagnostic:?}");
                        saw_initial_paused = matches!(
                            decision,
                            PolicyDecision::Paused(ref reason)
                                if reason == "Waiting for the idle threshold"
                        );
                    }
                    Some(ProviderEvent::FeeConfig { config }) => {
                        println!(
                            "provider-status fee_bps={} fee_address={}",
                            config.fee_bps, config.fee_address
                        );
                        if let Some(expected) = &expected_fee_address {
                            assert_eq!(&config.fee_address, expected);
                        }
                        saw_fee_config = true;
                    }
                    Some(ProviderEvent::Connection { state, message }) => {
                        println!("provider-status connection={state:?} message={message:?}");
                    }
                    Some(event) => println!("provider-status event={event:?}"),
                    None => break,
                }
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
                "job while paused: status={} job_id={}",
                job["status"], job_id
            );
            queued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

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
        tokio::select! {
            state = runtime_rx.recv() => {
                if let Some(state) = state {
                    println!("runtime={state:?}");
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some(ProviderEvent::Policy { decision, diagnostic }) => {
                        println!("provider-status policy={decision:?} diagnostic={diagnostic:?}");
                        saw_run |= matches!(decision, PolicyDecision::Run);
                    }
                    Some(ProviderEvent::JobStarted { job_id: started_id }) => {
                        println!("provider-status job=running id={started_id}");
                        job_started_before_run |= !saw_run;
                        saw_started |= started_id == job_id;
                    }
                    Some(ProviderEvent::JobFinished { result }) => {
                        println!("provider-status job=finished id={} error={:?}", result.job_id, result.error);
                        saw_finished |= result.job_id == job_id;
                    }
                    Some(ProviderEvent::Connection { state, message }) => {
                        println!("provider-status connection={state:?} message={message:?}");
                    }
                    Some(ProviderEvent::FeeConfig { config }) => {
                        println!(
                            "provider-status fee_bps={} fee_address={}",
                            config.fee_bps, config.fee_address
                        );
                        if let Some(expected) = &expected_fee_address {
                            assert_eq!(&config.fee_address, expected);
                        }
                        saw_fee_config = true;
                    }
                    Some(event) => println!("provider-status event={event:?}"),
                    None => break,
                }
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
        "GET /jobs/{job_id}: status={} result={:?}",
        job_result["status"], job_result["result"]["result_data"]
    );
    assert!(saw_initial_paused);
    assert!(saw_fee_config, "coordinator did not send its FeeConfig");
    assert!(
        queued,
        "job must remain queued while the provider is paused"
    );
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
