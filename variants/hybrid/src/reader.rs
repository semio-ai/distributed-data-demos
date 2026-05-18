//! T14.4 + T14.16: Multi-mode reader-thread machinery for the Hybrid
//! variant.
//!
//! In `ThreadingMode::Multi`, `HybridVariant::start_reader_threads`
//! spawns:
//!
//! - one UDP recv thread that does blocking `recv_from` on a dedicated
//!   recv-side `UdpSocket` and routes decoded items into one of two
//!   channels (see below);
//! - one TCP reader thread per peer (inbound and outbound) that does
//!   blocking `read` on the peer's read-clone `TcpStream` and routes
//!   decoded items into the same pair of channels.
//!
//! T14.16: two-channel architecture.
//!
//! - `data_tx` / `rx` — bounded `mpsc::sync_channel` (capacity
//!   `READER_CHANNEL_CAPACITY = 4096`) carrying decoded
//!   `HubDataMessage::Data` frames.
//!   - **UDP path (QoS 1/2)**: drop-on-full is acceptable; QoS 1/2
//!     tolerate loss by definition.
//!   - **TCP path (QoS 3/4 — T17.4)**: blocking-on-full. Dropping a
//!     TCP-delivered frame here would violate the strict-no-skip
//!     contract (DESIGN.md § 6.5). By blocking the TCP reader on a
//!     full channel, the kernel TCP recv buffer fills, kernel TCP
//!     back-pressures the peer's `write_all`, and the back-pressure
//!     signal reaches the application's `try_publish`. The driver
//!     thread's `poll_receive_multi` is responsible for draining the
//!     channel; if it stalls, `JOIN_TIMEOUT` on shutdown caps the
//!     wait.
//! - `lifecycle_tx` / `lifecycle_rx` — unbounded `mpsc::channel`
//!   carrying `HubLifecycleMessage::Eot` markers. Must NEVER drop:
//!   losing an EOT forces the peer's driver to wait the full
//!   `eot_timeout`, defeating the EOT contract.
//!
//! The driver thread drains both channels via `try_recv` inside
//! `HybridVariant::poll_receive_multi`, lifecycle first (so EOT is
//! never starved by a saturated data channel) then data.
//!
//! Lifecycle:
//!
//! - `start_reader_threads` is called by the driver AFTER `connect`
//!   returns successfully.
//! - `stop_reader_threads` is called by the driver BEFORE `disconnect`.
//!   It flips a shared `AtomicBool`, shuts down the sockets (so any
//!   blocked `recv`/`read` returns), and joins every handle with a
//!   generous timeout.

