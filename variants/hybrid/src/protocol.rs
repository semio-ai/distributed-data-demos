/// Compact binary message format shared between UDP and TCP transports.
///
/// Wire format:
/// ```text
/// [1 byte qos | 8 bytes seq (big-endian) | 2 bytes path_len (big-endian)
///  | N bytes path | 2 bytes writer_len (big-endian) | M bytes writer | payload bytes]
/// ```
use anyhow::{bail, Result};
use variant_base::types::{Qos, ReceivedUpdate};

/// Minimum header size: qos(1) + seq(8) + path_len(2) + writer_len(2) = 13 bytes.
const MIN_HEADER_SIZE: usize = 13;

/// Encode an update into the compact binary format.
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

/// Decode a compact binary message into a `ReceivedUpdate`.
pub fn decode(data: &[u8]) -> Result<ReceivedUpdate> {
    if data.len() < MIN_HEADER_SIZE {
        bail!(
            "message too short: {} bytes, need at least {}",
            data.len(),
            MIN_HEADER_SIZE
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
            "message truncated: need {} bytes for path, have {}",
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
            "message truncated: need {} bytes for writer, have {}",
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

/// Encode a message with a 4-byte length prefix for TCP framing.
///
/// TCP is a stream protocol, so we need length-prefix framing to know where
/// one message ends and the next begins.
pub fn encode_framed(qos: Qos, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Vec<u8> {
    let inner = encode(qos, seq, path, writer, payload);
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
}
