use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// Default grace period (ms) inserted between consecutive per-QoS spawn jobs
/// derived from the same `[[variant]]` entry. Gives sockets time to release
/// before the next QoS spawn re-binds the same port.
pub const DEFAULT_INTER_QOS_GRACE_MS: u64 = 250;

/// QoS specification for a `[[variant]]` entry. Accepts an integer, an array
/// of integers, or omission. Drives spawn-job expansion: each concrete level
/// produces one spawn invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QosSpec {
    /// `qos = N` -- a single concrete level.
    Single(u8),
    /// `qos = [..]` -- an explicit list of levels.
    Multi(Vec<u8>),
    /// `qos` key omitted -- expand to all four levels.
    All,
}

impl QosSpec {
    /// Return the concrete QoS levels to run, in ascending order, deduplicated.
    pub fn levels(&self) -> Vec<u8> {
        match self {
            QosSpec::Single(n) => vec![*n],
            QosSpec::Multi(v) => {
                let set: BTreeSet<u8> = v.iter().copied().collect();
                set.into_iter().collect()
            }
            QosSpec::All => vec![1, 2, 3, 4],
        }
    }

    /// Validate that all elements are in the 1..=4 range and arrays are non-empty.
    pub fn validate(&self) -> Result<()> {
        match self {
            QosSpec::Single(n) => {
                if !(1..=4).contains(n) {
                    bail!("qos {n} is out of range (must be 1..=4)");
                }
                Ok(())
            }
            QosSpec::Multi(v) => {
                if v.is_empty() {
                    bail!("qos array must be non-empty");
                }
                for n in v {
                    if !(1..=4).contains(n) {
                        bail!("qos {n} is out of range (must be 1..=4)");
                    }
                }
                Ok(())
            }
            QosSpec::All => Ok(()),
        }
    }
}

/// Top-level benchmark configuration parsed from a TOML file.
#[derive(Debug, Deserialize)]
pub struct BenchConfig {
    /// Unique identifier for this benchmark run.
    pub run: String,
    /// Runner names expected in this benchmark.
    pub runners: Vec<String>,
    /// Default timeout for variant processes (seconds).
    pub default_timeout_secs: u64,
    /// Optional inter-QoS grace period (milliseconds) inserted between
    /// consecutive per-QoS spawn jobs derived from the same variant entry.
    /// Defaults to `DEFAULT_INTER_QOS_GRACE_MS`.
    #[serde(default)]
    pub inter_qos_grace_ms: Option<u64>,
    /// Variant definitions, executed in order.
    #[serde(default)]
    pub variant: Vec<VariantConfig>,
}

/// A single variant definition within the benchmark config.
#[derive(Debug, Deserialize)]
pub struct VariantConfig {
    /// Unique variant name.
    pub name: String,
    /// Path to the variant executable (relative to runner CWD).
    pub binary: String,
    /// Per-variant timeout override (seconds).
    pub timeout_secs: Option<u64>,
    /// Common arguments passed to all variant instances.
    pub common: toml::Table,
    /// Variant-specific arguments.
    pub specific: Option<toml::Table>,
}

impl VariantConfig {
    /// Returns the effective timeout for this variant.
    pub fn effective_timeout(&self, default: u64) -> u64 {
        self.timeout_secs.unwrap_or(default)
    }

    /// Parse the `qos` field from the `[variant.common]` table into a
    /// `QosSpec`. Returns `QosSpec::All` if the key is absent.
    ///
    /// Accepts:
    /// - `qos = N` (integer, 1..=4) -> `Single(N)`
    /// - `qos = [..]` (non-empty array of 1..=4 integers) -> `Multi(..)`
    /// - key omitted -> `All`
    pub fn qos_spec(&self) -> Result<QosSpec> {
        let Some(val) = self.common.get("qos") else {
            return Ok(QosSpec::All);
        };

        if let Some(n) = val.as_integer() {
            let n = u8::try_from(n).with_context(|| format!("qos value {n} does not fit in u8"))?;
            let spec = QosSpec::Single(n);
            spec.validate()?;
            return Ok(spec);
        }

        if let Some(arr) = val.as_array() {
            let mut levels: Vec<u8> = Vec::with_capacity(arr.len());
            for item in arr {
                let n = item
                    .as_integer()
                    .with_context(|| format!("qos array element is not an integer: {item:?}"))?;
                let n =
                    u8::try_from(n).with_context(|| format!("qos value {n} does not fit in u8"))?;
                levels.push(n);
            }
            let spec = QosSpec::Multi(levels);
            spec.validate()?;
            return Ok(spec);
        }

        bail!("qos must be an integer, an array of integers, or omitted (got {val:?})");
    }
}

impl BenchConfig {
    /// Parse and validate a benchmark config from a TOML file path.
    pub fn from_file(path: &Path) -> Result<(Self, String)> {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let hash = config_hash(&raw);
        let text = String::from_utf8(raw).context("config file is not valid UTF-8")?;
        let config: BenchConfig = toml::from_str(&text).context("failed to parse TOML config")?;
        config.validate()?;
        Ok((config, hash))
    }

