use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::variant_trait::{PeerEot, Variant};

use crate::certs::generate_self_signed_cert;

/// Wire-format tag byte distinguishing data frames from EOT frames.
///
/// Every QUIC payload (datagram or single-frame uni-stream) begins with
/// one of these tag bytes. The decoder dispatches on the tag and reads
/// the corresponding body shape.
const TAG_DATA: u8 = 0x01;
const TAG_EOT: u8 = 0x02;

/// Number of times to send each EOT datagram (qos 1-2). The retries
/// give us redundancy against UDP loss without relying on the data
/// channel's drain timing.
const EOT_DATAGRAM_RETRIES: usize = 5;

/// Spacing between successive EOT datagram sends (qos 1-2).
const EOT_DATAGRAM_SPACING: Duration = Duration::from_millis(5);

/// Data-message header layout (after the leading TAG_DATA byte):
///   - writer_len: u16 (big-endian)
///   - writer: [u8; writer_len]
///   - path_len: u16 (big-endian)
///   - path: [u8; path_len]
///   - qos: u8
///   - seq: u64 (big-endian)
///   - payload: remaining bytes
const DATA_HEADER_OVERHEAD: usize = 1 + 2 + 2 + 1 + 8;

/// EOT-message header layout (after the leading TAG_EOT byte):
///   - writer_len: u16 (big-endian)
///   - writer: [u8; writer_len]
///   - eot_id: u64 (big-endian)
const EOT_HEADER_OVERHEAD: usize = 1 + 2 + 8;

/// Decoded inbound frame: either a data message or an EOT marker.
#[derive(Debug, Clone)]
enum DecodedFrame {
    Data(ReceivedUpdate),
    Eot(PeerEot),
}

fn encode_data(writer: &str, path: &str, qos: Qos, seq: u64, payload: &[u8]) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let path_bytes = path.as_bytes();
    let total = DATA_HEADER_OVERHEAD + writer_bytes.len() + path_bytes.len() + payload.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(TAG_DATA);
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(path_bytes);
    buf.push(qos.as_int());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(payload);

    buf
}

fn encode_eot(writer: &str, eot_id: u64) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    let total = EOT_HEADER_OVERHEAD + writer_bytes.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(TAG_EOT);
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf.extend_from_slice(&eot_id.to_be_bytes());

    buf
}

fn decode_frame(data: &[u8]) -> Result<DecodedFrame> {
    if data.is_empty() {
        anyhow::bail!("empty frame");
    }
    match data[0] {
        TAG_DATA => decode_data(&data[1..]).map(DecodedFrame::Data),
        TAG_EOT => decode_eot(&data[1..]).map(DecodedFrame::Eot),
        other => anyhow::bail!("unknown frame tag 0x{other:02x}"),
    }
}

fn decode_data(data: &[u8]) -> Result<ReceivedUpdate> {
    let mut offset = 0;

    if data.len() < 2 {
        anyhow::bail!("data frame too short for writer_len");
    }
    let writer_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;

    if data.len() < offset + writer_len {
        anyhow::bail!("data frame too short for writer");
    }
    let writer = std::str::from_utf8(&data[offset..offset + writer_len])
        .context("invalid writer UTF-8")?
        .to_string();
    offset += writer_len;

    if data.len() < offset + 2 {
        anyhow::bail!("data frame too short for path_len");
    }
    let path_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;

    if data.len() < offset + path_len {
        anyhow::bail!("data frame too short for path");
    }
    let path = std::str::from_utf8(&data[offset..offset + path_len])
        .context("invalid path UTF-8")?
        .to_string();
    offset += path_len;

    if data.len() < offset + 1 {
        anyhow::bail!("data frame too short for qos");
    }
    let qos = Qos::from_int(data[offset]).context("invalid QoS value")?;
    offset += 1;

    if data.len() < offset + 8 {
        anyhow::bail!("data frame too short for seq");
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

fn decode_eot(data: &[u8]) -> Result<PeerEot> {
    let mut offset = 0;

    if data.len() < 2 {
        anyhow::bail!("eot frame too short for writer_len");
    }
    let writer_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;

    if data.len() < offset + writer_len {
        anyhow::bail!("eot frame too short for writer");
    }
    let writer = std::str::from_utf8(&data[offset..offset + writer_len])
        .context("invalid writer UTF-8")?
        .to_string();
    offset += writer_len;

    if data.len() < offset + 8 {
        anyhow::bail!("eot frame too short for eot_id");
    }
    let eot_id = u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]);

    Ok(PeerEot { writer, eot_id })
}

