/// Binary message framing for the custom UDP protocol.
///
/// Wire format:
/// ```text
/// [4 bytes total_len] [1 byte qos] [8 bytes seq] [2 bytes path_len] [N bytes path]
/// [2 bytes writer_len] [M bytes writer] [payload bytes]
/// ```
///
/// All multi-byte integers are big-endian.
use anyhow::{bail, Result};
use variant_base::Qos;

/// Header overhead: total_len(4) + qos(1) + seq(8) + path_len(2) + writer_len(2) = 17 bytes.
const HEADER_FIXED_SIZE: usize = 4 + 1 + 8 + 2 + 2;

/// Maximum UDP payload size (standard Ethernet MTU minus IP + UDP headers).
#[cfg(test)]
pub const MAX_UDP_PAYLOAD: usize = 1472;

/// A parsed message from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub qos: Qos,
    pub seq: u64,
    pub path: String,
    pub writer: String,
    pub payload: Vec<u8>,
}

/// Encode a message into a byte buffer for transmission.
///
/// Returns the serialized bytes.
pub fn encode(qos: Qos, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let path_bytes = path.as_bytes();
    let writer_bytes = writer.as_bytes();

    if path_bytes.len() > u16::MAX as usize {
        bail!("path too long: {} bytes", path_bytes.len());
    }
    if writer_bytes.len() > u16::MAX as usize {
        bail!("writer too long: {} bytes", writer_bytes.len());
    }

    let total_len = HEADER_FIXED_SIZE + path_bytes.len() + writer_bytes.len() + payload.len();

    let mut buf = Vec::with_capacity(total_len);

    // total_len (4 bytes, big-endian)
    buf.extend_from_slice(&(total_len as u32).to_be_bytes());
    // qos (1 byte)
    buf.push(qos.as_int());
    // seq (8 bytes, big-endian)
    buf.extend_from_slice(&seq.to_be_bytes());
    // path_len (2 bytes, big-endian)
    buf.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
    // path (N bytes)
    buf.extend_from_slice(path_bytes);
    // writer_len (2 bytes, big-endian)
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    // writer (M bytes)
    buf.extend_from_slice(writer_bytes);
    // payload
    buf.extend_from_slice(payload);

    Ok(buf)
}

/// Decode a message from a byte buffer received from the network.
///
/// Returns the parsed `Message` or an error if the buffer is malformed.
pub fn decode(buf: &[u8]) -> Result<Message> {
    if buf.len() < HEADER_FIXED_SIZE {
        bail!(
            "buffer too short for header: {} < {}",
            buf.len(),
            HEADER_FIXED_SIZE
        );
    }

    let mut pos = 0;

    // total_len (4 bytes)
    let total_len =
        u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
    pos += 4;

    if buf.len() < total_len {
        bail!(
            "buffer shorter than declared total_len: {} < {}",
            buf.len(),
            total_len
        );
    }

    // qos (1 byte)
    let qos_byte = buf[pos];
    pos += 1;
    let qos =
        Qos::from_int(qos_byte).ok_or_else(|| anyhow::anyhow!("invalid QoS byte: {}", qos_byte))?;

    // seq (8 bytes)
    let seq = u64::from_be_bytes([
        buf[pos],
        buf[pos + 1],
        buf[pos + 2],
        buf[pos + 3],
        buf[pos + 4],
        buf[pos + 5],
        buf[pos + 6],
        buf[pos + 7],
    ]);
    pos += 8;

    // path_len (2 bytes)
    let path_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;

    if pos + path_len > total_len {
        bail!("path extends beyond total_len");
    }
    let path = std::str::from_utf8(&buf[pos..pos + path_len])?.to_string();
    pos += path_len;

    // writer_len (2 bytes)
    if pos + 2 > total_len {
        bail!("writer_len extends beyond total_len");
    }
    let writer_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;

    if pos + writer_len > total_len {
        bail!("writer extends beyond total_len");
    }
    let writer = std::str::from_utf8(&buf[pos..pos + writer_len])?.to_string();
    pos += writer_len;

    // payload = remaining bytes up to total_len
    let payload = buf[pos..total_len].to_vec();

    Ok(Message {
        qos,
        seq,
        path,
        writer,
        payload,
    })
}

/// Encode a NACK request for QoS 3.
///
/// Wire format: `[1 byte = 0xFF (NACK marker)] [2 bytes writer_len] [writer] [8 bytes missing_seq]`
pub fn encode_nack(writer: &str, missing_seq: u64) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let mut buf = Vec::with_capacity(1 + 2 + writer_bytes.len() + 8);
    buf.push(0xFF); // NACK marker
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(&missing_seq.to_be_bytes());
    buf
}