use std::io::Read;
use std::net::{Shutdown, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;

use crate::protocol::{self, Frame};

/// Capacity of the shared bounded `mpsc::sync_channel` carrying DATA
/// frames between the reader threads and the driver. Slightly oversized
/// for the high-rate fixture (4 * 1000 vpt = 4000 working set) so
/// backpressure on the driver side doesn't immediately cause drops --
/// but bounded, so a runaway accumulation can't OOM the process. See
/// T14.4 audit notes in `metak-orchestrator/STATUS.md`.
///
/// T14.16: lifecycle items (EOT, PeerDropped) ride a separate
/// `std::sync::mpsc::channel` that is unbounded and never drops -- the
/// "data may drop, EOT must not" invariant.
pub const READER_CHANNEL_CAPACITY: usize = 4096;

/// `SO_RCVTIMEO` applied to per-peer TCP read clones when handed to a
/// Multi-mode reader thread. Longer than the Single-mode 1 ms because
/// the reader thread doesn't have to interleave with anything else --
/// its only purpose is to wake up periodically to check the shutdown
/// flag.
pub const TCP_READER_TIMEOUT: Duration = Duration::from_millis(200);

/// `SO_RCVTIMEO` applied to the UDP recv socket handed to the Multi-
/// mode UDP reader thread. Same rationale as `TCP_READER_TIMEOUT`.
pub const UDP_READER_TIMEOUT: Duration = Duration::from_millis(200);

/// Maximum time to wait for reader threads to join during
/// `stop_reader_threads`. A reader thread that hasn't exited within
/// this budget is leaked rather than blocking the driver indefinitely.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Pre-decoded DATA frame pushed by a reader thread onto the bounded
/// data channel.
///
/// - UDP path (QoS 1/2): drop-on-full is acceptable; QoS 1/2 tolerate
///   loss by definition.
/// - TCP path (QoS 3/4 — T17.4): block-on-full. The TCP reader thread
///   uses [`push_data_or_block`] so back-pressure on a saturated
///   driver propagates through the kernel TCP recv buffer into the
///   peer's `write_all`, reaching the application via the strict
///   no-skip contract (DESIGN.md § 6.5).
#[derive(Debug, Clone)]
pub enum HubDataMessage {
    /// A `Frame::Data` decoded into a `ReceivedUpdate`.
    Data(variant_base::types::ReceivedUpdate),
}

/// Pre-decoded LIFECYCLE item pushed by a reader thread onto the
/// unbounded lifecycle channel. Must NEVER drop: an EOT loss forces
/// the peer's driver to wait the full `eot_timeout`, defeating the
/// EOT contract.
#[derive(Debug, Clone)]
pub enum HubLifecycleMessage {
    /// A `Frame::Eot` -- writer name + eot_id.
    Eot { writer: String, eot_id: u64 },
}

/// Holds the receive ends of both channels + handles + the shutdown
/// flag. Owned by `HybridVariant` in Multi mode; dropped in
/// `disconnect`.
///
/// T14.16: `rx` (bounded data) and `lifecycle_rx` (unbounded lifecycle)
/// replace the single shared `Receiver<HubMessage>` used pre-T14.16.
/// `poll_receive_multi` drains lifecycle first, then data, so EOT
/// observations are never starved by a saturated data channel.
pub struct ReaderHub {
    pub rx: Receiver<HubDataMessage>,
    pub lifecycle_rx: Receiver<HubLifecycleMessage>,
    pub shutdown: Arc<AtomicBool>,
    pub handles: Vec<JoinHandle<()>>,
    /// Cloned TCP read-side handles kept around so
    /// `stop_reader_threads` can call `shutdown(Both)` to interrupt
    /// blocked reads. (The reader threads own the actual read clones.)
    pub tcp_shutdown_handles: Vec<TcpStream>,
    /// Dedicated UDP recv socket cloned handle for shutdown signalling.
    /// On `stop`, we `set_nonblocking(true)` on this handle so the
    /// next blocked `recv_from` returns `WouldBlock` and the reader
    /// exits via the shutdown-flag check.
    pub udp_shutdown_handle: Option<UdpSocket>,
}

impl ReaderHub {
    /// Build a new hub. Returns the hub plus matching sender pairs:
    /// `(data_tx, lifecycle_tx)`. Reader threads receive clones of both
    /// senders and route per-frame.
    pub fn new() -> (
        Self,
        SyncSender<HubDataMessage>,
        Sender<HubLifecycleMessage>,
    ) {
        let (data_tx, data_rx) = mpsc::sync_channel(READER_CHANNEL_CAPACITY);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let hub = Self {
            rx: data_rx,
            lifecycle_rx,
            shutdown,
            handles: Vec::new(),
            tcp_shutdown_handles: Vec::new(),
            udp_shutdown_handle: None,
        };
        (hub, data_tx, lifecycle_tx)
    }

    /// Signal every reader thread to exit and join with `JOIN_TIMEOUT`.
    pub fn stop_and_join(mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);

        // Wake up blocked TCP reads by shutting down both directions.
        // The reader thread's `read` call then returns `Ok(0)` (EOF)
        // or an error and the loop exits.
        for s in self.tcp_shutdown_handles.drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }

        // Wake up the blocked UDP recv by flipping the recv handle to
        // non-blocking. The next `recv_from` returns `WouldBlock` and
        // the loop exits via the shutdown-flag check.
        if let Some(s) = self.udp_shutdown_handle.take() {
            let _ = s.set_nonblocking(true);
        }

        let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
        for handle in self.handles.drain(..) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                eprintln!(
                    "[variant-hybrid] warning: reader thread join budget exhausted; detaching"
                );
                continue;
            }
            // `JoinHandle::join` does not accept a timeout. Poll
            // `is_finished` so a thread that ignores shutdown is
            // detached rather than blocking the driver forever.
            let mut waited = Duration::ZERO;
            let poll_step = Duration::from_millis(20);
            while !handle.is_finished() && waited < remaining {
                thread::sleep(poll_step);
                waited += poll_step;
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                eprintln!(
                    "[variant-hybrid] warning: reader thread did not exit within {JOIN_TIMEOUT:?}; detaching"
                );
            }
        }
        Ok(())
    }
}

/// Spawn the UDP recv thread.
pub fn spawn_udp_reader(
    socket: UdpSocket,
    data_tx: SyncSender<HubDataMessage>,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("hybrid-udp-reader".to_string())
        .spawn(move || udp_reader_loop(socket, data_tx, lifecycle_tx, shutdown))
        .expect("failed to spawn UDP reader thread")
}

