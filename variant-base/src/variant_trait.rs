use anyhow::Result;

use crate::types::{Qos, ReceivedUpdate};

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

    /// Establish the transport connection.
    fn connect(&mut self) -> Result<()>;

    /// Publish a value over the transport.
    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()>;

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
