/// Compact binary message format shared between UDP and TCP transports.
///
/// Two frame variants are carried over the wire:
///
/// 1. **Data frames** -- normal `publish` payloads.
/// 2. **EOT frames** -- end-of-test markers, per the EOT protocol contract
///    (`metak-shared/api-contracts/eot-protocol.md`).
///
/// Discrimination is via the leading byte:
/// - bytes 1..=4 -- data frame; the byte is the QoS level and the remaining
///   layout matches the original data wire format described below.
/// - byte 0xE0 -- EOT frame; followed by an 8-byte big-endian `eot_id` and a
///   2-byte big-endian `writer_len` plus the UTF-8 writer name.
///
/// Data wire format:
/// ```text
/// [1 byte qos | 8 bytes seq (big-endian) | 2 bytes path_len (big-endian)
///  | N bytes path | 2 bytes writer_len (big-endian) | M bytes writer | payload bytes]
/// ```
///
/// EOT wire format:
/// ```text
/// [1 byte tag = 0xE0 | 8 bytes eot_id (big-endian)
///  | 2 bytes writer_len (big-endian) | M bytes writer]
/// ```
///
/// On the TCP path both frame kinds share the same length-prefix framing
/// (`encode_framed` / `encode_eot_framed` add a 4-byte big-endian length
/// prefix). On the UDP path the frame is the entire datagram payload (no
/// length prefix needed).
use anyhow::{bail, Result};
use variant_base::types::{Qos, ReceivedUpdate};

/// Tag byte that identifies an EOT frame. Chosen outside the QoS range
/// (1..=4) and outside the practical range of the existing `Qos::from_int`
/// space so older receivers reject unknown frames cleanly rather than
/// misinterpreting an EOT as a data message.
pub const EOT_TAG: u8 = 0xE0;

/// Minimum data-header size: qos(1) + seq(8) + path_len(2) + writer_len(2) = 13 bytes.
const MIN_DATA_HEADER_SIZE: usize = 13;

/// Minimum EOT-header size: tag(1) + eot_id(8) + writer_len(2) = 11 bytes.
const MIN_EOT_HEADER_SIZE: usize = 11;

/// A decoded frame, distinguishing data updates from end-of-test markers.
///
/// `PartialEq` / `Eq` are not derived because `ReceivedUpdate` (defined in
/// `variant-base`) does not implement them; the variant tests destructure
/// the variant manually instead.
#[derive(Debug, Clone)]
pub enum Frame {
    /// A normal replicated data update.
    Data(ReceivedUpdate),
    /// An end-of-test marker from a peer.
    Eot { writer: String, eot_id: u64 },
}

/// Encode a data update into the compact binary format.
pub fn encode(qos: Qos, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let writer_bytes = writer.as_bytes();
    let total = 1 + 8 + 2 + path_bytes.len() + 2 + writer_bytes.len() + payload.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(qos.as_int());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(payload);

    buf
}

/// Encode an end-of-test marker into the compact binary format.
///
/// Used for both the TCP path (wrapped in `encode_eot_framed`) and the UDP
/// path (sent as a standalone datagram).
pub fn encode_eot(writer: &str, eot_id: u64) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let total = 1 + 8 + 2 + writer_bytes.len();
    let mut buf = Vec::with_capacity(total);
    buf.push(EOT_TAG);
    buf.extend_from_slice(&eot_id.to_be_bytes());
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf
}

/// Decode a single frame (data or EOT) from a wire-format buffer.
pub fn decode_frame(data: &[u8]) -> Result<Frame> {
    if data.is_empty() {
        bail!("empty frame");
    }
    match data[0] {
        EOT_TAG => decode_eot_frame(data),
        1..=4 => decode_data_frame(data).map(Frame::Data),
        other => bail!("invalid frame tag byte: {} (expected 1..=4 or 0xE0)", other),
    }
}

/// Decode a data frame.
///
/// Kept as a thin wrapper around `decode_data_frame` so existing tests
/// that expect "decode -> ReceivedUpdate" still work without going
/// through `decode_frame`. Used in tests; production code on the
/// receive path uses `decode_frame` to dispatch data vs EOT.
#[allow(dead_code)]
pub fn decode(data: &[u8]) -> Result<ReceivedUpdate> {
    decode_data_frame(data)
}

