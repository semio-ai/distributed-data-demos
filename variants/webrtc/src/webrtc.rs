//! WebRTC DataChannel variant implementation.
//!
//! `WebRtcVariant` exposes the synchronous `Variant` trait surface and
//! drives an internal multi-threaded tokio runtime. The runtime hosts:
//!
//! - One `RTCPeerConnection` per peer (4 DataChannels multiplexed inside).
//! - A per-peer-pair TCP signaling task carrying SDP offer/answer +
//!   trickle ICE candidates (lower-sorted-name initiates, higher-sorted-
//!   name responds).
//! - A single `send_loop` task that serialises outbound writes onto the
//!   appropriate DataChannel based on the publish-time QoS.
//! - Per-DataChannel `on_message` callbacks that push decoded frames
//!   onto a single inbound mpsc channel which `poll_receive` /
//!   `poll_peer_eots` drain non-blockingly.
//!
//! ICE configuration is locked to host candidates only (no STUN, no
//! TURN, no mDNS) per the variant spec. The DataChannel options map
//! directly to the four QoS levels:
//!
//! - L1, L2: `ordered=false`, `max_retransmits=Some(0)` (best-effort).
//! - L3, L4: `ordered=true`, no retransmit limits (reliable + ordered).
//!
//! L2's "latest-value" semantics are layered on receive: the decoder
//! tracks the highest seen `seq` per `(writer, path)` for QoS 2 and
//! drops stale frames before they reach the driver.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_network::{EphemeralUDP, UDPNetwork};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::variant_trait::Variant;

/// Internal record of an observed peer EOT marker (T15.8 historical).
///
/// The on-wire EOT exchange was retired in T15.8. This struct stays so
/// the variant's internal channel plumbing compiles unchanged; the
/// receive path still decodes EOT control frames from pre-T15.8 peers,
/// but they are no longer surfaced to the driver.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PeerEot {
    writer: String,
    eot_id: u64,
}

use crate::pairing::{PairRole, PeerDesc};
use crate::protocol::{decode_frame, encode_data, Frame};
use crate::signaling::{read_frame, write_frame, SignalEnvelope};

/// Maximum time to wait for the full WebRTC connect (per peer): SDP
/// exchange, ICE gathering, DTLS handshake, and all four DataChannels
/// reaching the `open` state. On localhost this is fast (sub-second);
/// the timeout is a safety net for genuinely broken setups.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Time to wait for the responder side to bind its TCP signaling
/// listener before the initiator tries to connect. Avoids a tight
/// retry loop in the common case where both sides start at the same
/// instant.
const SIGNALING_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Backoff between TCP `connect` retries when the responder has not
/// yet bound the port.
const SIGNALING_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Channel labels -- one per QoS. The initiator creates these
/// labels; the responder accepts them via `on_data_channel`.
fn channel_label(qos: Qos) -> &'static str {
    match qos {
        Qos::BestEffort => "qos1-besteffort",
        Qos::LatestValue => "qos2-latest",
        Qos::ReliableUdp => "qos3-reliable",
        Qos::ReliableTcp => "qos4-reliable",
    }
}

fn label_to_qos(label: &str) -> Option<Qos> {
    match label {
        "qos1-besteffort" => Some(Qos::BestEffort),
        "qos2-latest" => Some(Qos::LatestValue),
        "qos3-reliable" => Some(Qos::ReliableUdp),
        "qos4-reliable" => Some(Qos::ReliableTcp),
        _ => None,
    }
}

/// DataChannelInit options for the four QoS levels.
fn channel_options(qos: Qos) -> RTCDataChannelInit {
    match qos {
        Qos::BestEffort | Qos::LatestValue => RTCDataChannelInit {
            ordered: Some(false),
            max_retransmits: Some(0),
            ..Default::default()
        },
        Qos::ReliableUdp | Qos::ReliableTcp => RTCDataChannelInit {
            ordered: Some(true),
            ..Default::default()
        },
    }
}

/// "Reliable" channel used for EOT regardless of the spawn's primary
/// QoS. Retained for historical reference (T15.8 removed the on-wire
/// EOT exchange).
#[allow(dead_code)]
const EOT_CHANNEL_QOS: Qos = Qos::ReliableTcp;

/// Soft upper bound on the per-(peer, qos) outbound queue depth, in
/// bytes, that `try_publish` consults for **unreliable** QoS levels
/// (1 and 2) under the T-impl.7 backpressure protocol.
///
/// The counter approximates `RTCDataChannel::buffered_amount()`: it
/// is incremented when a frame is enqueued for the per-peer send loop
/// and decremented after `dc.send` completes. Querying webrtc-rs's
/// async `buffered_amount` from `try_publish` would require entering
/// the runtime on every publish tick, which would dominate latency
/// measurements at our tick rates; the queued-bytes counter is a
/// strictly larger proxy (it counts bytes we have committed to the
/// pipeline but the SCTP layer has not yet acknowledged as sent).
///
/// 4 MiB absorbs short bursts (~40 ms at 100 K msg/s with 1 KiB
/// payloads) without unbounded growth. The check is a soft limit --
/// see the brief race with the increment-then-send sequence below.
const BACKPRESSURE_BYTES_THRESHOLD: usize = 4 * 1024 * 1024;

/// Bounded mpsc capacity for the per-(peer, qos) reliable send queues
/// (QoS 3 and QoS 4). Together with `blocking_send` from the sync
/// publish path this gives the application-visible back-pressure
/// signal required by DESIGN.md § 6.5 (the strict no-skip contract
/// for QoS 3/4): when the channel is full the `publish` caller blocks
/// until the send loop's `dc.send().await` has drained one slot, at
/// which point SCTP flow control on the wire becomes the effective
/// rate limit.
///
/// 64 messages is the smallest depth that comfortably absorbs a
/// single high-rate tick burst (the `1000x100hz` reproducer publishes
/// 1000 values per tick, but most are spread across paths and pumped
/// out under SCTP flow control before the next tick). Smaller depths
/// (1-8) gave dc.send sub-millisecond windows that the tokio scheduler
/// could not fill cheaply, dropping throughput further with no
/// delivery improvement. Larger depths (256+) defer back-pressure
/// past the point where the sync caller can meaningfully react and
/// pile up wall-clock latency for late-arriving frames.
const RELIABLE_CHANNEL_CAPACITY: usize = 64;

/// Bounded mpsc capacity for the per-(peer, qos) unreliable send
/// queues (QoS 1 and QoS 2). The same depth as the reliable side --
/// the unreliable path keeps using `try_send` so a saturated queue
/// surfaces as `Ok(false)` (a `backpressure_skipped` event), not a
/// block; the capacity is just a guard against the unbounded growth
/// that the previous `mpsc::unbounded_channel` permitted.
const UNRELIABLE_CHANNEL_CAPACITY: usize = 64;

/// Per-channel poll interval used by `disconnect` while waiting for
/// `RTCDataChannel::buffered_amount()` to reach zero on each reliable
/// channel. Short enough not to add noticeable latency at the tail
/// of a spawn, long enough not to peg a worker thread on the async
/// mutex inside `buffered_amount()`.
const BUFFERED_AMOUNT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Maximum wall-clock time `disconnect` is willing to wait for the
/// SCTP buffer on each reliable DataChannel to drain to zero before
/// closing the peer connection. Spawns running at the 1000x100hz
/// reproducer rate spend most of their tail draining 1-2 MiB of
/// in-flight SCTP frames; 5 seconds is comfortable for that workload
/// and well below the runner's `default_timeout_secs`.
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Inbound observation drained by the variant's poll methods.
#[derive(Debug)]
enum Inbound {
    Data(ReceivedUpdate),
    Eot(PeerEot),
}

