use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Default grace period (ms) inserted between consecutive spawn jobs derived
/// from the same `[[variant]]` entry. Gives sockets time to release before
/// the next spawn re-binds the same port.
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

/// Specification for a positive-integer field that accepts either a scalar or
/// a non-empty array. Used for `tick_rate_hz` and `values_per_tick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositiveSpec {
    /// `field = N` -- a single concrete value.
    Single(u32),
    /// `field = [..]` -- an explicit list of values.
    Multi(Vec<u32>),
}

impl PositiveSpec {
    /// Return the concrete values to run, in ascending order, deduplicated.
    pub fn values(&self) -> Vec<u32> {
        match self {
            PositiveSpec::Single(n) => vec![*n],
            PositiveSpec::Multi(v) => {
                let set: BTreeSet<u32> = v.iter().copied().collect();
                set.into_iter().collect()
            }
        }
    }

    /// Validate that all elements are positive and arrays are non-empty.
    pub fn validate(&self, field_name: &str) -> Result<()> {
        match self {
            PositiveSpec::Single(n) => {
                if *n == 0 {
                    bail!("{field_name} must be a positive integer (got 0)");
                }
                Ok(())
            }
            PositiveSpec::Multi(v) => {
                if v.is_empty() {
                    bail!("{field_name} array must be non-empty");
                }
                for n in v {
                    if *n == 0 {
                        bail!("{field_name} array contains 0; all values must be positive");
                    }
                }
                Ok(())
            }
        }
    }
}

/// Parse a `PositiveSpec` from a TOML value: either an integer or an array
/// of integers. Returns an error for any other shape, for non-positive
/// integers, or for values that exceed `u32::MAX`.
fn parse_positive_spec(field_name: &str, val: &toml::Value) -> Result<PositiveSpec> {
    if let Some(n) = val.as_integer() {
        let n = u32::try_from(n)
            .with_context(|| format!("{field_name} value {n} does not fit in u32"))?;
        let spec = PositiveSpec::Single(n);
        spec.validate(field_name)?;
        return Ok(spec);
    }

    if let Some(arr) = val.as_array() {
        let mut out: Vec<u32> = Vec::with_capacity(arr.len());
        for item in arr {
            let n = item.as_integer().with_context(|| {
                format!("{field_name} array element is not an integer: {item:?}")
            })?;
            let n = u32::try_from(n)
                .with_context(|| format!("{field_name} value {n} does not fit in u32"))?;
            out.push(n);
        }
        let spec = PositiveSpec::Multi(out);
        spec.validate(field_name)?;
        return Ok(spec);
    }

    bail!("{field_name} must be an integer or an array of integers (got {val:?})");
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
    /// Optional inter-spawn grace period (milliseconds) inserted between
    /// consecutive spawn jobs derived from the same variant entry. Defaults
    /// to `DEFAULT_INTER_QOS_GRACE_MS`.
    #[serde(default)]
    pub inter_qos_grace_ms: Option<u64>,
    /// Reusable variant defaults referenced by `[[variant]]` entries via
    /// `template = "<name>"`. Templates do not spawn.
    #[serde(default, rename = "variant_template")]
    pub variant_templates: Vec<VariantTemplate>,
    /// Variant definitions, executed in order.
    #[serde(default)]
    pub variant: Vec<VariantConfig>,
}

/// A reusable set of variant defaults, referenced by `[[variant]]` entries
/// via `template = "<name>"`.
#[derive(Debug, Clone, Deserialize)]
pub struct VariantTemplate {
    /// Template identifier (must be unique). Not a spawn name.
    pub name: String,
    /// Default binary path for variants that reference this template.
    #[serde(default)]
    pub binary: Option<String>,
    /// Default per-variant timeout (seconds).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Default `[variant.common]` keys.
    #[serde(default)]
    pub common: Option<toml::Table>,
    /// Default `[variant.specific]` keys.
    #[serde(default)]
    pub specific: Option<toml::Table>,
}

