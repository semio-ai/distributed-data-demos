use std::collections::VecDeque;

use anyhow::Result;

use crate::types::{Qos, ReceivedUpdate, ThreadingMode};
use crate::variant_trait::Variant;

/// A no-network variant that echoes writes to itself via an internal queue.
///
/// Used for testing the full pipeline without any real transport. Publishes
/// are pushed into an internal `VecDeque` and immediately available via
/// `poll_receive`, simulating instant local delivery.
///
/// Declares `[Single, Multi]` capabilities (E14 / T14.1). The dummy has
/// no real I/O, so both modes do the same thing internally -- the point
/// is to exercise the new threading-mode infrastructure end-to-end
/// regardless of which mode the runner picks.
pub struct VariantDummy {
    runner: String,
    queue: VecDeque<ReceivedUpdate>,
    /// Mode the driver passed at `connect` time. Kept so introspection
    /// tests can confirm the dummy received and stored the mode; the
    /// dummy itself does not branch on it.
    connected_mode: Option<ThreadingMode>,
}

impl VariantDummy {
    /// Create a new dummy variant.
    ///
    /// `runner` is the runner name used as the `writer` field in received updates.
    pub fn new(runner: &str) -> Self {
        Self {
            runner: runner.to_string(),
            queue: VecDeque::new(),
            connected_mode: None,
        }
    }

    /// Threading mode the driver supplied at `connect` time (if any).
    /// Test-only accessor; not part of the trait surface.
    pub fn connected_mode(&self) -> Option<ThreadingMode> {
        self.connected_mode
    }
}

impl Variant for VariantDummy {
    fn name(&self) -> &str {
        "dummy"
    }

    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        // The dummy has no real I/O so it trivially supports both
        // modes. Declaring both lets us drive the new threading-mode
        // infrastructure end-to-end in tests and smoke runs without
        // needing a real transport.
        &[ThreadingMode::Single, ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        // Record the mode so tests can confirm it propagated. No
        // network to connect to.
        self.connected_mode = Some(threading_mode);
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
        assert!(dummy.connect(ThreadingMode::Single).is_ok());
        assert!(dummy.disconnect().is_ok());
    }

    #[test]
    fn test_dummy_declares_both_threading_modes() {
        let dummy = VariantDummy::new("a");
        let modes = dummy.supported_threading_modes();
        assert!(modes.contains(&ThreadingMode::Single));
        assert!(modes.contains(&ThreadingMode::Multi));
    }

    #[test]
    fn test_dummy_stores_connect_mode() {
        let mut dummy = VariantDummy::new("a");
        assert_eq!(dummy.connected_mode(), None);
        dummy.connect(ThreadingMode::Multi).unwrap();
        assert_eq!(dummy.connected_mode(), Some(ThreadingMode::Multi));
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