/// Outbound message the send loop will deliver via its dedicated
/// DataChannel. The send loop is keyed by (peer, qos), so the message
/// itself no longer needs to carry that addressing.
struct OutboundMessage {
    /// Already-encoded wire bytes (data or EOT frame).
    data: Bytes,
    /// Pre-incremented "in flight" byte counter the send loop must
    /// decrement once `dc.send` completes. `None` for EOT frames and
    /// other paths that do not participate in `try_publish`
    /// backpressure accounting.
    inflight_counter: Option<Arc<AtomicUsize>>,
    /// Number of bytes the sender added to `inflight_counter` for
    /// this message. The send loop subtracts the same value back.
    /// Always 0 when `inflight_counter` is `None`.
    inflight_bytes: usize,
}

/// Per-peer DataChannel set keyed by QoS as `u8` (1..=4) since `Qos`
/// itself doesn't implement `Hash`.
type QosChannelMap = HashMap<u8, Arc<RTCDataChannel>>;
type PeerChannelMap = HashMap<String, QosChannelMap>;

/// Per-(peer, qos) in-flight byte counters used by `try_publish` for
/// the T-impl.7 backpressure check. Only the unreliable QoS levels
/// (1, 2) actually consult these; the entries are still populated for
/// 3/4 so the schema stays uniform, but reliable publishes never read
/// or guard on the counter (they unconditionally enqueue and await).
type InflightMap = HashMap<(String, u8), Arc<AtomicUsize>>;

/// Per-(peer, qos) bounded mpsc senders, one per outbound DataChannel.
/// One send loop task per entry drains its channel and serialises
/// `dc.send().await` onto the matching DataChannel. The separate
/// channel-per-QoS topology means a stalled reliable channel (QoS 3/4
/// under SCTP flow control) cannot head-of-line block the unreliable
/// send loops for the same peer.
type SendChannelMap = HashMap<(String, u8), mpsc::Sender<OutboundMessage>>;

/// Shutdown signal for background tasks.
type ShutdownTx = tokio::sync::watch::Sender<bool>;
type ShutdownRx = tokio::sync::watch::Receiver<bool>;

/// WebRTC variant struct. The trait surface stays sync; the runtime
/// is the asynchronous workhorse.
pub struct WebRtcVariant {
    runner: String,
    media_listen: SocketAddr,
    signaling_listen: SocketAddr,
    peers: Vec<PeerDesc>,

    runtime: Option<Runtime>,
    /// Per-(peer, qos) bounded mpsc senders. Reliable QoS 3/4
    /// `publish` calls block on `blocking_send` when the channel is
    /// full; unreliable QoS 1/2 publishes use `try_send` and surface
    /// channel-full as a soft skip (Ok(false)) via the existing
    /// inflight-byte threshold path.
    send_channels: SendChannelMap,
    /// Per-(peer, qos) DataChannel handles held alongside the
    /// `send_channels` map so `disconnect` can poll
    /// `buffered_amount()` on the reliable channels to drain SCTP's
    /// outbound buffer before closing the peer connection. Without
    /// this drain step the in-flight SCTP frames are lost on close
    /// even though `dc.send().await` already accepted them.
    peer_dcs: HashMap<(String, u8), Arc<RTCDataChannel>>,
    /// JoinHandles for the per-(peer, qos) send_loop tasks spawned in
    /// `connect`. `disconnect` awaits these (after dropping the
    /// matching sender so the receiver returns `None`) to know the
    /// mpsc is fully drained into `dc.send().await`. Only then can
    /// the SCTP `buffered_amount()` drain check meaningfully gate on
    /// "zero pending outbound bytes".
    send_loop_handles: Vec<tokio::task::JoinHandle<()>>,
    recv_rx: Option<mpsc::UnboundedReceiver<Inbound>>,
    shutdown_tx: Option<ShutdownTx>,
    /// Held alive for the lifetime of the variant so receive-side
    /// `on_message` callbacks keep working until disconnect.
    peer_connections: Vec<Arc<RTCPeerConnection>>,

    /// Per-(peer, qos) in-flight byte counters consulted by
    /// `try_publish` for the T-impl.7 backpressure protocol.
    inflight: InflightMap,

    /// Pending EOTs pulled from the inbound channel, awaiting drain
    /// by `poll_peer_eots`.
    pending_eots: Vec<PeerEot>,
    /// Pending data updates pulled from the inbound channel, awaiting
    /// drain by `poll_receive`.
    pending_data: std::collections::VecDeque<ReceivedUpdate>,
}

impl WebRtcVariant {
    /// Create a new WebRTC variant.
    pub fn new(
        runner: &str,
        signaling_listen: SocketAddr,
        media_listen: SocketAddr,
        peers: Vec<PeerDesc>,
    ) -> Self {
        Self {
            runner: runner.to_string(),
            media_listen,
            signaling_listen,
            peers,
            runtime: None,
            send_channels: HashMap::new(),
            peer_dcs: HashMap::new(),
            send_loop_handles: Vec::new(),
            recv_rx: None,
            shutdown_tx: None,
            peer_connections: Vec::new(),
            inflight: HashMap::new(),
            pending_eots: Vec::new(),
            pending_data: std::collections::VecDeque::new(),
        }
    }

    /// Drain the inbound channel into the per-kind side buffers.
    /// Idempotent and non-blocking.
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

/// Build an APIBuilder with host-only ICE: STUN / TURN / mDNS all
/// disabled, network type restricted to UDP4 (no TCP-ICE, no IPv6),
/// and the UDP socket pinned to the derived `media_port`.
fn build_api(media_port: u16) -> webrtc::api::API {
    let mut setting = SettingEngine::default();
    setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    setting.set_network_types(vec![NetworkType::Udp4]);
    // Pin the host candidate port: ephemeral with port_min == port_max.
    let ephemeral = EphemeralUDP::new(media_port, media_port)
        .expect("EphemeralUDP::new with port_min==port_max should not fail");
    setting.set_udp_network(UDPNetwork::Ephemeral(ephemeral));
    APIBuilder::new().with_setting_engine(setting).build()
}

/// Build a host-only `RTCConfiguration` with no ICE servers (no STUN,
/// no TURN).
fn build_rtc_config() -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    }
}

/// Wire an inbound DataChannel to the shared inbound pump: register an
/// `on_message` callback that decodes the frame and forwards to
/// `recv_tx`. QoS 2 is filtered for stale `seq` values via the per-
/// `(writer, path)` watermarks held in `latest_value`.
fn wire_data_channel(
    dc: Arc<RTCDataChannel>,
    recv_tx: mpsc::UnboundedSender<Inbound>,
    latest_value: Arc<Mutex<HashMap<(String, String), u64>>>,
    self_runner: String,
) {
    let label = dc.label().to_string();
    dc.on_message(Box::new(move |msg| {
        let recv_tx = recv_tx.clone();
        let latest_value = latest_value.clone();
        let label = label.clone();
        let self_runner = self_runner.clone();
        Box::pin(async move {
            let bytes: &[u8] = &msg.data;
            let frame = match decode_frame(bytes) {
                Ok(f) => f,
                Err(_) => return, // Ignore malformed frames silently.
            };
            match frame {
                Frame::Data(update) => {
                    // Defensive: ignore loopbacks of our own writes.
                    if update.writer == self_runner {
                        return;
                    }
                    // QoS 2 latest-value filter on the receiver: drop
                    // stale frames whose seq is below the watermark
                    // for that (writer, path). Only the QoS 2 channel
                    // applies this filter; the channel label is the
                    // discriminator so unreliable but correctly
                    // ordered QoS 2 traffic still benefits from the
                    // unordered transport's freshness semantics.
                    if update.qos == Qos::LatestValue {
                        let key = (update.writer.clone(), update.path.clone());
                        let mut map = latest_value.lock().await;
                        let cur = map.get(&key).copied().unwrap_or(0);
                        if update.seq < cur {
                            return;
                        }
                        map.insert(key, update.seq);
                    }
                    let _ = recv_tx.send(Inbound::Data(update));
                }
                Frame::Eot { writer, eot_id } => {
                    if writer == self_runner {
                        return;
                    }
                    let _ = recv_tx.send(Inbound::Eot(PeerEot { writer, eot_id }));
                }
            }
            // Touch the label so clippy doesn't complain about the
            // capture on builds where the variable is otherwise unused.
            let _ = label;
        })
    }));
}

