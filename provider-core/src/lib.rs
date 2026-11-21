pub mod config;
pub mod gpu;
pub mod ollama;
pub mod provider;

pub use config::{config_dir, config_path, load_config, save_config, ProviderConfig};
pub use gpu::{detect_gpu, GpuInfo};
pub use ollama::{ollama_is_installed, ollama_model_is_installed, run_ollama};
pub use provider::{run_provider, JobHistoryEntry};
