//! Per-peer-pair TCP signaling channel.
//!
//! The runner does not carry SDP, so each WebRTC peer pair brings up a
//! small TCP signaling socket on the derived signaling port. Frames are
//! length-prefixed JSON envelopes with a tagged `kind`:
//!
//!   {"kind":"offer","sdp":"..."}
//!   {"kind":"answer","sdp":"..."}
//!   {"kind":"candidate","candidate":"...","sdp_mid":"...","sdp_mline_index":0}
//!   {"kind":"done"}
//!
//! The frame on the wire is a 4-byte big-endian length, then that many
//! bytes of UTF-8 JSON. The framing is internal to this module; webrtc-rs
//! does not see it.
//!
//! Roles:
//! - Lower-sorted runner is the **initiator**: opens the TCP connection
//!   and sends the SDP offer.
//! - Higher-sorted runner is the **responder**: binds + accepts the
//!   connection and replies with the SDP answer.
//!
//! ICE candidates trickle in both directions for the lifetime of the
//! socket. Either side closes once it observes all four DataChannels
//! `open` -- closing the read side returns `Ok(None)` from `read_frame`
//! and the peer drains.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// JSON envelope sent on the signaling socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SignalEnvelope {
    /// SDP offer (initiator -> responder).
    Offer { sdp: String },
    /// SDP answer (responder -> initiator).
    Answer { sdp: String },
    /// Trickle ICE candidate (either direction).
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: Option<u16>,
    },
    /// Sender will not send any more envelopes.
    Done,
}

/// Encode an envelope to its 4-byte-length-prefixed JSON wire form.
pub fn encode(env: &SignalEnvelope) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(env).context("serialize signal envelope")?;
    let len = u32::try_from(json.len()).context("envelope too large")?;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Decode an envelope from a length-prefixed JSON byte slice. The length
/// prefix is NOT included in `body` (this helper expects only the JSON
/// payload, not the framed wire form).
pub fn decode_payload(body: &[u8]) -> Result<SignalEnvelope> {
    serde_json::from_slice::<SignalEnvelope>(body).context("deserialize signal envelope")
}

/// Read a single length-prefixed JSON envelope from `stream`. Returns
/// `Ok(None)` if the stream cleanly closed before any bytes were read,
/// or if the peer half-closed before this envelope's body was fully
/// available.
pub async fn read_frame(stream: &mut TcpStream) -> Result<Option<SignalEnvelope>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(anyhow!("read length prefix: {e}")),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        bail!("zero-length signaling frame");
    }
    if len > 1 << 20 {
        bail!("oversized signaling frame: {len} bytes");
    }
    let mut body = vec![0u8; len];
    match stream.read_exact(&mut body).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(anyhow!("read frame body: {e}")),
    }
    Ok(Some(decode_payload(&body)?))
}

/// Write a single length-prefixed JSON envelope to `stream`.
pub async fn write_frame(stream: &mut TcpStream, env: &SignalEnvelope) -> Result<()> {
    let buf = encode(env)?;
    stream.write_all(&buf).await.context("write frame")?;
    stream.flush().await.context("flush frame")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_offer() {
        let env = SignalEnvelope::Offer {
            sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".to_string(),
        };
        let bytes = encode(&env).unwrap();
        // First 4 bytes are the length prefix.
        assert_eq!(&bytes[..4], &(bytes.len() as u32 - 4).to_be_bytes());
        let decoded = decode_payload(&bytes[4..]).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_answer() {
        let env = SignalEnvelope::Answer {
            sdp: "answer-sdp".to_string(),
        };
        let bytes = encode(&env).unwrap();
        let decoded = decode_payload(&bytes[4..]).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_candidate() {
        let env = SignalEnvelope::Candidate {
            candidate: "candidate:1 1 udp 2113937151 192.0.2.1 49153 typ host".into(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
        };
        let bytes = encode(&env).unwrap();
        let decoded = decode_payload(&bytes[4..]).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_done() {
        let env = SignalEnvelope::Done;
        let bytes = encode(&env).unwrap();
        let decoded = decode_payload(&bytes[4..]).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn decode_unknown_kind_errors() {
        let body = br#"{"kind":"bogus"}"#;
        assert!(decode_payload(body).is_err());
    }

    #[test]
    fn decode_invalid_json_errors() {
        let body = b"not json";
        assert!(decode_payload(body).is_err());
    }

    #[test]
    fn encode_kind_matches_lowercase() {
        // Verify the envelope `kind` field uses lowercase tags. The
        // signaling contract documents the exact strings; we want to
        // notice if a future serde rename changes them.
        let bytes = encode(&SignalEnvelope::Offer { sdp: String::new() }).unwrap();
        let body = &bytes[4..];
        let s = std::str::from_utf8(body).unwrap();
        assert!(s.contains(r#""kind":"offer""#), "got: {s}");

        let bytes = encode(&SignalEnvelope::Done).unwrap();
        let body = &bytes[4..];
        let s = std::str::from_utf8(body).unwrap();
        assert!(s.contains(r#""kind":"done""#), "got: {s}");
    }
}
