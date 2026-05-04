/// Binary message framing for the custom UDP protocol.
///
/// Two frame types share the same length-prefixed wire layout. The byte
/// immediately after the 4-byte `total_len` (offset 4) is a tag that
/// distinguishes them:
///
/// - tag 1..=4 (a valid `Qos` value): a data frame.
///   ```text
///   [4 bytes total_len] [1 byte qos] [8 bytes seq] [2 bytes path_len] [N bytes path]
///   [2 bytes writer_len] [M bytes writer] [payload bytes]
///   ```
///
/// - tag `0xEE` ([`EOT_TAG`]): an end-of-test frame.
///   ```text
///   [4 bytes total_len] [1 byte tag=0xEE] [8 bytes eot_id] [2 bytes path_len=0]
///   [2 bytes writer_len] [N bytes writer]
///   ```
///   The `path_len=0` slot is reserved (always zero) to keep the layout
///   parallel with the data frame, so the same length-prefixed reader and
///   the same `HEADER_FIXED_SIZE` bounds check apply unchanged.
///
/// All multi-byte integers are big-endian.
use anyhow::{bail, Result};
use variant_base::Qos;

/// Tag byte (offset 4 within the framed message, i.e. immediately after the
/// 4-byte length prefix) marking an EOT frame. Chosen to be outside the
/// valid `Qos` range (1..=4), the NACK marker (0xFF), and typical leading
/// bytes of `total_len` so it cannot collide with a data frame's tag byte
/// or a NACK datagram's first byte on the UDP wire.
pub const EOT_TAG: u8 = 0xEE;

/// Header overhead: total_len(4) + qos(1) + seq(8) + path_len(2) + writer_len(2) = 17 bytes.
///
/// This is the minimum size of any valid frame on the wire — a data frame
/// with empty path, empty writer, and empty payload still occupies this
/// many bytes, and an EOT frame with empty writer also serializes to
/// exactly this size by design (see module docs). Length-prefixed framing
/// readers MUST validate that the declared `total_len` is at least
/// `HEADER_FIXED_SIZE` before allocating, otherwise garbage / torn-read
/// length prefixes can either underflow buffer arithmetic or produce
/// sub-header allocations that later panic on header slice access.
pub const HEADER_FIXED_SIZE: usize = 4 + 1 + 8 + 2 + 2;

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

/// A decoded end-of-test frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EotFrame {
    pub writer: String,
    pub eot_id: u64,
}

/// Top-level decoded frame: either a data message or an EOT marker.
///
/// Returned by [`decode_frame`]; lets the caller dispatch on transport
/// (UDP / TCP) regardless of which kind of frame just arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Message),
    Eot(EotFrame),
}

/// Encode an EOT frame into a byte buffer for transmission.
///
/// Output layout (see module docs):
/// `[total_len(4)] [tag=0xEE(1)] [eot_id(8)] [path_len=0(2)] [writer_len(2)] [writer]`.
///
/// The minimum serialized size (writer = "") is exactly `HEADER_FIXED_SIZE`,
/// so the frame can flow through `read_framed_message`'s bounds check
/// unchanged.
pub fn encode_eot(writer: &str, eot_id: u64) -> Result<Vec<u8>> {
    let writer_bytes = writer.as_bytes();
    if writer_bytes.len() > u16::MAX as usize {
        bail!("writer too long: {} bytes", writer_bytes.len());
    }
    // total_len(4) + tag(1) + eot_id(8) + path_len(2) + writer_len(2) + writer.
    let total_len = HEADER_FIXED_SIZE + writer_bytes.len();
    let mut buf = Vec::with_capacity(total_len);

    buf.extend_from_slice(&(total_len as u32).to_be_bytes());
    buf.push(EOT_TAG);
    buf.extend_from_slice(&eot_id.to_be_bytes());
    // path_len: reserved, always 0 for EOT frames.
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);

    Ok(buf)
}

