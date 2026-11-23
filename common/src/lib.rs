//! Shared messages used by both sides of the MVP WebSocket connection.
//!
//! Keeping these types in one crate prevents the coordinator and provider
//! from using slightly different JSON shapes.

use serde::{Deserialize, Serialize};

pub fn is_valid_model_name(model: &str) -> bool {
    let bytes = model.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
        && !model.contains("..")
        && !model.contains("//")
}

/// A unit of mock GPU work submitted to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobPayload {
    /// A client-provided identifier used to correlate a result with a job.
    pub id: String,
    /// The mock prompt/task description sent to the provider.
    pub prompt: String,
}

/// The result produced by a provider after completing a mock job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// Must match the id of the JobPayload that produced this result.
    pub job_id: String,
    /// Provider that produced (or failed to produce) the result.
    pub provider_id: String,
    /// Ollama's generated text.
    pub result_data: String,
    /// Wall-clock time spent processing the job.
    pub duration_ms: u128,
    /// Number of tokens Ollama evaluated for the response.
    pub eval_count: u32,
    /// Ollama evaluation throughput.
    pub tokens_per_sec: f64,
    /// Clear provider/Ollama error text, if processing failed.
    pub error: Option<String>,
}

/// Provider details sent once after a WebSocket connection is established.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRegistration {
    pub provider_id: String,
    pub payout_address: String,
    pub gpu_name: String,
    pub gpu_memory: String,
    pub model: String,
    #[serde(default = "provider_accepts_jobs_by_default")]
    pub accepting_jobs: bool,
}

fn provider_accepts_jobs_by_default() -> bool {
    true
}

/// Coordinator fee information sent to providers after they connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    pub fee_bps: u64,
    pub job_price_units: u64,
    pub provider_payout_units: u64,
    pub fee_address: String,
}

/// Messages exchanged over the coordinator/provider WebSocket.
///
/// `serde(tag = "type", content = "payload")` makes the wire format easy to
/// inspect, for example: `{"type":"job","payload":{"id":"...","prompt":"..."}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum WsMessage {
    /// A job sent from coordinator to provider.
    #[serde(rename = "job")]
    Job(JobPayload),

    /// A result sent from provider back to coordinator.
    #[serde(rename = "result")]
    Result(JobResult),

    /// A provider's identity and GPU details sent after connecting.
    #[serde(rename = "register")]
    Register(ProviderRegistration),

    /// The coordinator's current mock-ledger fee configuration.
    #[serde(rename = "fee_config")]
    FeeConfig(FeeConfig),

    /// Whether this connected provider can receive another job.
    #[serde(rename = "availability")]
    Availability { accepting_jobs: bool },
}

#[cfg(test)]
mod tests {
    use super::{is_valid_model_name, ProviderRegistration, WsMessage};

    #[test]
    fn model_names_use_a_restricted_safe_character_set() {
        assert!(is_valid_model_name("llama3.2:3b"));
        assert!(is_valid_model_name("hf.co/example/model:Q4_K_M"));
        for invalid in [
            "",
            "../escape",
            "model//tag",
            "bad name",
            "bad\\name",
            ".bad",
        ] {
            assert!(!is_valid_model_name(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn older_provider_registration_defaults_to_accepting_jobs() {
        let registration: ProviderRegistration = serde_json::from_str(
            r#"{
                "provider_id": "provider",
                "payout_address": "not-set",
                "gpu_name": "GPU",
                "gpu_memory": "unknown",
                "model": "model"
            }"#,
        )
        .unwrap();
        assert!(registration.accepting_jobs);
    }

    #[test]
    fn availability_message_has_explicit_paused_state() {
        let message = WsMessage::Availability {
            accepting_jobs: false,
        };
        let json = serde_json::to_value(message).unwrap();
        assert_eq!(json["type"], "availability");
        assert_eq!(json["payload"]["accepting_jobs"], false);
    }
}