fn udp_reader_loop(
    socket: UdpSocket,
    data_tx: SyncSender<HubDataMessage>,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 65536];
    while !shutdown.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((n, _addr)) => match protocol::decode_frame(&buf[..n]) {
                Ok(Frame::Data(update)) => {
                    push_data_or_drop(&data_tx, HubDataMessage::Data(update))
                }
                Ok(Frame::Eot { writer, eot_id }) => {
                    push_lifecycle(&lifecycle_tx, HubLifecycleMessage::Eot { writer, eot_id })
                }
                Err(e) => {
                    eprintln!("[variant-hybrid] UDP reader: decode error: {e:#}; dropping");
                }
            },
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                eprintln!("[variant-hybrid] UDP reader: recv error: {e}; exiting");
                break;
            }
        }
    }
}

/// Spawn one per-peer TCP reader thread.
pub fn spawn_tcp_reader(
    stream: TcpStream,
    label: String,
    data_tx: SyncSender<HubDataMessage>,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("hybrid-tcp-reader-{label}"))
        .spawn(move || tcp_reader_loop(stream, label, data_tx, lifecycle_tx, shutdown))
        .expect("failed to spawn TCP reader thread")
}

fn tcp_reader_loop(
    mut stream: TcpStream,
    label: String,
    data_tx: SyncSender<HubDataMessage>,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(65536);
    let mut tmp = [0u8; 65536];
    while !shutdown.load(Ordering::SeqCst) {
        match stream.read(&mut tmp) {
            Ok(0) => {
                if !shutdown.load(Ordering::SeqCst) {
                    eprintln!(
                        "[variant-hybrid] TCP reader {label}: peer closed (EOF); thread exits"
                    );
                }
                return;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                eprintln!("[variant-hybrid] TCP reader {label}: read error: {e}; thread exits");
                return;
            }
        }

        // Extract every complete length-prefixed frame currently in
        // the buffer. 4-byte big-endian length prefix.
        loop {
            if buf.len() < 4 {
                break;
            }
            let msg_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
            let total = 4 + msg_len;
            if buf.len() < total {
                break;
            }
            let payload: Vec<u8> = buf[4..total].to_vec();
            buf.drain(..total);
            match protocol::decode_frame(&payload) {
                Ok(Frame::Data(update)) => {
                    // T17.4: TCP path carries QoS 3/4 only. Block the
                    // reader on a full channel rather than dropping --
                    // dropping here would silently violate the strict
                    // no-skip contract (DESIGN.md § 6.5).
                    if !push_data_or_block(&data_tx, HubDataMessage::Data(update), &shutdown) {
                        return;
                    }
                }
                Ok(Frame::Eot { writer, eot_id }) => {
                    push_lifecycle(&lifecycle_tx, HubLifecycleMessage::Eot { writer, eot_id })
                }
                Err(e) => {
                    eprintln!("[variant-hybrid] TCP reader {label}: decode error: {e:#}; dropping");
                }
            }
        }
    }
}

/// Push `msg` onto the bounded DATA channel, dropping it if the channel
/// is full or the receiver has hung up. Logs a disambiguated warning on
/// full so the operator can be sure that lifecycle items (EOT) were
/// NOT lost when this line appears in stderr -- those ride the separate
/// unbounded `lifecycle_tx`. Does NOT block the reader -- in a
/// benchmark a blocking reader is worse than a missed Data frame.
///
/// **UDP reader only** (T17.4): QoS 1/2 tolerate loss by definition.
/// TCP readers use [`push_data_or_block`] instead.
fn push_data_or_drop(tx: &SyncSender<HubDataMessage>, msg: HubDataMessage) {
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            eprintln!(
                "[variant-hybrid] data channel full ({} slots) -- dropping Data frame (receiver saturated)",
                READER_CHANNEL_CAPACITY
            );
        }
        Err(TrySendError::Disconnected(_)) => {
            // Driver dropped the receiver -- spawn is winding down.
        }
    }
}

/// T17.4: TCP reader push that BLOCKS on a full channel instead of
/// dropping the frame. Periodically wakes to check `shutdown` so the
/// reader can exit cleanly during teardown even if the driver is no
/// longer draining. Returns `false` when shutdown is requested or the
/// receiver has hung up (caller exits its read loop).
///
/// Why blocking is required: TCP carries QoS 3/4 frames, which the
/// strict no-skip contract (DESIGN.md § 6.5) forbids dropping. Letting
/// the channel block back-pressures the kernel TCP recv buffer, which
/// in turn back-pressures the peer's `write_all`, which surfaces as
/// the application-level signal we want to measure.
const TCP_READER_FULL_BACKOFF: Duration = Duration::from_millis(2);

