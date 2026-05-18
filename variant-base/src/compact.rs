//! Compact in-memory event buffers (T18.1 / T18.2 / E18).
//!
//! During the operate and silent phases, the driver appends one
//! structured row per per-event JSONL line into columnar in-memory
//! buffers instead of (or in addition to) writing JSONL. At the end of
//! the run, the [`digest`](crate::driver) phase serialises everything
//! to a single Parquet file per spawn (`<variant>-<runner>-<run>.compact.parquet`).
//!
//! ## Why this exists
//!
//! Per-event JSONL is verbose: `~200 bytes/event` after compression
//! becomes prohibitive at the workload sizes E18 needs to scale to
//! (100 K msg/s x 30 s x 200 spawns = 60+ GB of JSONL). The compact
//! buffers + Parquet emit each per-event row in `~8` bytes of
//! columnar memory and `<2` bytes on disk under snappy compression, a
//! 30-50x reduction with no information loss for the events the
//! analysis pipeline actually consumes.
//!
//! ## Buffer shape
//!
//! Each compact event becomes one row across these columns:
//!
//! | Column | Rust type | Notes |
//! |--------|-----------|-------|
//! | `ts_ns` | `i64` | wall-clock nanos since UNIX epoch |
//! | `kind` | `u8` | event kind enum, see [`EventKind`] |
//! | `seq` | `u64` | writer sequence number (0 when not applicable) |
//! | `path_idx` | `u32` | interned path index |
//! | `peer_idx` | `u8` | interned writer/peer name; `u8::MAX` = self/none |
//! | `qos` | `u8` | QoS level 1..=4 (0 when not applicable) |
//! | `bytes` | `u32` | payload bytes (0 when not applicable) |
//!
//! Paths and peers are interned **lazily**: the first time the variant
//! observes a new path/peer it allocates an index and stores the string
//! in a side dictionary. Subsequent events for the same path/peer
//! reuse the same index. The dictionaries are bounded at `u32::MAX`
//! paths and `u8::MAX` peers; both caps are far above any realistic
//! benchmark (single-digit peers, single-digit-to-hundreds of paths).
//!
//! ## Memory accounting
//!
//! [`CompactBuffers::approx_bytes`] returns a coarse upper bound for
//! the in-memory footprint -- used by the driver to enforce the
//! `--digest-mem-soft-mb` warning and `--digest-mem-hard-mb` error
//! ceilings. The estimate counts each row as the sum of its column
//! widths (24 bytes per row) plus the string-heap bytes for the
//! intern dictionaries. It does NOT account for `Vec` over-allocation
//! slop -- a deliberate trade-off in favour of a single cheap addition
//! per `push` rather than a per-event capacity inspection.

use std::collections::HashMap;
use std::fmt;

/// Maximum number of distinct paths the intern table will accept.
///
/// `u32::MAX` is the column dtype's natural ceiling. The intern table
/// returns [`InternError::PathTableFull`] when this is exceeded so the
/// driver can fail the spawn cleanly rather than silently aliasing
/// distinct paths.
pub const MAX_PATHS: u32 = u32::MAX;

/// Maximum number of distinct peers (writer names) the intern table
/// will accept. Capped at `u8::MAX - 1 = 254` so `u8::MAX` (255) is
/// available as the sentinel "self / not-applicable" value. Real
/// benchmarks have <10 peers; the cap exists purely to surface
/// accidental runaway intern growth (e.g. peer names mistakenly
/// derived from per-message data instead of from `--peers`).
pub const MAX_PEERS: u8 = u8::MAX - 1;

/// Sentinel value for `peer_idx` indicating "no peer / self".
///
/// Used for events that do not carry a `writer` field (e.g.
/// `write`, `backpressure_skipped`). The Parquet writer encodes this
/// as the special index; the reader (analysis tool) maps it back to
/// "the runner who produced this file", which is recorded in the
/// Parquet file's key-value metadata.
pub const PEER_SELF: u8 = u8::MAX;

/// Approximate bytes per row in the columnar buffers. Used by the
/// memory-ceiling check; conservative -- counts the sum of column
/// widths exactly (8 + 1 + 8 + 4 + 1 + 1 + 4 = 27 bytes), rounded up
/// to 32 to absorb `Vec` over-allocation slop.
pub const ROW_BYTES_ESTIMATE: usize = 32;

