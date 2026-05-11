use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Threading execution model the variant is asked to use.
///
/// Introduced by E14 to characterise both single-threaded synchronous
/// operation (WASM-compatible; no tokio) and multi-threaded operation
/// (typically a per-peer reader thread on the receive side). The runner
/// injects the chosen mode via the `--threading-mode` CLI arg; the
/// driver passes it to `Variant::connect`. Each variant decides what
/// the mode means inside its own implementation.
///
/// Variants declare which modes they support via
/// [`crate::variant_trait::Variant::supported_threading_modes`]. The
/// runner consults that declaration and skips spawns whose mode the
/// variant cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadingMode {
    /// Single-threaded synchronous operation. No tokio, no per-peer
    /// reader threads. The driver thread drives publish and
    /// `poll_receive` in turn. WASM-compatible.
    Single,
    /// Multi-threaded operation. The variant may spawn OS threads
    /// (typically one per peer connection on the receive side) to
    /// decouple per-message parse cost from the driver's poll cadence.
    /// "Multi" does NOT imply async -- transports that fundamentally
    /// need a runtime (QUIC, WebRTC, Zenoh) may use one here, but
    /// transports that don't (websocket, hybrid, custom-udp) should
    /// not introduce one just because they spawn threads.
    Multi,
}

impl ThreadingMode {
    /// Return the lowercase string representation: `"single"` or `"multi"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadingMode::Single => "single",
            ThreadingMode::Multi => "multi",
        }
    }
}

impl fmt::Display for ThreadingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parse a `ThreadingMode` from a case-insensitive `"single"` / `"multi"`
/// string. Used by clap's `value_parser` on `--threading-mode` and by
/// tests.
impl FromStr for ThreadingMode {
    type Err = ThreadingModeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "single" => Ok(ThreadingMode::Single),
            "multi" => Ok(ThreadingMode::Multi),
            _ => Err(ThreadingModeParseError(s.to_string())),
        }
    }
}

/// Error returned by `<ThreadingMode as FromStr>::from_str` for any
/// value other than (case-insensitive) `"single"` / `"multi"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadingModeParseError(pub String);

impl fmt::Display for ThreadingModeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid threading mode '{}': expected 'single' or 'multi'",
            self.0
        )
    }
}

impl std::error::Error for ThreadingModeParseError {}

/// Quality of Service levels for data replication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Qos {
    BestEffort = 1,
    LatestValue = 2,
    ReliableUdp = 3,
    ReliableTcp = 4,
}

impl Qos {
    /// Convert an integer to a QoS level.
    pub fn from_int(value: u8) -> Option<Qos> {
        match value {
            1 => Some(Qos::BestEffort),
            2 => Some(Qos::LatestValue),
            3 => Some(Qos::ReliableUdp),
            4 => Some(Qos::ReliableTcp),
            _ => None,
        }
    }

    /// Return the integer representation.
    pub fn as_int(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for Qos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_int())
    }
}

/// Test protocol phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Connect,
    Stabilize,
    Operate,
    Eot,
    Silent,
}

impl Phase {
    /// Return the phase name as a lowercase string.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Connect => "connect",
            Phase::Stabilize => "stabilize",
            Phase::Operate => "operate",
            Phase::Eot => "eot",
            Phase::Silent => "silent",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A received update from the transport layer.
#[derive(Debug, Clone)]
pub struct ReceivedUpdate {
    /// Runner name of the node that wrote the value.
    pub writer: String,
    /// The writer's sequence number for this update.
    pub seq: u64,
    /// Key path (e.g. `/sensors/lidar`).
    pub path: String,
    /// QoS level.
    pub qos: Qos,
    /// Serialized payload bytes.
    pub payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threading_mode_display_is_lowercase() {
        assert_eq!(ThreadingMode::Single.to_string(), "single");
        assert_eq!(ThreadingMode::Multi.to_string(), "multi");
    }

    #[test]
    fn threading_mode_from_str_accepts_canonical_case_insensitive() {
        assert_eq!(
            "single".parse::<ThreadingMode>().unwrap(),
            ThreadingMode::Single
        );
        assert_eq!(
            "multi".parse::<ThreadingMode>().unwrap(),
            ThreadingMode::Multi
        );
        // Case-insensitive.
        assert_eq!(
            "Single".parse::<ThreadingMode>().unwrap(),
            ThreadingMode::Single
        );
        assert_eq!(
            "MULTI".parse::<ThreadingMode>().unwrap(),
            ThreadingMode::Multi
        );
    }

    #[test]
    fn threading_mode_from_str_rejects_unknown() {
        let err = "neither".parse::<ThreadingMode>().unwrap_err();
        assert!(err.to_string().contains("neither"));
        assert!(err.to_string().contains("single"));
        assert!(err.to_string().contains("multi"));
    }

    #[test]
    fn threading_mode_parse_display_roundtrip() {
        for mode in [ThreadingMode::Single, ThreadingMode::Multi] {
            let rendered = mode.to_string();
            let parsed: ThreadingMode = rendered.parse().unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn threading_mode_serde_uses_lowercase_tags() {
        // Serde tags are lowercase per `#[serde(rename_all = "lowercase")]`.
        let single = serde_json::to_string(&ThreadingMode::Single).unwrap();
        let multi = serde_json::to_string(&ThreadingMode::Multi).unwrap();
        assert_eq!(single, "\"single\"");
        assert_eq!(multi, "\"multi\"");

        // Symmetric: lowercase JSON deserialises back to the variant.
        let de_single: ThreadingMode = serde_json::from_str("\"single\"").unwrap();
        let de_multi: ThreadingMode = serde_json::from_str("\"multi\"").unwrap();
        assert_eq!(de_single, ThreadingMode::Single);
        assert_eq!(de_multi, ThreadingMode::Multi);
    }
}
