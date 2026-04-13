use std::collections::VecDeque;

use anyhow::Result;

use crate::types::{Qos, ReceivedUpdate};
use crate::variant_trait::Variant;

/// A no-network variant that echoes writes to itself via an internal queue.
///
/// Used for testing the full pipeline without any real transport. Publishes
/// are pushed into an internal `VecDeque` and immediately available via
/// `poll_receive`, simulating instant local delivery.
pub struct VariantDummy {
    runner: String,
    queue: VecDeque<ReceivedUpdate>,
}

impl VariantDummy {
    /// Create a new dummy variant.
    ///
    /// `runner` is the runner name used as the `writer` field in received updates.
    pub fn new(runner: &str) -> Self {
        Self {
            runner: runner.to_string(),
            queue: VecDeque::new(),
        }
    }
}

impl Variant for VariantDummy {
    fn name(&self) -> &str {
        "dummy"
    }

    fn connect(&mut self) -> Result<()> {
        // No-op: no network to connect to.
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        // Echo the write as a received update (writer = own runner).
        self.queue.push_back(ReceivedUpdate {
            writer: self.runner.clone(),
            seq,
            path: path.to_string(),
            qos,
            payload: payload.to_vec(),
        });
        Ok(())
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        Ok(self.queue.pop_front())
    }

    fn disconnect(&mut self) -> Result<()> {
        // No-op.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connect_disconnect_noop() {
        let mut dummy = VariantDummy::new("a");
        assert!(dummy.connect().is_ok());
        assert!(dummy.disconnect().is_ok());
    }

    #[test]
    fn test_publish_echoes_to_receive() {
        let mut dummy = VariantDummy::new("runner-a");
        dummy
            .publish("/bench/0", &[1, 2, 3, 4], Qos::BestEffort, 1)
            .unwrap();

        let update = dummy.poll_receive().unwrap();
        assert!(update.is_some());
        let update = update.unwrap();
        assert_eq!(update.writer, "runner-a");
        assert_eq!(update.seq, 1);
        assert_eq!(update.path, "/bench/0");
        assert_eq!(update.qos, Qos::BestEffort);
        assert_eq!(update.payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_poll_empty_returns_none() {
        let mut dummy = VariantDummy::new("a");
        let update = dummy.poll_receive().unwrap();
        assert!(update.is_none());
    }

    #[test]
    fn test_fifo_ordering() {
        let mut dummy = VariantDummy::new("a");
        dummy.publish("/a", &[], Qos::BestEffort, 1).unwrap();
        dummy.publish("/b", &[], Qos::LatestValue, 2).unwrap();

        let first = dummy.poll_receive().unwrap().unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(first.path, "/a");

        let second = dummy.poll_receive().unwrap().unwrap();
        assert_eq!(second.seq, 2);
        assert_eq!(second.path, "/b");

        assert!(dummy.poll_receive().unwrap().is_none());
    }

    #[test]
    fn test_name() {
        let dummy = VariantDummy::new("a");
        assert_eq!(dummy.name(), "dummy");
    }
}
