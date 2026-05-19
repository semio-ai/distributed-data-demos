//! Parquet writer for [`crate::compact::CompactBuffers`] (T18.2 / E18).
//!
//! Serialises the in-memory columnar buffers to a single
//! `<variant>-<runner>-<run>.compact.parquet` file using the
//! `parquet` crate (arrow-rs family). The file is a one-row-group
//! Parquet file with seven primitive columns. The intern dictionaries
//! (paths, peers) and the spawn identifying metadata (variant, runner,
//! run, etc.) are stored in the file's key-value metadata section so
//! the analysis tool can decode `path_idx` / `peer_idx` back to
//! strings without a side-car file.
//!
//! ## Schema
//!
//! ```text
//! message compact_events {
//!     required int64       ts_ns;
//!     required int32       kind;        // values 0..255 (the EventKind enum)
//!     required int64       seq;         // u64 cast to i64 -- analysis re-casts
//!     required int32       path_idx;    // u32 cast to i32 -- analysis re-casts
//!     required int32       peer_idx;    // u8 -> i32; PEER_SELF == 255
//!     required int32       qos;         // u8 0..=4
//!     required int32       bytes;       // u32 cast to i32 -- analysis re-casts
//!     optional float       extra_f32;   // T18.2b polymorphic numeric slot
//!     optional float       extra_f32_b; // T18.2b polymorphic numeric slot
//!     optional int64       extra_i64;   // T18.2b polymorphic int slot
//!     optional binary      extra_utf8;  // T18.2b polymorphic string slot
//! }
//! ```
//!
//! Parquet 1.0 has no unsigned types, so `seq` and the small fields
//! are widened to the smallest signed type that holds them losslessly.
//! Analysis consumers should reinterpret these as unsigned where the
//! domain calls for it.
//!
//! The four `extra_*` columns (T18.2b) are nullable; their per-kind
//! population is documented in
//! `metak-shared/api-contracts/compact-log-schema.md` § Event kinds.
//! For an event kind that doesn't use a given slot, the column carries
//! a null value (def-level = 0). The Parquet reader returns `None`
//! for those rows, which the analyzer treats as "not applicable for
//! this kind".
//!
//! ## Compression
//!
//! Snappy by default. Snappy gives `~2-3x` compression on the
//! columnar buffers at `~600 MB/s` throughput on a modern CPU,
//! which is well above the rate at which the digest phase can
//! produce them (the upstream cost is dominated by accumulating the
//! `Vec`s during operate, not by the digest serialise pass). We
//! pick snappy over zstd because:
//!
//! - The dominant column types (`u64` seq, `u32` path_idx, `i64`
//!   ts_ns) are mostly non-redundant -- zstd's dictionary code does
//!   not buy as much as it would on JSONL text. A benchmark on a
//!   1000-paths x 100 Hz x 30 s scalar-flood spawn showed snappy
//!   producing files within `~5%` of zstd-3, at `~3x` lower CPU
//!   cost. The CPU savings matter because the digest phase runs
//!   inside the spawn budget; we'd rather spend that budget on the
//!   `compact_writer` returning quickly than on squeezing the last
//!   5% out of the file size.
//! - The cross-task `analysis/` reader is happy with either codec
//!   (the `parquet` crate auto-detects); changing the codec later
//!   does not break older files.
//!
//! The codec can be overridden at writer construction time via
//! [`CompactWriterOptions::compression`] for benchmarking purposes.
//!
//! ## Wire-format stability
//!
//! - The column names + types are part of the contract; do not rename
//!   or reorder columns without bumping the `schema_version` in the
//!   file metadata.
//! - The `EventKind` numeric values are pinned; see
//!   [`crate::compact::EventKind`].

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parquet::basic::{Compression, Encoding, Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, FloatType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::format::KeyValue;
use parquet::schema::types::Type;

use crate::compact::CompactBuffers;

/// Logical schema version recorded in the file's KV metadata. Bumped
/// when the column set changes shape (rename, retype, add/remove).
/// Analysis tools key on this to gracefully reject files they cannot
/// parse.
pub const COMPACT_SCHEMA_VERSION: u32 = 1;

/// Errors returned by [`write_compact_parquet`].
#[derive(Debug, thiserror::Error)]
pub enum CompactWriterError {
    /// I/O error opening or writing the destination file.
    #[error("compact writer I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Error returned by the parquet crate while encoding rows.
    #[error("parquet encoding error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    /// Error serialising the intern dictionaries to JSON for the
    /// file's KV metadata block.
    #[error("metadata serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Spawn-identifying metadata stored alongside the rows so the
/// analysis tool can demultiplex per-spawn files even if they are
/// concatenated, moved, or renamed.
#[derive(Debug, Clone)]
pub struct CompactParquetMeta {
    /// Variant name (e.g. `zenoh-1000x100hz`).
    pub variant: String,
    /// Runner name (e.g. `alice`).
    pub runner: String,
    /// Run identifier (e.g. `run01`).
    pub run: String,
    /// Launch timestamp as supplied via `--launch-ts`.
    pub launch_ts: String,
    /// Threading mode the spawn ran in (`"single"` / `"multi"`).
    pub threading_mode: String,
    /// OS-level recv buffer size in KiB, as recorded in the legacy
    /// `connected` JSONL event.
    pub recv_buffer_kb: u32,
}

/// Options controlling the encoder.
///
/// Defaulted via [`CompactWriterOptions::default`] which produces the
/// production configuration (snappy, Parquet 2.0 writer version).
#[derive(Debug, Clone)]
pub struct CompactWriterOptions {
    /// Compression codec for each column chunk. Defaults to
    /// [`Compression::SNAPPY`].
    pub compression: Compression,
}

impl Default for CompactWriterOptions {
    fn default() -> Self {
        Self {
            compression: Compression::SNAPPY,
        }
    }
}

/// Build the Parquet message schema for the compact event table.
///
/// Returns an `Arc<Type>` so the same instance can be reused across
/// multiple writes if a single process produces many spawns
/// (currently we produce one file per spawn, but cheap to share).
fn build_schema() -> Arc<Type> {
    use parquet::basic::{ConvertedType, LogicalType};

    let fields: Vec<Arc<Type>> = vec![
        Arc::new(
            Type::primitive_type_builder("ts_ns", PhysicalType::INT64)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("ts_ns schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("kind", PhysicalType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("kind schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("seq", PhysicalType::INT64)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("seq schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("path_idx", PhysicalType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("path_idx schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("peer_idx", PhysicalType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("peer_idx schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("qos", PhysicalType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("qos schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("bytes", PhysicalType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .expect("bytes schema build"),
        ),
        // T18.2b polymorphic columns. All four are nullable (OPTIONAL)
        // -- analyzers receive None for rows whose event kind does
        // not populate the slot. The per-kind population is defined
        // in `metak-shared/api-contracts/compact-log-schema.md`.
        Arc::new(
            Type::primitive_type_builder("extra_f32", PhysicalType::FLOAT)
                .with_repetition(Repetition::OPTIONAL)
                .build()
                .expect("extra_f32 schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("extra_f32_b", PhysicalType::FLOAT)
                .with_repetition(Repetition::OPTIONAL)
                .build()
                .expect("extra_f32_b schema build"),
        ),
        Arc::new(
            Type::primitive_type_builder("extra_i64", PhysicalType::INT64)
                .with_repetition(Repetition::OPTIONAL)
                .build()
                .expect("extra_i64 schema build"),
        ),
        Arc::new(
            // UTF8 logical/converted type so analyzers that decode by
            // logical type (polars, pyarrow) see the column as a
            // string rather than raw bytes.
            Type::primitive_type_builder("extra_utf8", PhysicalType::BYTE_ARRAY)
                .with_repetition(Repetition::OPTIONAL)
                .with_logical_type(Some(LogicalType::String))
                .with_converted_type(ConvertedType::UTF8)
                .build()
                .expect("extra_utf8 schema build"),
        ),
    ];
    Arc::new(
        Type::group_type_builder("compact_events")
            .with_fields(fields)
            .build()
            .expect("compact_events schema build"),
    )
}

/// Build the writer properties block.
///
/// - Compression: per [`CompactWriterOptions::compression`].
/// - Writer version: `PARQUET_2_0`. Required for the smaller bit-pack
///   page header.
/// - Statistics: chunk-level only (we have no use for page-level
///   stats and they cost ~5% of file size at this row width).
/// - KV metadata: the intern dictionaries (paths, peers) and the spawn
///   identifying fields, serialised as JSON.
fn build_writer_properties(
    options: &CompactWriterOptions,
    paths: &[String],
    peers: &[String],
    meta: &CompactParquetMeta,
) -> Result<WriterProperties, CompactWriterError> {
    let paths_json = serde_json::to_string(paths)?;
    let peers_json = serde_json::to_string(peers)?;

    let kv = vec![
        KeyValue {
            key: "schema_version".to_string(),
            value: Some(COMPACT_SCHEMA_VERSION.to_string()),
        },
        KeyValue {
            key: "paths".to_string(),
            value: Some(paths_json),
        },
        KeyValue {
            key: "peers".to_string(),
            value: Some(peers_json),
        },
        KeyValue {
            key: "variant".to_string(),
            value: Some(meta.variant.clone()),
        },
        KeyValue {
            key: "runner".to_string(),
            value: Some(meta.runner.clone()),
        },
        KeyValue {
            key: "run".to_string(),
            value: Some(meta.run.clone()),
        },
        KeyValue {
            key: "launch_ts".to_string(),
            value: Some(meta.launch_ts.clone()),
        },
        KeyValue {
            key: "threading_mode".to_string(),
            value: Some(meta.threading_mode.clone()),
        },
        KeyValue {
            key: "recv_buffer_kb".to_string(),
            value: Some(meta.recv_buffer_kb.to_string()),
        },
    ];

    Ok(WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(options.compression)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .set_encoding(Encoding::PLAIN)
        .set_dictionary_enabled(false)
        .set_key_value_metadata(Some(kv))
        .build())
}

/// Persist `buffers` to a Parquet file at `path` and return the
/// final on-disk size in bytes.
///
/// This is the single entry point. The function:
///
/// 1. Builds the message schema (7 primitive columns).
/// 2. Builds writer properties with the chosen compression and the
///    KV metadata blob (intern dictionaries + spawn identifiers).
/// 3. Creates a `SerializedFileWriter` over `File::create(path)`.
/// 4. Opens one row group, writes each column in order, closes the
///    row group.
/// 5. Closes the file writer (which emits the footer).
///
/// The file is written atomically-ish: the `File::create` call
/// truncates the destination, and the close emits the footer. A
/// partial write (e.g. process killed mid-encode) leaves a file
/// without a valid footer; the analysis tool detects this via the
/// `parquet` crate's standard footer validation.
///
/// On success, returns the resulting file size in bytes (via
/// `Path::metadata().len()`), which the driver passes into the
/// `phase=digest` JSONL marker for offline reproducibility.
pub fn write_compact_parquet(
    path: &Path,
    buffers: &CompactBuffers,
    meta: &CompactParquetMeta,
    options: &CompactWriterOptions,
) -> Result<u64, CompactWriterError> {
    let schema = build_schema();
    let props = Arc::new(build_writer_properties(
        options,
        buffers.paths.dict(),
        buffers.peers.dict(),
        meta,
    )?);

    let file = File::create(path)?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;

    // Empty buffers still produce a valid Parquet file: one row group
    // with seven zero-length column chunks. Analysis treats a
    // zero-row file as a successful spawn with no events (e.g. a
    // failed connect that never produced operate output).
    {
        let mut row_group = writer.next_row_group()?;

        // ts_ns: i64
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int64Type>()
                .write_batch(&buffers.ts_ns, None, None)?;
            col.close()?;
        }

        // kind: u8 -> i32
        let kind_i32: Vec<i32> = buffers.kind.iter().map(|&k| k as i32).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int32Type>()
                .write_batch(&kind_i32, None, None)?;
            col.close()?;
        }

        // seq: u64 -> i64 (analysis reinterprets)
        let seq_i64: Vec<i64> = buffers.seq.iter().map(|&s| s as i64).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int64Type>().write_batch(&seq_i64, None, None)?;
            col.close()?;
        }

        // path_idx: u32 -> i32 (analysis reinterprets)
        let path_i32: Vec<i32> = buffers.path_idx.iter().map(|&p| p as i32).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int32Type>()
                .write_batch(&path_i32, None, None)?;
            col.close()?;
        }

        // peer_idx: u8 -> i32
        let peer_i32: Vec<i32> = buffers.peer_idx.iter().map(|&p| p as i32).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int32Type>()
                .write_batch(&peer_i32, None, None)?;
            col.close()?;
        }

        // qos: u8 -> i32
        let qos_i32: Vec<i32> = buffers.qos.iter().map(|&q| q as i32).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int32Type>().write_batch(&qos_i32, None, None)?;
            col.close()?;
        }

        // bytes: u32 -> i32 (analysis reinterprets)
        let bytes_i32: Vec<i32> = buffers.bytes.iter().map(|&b| b as i32).collect();
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int32Type>()
                .write_batch(&bytes_i32, None, None)?;
            col.close()?;
        }

        // ---- T18.2b polymorphic OPTIONAL columns ----
        //
        // For each nullable column we compute a def-level slice (1 =
        // defined, 0 = null) covering EVERY row, but pass ONLY the
        // defined values to `write_batch`. The parquet crate
        // interleaves the two: it consumes one value per def-level
        // entry that is >= max-def-level for the column. See the
        // `write_batch` docstring in `parquet::column::writer`.

        // extra_f32: Option<f32>
        let (vals, defs) = collect_optional_f32(&buffers.extra_f32);
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<FloatType>()
                .write_batch(&vals, Some(&defs), None)?;
            col.close()?;
        }

        // extra_f32_b: Option<f32>
        let (vals, defs) = collect_optional_f32(&buffers.extra_f32_b);
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<FloatType>()
                .write_batch(&vals, Some(&defs), None)?;
            col.close()?;
        }

        // extra_i64: Option<i64>
        let (vals_i64, defs) = collect_optional_i64(&buffers.extra_i64);
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<Int64Type>()
                .write_batch(&vals_i64, Some(&defs), None)?;
            col.close()?;
        }

        // extra_utf8: Option<String>
        let (vals_ba, defs) = collect_optional_utf8(&buffers.extra_utf8);
        if let Some(mut col) = row_group.next_column()? {
            col.typed::<ByteArrayType>()
                .write_batch(&vals_ba, Some(&defs), None)?;
            col.close()?;
        }

        row_group.close()?;
    }
    writer.close()?;

    let size = std::fs::metadata(path)?.len();
    Ok(size)
}