fn push_data_or_block(
    tx: &SyncSender<HubDataMessage>,
    mut msg: HubDataMessage,
    shutdown: &Arc<AtomicBool>,
) -> bool {
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                msg = returned;
                if shutdown.load(Ordering::SeqCst) {
                    return false;
                }
                thread::sleep(TCP_READER_FULL_BACKOFF);
            }
            Err(TrySendError::Disconnected(_)) => {
                return false;
            }
        }
    }
}

/// Push `msg` onto the unbounded LIFECYCLE channel. Because the channel
/// is unbounded, sends never block and never drop; the only failure
/// mode is the receiver having been dropped (driver tearing down), in
/// which case we silently swallow the result.
fn push_lifecycle(tx: &Sender<HubLifecycleMessage>, msg: HubLifecycleMessage) {
    let _ = tx.send(msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use variant_base::types::{Qos, ReceivedUpdate};

    fn dummy_update(seq: u64) -> ReceivedUpdate {
        ReceivedUpdate {
            writer: "alice".to_string(),
            seq,
            path: "/p".to_string(),
            qos: Qos::BestEffort,
            payload: vec![0u8; 8],
        }
    }

    #[test]
    fn reader_hub_new_returns_paired_senders() {
        let (hub, data_tx, lifecycle_tx) = ReaderHub::new();
        assert!(hub.handles.is_empty());

        // Lifecycle send routes onto lifecycle_rx.
        lifecycle_tx
            .send(HubLifecycleMessage::Eot {
                writer: "self".to_string(),
                eot_id: 1,
            })
            .expect("fresh lifecycle channel must accept a send");
        let received = hub
            .lifecycle_rx
            .try_recv()
            .expect("fresh lifecycle channel must yield a recv");
        match received {
            HubLifecycleMessage::Eot { writer, eot_id } => {
                assert_eq!(writer, "self");
                assert_eq!(eot_id, 1);
            }
        }

        // Data send routes onto rx (data channel).
        data_tx
            .try_send(HubDataMessage::Data(dummy_update(7)))
            .expect("fresh data channel must accept a send");
        let received = hub
            .rx
            .try_recv()
            .expect("fresh data channel must yield a recv");
        match received {
            HubDataMessage::Data(u) => {
                assert_eq!(u.seq, 7);
            }
        }
    }

    #[test]
    fn stop_and_join_on_empty_hub_is_ok() {
        let (hub, _data_tx, _lifecycle_tx) = ReaderHub::new();
        hub.stop_and_join().expect("empty hub stop must succeed");
    }

    #[test]
    fn push_data_or_drop_handles_disconnected() {
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        push_data_or_drop(&tx, HubDataMessage::Data(dummy_update(0)));
    }

    #[test]
    fn push_data_or_drop_handles_full_channel() {
        let (tx, _rx) = mpsc::sync_channel::<HubDataMessage>(1);
        tx.try_send(HubDataMessage::Data(dummy_update(1)))
            .expect("first send must fit");
        let before = std::time::Instant::now();
        push_data_or_drop(&tx, HubDataMessage::Data(dummy_update(2)));
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "push_data_or_drop on a full channel must not block"
        );
    }

    /// T14.16: lifecycle channel is unbounded -- many sends in a row
    /// must never drop and never block.
    #[test]
    fn push_lifecycle_never_drops_under_burst() {
        let (tx, rx) = mpsc::channel::<HubLifecycleMessage>();
        let before = std::time::Instant::now();
        for i in 0..10_000u64 {
            push_lifecycle(
                &tx,
                HubLifecycleMessage::Eot {
                    writer: "alice".to_string(),
                    eot_id: i,
                },
            );
        }
        assert!(
            before.elapsed() < Duration::from_millis(500),
            "push_lifecycle burst must not block"
        );
        // Every send made it through.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 10_000, "unbounded lifecycle channel must not drop");
    }

    /// T14.16: lifecycle send on a disconnected channel is silently
    /// absorbed (driver tearing down).
    #[test]
    fn push_lifecycle_handles_disconnected() {
        let (tx, rx) = mpsc::channel::<HubLifecycleMessage>();
        drop(rx);
        push_lifecycle(
            &tx,
            HubLifecycleMessage::Eot {
                writer: "x".to_string(),
                eot_id: 0,
            },
        );
    }
}
