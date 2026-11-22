use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{JobPayload, ProviderRegistration, WsMessage};
use futures_util::{SinkExt, StreamExt};
use provider_core::{
    provider_control_channel, run_provider_managed_with_control, ProviderConfig, ProviderEvent,
    ProviderEventCallback, ResourceMode, RuntimeManager, RuntimeProgressCallback,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[tokio::test]
#[ignore = "requires installed runtime and cached llama3.2:1b"]
async fn disconnect_during_job_discards_result_and_reconnects() {
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
        "cached llama3.2:1b is required"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider_id = format!("disconnect-provider-{}", uuid::Uuid::new_v4());
    let job_id = format!("disconnect-job-{}", uuid::Uuid::new_v4());

    let mut config = ProviderConfig::defaults();
    config.provider_id = provider_id.clone();
    config.coordinator_ws = format!("ws://{address}/ws");
    config.ollama_model = "llama3.2:1b".into();
    config.resource_settings.mode = ResourceMode::AlwaysOnLowPriority;
    config.resource_settings.do_not_run_on_battery = false;
    config.resource_settings.not_recommended_confirmed = true;
    config.resource_settings.performance_percent = 100;

    let (control, pause_rx) = provider_control_channel();
    control
        .update_resource_settings(config.resource_settings.clone())
        .unwrap();
    let settings_rx = control.subscribe_resource_settings();
    let (stop, stop_rx) = watch::channel(false);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let observed_events = Arc::new(Mutex::new(Vec::new()));
    let callback_events = observed_events.clone();
    let event_callback: ProviderEventCallback = Arc::new(move |event| {
        callback_events.lock().unwrap().push(event.clone());
        let _ = event_tx.send(event);
    });
    let runtime_callback: RuntimeProgressCallback = Arc::new(|_| {});

    let provider = tokio::spawn(run_provider_managed_with_control(
        config,
        false,
        stop_rx,
        pause_rx,
        settings_rx,
        runtime_callback,
        event_callback,
    ));

    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let (reconnected_tx, reconnected_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_provider_id = provider_id.clone();
    let server_job_id = job_id.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(Duration::from_secs(90), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let registration = match timeout(Duration::from_secs(5), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(json) => serde_json::from_str::<WsMessage>(&json).unwrap(),
            message => panic!("expected provider registration, got {message:?}"),
        };
        assert!(matches!(
            registration,
            WsMessage::Register(ProviderRegistration {
                accepting_jobs: true,
                ..
            })
        ));
        websocket
            .send(Message::Text(
                serde_json::to_string(&WsMessage::Job(JobPayload {
                    id: server_job_id,
                    prompt: "Write a very long answer.".to_string(),
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
        disconnect_rx.await.unwrap();
        drop(websocket);

        let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let registration = match timeout(Duration::from_secs(5), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(json) => serde_json::from_str::<WsMessage>(&json).unwrap(),
            message => panic!("expected reconnect registration, got {message:?}"),
        };
        assert!(matches!(
            registration,
            WsMessage::Register(ProviderRegistration {
                provider_id: ref id,
                accepting_jobs: true,
                ..
            }) if id == &server_provider_id
        ));
        reconnected_tx.send(()).unwrap();
        shutdown_rx.await.unwrap();
        drop(websocket);
    });

    timeout(Duration::from_secs(120), async {
        loop {
            if matches!(
                event_rx.recv().await,
                Some(ProviderEvent::JobStarted { job_id: started_id }) if started_id == job_id
            ) {
                break;
            }
        }
    })
    .await
    .expect("provider did not start the test job");
    disconnect_tx.send(()).unwrap();
    timeout(Duration::from_secs(15), reconnected_rx)
        .await
        .unwrap()
        .unwrap();

    {
        let events = observed_events.lock().unwrap();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ProviderEvent::JobFinished { result } if result.job_id == job_id
            )),
            "a result must not be emitted after the coordinator disconnects"
        );
        assert!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderEvent::Connection {
                        state: provider_core::ConnectionState::Connected,
                        ..
                    }
                ))
                .count()
                >= 2,
            "provider must report a second connected state after recovery"
        );
    }

    stop.send(true).unwrap();
    shutdown_tx.send(()).unwrap();
    timeout(Duration::from_secs(30), provider)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.await.unwrap();
}
