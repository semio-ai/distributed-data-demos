use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

/// Top-level benchmark configuration parsed from a TOML file.
#[derive(Debug, Deserialize)]
pub struct BenchConfig {
    /// Unique identifier for this benchmark run.
    pub run: String,
    /// Runner names expected in this benchmark.
    pub runners: Vec<String>,
    /// Default timeout for variant processes (seconds).
    pub default_timeout_secs: u64,
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

        // Validate qos in common sections if present.
        for v in &self.variant {
            if let Some(qos_val) = v.common.get("qos") {
                if let Some(qos) = qos_val.as_integer() {
                    if !(1..=4).contains(&qos) {
                        bail!("config: variant '{}' has qos {} (must be 1-4)", v.name, qos);
                    }
                }
            }
        }

        Ok(())
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
        assert!(
            err.to_string().contains("qos 5 (must be 1-4)"),
            "unexpected error: {err}"
        );
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
