use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::variant_trait::Variant;

use crate::certs::generate_self_signed_cert;

/// Message header prepended to all QUIC payloads (both datagrams and streams).
///
/// Layout (big-endian):
///   - writer_len: u16
///   - writer: [u8; writer_len]
///   - path_len: u16
///   - path: [u8; path_len]
///   - qos: u8
///   - seq: u64
///   - payload: remaining bytes
const HEADER_OVERHEAD: usize = 2 + 2 + 1 + 8; // fixed portion without variable-length strings

fn encode_message(writer: &str, path: &str, qos: Qos, seq: u64, payload: &[u8]) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let path_bytes = path.as_bytes();
    let total = HEADER_OVERHEAD + writer_bytes.len() + path_bytes.len() + payload.len();
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(path_bytes);
    buf.push(qos.as_int());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(payload);

    buf
}

fn decode_message(data: &[u8]) -> Result<ReceivedUpdate> {
    let mut offset = 0;

    if data.len() < 2 {
        anyhow::bail!("message too short for writer_len");
    }
    let writer_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;

    if data.len() < offset + writer_len {
        anyhow::bail!("message too short for writer");
    }
    let writer = std::str::from_utf8(&data[offset..offset + writer_len])
        .context("invalid writer UTF-8")?
        .to_string();
    offset += writer_len;

    if data.len() < offset + 2 {
        anyhow::bail!("message too short for path_len");
    }
    let path_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;

    if data.len() < offset + path_len {
        anyhow::bail!("message too short for path");
    }
    let path = std::str::from_utf8(&data[offset..offset + path_len])
        .context("invalid path UTF-8")?
        .to_string();
    offset += path_len;

    if data.len() < offset + 1 {
        anyhow::bail!("message too short for qos");
    }
    let qos = Qos::from_int(data[offset]).context("invalid QoS value")?;
    offset += 1;

    if data.len() < offset + 8 {
        anyhow::bail!("message too short for seq");
    }
    let seq = u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]);
    offset += 8;

    let payload = data[offset..].to_vec();

    Ok(ReceivedUpdate {
        writer,
        seq,
        path,
        qos,
        payload,
    })
}

/// Outbound message to be sent by the background send task.
struct OutboundMessage {
    data: Vec<u8>,
    /// True for reliable (QoS 3-4, use streams), false for best-effort (QoS 1-2, use datagrams).
    reliable: bool,
}

/// Shutdown signal for background tasks.
type ShutdownTx = tokio::sync::watch::Sender<bool>;
type ShutdownRx = tokio::sync::watch::Receiver<bool>;

/// QUIC variant implementing the Variant trait using quinn.
pub struct QuicVariant {
    runner: String,
    bind_addr: SocketAddr,
    peers: Vec<SocketAddr>,
    runtime: Option<Runtime>,
    send_tx: Option<mpsc::UnboundedSender<OutboundMessage>>,
    recv_rx: Option<mpsc::UnboundedReceiver<ReceivedUpdate>>,
    shutdown_tx: Option<ShutdownTx>,
}

impl QuicVariant {
    /// Create a new QUIC variant.
    ///
    /// - `runner`: the runner name (used as writer field in messages).
    /// - `bind_addr`: local address to bind the QUIC endpoint to.
    /// - `peers`: list of peer addresses to connect to.
    pub fn new(runner: &str, bind_addr: SocketAddr, peers: Vec<SocketAddr>) -> Self {
        Self {
            runner: runner.to_string(),
            bind_addr,
            peers,
            runtime: None,
            send_tx: None,
            recv_rx: None,
            shutdown_tx: None,
        }
    }
}

/// Build a quinn server config from the given certificate.
fn build_server_config(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));
    Ok(server_config)
}

/// Build a quinn client config that skips server certificate verification (LAN benchmark).
fn build_client_config() -> Result<quinn::ClientConfig> {
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
    Ok(client_config)
}