/// A single variant definition within the benchmark config.
#[derive(Debug, Clone, Deserialize)]
pub struct VariantConfig {
    /// Unique variant name.
    pub name: String,
    /// Optional reference to a `[[variant_template]]` whose defaults this
    /// entry inherits. Resolved in `BenchConfig::resolve_templates`.
    #[serde(default)]
    pub template: Option<String>,
    /// Path to the variant executable (relative to runner CWD). May be
    /// inherited from a referenced template; required after resolution.
    #[serde(default)]
    pub binary: String,
    /// Per-variant timeout override (seconds).
    pub timeout_secs: Option<u64>,
    /// Common arguments passed to all variant instances.
    #[serde(default)]
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

    /// Parse the `tick_rate_hz` field from `[variant.common]`. Required
    /// after template resolution.
    pub fn tick_rate_spec(&self) -> Result<PositiveSpec> {
        let val = self
            .common
            .get("tick_rate_hz")
            .context("tick_rate_hz is required in [variant.common]")?;
        parse_positive_spec("tick_rate_hz", val)
    }

    /// Parse the `values_per_tick` field from `[variant.common]`. Required
    /// after template resolution.
    pub fn values_per_tick_spec(&self) -> Result<PositiveSpec> {
        let val = self
            .common
            .get("values_per_tick")
            .context("values_per_tick is required in [variant.common]")?;
        parse_positive_spec("values_per_tick", val)
    }
}

impl BenchConfig {
    /// Parse and validate a benchmark config from a TOML file path.
    ///
    /// Template resolution runs before validation so all downstream code
    /// (spawn-job expansion, CLI-arg construction, validation rules) sees
    /// fully resolved variant entries.
    pub fn from_file(path: &Path) -> Result<(Self, String)> {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let hash = config_hash(&raw);
        let text = String::from_utf8(raw).context("config file is not valid UTF-8")?;
        let mut config: BenchConfig =
            toml::from_str(&text).context("failed to parse TOML config")?;
        config.resolve_templates()?;
        config.validate()?;
        Ok((config, hash))
    }

    /// Apply each `[[variant]]`'s `template = "..."` reference: deep-merge
    /// the named template's `common`/`specific` tables (variant entry wins
    /// on conflict) and fall through to the template's `binary`/
    /// `timeout_secs` when the variant entry omits them.
    ///
    /// Idempotent. Templates are validated for unique names. A
    /// `template = "..."` reference that does not match any defined
    /// template is a hard error.
    pub fn resolve_templates(&mut self) -> Result<()> {
        let mut templates_by_name: HashMap<String, &VariantTemplate> = HashMap::new();
        for t in &self.variant_templates {
            if t.name.is_empty() {
                bail!("config: variant_template has an empty 'name'");
            }
            if templates_by_name.insert(t.name.clone(), t).is_some() {
                bail!("config: duplicate variant_template name '{}'", t.name);
            }
        }

        for v in &mut self.variant {
            let Some(template_name) = v.template.clone() else {
                continue;
            };
            let template = templates_by_name
                .get(template_name.as_str())
                .copied()
                .with_context(|| {
                    format!(
                        "config: variant '{}' references unknown template '{}'",
                        v.name, template_name
                    )
                })?;

            if v.binary.is_empty() {
                if let Some(b) = &template.binary {
                    v.binary = b.clone();
                }
            }
            if v.timeout_secs.is_none() {
                v.timeout_secs = template.timeout_secs;
            }
            if let Some(template_common) = &template.common {
                merge_table_keys(&mut v.common, template_common);
            }
            if let Some(template_specific) = &template.specific {
                let target = v.specific.get_or_insert_with(toml::Table::new);
                merge_table_keys(target, template_specific);
            }
        }

        Ok(())
    }

    /// Validate config according to the schema contract rules. Assumes
    /// template resolution has already been applied.
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

        for v in &self.variant {
            v.qos_spec()
                .with_context(|| format!("config: variant '{}' has invalid qos", v.name))?;
            v.tick_rate_spec().with_context(|| {
                format!("config: variant '{}' has invalid tick_rate_hz", v.name)
            })?;
            v.values_per_tick_spec().with_context(|| {
                format!("config: variant '{}' has invalid values_per_tick", v.name)
            })?;
        }

