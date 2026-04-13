use std::collections::VecDeque;
use std::ffi::CString;
use std::time::Duration;

use anyhow::{Context, Result};
use rusteron_client::*;

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::Variant;

/// Configuration for the Aeron transport variant.
pub struct AeronConfig {
    /// Path to the Aeron media driver directory.
    pub aeron_dir: Option<String>,
    /// Aeron channel URI (e.g. `aeron:udp?endpoint=239.0.0.1:40456`).
    pub channel: String,
    /// Aeron stream ID.
    pub stream_id: i32,
    /// This runner's name, used as the writer field in published messages.
    pub runner: String,
}

/// Aeron-based variant using rusteron-client for ultra-low-latency messaging.
///
/// Connects to an Aeron media driver via shared memory, creates a Publication
/// and Subscription on the configured channel/stream, and implements the
/// Variant trait for the benchmark protocol.
pub struct AeronVariant {
    config: AeronConfig,
    aeron: Option<Aeron>,
    publication: Option<AeronPublication>,
    subscription: Option<AeronSubscription>,
    receive_queue: VecDeque<ReceivedUpdate>,
    // The fragment handler and inner handler must be kept alive for the
    // duration of the subscription polling.
    #[allow(dead_code)]
    fragment_handler: Option<Handler<AeronFragmentAssembler>>,
    #[allow(dead_code)]
    fragment_inner: Option<Handler<FragmentReceiver>>,
}

/// Fragment handler that buffers received messages into a shared queue.
///
/// Because the fragment handler callback receives `&mut self`, we store
/// received messages in an internal Vec that gets drained into the main
/// VecDeque after each poll cycle.
pub struct FragmentReceiver {
    /// Staging buffer for fragments received during a single poll call.
    pub staged: Vec<ReceivedUpdate>,
}

impl AeronFragmentHandlerCallback for FragmentReceiver {
    fn handle_aeron_fragment_handler(&mut self, buffer: &[u8], _header: AeronHeader) {
        // Deserialize the message from the buffer.
        // Format: writer_len(u16) | writer(bytes) | seq(u64) | qos(u8)
        //         | path_len(u16) | path(bytes) | payload(remaining)
        if let Some(update) = deserialize_message(buffer) {
            self.staged.push(update);
        }
    }
}

impl AeronVariant {
    /// Create a new Aeron variant with the given configuration.
    pub fn new(config: AeronConfig) -> Self {
        Self {
            config,
            aeron: None,
            publication: None,
            subscription: None,
            receive_queue: VecDeque::new(),
            fragment_handler: None,
            fragment_inner: None,
        }
    }
}

impl Variant for AeronVariant {
    fn name(&self) -> &str {
        "aeron"
    }

