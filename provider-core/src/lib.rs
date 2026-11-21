pub mod config;
pub mod gpu;
pub mod ollama;
pub mod provider;
pub mod runtime;

pub use config::{
    config_dir, config_path, load_config, save_config, validate_payout_address, ProviderConfig,
};
pub use gpu::{detect_gpu, parse_windows_adapters, GpuInfo, WindowsAdapter};
pub use ollama::run_ollama;
pub use provider::{run_provider, run_provider_with_confirmation, JobHistoryEntry};
pub use runtime::{
    runtime_suitability, RunningRuntime, RuntimeManager, RuntimeProgressCallback, RuntimeState,
    RuntimeInstall, RuntimeSuitability,
};
