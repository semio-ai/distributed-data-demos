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
        /// Whether this runner was launched with `--resume`. All runners must
        /// agree on this flag's value or discovery aborts. Defaults to `false`
        /// when missing for backwards compatibility with older peer binaries.
        #[serde(default)]
        resume: bool,
    },
    /// Resume-mode inventory of locally complete spawn jobs (Phase 1.25).
    ///
    /// `complete_jobs` is the sorted, deduplicated list of `effective_name`s
    /// for which the local log file exists and is non-empty. Empty files are
    /// deleted before this message is broadcast. The cross-runner intersection
    /// of these sets becomes the run's "skip set".
    ResumeManifest {
        name: String,
        run: String,
        complete_jobs: Vec<String>,
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
            resume: false,
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn discover_roundtrip_with_resume_true() {
        let msg = Message::Discover {
            name: "a".into(),
            config_hash: "abc123".into(),
            log_subdir: "run01-20260415_120000".into(),
            resume: true,
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
            resume: false,
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "discover");
        assert_eq!(json["name"], "a");
        assert_eq!(json["config_hash"], "hash123");
        assert_eq!(json["log_subdir"], "run01-20260415_120000");
        assert_eq!(json["resume"], false);
    }

    #[test]
    fn discover_missing_resume_field_defaults_to_false() {
        // Backwards compatibility: older binaries serialize Discover without
        // a `resume` field. They must still parse successfully and be treated
        // as `resume = false`.
        let json = br#"{"type":"discover","name":"a","config_hash":"h","log_subdir":"sub"}"#;
        let parsed = Message::from_bytes(json).unwrap();
        match parsed {
            Message::Discover {
                name,
                config_hash,
                log_subdir,
                resume,
            } => {
                assert_eq!(name, "a");
                assert_eq!(config_hash, "h");
                assert_eq!(log_subdir, "sub");
                assert!(!resume);
            }
            _ => panic!("expected Discover variant"),
        }
    }

    #[test]
    fn resume_manifest_roundtrip() {
        let msg = Message::ResumeManifest {
            name: "a".into(),
            run: "run01".into(),
            complete_jobs: vec!["zenoh-qos1".into(), "udp-qos2".into()],
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn resume_manifest_json_format() {
        let msg = Message::ResumeManifest {
            name: "alice".into(),
            run: "run01".into(),
            complete_jobs: vec!["v1".into(), "v2".into()],
        };
        let json: serde_json::Value = serde_json::from_slice(&msg.to_bytes()).unwrap();
        assert_eq!(json["type"], "resume_manifest");
        assert_eq!(json["name"], "alice");
        assert_eq!(json["run"], "run01");
        let jobs = json["complete_jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0], "v1");
        assert_eq!(jobs[1], "v2");
    }

    #[test]
    fn resume_manifest_empty_jobs() {
        // A runner with no completed jobs should still send a manifest with
        // an empty array (this is how the intersection rule learns that this
        // peer has nothing to skip).
        let msg = Message::ResumeManifest {
            name: "alice".into(),
            run: "run01".into(),
            complete_jobs: vec![],
        };
        let bytes = msg.to_bytes();
        let parsed = Message::from_bytes(&bytes).unwrap();
        assert_eq!(msg, parsed);
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
