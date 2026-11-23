//! DeLore GPU provider CLI.

use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use provider_core::{
    config_dir, decide_current_policy, detect_gpu, load_config, run_provider_with_confirmation,
    runtime_suitability, save_config, validate_payout_address, NativeSystemProbe, ProviderConfig,
    RuntimeManager, RuntimeProgressCallback, RuntimeState, RuntimeSuitability, SystemProbe,
};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

#[derive(Parser)]
#[command(name = "gpuswarm", about = "GPU Swarm provider daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Setup,
    Start {
        #[arg(long)]
        once: bool,
    },
    Status,
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommands>,
    },
    Uninstall,
}

#[derive(Subcommand)]
enum ConfigCommands {
    SetAddress { address: String },
}

fn confirm(message: &str) -> Result<bool> {
    print!("{message} [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn validate_public_address(address: &str) -> Result<()> {
    let trimmed = address.trim();
    if (trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()))
        || trimmed
            .strip_prefix("0x")
            .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        || trimmed.split_whitespace().count() == 12
        || trimmed.split_whitespace().count() == 24
    {
        anyhow::bail!("Never enter your private key or seed phrase. Only your PUBLIC address.");
    }
    if trimmed.len() != 42
        || !trimmed.starts_with("0x")
        || !trimmed[2..].chars().all(|c| c.is_ascii_hexdigit())
    {
        anyhow::bail!("Address must be 0x followed by exactly 40 hexadecimal characters.");
    }
    validate_payout_address(trimmed)?;
    verify_eip55_checksum(trimmed)
}

fn verify_eip55_checksum(address: &str) -> Result<()> {
    let body = &address[2..];
    let has_upper = body.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = body.chars().any(|c| c.is_ascii_lowercase());
    if !has_upper || !has_lower {
        return Ok(());
    }
    let lower = body.to_ascii_lowercase();
    let digest = Keccak256::digest(lower.as_bytes());
    for (index, ch) in body.chars().enumerate() {
        if ch.is_ascii_alphabetic() {
            let hash_byte = digest[index / 2];
            let hash_nibble = if index % 2 == 0 {
                hash_byte >> 4
            } else {
                hash_byte & 0x0f
            };
            let should_upper = hash_nibble >= 8;
            if should_upper != ch.is_ascii_uppercase() {
                anyhow::bail!("Public address is not valid EIP-55 checksum.");
            }
        }
    }
    Ok(())
}

