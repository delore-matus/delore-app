use std::path::{Path, PathBuf};

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

pub fn validate_payout_address(address: &str) -> Result<()> {
    let address = address.trim();
    if address.len() != 42 {
        anyhow::bail!("Payout address must be 0x followed by exactly 40 hexadecimal characters.");
    }
    if !address.starts_with("0x") {
        anyhow::bail!("Payout address must start with 0x.");
    }
    if !address[2..].chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("Payout address contains non-hexadecimal characters.");
    }
    Ok(())
}

pub fn set_payout_address(address: &str) -> Result<ProviderConfig> {
    set_payout_address_at(address, &config_path()?)
}

fn set_payout_address_at(address: &str, path: &Path) -> Result<ProviderConfig> {
    let address = address.trim();
    validate_payout_address(address)?;
    let mut config = load_config_from(path)?.unwrap_or_else(ProviderConfig::defaults);
    config.payout_address = Some(address.to_string());
    save_config_to(&config, path)?;
    Ok(config)
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not find the OS application config directory")?;
    Ok(base.join("GpuSwarm"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load_config() -> Result<Option<ProviderConfig>> {
    load_config_from(&config_path()?)
}

fn load_config_from(path: &Path) -> Result<Option<ProviderConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    let config: ProviderConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse config: {}", path.display()))?;
    if let Some(address) = config.payout_address.as_deref() {
        if let Err(error) = validate_payout_address(address) {
            eprintln!(
                "WARNING: stored payout_address in {} is invalid and will be ignored: {}",
                path.display(),
                error
            );
        }
    }
    Ok(Some(config))
}

pub fn save_config(config: &ProviderConfig) -> Result<()> {
    save_config_to(config, &config_path()?)
}

fn save_config_to(config: &ProviderConfig, path: &Path) -> Result<()> {
    if let Some(address) = config.payout_address.as_deref() {
        validate_payout_address(address)?;
    }
    let dir = path
        .parent()
        .context("provider config path has no parent directory")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config directory: {}", dir.display()))?;
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(path, json)
        .with_context(|| format!("failed to write config: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::{
        load_config_from, save_config_to, set_payout_address_at, validate_payout_address,
        ProviderConfig,
    };

    fn test_config_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("gpuswarm-payout-test-{}", uuid::Uuid::new_v4()))
            .join("config.json")
    }

    fn remove_test_config(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn valid_payout_address() {
        assert!(validate_payout_address("0x1111111111111111111111111111111111111111").is_ok());
    }

    #[test]
    fn payout_address_requires_0x_and_40_hex_chars() {
        assert!(validate_payout_address("1111111111111111111111111111111111111111").is_err());
        assert!(validate_payout_address("0x1234").is_err());
        assert!(validate_payout_address("0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn set_payout_address_saves_a_valid_address() {
        let path = test_config_path();
        let config = ProviderConfig::defaults();
        save_config_to(&config, &path).unwrap();

        let updated =
            set_payout_address_at("0x1111111111111111111111111111111111111111", &path).unwrap();

        assert_eq!(
            updated.payout_address.as_deref(),
            Some("0x1111111111111111111111111111111111111111")
        );
        assert_eq!(
            load_config_from(&path)
                .unwrap()
                .unwrap()
                .payout_address
                .as_deref(),
            updated.payout_address.as_deref()
        );
        remove_test_config(&path);
    }

    #[test]
    fn set_payout_address_rejects_invalid_values_without_changing_config() {
        let path = test_config_path();
        let config = ProviderConfig::defaults();
        save_config_to(&config, &path).unwrap();
        let original = fs::read(&path).unwrap();

        for invalid in ["0x1234", "0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(set_payout_address_at(invalid, &path).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }

        remove_test_config(&path);
    }
}
