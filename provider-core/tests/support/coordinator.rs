use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};

pub struct OwnedCoordinator {
    child: Child,
    workspace: PathBuf,
    pub http_url: String,
    pub ws_url: String,
}

impl OwnedCoordinator {
    pub async fn start() -> Result<Self> {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("provider-core has no workspace parent")?
            .to_path_buf();
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"));
        let coordinator_executable = target_dir.join("debug").join(if cfg!(windows) {
            "coordinator.exe"
        } else {
            "coordinator"
        });
        let status = Command::new("cargo")
            .args(["build", "-p", "coordinator"])
            .current_dir(&workspace)
            .status()
            .context("failed to build the coordinator for integration testing")?;
        if !status.success() {
            return Err(anyhow!("cargo build -p coordinator failed: {status}"));
        }

        let root = std::env::temp_dir().join(format!(
            "gpuswarm-e2e-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("config"))
            .with_context(|| format!("failed to create test directory {}", root.display()))?;
        let http_port = free_port()?;
        let ws_port = free_port()?;
        let http_url = format!("http://127.0.0.1:{http_port}");
        let ws_url = format!("ws://127.0.0.1:{ws_port}/ws");
        let child = Command::new(&coordinator_executable)
            .current_dir(&root)
            .env("GPUSWARM_HTTP_PORT", http_port.to_string())
            .env("GPUSWARM_WS_PORT", ws_port.to_string())
            .env("GPUSWARM_DB_PATH", root.join("coordinator.sqlite3"))
            .env(
                "GPUSWARM_FEE_ADDRESS",
                "0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850",
            )
            .env("GPUSWARM_NETWORK_MODEL", "llama3.2:1b")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start the test coordinator")?;
        let mut coordinator = Self {
            child,
            workspace: root.clone(),
            http_url,
            ws_url,
        };

        std::env::set_var("GPUSWARM_CONFIG_DIR", root.join("config"));
        std::env::set_var("GPUSWARM_TEST_IDLE_STATE", "active");
        std::env::set_var("GPUSWARM_TEST_POWER_STATE", "ac");
        std::env::remove_var("COORDINATOR_WS");
        std::env::remove_var("OLLAMA_MODEL");

        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = coordinator
                .child
                .try_wait()
                .context("failed to inspect test coordinator process")?
            {
                return Err(anyhow!("test coordinator exited during startup: {status}"));
            }
            if client
                .get(format!("{}/fees", coordinator.http_url))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                println!(
                    "owned-coordinator pid={} http={} ws={} db={} at={}",
                    coordinator.child.id(),
                    coordinator.http_url,
                    coordinator.ws_url,
                    root.join("coordinator.sqlite3").display(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                );
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "test coordinator did not become ready at {}",
                    coordinator.http_url
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(coordinator)
    }
}

impl Drop for OwnedCoordinator {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.workspace);
        println!(
            "stopped-owned-coordinator pid={} at={}",
            pid,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("failed to reserve a test port")?;
    Ok(listener.local_addr()?.port())
}