        Ok(())
    }

    /// Effective inter-spawn grace period (milliseconds).
    pub fn inter_qos_grace_ms(&self) -> u64 {
        self.inter_qos_grace_ms
            .unwrap_or(DEFAULT_INTER_QOS_GRACE_MS)
    }
}

/// Merge keys from `source` into `target`. Existing keys in `target` are
/// preserved (the variant entry wins on conflict); only missing keys are
/// copied over. No deep-table merging — keys are unioned at the top level
/// of the table only.
fn merge_table_keys(target: &mut toml::Table, source: &toml::Table) {
    for (key, val) in source {
        target.entry(key.clone()).or_insert_with(|| val.clone());
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

    fn parse(toml_str: &str) -> BenchConfig {
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        cfg
    }

    #[test]
    fn parse_sample_config() {
        let config = parse(sample_toml());
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
        assert_eq!(h1.len(), 64);
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
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
  tick_rate_hz = 1
  values_per_tick = 1

[[variant]]
name = "dup"
binary = "./b"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 1
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
  tick_rate_hz = 1
  values_per_tick = 1
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
  tick_rate_hz = 1
  values_per_tick = 1
  qos = 5
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
  tick_rate_hz = 1
  values_per_tick = 1
  qos = [1, 5]
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
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
  tick_rate_hz = 1
  values_per_tick = 1
  qos = []
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
        let err = config.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("qos array must be non-empty"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn qos_spec_single_integer() {
        let cfg = parse(
            r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 1
  qos = 2
"#,
        );
        let spec = cfg.variant[0].qos_spec().unwrap();
        assert_eq!(spec, QosSpec::Single(2));
        assert_eq!(spec.levels(), vec![2]);
    }

    #[test]
    fn qos_spec_array_form() {
        let cfg = parse(
            r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 1
  qos = [3, 1, 1, 4]
"#,
        );
        let spec = cfg.variant[0].qos_spec().unwrap();
        assert_eq!(spec, QosSpec::Multi(vec![3, 1, 1, 4]));
        assert_eq!(spec.levels(), vec![1, 3, 4]);
    }

    #[test]
    fn qos_spec_omitted_means_all() {
        let cfg = parse(
            r#"
run = "test"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 1
"#,
        );
        let spec = cfg.variant[0].qos_spec().unwrap();
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

    #[test]
    fn tick_rate_spec_scalar() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
"#,
        );
        let spec = cfg.variant[0].tick_rate_spec().unwrap();
        assert_eq!(spec, PositiveSpec::Single(100));
        assert_eq!(spec.values(), vec![100]);
    }

    #[test]
    fn tick_rate_spec_array_dedup_sorted() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [100, 10, 100, 1000]
  values_per_tick = 1
"#,
        );
        let spec = cfg.variant[0].tick_rate_spec().unwrap();
        assert_eq!(spec, PositiveSpec::Multi(vec![100, 10, 100, 1000]));
        assert_eq!(spec.values(), vec![10, 100, 1000]);
    }

    #[test]
    fn tick_rate_spec_rejects_zero() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 0
  values_per_tick = 1
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tick_rate_hz must be a positive integer"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn tick_rate_spec_rejects_empty_array() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = []
  values_per_tick = 1
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tick_rate_hz array must be non-empty"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn tick_rate_spec_rejects_array_with_zero() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, 0, 100]
  values_per_tick = 1
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tick_rate_hz array contains 0"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn tick_rate_spec_rejects_non_integer_element() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, "oops"]
  values_per_tick = 1
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tick_rate_hz array element is not an integer"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn values_per_tick_spec_scalar_and_array() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = [10, 100, 10]

[[variant]]
name = "v2"
binary = "./x"
  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 5
"#,
        );
        let spec = cfg.variant[0].values_per_tick_spec().unwrap();
        assert_eq!(spec.values(), vec![10, 100]);
        let spec2 = cfg.variant[1].values_per_tick_spec().unwrap();
        assert_eq!(spec2, PositiveSpec::Single(5));
    }

    #[test]
    fn template_resolution_merges_common_and_specific() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant_template]]
