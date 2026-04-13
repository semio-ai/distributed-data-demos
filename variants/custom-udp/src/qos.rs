/// QoS-specific receive-side logic.
///
/// - QoS 1 (BestEffort): accept all messages.
/// - QoS 2 (LatestValue): track highest seq per (writer, path), discard stale.
/// - QoS 3 (ReliableUdp): detect gaps per writer, request NACKs.
/// - QoS 4 (ReliableTcp): handled at the transport layer (TCP guarantees ordering).
use std::collections::HashMap;

/// Tracks the highest sequence number seen per (writer, path) pair.
/// Used for QoS 2 stale-discard.
pub struct LatestValueTracker {
    /// Map from (writer, path) to highest seq seen.
    highest: HashMap<(String, String), u64>,
}

impl LatestValueTracker {
    pub fn new() -> Self {
        Self {
            highest: HashMap::new(),
        }
    }

    /// Check if an update is stale. Returns `true` if the message should be
    /// accepted (not stale), `false` if it should be discarded.
    ///
    /// Updates the tracker if the message is accepted.
    pub fn accept(&mut self, writer: &str, path: &str, seq: u64) -> bool {
        let key = (writer.to_string(), path.to_string());
        match self.highest.get(&key) {
            Some(&prev_seq) if seq <= prev_seq => false,
            _ => {
                self.highest.insert(key, seq);
                true
            }
        }
    }
}

/// Tracks sequence numbers per writer for gap detection (QoS 3).
///
/// Detects when a sequence number is skipped and reports gaps.
pub struct GapDetector {
    /// Map from writer to next expected sequence number.
    expected: HashMap<String, u64>,
}

/// Result of checking a sequence number for gaps.
#[derive(Debug, PartialEq, Eq)]
pub enum GapCheckResult {
    /// Message is in order; no gaps.
    InOrder,
    /// First message from this writer; no prior state to compare.
    FirstSeen,
    /// Gap detected. Contains the list of missing sequence numbers
    /// between the last seen and the received seq.
    Gap { missing: Vec<u64> },
    /// Duplicate or out-of-order message (seq < expected).
    Duplicate,
}

impl GapDetector {
    pub fn new() -> Self {
        Self {
            expected: HashMap::new(),
        }
    }

    /// Check a received sequence number for gaps.
    pub fn check(&mut self, writer: &str, seq: u64) -> GapCheckResult {
        match self.expected.get(writer) {
            None => {
                // First message from this writer.
                self.expected.insert(writer.to_string(), seq + 1);
                GapCheckResult::FirstSeen
            }
            Some(&expected_seq) => {
                if seq == expected_seq {
                    // In order.
                    self.expected.insert(writer.to_string(), seq + 1);
                    GapCheckResult::InOrder
                } else if seq > expected_seq {
                    // Gap detected.
                    let missing: Vec<u64> = (expected_seq..seq).collect();
                    self.expected.insert(writer.to_string(), seq + 1);
                    GapCheckResult::Gap { missing }
                } else {
                    // Duplicate or late arrival.
                    GapCheckResult::Duplicate
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- LatestValueTracker tests --

    #[test]
    fn latest_value_accepts_first_message() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/p", 1));
    }

    #[test]
    fn latest_value_accepts_higher_seq() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/p", 1));
        assert!(tracker.accept("w1", "/p", 2));
        assert!(tracker.accept("w1", "/p", 10));
    }

    #[test]
    fn latest_value_discards_stale() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/p", 5));
        assert!(!tracker.accept("w1", "/p", 3)); // stale
        assert!(!tracker.accept("w1", "/p", 5)); // equal = stale
    }

    #[test]
    fn latest_value_independent_writers() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/p", 5));
        assert!(tracker.accept("w2", "/p", 3)); // different writer
    }

    #[test]
    fn latest_value_independent_paths() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/a", 5));
        assert!(tracker.accept("w1", "/b", 3)); // different path
    }

    #[test]
    fn latest_value_stale_then_fresh() {
        let mut tracker = LatestValueTracker::new();
        assert!(tracker.accept("w1", "/p", 10));
        assert!(!tracker.accept("w1", "/p", 5)); // stale
        assert!(tracker.accept("w1", "/p", 11)); // fresh again
    }

    // -- GapDetector tests --

    #[test]
    fn gap_first_message() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
    }

    #[test]
    fn gap_in_order() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
        assert_eq!(detector.check("w1", 2), GapCheckResult::InOrder);
        assert_eq!(detector.check("w1", 3), GapCheckResult::InOrder);
    }

    #[test]
    fn gap_detected() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
        // Skip seq 2, 3
        let result = detector.check("w1", 4);
        assert_eq!(
            result,
            GapCheckResult::Gap {
                missing: vec![2, 3]
            }
        );
    }

    #[test]
    fn gap_duplicate() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
        assert_eq!(detector.check("w1", 2), GapCheckResult::InOrder);
        assert_eq!(detector.check("w1", 1), GapCheckResult::Duplicate); // old
    }

    #[test]
    fn gap_independent_writers() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
        assert_eq!(detector.check("w2", 1), GapCheckResult::FirstSeen);
        assert_eq!(detector.check("w1", 2), GapCheckResult::InOrder);
        assert_eq!(
            detector.check("w2", 5),
            GapCheckResult::Gap {
                missing: vec![2, 3, 4]
            }
        );
    }

    #[test]
    fn gap_single_missing() {
        let mut detector = GapDetector::new();
        assert_eq!(detector.check("w1", 1), GapCheckResult::FirstSeen);
        let result = detector.check("w1", 3);
        assert_eq!(result, GapCheckResult::Gap { missing: vec![2] });
    }
}