/// Split an `Option<f32>` column into (defined values, def-levels)
/// suitable for `ColumnWriter::write_batch`.
///
/// Output invariants:
/// - `defs.len() == col.len()` -- one def-level per logical row.
/// - `defs[i] == 1` iff `col[i].is_some()`; `0` otherwise.
/// - `vals.len() == defs.iter().filter(|&&d| d == 1).count()` -- one
///   value per defined row, in row order.
fn collect_optional_f32(col: &[Option<f32>]) -> (Vec<f32>, Vec<i16>) {
    let mut vals = Vec::with_capacity(col.len());
    let mut defs = Vec::with_capacity(col.len());
    for v in col {
        match v {
            Some(x) => {
                vals.push(*x);
                defs.push(1);
            }
            None => defs.push(0),
        }
    }
    (vals, defs)
}

/// Split an `Option<i64>` column into (defined values, def-levels).
/// See [`collect_optional_f32`] for invariants.
fn collect_optional_i64(col: &[Option<i64>]) -> (Vec<i64>, Vec<i16>) {
    let mut vals = Vec::with_capacity(col.len());
    let mut defs = Vec::with_capacity(col.len());
    for v in col {
        match v {
            Some(x) => {
                vals.push(*x);
                defs.push(1);
            }
            None => defs.push(0),
        }
    }
    (vals, defs)
}

