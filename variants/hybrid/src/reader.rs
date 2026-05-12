//! T14.4: Multi-mode reader-thread machinery for the Hybrid variant.
//!
//! In `ThreadingMode::Multi`, `HybridVariant::start_reader_threads`
//! spawns:
//!
//! - one UDP recv thread that does blocking `recv_from` on a dedicated
//!   recv-side `UdpSocket` and pushes decoded `HubMessage`s onto a
//!   shared bounded `mpsc::SyncSender`;
//! - one TCP reader thread per peer (inbound and outbound) that does
//!   blocking `read` on the peer's read-clone `TcpStream` and pushes
//!   decoded `HubMessage`s onto the same channel.
//!
//! The driver thread drains the channel via `try_recv` inside
//! `HybridVariant::poll_receive`. The channel is bounded so a slow
//! consumer can't OOM us; pushers `try_send` and drop on full-channel
//! rather than blocking the reader (the variant is benchmark-grade,
//! not a buffered queue).
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
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;

use crate::protocol::{self, Frame};

/// Capacity of the shared bounded `mpsc::sync_channel` between the
/// reader threads and the driver. Slightly oversized for the high-rate
/// fixture (4 * 1000 vpt = 4000 working set) so backpressure on the
/// driver side doesn't immediately cause drops -- but bounded, so a
/// runaway accumulation can't OOM the process. See T14.4 audit notes
/// in `metak-orchestrator/STATUS.md`.
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

/// Pre-decoded frame pushed by a reader thread to the driver.
#[derive(Debug, Clone)]
pub enum HubMessage {
    /// A `Frame::Data` decoded into a `ReceivedUpdate`.
    Data(variant_base::types::ReceivedUpdate),
    /// A `Frame::Eot` -- writer name + eot_id.
    Eot { writer: String, eot_id: u64 },
}

/// Holds the receive end of the channel + handles + the shutdown flag.
/// Owned by `HybridVariant` in Multi mode; dropped in `disconnect`.
pub struct ReaderHub {
    pub rx: Receiver<HubMessage>,
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
    /// Build a new hub with the standard channel capacity.
    pub fn new() -> (Self, SyncSender<HubMessage>) {
        let (tx, rx) = mpsc::sync_channel(READER_CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let hub = Self {
            rx,
            shutdown,
            handles: Vec::new(),
            tcp_shutdown_handles: Vec::new(),
            udp_shutdown_handle: None,
        };
        (hub, tx)
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
    tx: SyncSender<HubMessage>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("hybrid-udp-reader".to_string())
        .spawn(move || udp_reader_loop(socket, tx, shutdown))
        .expect("failed to spawn UDP reader thread")
}

fn udp_reader_loop(socket: UdpSocket, tx: SyncSender<HubMessage>, shutdown: Arc<AtomicBool>) {
    let mut buf = [0u8; 65536];
    while !shutdown.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((n, _addr)) => match protocol::decode_frame(&buf[..n]) {
                Ok(Frame::Data(update)) => push_or_drop(&tx, HubMessage::Data(update)),
                Ok(Frame::Eot { writer, eot_id }) => {
                    push_or_drop(&tx, HubMessage::Eot { writer, eot_id })
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
    tx: SyncSender<HubMessage>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("hybrid-tcp-reader-{label}"))
        .spawn(move || tcp_reader_loop(stream, label, tx, shutdown))
        .expect("failed to spawn TCP reader thread")
}

fn tcp_reader_loop(
    mut stream: TcpStream,
    label: String,
    tx: SyncSender<HubMessage>,
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
                Ok(Frame::Data(update)) => push_or_drop(&tx, HubMessage::Data(update)),
                Ok(Frame::Eot { writer, eot_id }) => {
                    push_or_drop(&tx, HubMessage::Eot { writer, eot_id })
                }
                Err(e) => {
                    eprintln!("[variant-hybrid] TCP reader {label}: decode error: {e:#}; dropping");
                }
            }
        }
    }
}

/// Push `msg` onto `tx`, dropping it if the channel is full or the
/// receiver has hung up. Logs a single warning on full so the operator
/// notices runaway accumulation, but does NOT block the reader -- in
/// a benchmark a blocking reader is worse than a missed frame.
fn push_or_drop(tx: &SyncSender<HubMessage>, msg: HubMessage) {
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            eprintln!(
                "[variant-hybrid] reader channel full ({} slots); dropping frame",
                READER_CHANNEL_CAPACITY
            );
        }
        Err(TrySendError::Disconnected(_)) => {
            // Driver dropped the receiver -- spawn is winding down.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_hub_new_returns_paired_sender() {
        let (hub, tx) = ReaderHub::new();
        assert!(hub.handles.is_empty());
        tx.try_send(HubMessage::Eot {
            writer: "self".to_string(),
            eot_id: 1,
        })
        .expect("fresh channel must accept a send");
        let received = hub.rx.try_recv().expect("fresh channel must yield a recv");
        match received {
            HubMessage::Eot { writer, eot_id } => {
                assert_eq!(writer, "self");
                assert_eq!(eot_id, 1);
            }
            HubMessage::Data(_) => panic!("expected Eot"),
        }
    }

    #[test]
    fn stop_and_join_on_empty_hub_is_ok() {
        let (hub, _tx) = ReaderHub::new();
        hub.stop_and_join().expect("empty hub stop must succeed");
    }

    #[test]
    fn push_or_drop_handles_disconnected() {
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        push_or_drop(
            &tx,
            HubMessage::Eot {
                writer: "x".to_string(),
                eot_id: 0,
            },
        );
    }

    #[test]
    fn push_or_drop_handles_full_channel() {
        let (tx, _rx) = mpsc::sync_channel::<HubMessage>(1);
        tx.try_send(HubMessage::Eot {
            writer: "a".to_string(),
            eot_id: 1,
        })
        .expect("first send must fit");
        let before = std::time::Instant::now();
        push_or_drop(
            &tx,
            HubMessage::Eot {
                writer: "b".to_string(),
                eot_id: 2,
            },
        );
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "push_or_drop on a full channel must not block"
        );
    }
}
