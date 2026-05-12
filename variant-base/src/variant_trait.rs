use anyhow::Result;

use crate::logger::LoggerHandle;
use crate::types::{Qos, ReceivedUpdate, ThreadingMode};

/// A peer end-of-test marker observed by a variant.
///
/// Returned by `Variant::poll_peer_eots` to inform the driver which
/// peers have signalled end-of-test for the current spawn.
///
/// The variant is the source of truth for dedup: each `(writer, eot_id)`
/// MUST be returned at most once across the lifetime of a spawn. The
/// driver applies a defensive dedup-by-writer pass on its side as a
/// backstop, but variants must not rely on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEot {
    /// Runner name of the writer whose EOT was just observed.
    pub writer: String,
    /// 64-bit id from the writer's `signal_end_of_test`.
    pub eot_id: u64,
}

/// Trait that all benchmark variant implementations must implement.
///
/// This trait defines the minimal transport-specific operations. Everything else
/// (phases, logging, workload, CLI) lives outside the trait and is handled by
/// the protocol driver.
pub trait Variant {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Threading modes this variant supports.
    ///
    /// Default: `&[ThreadingMode::Single]` -- variants that have not
    /// opted into the E14 threading-mode dimension declare themselves
    /// single-threaded-only. Variants that support both modes (websocket,
    /// hybrid, custom-udp, the dummy) override this with
    /// `&[ThreadingMode::Single, ThreadingMode::Multi]`. Variants whose
    /// transport library is fundamentally async (quic, webrtc, zenoh)
    /// override with `&[ThreadingMode::Multi]` only.
    ///
    /// Order does not matter; the runner does declared-set membership
    /// checks.
    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        &[ThreadingMode::Single]
    }

    /// Establish the transport connection.
    ///
    /// `threading_mode` tells the variant which execution model to use.
    /// Variants that don't branch on the mode (e.g. those that only
    /// support `Single`) may ignore the argument. Variants that need
    /// reader threads should NOT spawn them here -- the driver calls
    /// [`Variant::start_reader_threads`] immediately after `connect`
    /// returns successfully. Doing the work in two methods keeps the
    /// connect-time error path simple and lets variants log a clean
    /// `connected` event before any reader thread starts running.
    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()>;

    /// Receive a shared, thread-safe handle to the driver's JSONL
    /// `Logger`.
    ///
    /// Called by the driver immediately AFTER `connect` returns and
    /// BEFORE `start_reader_threads`. Variants whose reader threads
    /// emit `receive` events directly (T14.10) capture the handle here
    /// and clone it into each spawned thread. Variants that route all
    /// logging through the driver thread (the historical model) can
    /// ignore the call; the default implementation is a no-op.
    ///
    /// The handle internally holds an `Arc<Mutex<Logger>>` so any
    /// number of threads may emit events safely; the driver retains
    /// its own clone for driver-side events.
    fn attach_logger(&mut self, _logger: LoggerHandle) {}

    /// Spawn per-peer reader threads (or any other multi-thread
    /// machinery) for the chosen mode.
    ///
    /// Called by the driver immediately AFTER `connect` returns
    /// successfully. The default implementation is a no-op, which is
    /// the right behaviour for variants that only support `Single` mode
    /// or that handle their own threading inside `connect`.
    ///
    /// Variants that support `Multi` mode override this to spawn the
    /// reader thread(s) and stash the join handles + shutdown signal
    /// inside `self` so [`Variant::stop_reader_threads`] can tear them
    /// down cleanly at disconnect time.
    fn start_reader_threads(&mut self, _mode: ThreadingMode) -> Result<()> {
        Ok(())
    }

    /// Tear down per-peer reader threads (or any other multi-thread
    /// machinery) started by [`Variant::start_reader_threads`].
    ///
    /// Called by the driver during the disconnect path, BEFORE the
    /// variant's own [`Variant::disconnect`] runs so reader threads can
    /// drain pending receives cleanly. Default implementation is a
    /// no-op.
    fn stop_reader_threads(&mut self) -> Result<()> {
        Ok(())
    }

    /// Publish a value over the transport.
    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()>;

    /// Try to publish a value over the transport, respecting backpressure.
    ///
    /// Returns:
    /// - `Ok(true)` if the value was accepted by the transport.
    /// - `Ok(false)` if the transport is currently backpressured (the
    ///   write was NOT delivered; the caller should skip it and move
    ///   on to the next value rather than retrying within the same tick).
    /// - `Err(_)` for real errors (propagated to the driver as today).
    ///
    /// `Ok(false)` is NOT an error -- it just means "not now". The
    /// driver logs a `backpressure_skipped` event instead of a `write`
    /// event so the analysis can distinguish "writer held back" from
    /// "writer sent and downstream dropped it".
    ///
    /// Default implementation: call `publish(...)` and return `Ok(true)`,
    /// preserving the existing fire-and-forget semantics for variants
    /// that do not override this method. Transports that can detect
    /// backpressure cheaply (non-blocking sends returning `WouldBlock`,
    /// QUIC `SendDatagramError::Blocked`, WebRTC `bufferedAmount`, etc.)
    /// should override this method per T-impl.7.
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        self.publish(path, payload, qos, seq)?;
        Ok(true)
    }

    /// Poll for a received update. Returns `None` if no update is available.
    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>>;

    /// Disconnect from the transport.
    fn disconnect(&mut self) -> Result<()>;

    /// Broadcast an "end of test" marker to all peers.
    ///
    /// Called once by the driver at the start of the EOT phase, after
    /// the last data write. The returned `eot_id` is a 64-bit value
    /// (typically random per-spawn) that the driver logs in the
    /// `eot_sent` event so receivers can correlate their `eot_received`
    /// events with the writer's `eot_sent`.
    ///
    /// Default implementation: returns `Ok(0)` and does nothing. A
    /// variant that does not override this method opts out of EOT;
    /// the driver will fall back to logging `eot_timeout` once the
    /// configured timeout elapses (since no peers will ever respond).
    fn signal_end_of_test(&mut self) -> Result<u64> {
        Ok(0)
    }

    /// Return any newly-observed peer EOTs since the last call.
    ///
    /// Called repeatedly by the driver in a poll loop until every
    /// expected peer is observed or the configured timeout elapses.
    ///
    /// The variant MUST dedupe internally: if peer X has already been
    /// returned in a previous call, do not return X again. The driver
    /// uses dedup-by-writer-name on its side as a defensive backstop,
    /// but the variant is the source of truth.
    ///
    /// Default implementation: returns an empty vec (variant opted
    /// out of EOT).
    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        Ok(Vec::new())
    }
}