name = "udp-base"
binary = "target/release/variant-custom-udp.exe"
  [variant_template.common]
  stabilize_secs = 3
  operate_secs = 30
  silent_secs = 3
  workload = "scalar-flood"
  log_dir = "./logs"
  [variant_template.specific]
  multicast_group = "239.0.0.1:19500"
  buffer_size = 65536

[[variant]]
template = "udp-base"
name = "custom-udp"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1000
  [variant.specific]
  tcp_base_port = 19800
"#,
        );
        let v = &cfg.variant[0];
        assert_eq!(v.binary, "target/release/variant-custom-udp.exe");
        assert_eq!(
            v.common.get("stabilize_secs").unwrap().as_integer(),
            Some(3)
        );
        assert_eq!(
            v.common.get("workload").unwrap().as_str(),
            Some("scalar-flood")
        );
        assert_eq!(v.common.get("log_dir").unwrap().as_str(), Some("./logs"));
        assert_eq!(
            v.common.get("tick_rate_hz").unwrap().as_integer(),
            Some(100)
        );
        let spec = v.specific.as_ref().unwrap();
        assert_eq!(
            spec.get("multicast_group").unwrap().as_str(),
            Some("239.0.0.1:19500")
        );
        assert_eq!(spec.get("tcp_base_port").unwrap().as_integer(), Some(19800));
    }

    #[test]
    fn template_variant_keys_win_on_conflict() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant_template]]
name = "tpl"
binary = "./from-template"
timeout_secs = 999
  [variant_template.common]
  workload = "scalar-flood"
  tick_rate_hz = 1
  values_per_tick = 1

[[variant]]
template = "tpl"
name = "v"
binary = "./from-variant"
timeout_secs = 42
  [variant.common]
  workload = "max-throughput"
"#,
        );
        let v = &cfg.variant[0];
        assert_eq!(v.binary, "./from-variant");
        assert_eq!(v.timeout_secs, Some(42));
        assert_eq!(
            v.common.get("workload").unwrap().as_str(),
            Some("max-throughput")
        );
        // Template defaults still flow through for missing keys.
        assert_eq!(v.common.get("tick_rate_hz").unwrap().as_integer(), Some(1));
    }

    #[test]
    fn template_falls_through_top_level_scalars() {
        let cfg = parse(
            r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant_template]]
name = "tpl"
binary = "./tpl-bin"
timeout_secs = 17
  [variant_template.common]
  tick_rate_hz = 1
  values_per_tick = 1

[[variant]]
template = "tpl"
name = "v"
"#,
        );
        let v = &cfg.variant[0];
        assert_eq!(v.binary, "./tpl-bin");
        assert_eq!(v.timeout_secs, Some(17));
    }

    #[test]
    fn template_unknown_name_is_error() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
template = "missing"
name = "v"
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = cfg.resolve_templates().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown template 'missing'"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn template_duplicate_name_is_error() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant_template]]
name = "tpl"
binary = "./a"

[[variant_template]]
name = "tpl"
binary = "./b"
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        let err = cfg.resolve_templates().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate variant_template name 'tpl'"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn template_resolution_requires_binary() {
        let toml_str = r#"
run = "t"
runners = ["a"]
default_timeout_secs = 10

[[variant_template]]
name = "tpl"
  [variant_template.common]
  tick_rate_hz = 1
  values_per_tick = 1

