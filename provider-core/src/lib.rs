pub use common::FeeConfig;

pub mod config;
pub mod gpu;
pub mod ollama;
pub mod provider;
pub mod runtime;

pub use config::{
    config_dir, config_path, effective_ollama_model, load_config, save_config, set_ollama_model,
    set_payout_address, validate_ollama_model, validate_payout_address, OllamaModelSelection,
    ProviderConfig,
};
pub use gpu::{detect_gpu, parse_windows_adapters, GpuInfo, WindowsAdapter};
pub use ollama::run_ollama;
pub use provider::{
    run_provider, run_provider_managed, run_provider_with_confirmation, ConnectionState,
    JobHistoryEntry, ProviderEvent, ProviderEventCallback,
};
pub use runtime::{
    runtime_suitability, RunningRuntime, RuntimeDownloadPhase, RuntimeInstall, RuntimeManager,
    RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
};
