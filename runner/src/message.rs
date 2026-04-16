use serde::{Deserialize, Serialize};

/// Coordination protocol messages exchanged between runners over UDP broadcast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Discovery announcement with config hash for verification.
    Discover {
        name: String,
        config_hash: String,
        log_subdir: String,
    },
    /// Ready barrier signal for a specific variant.
    Ready {
        name: String,
        variant: String,
        run: String,
    },
    /// Done barrier signal with execution outcome.
    Done {
        name: String,
        variant: String,
        run: String,
        status: String,
        exit_code: i32,
    },
}

impl Message {
    /// Serialize to JSON bytes for sending over UDP.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("message serialization should not fail")
    }

    /// Deserialize from JSON bytes received over UDP.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_roundtrip() {
        let msg = Message::Discover {
            name: "a".into(),
            config_hash: "abc123".into(),
            log_subdir: "run01-20260415_120000".into(),
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn ready_roundtrip() {
        let msg = Message::Ready {
            name: "b".into(),
            variant: "zenoh-replication".into(),
            run: "run01".into(),
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn done_roundtrip() {
        let msg = Message::Done {
            name: "a".into(),
            variant: "zenoh-replication".into(),
            run: "run01".into(),
            status: "success".into(),
            exit_code: 0,
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn discover_json_format() {
        let msg = Message::Discover {
            name: "a".into(),
            config_hash: "hash123".into(),
            log_subdir: "run01-20260415_120000".into(),
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "discover");
        assert_eq!(json["name"], "a");
        assert_eq!(json["config_hash"], "hash123");
        assert_eq!(json["log_subdir"], "run01-20260415_120000");
    }

    #[test]
    fn done_json_format() {
        let msg = Message::Done {
            name: "a".into(),
            variant: "v1".into(),
            run: "run01".into(),
            status: "timeout".into(),
            exit_code: -1,
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "done");
        assert_eq!(json["run"], "run01");
        assert_eq!(json["status"], "timeout");
        assert_eq!(json["exit_code"], -1);
    }

    #[test]
    fn invalid_bytes_returns_none() {
        assert!(Message::from_bytes(b"not json").is_none());
        assert!(Message::from_bytes(b"{}").is_none());
    }
}