/// Drive one peer pair: build the PeerConnection, run signaling
/// (initiator or responder), open / await the four DataChannels, and
/// register message handlers. Returns the open DataChannels keyed by
/// QoS so the send loop can dispatch onto them.
#[allow(clippy::too_many_arguments)]
async fn handle_peer_pair(
    peer: PeerDesc,
    self_runner: String,
    media_port: u16,
    signaling_listen: SocketAddr,
    recv_tx: mpsc::UnboundedSender<Inbound>,
    latest_value: Arc<Mutex<HashMap<(String, String), u64>>>,
) -> Result<(Arc<RTCPeerConnection>, QosChannelMap)> {
    let api = build_api(media_port);
    let pc = Arc::new(api.new_peer_connection(build_rtc_config()).await?);

    // Channel collecting locally-discovered ICE candidates so the
    // signaling task can trickle them out.
    let (local_cand_tx, local_cand_rx) = mpsc::unbounded_channel::<RTCIceCandidateInit>();
    let local_cand_tx_for_cb = local_cand_tx.clone();
    pc.on_ice_candidate(Box::new(move |c| {
        let local_cand_tx = local_cand_tx_for_cb.clone();
        Box::pin(async move {
            if let Some(c) = c {
                eprintln!(
                    "[webrtc] local ICE candidate: type={} addr={}:{} proto={}",
                    c.typ, c.address, c.port, c.protocol
                );
                if let Ok(init) = c.to_json() {
                    let _ = local_cand_tx.send(init);
                }
            }
        })
    }));

    // Surface peer-connection state transitions to the log -- mostly
    // diagnostic; the connect-completion barrier is the
    // DataChannel-open count below.
    let pc_state_peer = peer.name.clone();
    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let peer = pc_state_peer.clone();
        Box::pin(async move {
            eprintln!("[webrtc] peer={peer} connection state: {s:?}");
        })
    }));

    // Channel that fires when each DC reaches the Open state, keyed by
    // qos. We expect 4 entries (one per QoS).
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<(Qos, Arc<RTCDataChannel>)>();

    // For the responder, register on_data_channel BEFORE the SDP
    // exchange so we capture the four channels the initiator created.
    if peer.role == PairRole::Responder {
        let recv_tx_resp = recv_tx.clone();
        let latest_value_resp = latest_value.clone();
        let self_runner_resp = self_runner.clone();
        let open_tx_resp = open_tx.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let recv_tx = recv_tx_resp.clone();
            let latest_value = latest_value_resp.clone();
            let self_runner = self_runner_resp.clone();
            let open_tx = open_tx_resp.clone();
            Box::pin(async move {
                let label = dc.label().to_string();
                let qos = match label_to_qos(&label) {
                    Some(q) => q,
                    None => {
                        eprintln!("[webrtc] unknown DataChannel label: {label}");
                        return;
                    }
                };
                wire_data_channel(
                    dc.clone(),
                    recv_tx.clone(),
                    latest_value.clone(),
                    self_runner.clone(),
                );
                // Notify the open watcher: register an `on_open`
                // callback that forwards the DC into our open queue.
                let open_tx_inner = open_tx.clone();
                let dc_inner = dc.clone();
                let qos_inner = qos;
                if dc.ready_state() == RTCDataChannelState::Open {
                    let _ = open_tx_inner.send((qos_inner, dc_inner));
                } else {
                    dc.on_open(Box::new(move || {
                        let open_tx = open_tx_inner.clone();
                        let dc = dc_inner.clone();
                        let qos = qos_inner;
                        Box::pin(async move {
                            let _ = open_tx.send((qos, dc));
                        })
                    }));
                }
            })
        }));
    }

    // Initiator side: create the four DataChannels up-front.
    let mut initiator_channels: QosChannelMap = HashMap::new();
    if peer.role == PairRole::Initiator {
        for qos in [
            Qos::BestEffort,
            Qos::LatestValue,
            Qos::ReliableUdp,
            Qos::ReliableTcp,
        ] {
            let dc = pc
                .create_data_channel(channel_label(qos), Some(channel_options(qos)))
                .await?;
            wire_data_channel(
                dc.clone(),
                recv_tx.clone(),
                latest_value.clone(),
                self_runner.clone(),
            );
            let open_tx_i = open_tx.clone();
            let dc_i = dc.clone();
            let qos_i = qos;
            if dc.ready_state() == RTCDataChannelState::Open {
                let _ = open_tx_i.send((qos_i, dc_i));
            } else {
                dc.on_open(Box::new(move || {
                    let open_tx = open_tx_i.clone();
                    let dc = dc_i.clone();
                    let qos = qos_i;
                    Box::pin(async move {
                        let _ = open_tx.send((qos, dc));
                    })
                }));
            }
            initiator_channels.insert(qos.as_int(), dc);
        }
    }

    // Run the signaling exchange. The signaling task drains
    // `local_cand_rx` to forward our ICE candidates to the peer and
    // applies inbound candidates to the PeerConnection.
    run_signaling(
        pc.clone(),
        peer.clone(),
        signaling_listen,
        local_cand_rx,
        local_cand_tx,
    )
    .await
    .with_context(|| format!("signaling failed for peer {}", peer.name))?;

    // Wait for all four DataChannels to be open. The responder side
    // populates `open_rx` from `on_data_channel` + per-channel
    // `on_open`. The initiator side populates from per-channel
    // `on_open` registered above. Once all four QoS levels have been
    // observed open, return.
    let mut open: QosChannelMap = HashMap::new();
    let deadline_fut = tokio::time::sleep(CONNECT_TIMEOUT);
    tokio::pin!(deadline_fut);
    while open.len() < 4 {
        tokio::select! {
            biased;
            _ = &mut deadline_fut => {
                return Err(anyhow!(
                    "timed out waiting for DataChannels to open with peer {} (have {}/4)",
                    peer.name,
                    open.len()
                ));
            }
            msg = open_rx.recv() => {
                match msg {
                    Some((qos, dc)) => {
                        eprintln!("[webrtc] peer={} DataChannel open qos={}", peer.name, qos.as_int());
                        open.insert(qos.as_int(), dc);
                    }
                    None => return Err(anyhow!("open channel closed before all DataChannels opened")),
                }
            }
        }
    }

    Ok((pc, open))
}

/// Run the signaling exchange for one peer pair. The lower-sorted
/// runner connects and sends the offer; the higher-sorted runner
/// accepts and sends the answer. Trickle ICE candidates flow both ways
/// for the lifetime of the socket. The socket closes once both the
/// local and remote "done" envelopes have been processed (or the ICE
/// candidate stream has quiesced under a short timeout).
async fn run_signaling(
    pc: Arc<RTCPeerConnection>,
    peer: PeerDesc,
    signaling_listen: SocketAddr,
    local_cand_rx: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    _local_cand_tx: mpsc::UnboundedSender<RTCIceCandidateInit>,
) -> Result<()> {
    let stream = match peer.role {
        PairRole::Initiator => {
            // Connect with retries -- the responder may not be bound
            // yet at the moment we start.
            let deadline = tokio::time::Instant::now() + SIGNALING_CONNECT_TIMEOUT;
            loop {
                match TcpStream::connect(peer.signaling_addr).await {
                    Ok(s) => break s,
                    Err(e) => {
                        if tokio::time::Instant::now() >= deadline {
                            return Err(anyhow!(
                                "failed to connect to {} signaling at {}: {e}",
                                peer.name,
                                peer.signaling_addr
                            ));
                        }
                        tokio::time::sleep(SIGNALING_RETRY_BACKOFF).await;
                    }
                }
            }
        }
        PairRole::Responder => {
            // Bind + accept exactly one connection from the peer.
            let listener = TcpListener::bind(signaling_listen)
                .await
                .with_context(|| format!("bind signaling listener on {signaling_listen}"))?;
            // The accept timeout matches CONNECT_TIMEOUT to avoid
            // hanging if the peer never starts.
            let (stream, _addr) = timeout(CONNECT_TIMEOUT, listener.accept())
                .await
                .map_err(|_| anyhow!("timed out accepting signaling from peer {}", peer.name))?
                .with_context(|| format!("accept signaling from peer {}", peer.name))?;
            stream
        }
    };

    eprintln!(
        "[webrtc] signaling open to peer={} role={:?}",
        peer.name, peer.role
    );

    drive_signaling(pc, peer, stream, local_cand_rx).await
}