fn run_setup() -> Result<()> {
    println!("GPU SWARM");
    println!("GPU Swarm lets you rent out your idle GPU.");
    println!("Jobs are AI text prompts. Nothing else on your PC is accessed.");
    println!("You can stop anytime with Ctrl+C.");

    let gpu = detect_gpu();
    println!("Detected GPU: {}", gpu.gpu_name);
    println!("GPU memory: {}", gpu.gpu_memory);
    if gpu.support == "integrated-intel-unsupported" {
        println!("Integrated GPU, not supported: jobs run on CPU.");
    }

    let existing = load_config()?.unwrap_or_else(ProviderConfig::defaults);
    let model = std::env::var("OLLAMA_MODEL").unwrap_or(existing.ollama_model.clone());
    match runtime_suitability() {
        RuntimeSuitability::Suitable => {
            println!("Managed Ollama runtime will be prepared on first start.");
        }
        RuntimeSuitability::NotRecommended { reason } => {
            println!("Not recommended: {reason}");
            println!("Managed Ollama runtime will be prepared only after explicit confirmation.");
        }
    }

    let address = loop {
        print!("Payout address (0x followed by 40 hex characters): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let address = input.trim();
        match validate_public_address(address) {
            Ok(()) => {
                println!("Address: {address}");
                if confirm("Is this correct?")? {
                    println!("Test mode: no real money is paid.");
                    break address.to_string();
                }
            }
            Err(error) => println!("Warning: {error}"),
        }
    };

    let mut config = existing;
    config.payout_address = Some(address);
    config.coordinator_ws = std::env::var("COORDINATOR_WS").unwrap_or(config.coordinator_ws);
    config.ollama_model = model;
    save_config(&config)?;
    println!(
        "Saved config to {}",
        provider_core::config_path()?.display()
    );
    println!("Setup complete. Run: gpuswarm start");
    Ok(())
}

fn runtime_progress_callback() -> RuntimeProgressCallback {
    let last_percent = Arc::new(AtomicU8::new(u8::MAX));
    Arc::new(move |state| match state {
        RuntimeState::Downloading {
            phase,
            percent,
            bytes,
            expected_total_download_bytes,
        } => {
            let last = last_percent.load(Ordering::Relaxed);
            if last == u8::MAX
                || percent < last
                || percent == 100
                || percent >= last.saturating_add(5)
            {
                let action = match phase {
                    provider_core::RuntimeDownloadPhase::Runtime => "Runtime",
                    provider_core::RuntimeDownloadPhase::Model => "Model",
                };
                println!(
                    "{action} download: {percent}% ({bytes} bytes transferred; expected combined download: {expected_total_download_bytes} bytes)"
                );
                last_percent.store(percent, Ordering::Relaxed);
            }
        }
        state => println!("Runtime: {state:?}"),
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct FeesStatus {
    fee_bps: u64,
    provider_share_percent: u64,
    fee_address: String,
}

fn coordinator_http_url(coordinator_ws: &str) -> Result<String> {
    let http_override = match std::env::var("COORDINATOR_HTTP") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("COORDINATOR_HTTP contains invalid non-Unicode data")
        }
    };
    provider_core::coordinator_fees_url_with_override(coordinator_ws, http_override.as_deref())
}

fn percent_from_bps(fee_bps: u64) -> String {
    let whole = fee_bps / 100;
    let remainder = fee_bps % 100;
    if remainder == 0 {
        format!("{whole}%")
    } else {
        format!("{whole}.{remainder:02}%")
    }
}

async fn print_status() -> Result<()> {
    let config = load_config()?;
    match &config {
        Some(config) => {
            let provider_id =
                std::env::var("PROVIDER_ID").unwrap_or_else(|_| config.provider_id.clone());
            println!("Provider ID: {}", provider_id);
            println!("Coordinator: {}", config.coordinator_ws);
            println!("Ollama model: {}", config.ollama_model);
            println!(
                "Payout address: {}",
                config.payout_address.as_deref().unwrap_or("not set")
            );
        }
        None => println!("Configuration: not set (run `gpuswarm setup`)"),
    }

    if let Some(config) = config.as_ref() {
        let coordinator_ws =
            std::env::var("COORDINATOR_WS").unwrap_or_else(|_| config.coordinator_ws.clone());
        let url = coordinator_http_url(&coordinator_ws)?;
        match reqwest::Client::new().get(url).send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<FeesStatus>().await {
                    Ok(fees) => {
                        println!(
                            "Platform fee: {} — you receive {}% of each job price.",
                            percent_from_bps(fees.fee_bps),
                            fees.provider_share_percent
                        );
                        println!("Platform fee recipient: {}", fees.fee_address);
                    }
                    Err(_) => println!("Platform fee: unknown, will be shown on connect"),
                }
            }
            _ => println!("Platform fee: unknown, will be shown on connect"),
        }
    } else {
        println!("Platform fee: unknown, will be shown on connect");
    }

    let gpu = detect_gpu();
    println!("GPU: {}", gpu.gpu_name);
    println!("GPU memory: {}", gpu.gpu_memory);
    println!("GPU support: {}", gpu.support);
    if gpu.support == "integrated-intel-unsupported" {
        println!("Integrated GPU, not supported: jobs run on CPU.");
    }
    let resource_settings = config
        .as_ref()
        .map(|config| config.resource_settings.clone())
        .unwrap_or_default();
    let logical_cpu_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let mapping = resource_settings.preset.mapping(
        logical_cpu_count,
        resource_settings.mode == provider_core::ResourceMode::AlwaysOnLowPriority,
    );
    let system = NativeSystemProbe.sample();
    let policy = decide_current_policy(&resource_settings, system.idle, system.power, false);
    println!(
        "Resource policy: mode={:?}, idle threshold={} min, preset={:?}, decision={policy:?}",
        resource_settings.mode, resource_settings.idle_threshold_minutes, resource_settings.preset
    );
    println!(
        "Preset mapping: threads={}, keep-alive={}, priority={:?} (supported={}), concurrency={}, max loaded models={}",
        mapping.num_thread,
        mapping.keep_alive,
        mapping.process_priority,
        mapping.priority_supported,
        mapping.ollama_num_parallel,
        mapping.max_loaded_models
    );
    println!(
        "System probe: idle={:?}, power={:?}, diagnostic={}",
        system.idle,
        system.power,
        system.diagnostic.as_deref().unwrap_or("none")
    );
    let runtime = RuntimeManager::new()?;
    if runtime.runtime_dir().exists() {
        runtime.executable_path()?;
        println!(
            "Managed runtime: ready (v{})",
            provider_core::runtime::OLLAMA_VERSION
        );
    } else {
        println!("Managed runtime: not installed");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Setup => run_setup()?,
        Commands::Start { once } => {
            let config = load_config()?.ok_or_else(|| {
                anyhow::anyhow!("GPU Swarm is not configured. Run `gpuswarm setup` first.")
            })?;
            let confirmed_not_recommended = if !config.resource_settings.not_recommended_confirmed {
                match runtime_suitability() {
                    RuntimeSuitability::Suitable => false,
                    RuntimeSuitability::NotRecommended { reason } => {
                        println!("Not recommended: {reason}");
                        if !confirm("Continue with managed runtime preparation and CPU inference?")?
                        {
                            anyhow::bail!("runtime preparation cancelled");
                        }
                        true
                    }
                }
            } else {
                false
            };
            run_provider_with_confirmation(
                config,
                confirmed_not_recommended,
                once,
                runtime_progress_callback(),
            )
            .await?;
        }
        Commands::Status => print_status().await?,
        Commands::Config { command: None } => match load_config()? {
            Some(config) => println!("{}", serde_json::to_string_pretty(&config)?),
            None => println!("No configuration found. Run `gpuswarm setup` first."),
        },
        Commands::Config {
            command: Some(ConfigCommands::SetAddress { address }),
        } => {
            validate_public_address(&address)?;
            let mut config = load_config()?.unwrap_or_else(ProviderConfig::defaults);
            config.payout_address = Some(address.trim().to_string());
            save_config(&config)?;
            println!(
                "Saved config to {}",
                config_dir()?.join("config.json").display()
            );
        }
        Commands::Uninstall => {
            if confirm("Delete the GpuSwarm configuration?")? {
                let path = provider_core::config_path()?;
                if path.exists() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to delete {}", path.display()))?;
                }
                let dir = config_dir()?;
                if dir.exists() {
                    let _ = std::fs::remove_dir(&dir);
                }
                println!("Configuration deleted.");
            } else {
                println!("Uninstall cancelled.");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::coordinator_http_url;

    #[test]
    fn fee_endpoint_uses_http_or_https_for_the_coordinator_scheme() {
        assert_eq!(
            coordinator_http_url("ws://127.0.0.1:8081/ws").unwrap(),
            "http://127.0.0.1:8080/fees"
        );
        assert_eq!(
            coordinator_http_url("wss://coordinator.example/ws").unwrap(),
            "https://coordinator.example/fees"
        );
        assert_eq!(
            coordinator_http_url("WSS://coordinator.example/ws").unwrap(),
            "https://coordinator.example/fees"
        );
        assert!(coordinator_http_url("ws://coordinator.example/ws").is_err());
    }
}
