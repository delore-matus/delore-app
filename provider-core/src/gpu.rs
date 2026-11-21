use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub gpu_name: String,
    pub gpu_memory: String,
    pub support: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsAdapter {
    pub name: String,
}

#[derive(Deserialize)]
struct WindowsCimAdapter {
    #[serde(rename = "Name")]
    name: Option<String>,
}

pub fn parse_windows_adapters(input: &str) -> Vec<WindowsAdapter> {
    let input = input.trim();
    if input.is_empty() {
        return Vec::new();
    }

    let value = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };

    let objects: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(values) => values.iter().collect(),
        serde_json::Value::Object(_) => vec![&value],
        _ => return Vec::new(),
    };

    let mut adapters = Vec::new();
    for object in objects {
        let parsed = serde_json::from_value::<WindowsCimAdapter>(object.clone());
        let Ok(adapter) = parsed else {
            continue;
        };
        let Some(name) = adapter.name else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let lower = name.to_lowercase();
        if lower.contains("microsoft basic")
            || lower.contains("remote display")
            || lower.contains("virtual")
        {
            continue;
        }
        adapters.push(WindowsAdapter {
            name: name.to_string(),
        });
    }
    adapters
}

pub trait GpuDetector {
    fn detect(&self) -> GpuInfo;
}

pub struct NvidiaDetector;

fn classify_gpu_name(name: &str) -> GpuInfo {
    let lower = name.to_lowercase();
    if lower.contains("intel")
        && (lower.contains("hd ") || lower.contains("uhd") || lower.contains("iris"))
    {
        return GpuInfo {
            gpu_name: name.trim().to_string(),
            gpu_memory: "unknown".to_string(),
            support: "integrated-intel-unsupported".to_string(),
        };
    }
    if lower.contains("amd") || lower.contains("radeon") {
        return GpuInfo {
            gpu_name: name.trim().to_string(),
            gpu_memory: "unknown".to_string(),
            support: "integrated-amd-best-effort".to_string(),
        };
    }
    GpuInfo {
        gpu_name: name.trim().to_string(),
        gpu_memory: "unknown".to_string(),
        support: "other-best-effort".to_string(),
    }
}

impl GpuDetector for NvidiaDetector {
    fn detect(&self) -> GpuInfo {
        let output = Command::new("nvidia-smi")
            .args(["--query-gpu=name,memory.total", "--format=csv,noheader"])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                if let Some(line) = text.lines().next() {
                    let mut fields = line.splitn(2, ',');
                    let gpu_name = fields.next().unwrap_or("unknown").trim().to_string();
                    let gpu_memory = fields.next().unwrap_or("unknown").trim().to_string();
                    return GpuInfo {
                        gpu_name,
                        gpu_memory,
                        support: "nvidia-supported".to_string(),
                    };
                }
            }
            _ => {}
        }
        GpuInfo {
            gpu_name: "unknown/CPU".to_string(),
            gpu_memory: "unknown".to_string(),
            support: "none".to_string(),
        }
    }
}

pub struct BestEffortDetector;

impl GpuDetector for BestEffortDetector {
    fn detect(&self) -> GpuInfo {
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = Command::new("system_profiler")
                .arg("SPDisplaysDataType")
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if let Some(line) = text.lines().find(|line| line.contains("Chipset Model")) {
                        let name = line.split(':').nth(1).unwrap_or("unknown").trim();
                        return GpuInfo {
                            gpu_name: name.to_string(),
                            gpu_memory: "unknown".to_string(),
                            support: "apple-silicon-best-effort".to_string(),
                        };
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(output) = Command::new("lspci").output() {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if let Some(line) = text.lines().find(|line| {
                        line.to_lowercase().contains("vga")
                            || line.to_lowercase().contains("3d controller")
                    }) {
                        return classify_gpu_name(line);
                    }
                }
            }
        }

        #[cfg(target_os = "windows")]
        {
            if let Ok(output) = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Get-CimInstance Win32_VideoController | Select-Object Name,AdapterRAM | ConvertTo-Json -Compress",
                ])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    for adapter in parse_windows_adapters(&text) {
                        let lower = adapter.name.to_lowercase();
                        if lower.contains("intel")
                            || lower.contains("amd")
                            || lower.contains("radeon")
                        {
                            return classify_gpu_name(&adapter.name);
                        }
                    }
                }
            }
        }

        GpuInfo {
            gpu_name: "unknown/CPU".to_string(),
            gpu_memory: "unknown".to_string(),
            support: "none".to_string(),
        }
    }
}

pub fn detect_gpu() -> GpuInfo {
    let nvidia = NvidiaDetector.detect();
    if nvidia.support == "nvidia-supported" {
        return nvidia;
    }
    BestEffortDetector.detect()
}

#[cfg(test)]
mod tests {
    use super::{classify_gpu_name, parse_windows_adapters};

    #[test]
    fn parses_header_only_or_csv_garbage_as_empty() {
        assert!(parse_windows_adapters("Node,AdapterRAM,Name").is_empty());
        assert!(parse_windows_adapters("NODE,4294967296,Intel").is_empty());
    }

    #[test]
    fn parses_single_adapter_object() {
        let adapters = parse_windows_adapters(
            r#"{"Name":"Intel(R) HD Graphics 520","AdapterRAM":1073741824}"#,
        );
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name, "Intel(R) HD Graphics 520");
    }

    #[test]
    fn parses_multiple_adapters_and_skips_virtual() {
        let adapters = parse_windows_adapters(
            r#"[
                {"Name":"Microsoft Basic Display Adapter","AdapterRAM":0},
                {"Name":"Intel(R) HD Graphics 520","AdapterRAM":1073741824},
                {"Name":"Remote Display Adapter","AdapterRAM":0}
            ]"#,
        );
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name, "Intel(R) HD Graphics 520");
    }

    #[test]
    fn parses_empty_output_as_empty() {
        assert!(parse_windows_adapters("").is_empty());
        assert!(parse_windows_adapters("   ").is_empty());
    }

    #[test]
    fn parses_invalid_json_as_empty() {
        assert!(parse_windows_adapters("{not json").is_empty());
        assert!(parse_windows_adapters("null").is_empty());
    }

    #[test]
    fn intel_hd_is_integrated_and_unsupported() {
        let gpu = classify_gpu_name("Intel(R) HD Graphics 520");
        assert_eq!(gpu.gpu_name, "Intel(R) HD Graphics 520");
        assert_eq!(gpu.support, "integrated-intel-unsupported");
        assert_eq!(gpu.gpu_memory, "unknown");
    }

    #[test]
    fn intel_iris_is_integrated_and_unsupported() {
        let gpu = classify_gpu_name("Intel Iris Xe Graphics");
        assert_eq!(gpu.support, "integrated-intel-unsupported");
    }

    #[test]
    fn amd_is_integrated_best_effort() {
        let gpu = classify_gpu_name("AMD Radeon(TM) Graphics");
        assert_eq!(gpu.support, "integrated-amd-best-effort");
    }
}
