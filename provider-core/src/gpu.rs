use std::process::Command;

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub gpu_name: String,
    pub gpu_memory: String,
    pub support: String,
}

pub trait GpuDetector {
    fn detect(&self) -> GpuInfo;
}

pub struct NvidiaDetector;

fn classify_gpu_name(name: &str) -> GpuInfo {
    let lower = name.to_lowercase();
    if lower.contains("intel")
        && (lower.contains("hd ")
            || lower.contains("uhd")
            || lower.contains("iris"))
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
            if let Ok(output) = Command::new("wmic")
                .args([
                    "path",
                    "win32_VideoController",
                    "get",
                    "Name,AdapterRAM",
                    "/format:csv",
                ])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    for line in text.lines().skip(1) {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let fields: Vec<&str> = line.split(',').collect();
                        let name = fields
                            .last()
                            .map(|value| value.trim())
                            .filter(|value| !value.is_empty());
                        if let Some(name) = name {
                            let lower = name.to_lowercase();
                            if lower.contains("intel")
                                || lower.contains("amd")
                                || lower.contains("radeon")
                                || lower.contains("nvidia")
                            {
                                return classify_gpu_name(name);
                            }
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
    use super::classify_gpu_name;

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
