use anyhow::Result;

use crate::types::{Qos, ReceivedUpdate};

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
}