    /// Validate config according to the schema contract rules.
    pub fn validate(&self) -> Result<()> {
        if self.run.is_empty() {
            bail!("config: 'run' must be non-empty");
        }
        if self.runners.is_empty() {
            bail!("config: 'runners' must contain at least one name");
        }
        if self.default_timeout_secs == 0 {
            bail!("config: 'default_timeout_secs' must be positive");
        }

        // Check for duplicate variant names.
        let mut seen = HashSet::new();
        for v in &self.variant {
            if !seen.insert(&v.name) {
                bail!("config: duplicate variant name '{}'", v.name);
            }
            if v.binary.is_empty() {
                bail!("config: variant '{}' has an empty 'binary' path", v.name);
            }
            if let Some(t) = v.timeout_secs {
                if t == 0 {
                    bail!(
                        "config: variant '{}' has timeout_secs of 0 (must be positive)",
                        v.name
                    );
                }
            }
        }

        // Validate qos in common sections via QosSpec parsing. This handles
        // integer, array, and omitted forms uniformly and rejects out-of-range
        // values.
        for v in &self.variant {
            v.qos_spec()
                .with_context(|| format!("config: variant '{}' has invalid qos", v.name))?;
        }

        Ok(())
    }

    /// Effective inter-QoS grace period (milliseconds).
    pub fn inter_qos_grace_ms(&self) -> u64 {
        self.inter_qos_grace_ms
            .unwrap_or(DEFAULT_INTER_QOS_GRACE_MS)
    }
}

/// Compute SHA-256 hash of raw file bytes, hex-encoded.
pub fn config_hash(raw: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw);
    let result = hasher.finalize();
    hex_encode(&result)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_toml() -> &'static str {
        r#"
run = "test01"
runners = ["a", "b"]
default_timeout_secs = 120

[[variant]]
name = "zenoh-replication"
binary = "./zenoh-variant"
timeout_secs = 60

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 5
  operate_secs = 30
  silent_secs = 3
  workload = "scalar-flood"
  values_per_tick = 10
  qos = 2
  log_dir = "./logs"

  [variant.specific]
  zenoh_mode = "peer"
  zenoh_listen = "udp/0.0.0.0:7447"
"#
    }

    #[test]
    fn parse_sample_config() {
        let config: BenchConfig = toml::from_str(sample_toml()).unwrap();
        assert_eq!(config.run, "test01");
        assert_eq!(config.runners, vec!["a", "b"]);
        assert_eq!(config.default_timeout_secs, 120);
        assert_eq!(config.variant.len(), 1);

        let v = &config.variant[0];
        assert_eq!(v.name, "zenoh-replication");
        assert_eq!(v.binary, "./zenoh-variant");
        assert_eq!(v.timeout_secs, Some(60));
        assert_eq!(v.effective_timeout(120), 60);
        assert_eq!(
            v.common.get("tick_rate_hz").unwrap().as_integer(),
            Some(100)
        );
        assert!(v.specific.is_some());
        let spec = v.specific.as_ref().unwrap();
        assert_eq!(spec.get("zenoh_mode").unwrap().as_str(), Some("peer"));
    }

    #[test]
    fn config_hash_is_deterministic() {
        let raw = sample_toml().as_bytes();
        let h1 = config_hash(raw);
        let h2 = config_hash(raw);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn config_hash_changes_on_different_input() {
        let h1 = config_hash(b"aaa");
        let h2 = config_hash(b"bbb");
        assert_ne!(h1, h2);
    }

    #[test]
    fn validation_rejects_empty_run() {
        let toml_str = r#"
run = ""
runners = ["a"]
default_timeout_secs = 10
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("'run' must be non-empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_empty_runners() {
        let toml_str = r#"
run = "test"
runners = []
default_timeout_secs = 10
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("'runners' must contain at least one"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_duplicate_variant_names() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "dup"
binary = "./a"
  [variant.common]

[[variant]]
name = "dup"
binary = "./b"
  [variant.common]
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate variant name 'dup'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_empty_binary() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v1"
binary = ""
  [variant.common]
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("empty 'binary' path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_zero_timeout() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 0
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("'default_timeout_secs' must be positive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_invalid_qos() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v1"
binary = "./x"
  [variant.common]
  qos = 5
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("qos 5 is out of range"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn validation_rejects_qos_array_with_out_of_range_element() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v1"
binary = "./x"
  [variant.common]
  qos = [1, 5]
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("qos 5 is out of range"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn validation_rejects_empty_qos_array() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v1"
binary = "./x"
  [variant.common]
  qos = []
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("qos array must be non-empty"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn qos_spec_single_integer() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  qos = 2
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let spec = config.variant[0].qos_spec().unwrap();
        assert_eq!(spec, QosSpec::Single(2));
        assert_eq!(spec.levels(), vec![2]);
    }

    #[test]
    fn qos_spec_array_form() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  qos = [3, 1, 1, 4]
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let spec = config.variant[0].qos_spec().unwrap();
        assert_eq!(spec, QosSpec::Multi(vec![3, 1, 1, 4]));
        // levels() returns sorted, deduplicated.
        assert_eq!(spec.levels(), vec![1, 3, 4]);
    }

    #[test]
    fn qos_spec_omitted_means_all() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let spec = config.variant[0].qos_spec().unwrap();
        assert_eq!(spec, QosSpec::All);
        assert_eq!(spec.levels(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn inter_qos_grace_default_when_omitted() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.inter_qos_grace_ms(), DEFAULT_INTER_QOS_GRACE_MS);
    }

    #[test]
    fn inter_qos_grace_overridden_in_config() {
        let toml_str = r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10
inter_qos_grace_ms = 500
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.inter_qos_grace_ms(), 500);
    }

    #[test]
    fn from_file_roundtrip() {
        let dir = std::env::temp_dir().join("runner_config_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(sample_toml().as_bytes()).unwrap();
        drop(f);

        let (config, hash) = BenchConfig::from_file(&path).unwrap();
        assert_eq!(config.run, "test01");
        assert_eq!(hash.len(), 64);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
