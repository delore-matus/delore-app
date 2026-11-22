use serde::Serialize;
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceMetrics {
    pub runtime_cpu_percent: Option<f32>,
    pub runtime_ram_bytes: Option<u64>,
    pub gpu_utilization_percent: Option<u32>,
    pub gpu_vram_used_bytes: Option<u64>,
    pub gpu_device_wide: bool,
}

pub struct ResourceMonitor {
    system: System,
    nvml: Option<nvml_wrapper::Nvml>,
    previous_process_id: Option<u32>,
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self {
            system: System::new(),
            nvml: nvml_wrapper::Nvml::init().ok(),
            previous_process_id: None,
        }
    }
}

impl ResourceMonitor {
    pub fn sample(&mut self, process_id: Option<u32>) -> ResourceMetrics {
        let process = process_id.and_then(|process_id| {
            let pid = Pid::from_u32(process_id);
            self.system
                .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
            self.system.process(pid)
        });
        let stable_process_sample = process_id.is_some() && self.previous_process_id == process_id;
        let (runtime_cpu_percent, runtime_ram_bytes) = process.map_or((None, None), |process| {
            (
                stable_process_sample.then(|| process.cpu_usage()),
                Some(process.memory()),
            )
        });
        self.previous_process_id = process_id;
        let (gpu_utilization_percent, gpu_vram_used_bytes) = sample_nvidia_gpu(self.nvml.as_ref());

        ResourceMetrics {
            runtime_cpu_percent,
            runtime_ram_bytes,
            gpu_utilization_percent,
            gpu_vram_used_bytes,
            gpu_device_wide: gpu_utilization_percent.is_some() || gpu_vram_used_bytes.is_some(),
        }
    }
}

fn sample_nvidia_gpu(nvml: Option<&nvml_wrapper::Nvml>) -> (Option<u32>, Option<u64>) {
    let Some(nvml) = nvml else {
        return (None, None);
    };
    let device = match nvml.device_by_index(0) {
        Ok(device) => device,
        Err(_) => return (None, None),
    };
    let utilization = device.utilization_rates().ok().map(|rates| rates.gpu);
    let vram = device.memory_info().ok().map(|memory| memory.used);
    (utilization, vram)
}

#[cfg(test)]
mod tests {
    use super::{ResourceMetrics, ResourceMonitor};

    #[test]
    fn unavailable_process_and_gpu_metrics_are_not_reported_as_zero() {
        let mut monitor = ResourceMonitor::default();
        let metrics = monitor.sample(Some(u32::MAX));
        assert_eq!(metrics.runtime_cpu_percent, None);
        assert_eq!(metrics.runtime_ram_bytes, None);
        assert_eq!(metrics.gpu_utilization_percent, None);
        assert_eq!(metrics.gpu_vram_used_bytes, None);
        assert!(!metrics.gpu_device_wide);
    }

    #[test]
    fn metrics_shape_marks_gpu_values_as_device_wide() {
        let metrics = ResourceMetrics {
            runtime_cpu_percent: Some(0.0),
            runtime_ram_bytes: Some(1),
            gpu_utilization_percent: Some(0),
            gpu_vram_used_bytes: Some(2),
            gpu_device_wide: true,
        };
        assert!(metrics.gpu_device_wide);
    }
}
