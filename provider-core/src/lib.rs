pub use common::FeeConfig;

pub mod config;
pub mod gpu;
pub mod ollama;
pub mod platform;
pub mod provider;
pub mod resource;
pub mod resource_monitor;
pub mod runtime;

pub use config::{
    config_dir, config_path, confirm_not_recommended_runtime, effective_ollama_model, load_config,
    save_config, set_ollama_model, set_payout_address, set_resource_settings,
    validate_ollama_model, validate_payout_address, OllamaModelSelection, ProviderConfig,
};
pub use gpu::{detect_gpu, parse_windows_adapters, GpuInfo, WindowsAdapter};
pub use ollama::run_ollama;
pub use ollama::run_ollama_with_settings;
pub use platform::{NativeSystemProbe, SystemProbe, SystemSample};
pub use provider::{
    provider_control_channel, run_provider, run_provider_managed,
    run_provider_managed_with_control, run_provider_managed_with_probe,
    run_provider_with_confirmation, ConnectionState, JobHistoryEntry, ProviderControl,
    ProviderEvent, ProviderEventCallback,
};
pub use resource::{
    decide_current_policy, decide_policy, is_within_schedule, IdleState, PolicyDecision,
    PolicyInput, PowerState, PresetMapping, ProcessPriority, ResourceMode, ResourcePreset,
    ResourceSchedule, ResourceSettings,
};
pub use resource_monitor::{ResourceMetrics, ResourceMonitor};
pub use runtime::{
    ensure_provider_confirmation, runtime_suitability, RunningRuntime, RuntimeDownloadPhase,
    RuntimeInstall, RuntimeManager, RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
};