/// Drive the signaling exchange on an established TCP stream. Splits
/// the stream into reader / writer halves so the inbound envelope read
/// and the outbound ICE-candidate fan-out can run concurrently.
async fn drive_signaling(
    pc: Arc<RTCPeerConnection>,
    peer: PeerDesc,
    mut stream: TcpStream,
    mut local_cand_rx: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
) -> Result<()> {
    // Initiator: create + send offer right away, then await answer.
    // Responder: await offer, set remote, create + send answer.
    if peer.role == PairRole::Initiator {
        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        write_frame(
            &mut stream,
            &SignalEnvelope::Offer {
                sdp: offer.sdp.clone(),
            },
        )
        .await?;
    }

    let mut local_done_sent = false;
    let mut remote_done_seen = false;
    // Track whether SDP exchange is finished so we can stop the loop
    // once both "done" markers have flowed and there are no pending
    // local candidates to send.
    let mut sdp_exchanged = false;

    // Pre-loop: we need to read at least one envelope (offer for the
    // responder, answer for the initiator) before transitioning into
    // the trickle-ICE phase. Use the same loop body and let the state
    // flags drive completion.
    while !(remote_done_seen && local_done_sent && sdp_exchanged) {
        tokio::select! {
            biased;
            // Forward locally-gathered ICE candidates to the peer.
            cand = local_cand_rx.recv() => {
                match cand {
                    Some(init) => {
                        write_frame(
                            &mut stream,
                            &SignalEnvelope::Candidate {
                                candidate: init.candidate,
                                sdp_mid: init.sdp_mid,
                                sdp_mline_index: init.sdp_mline_index,
                            },
                        ).await?;
                    }
                    None => {
                        // The local candidate channel was closed; nothing
                        // more to send. If SDP is exchanged, we still
                        // need to honour the remote "done" before exit.
                        if sdp_exchanged && !local_done_sent {
                            write_frame(&mut stream, &SignalEnvelope::Done).await?;
                            local_done_sent = true;
                        }
                    }
                }
            }
            // Drain inbound envelopes.
            env = read_frame(&mut stream) => {
                let env = match env? {
                    Some(env) => env,
                    None => {
                        // Peer closed; treat as remote done.
                        remote_done_seen = true;
                        if !local_done_sent {
                            // Best-effort: try to send our own "done"
                            // before treating the connection as gone.
                            let _ = write_frame(&mut stream, &SignalEnvelope::Done).await;
                            local_done_sent = true;
                        }
                        continue;
                    }
                };
                match env {
                    SignalEnvelope::Offer { sdp } => {
                        if peer.role != PairRole::Responder {
                            return Err(anyhow!("initiator received unexpected Offer"));
                        }
                        let desc = RTCSessionDescription::offer(sdp)?;
                        pc.set_remote_description(desc).await?;
                        let answer = pc.create_answer(None).await?;
                        pc.set_local_description(answer.clone()).await?;
                        write_frame(
                            &mut stream,
                            &SignalEnvelope::Answer { sdp: answer.sdp.clone() },
                        ).await?;
                        sdp_exchanged = true;
                    }
                    SignalEnvelope::Answer { sdp } => {
                        if peer.role != PairRole::Initiator {
                            return Err(anyhow!("responder received unexpected Answer"));
                        }
                        let desc = RTCSessionDescription::answer(sdp)?;
                        pc.set_remote_description(desc).await?;
                        sdp_exchanged = true;
                    }
                    SignalEnvelope::Candidate {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    } => {
                        eprintln!(
                            "[webrtc] remote ICE candidate from peer={}: {}",
                            peer.name, candidate
                        );
                        let init = RTCIceCandidateInit {
                            candidate,
                            sdp_mid,
                            sdp_mline_index,
                            username_fragment: None,
                        };
                        if let Err(e) = pc.add_ice_candidate(init).await {
                            eprintln!(
                                "[webrtc] add_ice_candidate failed for peer={}: {e}",
                                peer.name
                            );
                        }
                    }
                    SignalEnvelope::Done => {
                        remote_done_seen = true;
                        if !local_done_sent && sdp_exchanged {
                            write_frame(&mut stream, &SignalEnvelope::Done).await?;
                            local_done_sent = true;
                        }
                    }
                }
            }
            // After SDP is exchanged, allow ourselves to send Done once
            // local candidate gathering has settled. We use a short
            // grace timer rather than waiting for `null` from
            // `on_ice_candidate` so we don't block on driver behaviour
            // we can't fully control.
            _ = tokio::time::sleep(Duration::from_millis(500)), if sdp_exchanged && !local_done_sent => {
                write_frame(&mut stream, &SignalEnvelope::Done).await?;
                local_done_sent = true;
            }
        }
    }

    eprintln!("[webrtc] signaling complete with peer={}", peer.name);
    Ok(())
}

/// Background send loop driving exactly one DataChannel for one
/// (peer, qos) pair.
///
/// The function is spawned once per (peer, qos) entry in the variant's
/// send-channel map, which gives every DataChannel its own ordered
/// drain. For reliable QoS (3/4) `dc.send().await` participates in
/// SCTP per-stream flow control: when the peer's reassembly window
/// is full the await blocks, the bounded inbound mpsc fills up, and
/// the next `publish` call on the sync side blocks on
/// `blocking_send`. This is the back-pressure chain that satisfies
/// DESIGN.md § 6.5's strict no-skip contract.
///
/// For unreliable QoS (1/2) the `dc.send` future completes promptly
/// (SCTP does not retransmit), so the channel rarely backs up; the
/// bounded channel acts as a safety cap and `try_publish`'s
/// inflight-byte threshold delivers the contractual `Ok(false)` skip
/// before the channel itself fills.
///
/// For each message, after `dc.send` resolves (success or error), the
/// loop decrements the per-(peer, qos) in-flight byte counter by
/// `inflight_bytes`. The counter stays consistent with the queue
/// depth observed from the sender's side even when SCTP eventually
/// reports a failure.
async fn send_loop_for_channel(
    mut rx: mpsc::Receiver<OutboundMessage>,
    dc: Arc<RTCDataChannel>,
    peer_name: String,
    qos: u8,
    mut shutdown_rx: ShutdownRx,
) {
    loop {
        // Pull the next message and dispatch it onto the wire. The
        // bounded mpsc + `blocking_send` chain on the sync side
        // already back-pressures the publish caller; `dc.send().await`
        // participates in SCTP per-stream flow control internally,
        // so SCTP's reassembly window is the effective second-stage
        // limit. The drain-on-disconnect step (`disconnect` stage 3)
        // ensures any frames still in SCTP's outbound buffer at
        // end-of-spawn make it to the wire before the peer
        // connection closes -- which was the 70% delivery cliff that
        // remained when the bounded mpsc alone was the only
        // back-pressure layer.
        let m = tokio::select! {
            msg = rx.recv() => msg,
            _ = shutdown_rx.changed() => break,
        };
        let Some(m) = m else {
            break;
        };

        let bytes: bytes::Bytes = m.data.clone();
        if let Err(e) = dc.send(&bytes).await {
            eprintln!("[webrtc] send to peer={peer_name} qos={qos} failed: {e}");
        }
        if let Some(c) = m.inflight_counter.as_ref() {
            c.fetch_sub(m.inflight_bytes, Ordering::Relaxed);
        }
    }
}

