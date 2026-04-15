use anyhow::{bail, Result};

/// A single write operation produced by a workload profile.
#[derive(Debug, Clone)]
pub struct WriteOp {
    /// Key path (e.g. `/bench/0`).
    pub path: String,
    /// Serialized payload bytes.
    pub payload: Vec<u8>,
}

/// Trait for workload profiles that generate write operations each tick.
pub trait Workload {
    /// Generate write operations for one tick.
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp>;
}

/// Scalar-flood workload: generates `values_per_tick` writes to paths
/// `/bench/0`, `/bench/1`, ... with 8-byte f64 payloads.
pub struct ScalarFlood {
    tick_counter: u64,
}

impl ScalarFlood {
    pub fn new() -> Self {
        Self { tick_counter: 0 }
    }
}

impl Default for ScalarFlood {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for ScalarFlood {
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp> {
        self.tick_counter += 1;
        (0..values_per_tick)
            .map(|i| {
                let value: f64 =
                    (self.tick_counter * u64::from(values_per_tick) + u64::from(i)) as f64;
                WriteOp {
                    path: format!("/bench/{}", i),
                    payload: value.to_le_bytes().to_vec(),
                }
            })
            .collect()
    }
}

/// Create a workload profile by name.
///
/// Returns an error for unknown workload names.
pub fn create_workload(name: &str) -> Result<Box<dyn Workload>> {
    match name {
        "scalar-flood" | "max-throughput" => Ok(Box::new(ScalarFlood::new())),
        _ => bail!("unknown workload profile: {}", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scalar_flood_count() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(5);
        assert_eq!(ops.len(), 5);
    }

    #[test]
    fn test_scalar_flood_paths() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(3);
        assert_eq!(ops[0].path, "/bench/0");
        assert_eq!(ops[1].path, "/bench/1");
        assert_eq!(ops[2].path, "/bench/2");
    }

    #[test]
    fn test_scalar_flood_payload_size() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(1);
        assert_eq!(ops[0].payload.len(), 8, "payload should be 8 bytes (f64)");
    }

    #[test]
    fn test_create_workload_valid() {
        let wl = create_workload("scalar-flood");
        assert!(wl.is_ok());
    }

    #[test]
    fn test_create_workload_unknown() {
        let wl = create_workload("nonexistent");
        assert!(wl.is_err());
    }
}