/// Outbound message destined for the background send task.
struct OutboundMessage {
    data: Vec<u8>,
    /// True for reliable (qos 3-4, use streams), false for best-effort
    /// (qos 1-2, use datagrams).
    reliable: bool,
    /// Per-message send strategy. EOT datagrams need retries with
    /// spacing for redundancy under loss; data messages are single-shot.
    retries: usize,
    spacing: Duration,
}

/// Inbound observation from the receive side: either a data message
/// or a freshly-deduped peer EOT.
#[derive(Debug)]
enum Inbound {
    Data(ReceivedUpdate),
    Eot(PeerEot),
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
    recv_rx: Option<mpsc::UnboundedReceiver<Inbound>>,
    shutdown_tx: Option<ShutdownTx>,
    /// Pending peer EOTs, drained by `poll_peer_eots`. Populated by
    /// either method as it pumps the inbound channel (the driver
    /// interleaves the two calls; both must service whichever inbound
    /// shape arrived since the last pump).
    pending_eots: Vec<PeerEot>,
    /// Pending data updates, drained by `poll_receive`. Populated when
    /// `poll_peer_eots` pumps the channel and finds data behind an EOT
    /// (or vice versa) -- preserves contract that both kinds of
    /// observation flow through the same channel topology without
    /// dropping inbound events on the floor.
    pending_data: std::collections::VecDeque<ReceivedUpdate>,
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
            pending_eots: Vec::new(),
            pending_data: std::collections::VecDeque::new(),
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

/// State shared across the per-connection receive tasks: a dedup set
/// of `(writer, eot_id)` pairs already forwarded to the variant. Each
/// connection's datagram and stream tasks both consult and update
/// this set so the same EOT seen via both transports is reported once.
#[derive(Default)]
struct EotDedup {
    seen: tokio::sync::Mutex<HashSet<(String, u64)>>,
}

impl EotDedup {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns true if this is the first time we have seen this
    /// `(writer, eot_id)` pair; false if it is a duplicate.
    async fn first_sight(&self, writer: &str, eot_id: u64) -> bool {
        let mut guard = self.seen.lock().await;
        guard.insert((writer.to_string(), eot_id))
    }
}

/// Handle a single incoming QUIC connection: read datagrams and streams,
/// forward decoded messages to recv_tx.
async fn handle_connection(
    connection: quinn::Connection,
    recv_tx: mpsc::UnboundedSender<Inbound>,
    eot_dedup: Arc<EotDedup>,
    mut shutdown_rx: ShutdownRx,
) {
    let recv_tx_stream = recv_tx.clone();
    let mut shutdown_rx_stream = shutdown_rx.clone();
    let dedup_stream = eot_dedup.clone();

    // Spawn a task for reading datagrams.
    let conn_dgram = connection.clone();
    let recv_tx_dgram = recv_tx;
    let dedup_dgram = eot_dedup;
    let dgram_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = conn_dgram.read_datagram() => {
                    match result {
                        Ok(data) => {
                            dispatch_decoded(&data, &recv_tx_dgram, &dedup_dgram).await;
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
                            let dedup = dedup_stream.clone();
                            tokio::spawn(async move {
                                // Each uni-stream carries exactly one
                                // frame (data or EOT). `read_to_end`
                                // returns once the writer calls
                                // `finish` and the stream is fully
                                // drained -- that is also the
                                // stream-end-as-EOT signal for the
                                // reliable path: if the trailing frame
                                // decodes to an Eot, we surface it.
                                if let Ok(buf) = recv_stream.read_to_end(64 * 1024).await {
                                    dispatch_decoded(&buf, &tx, &dedup).await;
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

/// Decode a single inbound buffer (datagram or finished uni-stream) and
/// forward the result to the variant's inbound channel. EOT frames are
/// deduped by `(writer, eot_id)` before being surfaced.
async fn dispatch_decoded(data: &[u8], tx: &mpsc::UnboundedSender<Inbound>, dedup: &EotDedup) {
    match decode_frame(data) {
        Ok(DecodedFrame::Data(update)) => {
            let _ = tx.send(Inbound::Data(update));
        }
        Ok(DecodedFrame::Eot(eot)) => {
            if dedup.first_sight(&eot.writer, eot.eot_id).await {
                let _ = tx.send(Inbound::Eot(eot));
            }
        }
        Err(_) => {
            // Malformed frame -- drop silently to match prior behaviour.
        }
    }
}

impl Variant for QuicVariant {
    fn name(&self) -> &str {
        "quic"
    }

    fn connect(&mut self) -> Result<()> {
        let runtime = Runtime::new().context("failed to create tokio runtime")?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<OutboundMessage>();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel::<Inbound>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let eot_dedup = EotDedup::new();

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
        let accept_dedup = eot_dedup.clone();
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
                                        let dd = accept_dedup.clone();
                                        tokio::spawn(handle_connection(conn, tx, dd, srx));
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
            let dd = eot_dedup.clone();
            let c = conn.clone();
            runtime.spawn(handle_connection(c, tx, dd, srx));
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

        let data = encode_data(&self.runner, path, qos, seq, payload);
        let reliable = matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp);

        send_tx
            .send(OutboundMessage {
                data,
                reliable,
                retries: 1,
                spacing: Duration::ZERO,
            })
            .map_err(|_| anyhow::anyhow!("send channel closed"))?;

        Ok(())
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // Always pump the channel first so any EOT events that arrived
        // since the last call land in `pending_eots` and any data
        // events behind them land in `pending_data`. Then return one
        // queued data update per call (FIFO).
        self.pump_inbound();

        if let Some(update) = self.pending_data.pop_front() {
            return Ok(Some(update));
        }
        Ok(None)
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

    fn signal_end_of_test(&mut self) -> Result<u64> {
        let send_tx = self
            .send_tx
            .as_ref()
            .context("not connected -- call connect() first")?;

        let eot_id: u64 = rand::random::<u64>();
        let payload = encode_eot(&self.runner, eot_id);

        // Reliable per-stream EOT (qos 3-4 path): one frame per stream
        // followed by `finish`, exactly like a data message. Sent
        // alongside the datagram path so EOT travels through every
        // transport this variant uses.
        send_tx
            .send(OutboundMessage {
                data: payload.clone(),
                reliable: true,
                retries: 1,
                spacing: Duration::ZERO,
            })
            .map_err(|_| anyhow::anyhow!("send channel closed"))?;

        // Datagram EOT (qos 1-2 path): 5 sends with 5ms spacing for
        // redundancy under loss; receivers dedupe by `(writer, eot_id)`.
        send_tx
            .send(OutboundMessage {
                data: payload,
                reliable: false,
                retries: EOT_DATAGRAM_RETRIES,
                spacing: EOT_DATAGRAM_SPACING,
            })
            .map_err(|_| anyhow::anyhow!("send channel closed"))?;

        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        // Pump first so any EOTs queued behind data (or data queued
        // behind EOTs) make it onto our side buffers. The
        // dedup-by-(writer, eot_id) work happens on the receive side
        // in `dispatch_decoded`, so anything that lands in
        // `pending_eots` is already first-sight.
        self.pump_inbound();
        let drained = std::mem::take(&mut self.pending_eots);
        Ok(drained)
    }
}

impl QuicVariant {
    /// Drain the inbound channel into the per-kind side buffers.
    /// Idempotent and non-blocking: returns when the channel reports
    /// empty or disconnected.
    fn pump_inbound(&mut self) {
        let Some(recv_rx) = self.recv_rx.as_mut() else {
            return;
        };
        loop {
            match recv_rx.try_recv() {
                Ok(Inbound::Data(update)) => self.pending_data.push_back(update),
                Ok(Inbound::Eot(eot)) => self.pending_eots.push(eot),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
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
                        for attempt in 0..outbound.retries.max(1) {
                            for conn in &connections {
                                if outbound.reliable {
                                    // Send via unidirectional stream.
                                    if let Ok(mut send_stream) = conn.open_uni().await {
                                        let data = outbound.data.clone();
                                        tokio::spawn(async move {
                                            if send_stream.write_all(&data).await.is_ok() {
                                                let _ = send_stream.finish();
                                            }
                                        });
                                    }
                                } else {
                                    // Send via datagram (fire-and-forget).
                                    let _ = conn.send_datagram(outbound.data.clone().into());
                                }
                            }
                            // Spacing between retries (only matters
                            // when `retries > 1`, e.g. EOT datagrams).
                            if outbound.retries > 1 && attempt + 1 < outbound.retries {
                                tokio::time::sleep(outbound.spacing).await;
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
    fn test_encode_decode_data_roundtrip() {
        let writer = "runner-a";
        let path = "/bench/0";
        let qos = Qos::BestEffort;
        let seq = 42;
        let payload = vec![1, 2, 3, 4, 5];

        let encoded = encode_data(writer, path, qos, seq, &payload);
        let decoded = decode_frame(&encoded).expect("decode should succeed");

        match decoded {
            DecodedFrame::Data(update) => {
                assert_eq!(update.writer, writer);
                assert_eq!(update.path, path);
                assert_eq!(update.qos, qos);
                assert_eq!(update.seq, seq);
                assert_eq!(update.payload, payload);
            }
            _ => panic!("expected Data frame"),
        }
    }

    #[test]
    fn test_encode_decode_all_qos() {
        for (qos, qos_val) in [
            (Qos::BestEffort, 1),
            (Qos::LatestValue, 2),
            (Qos::ReliableUdp, 3),
            (Qos::ReliableTcp, 4),
        ] {
            let encoded = encode_data("w", "/p", qos, qos_val as u64, &[]);
            let decoded = decode_frame(&encoded).unwrap();
            match decoded {
                DecodedFrame::Data(update) => {
                    assert_eq!(update.qos, qos);
                    assert_eq!(update.seq, qos_val as u64);
                }
                _ => panic!("expected Data frame"),
            }
        }
    }

    #[test]
    fn test_encode_decode_eot_roundtrip() {
        let writer = "runner-b";
        let eot_id: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let encoded = encode_eot(writer, eot_id);
        let decoded = decode_frame(&encoded).expect("decode should succeed");

        match decoded {
            DecodedFrame::Eot(eot) => {
                assert_eq!(eot.writer, writer);
                assert_eq!(eot.eot_id, eot_id);
            }
            _ => panic!("expected Eot frame"),
        }
    }

    #[test]
    fn test_encode_decode_eot_max_id() {
        // Boundary check: u64::MAX must roundtrip.
        let encoded = encode_eot("w", u64::MAX);
        let decoded = decode_frame(&encoded).unwrap();
        match decoded {
            DecodedFrame::Eot(eot) => {
                assert_eq!(eot.writer, "w");
                assert_eq!(eot.eot_id, u64::MAX);
            }
            _ => panic!("expected Eot frame"),
        }
    }

    #[test]
    fn test_decode_empty_payload() {
        let encoded = encode_data("w", "/p", Qos::BestEffort, 1, &[]);
        let decoded = decode_frame(&encoded).unwrap();
        match decoded {
            DecodedFrame::Data(update) => assert!(update.payload.is_empty()),
            _ => panic!("expected Data frame"),
        }
    }

    #[test]
    fn test_decode_truncated_message() {
        assert!(decode_frame(&[]).is_err());
        // Tag byte alone with no body.
        assert!(decode_frame(&[TAG_DATA]).is_err());
        assert!(decode_frame(&[TAG_EOT]).is_err());
        // Tag + writer_len=5 but only 2 bytes follow.
        assert!(decode_frame(&[TAG_DATA, 0, 5, 1, 2]).is_err());
        // EOT with truncated id (writer_len=0, no eot_id bytes).
        assert!(decode_frame(&[TAG_EOT, 0, 0]).is_err());
    }

    #[test]
    fn test_decode_unknown_tag() {
        // 0xFF is not a valid tag.
        assert!(decode_frame(&[0xFF, 0, 0]).is_err());
    }

    #[test]
    fn test_quic_variant_name() {
        let v = QuicVariant::new("a", "0.0.0.0:0".parse().unwrap(), vec![]);
        assert_eq!(v.name(), "quic");
    }

    /// Verify the dedup primitive: a `(writer, eot_id)` pair is
    /// reported on first sight only; the second call returns false.
    #[tokio::test]
    async fn test_eot_dedup_first_sight() {
        let dedup = EotDedup::new();
        assert!(dedup.first_sight("alice", 42).await);
        assert!(!dedup.first_sight("alice", 42).await);
        // Different writer or different id is a fresh sight.
        assert!(dedup.first_sight("bob", 42).await);
        assert!(dedup.first_sight("alice", 43).await);
        // Re-asserting any of the above must still return false.
        assert!(!dedup.first_sight("bob", 42).await);
        assert!(!dedup.first_sight("alice", 43).await);
    }

    /// Datagram retry-and-dedup harness: sender sends N copies of an
    /// EOT datagram (mimicking the qos 1-2 path), receiver decodes
    /// each through `dispatch_decoded` and the dedup pipeline only
    /// surfaces it once. Verifies the receiver-side dedup invariant
    /// holds when the sender duplicates aggressively.
    #[tokio::test]
    async fn test_datagram_retry_dedup() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Inbound>();
        let dedup = EotDedup::new();

        let payload = encode_eot("alice", 7);
        // Fire 5 copies, like the real EOT_DATAGRAM_RETRIES path.
        for _ in 0..EOT_DATAGRAM_RETRIES {
            dispatch_decoded(&payload, &tx, &dedup).await;
        }
        drop(tx);

        let mut eots: Vec<PeerEot> = Vec::new();
        while let Some(msg) = rx.recv().await {
            if let Inbound::Eot(eot) = msg {
                eots.push(eot);
            }
        }
        assert_eq!(eots.len(), 1, "EOT must be surfaced exactly once");
        assert_eq!(eots[0].writer, "alice");
        assert_eq!(eots[0].eot_id, 7);
    }

    /// Stream-close-with-trailer harness: write a data frame and then
    /// an EOT frame on a uni-stream and `finish`; reader observes the
    /// data first, then the EOT. Validates the per-frame stream
    /// pattern (each `open_uni` carries one frame and ends in
    /// `finish`).
    #[tokio::test]
    async fn test_stream_close_with_trailer() {
        // Spin up a loopback Quinn endpoint pair.
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let ck = generate_self_signed_cert().unwrap();
        let cert_der = ck.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
        );
        let server_config = build_server_config(cert_der, key_der).unwrap();
        let client_config = build_client_config().unwrap();

        let server_endpoint = quinn::Endpoint::server(server_config, server_addr).unwrap();
        let server_local = server_endpoint.local_addr().unwrap();

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut client_endpoint = quinn::Endpoint::client(client_addr).unwrap();
        client_endpoint.set_default_client_config(client_config);

        // Server side: accept a connection, open the receive
        // pipeline, and collect inbound observations.
        let (tx, mut rx) = mpsc::unbounded_channel::<Inbound>();
        let dedup = EotDedup::new();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_handle = tokio::spawn(async move {
            let incoming = server_endpoint.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            handle_connection(conn, tx, dedup, shutdown_rx).await;
        });

        // Client side: connect and send a data frame, then an EOT
        // frame, on two separate uni-streams.
        let conn = client_endpoint
            .connect(server_local, "localhost")
            .unwrap()
            .await
            .unwrap();

        let data_frame = encode_data("alice", "/p", Qos::ReliableTcp, 1, &[10, 20, 30]);
        let eot_frame = encode_eot("alice", 99);

        let mut s1 = conn.open_uni().await.unwrap();
        s1.write_all(&data_frame).await.unwrap();
        s1.finish().unwrap();

        let mut s2 = conn.open_uni().await.unwrap();
        s2.write_all(&eot_frame).await.unwrap();
        s2.finish().unwrap();

        // Allow the streams to flush.
        let mut observed_data = false;
        let mut observed_eot = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !(observed_data && observed_eot) {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(Inbound::Data(update)) => {
                            assert_eq!(update.writer, "alice");
                            assert_eq!(update.seq, 1);
                            assert_eq!(update.payload, vec![10, 20, 30]);
                            observed_data = true;
                        }
                        Some(Inbound::Eot(eot)) => {
                            assert_eq!(eot.writer, "alice");
                            assert_eq!(eot.eot_id, 99);
                            observed_eot = true;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        assert!(observed_data, "reader should observe the data frame");
        assert!(
            observed_eot,
            "reader should observe the EOT frame on stream-close"
        );

        // Shut down: closing the connection lets the server task drain.
        conn.close(0u32.into(), b"done");
        client_endpoint.wait_idle().await;
        server_handle.abort();
    }
}