fn decode_data_frame(data: &[u8]) -> Result<ReceivedUpdate> {
    if data.len() < MIN_DATA_HEADER_SIZE {
        bail!(
            "data frame too short: {} bytes, need at least {}",
            data.len(),
            MIN_DATA_HEADER_SIZE
        );
    }

    let qos_byte = data[0];
    let qos =
        Qos::from_int(qos_byte).ok_or_else(|| anyhow::anyhow!("invalid QoS byte: {}", qos_byte))?;

    let seq = u64::from_be_bytes(data[1..9].try_into().unwrap());

    let path_len = u16::from_be_bytes(data[9..11].try_into().unwrap()) as usize;
    let path_end = 11 + path_len;
    if data.len() < path_end + 2 {
        bail!(
            "data frame truncated: need {} bytes for path, have {}",
            path_end + 2,
            data.len()
        );
    }
    let path = std::str::from_utf8(&data[11..path_end])
        .map_err(|e| anyhow::anyhow!("invalid path UTF-8: {}", e))?
        .to_string();

    let writer_len = u16::from_be_bytes(data[path_end..path_end + 2].try_into().unwrap()) as usize;
    let writer_end = path_end + 2 + writer_len;
    if data.len() < writer_end {
        bail!(
            "data frame truncated: need {} bytes for writer, have {}",
            writer_end,
            data.len()
        );
    }
    let writer = std::str::from_utf8(&data[path_end + 2..writer_end])
        .map_err(|e| anyhow::anyhow!("invalid writer UTF-8: {}", e))?
        .to_string();

    let payload = data[writer_end..].to_vec();

    Ok(ReceivedUpdate {
        writer,
        seq,
        path,
        qos,
        payload,
    })
}

fn decode_eot_frame(data: &[u8]) -> Result<Frame> {
    if data.len() < MIN_EOT_HEADER_SIZE {
        bail!(
            "EOT frame too short: {} bytes, need at least {}",
            data.len(),
            MIN_EOT_HEADER_SIZE
        );
    }
    debug_assert_eq!(data[0], EOT_TAG);

    let eot_id = u64::from_be_bytes(data[1..9].try_into().unwrap());
    let writer_len = u16::from_be_bytes(data[9..11].try_into().unwrap()) as usize;
    let writer_end = 11 + writer_len;
    if data.len() < writer_end {
        bail!(
            "EOT frame truncated: need {} bytes for writer, have {}",
            writer_end,
            data.len()
        );
    }
    let writer = std::str::from_utf8(&data[11..writer_end])
        .map_err(|e| anyhow::anyhow!("invalid writer UTF-8 in EOT frame: {}", e))?
        .to_string();
    Ok(Frame::Eot { writer, eot_id })
}