[[variant]]
template = "tpl"
name = "v"
"#;
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty 'binary' path"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn two_runner_all_variants_expands_to_expected_spawn_list() {
        // Locks in the post-rewrite spawn list for the headline config: every
        // (variant family, vpt, hz, qos) combo from the original config is
        // present, and no extras have crept in. Five families
        // (custom-udp, hybrid, quic, zenoh, webrtc) emit the full 4-qos
        // expansion (32 spawns each = 160), while websocket is restricted to
        // qos [3, 4] (16 spawns), for a total of 176.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("configs/two-runner-all-variants.toml");
        let (config, _) = BenchConfig::from_file(&path).unwrap();

        let mut spawn_names: Vec<String> = Vec::new();
        for (idx, v) in config.variant.iter().enumerate() {
            for job in crate::spawn_job::expand_variant(idx, v).unwrap() {
                spawn_names.push(job.effective_name);
            }
        }

        let mut expected: Vec<String> = Vec::new();
        let full_qos_families = ["custom-udp", "hybrid", "quic", "zenoh", "webrtc"];
        let vpt_hz_pairs: &[(u32, u32)] = &[
            (1000, 100),
            (1000, 10),
            (100, 1000),
            (100, 100),
            (100, 10),
            (10, 100),
            (10, 1000),
        ];
        for fam in &full_qos_families {
            for (vpt, hz) in vpt_hz_pairs {
                for qos in 1..=4 {
                    expected.push(format!("{fam}-{vpt}x{hz}hz-qos{qos}"));
                }
            }
            for qos in 1..=4 {
                expected.push(format!("{fam}-max-qos{qos}"));
            }
        }
        // websocket family: qos restricted to [3, 4].
        for (vpt, hz) in vpt_hz_pairs {
            for qos in [3, 4] {
                expected.push(format!("websocket-{vpt}x{hz}hz-qos{qos}"));
            }
        }
        for qos in [3, 4] {
            expected.push(format!("websocket-max-qos{qos}"));
        }

        let mut sorted_actual = spawn_names.clone();
        sorted_actual.sort();
        let mut sorted_expected = expected.clone();
        sorted_expected.sort();
        assert_eq!(
            sorted_actual,
            sorted_expected,
            "spawn-name set mismatch (actual count={}, expected count={})",
            sorted_actual.len(),
            sorted_expected.len()
        );
        assert_eq!(
            spawn_names.len(),
            176,
            "expected 5 families x 32 + websocket 16 = 176 spawns"
        );
    }

    #[test]
    fn multi_machine_10peer_config_expands_as_documented() {
        // Verifies the 10-peer config produces the spawn-count documented in
        // its header (4 sweep families x 16 + 4 max + websocket 4x2 + max 1x2
        // = 64 + 4 + 8 + 2 = ... let's compute it properly here).
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("configs/multi-machine-10peer-all.toml");
        let (config, _) = BenchConfig::from_file(&path).unwrap();
        assert_eq!(config.runners.len(), 10);

        let mut total = 0usize;
        let mut by_family: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (idx, v) in config.variant.iter().enumerate() {
            let jobs = crate::spawn_job::expand_variant(idx, v).unwrap();
            total += jobs.len();
            for job in jobs {
                let family = job.effective_name.split('-').next().unwrap().to_string();
                *by_family.entry(family).or_default() += 1;
            }
        }

        // custom-udp / hybrid / quic / zenoh: each entry sweeps 2 hz x 2 vpt = 4
        // combos x 4 qos = 16 + 1 max x 4 qos = 4 -> 20 spawns each.
        // websocket: 2x2 = 4 combos x 2 qos = 8 + 1 max x 2 qos = 2 -> 10 spawns.
        // Total: 4 * 20 + 10 = 90.
        assert_eq!(total, 90, "expected 90 spawns total, got {total}");
        for fam in &["custom", "hybrid", "quic", "zenoh"] {
            assert_eq!(
                by_family.get(*fam).copied().unwrap_or(0),
                20,
                "family '{fam}' should produce 20 spawns"
            );
        }
        assert_eq!(
            by_family.get("websocket").copied().unwrap_or(0),
            10,
            "websocket should produce 10 spawns"
        );
    }

    #[test]
    fn all_repo_configs_parse() {
        // Every config in the repo's `configs/` directory must parse cleanly
        // through the runner's loader. Catches contract drift in any config
        // file when the parser changes.
        let configs_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("configs");
        let mut count = 0usize;
        for entry in std::fs::read_dir(&configs_dir).expect("configs/ must exist") {
            let entry = entry.expect("readable dir entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            BenchConfig::from_file(&path)
                .unwrap_or_else(|e| panic!("failed to parse {}: {e:#}", path.display()));
            count += 1;
        }
        assert!(count > 0, "expected at least one config in {configs_dir:?}");
    }
}
