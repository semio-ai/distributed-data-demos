use anyhow::{Context, Result};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;
use zenoh::Wait;

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::variant_trait::Variant;

/// Converts a Zenoh ZResult error into an anyhow error.
fn zenoh_err(e: zenoh::Error) -> anyhow::Error {
    anyhow::anyhow!("{}", e)
}

/// Compact binary codec for messages sent over Zenoh.
///
/// Layout (little-endian):
///   - writer_len: u16
///   - writer: [u8; writer_len]
///   - seq: u64
///   - qos: u8
///   - path_len: u16
///   - path: [u8; path_len]
///   - payload: [u8; remaining]
struct MessageCodec;

impl MessageCodec {
    fn encode(writer: &str, seq: u64, qos: Qos, path: &str, payload: &[u8]) -> Vec<u8> {
        let writer_bytes = writer.as_bytes();
        let path_bytes = path.as_bytes();
        let total = 2 + writer_bytes.len() + 8 + 1 + 2 + path_bytes.len() + payload.len();
        let mut buf = Vec::with_capacity(total);

        buf.extend_from_slice(&(writer_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(writer_bytes);
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.push(qos.as_int());
        buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(path_bytes);
        buf.extend_from_slice(payload);

        buf
    }

    fn decode(data: &[u8]) -> Result<ReceivedUpdate> {
        let mut pos = 0;

        anyhow::ensure!(data.len() >= 2, "message too short for writer_len");
        let writer_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        anyhow::ensure!(
            data.len() >= pos + writer_len,
            "message too short for writer"
        );
        let writer =
            std::str::from_utf8(&data[pos..pos + writer_len]).context("invalid writer UTF-8")?;
        pos += writer_len;

        anyhow::ensure!(data.len() >= pos + 8, "message too short for seq");
        let seq = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        anyhow::ensure!(data.len() > pos, "message too short for qos");
        let qos_val = data[pos];
        let qos = Qos::from_int(qos_val)
            .ok_or_else(|| anyhow::anyhow!("invalid qos value: {}", qos_val))?;
        pos += 1;

        anyhow::ensure!(data.len() >= pos + 2, "message too short for path_len");
        let path_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        anyhow::ensure!(data.len() >= pos + path_len, "message too short for path");
        let path = std::str::from_utf8(&data[pos..pos + path_len]).context("invalid path UTF-8")?;
        pos += path_len;

        let payload = data[pos..].to_vec();

        Ok(ReceivedUpdate {
            writer: writer.to_string(),
            seq,
            path: path.to_string(),
            qos,
            payload,
        })
    }
}

/// Zenoh-specific CLI arguments parsed from the `extra` pass-through args.
pub struct ZenohArgs {
    pub mode: String,
    pub listen: Option<String>,
}

impl ZenohArgs {
    /// Parse Zenoh-specific arguments from the extra CLI args.
    pub fn parse(extra: &[String]) -> Result<Self> {
        let mut mode = String::from("peer");
        let mut listen = None;

        let mut i = 0;
        while i < extra.len() {
            match extra[i].as_str() {
                "--zenoh-mode" => {
                    i += 1;
                    anyhow::ensure!(i < extra.len(), "--zenoh-mode requires a value");
                    mode = extra[i].clone();
                }
                "--zenoh-listen" => {
                    i += 1;
                    anyhow::ensure!(i < extra.len(), "--zenoh-listen requires a value");
                    listen = Some(extra[i].clone());
                }
                other => {
                    anyhow::bail!("unknown Zenoh argument: {}", other);
                }
            }
            i += 1;
        }

        Ok(Self { mode, listen })
    }
}

/// Zenoh variant implementing the `Variant` trait.
///
/// Uses Zenoh's blocking API (the `Wait` trait) for all operations.
/// Messages are published on key expressions under `bench/` and a
/// wildcard subscriber on `bench/**` receives all updates.
pub struct ZenohVariant {
    runner: String,
    zenoh_args: ZenohArgs,
    session: Option<zenoh::Session>,
    subscriber: Option<Subscriber<FifoChannelHandler<Sample>>>,
}

impl ZenohVariant {
    /// Create a new Zenoh variant.
    ///
    /// `runner` is the runner name used as the writer field in messages.
    /// `extra` contains the pass-through CLI args for Zenoh-specific config.
    pub fn new(runner: &str, extra: &[String]) -> Result<Self> {
        let zenoh_args = ZenohArgs::parse(extra)?;
        Ok(Self {
            runner: runner.to_string(),
            zenoh_args,
            session: None,
            subscriber: None,
        })
    }
}

impl Variant for ZenohVariant {
    fn name(&self) -> &str {
        "zenoh"
    }

    fn connect(&mut self) -> Result<()> {
        let mut config = zenoh::Config::default();

        // Set the Zenoh mode.
        match self.zenoh_args.mode.as_str() {
            "peer" | "client" | "router" => {}
            other => anyhow::bail!("unsupported zenoh mode: {}", other),
        };
        config
            .insert_json5("mode", &format!("\"{}\"", self.zenoh_args.mode))
            .map_err(zenoh_err)?;

        // Set listen endpoints if provided.
        if let Some(ref listen) = self.zenoh_args.listen {
            config
                .insert_json5("listen/endpoints", &format!("[\"{}\"]", listen))
                .map_err(zenoh_err)?;
        }

        let session = zenoh::open(config).wait().map_err(zenoh_err)?;

        let subscriber = session
            .declare_subscriber("bench/**")
            .wait()
            .map_err(zenoh_err)?;

        self.session = Some(session);
        self.subscriber = Some(subscriber);

        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        // Build the key expression: strip leading slash from path if present.
        let key = if let Some(stripped) = path.strip_prefix('/') {
            format!("bench/{stripped}")
        } else {
            format!("bench/{path}")
        };

        let encoded = MessageCodec::encode(&self.runner, seq, qos, path, payload);

        session.put(&key, encoded).wait().map_err(zenoh_err)?;

        Ok(())
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        let subscriber = self
            .subscriber
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        // Subscriber<FifoChannelHandler<Sample>> derefs to FifoChannelHandler,
        // whose try_recv returns ZResult<Option<Sample>>.
        match subscriber.try_recv().map_err(zenoh_err)? {
            Some(sample) => {
                let data: Vec<u8> = sample.payload().to_bytes().to_vec();
                let update = MessageCodec::decode(&data)?;
                Ok(Some(update))
            }
            None => Ok(None),
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        // Drop subscriber first (undeclares it), then close session.
        if let Some(subscriber) = self.subscriber.take() {
            subscriber.undeclare().wait().map_err(zenoh_err)?;
        }
        if let Some(session) = self.session.take() {
            session.close().wait().map_err(zenoh_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_codec_roundtrip() {
        let writer = "runner-a";
        let seq = 42;
        let qos = Qos::BestEffort;
        let path = "/bench/0";
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];

        let encoded = MessageCodec::encode(writer, seq, qos, path, &payload);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.writer, writer);
        assert_eq!(decoded.seq, seq);
        assert_eq!(decoded.qos, qos);
        assert_eq!(decoded.path, path);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_message_codec_empty_payload() {
        let encoded = MessageCodec::encode("w", 0, Qos::ReliableTcp, "/p", &[]);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.writer, "w");
        assert_eq!(decoded.seq, 0);
        assert_eq!(decoded.qos, Qos::ReliableTcp);
        assert_eq!(decoded.path, "/p");
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_message_codec_large_seq() {
        let encoded = MessageCodec::encode("x", u64::MAX, Qos::LatestValue, "/a/b/c", &[0xFF]);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.seq, u64::MAX);
    }

    #[test]
    fn test_message_codec_decode_too_short() {
        assert!(MessageCodec::decode(&[]).is_err());
        assert!(MessageCodec::decode(&[0]).is_err());
    }

    #[test]
    fn test_zenoh_args_defaults() {
        let args = ZenohArgs::parse(&[]).unwrap();
        assert_eq!(args.mode, "peer");
        assert!(args.listen.is_none());
    }

    #[test]
    fn test_zenoh_args_mode_and_listen() {
        let extra = vec![
            "--zenoh-mode".to_string(),
            "client".to_string(),
            "--zenoh-listen".to_string(),
            "tcp/0.0.0.0:7447".to_string(),
        ];
        let args = ZenohArgs::parse(&extra).unwrap();
        assert_eq!(args.mode, "client");
        assert_eq!(args.listen.as_deref(), Some("tcp/0.0.0.0:7447"));
    }

    #[test]
    fn test_zenoh_args_unknown_arg() {
        let extra = vec!["--unknown".to_string()];
        assert!(ZenohArgs::parse(&extra).is_err());
    }

    #[test]
    fn test_zenoh_variant_name() {
        let v = ZenohVariant::new("a", &[]).unwrap();
        assert_eq!(v.name(), "zenoh");
    }
}
