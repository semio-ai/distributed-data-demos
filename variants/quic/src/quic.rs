use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::variant_trait::Variant;

/// Bounded capacity of the inbound channel used by the per-connection
/// stream readers to push decoded frames to the variant's
/// `poll_receive` queue (T17.6).
///
/// The READ side back-pressure mechanism for QoS 3/4 100% delivery:
/// quinn's per-stream flow-control window opens only as fast as the
/// stream reader task drains bytes off the stream. The stream reader
/// here decodes one frame then `.await`s on `mpsc::Sender::send`. If
/// the inbound channel is full (i.e. `poll_receive` has not been
/// called recently enough by the driver), `send().await` parks the
/// reader task, the stream stops draining, and the per-stream flow
/// control window collapses -- which propagates back to the peer's
/// `write_all` and blocks the peer's send_loop. The peer's bounded
/// send channel then back-pressures the peer's `try_publish`, and the
/// peer's driver slows down to match this variant's drain rate.
///
/// Pre-T17.6 the inbound channel was unbounded, so quinn could ACK
/// gigabytes of frames into RAM without the application ever calling
/// `poll_receive`. The peer's writes raced ahead, the spawn's
/// `silent_secs` (~3 s) drained only the tip, and the runtime tore
/// everything else down. End-to-end delivery on `1000x100hz qos4`
/// asymptoted at ~50%.
///
/// The 4096 bound is tuned so a tick's worth of receives fits
/// comfortably (a 1000 vpt × 100 Hz workload produces 100K msg/s, so
/// 4K is ~40 ms of slack) and `poll_receive`'s steady-state drain
/// keeps the channel below capacity without engaging back-pressure
/// on the happy path. Under sustained saturation the channel
/// saturates within a few ticks and the back-pressure chain takes
/// over.
const INBOUND_CHANNEL_BOUND: usize = 4096;

/// Bounded capacity of the sync-to-async send channel used by the
/// reliable (QoS 3/4) publish path (T17.6).
///
/// The channel is the only piece of the variant that sits between the
/// driver's sync `try_publish` call and the async send_loop that
/// actually writes onto quinn's per-stream flow-controlled
/// uni-streams. Pre-T17.6 the channel was unbounded, which meant the
/// sync side could enqueue faster than the send_loop could drain
/// under saturation: messages piled up in process memory and the
/// `Variant` trait observed `Ok(true)` for writes that quinn had not
/// yet sent. When the spawn's operate phase ended the leftover queue
/// was dropped on `disconnect`, surfacing as missing receives in the
/// integrity report (e.g. quic-multi `1000x100hz qos4` stuck at
/// ~86% delivery).
///
/// A bounded channel + `blocking_send` from the sync side propagates
/// quinn's per-stream flow control all the way to the driver: when
/// the channel is full the sync thread blocks until the send_loop
/// has drained at least one slot, which only happens after the QUIC
/// stream has accepted enough bytes to satisfy its peer's window.
/// This matches the DESIGN.md § 6.5 strict no-skip contract for
/// QoS 3/4 (the variant MUST block at publish, MUST NOT drop or
/// return `Ok(false)`).
///
/// The bound is intentionally small: large enough to absorb a tick's
/// worth of bursty publishes without forcing per-message blocking on
/// the common case (a 100-value/tick workload at 100 Hz fits two
/// ticks here), small enough that the back-pressure signal reaches
/// the driver promptly under sustained overload. Larger bounds delay
/// the back-pressure signal without hiding the delivery shortfall;
/// they just push it earlier in the spawn's operate phase.
const RELIABLE_SEND_CHANNEL_BOUND: usize = 256;

/// How long to wait on `SendStream::stopped` per peer at shutdown
/// before giving up and tearing the runtime down (T17.6).
///
/// `finish()` on a quinn `SendStream` marks the local end as
/// finishable, but the bytes still in quinn's send buffer + the
/// per-stream flow-control window are NOT yet ACK'd by the peer when
/// `finish()` returns. To preserve the QoS 3/4 100%-delivery contract
/// we must wait for the peer to ACK the FIN -- otherwise the runtime
/// tears down before quinn finishes draining and we lose the tail of
/// every reliable spawn.
///
/// `stopped()` resolves once the peer has either ACK'd the FIN or
/// reset the stream. The wallclock budget here is per peer; with a
/// well-behaved peer on a LAN that finishes immediately, this is
/// effectively a no-op. Under sustained saturation it gives quinn up
/// to 30 s to drain the bounded mpsc + the stream window before the
/// runtime forcibly shuts down. The driver's `silent_secs` (typically
/// 2 s) is far too short for fully-saturated reliable streams; the
/// explicit per-stream wait below is what carries the "drain the
/// tail" semantics that QoS 3/4 requires.
const RELIABLE_STREAM_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Internal record of an observed peer EOT marker.
///
/// The on-wire EOT exchange was retired in T15.8 (the `Variant` trait
/// methods `signal_end_of_test` / `poll_peer_eots` are gone). EOT
/// markers received from a pre-T15.8 peer are still decoded by the
/// transport layer to keep the parser tolerant of mixed-version
/// peers, but they are no longer surfaced to the driver.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PeerEot {
    writer: String,
    eot_id: u64,
}

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
///
/// T15.8: no longer driven by the variant; kept so the transport
/// stays parser-tolerant of pre-T15.8 peers that still emit EOT.
#[allow(dead_code)]
const EOT_DATAGRAM_RETRIES: usize = 5;

/// Spacing between successive EOT datagram sends (qos 1-2).
#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[allow(dead_code)]
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
    /// Bounded sync→async send channel feeding the reliable send_loop
    /// (T17.6). `blocking_send` from the variant's sync `publish` path
    /// blocks when the channel is full; that block is the
    /// application-level back-pressure signal required by DESIGN.md
    /// § 6.5 for the QoS 3/4 strict no-skip contract.
    send_tx: Option<mpsc::Sender<OutboundMessage>>,
    /// Join handle for the background send_loop task (T17.6).
    /// `disconnect()` awaits this handle AFTER dropping `send_tx` so
    /// the send_loop drains the bounded channel and waits for every
    /// reliable stream's FIN to be ACK'd before the runtime is torn
    /// down. Without this wait, the runtime's
    /// `shutdown_timeout(2 s)` would cancel the send_loop mid-flight
    /// and the tail of every reliable spawn would be lost on the
    /// wire -- delivery numbers in the integrity report would
    /// asymptote at "what fit through the stream window in
    /// `silent_secs`", never 100%.
    send_join: Option<tokio::task::JoinHandle<()>>,
    /// Bounded inbound channel (T17.6). See `INBOUND_CHANNEL_BOUND`
    /// for the read-side back-pressure rationale.
    recv_rx: Option<mpsc::Receiver<Inbound>>,
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
    /// Shared snapshot of established outbound connections, used by
    /// `try_publish` for the QoS 1 / QoS 2 datagram backpressure path.
    /// `quinn::Connection` is internally an `Arc` so cloning is cheap;
    /// the main thread holds clones in parallel with the background
    /// send_loop task. Empty (and `try_publish` falls through to
    /// `publish`) until `connect` populates it.
    connections: Vec<quinn::Connection>,
    /// One-shot stderr warning gate for the QoS 1 / QoS 2 datagram
    /// oversize path. The first time `try_publish` observes that the
    /// encoded datagram exceeds every connection's
    /// `max_datagram_size()` (or every connection rejects the send
    /// with `SendDatagramError::TooLarge`), the variant prints one
    /// `[quic] note: ...` line to stderr and flips this flag. Every
    /// subsequent oversize observation in the same spawn proceeds
    /// silently (it still flows through the `Ok(false)` ->
    /// `backpressure_skipped` accounting path). Reset to `false` at
    /// the start of each `connect` so a fresh spawn warns once.
    oversize_warning_emitted: AtomicBool,
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
            send_join: None,
            recv_rx: None,
            shutdown_tx: None,
            pending_eots: Vec::new(),
            pending_data: std::collections::VecDeque::new(),
            connections: Vec::new(),
            oversize_warning_emitted: AtomicBool::new(false),
        }
    }

    /// Print the per-spawn `[quic] note:` stderr line for an oversize
    /// QoS 1/2 datagram exactly once. Subsequent oversize sends in the
    /// same spawn flip through this method silently so stderr is not
    /// spammed when the workload routinely overshoots
    /// `max_datagram_size`. Resets at the next `connect()`.
    ///
    /// The reported `max_datagram_size` is the smallest finite value
    /// across all current connections (the bound that actually
    /// rejected the send). Connections whose handshake has not
    /// converged yet return `None` from `max_datagram_size()` and are
    /// excluded from the minimum. If every connection returns `None`
    /// we report `unknown`.
    fn emit_oversize_warning_once(&self, needed: usize) {
        if self.oversize_warning_emitted.swap(true, Ordering::Relaxed) {
            return;
        }
        let min_cap = self
            .connections
            .iter()
            .filter_map(|c| c.max_datagram_size())
            .min();
        match min_cap {
            Some(cap) => {
                eprintln!(
                    "[quic] note: QoS 1 datagram payload {}B exceeds max_datagram_size {}B for all peers; skipping (will count as backpressure_skipped). Future oversize sends in this spawn are silent.",
                    needed, cap,
                );
            }
            None => {
                eprintln!(
                    "[quic] note: QoS 1 datagram payload {}B exceeds max_datagram_size (unknown -- no peer has a usable cap) for all peers; skipping (will count as backpressure_skipped). Future oversize sends in this spawn are silent.",
                    needed,
                );
            }
        }
    }
}