/// Categorical event kind written into the `kind` column.
///
/// The numeric encoding is part of the Parquet file's wire format --
/// **do not renumber existing variants**. Adding new kinds is fine
/// (extend the enum + `From<EventKind> for u8`); analysis consumers
/// must tolerate unknown kinds gracefully (drop or warn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    /// A `write` event: the driver successfully published `seq` on
    /// `path` at `qos` of `bytes` bytes. `peer_idx == PEER_SELF`.
    Write = 0,
    /// A `receive` event: the variant's reader observed `seq` from
    /// `peer_idx` on `path` at `qos` of `bytes` bytes.
    Receive = 1,
    /// A `backpressure_skipped` event: `try_publish` returned
    /// `Ok(false)` for an op the driver intended to write. `seq` is
    /// 0 (the skipped op never got a seq), `bytes` is 0.
    BackpressureSkipped = 2,
    /// A `gap_detected` event: the reader noticed `seq` is missing
    /// in the stream from `peer_idx` (QoS 3 only). `bytes` is 0.
    GapDetected = 3,
    /// A `gap_filled` event: a previously-detected gap was recovered
    /// (QoS 3 only). `bytes` is 0.
    GapFilled = 4,
}

impl EventKind {
    /// Return the canonical string name for this kind. Matches the
    /// `event` field of the legacy JSONL stream so analysis code can
    /// cross-correlate the two on the same identifiers.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Write => "write",
            EventKind::Receive => "receive",
            EventKind::BackpressureSkipped => "backpressure_skipped",
            EventKind::GapDetected => "gap_detected",
            EventKind::GapFilled => "gap_filled",
        }
    }
}

impl From<EventKind> for u8 {
    fn from(kind: EventKind) -> u8 {
        kind as u8
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors that can arise from [`InternTable`] operations and from
/// pushing rows into [`CompactBuffers`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InternError {
    /// Path intern table reached [`MAX_PATHS`]. Indicates a benchmark
    /// configuration mistake -- realistic workloads use single-digit
    /// to a few-hundred distinct paths.
    #[error("path intern table is full (cap = {0})")]
    PathTableFull(u32),
    /// Peer intern table reached [`MAX_PEERS`]. Indicates that
    /// `peer_name` is being derived from per-message data instead of
    /// the runner-injected `--peers` set.
    #[error("peer intern table is full (cap = {0})")]
    PeerTableFull(u8),
}

/// A monotonically-growing intern table for path strings.
///
/// Keyed by exact-match `&str`; case is preserved verbatim. The
/// `intern` operation is `O(1)` average (`HashMap` lookup) for repeat
/// strings and `O(len)` for new ones (the string is cloned into the
/// owned dictionary). Both the lookup and insert paths produce the
/// same `u32` index for the same string within a process.
///
/// The intern table is intentionally a separate type from
/// [`CompactBuffers`] so it can be unit-tested in isolation and so
/// the Parquet writer can borrow just the dictionary slice without
/// having to lock the entire buffer.
#[derive(Debug, Default)]
pub struct PathInterner {
    /// String -> index. Two-way mapping: `dict[index] == string`.
    lookup: HashMap<String, u32>,
    /// index -> string. Dense vec so dictionary export is a cheap
    /// `slice::iter().cloned()`.
    dict: Vec<String>,
}

impl PathInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct paths interned so far.
    pub fn len(&self) -> u32 {
        self.dict.len() as u32
    }

    /// True iff no paths have been interned.
    pub fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }

    /// Intern `path` and return its index. On first sight allocates a
    /// new index and clones the string into the dictionary; on
    /// subsequent sights returns the cached index. Returns
    /// [`InternError::PathTableFull`] when the cap is exceeded.
    pub fn intern(&mut self, path: &str) -> Result<u32, InternError> {
        if let Some(&idx) = self.lookup.get(path) {
            return Ok(idx);
        }
        if self.dict.len() as u64 >= u64::from(MAX_PATHS) {
            return Err(InternError::PathTableFull(MAX_PATHS));
        }
        let idx = self.dict.len() as u32;
        self.dict.push(path.to_string());
        self.lookup.insert(path.to_string(), idx);
        Ok(idx)
    }

    /// Borrow the dictionary as a slice in insertion order. The
    /// Parquet writer iterates this directly to populate the
    /// `paths` key in the file's KV metadata.
    pub fn dict(&self) -> &[String] {
        &self.dict
    }

    /// Heap-bytes upper bound used by [`CompactBuffers::approx_bytes`].
    /// Counts the dictionary string lengths plus a 64-byte
    /// per-entry overhead estimate to cover `Vec<String>` capacity
    /// pre-allocation and the `HashMap` keys (which are owned `String`s
    /// stored separately from `dict`).
    pub fn approx_bytes(&self) -> usize {
        self.dict.iter().map(|s| s.len()).sum::<usize>() + self.dict.len() * 64
    }
}

