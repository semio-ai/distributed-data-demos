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
    /// Clock-sync probe request from `from` to `to`. The initiator records
    /// `t1` (send time) and the responder echoes it back so the initiator
    /// does not need state for in-flight probes. Timestamps are RFC 3339
    /// nanosecond strings.
    ProbeRequest {
        from: String,
        to: String,
        id: u64,
        t1: String,
    },
    /// Clock-sync probe response. `t1` is echoed from the request; `t2` is
    /// the receiver's wall-clock at receive; `t3` is the receiver's
    /// wall-clock at send-back. All timestamps are RFC 3339 nanosecond
    /// strings.
    ProbeResponse {
        from: String,
        to: String,
        id: u64,
        t1: String,
        t2: String,
        t3: String,
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

    #[test]
    fn probe_request_roundtrip() {
        let msg = Message::ProbeRequest {
            from: "a".into(),
            to: "b".into(),
            id: 7,
            t1: "2026-05-03T12:00:00.123456789Z".into(),
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn probe_response_roundtrip() {
        let msg = Message::ProbeResponse {
            from: "b".into(),
            to: "a".into(),
            id: 7,
            t1: "2026-05-03T12:00:00.123456789Z".into(),
            t2: "2026-05-03T12:00:00.123567890Z".into(),
            t3: "2026-05-03T12:00:00.123678901Z".into(),
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn probe_request_json_format() {
        let msg = Message::ProbeRequest {
            from: "a".into(),
            to: "b".into(),
            id: 42,
            t1: "2026-05-03T12:00:00.000000000Z".into(),
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "probe_request");
        assert_eq!(json["from"], "a");
        assert_eq!(json["to"], "b");
        assert_eq!(json["id"], 42);
    }

    #[test]
    fn probe_response_json_format() {
        let msg = Message::ProbeResponse {
            from: "b".into(),
            to: "a".into(),
            id: 42,
            t1: "2026-05-03T12:00:00.000000000Z".into(),
            t2: "2026-05-03T12:00:00.000100000Z".into(),
            t3: "2026-05-03T12:00:00.000200000Z".into(),
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "probe_response");
        assert_eq!(json["from"], "b");
        assert_eq!(json["to"], "a");
        assert_eq!(json["id"], 42);
    }
}