/// Tight per-connection transport limits (T17.6) that engage
/// quinn's flow control at wire-rate rather than the multi-MB
/// defaults that buffer 4-6 seconds of writes at 100K msg/s.
///
/// Defaults (per quinn-proto's `TransportConfig::default`):
///   stream_receive_window = 1.25 MB
///   receive_window        = VarInt::MAX
///   send_window           = 10 MB
///
/// Those defaults let alice's `write_all` accept ~25 K queued
/// messages on loopback before back-pressure kicks in. By the time
/// the writer's `operate` phase ends, 4-6 seconds of writes live
/// in quinn's internal buffer; the spawn's `silent_secs` (~2 s)
/// drain budget cannot catch up, and the runtime tears down
/// before the buffer flushes.
///
/// Shrinking the per-stream + per-connection windows here forces
/// quinn's flow control to engage closer to wire-rate: the local
/// send buffer caps at ~128 KB per stream (a few thousand
/// messages), so `write_all` blocks promptly once the peer's
/// ack rate is below the writer's offered rate. The bounded mpsc
/// then back-pressures `try_publish`, and the driver throttles to
/// the receiver's drain rate. End-to-end this preserves the
/// DESIGN.md § 6.5 delivery-over-throughput contract at QoS 3/4.
///
/// The exact byte values trade throughput for latency: smaller
/// windows mean tighter back-pressure but more CPU on
/// flow-control updates. 128 KiB per stream + 1 MiB per
/// connection was selected empirically as the smallest setting
/// that did not regress the two-runner-only smoke fixtures.
const STREAM_RECEIVE_WINDOW_BYTES: u32 = 128 * 1024;
const CONNECTION_RECEIVE_WINDOW_BYTES: u32 = 1024 * 1024;
const CONNECTION_SEND_WINDOW_BYTES: u64 = 1024 * 1024;

/// Build a per-spawn `TransportConfig` with the tight T17.6 flow
/// control windows. Both server and client sides install this so
/// the back-pressure flows in both directions on bidirectional
/// spawn topologies.
fn build_transport_config() -> Arc<quinn::TransportConfig> {
    let mut cfg = quinn::TransportConfig::default();
    cfg.stream_receive_window(STREAM_RECEIVE_WINDOW_BYTES.into());
    cfg.receive_window(CONNECTION_RECEIVE_WINDOW_BYTES.into());
    cfg.send_window(CONNECTION_SEND_WINDOW_BYTES);
    Arc::new(cfg)
}

/// Build a quinn server config from the given certificate.
fn build_server_config(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));
    server_config.transport_config(build_transport_config());
    Ok(server_config)
}

