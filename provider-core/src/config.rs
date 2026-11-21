use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub payout_address: Option<String>,
    pub provider_id: String,
    pub coordinator_ws: String,
    pub ollama_model: String,
}

impl ProviderConfig {
    pub fn defaults() -> Self {
        Self {
            payout_address: None,
            provider_id: uuid::Uuid::new_v4().to_string(),
            coordinator_ws: "ws://127.0.0.1:8081/ws".to_string(),
            ollama_model: "llama3.2:3b".to_string(),
        }
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let base =
        dirs::config_dir().context("could not find the OS application config directory")?;
    Ok(base.join("GpuSwarm"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load_config() -> Result<Option<ProviderConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read config: {}", path.display()))?;
    let config = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse config: {}", path.display()))?;
    Ok(Some(config))
}

pub fn save_config(config: &ProviderConfig) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory: {}", dir.display()))?;
    let path = config_path()?;
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write config: {}", path.display()))?;
    Ok(())
}