impl Variant for WebRtcVariant {
    fn name(&self) -> &str {
        "webrtc"
    }

    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        // T14.6: webrtc-rs is fundamentally async and brings its own
        // task pool (DTLS handshake, SCTP timers, ICE state machine,
        // per-DataChannel on_message callbacks). A sync single-threaded
        // WebRTC client would be a major rewrite of the upstream crate
        // and defeats the point of benchmarking the off-the-shelf
        // stack. We declare Multi only; `connect(Single)` errors
        // before any I/O. See `variants/webrtc/CUSTOM.md` "Threading
        // modes (T14.6)".
        &[ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        // T14.6: reject Single mode BEFORE any I/O. Capability is
        // declared via `supported_threading_modes()`; this is the
        // belt-and-braces guard for the case the runner asks anyway.
        if threading_mode == ThreadingMode::Single {
            anyhow::bail!(
                "variant-webrtc does not support single-threaded mode \
                 (webrtc-rs requires async + task pool); spawn with --threading-mode multi"
            );
        }
        // T17.7: bumped from 2 to 4 worker threads. Each peer now has
        // four dedicated `send_loop_for_channel` tasks (one per QoS)
        // in addition to webrtc-rs's internal task pool. While
        // `dc.send().await` cooperatively yields rather than blocking
        // a worker, the SCTP / DTLS internals can briefly hold a
        // worker during retransmit windows; four workers gives the
        // scheduler enough room without dominating the CPU footprint
        // of the variant.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_io()
            .enable_time()
            .build()
            .context("build tokio runtime")?;

        let (recv_tx, recv_rx) = mpsc::unbounded_channel::<Inbound>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let media_port = self.media_listen.port();
        let signaling_listen = self.signaling_listen;
        let self_runner = self.runner.clone();
        let peers = self.peers.clone();

        // Per-(writer,path) latest-seq watermark for the receiver-side
        // QoS 2 filter. Shared across all DataChannels.
        let latest_value: Arc<Mutex<HashMap<(String, String), u64>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Build all peer connections concurrently inside the runtime.
        // Each peer pair has its own signaling task. NOTE: webrtc-rs
        // ties one PeerConnection to one UDP socket via the SettingEngine,
        // and we want host-only candidates pinned to `media_port`. With
        // a single port per peer, two peers on the same runner cannot
        // share a socket -- but our spawn-per-QoS / per-pair model
        // already gives each pair a disjoint media_port range, and on
        // the two-runner case there is exactly one peer.
        if self.peers.len() > 1 {
            return Err(anyhow!(
                "this variant supports a single peer per spawn (host candidate \
                 port pinning is per-PeerConnection); got {} peers",
                self.peers.len()
            ));
        }

        let connect_result: Result<Vec<(Arc<RTCPeerConnection>, PeerDesc, QosChannelMap)>> =
            runtime.block_on(async {
                let mut results = Vec::with_capacity(peers.len());
                for peer in peers {
                    let (pc, channels) = handle_peer_pair(
                        peer.clone(),
                        self_runner.clone(),
                        media_port,
                        signaling_listen,
                        recv_tx.clone(),
                        latest_value.clone(),
                    )
                    .await?;
                    results.push((pc, peer, channels));
                }
                Ok(results)
            });

        let connected = connect_result?;

        // Build a peer-channel map for the send loop.
        let mut peer_channels: PeerChannelMap = HashMap::new();
        let mut pcs: Vec<Arc<RTCPeerConnection>> = Vec::new();
        for (pc, peer, channels) in connected {
            peer_channels.insert(peer.name.clone(), channels);
            pcs.push(pc);
        }

        // Populate the per-(peer, qos) in-flight byte counters. We
        // create one counter for every (peer, qos) pair regardless of
        // whether that QoS will actually use it -- `try_publish` only
        // consults the counter for QoS 1/2, but having a uniform map
        // keeps the send loop's accounting trivial.
        let mut inflight: InflightMap = HashMap::new();
        for peer_name in peer_channels.keys() {
            for qos in [
                Qos::BestEffort,
                Qos::LatestValue,
                Qos::ReliableUdp,
                Qos::ReliableTcp,
            ] {
                inflight.insert(
                    (peer_name.clone(), qos.as_int()),
                    Arc::new(AtomicUsize::new(0)),
                );
            }
        }

        // Spawn one send loop per (peer, qos) DataChannel. Each loop
        // owns the receiver half of a bounded mpsc; the variant holds
        // the matching sender in `send_channels`. The per-channel
        // topology means a reliable QoS that has back-pressured down
        // to a stalled `dc.send().await` cannot stall the unreliable
        // QoS loops for the same peer.
        let mut send_channels: SendChannelMap = HashMap::new();
        let mut peer_dcs: HashMap<(String, u8), Arc<RTCDataChannel>> = HashMap::new();
        let mut send_loop_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for (peer_name, channels) in &peer_channels {
            for (qos_int, dc) in channels {
                let capacity = match Qos::from_int(*qos_int) {
                    Some(Qos::ReliableUdp) | Some(Qos::ReliableTcp) => RELIABLE_CHANNEL_CAPACITY,
                    _ => UNRELIABLE_CHANNEL_CAPACITY,
                };
                let (tx, rx) = mpsc::channel::<OutboundMessage>(capacity);
                send_channels.insert((peer_name.clone(), *qos_int), tx);
                peer_dcs.insert((peer_name.clone(), *qos_int), dc.clone());

                let dc_clone = dc.clone();
                let peer_name_owned = peer_name.clone();
                let qos_owned = *qos_int;
                let shutdown_for_loop = shutdown_rx.clone();
                let handle = runtime.spawn(async move {
                    send_loop_for_channel(
                        rx,
                        dc_clone,
                        peer_name_owned,
                        qos_owned,
                        shutdown_for_loop,
                    )
                    .await;
                });
                send_loop_handles.push(handle);
            }
        }

        self.send_channels = send_channels;
        self.peer_dcs = peer_dcs;
        self.send_loop_handles = send_loop_handles;
        self.recv_rx = Some(recv_rx);
        self.shutdown_tx = Some(shutdown_tx);
        self.peer_connections = pcs;
        self.inflight = inflight;
        self.runtime = Some(runtime);

        Ok(())
    }