/// A monotonically-growing intern table for peer (writer) names.
///
/// Mirrors [`PathInterner`] but caps at `u8::MAX - 1` so the `peer_idx`
/// column can use `u8::MAX` ([`PEER_SELF`]) as the "no peer" sentinel.
#[derive(Debug, Default)]
pub struct PeerInterner {
    lookup: HashMap<String, u8>,
    dict: Vec<String>,
}

impl PeerInterner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> u8 {
        self.dict.len() as u8
    }

    pub fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }

    /// Intern `peer` and return its index. Returns
    /// [`InternError::PeerTableFull`] when the cap is exceeded.
    pub fn intern(&mut self, peer: &str) -> Result<u8, InternError> {
        if let Some(&idx) = self.lookup.get(peer) {
            return Ok(idx);
        }
        if self.dict.len() >= MAX_PEERS as usize {
            return Err(InternError::PeerTableFull(MAX_PEERS));
        }
        let idx = self.dict.len() as u8;
        self.dict.push(peer.to_string());
        self.lookup.insert(peer.to_string(), idx);
        Ok(idx)
    }

    pub fn dict(&self) -> &[String] {
        &self.dict
    }

    pub fn approx_bytes(&self) -> usize {
        self.dict.iter().map(|s| s.len()).sum::<usize>() + self.dict.len() * 64
    }
}

/// In-memory columnar buffers for per-event data.
///
/// One instance per spawn. Owns its intern tables. All `push_*`
/// methods are infallible-on-capacity (the underlying `Vec`s grow as
/// needed); the only failure mode is exhaustion of the intern
/// tables, surfaced as [`InternError`].
///
/// Single-threaded: not `Sync`. Variants whose reader threads need to
/// emit `receive` events into the buffers must funnel them through a
/// mutex on the driver side (analogous to how the legacy
/// [`crate::logger::LoggerHandle`] works).
#[derive(Debug, Default)]
pub struct CompactBuffers {
    pub paths: PathInterner,
    pub peers: PeerInterner,

    // Columnar event rows.
    pub ts_ns: Vec<i64>,
    pub kind: Vec<u8>,
    pub seq: Vec<u64>,
    pub path_idx: Vec<u32>,
    pub peer_idx: Vec<u8>,
    pub qos: Vec<u8>,
    pub bytes: Vec<u32>,
}

impl CompactBuffers {
    /// Construct an empty set of buffers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of events accumulated so far. All column `Vec`s have
    /// this length post-push (the per-column lengths are kept in
    /// lockstep by the `push_*` methods).
    pub fn len(&self) -> usize {
        self.ts_ns.len()
    }