    fn connect(&mut self) -> Result<()> {
        let ctx = AeronContext::new().context("failed to create Aeron context")?;

        if let Some(ref dir) = self.config.aeron_dir {
            let c_dir =
                CString::new(dir.as_str()).context("invalid aeron-dir: contains null byte")?;
            ctx.set_dir(&c_dir)
                .context("failed to set Aeron directory")?;
        }

        let aeron = Aeron::new(&ctx).context("failed to create Aeron client")?;
        aeron.start().context("failed to start Aeron client")?;

        let channel = CString::new(self.config.channel.as_str())
            .context("invalid channel: contains null byte")?;
        let timeout = Duration::from_secs(5);

        // Create publication (async then poll-blocking).
        let publication = aeron
            .async_add_publication(&channel, self.config.stream_id)
            .context("failed to add publication")?
            .poll_blocking(timeout)
            .context("publication not ready within timeout")?;

        // Create subscription (async then poll-blocking).
        let subscription = aeron
            .async_add_subscription(
                &channel,
                self.config.stream_id,
                Handlers::no_available_image_handler(),
                Handlers::no_unavailable_image_handler(),
            )
            .context("failed to add subscription")?
            .poll_blocking(timeout)
            .context("subscription not ready within timeout")?;

        // Create the fragment handler with assembler support for large messages.
        let receiver = FragmentReceiver { staged: Vec::new() };
        let (assembler_handler, inner_handler) = Handler::leak_with_fragment_assembler(receiver)
            .context("failed to create fragment assembler")?;

        self.publication = Some(publication);
        self.subscription = Some(subscription);
        self.fragment_handler = Some(assembler_handler);
        self.fragment_inner = Some(inner_handler);
        self.aeron = Some(aeron);

        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let publication = self
            .publication
            .as_ref()
            .context("not connected: no publication")?;

        let buffer = serialize_message(&self.config.runner, seq, qos, path, payload);

        // Retry loop for back-pressure. The Aeron offer() returns a positive
        // stream position on success, or a negative value on error.
        let max_retries = 100;
        for attempt in 0..max_retries {
            let result = publication.offer(&buffer, Handlers::no_reserved_value_supplier_handler());

            if result >= 0 {
                return Ok(());
            }

            // Negative result indicates an error condition.
            let error = AeronCError::from_code(result as i32);
            match error.kind() {
                AeronErrorType::PublicationBackPressured
                | AeronErrorType::PublicationAdminAction => {
                    // Transient conditions: spin briefly and retry.
                    if attempt < max_retries - 1 {
                        std::hint::spin_loop();
                    }
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Aeron offer failed: {:?} (result={})",
                        error.kind(),
                        result
                    ));
                }
            }
        }

        Err(anyhow::anyhow!(
            "Aeron offer failed after {} retries due to back-pressure",
            max_retries
        ))
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // Return from the queue first.
        if let Some(update) = self.receive_queue.pop_front() {
            return Ok(Some(update));
        }

        // Poll the subscription for new fragments.
        let subscription = match self.subscription.as_ref() {
            Some(s) => s,
            None => return Ok(None),
        };
        let handler = match self.fragment_handler.as_ref() {
            Some(h) => h,
            None => return Ok(None),
        };

        let _fragments = subscription
            .poll(Some(handler), 256)
            .context("Aeron subscription poll failed")?;

        // Drain staged fragments from the receiver into our queue.
        if let Some(ref inner) = self.fragment_inner {
            // Safety: we need mutable access to drain staged messages.
            // The Handler wraps a leaked pointer; we access it via get_inner_mut.
            let receiver = unsafe { &mut *(inner.as_raw() as *mut FragmentReceiver) };
            for update in receiver.staged.drain(..) {
                self.receive_queue.push_back(update);
            }
        }

        Ok(self.receive_queue.pop_front())
    }

    fn disconnect(&mut self) -> Result<()> {
        // Drop resources in reverse order of creation.
        self.fragment_handler = None;
        self.fragment_inner = None;
        self.subscription = None;
        self.publication = None;

        if let Some(aeron) = self.aeron.take() {
            aeron.close().context("failed to close Aeron client")?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Message serialization
// ---------------------------------------------------------------------------
//
// Wire format (compact binary, Aeron handles framing):
//
//   writer_len : u16  (little-endian)
//   writer     : [u8; writer_len]
//   seq        : u64  (little-endian)
//   qos        : u8
//   path_len   : u16  (little-endian)
//   path       : [u8; path_len]
//   payload    : [u8; remaining]

/// Serialize a message into the wire format.
pub fn serialize_message(writer: &str, seq: u64, qos: Qos, path: &str, payload: &[u8]) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let path_bytes = path.as_bytes();

    // 2 + writer_len + 8 + 1 + 2 + path_len + payload_len
    let capacity = 2 + writer_bytes.len() + 8 + 1 + 2 + path_bytes.len() + payload.len();
    let mut buf = Vec::with_capacity(capacity);

    buf.extend_from_slice(&(writer_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.push(qos.as_int());
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(payload);

    buf
}

/// Deserialize a message from the wire format. Returns `None` on malformed input.
pub fn deserialize_message(data: &[u8]) -> Option<ReceivedUpdate> {
    let mut pos = 0;

    if data.len() < 2 {
        return None;
    }
    let writer_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;

    if data.len() < pos + writer_len {
        return None;
    }
    let writer = std::str::from_utf8(&data[pos..pos + writer_len]).ok()?;
    pos += writer_len;

    // seq (8) + qos (1) + path_len (2) = 11 bytes minimum remaining
    if data.len() < pos + 11 {
        return None;
    }

    let seq = u64::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]);
    pos += 8;

    let qos = Qos::from_int(data[pos])?;
    pos += 1;

    let path_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;

    if data.len() < pos + path_len {
        return None;
    }
    let path = std::str::from_utf8(&data[pos..pos + path_len]).ok()?;
    pos += path_len;

    let payload = data[pos..].to_vec();

    Some(ReceivedUpdate {
        writer: writer.to_string(),
        seq,
        path: path.to_string(),
        qos,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let writer = "runner-a";
        let seq = 42u64;
        let qos = Qos::BestEffort;
        let path = "/sensors/lidar";
        let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8];

        let buf = serialize_message(writer, seq, qos, path, &payload);
        let update = deserialize_message(&buf).expect("deserialization should succeed");

        assert_eq!(update.writer, writer);
        assert_eq!(update.seq, seq);
        assert_eq!(update.qos, qos);
        assert_eq!(update.path, path);
        assert_eq!(update.payload, payload);
    }

    #[test]
    fn test_serialize_deserialize_empty_payload() {
        let buf = serialize_message("b", 1, Qos::ReliableTcp, "/a", &[]);
        let update = deserialize_message(&buf).expect("deserialization should succeed");

        assert_eq!(update.writer, "b");
        assert_eq!(update.seq, 1);
        assert_eq!(update.qos, Qos::ReliableTcp);
        assert_eq!(update.path, "/a");
        assert!(update.payload.is_empty());
    }

    #[test]
    fn test_deserialize_truncated_returns_none() {
        // Too short for even the writer_len field.
        assert!(deserialize_message(&[]).is_none());
        assert!(deserialize_message(&[0]).is_none());

        // Writer length says 5 but only 2 bytes follow.
        assert!(deserialize_message(&[5, 0, b'a', b'b']).is_none());
    }

    #[test]
    fn test_deserialize_invalid_qos_returns_none() {
        let mut buf = serialize_message("a", 1, Qos::BestEffort, "/x", &[0]);
        // Find the qos byte position: 2 + 1 (writer "a") + 8 (seq) = 11
        buf[11] = 99; // invalid QoS value
        assert!(deserialize_message(&buf).is_none());
    }

    #[test]
    fn test_all_qos_levels_roundtrip() {
        for qos_int in 1..=4u8 {
            let qos = Qos::from_int(qos_int).unwrap();
            let buf = serialize_message("w", 100, qos, "/p", &[42]);
            let update = deserialize_message(&buf).unwrap();
            assert_eq!(update.qos, qos);
        }
    }

    #[test]
    fn test_variant_name() {
        let v = AeronVariant::new(AeronConfig {
            aeron_dir: None,
            channel: "aeron:udp?endpoint=239.0.0.1:40456".to_string(),
            stream_id: 1001,
            runner: "test".to_string(),
        });
        assert_eq!(v.name(), "aeron");
    }
}
