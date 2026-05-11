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

use std::collections::{HashMap, HashSet};
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

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::variant_trait::{PeerEot, Variant};

use crate::pairing::{PairRole, PeerDesc};
use crate::protocol::{decode_frame, encode_data, encode_eot, Frame};
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
/// QoS. Sending EOT on an unreliable channel could deadlock the wait
/// if the marker drops in flight.
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

/// Inbound observation drained by the variant's poll methods.
#[derive(Debug)]
enum Inbound {
    Data(ReceivedUpdate),
    Eot(PeerEot),
}

/// Outbound message the send loop will deliver via the appropriate
/// DataChannel.
struct OutboundMessage {
    /// Target peer name (must match a key in `peer_channels`).
    peer: String,
    /// Which QoS channel to send on for this message (1..=4).
    qos: u8,
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
    send_tx: Option<mpsc::UnboundedSender<OutboundMessage>>,
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
            send_tx: None,
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

/// Background send loop: receives outbound messages and dispatches
/// them onto the appropriate DataChannel for the target peer / QoS.
///
/// For each message, after the `dc.send` future resolves (whether
/// successfully or not), decrement the per-(peer, qos) in-flight byte
/// counter by `inflight_bytes`. This keeps the counter that
/// `try_publish` consults in step with the actual queue depth as
/// observed from our side, even when `send` returns an error (in
/// which case the bytes never reach the wire but they are also no
/// longer queued for us).
async fn send_loop(
    mut rx: mpsc::UnboundedReceiver<OutboundMessage>,
    peer_channels: PeerChannelMap,
    mut shutdown_rx: ShutdownRx,
) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(m) => {
                        let Some(channels) = peer_channels.get(&m.peer) else {
                            if let Some(c) = m.inflight_counter.as_ref() {
                                c.fetch_sub(m.inflight_bytes, Ordering::Relaxed);
                            }
                            continue;
                        };
                        let Some(dc) = channels.get(&m.qos) else {
                            if let Some(c) = m.inflight_counter.as_ref() {
                                c.fetch_sub(m.inflight_bytes, Ordering::Relaxed);
                            }
                            continue;
                        };
                        let bytes: bytes::Bytes = m.data.clone();
                        if let Err(e) = dc.send(&bytes).await {
                            eprintln!("[webrtc] send to peer={} qos={} failed: {e}", m.peer, m.qos);
                        }
                        if let Some(c) = m.inflight_counter.as_ref() {
                            c.fetch_sub(m.inflight_bytes, Ordering::Relaxed);
                        }
                    }
                    None => break,
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }
}

impl Variant for WebRtcVariant {
    fn name(&self) -> &str {
        "webrtc"
    }

    fn connect(&mut self, threading_mode: variant_base::ThreadingMode) -> Result<()> {
        // T14.1 compile-fix only -- the trait signature gained the
        // threading-mode argument. Real Multi-only capability
        // declaration + Single-mode rejection is filed under T14.6.
        let _ = threading_mode;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .build()
            .context("build tokio runtime")?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<OutboundMessage>();
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

        // Spawn the send loop.
        let pc_for_send = peer_channels.clone();
        let shutdown_for_send = shutdown_rx.clone();
        runtime.spawn(async move {
            send_loop(send_rx, pc_for_send, shutdown_for_send).await;
        });

        self.send_tx = Some(send_tx);
        self.recv_rx = Some(recv_rx);
        self.shutdown_tx = Some(shutdown_tx);
        self.peer_connections = pcs;
        self.inflight = inflight;
        self.runtime = Some(runtime);

        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let send_tx = self
            .send_tx
            .as_ref()
            .context("not connected -- call connect() first")?;
        let bytes = encode_data(qos, seq, path, &self.runner, payload);
        let inflight_bytes = bytes.len();
        let data = Bytes::from(bytes);
        for peer in &self.peers {
            let counter = self
                .inflight
                .get(&(peer.name.clone(), qos.as_int()))
                .cloned();
            if let Some(c) = counter.as_ref() {
                c.fetch_add(inflight_bytes, Ordering::Relaxed);
            }
            send_tx
                .send(OutboundMessage {
                    peer: peer.name.clone(),
                    qos: qos.as_int(),
                    data: data.clone(),
                    inflight_counter: counter,
                    inflight_bytes,
                })
                .map_err(|_| anyhow!("send channel closed"))?;
        }
        Ok(())
    }

    /// Non-blocking publish with backpressure detection (T-impl.7).
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
    /// For **reliable QoS** (3, 4): delegate to `publish` -- the
    /// inbound DataChannel is ordered + reliable, and any blocking
    /// happens inside `send_loop` (which awaits `dc.send`). Returning
    /// `Ok(false)` for a reliable channel would silently drop a seq
    /// that the receiver expects to see, creating a permanent gap.
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
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        self.send_tx.take();
        self.recv_rx.take();
        self.inflight.clear();

        // Close all peer connections gracefully.
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