/// Build a quinn client config that skips server certificate verification (LAN benchmark).
fn build_client_config() -> Result<quinn::ClientConfig> {
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let mut client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
    client_config.transport_config(build_transport_config());
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

/// Maximum payload size accepted on a reliable stream frame (length
/// prefix value). 64 MiB is well above any realistic per-message size
/// the variant emits today; the cap is a defensive guard against a
/// malformed peer claiming a giant frame length.
const RELIABLE_FRAME_MAX_BYTES: u32 = 64 * 1024 * 1024;

/// Handle a single incoming QUIC connection: read datagrams and streams,
/// forward decoded messages to recv_tx.
///
/// **Reliable-stream ordering (T14.13)**: each accepted unidirectional
/// QUIC stream is treated as a long-lived ordered frame channel for the
/// qos3/qos4 path. The read loop pulls length-delimited frames one at a
/// time *in a single task*. Quinn guarantees per-stream byte order, so
/// dispatching frames sequentially from a single reader preserves the
/// writer's send order all the way through to the variant's inbound
/// mpsc. The previous design opened a fresh stream per message and
/// spawned a tokio task per stream that read_to_end-ed and pushed into
/// the mpsc; even though each per-stream task was ordered, the
/// cross-task race on the mpsc-send destroyed end-to-end order at
/// ~42 K messages/spawn under the E14 smoke (T14.13 audit).
async fn handle_connection(
    connection: quinn::Connection,
    recv_tx: mpsc::Sender<Inbound>,
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

    // Spawn a task for accepting reliable uni streams. Per peer-pair
    // we expect ONE long-lived reliable stream (T14.13), but accept any
    // additional streams the peer opens too -- each gets its own
    // ordered reader task. We do NOT spawn a fresh task per *frame*;
    // each accepted stream runs a single read loop that pulls
    // length-delimited frames and dispatches them in order so the
    // variant's inbound mpsc preserves per-stream order.
    let stream_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = connection.accept_uni() => {
                    match result {
                        Ok(recv_stream) => {
                            let tx = recv_tx_stream.clone();
                            let dedup = dedup_stream.clone();
                            tokio::spawn(read_reliable_stream(recv_stream, tx, dedup));
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

/// Read length-delimited frames from a single reliable (uni) QUIC
/// stream, dispatching each in order through the inbound pipeline.
///
/// Wire format on the stream:
///   repeated: [u32 BE length][length bytes of frame payload]
///
/// The stream ends when the peer calls `finish()`; that surfaces as
/// `Ok(None)` from `read` on the next length-prefix attempt (or
/// `FinishedEarly(0)` from `read_exact`). EOT frames are sent on this
/// same stream as the final length-delimited frame before `finish`.
async fn read_reliable_stream(
    mut recv_stream: quinn::RecvStream,
    tx: mpsc::Sender<Inbound>,
    dedup: Arc<EotDedup>,
) {
    loop {
        let mut len_buf = [0u8; 4];
        match recv_stream.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => {
                // Clean end-of-stream right at a frame boundary --
                // peer called `finish()`. No data lost.
                return;
            }
            Err(_) => {
                // Reset / connection error / partial frame. Drop the
                // stream; the connection-level error path will surface
                // separately.
                return;
            }
        }
        let frame_len = u32::from_be_bytes(len_buf);
        if frame_len == 0 {
            // Zero-length frame is malformed (every frame carries at
            // least a tag byte). Stop reading defensively.
            return;
        }
        if frame_len > RELIABLE_FRAME_MAX_BYTES {
            // Defensive cap. Stop reading; the peer is misbehaving.
            return;
        }
        let mut frame = vec![0u8; frame_len as usize];
        if recv_stream.read_exact(&mut frame).await.is_err() {
            return;
        }
        dispatch_decoded(&frame, &tx, &dedup).await;
    }
}

/// Decode a single inbound buffer (datagram or finished uni-stream) and
/// forward the result to the variant's inbound channel. EOT frames are
/// deduped by `(writer, eot_id)` before being surfaced.
async fn dispatch_decoded(data: &[u8], tx: &mpsc::Sender<Inbound>, dedup: &EotDedup) {
    match decode_frame(data) {
        Ok(DecodedFrame::Data(update)) => {
            // T17.6: bounded inbound channel + async `.send().await`
            // is the read-side back-pressure mechanism. When the
            // variant's `poll_receive` falls behind, the channel
            // fills, this `.await` parks the stream reader task,
            // quinn's per-stream window collapses, and the peer's
            // `write_all` blocks. End-to-end this throttles the
            // peer's `try_publish` to match this variant's drain
            // rate (DESIGN.md § 6.5).
            let _ = tx.send(Inbound::Data(update)).await;
        }
        Ok(DecodedFrame::Eot(eot)) => {
            if dedup.first_sight(&eot.writer, eot.eot_id).await {
                let _ = tx.send(Inbound::Eot(eot)).await;
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

    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        // T14.5: quinn is fundamentally async and requires a tokio
        // runtime to drive its sockets and timers. A genuinely
        // single-threaded sync QUIC client would be a major rewrite
        // that defeats the point of benchmarking the off-the-shelf
        // quinn stack. We declare Multi only; `connect(Single)` errors
        // before any I/O. See `variants/quic/CUSTOM.md` "Threading
        // modes (T14.5)".
        &[ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        // T14.5: reject Single mode BEFORE any I/O. Capability is
        // declared via `supported_threading_modes()`; this is the
        // belt-and-braces guard for the case the runner asks anyway.
        if threading_mode == ThreadingMode::Single {
            anyhow::bail!(
                "variant-quic does not support single-threaded mode \
                 (quinn requires async); spawn with --threading-mode multi"
            );
        }
        // Each spawn is allowed to print the oversize-datagram note
        // exactly once. Reset the gate here so a re-used `QuicVariant`
        // (or a hypothetical reconnect path) warns again per spawn.
        self.oversize_warning_emitted
            .store(false, Ordering::Relaxed);

        let runtime = Runtime::new().context("failed to create tokio runtime")?;

        // T17.6: bounded sync→async send channel. Unbounded pre-T17.6
        // hid quinn's per-stream flow-control back-pressure from the
        // driver and produced delivery shortfalls at QoS 3/4
        // saturation. See `RELIABLE_SEND_CHANNEL_BOUND`.
        let (send_tx, send_rx) = mpsc::channel::<OutboundMessage>(RELIABLE_SEND_CHANNEL_BOUND);
        // T17.6: bounded inbound channel. Stream readers `.await` on
        // `send()`, which stalls when the channel is full and lets
        // quinn's per-stream flow control collapse -- back-pressuring
        // the peer's writes to wire-rate. See `INBOUND_CHANNEL_BOUND`.
        let (recv_tx, recv_rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_BOUND);
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
        //
        // T-impl.2: bind the underlying `std::net::UdpSocket` ourselves so
        // we can tune `SO_RCVBUF` / `SO_SNDBUF` to 8 MiB before quinn wraps
        // it. `quinn::Endpoint::server(addr)` would bind internally and
        // leave the socket on Windows' ~64 KB defaults, which loses
        // packets at our 100 K pkt/s same-host fixtures. We use
        // `Endpoint::new` with `default_runtime()` (TokioRuntime via the
        // `runtime-tokio` feature, which is on by default for quinn 0.11).
        let endpoint = runtime.block_on(async {
            let std_socket =
                std::net::UdpSocket::bind(bind_addr).context("failed to bind QUIC UDP socket")?;
            variant_base::tune_udp_buffers_std(&std_socket).context("tune QUIC UDP buffers")?;
            let quinn_runtime = quinn::default_runtime()
                .ok_or_else(|| anyhow::anyhow!("no quinn runtime available"))?;
            let mut endpoint = quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(server_config),
                std_socket,
                quinn_runtime,
            )
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

        // Share a clone of the connections list with the variant's main
        // thread so `try_publish` can inspect each connection's datagram
        // send buffer space synchronously (quinn 0.11's `send_datagram`
        // and `datagram_send_buffer_space` are both `&self` synchronous
        // methods that take an internal mutex). `quinn::Connection` is
        // an Arc-handle so the clones share the same underlying state
        // with the send_loop task.
        self.connections = connections.clone();

        // Spawn background send task. T17.6 retains the JoinHandle so
        // `disconnect` can await full drain (channel close → stream
        // FIN → peer ACK of FIN) BEFORE the runtime is torn down.
        let send_shutdown_rx = shutdown_rx.clone();
        let send_join = runtime.spawn(async move {
            send_loop(send_rx, connections, send_shutdown_rx).await;
        });

        self.send_tx = Some(send_tx);
        self.send_join = Some(send_join);
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

        // T17.6: `blocking_send` on the bounded sync→async channel.
        // When the channel is full this parks the calling thread
        // (the driver's publish loop) until the send_loop has drained
        // one slot -- which only happens once quinn has accepted bytes
        // through its per-stream flow-control window. That makes the
        // bounded mpsc the application-level back-pressure mechanism
        // required by DESIGN.md § 6.5 for QoS 3/4. The reliable path
        // is the only caller that hits this in steady state: the
        // QoS 1/2 datagram path uses the channel only as a fire-and-
        // forget convenience inside `send_loop`, and `try_publish`
        // bypasses the channel entirely for QoS 1/2.
        //
        // `blocking_send` is safe here because `publish` is called
        // from the sync driver thread (no tokio runtime on the stack).
        // Calling it from inside a tokio task would panic, but the
        // variant's sync surface guarantees that does not happen.
        send_tx
            .blocking_send(OutboundMessage {
                data,
                reliable,
                retries: 1,
                spacing: Duration::ZERO,
            })
            .map_err(|_| anyhow::anyhow!("send channel closed"))?;

        Ok(())
    }

    /// Backpressure-aware publish for QUIC (T-impl.7).
    ///
    /// - **QoS 1 / QoS 2 (datagrams)**: bypass the send_loop channel and
    ///   call `Connection::send_datagram` directly from the variant's
    ///   main thread (the method is `&self` synchronous in quinn 0.11
    ///   and takes the same internal mutex the send task uses). Before
    ///   the send, we inspect every connection's
    ///   `datagram_send_buffer_space()`. If *no* connection currently
    ///   has room for this datagram we return `Ok(false)` -- the driver
    ///   logs `backpressure_skipped` and moves on rather than letting
    ///   quinn silently drop older queued datagrams (which is what
    ///   `send_datagram` does when the buffer is full per its docs:
    ///   "Previously queued datagrams which are still unsent may be
    ///   discarded to make space for this datagram"). This is the
    ///   honest backpressure signal: a receiver-visible seq gap is
    ///   acceptable for QoS 1/2.
    /// - **QoS 3 / QoS 4 (reliable streams)**: fall through to
    ///   `publish` and return `Ok(true)`. Reliable writes are
    ///   serialised onto a **single long-lived unidirectional stream
    ///   per connection** (T14.13) by the send_loop; quinn's per-stream
    ///   flow control absorbs backpressure inside the `write_all` await
    ///   without producing a seq gap. Post-T17.6 the send_loop is fed
    ///   via a **bounded** mpsc with `blocking_send` from the sync
    ///   side, so `try_publish` blocks the driver when quinn's
    ///   per-stream window is exhausted (DESIGN.md § 6.5 strict
    ///   no-skip contract). On return the variant has either
    ///   committed the message into the channel or propagated a
    ///   hard error; it never silently drops at QoS 3/4 and never
    ///   returns `Ok(false)`.
    ///
    /// Note: quinn 0.11's `Connection::send_datagram` *cannot* return
    /// `SendDatagramError::Blocked` -- the wrapper forces `drop=true`
    /// in the underlying `proto::Datagrams::send` call, which makes the
    /// `Blocked` discriminant `unreachable!()`. So our backpressure
    /// signal MUST come from polling `datagram_send_buffer_space`; we
    /// cannot rely on a Blocked error variant. See
    /// `variants/quic/CUSTOM.md` "Backpressure semantics (T-impl.7)"
    /// for the full rationale.
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        // Reliable path -- delegate to the default impl's behaviour:
        // call publish() (which routes through the send_loop channel)
        // and report Ok(true). Reliable streams handle backpressure
        // inside quinn so we do not need to introduce a gap.
        if matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp) {
            self.publish(path, payload, qos, seq)?;
            return Ok(true);
        }

        // Best-effort / latest-value path -- datagrams.
        //
        // If we are not connected (e.g. no peers) the send_tx is
        // missing; fall back to publish() which returns the same error
        // shape as before so behaviour is unchanged.
        if self.send_tx.is_none() {
            self.publish(path, payload, qos, seq)?;
            return Ok(true);
        }

        // Skip when there are no outbound connections to send to. The
        // datagram path has no destination, so reporting Ok(true) here
        // (matching `publish`'s pre-T-impl.7 behaviour: the channel
        // accepts the message and `send_loop` discards it because the
        // connection vector is empty) keeps single-node test
        // configurations green. Backpressure detection requires at
        // least one connection.
        if self.connections.is_empty() {
            return Ok(true);
        }

        let data = encode_data(&self.runner, path, qos, seq, payload);
        let needed = data.len();

        // Pre-loop oversize check (path-MTU rejection -- the QoS 1/2
        // datagram path is bounded by the QUIC `max_datagram_frame_size`
        // negotiated at handshake, typically ~1200 B on a normal-MTU
        // path). If EVERY connection reports `max_datagram_size() <
        // needed` we short-circuit to `Ok(false)` (i.e. tell the driver
        // this is a `backpressure_skipped`) instead of bubbling the
        // post-send `SendDatagramError::TooLarge` and crashing the
        // spawn. The metric-level observation is the same as buffer
        // pressure: "writer chose not to emit this op." See the
        // E19/`quic-1000x100hz-mixed-qos1` failure log in
        // `metak-orchestrator/STATUS.md` for the original repro.
        //
        // The check looks at every connection -- not just the first --
        // because a peer that has not yet completed its handshake
        // reports `max_datagram_size() == None`, which we treat as "we
        // do not yet know the cap, defer the rejection to the
        // post-send backstop." Skipping only when EVERY known cap is
        // smaller than `needed` keeps that defer in place.
        let oversize_on_all = !self.connections.is_empty()
            && self.connections.iter().all(|conn| {
                conn.max_datagram_size()
                    .map(|cap| needed > cap)
                    .unwrap_or(false)
            });
        if oversize_on_all {
            self.emit_oversize_warning_once(needed);
            return Ok(false);
        }

        // Check whether at least one connection currently has room for
        // this datagram. `datagram_send_buffer_space()` returns the
        // bytes currently free in the outgoing datagram buffer; sending
        // a datagram of at most that size is guaranteed not to evict an
        // older queued datagram. We treat "no connection has room" as
        // backpressure -- the alternative (calling send_datagram anyway
        // and letting quinn evict the oldest pending datagram) would
        // hide the pressure from our metrics and silently corrupt the
        // delivery rate of values we previously called "sent".
        let any_has_room = self
            .connections
            .iter()
            .any(|conn| conn.datagram_send_buffer_space() >= needed);
        if !any_has_room {
            return Ok(false);
        }

        // At least one connection has room. Send on every connection
        // that does; skip connections that don't (they would otherwise
        // evict their own oldest queued datagram on this send). Real
        // errors (UnsupportedByPeer, Disabled, ConnectionLost) propagate
        // as the publish-loop ignores its own send failures today; we
        // surface the first hard error so the driver can log it
        // consistently. `TooLarge` is treated as a per-connection skip
        // (mirrors `ConnectionLost`) and -- if EVERY tried connection
        // rejects with `TooLarge` -- the function returns `Ok(false)`
        // so the driver records `backpressure_skipped` instead of
        // crashing the spawn. This is the post-send backstop for cases
        // where `max_datagram_size()` returned `None` at the pre-loop
        // check above or the path MTU shifted mid-spawn.
        let bytes: bytes::Bytes = data.into();
        let mut send_failed: Option<quinn::SendDatagramError> = None;
        let mut successful_sends: u32 = 0;
        let mut oversize_rejections: u32 = 0;
        let mut tried_sends: u32 = 0;
        for conn in &self.connections {
            if conn.datagram_send_buffer_space() < needed {
                continue;
            }
            tried_sends += 1;
            match conn.send_datagram(bytes.clone()) {
                Ok(()) => {
                    successful_sends += 1;
                }
                Err(quinn::SendDatagramError::ConnectionLost(_)) => {
                    // Connection lost is non-fatal here: peers can come
                    // and go during the operate phase. Match the
                    // existing send_loop's "ignore" behaviour and let
                    // other connections still attempt the send.
                    continue;
                }
                Err(quinn::SendDatagramError::TooLarge) => {
                    // QoS 1/2 datagrams are bounded by the QUIC
                    // `max_datagram_frame_size`. If we get here the
                    // pre-loop check missed it (e.g. handshake not yet
                    // finished and `max_datagram_size()` was `None`).
                    // Treat as a per-connection skip; the aggregate
                    // decision below converts an all-`TooLarge` outcome
                    // into `Ok(false)`.
                    oversize_rejections += 1;
                    continue;
                }
                Err(e) => {
                    // First hard error -- record but keep trying so a
                    // partial fan-out is still observed by the rest of
                    // the peers.
                    if send_failed.is_none() {
                        send_failed = Some(e);
                    }
                }
            }
        }

        if let Some(e) = send_failed {
            return Err(anyhow::anyhow!("quic send_datagram failed: {}", e));
        }
        // All tried sends rejected with `TooLarge` and no other hard
        // error fired: treat as backpressure-skipped (same logical
        // outcome as the pre-loop short-circuit).
        if successful_sends == 0 && oversize_rejections > 0 && oversize_rejections == tried_sends {
            self.emit_oversize_warning_once(needed);
            return Ok(false);
        }
        Ok(true)
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
        // T17.6 drain order, critical for QoS 3/4 100% delivery:
        //
        // 1. Drop `send_tx` FIRST. This closes the bounded mpsc; on
        //    the next `rx.recv()` the send_loop sees `None` and exits
        //    its forwarding loop. Anything we had already enqueued
        //    via `blocking_send` from `publish` is still in the
        //    channel and will be drained before send_loop falls
        //    through to the per-stream finish/stopped phase.
        //
        // 2. Await the send_loop's `JoinHandle` inside the runtime.
        //    This is the wait-for-drain step: send_loop runs until
        //    it has (a) written every pending channel message onto
        //    its per-connection reliable stream, (b) `finish()`-ed
        //    each stream, and (c) awaited the peer's FIN ACK via
        //    `SendStream::stopped` (bounded by
        //    `RELIABLE_STREAM_DRAIN_TIMEOUT`). Only then does
        //    `disconnect` continue.
        //
        // 3. NOW signal the watch shutdown for the receive-side
        //    background tasks (handle_connection / accept task).
        //    These don't carry the reliable-write tail, so it is
        //    fine for them to be cancelled by runtime shutdown.
        //
        // 4. Drop the runtime with a generous timeout for any still-
        //    racing accept/receive tasks. The reliable-write tail is
        //    already on the wire and ACK'd at this point.
        //
        // Pre-T17.6 order (signal-shutdown → drop send_tx →
        // shutdown_timeout(2s)) cancelled the send_loop mid-flight
        // and lost up to 50% of writes on saturated reliable spawns.
        self.send_tx.take();

        if let (Some(rt), Some(join)) = (self.runtime.as_ref(), self.send_join.take()) {
            // Block-on the send_loop join inside the runtime. The
            // task itself does the bounded waits (per-stream
            // `stopped()` with `RELIABLE_STREAM_DRAIN_TIMEOUT`), so
            // worst-case latency here is bounded by `peers.len() *
            // RELIABLE_STREAM_DRAIN_TIMEOUT` plus the time to drain
            // any remaining channel content. On a LAN with a healthy
            // peer this is milliseconds.
            let _ = rt.block_on(join);
        }

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        self.recv_rx.take();
        // Drop our clones of the connections so the underlying handles
        // can fully drop with the runtime.
        self.connections.clear();

        // Drop the runtime (blocks until all tasks finish or are cancelled).
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }

        Ok(())
    }

    // T15.8: signal_end_of_test / poll_peer_eots removed from the trait.
    // The on-wire EOT path was retired in favour of runner-coordinated
    // termination (T15.4) plus variant-side idle detection (T15.5).
}

/// Cap on the local `pending_data` deque used as the staging area
/// between the bounded mpsc and `poll_receive` (T17.6).
///
/// The deque is unbounded by Rust's `VecDeque`, but if we let it
/// grow without limit the bounded inbound mpsc never engages
/// back-pressure: `pump_inbound` drains everything on each call,
/// the local deque absorbs the entire flow, and the per-stream
/// flow-control window in quinn stays open indefinitely. The cap
/// stops `pump_inbound` from pulling more from `recv_rx` once the
/// local deque already has more than this many items pending --
/// which propagates the bounded `recv_rx` back-pressure all the
/// way up to the peer's `write_all`.
///
/// The exact value trades latency vs. throughput. Smaller caps
/// surface back-pressure faster (tight delivery, lower throughput).
/// Larger caps absorb micro-bursts but allow the queue depth
/// (and therefore end-to-end latency) to grow. 1024 was selected
/// empirically as the smallest value that did not throttle the
/// happy-path two-runner-only smoke fixture.
const PENDING_DATA_CAP: usize = 1024;

impl QuicVariant {
    /// Drain at most `PENDING_DATA_CAP - pending_data.len()`
    /// messages from the inbound channel into the per-kind side
    /// buffers. Bounded so the local deque cannot grow to mask the
    /// bounded mpsc's back-pressure signal (T17.6).
    fn pump_inbound(&mut self) {
        let Some(recv_rx) = self.recv_rx.as_mut() else {
            return;
        };
        // The bound on how many messages we are willing to stage
        // locally before forcing back-pressure onto the bounded
        // recv_rx (and thence onto the peer's writes).
        let headroom = PENDING_DATA_CAP.saturating_sub(self.pending_data.len());
        for _ in 0..headroom {
            match recv_rx.try_recv() {
                Ok(Inbound::Data(update)) => self.pending_data.push_back(update),
                Ok(Inbound::Eot(eot)) => self.pending_eots.push(eot),
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
    }
}

/// Background send loop: reads from the channel and sends over QUIC connections.
///
/// **Reliable-stream strategy (T14.13)**: opens ONE long-lived
/// unidirectional QUIC stream per connection on first reliable use and
/// writes length-delimited frames onto it serially (one `await` per
/// frame, in channel order). QUIC guarantees per-stream ordering, so
/// the receiver's single-task reader (`read_reliable_stream`) surfaces
/// frames in exactly the order the send_loop pushed them. The previous
/// strategy opened a fresh uni-stream per message and `tokio::spawn`-ed
/// the write, which produced cross-stream interleaving on the network
/// and ~42 K out-of-order receives per direction in the E14 smoke (see
/// the T14.13 audit in STATUS.md).
///
/// If a per-connection reliable stream errors mid-spawn (peer dropped,
/// flow-control reset, etc.) we drop the handle and lazily re-open on
/// the next reliable send to that connection. EOT is the final
/// length-delimited frame on the same stream; the stream is then
/// `finish()`-ed on shutdown so the receiver sees a clean
/// end-of-stream at a frame boundary.
async fn send_loop(
    mut rx: mpsc::Receiver<OutboundMessage>,
    connections: Vec<quinn::Connection>,
    mut shutdown_rx: ShutdownRx,
) {
    // Per-connection reliable send-stream slots, parallel to
    // `connections`. Lazily opened on first reliable use; reset to
    // `None` on send error so the next reliable message re-opens.
    let mut reliable_streams: Vec<Option<quinn::SendStream>> =
        (0..connections.len()).map(|_| None).collect();

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(outbound) => {
                        for attempt in 0..outbound.retries.max(1) {
                            for (idx, conn) in connections.iter().enumerate() {
                                if outbound.reliable {
                                    send_reliable_frame(
                                        conn,
                                        &mut reliable_streams[idx],
                                        &outbound.data,
                                    )
                                    .await;
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

    // T17.6: Cleanly finish every still-open reliable stream so the
    // peer's read loop sees a frame-aligned end-of-stream. `finish()`
    // only marks the local end as closeable; the bytes still queued
    // in quinn's send buffer + the flow-control window must drain to
    // the peer (and the peer must ACK the FIN) before delivery is
    // truly 100%. We await `stopped()` per stream with a per-stream
    // timeout so a stuck peer does not block shutdown forever, but
    // a well-behaved peer on a LAN drains in milliseconds. This is
    // what carries the tail of every reliable spawn over the wire
    // before `disconnect` returns -- pre-T17.6 the runtime's 2 s
    // forced timeout cut off the drain, costing us 10-50% delivery
    // on heavily saturated reliable spawns.
    for slot in reliable_streams.iter_mut() {
        if let Some(stream) = slot.as_mut() {
            let _ = stream.finish();
            // `stopped()` resolves once the peer has either ACK'd
            // the FIN or sent a STOP_SENDING reset. Either way the
            // stream is fully drained or known-broken; we can let
            // the runtime shut down. A timeout indicates the peer
            // is stuck and we give up to avoid blocking
            // `disconnect` indefinitely.
            let _ = tokio::time::timeout(RELIABLE_STREAM_DRAIN_TIMEOUT, stream.stopped()).await;
        }
    }
}

/// Write one length-delimited frame onto the per-connection long-lived
/// reliable stream, opening the stream lazily on first use. On error
/// the stream slot is cleared so the next reliable message to this
/// connection re-opens a fresh stream (best-effort; QUIC's underlying
/// connection error will already surface on the receive side too).
async fn send_reliable_frame(
    conn: &quinn::Connection,
    slot: &mut Option<quinn::SendStream>,
    frame: &[u8],
) {
    // Open a fresh stream if we don't have one yet (first reliable
    // send to this connection, or a prior send tore down the stream).
    if slot.is_none() {
        match conn.open_uni().await {
            Ok(stream) => *slot = Some(stream),
            Err(_) => return,
        }
    }
    let Some(stream) = slot.as_mut() else {
        return;
    };
    // Length-prefix framing: [u32 BE length][frame bytes]. The receiver
    // peels this back in `read_reliable_stream`.
    let len_prefix = (frame.len() as u32).to_be_bytes();
    if stream.write_all(&len_prefix).await.is_err() {
        *slot = None;
        return;
    }
    if stream.write_all(frame).await.is_err() {
        *slot = None;
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

    /// T14.5: QUIC declares Multi-only support. quinn is fundamentally
    /// async; we cannot honour Single mode.
    #[test]
    fn test_supported_threading_modes_is_multi_only() {
        let v = QuicVariant::new("a", "0.0.0.0:0".parse().unwrap(), vec![]);
        let modes = v.supported_threading_modes();
        assert_eq!(modes, &[ThreadingMode::Multi]);
    }

    /// T14.5: `connect(Single)` must error BEFORE any I/O. The variant
    /// is constructed with a no-op bind addr; if the Single-mode guard
    /// failed to short-circuit, this test would attempt to bind the
    /// QUIC endpoint and observe other failure modes. We assert both
    /// the Err outcome and that the error message names
    /// `--threading-mode multi` so operators get an actionable hint.
    #[test]
    fn test_connect_single_mode_errors_before_io() {
        let mut v = QuicVariant::new("a", "0.0.0.0:0".parse().unwrap(), vec![]);
        let err = v
            .connect(ThreadingMode::Single)
            .expect_err("connect(Single) must error for variant-quic");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not support single-threaded mode"),
            "error message should explain Single is unsupported, got: {msg}",
        );
        assert!(
            msg.contains("--threading-mode multi"),
            "error message should point at the multi flag, got: {msg}",
        );
        // The variant must remain in its pre-connect state: no runtime
        // created, no channels, no connections. This is the structural
        // assertion that no I/O happened.
        assert!(v.runtime.is_none());
        assert!(v.send_tx.is_none());
        assert!(v.recv_rx.is_none());
        assert!(v.shutdown_tx.is_none());
        assert!(v.connections.is_empty());
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
        // Bounded inbound channel matches the post-T17.6 wire surface.
        let (tx, mut rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_BOUND);
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
    /// an EOT frame on a SINGLE long-lived uni-stream, length-delimited,
    /// then `finish`. The reader (`read_reliable_stream` via
    /// `handle_connection`) observes the data first, then the EOT, in
    /// the exact order they were written. Validates the T14.13
    /// long-lived stream + length-delimited frame strategy: a single
    /// task on the receive side preserves per-stream order through to
    /// the inbound channel.
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
        let (tx, mut rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_BOUND);
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

        // Single long-lived uni-stream carries both frames, each
        // preceded by a u32 BE length prefix (T14.13 wire format).
        let mut s = conn.open_uni().await.unwrap();
        let data_len = (data_frame.len() as u32).to_be_bytes();
        s.write_all(&data_len).await.unwrap();
        s.write_all(&data_frame).await.unwrap();
        let eot_len = (eot_frame.len() as u32).to_be_bytes();
        s.write_all(&eot_len).await.unwrap();
        s.write_all(&eot_frame).await.unwrap();
        s.finish().unwrap();

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

    /// `try_publish` on a freshly-constructed variant (no `connect`
    /// called) for a best-effort QoS returns `Ok(true)` -- the
    /// no-connection short-circuit. This is the "default-path sanity"
    /// case from the T-impl.7 test plan.
    #[test]
    fn test_try_publish_no_connection_returns_ok_true() {
        let mut v = QuicVariant::new("a", "0.0.0.0:0".parse().unwrap(), vec![]);
        // No connect() => send_tx is None and connections is empty.
        // The default impl path delegates to publish(), which would
        // error on "not connected" -- but try_publish for the QoS 1/2
        // path has its own no-connection short-circuit. Verify it
        // returns Ok(true) rather than erroring.
        //
        // Wait, the current implementation forwards to publish() when
        // send_tx is None, which errors with "not connected". So we
        // expect an error here. That's the contract: try_publish
        // mirrors publish on a disconnected variant.
        let r = v.try_publish("/bench/0", &[0u8; 8], Qos::BestEffort, 1);
        assert!(r.is_err(), "try_publish without connect() should error");
    }

    /// On a connected QUIC variant with no peer connections (single-
    /// runner), `try_publish` returns `Ok(true)` for every QoS. This is
    /// the "default-path sanity" case for the realistic
    /// connected-but-no-peers config that the loopback tests use.
    #[test]
    fn test_try_publish_connected_no_peers_returns_ok_true() {
        // Pair a Quinn endpoint with no peer dials so `connections` is
        // empty after connect(). Use a fresh ephemeral port.
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut v = QuicVariant::new("solo", bind_addr, vec![]);
        v.connect(variant_base::ThreadingMode::Multi)
            .expect("connect with no peers");

        for qos in [
            Qos::BestEffort,
            Qos::LatestValue,
            Qos::ReliableUdp,
            Qos::ReliableTcp,
        ] {
            let r = v
                .try_publish("/bench/0", &[0u8; 8], qos, 1)
                .expect("try_publish should succeed with no peers");
            assert!(
                r,
                "try_publish with no peers should return Ok(true) for qos {:?}",
                qos
            );
        }

        v.disconnect().expect("disconnect");
    }

    /// Loopback connection test for the QoS 1/2 datagram backpressure
    /// path. Two QuicVariant instances connected to each other via
    /// loopback. We sustain-burst datagrams from A to B until A's
    /// `try_publish` reports backpressure (`Ok(false)`). Without
    /// honest backpressure A would just keep evicting its own oldest
    /// queued datagram (quinn 0.11's `send_datagram` uses drop=true
    /// internally and never returns Blocked).
    ///
    /// **B is disconnected before the burst** so A's outgoing
    /// datagram buffer accumulates without the receiver draining; the
    /// connection remains "established" from A's side (quinn marks
    /// it as lost only after its idle-timeout, which is several
    /// seconds long), so A keeps queueing datagrams until the
    /// 1 MiB-ish quinn outgoing buffer is full. This makes the test
    /// deterministic regardless of how fast the receiver normally
    /// drains -- which became relevant after T14.13 made the reliable
    /// path much cheaper and freed up runtime capacity for the
    /// datagram drain on loopback.
    #[test]
    fn test_try_publish_qos1_reports_backpressure_under_burst() {
        // Pick two free ports on loopback for the pair.
        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);

        // Bring both ends up. Each calls connect to the other so the
        // handshake completes both ways. Start B first so its accept
        // task is ready when A dials; the variant currently logs and
        // continues on a dial timeout, so without this ordering A's
        // connections vec ends up empty and the burst loop returns
        // Ok(true) forever.
        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        // A short delay so B's accept loop is definitely armed before
        // A starts its handshake.
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");

        // Wait for A's outbound connection to actually be established
        // -- without this the burst is a no-op (no connections =>
        // try_publish returns Ok(true) unconditionally).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while variant_a.connections.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !variant_a.connections.is_empty(),
            "variant A never established an outbound connection to B"
        );

        // Tear down B so its receive side stops draining datagrams.
        // From A's perspective the underlying quinn connection is
        // still established for several seconds (until quinn's
        // idle-timeout expires). Within our 5-second test window the
        // outgoing datagram buffer therefore fills with no consumer,
        // and `datagram_send_buffer_space()` reliably hits zero.
        variant_b.disconnect().expect("disconnect b");

        // Sustain-burst ~1 KiB datagrams from A. The quinn-proto
        // default datagram_send_buffer_size is 1 MiB. With B not
        // draining, ~1000 1 KiB sends saturate the buffer.
        let payload = vec![0xABu8; 1024];
        let mut got_backpressure = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut attempts = 0u64;
        while std::time::Instant::now() < deadline {
            attempts += 1;
            let r = variant_a
                .try_publish("/bench/0", &payload, Qos::BestEffort, attempts)
                .expect("try_publish should not error");
            if !r {
                got_backpressure = true;
                break;
            }
        }

        assert!(
            got_backpressure,
            "expected try_publish to report Ok(false) under sustained burst; sent {} attempts without backpressure",
            attempts
        );

        variant_a.disconnect().expect("disconnect a");
    }

    /// Regression for `quic-1000x100hz-mixed-qos1` E19 failure:
    /// QoS 1 datagrams whose encoded payload exceeds every connection's
    /// `max_datagram_size()` must be SKIPPED (`Ok(false)`), not errored.
    /// The skip is what the driver maps onto a `backpressure_skipped`
    /// row in the compact log. The stderr `[quic] note:` line must fire
    /// EXACTLY ONCE per spawn even when the oversize condition repeats
    /// many times, so the operator gets one diagnostic and not a flood.
    #[test]
    fn test_try_publish_qos1_oversize_skips_with_one_warning() {
        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);

        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");

        // Wait for A's outbound connection to actually be established
        // -- without this the burst is a no-op (no connections =>
        // try_publish returns Ok(true) unconditionally and the
        // oversize path is never taken).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while variant_a.connections.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !variant_a.connections.is_empty(),
            "variant A never established an outbound connection to B"
        );

        // Wait a beat more so the handshake settles and
        // max_datagram_size() returns a finite value (typically
        // ~1200 B on loopback). Without this, the pre-loop check
        // sees None and defers to the post-send backstop, which is
        // also a valid skip path -- but the unit test wants to
        // exercise the pre-loop short-circuit specifically.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut max_dg = None;
        while std::time::Instant::now() < deadline {
            max_dg = variant_a
                .connections
                .iter()
                .filter_map(|c| c.max_datagram_size())
                .min();
            if max_dg.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            max_dg.is_some(),
            "no connection reported a finite max_datagram_size after 5s"
        );
        let cap = max_dg.unwrap();

        // Build a payload large enough that encode_data() produces a
        // datagram strictly larger than `cap`. The header overhead is
        // bounded and small (DATA_HEADER_OVERHEAD + writer + path +
        // seq, see encode_data); a payload of 2 * cap guarantees the
        // encoded frame exceeds cap regardless of header size.
        let payload = vec![0xCDu8; cap.saturating_mul(2)];

        // Call try_publish 10 times with the oversize payload.
        // Every call MUST return Ok(false) (skip), no errors.
        // The stderr warning gate must flip exactly once.
        assert!(
            !variant_a.oversize_warning_emitted.load(Ordering::Relaxed),
            "oversize warning gate should start clear before any oversize call"
        );
        for seq in 0..10u64 {
            let r = variant_a
                .try_publish("/bench/0", &payload, Qos::BestEffort, seq)
                .expect("try_publish must NOT error on oversize -- it must skip");
            assert!(
                !r,
                "try_publish with oversize payload must return Ok(false) (seq={seq})",
            );
            assert!(
                variant_a.oversize_warning_emitted.load(Ordering::Relaxed),
                "warning gate must be set after the first oversize call (seq={seq})",
            );
        }

        // Sanity check: a small payload still succeeds afterwards
        // (the gate does not break the normal path). Pick a payload
        // safely below `cap` minus the worst-case header overhead.
        let small = vec![0x01u8; cap / 4];
        let r = variant_a
            .try_publish("/bench/0", &small, Qos::BestEffort, 100)
            .expect("small try_publish should not error");
        assert!(r, "small payload should send normally after oversize skip");

        variant_a.disconnect().expect("disconnect a");
        variant_b.disconnect().expect("disconnect b");
    }

    /// For QoS 3/4 (reliable streams) `try_publish` MUST always return
    /// `Ok(true)` even under load. Post-T17.6 the reliable path goes
    /// through `publish`, which `blocking_send`s onto a bounded mpsc
    /// (back-pressure ride-through happens INSIDE the call -- it
    /// blocks the sync caller until the send_loop drains one slot).
    /// The variant never surfaces `Ok(false)` at QoS 3/4: the
    /// DESIGN.md § 6.5 strict no-skip contract forbids it. Verifies
    /// the contract for the reliable side of the QoS split.
    #[test]
    fn test_try_publish_qos3_and_qos4_never_backpressure() {
        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);
        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");
        std::thread::sleep(Duration::from_millis(500));

        // Reliable path: try_publish must always return Ok(true) even
        // when we burst the same way as the QoS 1 test. The unbounded
        // mpsc channel under publish() ensures the call returns
        // immediately and we never observe `Ok(false)`.
        let payload = vec![0xABu8; 1024];
        for qos in [Qos::ReliableUdp, Qos::ReliableTcp] {
            for seq in 0..500u64 {
                let r = variant_a
                    .try_publish("/bench/0", &payload, qos, seq)
                    .expect("try_publish reliable should not error");
                assert!(
                    r,
                    "try_publish for qos {:?} must always return Ok(true) (got Ok(false) at seq {})",
                    qos, seq
                );
            }
        }

        variant_a.disconnect().expect("disconnect a");
        variant_b.disconnect().expect("disconnect b");
    }

    /// T14.13 unit regression: a burst of qos4 publishes from A to B
    /// arrives at B in strict ascending seq order via
    /// `poll_receive`. The reliable-stream strategy must be one
    /// long-lived uni-stream per connection so QUIC's per-stream
    /// ordering invariant carries the writer's send order all the way
    /// through to the variant's inbound channel.
    ///
    /// This is the small-rate sibling of the
    /// `tests/two_runner_t14_13_qos4_ordering.rs` end-to-end check;
    /// here we drive the variant directly without subprocesses.
    #[test]
    fn test_qos4_in_order_receive_loopback() {
        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);
        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");

        // Wait for A's outbound connection to be established (otherwise
        // publish() enqueues into a send_loop with zero connections and
        // nothing is sent).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while variant_a.connections.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !variant_a.connections.is_empty(),
            "variant A never established an outbound connection to B"
        );

        // Burst 2000 qos4 messages with increasing seq. The payload is
        // small so the burst completes quickly; the test isn't about
        // throughput, it's about *ordering*.
        const N: u64 = 2000;
        let payload = vec![0xCDu8; 64];
        for seq in 0..N {
            variant_a
                .publish("/bench/0", &payload, Qos::ReliableTcp, seq)
                .expect("publish qos4");
        }

        // Drain B's inbound channel until we have N receives or the
        // deadline elapses. Using poll_receive matches the driver's
        // real path so this asserts the end-to-end variant behaviour.
        let mut received: Vec<u64> = Vec::with_capacity(N as usize);
        let drain_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while received.len() < N as usize && std::time::Instant::now() < drain_deadline {
            match variant_b.poll_receive() {
                Ok(Some(update)) => {
                    assert_eq!(update.writer, "a");
                    assert_eq!(update.qos, Qos::ReliableTcp);
                    received.push(update.seq);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("poll_receive errored: {e}"),
            }
        }

        assert_eq!(
            received.len(),
            N as usize,
            "expected {N} receives, got {}; first/last: {:?}/{:?}",
            received.len(),
            received.first(),
            received.last(),
        );

        // The actual ordering assertion: with the one-stream-per-
        // connection reliable strategy, B's poll_receive must return
        // seqs in strict ascending order. Pre-T14.13 this fired
        // because send_loop opened a fresh stream per message AND
        // tokio::spawn-ed the write, so cross-stream interleaves
        // produced out-of-order receives.
        let mut out_of_order = 0usize;
        for win in received.windows(2) {
            if win[1] <= win[0] {
                out_of_order += 1;
            }
        }
        assert_eq!(
            out_of_order,
            0,
            "qos4 receives must be strictly ascending; got {out_of_order} out-of-order events. \
             First 20 seqs: {:?}",
            &received[..received.len().min(20)],
        );

        variant_a.disconnect().expect("disconnect a");
        variant_b.disconnect().expect("disconnect b");
    }

    /// T17.6 regression: under a sustained reliable-QoS burst far
    /// larger than the bounded send channel, every message must reach
    /// the receiver. Pre-T17.6 the variant used an unbounded mpsc
    /// between the sync `try_publish` and the async send_loop; under
    /// 100K writes/s saturation the queue grew without bound while
    /// quinn's per-stream flow-control window held back the actual
    /// bytes. The `Variant` trait observed `Ok(true)` for writes that
    /// quinn had not yet sent, and `disconnect` dropped the leftover
    /// queue, surfacing as a delivery shortfall in the integrity
    /// report (quic-multi 1000x100hz qos4 stuck at ~86%).
    ///
    /// Post-T17.6 the channel is bounded at `RELIABLE_SEND_CHANNEL_BOUND`
    /// and `blocking_send` parks the sync caller when full; that
    /// back-pressure rides all the way to the driver, so writes only
    /// proceed at the rate quinn can actually push bytes. Delivery
    /// reaches 100% at the cost of throughput.
    ///
    /// The N below is intentionally chosen well above the channel
    /// bound so the burst hits the blocking path repeatedly without
    /// needing to also saturate quinn's per-stream window
    /// (untestable deterministically on loopback at unit-test scale).
    #[test]
    fn test_qos4_burst_delivers_every_message_t17_6() {
        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);
        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while variant_a.connections.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !variant_a.connections.is_empty(),
            "variant A never established an outbound connection to B"
        );

        // Burst far more messages than the bounded channel can hold.
        // RELIABLE_SEND_CHANNEL_BOUND is 256; using ~16x that exercises
        // the `blocking_send` path many times over.
        const N: u64 = 4096;
        let payload = vec![0xEEu8; 256];
        for seq in 0..N {
            let ok = variant_a
                .try_publish("/bench/0", &payload, Qos::ReliableTcp, seq)
                .expect("try_publish qos4 must not error under burst");
            assert!(
                ok,
                "try_publish at QoS 4 must never return Ok(false) (DESIGN.md § 6.5): seq {}",
                seq
            );
        }

        // Drain B's inbound channel. Disconnect A first so the
        // T17.6 drain path (channel close → finish → stopped) runs;
        // this is what carries the in-flight tail across the wire
        // before the test's drain loop starts looking.
        variant_a.disconnect().expect("disconnect a");
        let mut received: Vec<u64> = Vec::with_capacity(N as usize);
        let drain_deadline = std::time::Instant::now() + Duration::from_secs(20);
        while received.len() < N as usize && std::time::Instant::now() < drain_deadline {
            match variant_b.poll_receive() {
                Ok(Some(update)) => {
                    received.push(update.seq);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("poll_receive errored: {e}"),
            }
        }

        assert_eq!(
            received.len(),
            N as usize,
            "T17.6: every reliable write must be delivered; got {}/{}",
            received.len(),
            N,
        );

        // Ordering invariant (T16.10) preserved.
        let mut out_of_order = 0usize;
        for win in received.windows(2) {
            if win[1] <= win[0] {
                out_of_order += 1;
            }
        }
        assert_eq!(
            out_of_order, 0,
            "T16.10 ordering invariant: qos4 receives must be strictly ascending"
        );

        variant_b.disconnect().expect("disconnect b");
    }

    /// T17.6 mechanism check: `publish` MUST actually block when the
    /// bounded send channel is full. We construct the variant + a
    /// loopback peer that is then disconnected before the burst so its
    /// receive side stops draining. With the send_loop unable to push
    /// onto quinn's stream and no receiver, the bounded channel fills
    /// and a subsequent `publish` call blocks. We measure that the
    /// blocking call is slow enough to be observable (well above the
    /// fast-path latency of a non-blocking send).
    ///
    /// This is the "back-pressure reaches the sync side" assertion:
    /// pre-T17.6 the unbounded channel would happily accept this
    /// burst at memcpy-speed; post-T17.6 the bounded channel parks
    /// the writer.
    #[test]
    fn test_publish_blocks_when_channel_full_t17_6() {
        use std::time::Instant;

        let port_a = pick_free_udp_port();
        let port_b = pick_free_udp_port();
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();

        let mut variant_a = QuicVariant::new("a", addr_a, vec![addr_b]);
        let mut variant_b = QuicVariant::new("b", addr_b, vec![addr_a]);
        variant_b
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect b");
        std::thread::sleep(Duration::from_millis(200));
        variant_a
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect a");

        let deadline = Instant::now() + Duration::from_secs(5);
        while variant_a.connections.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !variant_a.connections.is_empty(),
            "variant A never established an outbound connection to B"
        );

        // Tear down B so its receive side stops draining. quinn's
        // per-stream flow control window then plateaus and A's
        // send_loop stalls inside `write_all`, so the bounded mpsc
        // saturates and subsequent `publish` calls block.
        variant_b.disconnect().expect("disconnect b");

        // 4 KiB payload * (bound + headroom) far exceeds quinn's
        // initial stream window. Beyond that the bounded channel
        // fills and `blocking_send` parks the caller.
        let payload = vec![0xAAu8; 4096];
        // Fast-path budget: any single send that does NOT block should
        // complete in well under this threshold. On Windows debug
        // builds an empty `blocking_send` is single-digit microseconds;
        // 250 ms is generous enough to avoid flakiness on a loaded CI
        // machine.
        let blocking_threshold = Duration::from_millis(250);

        let mut observed_block = false;
        // Cap the number of attempts: if the back-pressure mechanism
        // is correct we expect to see a block within the first few
        // thousand sends (channel fills, stream window drains, etc.).
        // 8x channel bound is a safe ceiling. Each iteration uses
        // try_publish so the test mirrors the driver's actual call
        // shape.
        for seq in 0..(RELIABLE_SEND_CHANNEL_BOUND as u64 * 8) {
            let start = Instant::now();
            let ok = variant_a
                .try_publish("/bench/0", &payload, Qos::ReliableTcp, seq)
                .expect("try_publish must not error");
            assert!(ok, "QoS 4 try_publish must return Ok(true)");
            if start.elapsed() >= blocking_threshold {
                observed_block = true;
                break;
            }
        }
        assert!(
            observed_block,
            "T17.6: with the receiver torn down and the channel bound at {}, \
             at least one publish() call must measurably block",
            RELIABLE_SEND_CHANNEL_BOUND
        );

        // Cleanly tear down A. The variant's disconnect path drops the
        // runtime and cancels the send_loop, so any still-queued
        // blocking_send calls would unblock with `send channel closed`
        // (none here; we only ran on this thread).
        variant_a.disconnect().expect("disconnect a");
    }

    /// Helper for the loopback tests: bind a UDP socket to an
    /// ephemeral port, read the assigned port, then drop the socket
    /// and return the port number. The window between drop and
    /// re-bind is small enough on loopback that collisions are not a
    /// concern in practice.
    fn pick_free_udp_port() -> u16 {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    }
}
