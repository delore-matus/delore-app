use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::resource::ResourceSettings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub payout_address: Option<String>,
    pub provider_id: String,
    pub coordinator_ws: String,
    pub ollama_model: String,
    #[serde(default)]
    pub resource_settings: ResourceSettings,
    #[serde(default)]
    pub desktop_preferences: DesktopPreferences,
}

#[derive(Debug)]
pub struct ConfigLoadResult {
    pub config: Option<ProviderConfig>,
    pub recovered_unreadable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Dark,
    Light,
    #[default]
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AppLanguage {
    #[default]
    English,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopPreferences {
    pub theme: Theme,
    pub language: AppLanguage,
    pub onboarding_complete: bool,
    pub start_on_login: bool,
    pub start_minimized_to_tray: bool,
    pub keep_running_in_tray: bool,
    pub notifications_enabled: bool,
}

impl Default for DesktopPreferences {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            language: AppLanguage::English,
            onboarding_complete: false,
            start_on_login: false,
            start_minimized_to_tray: false,
            keep_running_in_tray: false,
            notifications_enabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaModelSelection {
    pub configured: String,
    pub effective: String,
    pub environment_override: bool,
}

impl ProviderConfig {
    pub fn defaults() -> Self {
        Self {
            payout_address: None,
            provider_id: uuid::Uuid::new_v4().to_string(),
            coordinator_ws: "ws://127.0.0.1:8081/ws".to_string(),
            ollama_model: "llama3.2:3b".to_string(),
            resource_settings: ResourceSettings::default(),
            desktop_preferences: DesktopPreferences::default(),
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

pub fn validate_ollama_model(model: &str) -> Result<()> {
    let bytes = model.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
        || model.contains("..")
        || model.contains("//")
    {
        anyhow::bail!(
            "Ollama model must be 1-128 characters using letters, numbers, '.', '_', '-', '/', or ':' and must start and end with a letter or number."
        );
    }
    Ok(())
}

pub fn effective_ollama_model(config: &ProviderConfig) -> Result<OllamaModelSelection> {
    let environment_override = match std::env::var("OLLAMA_MODEL") {
        Ok(model) => Some(model),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("OLLAMA_MODEL contains invalid non-Unicode data.")
        }
    };
    select_ollama_model(&config.ollama_model, environment_override.as_deref())
}

fn select_ollama_model(
    configured: &str,
    environment_override: Option<&str>,
) -> Result<OllamaModelSelection> {
    validate_ollama_model(configured)?;
    let effective = if let Some(model) = environment_override {
        validate_ollama_model(model).context("OLLAMA_MODEL is set to an invalid model name")?;
        model.to_string()
    } else {
        configured.to_string()
    };
    Ok(OllamaModelSelection {
        configured: configured.to_string(),
        effective,
        environment_override: environment_override.is_some(),
    })
}

pub fn set_payout_address(address: &str) -> Result<ProviderConfig> {
    set_payout_address_at(address, &config_path()?)
}

pub fn set_ollama_model(model: &str) -> Result<ProviderConfig> {
    set_ollama_model_at(model, &config_path()?)
}

pub fn set_resource_settings(settings: ResourceSettings) -> Result<ProviderConfig> {
    set_resource_settings_at(settings, &config_path()?)
}

pub fn set_performance_percent(performance_percent: i16) -> Result<ProviderConfig> {
    set_performance_percent_at(performance_percent, &config_path()?)
}

pub fn set_desktop_preferences(preferences: DesktopPreferences) -> Result<ProviderConfig> {
    set_desktop_preferences_at(preferences, &config_path()?)
}

pub fn confirm_not_recommended_runtime() -> Result<ProviderConfig> {
    let mut config = load_config()?.unwrap_or_else(ProviderConfig::defaults);
    config.resource_settings.not_recommended_confirmed = true;
    save_config(&config)?;
    Ok(config)
}

fn set_resource_settings_at(settings: ResourceSettings, path: &Path) -> Result<ProviderConfig> {
    settings.validate()?;
    let mut config = load_config_from(path)?.unwrap_or_else(ProviderConfig::defaults);
    let confirmed_before = config.resource_settings.not_recommended_confirmed;
    config.resource_settings = settings;
    config.resource_settings.not_recommended_confirmed |= confirmed_before;
    save_config_to(&config, path)?;
    Ok(config)
}

fn set_desktop_preferences_at(
    preferences: DesktopPreferences,
    path: &Path,
) -> Result<ProviderConfig> {
    let mut config = load_config_from(path)?.unwrap_or_else(ProviderConfig::defaults);
    config.desktop_preferences = preferences;
    save_config_to(&config, path)?;
    Ok(config)
}

fn set_performance_percent_at(performance_percent: i16, path: &Path) -> Result<ProviderConfig> {
    if !(0..=100).contains(&performance_percent) {
        anyhow::bail!("Performance must be between 0 and 100.");
    }
    let mut config = load_config_from(path)?.unwrap_or_else(ProviderConfig::defaults);
    config.resource_settings.performance_percent = u8::try_from(performance_percent)?;
    config.resource_settings.validate()?;
    save_config_to(&config, path)?;
    Ok(config)
}

fn set_ollama_model_at(model: &str, path: &Path) -> Result<ProviderConfig> {
    let model = model.trim();
    validate_ollama_model(model)?;
    let mut config = load_config_from(path)?.unwrap_or_else(ProviderConfig::defaults);
    config.ollama_model = model.to_string();
    save_config_to(&config, path)?;
    Ok(config)
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
    if let Ok(override_dir) = std::env::var("GPUSWARM_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return Ok(PathBuf::from(override_dir));
        }
    }
    let base = dirs::config_dir().context("could not find the OS application config directory")?;
    Ok(base.join("GpuSwarm"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn append_technical_log(message: &str) -> Result<()> {
    let logs_dir = config_dir()?.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed to create log directory: {}", logs_dir.display()))?;
    let path = logs_dir.join("delore.log");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open log file: {}", path.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    writeln!(file, "{timestamp} {message}")
        .with_context(|| format!("failed to write log file: {}", path.display()))
}

pub fn load_config() -> Result<Option<ProviderConfig>> {
    load_config_from(&config_path()?)
}

pub fn load_config_with_recovery() -> Result<ConfigLoadResult> {
    load_config_with_recovery_from(&config_path()?, |message| {
        crate::technical_log!("{message}");
    })
}

fn load_config_with_recovery_from(
    path: &Path,
    mut logger: impl FnMut(&str),
) -> Result<ConfigLoadResult> {
    match load_config_from(path) {
        Ok(config) => Ok(ConfigLoadResult {
            config,
            recovered_unreadable: false,
        }),
        Err(error) => {
            let backup_path = path.with_file_name("config.json.bad");
            std::fs::copy(path, &backup_path).with_context(|| {
                format!(
                    "failed to back up unreadable config from {} to {}",
                    path.display(),
                    backup_path.display()
                )
            })?;
            logger(&format!(
                "Unreadable config at {}; preserving backup at {}: {error:#}",
                path.display(),
                backup_path.display()
            ));
            Ok(ConfigLoadResult {
                config: None,
                recovered_unreadable: true,
            })
        }
    }
}

fn decode_config_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        let (chunks, _) = bytes[2..].as_chunks::<2>();
        let units: Vec<u16> = chunks
            .iter()
            .map(|chunk| u16::from_le_bytes(*chunk))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let (chunks, _) = bytes[2..].as_chunks::<2>();
        let units: Vec<u16> = chunks
            .iter()
            .map(|chunk| u16::from_be_bytes(*chunk))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn load_config_from(path: &Path) -> Result<Option<ProviderConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    let text = decode_config_bytes(&bytes);
    let config: ProviderConfig = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse config: {}", path.display()))?;
    if let Some(address) = config.payout_address.as_deref() {
        if let Err(error) = validate_payout_address(address) {
            crate::technical_log!(
                "WARNING: stored payout_address in {} is invalid and will be ignored: {}",
                path.display(),
                error
            );
        }
    }
    if let Err(error) = validate_ollama_model(&config.ollama_model) {
        crate::technical_log!(
            "WARNING: stored ollama_model in {} is invalid: {}",
            path.display(),
            error
        );
    }
    config
        .resource_settings
        .validate()
        .with_context(|| format!("invalid resource_settings in {}", path.display()))?;
    Ok(Some(config))
}

pub fn save_config(config: &ProviderConfig) -> Result<()> {
    save_config_to(config, &config_path()?)
}

fn save_config_to(config: &ProviderConfig, path: &Path) -> Result<()> {
    if let Some(address) = config.payout_address.as_deref() {
        validate_payout_address(address)?;
    }
    validate_ollama_model(&config.ollama_model)?;
    config.resource_settings.validate()?;
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
        load_config_from, load_config_with_recovery_from, save_config_to, select_ollama_model,
        set_desktop_preferences_at, set_ollama_model_at, set_payout_address_at,
        set_performance_percent_at, set_resource_settings_at, validate_ollama_model,
        validate_payout_address, AppLanguage, DesktopPreferences, ProviderConfig, Theme,
    };
    use crate::resource::{ResourceMode, ResourceSchedule, ResourceSettings};

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
    fn validates_ollama_model_names() {
        assert!(validate_ollama_model("llama3.2:3b").is_ok());
        assert!(validate_ollama_model("hf.co/example/model:Q4_K_M").is_ok());
        for invalid in [
            "",
            " ",
            "../model",
            "model//tag",
            "model;command",
            &"a".repeat(129),
        ] {
            assert!(validate_ollama_model(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn environment_model_override_is_validated_and_reported() {
        let selection = select_ollama_model("llama3.2:3b", Some("llama3.2:1b")).unwrap();
        assert_eq!(selection.configured, "llama3.2:3b");
        assert_eq!(selection.effective, "llama3.2:1b");
        assert!(selection.environment_override);
        assert!(select_ollama_model("llama3.2:3b", Some("invalid model")).is_err());
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

    #[test]
    fn set_ollama_model_saves_a_valid_model() {
        let path = test_config_path();
        save_config_to(&ProviderConfig::defaults(), &path).unwrap();
        let updated = set_ollama_model_at("hf.co/example/model:Q4_K_M", &path).unwrap();
        assert_eq!(updated.ollama_model, "hf.co/example/model:Q4_K_M");
        assert_eq!(
            load_config_from(&path).unwrap().unwrap().ollama_model,
            updated.ollama_model
        );
        remove_test_config(&path);
    }

    #[test]
    fn set_ollama_model_rejects_invalid_values_without_changing_config() {
        let path = test_config_path();
        save_config_to(&ProviderConfig::defaults(), &path).unwrap();
        let original = fs::read(&path).unwrap();
        for invalid in ["", "invalid model", "../model"] {
            assert!(set_ollama_model_at(invalid, &path).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        remove_test_config(&path);
    }

    #[test]
    fn loads_bom_utf16_and_windows_line_endings() {
        let path = test_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let json = r#"{"payout_address":null,"provider_id":"encoding-test","coordinator_ws":"ws://127.0.0.1:8081/ws","ollama_model":"llama3.2:1b","resource_settings":{"mode":"only_when_idle","idle_threshold_minutes":5,"schedule":null,"preset":"balanced","do_not_run_on_battery":true,"not_recommended_confirmed":false,"performance_percent":50},"desktop_preferences":{"theme":"dark","language":"english","start_on_login":false,"start_minimized_to_tray":false,"keep_running_in_tray":false,"notifications_enabled":false}}"#.replace('\n', "\r\n");

        let mut utf8_bom = vec![0xef, 0xbb, 0xbf];
        utf8_bom.extend_from_slice(json.as_bytes());
        fs::write(&path, utf8_bom).unwrap();
        assert_eq!(
            load_config_from(&path).unwrap().unwrap().provider_id,
            "encoding-test"
        );

        let mut utf16_le = vec![0xff, 0xfe];
        for unit in json.encode_utf16() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, utf16_le).unwrap();
        assert_eq!(
            load_config_from(&path).unwrap().unwrap().provider_id,
            "encoding-test"
        );

        let mut utf16_be = vec![0xfe, 0xff];
        for unit in json.encode_utf16() {
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        fs::write(&path, utf16_be).unwrap();
        assert_eq!(
            load_config_from(&path).unwrap().unwrap().provider_id,
            "encoding-test"
        );
        remove_test_config(&path);
    }

    #[test]
    fn old_config_without_resource_settings_loads_with_defaults_without_rewriting() {
        let path = test_config_path();
        let old_json = r#"{
            "payout_address": null,
            "provider_id": "test-provider",
            "coordinator_ws": "ws://127.0.0.1:8081/ws",
            "ollama_model": "llama3.2:3b"
        }"#;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, old_json).unwrap();

        let config = load_config_from(&path).unwrap().unwrap();
        assert_eq!(config.resource_settings, ResourceSettings::default());
        assert_eq!(config.desktop_preferences, DesktopPreferences::default());
        assert!(!config.desktop_preferences.onboarding_complete);
        assert_eq!(fs::read_to_string(&path).unwrap(), old_json);
        remove_test_config(&path);
    }

    #[test]
    fn existing_performance_values_load_without_rewriting() {
        let path = test_config_path();
        for value in [10, 100] {
            let mut config = ProviderConfig::defaults();
            config.resource_settings.performance_percent = value;
            save_config_to(&config, &path).unwrap();
            let original = fs::read(&path).unwrap();
            let loaded = load_config_from(&path).unwrap().unwrap();
            assert_eq!(loaded.resource_settings.performance_percent, value);
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        remove_test_config(&path);
    }

    #[test]
    fn malformed_config_falls_back_and_keeps_a_bad_backup() {
        let path = test_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let malformed = b"{this is not valid json";
        fs::write(&path, malformed).unwrap();
        let mut logged = String::new();

        let loaded = load_config_with_recovery_from(&path, |message| {
            logged = message.to_string();
        })
        .unwrap();

        assert!(loaded.config.is_none());
        assert!(loaded.recovered_unreadable);
        assert_eq!(fs::read(&path).unwrap(), malformed);
        assert_eq!(
            fs::read(path.with_file_name("config.json.bad")).unwrap(),
            malformed
        );
        assert!(logged.contains("failed to parse config"));
        remove_test_config(&path);
    }

    #[test]
    fn desktop_preferences_round_trip_and_default_normally() {
        let path = test_config_path();
        let preferences = DesktopPreferences {
            theme: Theme::Light,
            language: AppLanguage::English,
            onboarding_complete: true,
            start_on_login: true,
            start_minimized_to_tray: true,
            keep_running_in_tray: true,
            notifications_enabled: true,
        };
        let updated = set_desktop_preferences_at(preferences.clone(), &path).unwrap();
        assert_eq!(updated.desktop_preferences, preferences);
        assert_eq!(
            load_config_from(&path)
                .unwrap()
                .unwrap()
                .desktop_preferences,
            preferences
        );
        remove_test_config(&path);
    }

    #[test]
    fn invalid_theme_is_rejected_without_rewriting_config() {
        let path = test_config_path();
        save_config_to(&ProviderConfig::defaults(), &path).unwrap();
        let original = fs::read(&path).unwrap();
        let invalid = serde_json::json!({
            "theme": "ultraviolet",
            "language": "english",
            "start_on_login": false,
            "start_minimized_to_tray": false,
            "keep_running_in_tray": false,
            "notifications_enabled": false
        });
        assert!(serde_json::from_value::<DesktopPreferences>(invalid).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        remove_test_config(&path);
    }

    #[test]
    fn performance_update_preserves_existing_resource_policy_and_rejects_bad_values() {
        let path = test_config_path();
        let mut config = ProviderConfig::defaults();
        config.resource_settings.mode = ResourceMode::ScheduledHours;
        config.resource_settings.schedule = Some(ResourceSchedule {
            weekdays: vec![1],
            start: "09:00".into(),
            end: "17:00".into(),
        });
        save_config_to(&config, &path).unwrap();
        let original = fs::read(&path).unwrap();

        for invalid in [-1, 101, 255] {
            assert!(set_performance_percent_at(invalid, &path).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        for valid in [0, 1, 100] {
            let updated = set_performance_percent_at(valid, &path).unwrap();
            assert_eq!(updated.resource_settings.performance_percent, valid as u8);
        }
        let updated = load_config_from(&path).unwrap().unwrap();
        assert_eq!(updated.resource_settings.performance_percent, 100);
        assert_eq!(updated.resource_settings.mode, ResourceMode::ScheduledHours);
        remove_test_config(&path);
    }

    #[test]
    fn out_of_range_config_performance_is_rejected_without_rewriting() {
        let path = test_config_path();
        for value in [-1, 101] {
            let mut json = serde_json::to_value(ProviderConfig::defaults()).unwrap();
            json["resource_settings"]["performance_percent"] = serde_json::json!(value);
            let bytes = serde_json::to_vec(&json).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(load_config_from(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        remove_test_config(&path);
    }

    #[test]
    fn resource_settings_reject_invalid_values_and_preserve_config() {
        let path = test_config_path();
        save_config_to(&ProviderConfig::defaults(), &path).unwrap();
        let original = fs::read(&path).unwrap();

        let invalid = ResourceSettings {
            idle_threshold_minutes: 0,
            ..ResourceSettings::default()
        };
        assert!(set_resource_settings_at(invalid, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        let invalid_json = serde_json::json!({
            "payout_address": null,
            "provider_id": "test-provider",
            "coordinator_ws": "ws://127.0.0.1:8081/ws",
            "ollama_model": "llama3.2:3b",
            "resource_settings": {
                "mode": "scheduled_hours",
                "idle_threshold_minutes": 5,
                "schedule": null,
                "preset": "balanced",
                "do_not_run_on_battery": true,
                "not_recommended_confirmed": false
            }
        });
        fs::write(&path, serde_json::to_vec(&invalid_json).unwrap()).unwrap();
        assert!(load_config_from(&path).is_err());
        remove_test_config(&path);
    }

    #[test]
    fn resource_schedule_round_trips_validated_local_hours() {
        let path = test_config_path();
        let settings = ResourceSettings {
            mode: ResourceMode::ScheduledHours,
            schedule: Some(ResourceSchedule {
                weekdays: vec![1, 5],
                start: "22:15".into(),
                end: "06:45".into(),
            }),
            ..ResourceSettings::default()
        };
        let config = set_resource_settings_at(settings.clone(), &path).unwrap();
        assert_eq!(config.resource_settings, settings);
        assert_eq!(
            load_config_from(&path).unwrap().unwrap().resource_settings,
            settings
        );
        remove_test_config(&path);
    }

    #[test]
    fn not_recommended_confirmation_is_stored_in_config() {
        let path = test_config_path();
        let settings = ResourceSettings {
            not_recommended_confirmed: true,
            ..ResourceSettings::default()
        };
        set_resource_settings_at(settings, &path).unwrap();
        let loaded = load_config_from(&path).unwrap().unwrap();
        assert!(loaded.resource_settings.not_recommended_confirmed);
        remove_test_config(&path);
    }

    #[test]
    fn resource_settings_updates_cannot_clear_hardware_confirmation() {
        let path = test_config_path();
        let confirmed = ResourceSettings {
            not_recommended_confirmed: true,
            ..ResourceSettings::default()
        };
        set_resource_settings_at(confirmed, &path).unwrap();
        set_resource_settings_at(ResourceSettings::default(), &path).unwrap();
        assert!(
            load_config_from(&path)
                .unwrap()
                .unwrap()
                .resource_settings
                .not_recommended_confirmed
        );
        remove_test_config(&path);
    }
}
