use anyhow::Result;

use crate::logger::LoggerHandle;
use crate::types::{Qos, ReceivedUpdate, ThreadingMode};

/// Trait that all benchmark variant implementations must implement.
///
/// This trait defines the minimal transport-specific operations. Everything else
/// (phases, logging, workload, CLI) lives outside the trait and is handled by
/// the protocol driver.
///
/// **E15 / T15.8 cleanup**: the previous `signal_end_of_test` and
/// `poll_peer_eots` methods (and the `PeerEot` type) were removed. The
/// on-wire EOT exchange is no longer used; the runner-coordinated
/// termination state machine (T15.4) combined with variant-side idle
/// detection (T15.5) is the sole exit mechanism. See
/// `metak-shared/api-contracts/eot-protocol.md` (historical) for the
/// pre-E15 design.
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
    /// **QoS contract** (see `metak-shared/DESIGN.md` § 6.5):
    /// - **QoS 1 / QoS 2** (best-effort / latest-value): `Ok(false)` is
    ///   the contractual back-pressure signal. The driver records a
    ///   `backpressure_skipped` JSONL event and moves on -- the value
    ///   is dropped, not retried. Variants that can detect back-pressure
    ///   cheaply (non-blocking sends returning `WouldBlock`, QUIC
    ///   `SendDatagramError::Blocked`, WebRTC `bufferedAmount`, etc.)
    ///   should override this method per T-impl.7.
    /// - **QoS 3 / QoS 4** (reliable-UDP / reliable-TCP): variants MUST
    ///   block internally until the message is accepted -- delivery is
    ///   the contract, throughput collapse is the acceptable failure
    ///   mode. Returning `Ok(false)` at QoS 3/4 is a **contract
    ///   violation**: the driver loops on `try_publish` until it gets
    ///   `Ok(true)` (or `Err`), emits a one-shot stderr warning, and
    ///   does NOT emit a `backpressure_skipped` event (the analyzer's
    ///   T17.9 integrity check flags any such event at QoS 3/4 as a
    ///   variant bug). See also `metak-shared/api-contracts/variant-cli.md`
    ///   `--qos` and `metak-shared/api-contracts/jsonl-log-schema.md`
    ///   `backpressure_skipped`.
    ///
    /// Default implementation: call `publish(...)` and return `Ok(true)`,
    /// preserving the existing fire-and-forget semantics for variants
    /// that do not override this method.
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        self.publish(path, payload, qos, seq)?;
        Ok(true)
    }

    /// Poll for a received update. Returns `None` if no update is available.
    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>>;

    /// Disconnect from the transport.
    fn disconnect(&mut self) -> Result<()>;
}