    /// Blocking publish.
    ///
    /// Encodes the frame once and dispatches it onto each peer's
    /// per-(peer, qos) bounded mpsc. For **reliable QoS** (3/4) the
    /// dispatch uses `blocking_send`, which is the synchronous
    /// back-pressure pipe required by DESIGN.md § 6.5: when the
    /// channel is full (because the send loop's `dc.send().await` is
    /// stalled inside SCTP flow control) the caller blocks until the
    /// send loop drains a slot. For **unreliable QoS** (1/2) the
    /// dispatch uses `try_send` and silently drops on `Full` -- the
    /// `try_publish` path above this normally catches saturation via
    /// the inflight-byte threshold and returns `Ok(false)` before we
    /// ever reach `publish`, so a `Full` here is the rare overflow
    /// case where the soft threshold under-estimated the depth; it is
    /// preferable to drop a single unreliable frame than block the
    /// sync caller (best-effort skipping is the contractual QoS 1/2
    /// behaviour).
    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        // No peers (loopback / single-runner config) is a valid
        // connected state; the for-loop below is a no-op and we
        // return Ok. Missing per-(peer, qos) channel entries are
        // silently skipped too, matching the pre-T17.7 send_loop
        // behaviour for unknown peers.
        let bytes = encode_data(qos, seq, path, &self.runner, payload);
        let inflight_bytes = bytes.len();
        let data = Bytes::from(bytes);
        let reliable = matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp);
        for peer in &self.peers {
            let key = (peer.name.clone(), qos.as_int());
            let Some(tx) = self.send_channels.get(&key) else {
                continue;
            };
            let counter = self.inflight.get(&key).cloned();
            if let Some(c) = counter.as_ref() {
                c.fetch_add(inflight_bytes, Ordering::Relaxed);
            }
            let msg = OutboundMessage {
                data: data.clone(),
                inflight_counter: counter.clone(),
                inflight_bytes,
            };
            let send_result = if reliable {
                // Block the sync caller until the send loop drains a
                // slot. This is the DESIGN.md § 6.5 strict no-skip
                // chain: bounded channel -> blocking_send ->
                // dc.send().await -> SCTP flow control.
                tx.blocking_send(msg)
                    .map_err(|_| anyhow!("send channel closed"))
            } else {
                // Best-effort fan-out for QoS 1/2: if the bounded
                // channel happens to be full (`try_publish`'s
                // threshold should have caught this earlier), drop
                // the frame and roll back the inflight counter rather
                // than blocking.
                match tx.try_send(msg) {
                    Ok(()) => Ok(()),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        if let Some(c) = counter.as_ref() {
                            c.fetch_sub(inflight_bytes, Ordering::Relaxed);
                        }
                        Ok(())
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        Err(anyhow!("send channel closed"))
                    }
                }
            };
            send_result?;
        }
        Ok(())
    }

    /// Non-blocking publish with backpressure detection (T-impl.7 +
    /// T17.7).
    ///
    /// For **unreliable QoS** (1 best-effort, 2 latest-value): consult
    /// the per-(peer, qos) in-flight byte counter -- if any target
    /// peer's pipeline already holds more than
    /// `BACKPRESSURE_BYTES_THRESHOLD` queued bytes, return `Ok(false)`
    /// without enqueuing anything. The driver logs a
    /// `backpressure_skipped` event and the caller moves on; the value
    /// is **not** delivered to any peer for this seq. The threshold
    /// check applies symmetrically across peers (skip if any one is
    /// backpressured) so unreliable channels do not silently fan out
    /// to only a subset of peers and create asymmetric loss.
    ///
    /// For **reliable QoS** (3, 4): delegate to `publish` and return
    /// `Ok(true)`. `publish` itself blocks on the bounded send
    /// channel's `blocking_send` (T17.7), which propagates SCTP
    /// per-stream flow control all the way to the sync caller. The
    /// strict-delivery contract in DESIGN.md § 6.5 forbids returning
    /// `Ok(false)` here because it would create a receiver-visible
    /// seq gap; the contract instead trades throughput for delivery,
    /// and the wall-clock time spent inside `blocking_send` is the
    /// honest mechanism.
    ///
    /// Note: there is a brief race between checking the counter and
    /// the subsequent increment-then-enqueue sequence. That's by
    /// design -- the threshold is a soft limit and a few extra
    /// messages in flight when we tip over the threshold are
    /// acceptable.
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                // Backpressure check: if any peer's queue is already
                // over the threshold for this QoS, skip the value.
                for peer in &self.peers {
                    if let Some(c) = self.inflight.get(&(peer.name.clone(), qos.as_int())) {
                        if c.load(Ordering::Relaxed) > BACKPRESSURE_BYTES_THRESHOLD {
                            return Ok(false);
                        }
                    }
                }
                self.publish(path, payload, qos, seq)?;
                Ok(true)
            }
            Qos::ReliableUdp | Qos::ReliableTcp => {
                // Reliable QoS: never produce a receiver-visible gap.
                self.publish(path, payload, qos, seq)?;
                Ok(true)
            }
        }
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        self.pump_inbound();
        if let Some(update) = self.pending_data.pop_front() {
            return Ok(Some(update));
        }
        Ok(None)
    }

    fn disconnect(&mut self) -> Result<()> {
        // T17.7 strict-delivery drain protocol. Order matters:
        //
        //   1. Drop every send-channel sender so the bounded mpsc
        //      receivers in the send loops will return `None` once
        //      they have drained the queued messages into
        //      `dc.send().await`.
        //   2. Await every send-loop JoinHandle so we KNOW each loop
        //      has finished and the mpsc is empty.
        //   3. Poll `dc.buffered_amount()` on each reliable
        //      DataChannel until it reaches zero (or DRAIN_DEADLINE
        //      expires). At this point the only writes still in
        //      flight are SCTP-level bytes that have been accepted by
        //      the kernel and must drain to the wire.
        //   4. Signal shutdown, close peer connections, tear down
        //      the runtime.
        //
        // Stages 1+2 together guarantee that we are NOT racing the
        // send loops to the buffered_amount check: any time we see
        // `buffered_amount == 0` after stage 2 it means the whole
        // pipeline (mpsc -> dc.send -> SCTP) is drained.
        self.send_channels.clear();
        let handles = std::mem::take(&mut self.send_loop_handles);
        let reliable_dcs: Vec<Arc<RTCDataChannel>> = self
            .peer_dcs
            .iter()
            .filter(|((_, qos), _)| {
                matches!(
                    Qos::from_int(*qos),
                    Some(Qos::ReliableUdp) | Some(Qos::ReliableTcp)
                )
            })
            .map(|(_, dc)| dc.clone())
            .collect();
        if let Some(rt) = self.runtime.as_ref() {
            rt.block_on(async {
                // Stage 2: wait for every send loop to finish, up to
                // DRAIN_DEADLINE. With the senders dropped above,
                // each `rx.recv()` returns `None` after the queue
                // empties and the loop exits. If SCTP is wedged
                // (peer disconnected mid-spawn, etc.) the in-flight
                // `dc.send().await` may never complete; the timeout
                // lets us continue to stage 3 + close the peer
                // connection rather than hang the whole `disconnect`
                // call.
                let stage2_deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
                for h in handles {
                    let now = tokio::time::Instant::now();
                    if now >= stage2_deadline {
                        h.abort();
                        continue;
                    }
                    let remaining = stage2_deadline.saturating_duration_since(now);
                    if tokio::time::timeout(remaining, h).await.is_err() {
                        // Send loop never finished within the
                        // window; we move on. Aborting here is
                        // unnecessary because `runtime.shutdown_timeout`
                        // below will tear it down.
                    }
                }
                // Stage 3: drain SCTP. With the send loops finished
                // (or aborted) we cannot add any more bytes to
                // `buffered_amount`, so once it hits zero it stays
                // zero.
                let stage3_deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
                for dc in &reliable_dcs {
                    while tokio::time::Instant::now() < stage3_deadline {
                        if dc.buffered_amount().await == 0 {
                            break;
                        }
                        tokio::time::sleep(BUFFERED_AMOUNT_POLL_INTERVAL).await;
                    }
                }
            });
        }
        self.peer_dcs.clear();

        // Stage 4: signal shutdown so any remaining background
        // tasks exit, then close the peer connections.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        self.recv_rx.take();
        self.inflight.clear();

        if let Some(rt) = self.runtime.as_ref() {
            let pcs: Vec<Arc<RTCPeerConnection>> = std::mem::take(&mut self.peer_connections);
            rt.block_on(async {
                for pc in pcs {
                    let _ = pc.close().await;
                }
            });
        }

        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        Ok(())
    }

    // T15.8: signal_end_of_test / poll_peer_eots removed from the trait.
    // The on-wire EOT exchange is no longer used; the inbound EOT
    // routing in `pump_inbound` stays so pre-T15.8 peers that still
    // emit EOT frames are tolerated without parser errors.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_options_qos1_unordered_no_retransmit() {
        let opts = channel_options(Qos::BestEffort);
        assert_eq!(opts.ordered, Some(false));
        assert_eq!(opts.max_retransmits, Some(0));
    }

    #[test]
    fn channel_options_qos2_unordered_no_retransmit() {
        let opts = channel_options(Qos::LatestValue);
        assert_eq!(opts.ordered, Some(false));
        assert_eq!(opts.max_retransmits, Some(0));
    }

    #[test]
    fn channel_options_qos3_ordered_default_reliable() {
        let opts = channel_options(Qos::ReliableUdp);
        assert_eq!(opts.ordered, Some(true));
        assert_eq!(opts.max_retransmits, None);
        assert_eq!(opts.max_packet_life_time, None);
    }

    #[test]
    fn channel_options_qos4_ordered_default_reliable() {
        let opts = channel_options(Qos::ReliableTcp);
        assert_eq!(opts.ordered, Some(true));
        assert_eq!(opts.max_retransmits, None);
        assert_eq!(opts.max_packet_life_time, None);
    }

    #[test]
    fn label_qos_bijection() {
        for qos in [
            Qos::BestEffort,
            Qos::LatestValue,
            Qos::ReliableUdp,
            Qos::ReliableTcp,
        ] {
            let label = channel_label(qos);
            assert_eq!(label_to_qos(label), Some(qos));
        }
        assert_eq!(label_to_qos("unknown"), None);
    }

    /// T14.6: WebRTC declares Multi-only support. webrtc-rs brings its
    /// own task pool; Single mode is not honourable.
    #[test]
    fn test_supported_threading_modes_is_multi_only() {
        let signaling = SocketAddr::from(([127, 0, 0, 1], 0));
        let media = SocketAddr::from(([127, 0, 0, 1], 0));
        let v = WebRtcVariant::new("self", signaling, media, vec![]);
        let modes = v.supported_threading_modes();
        assert_eq!(modes, &[ThreadingMode::Multi]);
    }

    /// T14.6: `connect(Single)` must error BEFORE any I/O. The guard
    /// must short-circuit before the variant builds its tokio runtime
    /// or opens any sockets. We assert both the Err outcome and that
    /// no runtime / channels / peer connections were stashed on
    /// `self`, which is the structural sign that no I/O happened.
    #[test]
    fn test_connect_single_mode_errors_before_io() {
        let signaling = SocketAddr::from(([127, 0, 0, 1], 0));
        let media = SocketAddr::from(([127, 0, 0, 1], 0));
        let mut v = WebRtcVariant::new("self", signaling, media, vec![]);
        let err = v
            .connect(ThreadingMode::Single)
            .expect_err("connect(Single) must error for variant-webrtc");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not support single-threaded mode"),
            "error message should explain Single is unsupported, got: {msg}",
        );
        assert!(
            msg.contains("--threading-mode multi"),
            "error message should point at the multi flag, got: {msg}",
        );
        assert!(v.runtime.is_none());
        assert!(v.send_channels.is_empty());
        assert!(v.recv_rx.is_none());
        assert!(v.shutdown_tx.is_none());
        assert!(v.peer_connections.is_empty());
    }

    #[test]
    fn build_api_does_not_panic() {
        // Smoke test: building the API with our settings should never
        // panic. Use 0 to grab any free port (the EphemeralUDP itself
        // doesn't open a socket; the PeerConnection will when ICE
        // gathers, which we don't do here).
        let _api = build_api(0);
    }

    #[test]
    fn build_rtc_config_has_no_ice_servers() {
        let cfg = build_rtc_config();
        assert!(cfg.ice_servers.is_empty());
    }

    // ---------------- T-impl.7: try_publish backpressure ----------------

    /// Build a `WebRtcVariant` in a "connected-shape" state without
    /// actually opening any PeerConnections: per-(peer, qos) bounded
    /// send channels whose receivers the test owns, the per-(peer,
    /// qos) in-flight counters populated, and one `PeerDesc` in
    /// `self.peers` so `publish` / `try_publish` know who to send to.
    /// The DataChannel side is bypassed -- the test asserts on what
    /// `try_publish` queues (or doesn't queue), not what the wire
    /// sees.
    type TestHarness = (
        WebRtcVariant,
        HashMap<(String, u8), mpsc::Receiver<OutboundMessage>>,
        HashMap<(String, u8), Arc<AtomicUsize>>,
    );

    fn build_test_variant_with_peer(peer_name: &str) -> TestHarness {
        let signaling = SocketAddr::from(([127, 0, 0, 1], 0));
        let media = SocketAddr::from(([127, 0, 0, 1], 0));
        let peers = vec![PeerDesc {
            name: peer_name.to_string(),
            signaling_addr: signaling,
            media_addr: media,
            role: PairRole::Initiator,
        }];
        let mut variant = WebRtcVariant::new("self", signaling, media, peers);
        let mut inflight: HashMap<(String, u8), Arc<AtomicUsize>> = HashMap::new();
        let mut send_channels: SendChannelMap = HashMap::new();
        let mut receivers: HashMap<(String, u8), mpsc::Receiver<OutboundMessage>> = HashMap::new();
        for qos in [
            Qos::BestEffort,
            Qos::LatestValue,
            Qos::ReliableUdp,
            Qos::ReliableTcp,
        ] {
            inflight.insert(
                (peer_name.to_string(), qos.as_int()),
                Arc::new(AtomicUsize::new(0)),
            );
            let capacity = match qos {
                Qos::ReliableUdp | Qos::ReliableTcp => RELIABLE_CHANNEL_CAPACITY,
                _ => UNRELIABLE_CHANNEL_CAPACITY,
            };
            let (tx, rx) = mpsc::channel::<OutboundMessage>(capacity);
            send_channels.insert((peer_name.to_string(), qos.as_int()), tx);
            receivers.insert((peer_name.to_string(), qos.as_int()), rx);
        }
        variant.inflight = inflight.clone();
        variant.send_channels = send_channels;
        (variant, receivers, inflight)
    }

    /// Helper: borrow the per-(peer, qos) receiver in the test
    /// harness.
    fn rx_for<'a>(
        rxs: &'a mut HashMap<(String, u8), mpsc::Receiver<OutboundMessage>>,
        peer: &str,
        qos: Qos,
    ) -> &'a mut mpsc::Receiver<OutboundMessage> {
        rxs.get_mut(&(peer.to_string(), qos.as_int()))
            .expect("test harness must have a receiver for every (peer, qos)")
    }

    #[test]
    fn try_publish_qos1_returns_false_when_buffer_over_threshold() {
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        // Pre-load the counter to just over the threshold.
        inflight
            .get(&("bob".to_string(), Qos::BestEffort.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD + 1, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::BestEffort, 42)
            .expect("try_publish must not error");
        assert!(
            !result,
            "QoS 1 over threshold must return Ok(false) (backpressured)"
        );
        // No message should have been enqueued for the send loop.
        assert!(
            rx_for(&mut rxs, "bob", Qos::BestEffort).try_recv().is_err(),
            "no OutboundMessage should be queued when try_publish returns Ok(false)"
        );
    }

    #[test]
    fn try_publish_qos2_returns_false_when_buffer_over_threshold() {
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::LatestValue.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD + 1, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::LatestValue, 42)
            .expect("try_publish must not error");
        assert!(!result, "QoS 2 over threshold must return Ok(false)");
        assert!(rx_for(&mut rxs, "bob", Qos::LatestValue)
            .try_recv()
            .is_err());
    }

    #[test]
    fn try_publish_qos1_returns_true_below_threshold() {
        let (mut v, mut rxs, _inflight) = build_test_variant_with_peer("bob");
        // Counter starts at zero -- well below the threshold.
        let result = v
            .try_publish("/p", &[0u8; 8], Qos::BestEffort, 1)
            .expect("try_publish must not error");
        assert!(result, "QoS 1 below threshold must return Ok(true)");
        // Exactly one OutboundMessage must have been queued on the
        // matching (peer, qos) channel.
        let rx = rx_for(&mut rxs, "bob", Qos::BestEffort);
        let msg = rx
            .try_recv()
            .expect("a single OutboundMessage should be queued");
        assert!(msg.inflight_bytes > 0);
        assert!(rx.try_recv().is_err(), "exactly one queued message");
    }

    #[test]
    fn try_publish_qos3_returns_true_even_when_arbitrary_counter_set() {
        // Reliable QoS must NEVER return Ok(false) -- the receiver-
        // visible gap would corrupt ordering / completeness. The
        // inflight counter is ignored by the QoS 3/4 try_publish
        // path; back-pressure for these channels surfaces as wall-
        // clock blocking inside `publish`'s `blocking_send` (T17.7).
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::ReliableUdp.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD * 10, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::ReliableUdp, 1)
            .expect("try_publish must not error");
        assert!(
            result,
            "QoS 3 must always return Ok(true), even under pressure"
        );
        let _msg = rx_for(&mut rxs, "bob", Qos::ReliableUdp)
            .try_recv()
            .expect("message must be queued for QoS 3");
    }

    #[test]
    fn try_publish_qos4_returns_true_even_when_arbitrary_counter_set() {
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::ReliableTcp.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD * 10, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::ReliableTcp, 1)
            .expect("try_publish must not error");
        assert!(
            result,
            "QoS 4 must always return Ok(true), even under pressure"
        );
        let _msg = rx_for(&mut rxs, "bob", Qos::ReliableTcp)
            .try_recv()
            .expect("message must be queued for QoS 4");
    }

    #[test]
    fn try_publish_qos1_at_exact_threshold_still_sends() {
        // Threshold check uses strict greater-than so a counter sitting
        // exactly at the threshold should still allow a send. This
        // documents the boundary in the contract.
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::BestEffort.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::BestEffort, 1)
            .expect("try_publish must not error");
        assert!(result, "exactly-at-threshold should still send (strict >)");
        let _msg = rx_for(&mut rxs, "bob", Qos::BestEffort)
            .try_recv()
            .expect("message must be queued at exact threshold");
    }

    #[test]
    fn try_publish_qos1_increments_then_send_loop_decrements_counter() {
        // Verify the accounting plumbing: queue a message, observe the
        // counter has grown by the inflight_bytes value carried on the
        // OutboundMessage. Simulate the send_loop's decrement and
        // verify it returns to zero. This validates the round-trip
        // bookkeeping that keeps the threshold check honest.
        let (mut v, mut rxs, inflight) = build_test_variant_with_peer("bob");
        let counter = inflight
            .get(&("bob".to_string(), Qos::BestEffort.as_int()))
            .unwrap()
            .clone();
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        v.try_publish("/p", &[0u8; 16], Qos::BestEffort, 1).unwrap();
        let queued_value = counter.load(Ordering::Relaxed);
        assert!(queued_value > 0, "counter should have been incremented");

        let msg = rx_for(&mut rxs, "bob", Qos::BestEffort)
            .try_recv()
            .expect("a message should be queued");
        assert_eq!(msg.inflight_bytes, queued_value);
        assert!(msg.inflight_counter.is_some());

        // Simulate the send_loop's decrement after dc.send completes.
        msg.inflight_counter
            .as_ref()
            .unwrap()
            .fetch_sub(msg.inflight_bytes, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "counter must return to zero after the send loop drains the queue"
        );
    }

    /// T17.7: For reliable QoS (3/4), `publish` MUST block the sync
    /// caller when the per-(peer, qos) bounded channel is full. This
    /// is the back-pressure contract that propagates SCTP per-stream
    /// flow control all the way to the application.
    ///
    /// We fill the bounded channel to its capacity (no send loop is
    /// running so nothing drains), spawn a thread that calls
    /// `publish` for a single extra reliable frame, and verify:
    ///   1. The thread does NOT complete within a generous wall-clock
    ///      window (it is correctly blocked on `blocking_send`).
    ///   2. As soon as the test drains one slot from the bounded
    ///      channel, the thread unblocks and `publish` returns Ok.
    #[test]
    fn publish_qos3_blocks_when_bounded_channel_full() {
        let (mut v, mut rxs, _inflight) = build_test_variant_with_peer("bob");

        // Fill the QoS-3 channel right up to capacity.
        for seq in 0..(RELIABLE_CHANNEL_CAPACITY as u64) {
            v.publish("/p", &[0u8; 8], Qos::ReliableUdp, seq)
                .expect("publish should succeed until capacity");
        }

        // Hand the variant off to a thread that issues one extra
        // publish. With the channel saturated, `blocking_send` must
        // block until the test drains a slot.
        let v_arc = Arc::new(std::sync::Mutex::new(v));
        let v_for_thread = v_arc.clone();
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let blocked_for_thread = blocked.clone();
        let handle = std::thread::spawn(move || {
            let mut guard = v_for_thread.lock().unwrap();
            guard
                .publish("/p", &[0u8; 8], Qos::ReliableUdp, 9999)
                .expect("publish should eventually succeed");
            blocked_for_thread.store(false, std::sync::atomic::Ordering::SeqCst);
        });

        // The publish thread should be parked on `blocking_send`.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            blocked.load(std::sync::atomic::Ordering::SeqCst),
            "publish() should still be blocked on blocking_send while the bounded \
             reliable channel is at capacity (capacity={})",
            RELIABLE_CHANNEL_CAPACITY,
        );

        // Drain one slot. The blocked thread should now unblock.
        let _drained = rx_for(&mut rxs, "bob", Qos::ReliableUdp)
            .try_recv()
            .expect("the saturated channel must have at least one queued message");

        // Wait up to a few seconds for the publish thread to finish.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !blocked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !blocked.load(std::sync::atomic::Ordering::SeqCst),
            "publish() should unblock once the bounded channel has a free slot"
        );
        handle.join().expect("publish thread should finish cleanly");
    }

    /// T17.7: With the bounded channel saturated and no drain
    /// happening, `try_publish` at QoS 3/4 MUST also block (because
    /// it delegates to `publish` for reliable QoS) rather than skip.
    /// Skipping at QoS 3/4 is a DESIGN.md § 6.5 violation; the driver
    /// (T17.2) double-checks this by looping on `Ok(false)`, but the
    /// variant itself must never produce `Ok(false)` at QoS 3/4.
    #[test]
    fn try_publish_qos4_blocks_when_bounded_channel_full() {
        let (mut v, mut rxs, _inflight) = build_test_variant_with_peer("bob");
        for seq in 0..(RELIABLE_CHANNEL_CAPACITY as u64) {
            v.try_publish("/p", &[0u8; 8], Qos::ReliableTcp, seq)
                .expect("try_publish at QoS 4 must not error");
        }
        let v_arc = Arc::new(std::sync::Mutex::new(v));
        let v_for_thread = v_arc.clone();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_for_thread = completed.clone();
        let handle = std::thread::spawn(move || {
            let mut guard = v_for_thread.lock().unwrap();
            let ok = guard
                .try_publish("/p", &[0u8; 8], Qos::ReliableTcp, 9999)
                .expect("try_publish at QoS 4 must not error");
            assert!(
                ok,
                "QoS 4 try_publish must return Ok(true) once it unblocks"
            );
            completed_for_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !completed.load(std::sync::atomic::Ordering::SeqCst),
            "try_publish at QoS 4 must block when the bounded channel is full \
             (DESIGN.md § 6.5 -- no Ok(false) skip allowed at QoS 3/4)"
        );

        let _drained = rx_for(&mut rxs, "bob", Qos::ReliableTcp)
            .try_recv()
            .expect("a queued message must exist");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if completed.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "try_publish should unblock once the bounded channel has a free slot"
        );
        handle
            .join()
            .expect("try_publish thread should finish cleanly");
    }
}
