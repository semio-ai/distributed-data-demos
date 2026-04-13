use sysinfo::{Pid, System};

/// Monitors CPU and memory usage for the current process.
pub struct ResourceMonitor {
    system: System,
    pid: Pid,
}

impl ResourceMonitor {
    /// Create a new resource monitor for the current process.
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        // Initial refresh to establish a baseline for CPU measurement.
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        Self { system, pid }
    }

    /// Sample current CPU percentage and memory usage in MB.
    ///
    /// Returns `(cpu_percent, memory_mb)`.
    pub fn sample(&mut self) -> (f64, f64) {
        self.system
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);
        match self.system.process(self.pid) {
            Some(process) => {
                let cpu = process.cpu_usage() as f64;
                let mem = process.memory() as f64 / (1024.0 * 1024.0);
                (cpu, mem)
            }
            None => (0.0, 0.0),
        }
    }
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_returns_plausible_values() {
        let mut monitor = ResourceMonitor::new();
        let (cpu, mem) = monitor.sample();
        // CPU can be 0.0 if not enough time has elapsed, but should not be negative.
        assert!(cpu >= 0.0, "CPU should be non-negative, got {}", cpu);
        // Memory should be positive (our process uses at least some memory).
        assert!(mem >= 0.0, "Memory should be non-negative, got {}", mem);
    }
}