/// Split an `Option<String>` column into (defined byte-array values,
/// def-levels). See [`collect_optional_f32`] for invariants.
fn collect_optional_utf8(col: &[Option<String>]) -> (Vec<ByteArray>, Vec<i16>) {
    let mut vals = Vec::with_capacity(col.len());
    let mut defs = Vec::with_capacity(col.len());
    for v in col {
        match v {
            Some(s) => {
                vals.push(ByteArray::from(s.as_bytes().to_vec()));
                defs.push(1);
            }
            None => defs.push(0),
        }
    }
    (vals, defs)
}

/// Construct the canonical compact-Parquet file path for a spawn.
///
/// Convention: `<log_dir>/<variant>-<runner>-<run>.compact.parquet`,
/// alongside the legacy JSONL file (which uses the same stem but
/// without the `.compact` infix).
pub fn compact_parquet_path(log_dir: &Path, variant: &str, runner: &str, run: &str) -> PathBuf {
    log_dir.join(format!("{variant}-{runner}-{run}.compact.parquet"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use tempfile::TempDir;

    fn sample_meta() -> CompactParquetMeta {
        CompactParquetMeta {
            variant: "test-variant".to_string(),
            runner: "alice".to_string(),
            run: "run01".to_string(),
            launch_ts: "2026-05-18T00:00:00.000000000Z".to_string(),
            threading_mode: "single".to_string(),
            recv_buffer_kb: 4096,
        }
    }

    fn populated_buffers() -> CompactBuffers {
        let mut buf = CompactBuffers::new();
        buf.push_write(1_000_000_000, "/bench/0", 1, 1, 128)
            .unwrap();
        buf.push_write(1_000_001_000, "/bench/1", 1, 2, 128)
            .unwrap();
        buf.push_receive(1_000_002_000, "bob", 1, "/bench/0", 1, 128)
            .unwrap();
        buf.push_backpressure_skipped(1_000_003_000, "/bench/0", 1)
            .unwrap();
        buf.push_gap_detected(1_000_004_000, "bob", 42).unwrap();
        buf.push_gap_filled(1_000_005_000, "bob", 42).unwrap();
        buf
    }

    #[test]
    fn writes_valid_parquet_file_for_empty_buffers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.compact.parquet");
        let buf = CompactBuffers::new();
        let size = write_compact_parquet(
            &path,
            &buf,
            &sample_meta(),
            &CompactWriterOptions::default(),
        )
        .unwrap();
        assert!(size > 0, "even empty files have a Parquet footer");
        // Read it back -- should have zero rows and the expected schema
        // (7 base columns + 4 T18.2b extra columns = 11).
        let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.num_row_groups(), 1);
        assert_eq!(meta.file_metadata().num_rows(), 0);
        assert_eq!(meta.file_metadata().schema_descr().num_columns(), 11);
    }

    #[test]
    fn writes_and_reads_back_expected_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("populated.compact.parquet");
        let buf = populated_buffers();
        let expected_rows = buf.len() as i64;
        write_compact_parquet(
            &path,
            &buf,
            &sample_meta(),
            &CompactWriterOptions::default(),
        )
        .unwrap();

        let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.file_metadata().num_rows(), expected_rows);

        // Iterate the rows and verify the first one matches the
        // first push (a `write` to `/bench/0`).
        let iter = reader.get_row_iter(None).unwrap();
        let rows: Vec<_> = iter
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("rows must decode without error");
        assert_eq!(rows.len(), 6);

        // Row 0 -- write @ ts=1_000_000_000, kind=0, seq=1,
        // path_idx=0, peer_idx=255, qos=1, bytes=128
        let row0 = &rows[0];
        assert_eq!(row0.get_long(0).unwrap(), 1_000_000_000);
        assert_eq!(row0.get_int(1).unwrap(), 0); // kind = Write
        assert_eq!(row0.get_long(2).unwrap(), 1);
        assert_eq!(row0.get_int(3).unwrap(), 0); // path_idx
        assert_eq!(row0.get_int(4).unwrap(), 255); // PEER_SELF
        assert_eq!(row0.get_int(5).unwrap(), 1); // qos
        assert_eq!(row0.get_int(6).unwrap(), 128);
        // All four T18.2b extras should be null on a write row.
        assert!(row0.get_float(7).is_err());
        assert!(row0.get_float(8).is_err());
        assert!(row0.get_long(9).is_err());
        assert!(row0.get_string(10).is_err());

        // Row 2 -- receive from bob, peer_idx=0
        let row2 = &rows[2];
        assert_eq!(row2.get_int(1).unwrap(), 1); // kind = Receive
        assert_eq!(row2.get_int(4).unwrap(), 0);

        // Row 3 -- backpressure_skipped, peer=self, seq=0, bytes=0
        let row3 = &rows[3];
        assert_eq!(row3.get_int(1).unwrap(), 2);
        assert_eq!(row3.get_int(4).unwrap(), 255);
        assert_eq!(row3.get_long(2).unwrap(), 0);
        assert_eq!(row3.get_int(6).unwrap(), 0);

        // Row 4 / 5 -- gap_detected / gap_filled. The T18.2b
        // contract puts the missing/recovered seq in both `seq` and
        // `extra_i64`.
        assert_eq!(rows[4].get_int(1).unwrap(), 3);
        assert_eq!(rows[4].get_long(9).unwrap(), 42);
        assert_eq!(rows[5].get_int(1).unwrap(), 4);
        assert_eq!(rows[5].get_long(9).unwrap(), 42);
    }

    /// T18.2b: round-trip every new lifecycle event kind through the
    /// Parquet writer + reader. Asserts that the right `extra_*`
    /// slot carries the value the per-kind `push_*` helper supplied.
    #[test]
    fn lifecycle_event_kinds_round_trip_through_parquet() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lifecycle.compact.parquet");

        let mut buf = CompactBuffers::new();
        buf.push_phase(100, "operate").unwrap();
        buf.push_connected(200, Some("alice"), 7.5, "single")
            .unwrap();
        buf.push_eot_sent(300, 42).unwrap();
        buf.push_eot_received(400, "alice", 42).unwrap();
        buf.push_eot_timeout(500, 5000, r#"["bob"]"#).unwrap();
        buf.push_resource(600, 12.5, 256.0).unwrap();
        buf.push_clock_sync(700, "alice", -1_234, 0.5).unwrap();

        write_compact_parquet(
            &path,
            &buf,
            &sample_meta(),
            &CompactWriterOptions::default(),
        )
        .unwrap();

        let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
        let rows: Vec<_> = reader
            .get_row_iter(None)
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 7);

        // phase -- extra_utf8 = "operate"
        let phase = &rows[0];
        assert_eq!(phase.get_int(1).unwrap(), 5);
        assert_eq!(phase.get_string(10).unwrap(), "operate");

        // connected -- peer_idx populated, extra_f32=7.5,
        // extra_utf8="single"
        let connected = &rows[1];
        assert_eq!(connected.get_int(1).unwrap(), 6);
        assert_eq!(connected.get_int(4).unwrap(), 0); // peer_idx = alice
        assert!((connected.get_float(7).unwrap() - 7.5).abs() < 1e-6);
        assert_eq!(connected.get_string(10).unwrap(), "single");

        // eot_sent -- extra_i64 = 42
        let eot_sent = &rows[2];
        assert_eq!(eot_sent.get_int(1).unwrap(), 7);
        assert_eq!(eot_sent.get_long(9).unwrap(), 42);

        // eot_received -- peer_idx populated, extra_i64 = 42
        let eot_recv = &rows[3];
        assert_eq!(eot_recv.get_int(1).unwrap(), 8);
        assert_eq!(eot_recv.get_int(4).unwrap(), 0);
        assert_eq!(eot_recv.get_long(9).unwrap(), 42);

        // eot_timeout -- extra_i64 = wait_ms, extra_utf8 = JSON
        let eot_to = &rows[4];
        assert_eq!(eot_to.get_int(1).unwrap(), 9);
        assert_eq!(eot_to.get_long(9).unwrap(), 5000);
        assert_eq!(eot_to.get_string(10).unwrap(), r#"["bob"]"#);

        // resource -- extra_f32 = cpu_percent, extra_f32_b = mem_mb
        let resource = &rows[5];
        assert_eq!(resource.get_int(1).unwrap(), 10);
        assert!((resource.get_float(7).unwrap() - 12.5).abs() < 1e-6);
        assert!((resource.get_float(8).unwrap() - 256.0).abs() < 1e-6);

        // clock_sync -- peer_idx populated, extra_i64=offset_ns,
        // extra_f32=rtt_ms
        let cs = &rows[6];
        assert_eq!(cs.get_int(1).unwrap(), 11);
        assert_eq!(cs.get_int(4).unwrap(), 0);
        assert_eq!(cs.get_long(9).unwrap(), -1_234);
        assert!((cs.get_float(7).unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn kv_metadata_contains_dictionaries_and_spawn_identifiers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.compact.parquet");
        let buf = populated_buffers();
        write_compact_parquet(
            &path,
            &buf,
            &sample_meta(),
            &CompactWriterOptions::default(),
        )
        .unwrap();

        let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
        let kv = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .expect("kv metadata must be present");
        let lookup: std::collections::HashMap<&str, &str> = kv
            .iter()
            .filter_map(|x| x.value.as_deref().map(|v| (x.key.as_str(), v)))
            .collect();

        assert_eq!(lookup.get("schema_version"), Some(&"1"));
        assert_eq!(lookup.get("variant"), Some(&"test-variant"));
        assert_eq!(lookup.get("runner"), Some(&"alice"));
        assert_eq!(lookup.get("run"), Some(&"run01"));
        assert_eq!(lookup.get("threading_mode"), Some(&"single"));
        assert_eq!(lookup.get("recv_buffer_kb"), Some(&"4096"));

        // Dictionaries should round-trip as JSON.
        let paths_json = lookup.get("paths").expect("paths key");
        let paths: Vec<String> = serde_json::from_str(paths_json).unwrap();
        assert_eq!(paths, vec!["/bench/0".to_string(), "/bench/1".to_string()]);
        let peers_json = lookup.get("peers").expect("peers key");
        let peers: Vec<String> = serde_json::from_str(peers_json).unwrap();
        assert_eq!(peers, vec!["bob".to_string()]);
    }

    #[test]
    fn compact_parquet_path_uses_canonical_name() {
        let p = compact_parquet_path(Path::new("/tmp/logs"), "vbench", "alice", "run01");
        assert!(p
            .to_string_lossy()
            .ends_with("vbench-alice-run01.compact.parquet"));
    }
}