/// Custom certificate verifier that accepts any server certificate.
/// This is only appropriate for LAN benchmarking.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Handle a single incoming QUIC connection: read datagrams and streams,
/// forward decoded messages to recv_tx.
async fn handle_connection(
    connection: quinn::Connection,
    recv_tx: mpsc::UnboundedSender<ReceivedUpdate>,
    mut shutdown_rx: ShutdownRx,
) {
    let recv_tx_stream = recv_tx.clone();
    let mut shutdown_rx_stream = shutdown_rx.clone();

    // Spawn a task for reading datagrams.
    let conn_dgram = connection.clone();
    let recv_tx_dgram = recv_tx;
    let dgram_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = conn_dgram.read_datagram() => {
                    match result {
                        Ok(data) => {
                            if let Ok(update) = decode_message(&data) {
                                let _ = recv_tx_dgram.send(update);
                            }
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    // Spawn a task for reading uni streams.
    let stream_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = connection.accept_uni() => {
                    match result {
                        Ok(mut recv_stream) => {
                            let tx = recv_tx_stream.clone();
                            tokio::spawn(async move {
                                if let Ok(data) = recv_stream.read_to_end(64 * 1024).await {
                                    if let Ok(update) = decode_message(&data) {
                                        let _ = tx.send(update);
                                    }
                                }
                            });
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx_stream.changed() => break,
            }
        }
    });

    let _ = dgram_task.await;
    let _ = stream_task.await;
}

impl Variant for QuicVariant {
    fn name(&self) -> &str {
        "quic"
    }

    fn connect(&mut self) -> Result<()> {
        let runtime = Runtime::new().context("failed to create tokio runtime")?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<OutboundMessage>();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel::<ReceivedUpdate>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let bind_addr = self.bind_addr;
        let peers = self.peers.clone();

        // Generate self-signed cert.
        let ck = generate_self_signed_cert().context("failed to generate self-signed cert")?;
        let cert_der = ck.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
        );

        let server_config =
            build_server_config(cert_der, key_der).context("failed to build server config")?;

        let client_config = build_client_config().context("failed to build client config")?;

        // Create the endpoint inside the runtime.
        let endpoint = runtime.block_on(async {
            let mut endpoint = quinn::Endpoint::server(server_config, bind_addr)
                .context("failed to bind QUIC endpoint")?;
            endpoint.set_default_client_config(client_config);
            Ok::<quinn::Endpoint, anyhow::Error>(endpoint)
        })?;

        let local_addr = endpoint.local_addr()?;
        eprintln!("[quic] bound to {local_addr}");

        // Spawn background accept task.
        let accept_recv_tx = recv_tx.clone();
        let accept_shutdown_rx = shutdown_rx.clone();
        let accept_endpoint = endpoint.clone();
        runtime.spawn(async move {
            let mut shutdown = accept_shutdown_rx;
            loop {
                tokio::select! {
                    incoming = accept_endpoint.accept() => {
                        match incoming {
                            Some(incoming_conn) => {
                                match incoming_conn.await {
                                    Ok(conn) => {
                                        let tx = accept_recv_tx.clone();
                                        let srx = shutdown.clone();
                                        tokio::spawn(handle_connection(conn, tx, srx));
                                    }
                                    Err(e) => {
                                        eprintln!("[quic] incoming connection failed: {e}");
                                    }
                                }
                            }
                            None => break, // Endpoint closed.
                        }
                    }
                    _ = shutdown.changed() => break,
                }
            }
        });

        // Connect to each peer.
        let connections: Vec<quinn::Connection> = runtime.block_on(async {
            let mut conns = Vec::new();
            for peer_addr in &peers {
                match endpoint.connect(*peer_addr, "localhost") {
                    Ok(connecting) => match connecting.await {
                        Ok(conn) => {
                            eprintln!("[quic] connected to {peer_addr}");
                            conns.push(conn);
                        }
                        Err(e) => {
                            eprintln!("[quic] failed to connect to {peer_addr}: {e}");
                        }
                    },
                    Err(e) => {
                        eprintln!("[quic] connect error for {peer_addr}: {e}");
                    }
                }
            }
            conns
        });

        // Spawn receive handlers for outbound peer connections (they can also send us data).
        for conn in &connections {
            let tx = recv_tx.clone();
            let srx = shutdown_rx.clone();
            let c = conn.clone();
            runtime.spawn(handle_connection(c, tx, srx));
        }

        // Spawn background send task.
        let send_shutdown_rx = shutdown_rx.clone();
        runtime.spawn(async move {
            send_loop(send_rx, connections, send_shutdown_rx).await;
        });

        self.send_tx = Some(send_tx);
        self.recv_rx = Some(recv_rx);
        self.shutdown_tx = Some(shutdown_tx);
        self.runtime = Some(runtime);

        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let send_tx = self
            .send_tx
            .as_ref()
            .context("not connected -- call connect() first")?;

        let data = encode_message(&self.runner, path, qos, seq, payload);
        let reliable = matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp);

        send_tx
            .send(OutboundMessage { data, reliable })
            .map_err(|_| anyhow::anyhow!("send channel closed"))?;

        Ok(())
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        let recv_rx = self
            .recv_rx
            .as_mut()
            .context("not connected -- call connect() first")?;

        match recv_rx.try_recv() {
            Ok(update) => Ok(Some(update)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        // Signal shutdown to all background tasks.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        // Drop channels.
        self.send_tx.take();
        self.recv_rx.take();

        // Drop the runtime (blocks until all tasks finish or are cancelled).
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }

        Ok(())
    }
}

/// Background send loop: reads from the channel and sends over QUIC connections.
async fn send_loop(
    mut rx: mpsc::UnboundedReceiver<OutboundMessage>,
    connections: Vec<quinn::Connection>,
    mut shutdown_rx: ShutdownRx,
) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(outbound) => {
                        for conn in &connections {
                            if outbound.reliable {
                                // Send via unidirectional stream.
                                if let Ok(mut send_stream) = conn.open_uni().await {
                                    let data = outbound.data.clone();
                                    tokio::spawn(async move {
                                        let _ = send_stream.write_all(&data).await;
                                        let _ = send_stream.finish();
                                    });
                                }
                            } else {
                                // Send via datagram (fire-and-forget).
                                let _ = conn.send_datagram(outbound.data.clone().into());
                            }
                        }
                    }
                    None => break, // Channel closed.
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let writer = "runner-a";
        let path = "/bench/0";
        let qos = Qos::BestEffort;
        let seq = 42;
        let payload = vec![1, 2, 3, 4, 5];

        let encoded = encode_message(writer, path, qos, seq, &payload);
        let decoded = decode_message(&encoded).expect("decode should succeed");

        assert_eq!(decoded.writer, writer);
        assert_eq!(decoded.path, path);
        assert_eq!(decoded.qos, qos);
        assert_eq!(decoded.seq, seq);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_encode_decode_all_qos() {
        for (qos, qos_val) in [
            (Qos::BestEffort, 1),
            (Qos::LatestValue, 2),
            (Qos::ReliableUdp, 3),
            (Qos::ReliableTcp, 4),
        ] {
            let encoded = encode_message("w", "/p", qos, qos_val as u64, &[]);
            let decoded = decode_message(&encoded).unwrap();
            assert_eq!(decoded.qos, qos);
            assert_eq!(decoded.seq, qos_val as u64);
        }
    }

    #[test]
    fn test_decode_empty_payload() {
        let encoded = encode_message("w", "/p", Qos::BestEffort, 1, &[]);
        let decoded = decode_message(&encoded).unwrap();
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_decode_truncated_message() {
        assert!(decode_message(&[]).is_err());
        assert!(decode_message(&[0]).is_err());
        assert!(decode_message(&[0, 5, 1, 2]).is_err()); // writer_len=5 but only 2 bytes
    }

    #[test]
    fn test_quic_variant_name() {
        let v = QuicVariant::new("a", "0.0.0.0:0".parse().unwrap(), vec![]);
        assert_eq!(v.name(), "quic");
    }
}