/// Check if a buffer is a NACK message (starts with 0xFF marker).
pub fn is_nack(buf: &[u8]) -> bool {
    !buf.is_empty() && buf[0] == 0xFF
}

/// Decode a NACK request.
///
/// Returns `(writer, missing_seq)`.
pub fn decode_nack(buf: &[u8]) -> Result<(String, u64)> {
    if buf.is_empty() || buf[0] != 0xFF {
        bail!("not a NACK message");
    }
    if buf.len() < 1 + 2 {
        bail!("NACK too short for writer_len");
    }
    let writer_len = u16::from_be_bytes([buf[1], buf[2]]) as usize;
    if buf.len() < 1 + 2 + writer_len + 8 {
        bail!("NACK too short");
    }
    let writer = std::str::from_utf8(&buf[3..3 + writer_len])?.to_string();
    let seq_start = 3 + writer_len;
    let missing_seq = u64::from_be_bytes([
        buf[seq_start],
        buf[seq_start + 1],
        buf[seq_start + 2],
        buf[seq_start + 3],
        buf[seq_start + 4],
        buf[seq_start + 5],
        buf[seq_start + 6],
        buf[seq_start + 7],
    ]);
    Ok((writer, missing_seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let encoded = encode(Qos::BestEffort, 42, "/bench/0", "runner-a", &payload).unwrap();
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded.qos, Qos::BestEffort);
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.path, "/bench/0");
        assert_eq!(decoded.writer, "runner-a");
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn roundtrip_empty_payload() {
        let encoded = encode(Qos::LatestValue, 0, "/x", "w", &[]).unwrap();
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded.qos, Qos::LatestValue);
        assert_eq!(decoded.seq, 0);
        assert_eq!(decoded.path, "/x");
        assert_eq!(decoded.writer, "w");
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn roundtrip_all_qos_levels() {
        for (qos, val) in [
            (Qos::BestEffort, 1),
            (Qos::LatestValue, 2),
            (Qos::ReliableUdp, 3),
            (Qos::ReliableTcp, 4),
        ] {
            let encoded = encode(qos, val as u64, "/p", "w", &[0xAB]).unwrap();
            let decoded = decode(&encoded).unwrap();
            assert_eq!(decoded.qos, qos);
            assert_eq!(decoded.seq, val as u64);
        }
    }

    #[test]
    fn decode_truncated_header() {
        let result = decode(&[0, 0, 0]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_invalid_qos() {
        let mut encoded = encode(Qos::BestEffort, 1, "/p", "w", &[]).unwrap();
        // Corrupt the qos byte (at offset 4).
        encoded[4] = 99;
        let result = decode(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn decode_total_len_mismatch() {
        let mut encoded = encode(Qos::BestEffort, 1, "/p", "w", &[1, 2]).unwrap();
        // Set total_len to something larger than the buffer.
        let fake_len = (encoded.len() as u32 + 100).to_be_bytes();
        encoded[0..4].copy_from_slice(&fake_len);
        let result = decode(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_large_path_and_writer() {
        let path = "/".to_string() + &"a".repeat(500);
        let writer = "w".repeat(300);
        let payload = vec![0xCD; 100];

        let encoded = encode(Qos::ReliableUdp, 999, &path, &writer, &payload).unwrap();
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded.path, path);
        assert_eq!(decoded.writer, writer);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn nack_roundtrip() {
        let encoded = encode_nack("runner-b", 42);
        assert!(is_nack(&encoded));

        let (writer, seq) = decode_nack(&encoded).unwrap();
        assert_eq!(writer, "runner-b");
        assert_eq!(seq, 42);
    }

    #[test]
    fn not_nack() {
        let encoded = encode(Qos::BestEffort, 1, "/p", "w", &[]).unwrap();
        assert!(!is_nack(&encoded));
    }

    #[test]
    fn nack_decode_too_short() {
        assert!(decode_nack(&[0xFF]).is_err());
        assert!(decode_nack(&[0xFF, 0, 1, b'w']).is_err());
    }

    #[test]
    fn message_size_scalar_flood() {
        // Typical scalar-flood: 8-byte payload, short path and writer.
        let encoded = encode(Qos::BestEffort, 1, "/bench/0", "runner-a", &[0u8; 8]).unwrap();
        assert!(
            encoded.len() < MAX_UDP_PAYLOAD,
            "scalar-flood message {} bytes should fit in one UDP datagram",
            encoded.len()
        );
    }
}