/// Decode an EOT frame from a byte buffer.
///
/// Caller must have already established that `buf[4] == EOT_TAG`. Returns an
/// error on layout violations (truncated buffer, oversize writer_len,
/// non-zero reserved path_len, invalid UTF-8 writer).
pub fn decode_eot(buf: &[u8]) -> Result<EotFrame> {
    if buf.len() < HEADER_FIXED_SIZE {
        bail!(
            "EOT buffer too short: {} < {}",
            buf.len(),
            HEADER_FIXED_SIZE
        );
    }

    let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < total_len {
        bail!(
            "EOT buffer shorter than declared total_len: {} < {}",
            buf.len(),
            total_len
        );
    }
    if total_len < HEADER_FIXED_SIZE {
        bail!("EOT total_len {} below HEADER_FIXED_SIZE", total_len);
    }
    if buf[4] != EOT_TAG {
        bail!(
            "EOT tag mismatch: expected 0x{:02X}, got 0x{:02X}",
            EOT_TAG,
            buf[4]
        );
    }

    let mut pos = 5;
    let eot_id = u64::from_be_bytes([
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

    // path_len is reserved (always 0).
    let path_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;
    if path_len != 0 {
        bail!("EOT path_len must be 0, got {}", path_len);
    }

    let writer_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;
    if pos + writer_len > total_len {
        bail!("EOT writer extends beyond total_len");
    }
    let writer = std::str::from_utf8(&buf[pos..pos + writer_len])?.to_string();

    Ok(EotFrame { writer, eot_id })
}

/// Decode a length-prefixed frame buffer (such as returned by
/// `read_framed_message`) into either a data `Message` or an `EotFrame`.
///
/// Dispatches on the tag byte at offset 4: `0xEE` -> EOT, `1..=4` -> data.
pub fn decode_frame(buf: &[u8]) -> Result<Frame> {
    if buf.len() < 5 {
        bail!("frame too short for tag byte: {} < 5", buf.len());
    }
    if buf[4] == EOT_TAG {
        Ok(Frame::Eot(decode_eot(buf)?))
    } else {
        Ok(Frame::Data(decode(buf)?))
    }
}

/// Returns true if a raw UDP datagram body looks like an EOT frame.
///
/// EOT frames on the UDP wire begin with the 4-byte `total_len` prefix
/// followed by the [`EOT_TAG`] byte at offset 4, the same layout used on
/// TCP. The check requires at least 5 bytes so we can read the tag byte
/// safely.
pub fn is_eot_udp(buf: &[u8]) -> bool {
    buf.len() >= 5 && buf[4] == EOT_TAG
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

    #[test]
    fn eot_roundtrip() {
        let encoded = encode_eot("alice", 0xDEAD_BEEF_CAFE_BABE).unwrap();
        let decoded = decode_eot(&encoded).unwrap();
        assert_eq!(decoded.writer, "alice");
        assert_eq!(decoded.eot_id, 0xDEAD_BEEF_CAFE_BABE);
    }

    #[test]
    fn eot_roundtrip_random_id() {
        let id: u64 = 0x0123_4567_89AB_CDEF;
        let encoded = encode_eot("runner-b", id).unwrap();
        let decoded = decode_eot(&encoded).unwrap();
        assert_eq!(decoded.writer, "runner-b");
        assert_eq!(decoded.eot_id, id);
    }

    #[test]
    fn eot_min_size_meets_header_fixed_size() {
        // EOT frame with empty writer must still serialize to at least
        // HEADER_FIXED_SIZE bytes so it survives the framing bounds check.
        let encoded = encode_eot("", 0).unwrap();
        assert_eq!(
            encoded.len(),
            HEADER_FIXED_SIZE,
            "empty-writer EOT must land exactly at HEADER_FIXED_SIZE; got {}",
            encoded.len()
        );
    }

    #[test]
    fn eot_tag_is_at_offset_4() {
        let encoded = encode_eot("w", 7).unwrap();
        assert_eq!(encoded[4], EOT_TAG);
    }

    #[test]
    fn decode_frame_dispatches_eot() {
        let encoded = encode_eot("writer-x", 99).unwrap();
        match decode_frame(&encoded).unwrap() {
            Frame::Eot(eot) => {
                assert_eq!(eot.writer, "writer-x");
                assert_eq!(eot.eot_id, 99);
            }
            Frame::Data(_) => panic!("expected Frame::Eot, got Frame::Data"),
        }
    }

    #[test]
    fn decode_frame_dispatches_data() {
        let encoded = encode(Qos::BestEffort, 5, "/p", "writer-y", &[1, 2, 3]).unwrap();
        match decode_frame(&encoded).unwrap() {
            Frame::Data(msg) => {
                assert_eq!(msg.seq, 5);
                assert_eq!(msg.path, "/p");
                assert_eq!(msg.writer, "writer-y");
                assert_eq!(msg.payload, vec![1, 2, 3]);
            }
            Frame::Eot(_) => panic!("expected Frame::Data, got Frame::Eot"),
        }
    }

    #[test]
    fn is_eot_udp_distinguishes_data_and_eot() {
        let data = encode(Qos::BestEffort, 1, "/p", "w", &[0]).unwrap();
        let eot = encode_eot("w", 1).unwrap();
        assert!(!is_eot_udp(&data));
        assert!(is_eot_udp(&eot));
    }

    #[test]
    fn is_eot_udp_handles_short_buffers() {
        // Shorter than the tag byte's offset: must not panic.
        assert!(!is_eot_udp(&[]));
        assert!(!is_eot_udp(&[0xEE]));
        assert!(!is_eot_udp(&[0, 0, 0, 0]));
    }

    #[test]
    fn decode_eot_rejects_truncated() {
        let mut encoded = encode_eot("xyz", 1).unwrap();
        encoded.truncate(HEADER_FIXED_SIZE - 1);
        assert!(decode_eot(&encoded).is_err());
    }

    #[test]
    fn decode_eot_rejects_wrong_tag() {
        let mut encoded = encode_eot("a", 1).unwrap();
        encoded[4] = 1; // pretend it's a data frame
        assert!(decode_eot(&encoded).is_err());
    }

    #[test]
    fn decode_eot_rejects_nonzero_path_len() {
        let mut encoded = encode_eot("a", 1).unwrap();
        // path_len lives at offset 4 + 1 + 8 = 13..15.
        encoded[13] = 0;
        encoded[14] = 1;
        assert!(decode_eot(&encoded).is_err());
    }

    #[test]
    fn eot_frame_starts_with_total_len_prefix() {
        // Sanity: the first 4 bytes are the BE u32 total_len, identical to
        // data frames. This is what `read_framed_message` consumes.
        let encoded = encode_eot("hello", 1).unwrap();
        let total_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(total_len as usize, encoded.len());
    }
}
