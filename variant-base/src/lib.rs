pub mod build_info;
pub mod cli;
pub mod compact;
pub mod compact_writer;
pub mod driver;
pub mod dummy;
pub mod logger;
pub mod progress_emitter;
pub mod resource;
pub mod seq;
pub mod socket;
pub mod types;
pub mod variant_trait;
pub mod watchdog;
pub mod workload;

// Re-export primary types for convenient access.
pub use cli::CliArgs;
pub use compact::{
    CompactBuffers, EventKind, InternError, PathInterner, PeerInterner, MAX_PATHS, MAX_PEERS,
    PEER_SELF, ROW_BYTES_ESTIMATE,
};
pub use compact_writer::{write_compact_parquet, CompactParquetMeta, CompactWriterError};
pub use dummy::VariantDummy;
pub use logger::{CompactSink, Logger, LoggerHandle};
pub use progress_emitter::{build_progress_line, ProgressEmitter, ProgressSnapshot, DONE_PHASE};
pub use resource::ResourceMonitor;
pub use seq::SeqGenerator;
pub use socket::{tune_udp_buffers, tune_udp_buffers_std};
pub use types::{Phase, Qos, ReceivedUpdate, ThreadingMode, ThreadingModeParseError};
pub use variant_trait::Variant;
pub use watchdog::{Watchdog, WATCHDOG_EXIT_CODE, WATCHDOG_STDERR_PREFIX};
pub use workload::{
    create_workload, create_workload_with_params, BlockFlood, MixedTypes, ScalarFlood, Workload,
    WorkloadParams, WriteOp, WriteShape, SHAPE_INTERN,
};
