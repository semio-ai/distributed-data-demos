//! Compact binary message format carried inside DataChannel payload bytes.
//!
//! Mirrors the wire format used by `variants/hybrid`, `variants/custom-udp`,
//! and `variants/websocket`. SCTP framing carries the message length, so we
//! do not add an outer length prefix.
//!
//! Two frame variants are carried:
//!
//! 1. **Data frames** -- normal `publish` payloads. Discriminant byte is the
//!    QoS level (1..=4).
//! 2. **EOT frames** -- end-of-test markers per the EOT protocol contract
//!    (`metak-shared/api-contracts/eot-protocol.md`). Discriminant byte is
//!    `EOT_TAG = 0xE0`.
//!
//! Data wire format:
//! ```text
//! [1 byte qos | 8 bytes seq (big-endian) | 2 bytes path_len (big-endian)
//!  | N bytes path | 2 bytes writer_len (big-endian) | M bytes writer | payload bytes]
//! ```
//!
//! EOT wire format:
//! ```text
//! [1 byte tag = 0xE0 | 8 bytes eot_id (big-endian)
//!  | 2 bytes writer_len (big-endian) | M bytes writer]
//! ```

use anyhow::{bail, Result};
use variant_base::types::{Qos, ReceivedUpdate};

/// Tag byte that identifies an EOT frame. Chosen outside the QoS range
/// (1..=4) so receivers reject unknown frames cleanly rather than
/// misinterpreting an EOT as a data message.
pub const EOT_TAG: u8 = 0xE0;

/// Minimum data-header size: qos(1) + seq(8) + path_len(2) + writer_len(2) = 13 bytes.
const MIN_DATA_HEADER_SIZE: usize = 13;

/// Minimum EOT-header size: tag(1) + eot_id(8) + writer_len(2) = 11 bytes.
const MIN_EOT_HEADER_SIZE: usize = 11;

/// A decoded frame, distinguishing data updates from end-of-test markers.
#[derive(Debug, Clone)]
pub enum Frame {
    Data(ReceivedUpdate),
    Eot { writer: String, eot_id: u64 },
}

/// Encode a data update into the compact binary format.
pub fn encode_data(qos: Qos, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encode_decode_data() {
        let qos = Qos::ReliableTcp;
        let seq = 42;
        let path = "/sensors/lidar";
        let writer = "runner-a";
        let payload = vec![1, 2, 3, 4, 5];

        let encoded = encode_data(qos, seq, path, writer, &payload);
        let frame = decode_frame(&encoded).unwrap();
        match frame {
            Frame::Data(decoded) => {
                assert_eq!(decoded.qos, qos);
                assert_eq!(decoded.seq, seq);
                assert_eq!(decoded.path, path);
                assert_eq!(decoded.writer, writer);
                assert_eq!(decoded.payload, payload);
            }
            other => panic!("expected Frame::Data, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_all_qos_levels() {
        for qos_int in 1..=4u8 {
            let qos = Qos::from_int(qos_int).unwrap();
            let encoded = encode_data(qos, 100, "/test", "w", &[0xFF]);
            let decoded = decode_frame(&encoded).unwrap();
            match decoded {
                Frame::Data(u) => assert_eq!(u.qos, qos),
                other => panic!("expected Frame::Data, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_payload() {
        let encoded = encode_data(Qos::ReliableTcp, 0, "/a", "b", &[]);
        let frame = decode_frame(&encoded).unwrap();
        match frame {
            Frame::Data(u) => {
                assert_eq!(u.payload, Vec::<u8>::new());
                assert_eq!(u.path, "/a");
                assert_eq!(u.writer, "b");
            }
            other => panic!("expected Frame::Data, got {other:?}"),
        }
    }

    #[test]
    fn decode_too_short() {
        assert!(decode_frame(&[1u8, 2, 3]).is_err());
    }

    #[test]
    fn decode_invalid_tag() {
        assert!(decode_frame(&[0x77u8, 0, 0, 0]).is_err());
    }

    #[test]
    fn decode_truncated_path() {
        let mut encoded = encode_data(Qos::ReliableTcp, 1, "/x", "w", &[]);
        encoded[9] = 0;
        encoded[10] = 255;
        assert!(decode_frame(&encoded).is_err());
    }

    #[test]
    fn large_sequence_number() {
        let seq = u64::MAX;
        let encoded = encode_data(Qos::ReliableUdp, seq, "/big", "runner", &[]);
        let frame = decode_frame(&encoded).unwrap();
        match frame {
            Frame::Data(u) => assert_eq!(u.seq, seq),
            other => panic!("expected Frame::Data, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_eot() {
        let writer = "runner-alpha";
        let eot_id: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let encoded = encode_eot(writer, eot_id);
        let frame = decode_frame(&encoded).expect("EOT frame must decode");
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
    fn decode_eot_too_short_errors() {
        let buf = vec![EOT_TAG, 0, 0, 0];
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn decode_eot_truncated_writer_errors() {
        let mut buf = encode_eot("hello", 1);
        buf[9] = 0;
        buf[10] = 99;
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn eot_tag_distinct_from_qos_range() {
        for q in 1u8..=4 {
            assert_ne!(EOT_TAG, q);
        }
    }
}