/// Encode a data frame with a 4-byte length prefix for TCP framing.
///
/// TCP is a stream protocol, so we need length-prefix framing to know where
/// one message ends and the next begins.
pub fn encode_framed(qos: Qos, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Vec<u8> {
    let inner = encode(qos, seq, path, writer, payload);
    prepend_len_prefix(inner)
}

/// Encode an EOT frame with a 4-byte length prefix for TCP framing.
pub fn encode_eot_framed(writer: &str, eot_id: u64) -> Vec<u8> {
    let inner = encode_eot(writer, eot_id);
    prepend_len_prefix(inner)
}

fn prepend_len_prefix(inner: Vec<u8>) -> Vec<u8> {
    let len = inner.len() as u32;
    let mut buf = Vec::with_capacity(4 + inner.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&inner);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encode_decode() {
        let qos = Qos::BestEffort;
        let seq = 42;
        let path = "/sensors/lidar";
        let writer = "runner-a";
        let payload = vec![1, 2, 3, 4, 5];

        let encoded = encode(qos, seq, path, writer, &payload);
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded.qos, qos);
        assert_eq!(decoded.seq, seq);
        assert_eq!(decoded.path, path);
        assert_eq!(decoded.writer, writer);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn roundtrip_all_qos_levels() {
        for qos_int in 1..=4u8 {
            let qos = Qos::from_int(qos_int).unwrap();
            let encoded = encode(qos, 100, "/test", "w", &[0xFF]);
            let decoded = decode(&encoded).unwrap();
            assert_eq!(decoded.qos, qos);
        }
    }

    #[test]
    fn empty_payload() {
        let encoded = encode(Qos::ReliableTcp, 0, "/a", "b", &[]);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.payload, Vec::<u8>::new());
        assert_eq!(decoded.path, "/a");
        assert_eq!(decoded.writer, "b");
    }

    #[test]
    fn decode_too_short() {
        let data = vec![1, 2, 3];
        assert!(decode(&data).is_err());
    }

    #[test]
    fn decode_invalid_qos() {
        let mut encoded = encode(Qos::BestEffort, 1, "/x", "w", &[]);
        encoded[0] = 99; // invalid QoS
        assert!(decode(&encoded).is_err());
    }

    #[test]
    fn decode_truncated_path() {
        let mut encoded = encode(Qos::BestEffort, 1, "/x", "w", &[]);
        // Set path_len to a value larger than remaining data
        encoded[9] = 0;
        encoded[10] = 255;
        assert!(decode(&encoded).is_err());
    }

    #[test]
    fn framed_includes_length_prefix() {
        let framed = encode_framed(Qos::BestEffort, 1, "/p", "w", &[10, 20]);
        let len = u32::from_be_bytes(framed[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, framed.len() - 4);
        // The rest should be decodable
        let decoded = decode(&framed[4..]).unwrap();
        assert_eq!(decoded.seq, 1);
        assert_eq!(decoded.payload, vec![10, 20]);
    }

    #[test]
    fn large_sequence_number() {
        let seq = u64::MAX;
        let encoded = encode(Qos::LatestValue, seq, "/big", "runner", &[]);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.seq, seq);
    }

    #[test]
    fn roundtrip_eot_datagram() {
        let writer = "runner-alpha";
        let eot_id: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let encoded = encode_eot(writer, eot_id);
        let frame = decode_frame(&encoded).expect("EOT datagram must decode");
        match frame {
            Frame::Eot {
                writer: w,
                eot_id: id,
            } => {
                assert_eq!(w, writer);
                assert_eq!(id, eot_id);
            }
            other => panic!("expected Frame::Eot, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_eot_framed() {
        let writer = "runner-bob";
        let eot_id: u64 = 1;
        let framed = encode_eot_framed(writer, eot_id);
        let len = u32::from_be_bytes(framed[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, framed.len() - 4);
        match decode_frame(&framed[4..]).expect("framed EOT must decode") {
            Frame::Eot {
                writer: w,
                eot_id: id,
            } => {
                assert_eq!(w, writer);
                assert_eq!(id, eot_id);
            }
            other => panic!("expected Frame::Eot, got {other:?}"),
        }
    }

    #[test]
    fn decode_frame_dispatches_data_vs_eot() {
        // Data frame.
        let data_buf = encode(Qos::ReliableTcp, 7, "/p", "w", &[1, 2]);
        match decode_frame(&data_buf).unwrap() {
            Frame::Data(u) => {
                assert_eq!(u.qos, Qos::ReliableTcp);
                assert_eq!(u.seq, 7);
                assert_eq!(u.payload, vec![1u8, 2]);
            }
            other => panic!("expected Frame::Data, got {other:?}"),
        }

        // EOT frame.
        let eot_buf = encode_eot("w", 42);
        match decode_frame(&eot_buf).unwrap() {
            Frame::Eot { writer, eot_id } => {
                assert_eq!(writer, "w");
                assert_eq!(eot_id, 42);
            }
            other => panic!("expected Frame::Eot, got {other:?}"),
        }
    }

    #[test]
    fn decode_eot_too_short_errors() {
        // Just the tag and a partial id.
        let buf = vec![EOT_TAG, 0, 0, 0];
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn decode_eot_truncated_writer_errors() {
        let mut buf = encode_eot("hello", 1);
        // Bump writer_len so the buffer is too short.
        buf[9] = 0;
        buf[10] = 99;
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn decode_frame_unknown_tag_errors() {
        let buf = vec![0x77u8, 0, 0, 0];
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn eot_tag_distinct_from_qos_range() {
        // Sanity guard: the EOT tag must not collide with any valid QoS
        // byte (1..=4). If someone ever adds Qos::5, this guard fires.
        for q in 1u8..=4 {
            assert_ne!(EOT_TAG, q);
        }
    }
}
