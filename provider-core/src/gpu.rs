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
                        support: "nvidia".to_string(),
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
                        return GpuInfo {
                            gpu_name: line.trim().to_string(),
                            gpu_memory: "unknown".to_string(),
                            support: "amd-or-other-best-effort".to_string(),
                        };
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
                    if let Some(line) = text.lines().skip(1).find(|line| !line.trim().is_empty()) {
                        return GpuInfo {
                            gpu_name: line.trim().to_string(),
                            gpu_memory: "unknown".to_string(),
                            support: "amd-or-other-best-effort".to_string(),
                        };
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
    if nvidia.support == "nvidia" {
        return nvidia;
    }
    BestEffortDetector.detect()
}