    /// True when no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.ts_ns.is_empty()
    }

    /// Coarse upper bound on the in-memory footprint, in bytes. Used
    /// by the driver to fire the [`--digest-mem-soft-mb`] warning and
    /// [`--digest-mem-hard-mb`] abort thresholds.
    pub fn approx_bytes(&self) -> usize {
        self.len() * ROW_BYTES_ESTIMATE + self.paths.approx_bytes() + self.peers.approx_bytes()
    }

    /// Push a `write` event row. The driver supplies the pre-publish
    /// `write_ts` here (matching the same T16.2 invariant as the
    /// JSONL `log_write_at`).
    pub fn push_write(
        &mut self,
        ts_ns: i64,
        path: &str,
        qos: u8,
        seq: u64,
        bytes: u32,
    ) -> Result<(), InternError> {
        let path_idx = self.paths.intern(path)?;
        self.push_row(
            ts_ns,
            EventKind::Write,
            seq,
            path_idx,
            PEER_SELF,
            qos,
            bytes,
        );
        Ok(())
    }

    /// Push a `receive` event row.
    pub fn push_receive(
        &mut self,
        ts_ns: i64,
        writer: &str,
        seq: u64,
        path: &str,
        qos: u8,
        bytes: u32,
    ) -> Result<(), InternError> {
        let path_idx = self.paths.intern(path)?;
        let peer_idx = self.peers.intern(writer)?;
        self.push_row(
            ts_ns,
            EventKind::Receive,
            seq,
            path_idx,
            peer_idx,
            qos,
            bytes,
        );
        Ok(())
    }

    /// Push a `backpressure_skipped` event row.
    pub fn push_backpressure_skipped(
        &mut self,
        ts_ns: i64,
        path: &str,
        qos: u8,
    ) -> Result<(), InternError> {
        let path_idx = self.paths.intern(path)?;
        self.push_row(
            ts_ns,
            EventKind::BackpressureSkipped,
            0,
            path_idx,
            PEER_SELF,
            qos,
            0,
        );
        Ok(())
    }

    /// Push a `gap_detected` event row.
    pub fn push_gap_detected(
        &mut self,
        ts_ns: i64,
        writer: &str,
        missing_seq: u64,
    ) -> Result<(), InternError> {
        let peer_idx = self.peers.intern(writer)?;
        // No path on gap events -- use 0 as a placeholder. Analysis
        // never reads path_idx for gap kinds so the value is
        // unconstrained.
        self.push_row(
            ts_ns,
            EventKind::GapDetected,
            missing_seq,
            0,
            peer_idx,
            0,
            0,
        );
        Ok(())
    }

    /// Push a `gap_filled` event row.
    pub fn push_gap_filled(
        &mut self,
        ts_ns: i64,
        writer: &str,
        recovered_seq: u64,
    ) -> Result<(), InternError> {
        let peer_idx = self.peers.intern(writer)?;
        self.push_row(
            ts_ns,
            EventKind::GapFilled,
            recovered_seq,
            0,
            peer_idx,
            0,
            0,
        );
        Ok(())
    }

    /// Append one row to every column. Private; all public push
    /// methods go through this so the column lengths stay in lockstep.
    #[allow(clippy::too_many_arguments)]
    fn push_row(
        &mut self,
        ts_ns: i64,
        kind: EventKind,
        seq: u64,
        path_idx: u32,
        peer_idx: u8,
        qos: u8,
        bytes: u32,
    ) {
        self.ts_ns.push(ts_ns);
        self.kind.push(kind.into());
        self.seq.push(seq);
        self.path_idx.push(path_idx);
        self.peer_idx.push(peer_idx);
        self.qos.push(qos);
        self.bytes.push(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffers_have_zero_length() {
        let buf = CompactBuffers::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert!(buf.paths.is_empty());
        assert!(buf.peers.is_empty());
    }

    #[test]
    fn path_interner_returns_same_index_for_repeated_strings() {
        let mut interner = PathInterner::new();
        let a = interner.intern("/bench/0").unwrap();
        let b = interner.intern("/bench/0").unwrap();
        let c = interner.intern("/bench/1").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(interner.len(), 2);
        assert_eq!(
            interner.dict(),
            &["/bench/0".to_string(), "/bench/1".to_string()]
        );
    }

    #[test]
    fn path_interner_assigns_monotonically_increasing_indices() {
        let mut interner = PathInterner::new();
        for i in 0..100 {
            let path = format!("/p{i}");
            let idx = interner.intern(&path).unwrap();
            assert_eq!(idx, i as u32);
        }
        assert_eq!(interner.len(), 100);
    }

    #[test]
    fn peer_interner_returns_same_index_for_repeated_strings() {
        let mut interner = PeerInterner::new();
        let alice = interner.intern("alice").unwrap();
        let bob = interner.intern("bob").unwrap();
        let alice2 = interner.intern("alice").unwrap();
        assert_eq!(alice, alice2);
        assert_ne!(alice, bob);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn peer_interner_rejects_beyond_cap() {
        let mut interner = PeerInterner::new();
        // Fill to MAX_PEERS (254 entries).
        for i in 0..MAX_PEERS {
            let name = format!("p{i}");
            interner.intern(&name).unwrap();
        }
        assert_eq!(interner.len(), MAX_PEERS);
        // The (MAX_PEERS + 1)-th distinct peer must error.
        let err = interner.intern("overflow").unwrap_err();
        assert_eq!(err, InternError::PeerTableFull(MAX_PEERS));
    }

    #[test]
    fn peer_self_sentinel_is_u8_max() {
        // The contract: PEER_SELF must be representable and must NOT
        // be a valid peer index returned by the interner. The
        // interner caps below it precisely so analysis can use the
        // sentinel unambiguously.
        assert_eq!(PEER_SELF, 255);
        assert_eq!(MAX_PEERS, 254);
    }

    #[test]
    fn push_write_populates_columns() {
        let mut buf = CompactBuffers::new();
        buf.push_write(1_000_000_000, "/bench/0", 1, 42, 128)
            .unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.ts_ns, vec![1_000_000_000]);
        assert_eq!(buf.kind, vec![EventKind::Write as u8]);
        assert_eq!(buf.seq, vec![42]);
        assert_eq!(buf.path_idx, vec![0]);
        assert_eq!(buf.peer_idx, vec![PEER_SELF]);
        assert_eq!(buf.qos, vec![1]);
        assert_eq!(buf.bytes, vec![128]);
        assert_eq!(buf.paths.dict(), &["/bench/0".to_string()]);
    }

    #[test]
    fn push_receive_populates_columns_and_interns_peer() {
        let mut buf = CompactBuffers::new();
        buf.push_receive(2_000_000_000, "alice", 7, "/bench/0", 4, 256)
            .unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.kind, vec![EventKind::Receive as u8]);
        assert_eq!(buf.peer_idx, vec![0]);
        assert_eq!(buf.peers.dict(), &["alice".to_string()]);
    }

    #[test]
    fn push_backpressure_skipped_uses_self_sentinel() {
        let mut buf = CompactBuffers::new();
        buf.push_backpressure_skipped(3_000_000_000, "/bench/0", 4)
            .unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.kind, vec![EventKind::BackpressureSkipped as u8]);
        assert_eq!(buf.peer_idx, vec![PEER_SELF]);
        assert_eq!(buf.seq, vec![0]);
        assert_eq!(buf.bytes, vec![0]);
    }

    #[test]
    fn push_gap_events_intern_writer_and_record_seq() {
        let mut buf = CompactBuffers::new();
        buf.push_gap_detected(4_000_000_000, "bob", 999).unwrap();
        buf.push_gap_filled(4_500_000_000, "bob", 999).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.kind[0], EventKind::GapDetected as u8);
        assert_eq!(buf.kind[1], EventKind::GapFilled as u8);
        assert_eq!(buf.seq, vec![999, 999]);
        assert_eq!(buf.peer_idx, vec![0, 0]);
        assert_eq!(buf.peers.dict(), &["bob".to_string()]);
    }

    #[test]
    fn mixed_pushes_keep_column_lengths_in_lockstep() {
        let mut buf = CompactBuffers::new();
        buf.push_write(1, "/a", 1, 1, 8).unwrap();
        buf.push_receive(2, "alice", 1, "/a", 1, 8).unwrap();
        buf.push_backpressure_skipped(3, "/b", 4).unwrap();
        buf.push_gap_detected(4, "bob", 7).unwrap();
        buf.push_gap_filled(5, "bob", 7).unwrap();

        let n = buf.len();
        assert_eq!(n, 5);
        assert_eq!(buf.ts_ns.len(), n);
        assert_eq!(buf.kind.len(), n);
        assert_eq!(buf.seq.len(), n);
        assert_eq!(buf.path_idx.len(), n);
        assert_eq!(buf.peer_idx.len(), n);
        assert_eq!(buf.qos.len(), n);
        assert_eq!(buf.bytes.len(), n);
        // Two paths interned (/a, /b); two peers (alice, bob).
        assert_eq!(buf.paths.dict(), &["/a".to_string(), "/b".to_string()]);
        assert_eq!(buf.peers.dict(), &["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn approx_bytes_grows_with_rows_and_dictionaries() {
        let mut buf = CompactBuffers::new();
        let baseline = buf.approx_bytes();
        for i in 0..1000 {
            buf.push_write(i as i64, "/p", 1, i as u64, 8).unwrap();
        }
        let after_rows = buf.approx_bytes();
        // 1000 rows + one path entry; should grow by at least
        // 1000 * ROW_BYTES_ESTIMATE.
        assert!(
            after_rows - baseline >= 1000 * ROW_BYTES_ESTIMATE,
            "approx_bytes did not grow by row contribution: {baseline} -> {after_rows}"
        );
    }

    #[test]
    fn event_kind_names_match_legacy_jsonl_event_strings() {
        // Same identifiers as the JSONL `event` field so analysis can
        // cross-correlate without an additional translation table.
        assert_eq!(EventKind::Write.as_str(), "write");
        assert_eq!(EventKind::Receive.as_str(), "receive");
        assert_eq!(
            EventKind::BackpressureSkipped.as_str(),
            "backpressure_skipped"
        );
        assert_eq!(EventKind::GapDetected.as_str(), "gap_detected");
        assert_eq!(EventKind::GapFilled.as_str(), "gap_filled");
    }

    #[test]
    fn event_kind_discriminants_are_stable() {
        // The numeric encoding is part of the on-disk Parquet wire
        // format. Renumbering would break analysis that's already
        // parsing previously-produced files. Pin the values here.
        assert_eq!(EventKind::Write as u8, 0);
        assert_eq!(EventKind::Receive as u8, 1);
        assert_eq!(EventKind::BackpressureSkipped as u8, 2);
        assert_eq!(EventKind::GapDetected as u8, 3);
        assert_eq!(EventKind::GapFilled as u8, 4);
    }
}