    fn signal_end_of_test(&mut self) -> Result<u64> {
        let send_tx = self
            .send_tx
            .as_ref()
            .context("not connected -- call connect() first")?;
        let eot_id: u64 = rand::random::<u64>();
        let bytes = encode_eot(&self.runner, eot_id);
        let data = Bytes::from(bytes);
        // Always go on the reliable channel regardless of the spawn's
        // primary QoS -- an EOT lost on an unreliable channel could
        // deadlock the wait.
        for peer in &self.peers {
            // EOT frames bypass the T-impl.7 backpressure accounting:
            // they are reliable, one-shot, and must not be skipped.
            send_tx
                .send(OutboundMessage {
                    peer: peer.name.clone(),
                    qos: EOT_CHANNEL_QOS.as_int(),
                    data: data.clone(),
                    inflight_counter: None,
                    inflight_bytes: 0,
                })
                .map_err(|_| anyhow!("send channel closed"))?;
        }
        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        self.pump_inbound();
        // Dedup defensively at the variant boundary too: a duplicate
        // EOT could in principle land on multiple channels (we only
        // send on reliable, but be safe).
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut out: Vec<PeerEot> = Vec::with_capacity(self.pending_eots.len());
        for e in std::mem::take(&mut self.pending_eots) {
            if seen.insert((e.writer.clone(), e.eot_id)) {
                out.push(e);
            }
        }
        Ok(out)
    }
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
    /// actually opening any PeerConnections: a real send mpsc that the
    /// test owns the receiver of, the per-(peer, qos) in-flight
    /// counters populated, and one `PeerDesc` in `self.peers` so
    /// `publish` / `try_publish` know who to send to. The DataChannel
    /// side is bypassed -- the test asserts on what `try_publish`
    /// queues (or doesn't queue), not what the wire sees.
    type TestHarness = (
        WebRtcVariant,
        mpsc::UnboundedReceiver<OutboundMessage>,
        HashMap<(String, u8), Arc<AtomicUsize>>,
    );

    fn build_test_variant_with_peer(peer_name: &str) -> TestHarness {
        let (send_tx, send_rx) = mpsc::unbounded_channel::<OutboundMessage>();
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
        }
        variant.inflight = inflight.clone();
        variant.send_tx = Some(send_tx);
        (variant, send_rx, inflight)
    }

    #[test]
    fn try_publish_qos1_returns_false_when_buffer_over_threshold() {
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
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
            rx.try_recv().is_err(),
            "no OutboundMessage should be queued when try_publish returns Ok(false)"
        );
    }

    #[test]
    fn try_publish_qos2_returns_false_when_buffer_over_threshold() {
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::LatestValue.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD + 1, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::LatestValue, 42)
            .expect("try_publish must not error");
        assert!(!result, "QoS 2 over threshold must return Ok(false)");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn try_publish_qos1_returns_true_below_threshold() {
        let (mut v, mut rx, _inflight) = build_test_variant_with_peer("bob");
        // Counter starts at zero -- well below the threshold.
        let result = v
            .try_publish("/p", &[0u8; 8], Qos::BestEffort, 1)
            .expect("try_publish must not error");
        assert!(result, "QoS 1 below threshold must return Ok(true)");
        // Exactly one OutboundMessage must have been queued.
        let msg = rx
            .try_recv()
            .expect("a single OutboundMessage should be queued");
        assert_eq!(msg.peer, "bob");
        assert_eq!(msg.qos, Qos::BestEffort.as_int());
        assert!(msg.inflight_bytes > 0);
        assert!(rx.try_recv().is_err(), "exactly one queued message");
    }

    #[test]
    fn try_publish_qos3_returns_true_even_when_arbitrary_counter_set() {
        // Reliable QoS must NEVER return Ok(false) -- the receiver-
        // visible gap would corrupt ordering / completeness.
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
        // Pre-load the QoS-3 counter to many multiples of the threshold;
        // this models a deeply backed-up reliable channel. try_publish
        // must still return Ok(true) (and the caller / send loop will
        // back-pressure us through `dc.send().await` in the send loop).
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
        let msg = rx.try_recv().expect("message must be queued for QoS 3");
        assert_eq!(msg.qos, Qos::ReliableUdp.as_int());
    }

    #[test]
    fn try_publish_qos4_returns_true_even_when_arbitrary_counter_set() {
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
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
        let msg = rx.try_recv().expect("message must be queued for QoS 4");
        assert_eq!(msg.qos, Qos::ReliableTcp.as_int());
    }

    #[test]
    fn try_publish_qos1_at_exact_threshold_still_sends() {
        // Threshold check uses strict greater-than so a counter sitting
        // exactly at the threshold should still allow a send. This
        // documents the boundary in the contract.
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
        inflight
            .get(&("bob".to_string(), Qos::BestEffort.as_int()))
            .unwrap()
            .store(BACKPRESSURE_BYTES_THRESHOLD, Ordering::Relaxed);

        let result = v
            .try_publish("/p", &[0u8; 8], Qos::BestEffort, 1)
            .expect("try_publish must not error");
        assert!(result, "exactly-at-threshold should still send (strict >)");
        let _msg = rx
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
        let (mut v, mut rx, inflight) = build_test_variant_with_peer("bob");
        let counter = inflight
            .get(&("bob".to_string(), Qos::BestEffort.as_int()))
            .unwrap()
            .clone();
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        v.try_publish("/p", &[0u8; 16], Qos::BestEffort, 1).unwrap();
        let queued_value = counter.load(Ordering::Relaxed);
        assert!(queued_value > 0, "counter should have been incremented");

        let msg = rx.try_recv().expect("a message should be queued");
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
}
