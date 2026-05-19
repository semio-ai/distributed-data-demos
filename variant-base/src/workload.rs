//! Workload profile definitions (E19 / T19.2).
//!
//! A workload profile is the per-tick generator that produces the
//! `WriteOp` values the driver will publish. Each profile owns its own
//! state and may consume random bits; the driver does not introspect
//! that state.
//!
//! ## Profiles
//!
//! - **`scalar-flood`** -- the historical default. Emits
//!   `values_per_tick` WriteOps per tick, each carrying one 8-byte
//!   `f64` scalar. `leaf_count = 1, shape = Scalar`.
//! - **`max-throughput`** -- identical generator to `scalar-flood`; the
//!   driver removes the inter-tick sleep when this profile is selected.
//!   `leaf_count = 1, shape = Scalar`.
//! - **`block-flood`** (E19) -- emits `values_per_tick / blob_size`
//!   WriteOps per tick, each carrying a `blob_size`-element block of
//!   `f64` scalars (`blob_size * 8` bytes). `leaf_count = blob_size,
//!   shape = Array`. Validation: `values_per_tick % blob_size == 0`.
//! - **`mixed-types`** (E19) -- emits a heterogeneous mix of scalar /
//!   array / nested-struct WriteOps per tick, summing to exactly
//!   `values_per_tick` total leaves. The allocation algorithm is
//!   documented on [`MixedTypes::generate`].
//!
//! ## `WriteOp`, `leaf_count`, `shape`
//!
//! Each `WriteOp` now carries `leaf_count: u32` (number of scalar
//! leaves packed into `payload`) and `shape: WriteShape`
//! (`{ Scalar, Array, Struct }`). Both fields are recorded on every
//! `write` JSONL event and compact-Parquet row so the analysis tool
//! can report per-shape throughput and correlate receives to inherit
//! the metadata. See `metak-shared/api-contracts/jsonl-log-schema.md`
//! and `metak-shared/api-contracts/compact-log-schema.md` (E19
//! additions).
//!
//! ## Wire opacity
//!
//! The `Variant` trait still receives opaque `&[u8]` payloads; the
//! shape metadata never travels on the wire. The receiver does NOT log
//! `leaf_count` / `shape`. The analyzer correlates receives with their
//! matching write event by `(writer, seq, path)` and inherits the
//! metadata from the write side.
//!
//! ## Determinism
//!
//! `MixedTypes` (and any other random-aware profile) seeds its RNG
//! from `WorkloadParams::workload_seed` when present; otherwise
//! it derives a deterministic seed from `--variant + --run` so two
//! re-runs of the same spawn produce identical workload sequences.
//! `BlockFlood` uses the same `f64` index pattern as `ScalarFlood` and
//! is therefore deterministic without an explicit RNG.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use anyhow::{bail, Result};
use rand::distributions::Distribution;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Categorical shape of a [`WriteOp`].
///
/// Matches the `shape` JSONL field and the `shape_intern` dictionary in
/// the compact-Parquet KV metadata. The string forms (`"scalar"`,
/// `"array"`, `"struct"`) are stable wire identifiers -- do not rename
/// without bumping the schema version on both contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteShape {
    /// A single scalar leaf (1 leaf). The historical `scalar-flood`
    /// shape; also used by `mixed-types` for the per-tick standalone
    /// scalar WriteOps.
    Scalar,
    /// An array of `leaf_count` scalar leaves packed into one payload.
    /// The `block-flood` shape; also used by `mixed-types` for its
    /// array WriteOps.
    Array,
    /// A nested struct / dict carrying `leaf_count` total leaves across
    /// its subtree. Used by `mixed-types` only.
    Struct,
}

impl WriteShape {
    /// The canonical lowercase string identifier emitted to JSONL
    /// `shape` and indexed into the compact `shape_intern` dictionary.
    pub fn as_str(self) -> &'static str {
        match self {
            WriteShape::Scalar => "scalar",
            WriteShape::Array => "array",
            WriteShape::Struct => "struct",
        }
    }

    /// Index into the canonical `shape_intern` dictionary
    /// `["scalar", "array", "struct"]` (see
    /// `metak-shared/api-contracts/compact-log-schema.md` E19 additions).
    pub fn as_u8(self) -> u8 {
        match self {
            WriteShape::Scalar => 0,
            WriteShape::Array => 1,
            WriteShape::Struct => 2,
        }
    }
}

/// The canonical `shape_intern` dictionary stored in the compact-Parquet
/// KV metadata. Indexed by [`WriteShape::as_u8`]. Pinned so the
/// analyzer's defaults match the writer's emission.
pub const SHAPE_INTERN: [&str; 3] = ["scalar", "array", "struct"];

/// A single write operation produced by a workload profile.
///
/// `path` and `payload` are the wire-facing fields. `leaf_count` and
/// `shape` are metadata: they tag the WriteOp at log-time so analysis
/// can compute per-shape throughput, leaves-per-second, and
/// leaves-lost-rate. They do NOT travel on the wire and are NOT
/// observable from the receive side.
#[derive(Debug, Clone)]
pub struct WriteOp {
    /// Key path (e.g. `/bench/0`).
    pub path: String,
    /// Serialized payload bytes. Opaque to the transport.
    pub payload: Vec<u8>,
    /// Number of scalar leaves packed in `payload`. Scalar = 1; array =
    /// element count; struct = total leaves in the tree. Used by the
    /// analyzer to compute leaves-per-sec.
    pub leaf_count: u32,
    /// How the leaves are packed into the payload. See [`WriteShape`].
    pub shape: WriteShape,
}

impl WriteOp {
    /// Convenience constructor for a single-scalar WriteOp.
    /// Equivalent to a `ScalarFlood`-shaped op.
    pub fn scalar(path: String, payload: Vec<u8>) -> Self {
        Self {
            path,
            payload,
            leaf_count: 1,
            shape: WriteShape::Scalar,
        }
    }
}

/// Trait for workload profiles that generate write operations each tick.
pub trait Workload {
    /// Generate write operations for one tick.
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp>;
}

/// Parameters supplied by the driver to [`create_workload`] when
/// constructing a workload profile.
///
/// All `block_size` / `mixed_*` fields are optional at the variant-base
/// crate level: the CLI plumbing that exposes them lands in T19.3.
/// Until T19.3 wires real values from the CLI, the driver passes a
/// [`WorkloadParams::for_scalar_flood`] struct (all None) which makes
/// `scalar-flood` / `max-throughput` continue to work unchanged and
/// makes `block-flood` / `mixed-types` return a descriptive Err.
///
/// `variant` and `run` are the spawn identifiers; `MixedTypes` derives a
/// deterministic fallback seed from them when `workload_seed` is None,
/// per the E19 locked spec.
#[derive(Debug, Clone, Default)]
pub struct WorkloadParams {
    /// Variant name (e.g. `dummy-1000x100hz`). Used as part of the
    /// `mixed-types` fallback seed when `workload_seed` is None.
    pub variant: String,
    /// Run identifier (e.g. `run01`). Used as part of the
    /// `mixed-types` fallback seed when `workload_seed` is None.
    pub run: String,
    /// `--blob-size` for `block-flood`. Required when the workload name
    /// is `block-flood`; ignored otherwise.
    pub blob_size: Option<u32>,
    /// `--mixed-scalars-min`. Required for `mixed-types`.
    pub mixed_scalars_min: Option<u32>,
    /// `--mixed-scalars-max`. Required for `mixed-types`.
    pub mixed_scalars_max: Option<u32>,
    /// `--mixed-arrays-min`. Required for `mixed-types`.
    pub mixed_arrays_min: Option<u32>,
    /// `--mixed-arrays-max`. Required for `mixed-types`.
    pub mixed_arrays_max: Option<u32>,
    /// `--mixed-dict-split-max`. Required for `mixed-types`. Must be
    /// `>= 2` per the locked spec; that check lives in T19.3 (the
    /// driver-side validation), not in the workload constructor.
    pub mixed_dict_split_max: Option<u32>,
    /// `--workload-seed`. Optional; when None, `mixed-types` derives a
    /// deterministic seed from `variant + run`.
    pub workload_seed: Option<u64>,
}

impl WorkloadParams {
    /// Build a params struct suitable for `scalar-flood` /
    /// `max-throughput` only. All E19 fields are `None`.
    pub fn for_scalar_flood(variant: &str, run: &str) -> Self {
        Self {
            variant: variant.to_string(),
            run: run.to_string(),
            ..Self::default()
        }
    }
}

/// Scalar-flood workload: generates `values_per_tick` writes to paths
/// `/bench/0`, `/bench/1`, ... with 8-byte f64 payloads.
///
/// `leaf_count = 1, shape = Scalar` on every WriteOp.
pub struct ScalarFlood {
    tick_counter: u64,
}

impl ScalarFlood {
    pub fn new() -> Self {
        Self { tick_counter: 0 }
    }
}

impl Default for ScalarFlood {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for ScalarFlood {
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp> {
        self.tick_counter += 1;
        (0..values_per_tick)
            .map(|i| {
                let value: f64 =
                    (self.tick_counter * u64::from(values_per_tick) + u64::from(i)) as f64;
                WriteOp {
                    path: format!("/bench/{}", i),
                    payload: value.to_le_bytes().to_vec(),
                    leaf_count: 1,
                    shape: WriteShape::Scalar,
                }
            })
            .collect()
    }
}

/// Block-flood workload (E19 / T19.2): emits `values_per_tick /
/// blob_size` WriteOps per tick, each carrying a `blob_size`-element
/// block of `f64` scalars packed into one opaque payload.
///
/// - `leaf_count = blob_size` on every WriteOp.
/// - `shape = Array` on every WriteOp.
/// - `payload.len() == blob_size * 8`.
///
/// Validation: `values_per_tick % blob_size == 0`. The driver enforces
/// this at startup (T19.3); [`BlockFlood::generate`] returns an empty
/// Vec on a divisibility mismatch as a defensive safeguard, but the
/// driver SHOULD have rejected the spawn before reaching this point.
#[derive(Debug)]
pub struct BlockFlood {
    blob_size: u32,
    tick_counter: u64,
}

impl BlockFlood {
    /// Construct a new block-flood workload with a configured
    /// `blob_size`.
    pub fn new(blob_size: u32) -> Result<Self> {
        if blob_size == 0 {
            bail!("block-flood blob_size must be > 0");
        }
        Ok(Self {
            blob_size,
            tick_counter: 0,
        })
    }
}

impl Workload for BlockFlood {
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp> {
        // Defensive: the driver validates this at startup (T19.3), but
        // an unvalidated caller would otherwise produce a tick whose
        // leaf count silently disagrees with `values_per_tick`. Return
        // an empty Vec on mismatch -- the operate loop treats this as
        // "nothing to publish this tick" which is the least-surprising
        // failure mode in absence of explicit validation.
        if self.blob_size == 0 || !values_per_tick.is_multiple_of(self.blob_size) {
            return Vec::new();
        }
        self.tick_counter += 1;
        let writes_per_tick = values_per_tick / self.blob_size;
        (0..writes_per_tick)
            .map(|w| {
                // Fill the block with deterministic f64s so the
                // analyzer cannot accidentally rely on payload content
                // (the analysis pipeline never inspects bytes; this
                // pattern is purely for reproducibility).
                let mut payload = Vec::with_capacity(self.blob_size as usize * 8);
                for i in 0..self.blob_size {
                    let v: f64 = (self.tick_counter * u64::from(values_per_tick)
                        + u64::from(w) * u64::from(self.blob_size)
                        + u64::from(i)) as f64;
                    payload.extend_from_slice(&v.to_le_bytes());
                }
                WriteOp {
                    path: format!("/bench/block/{w}"),
                    payload,
                    leaf_count: self.blob_size,
                    shape: WriteShape::Array,
                }
            })
            .collect()
    }
}

/// Mixed-types workload (E19 / T19.2): emits a heterogeneous mix of
/// scalar / array / nested-struct WriteOps per tick.
///
/// ## Allocation algorithm (locked spec)
///
/// Per the E19 locked spec (see `metak-orchestrator/EPICS.md` E19),
/// each tick the generator:
///
/// 1. Draws `nS = rand(mixed_scalars_min, mixed_scalars_max)`.
///    Emits `nS` scalar WriteOps (`leaf_count = 1`).
/// 2. Computes `remaining = vpt - nS`. Draws
///    `nA = rand(mixed_arrays_min, min(mixed_arrays_max, remaining / 2))`
///    and distributes the `nA` leaves across
///    `rand(1, mixed_arrays_max)` array WriteOps using a uniform random
///    partition. Each array WriteOp has `shape = Array`.
/// 3. Computes `remaining = vpt - nS - nA`. Allocates the remainder
///    recursively as a nested-dict tree of struct WriteOps, splitting
///    `rand(1, mixed_dict_split_max)` ways at each level. Termination:
///    depth bound `log_2(vpt) + 4`; if reached, force a flat dict at
///    that level (one struct WriteOp with all remaining leaves).
///
/// ## Determinism
///
/// The RNG is `rand::rngs::StdRng` seeded from
/// `WorkloadParams::workload_seed` when present; otherwise a
/// deterministic seed is derived from `variant + run` so re-runs with
/// identical config produce identical workload sequences.
///
/// ## Latency invariant
///
/// `MixedTypes::generate(N)` always returns WriteOps whose `leaf_count`
/// values sum to exactly `N` (or 0 when the params do not allow a
/// non-empty mix). This is the analyzer-visible contract: leaves
/// reported by the variant equals leaves intended by the operator.
pub struct MixedTypes {
    scalars_min: u32,
    scalars_max: u32,
    arrays_min: u32,
    arrays_max: u32,
    dict_split_max: u32,
    rng: StdRng,
    tick_counter: u64,
}

impl MixedTypes {
    /// Construct a new mixed-types workload.
    ///
    /// Returns an Err if `scalars_min > scalars_max`, `arrays_min >
    /// arrays_max`, or `dict_split_max < 2`. These mirror the
    /// driver-side validation in T19.3; constructing without the
    /// driver's pre-check (e.g. in unit tests) is permitted but still
    /// rejects malformed inputs.
    pub fn new(
        scalars_min: u32,
        scalars_max: u32,
        arrays_min: u32,
        arrays_max: u32,
        dict_split_max: u32,
        seed: u64,
    ) -> Result<Self> {
        if scalars_min > scalars_max {
            bail!("mixed-types: scalars_min ({scalars_min}) > scalars_max ({scalars_max})");
        }
        if arrays_min > arrays_max {
            bail!("mixed-types: arrays_min ({arrays_min}) > arrays_max ({arrays_max})");
        }
        if dict_split_max < 2 {
            bail!("mixed-types: dict_split_max must be >= 2 (got {dict_split_max})");
        }
        Ok(Self {
            scalars_min,
            scalars_max,
            arrays_min,
            arrays_max,
            dict_split_max,
            rng: StdRng::seed_from_u64(seed),
            tick_counter: 0,
        })
    }

    /// Derive a deterministic 64-bit seed from `(variant, run)` for
    /// the `--workload-seed`-omitted fallback. Uses `DefaultHasher` --
    /// any stable hasher is fine; reproducibility across runs of the
    /// same Rust toolchain is what we need, not cryptographic strength.
    pub fn derive_seed_from_spawn(variant: &str, run: &str) -> u64 {
        let mut h = DefaultHasher::new();
        // Prefix-and-separator so `("ab", "cd")` and `("a", "bcd")`
        // hash differently. This is paranoid but cheap.
        "variant".hash(&mut h);
        variant.hash(&mut h);
        "run".hash(&mut h);
        run.hash(&mut h);
        h.finish()
    }

    /// Maximum recursion depth in the nested-dict allocation. Bounded
    /// at `log_2(vpt) + 4` (rounded up); if reached, the remaining
    /// leaves are flattened into one struct WriteOp at that level.
    fn max_depth(vpt: u32) -> u32 {
        // ceil(log2(vpt)) + 4; floor at 4 to guarantee at least four
        // levels of branching even for very small vpt.
        let log = if vpt <= 1 {
            0
        } else {
            32 - (vpt - 1).leading_zeros()
        };
        log + 4
    }
}

impl Workload for MixedTypes {
    fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp> {
        if values_per_tick == 0 {
            return Vec::new();
        }
        self.tick_counter += 1;
        let vpt = values_per_tick;
        let mut ops: Vec<WriteOp> = Vec::new();

        // ---- Step 1: scalars ----
        //
        // Cap scalars_max at vpt so we never draw more scalars than
        // total leaves. This protects the recursion bookkeeping from
        // negative `remaining` values when an operator over-configures
        // scalars_max.
        let scalars_max = self.scalars_max.min(vpt);
        let scalars_min = self.scalars_min.min(scalars_max);
        let n_scalars: u32 = if scalars_min == scalars_max {
            scalars_min
        } else {
            self.rng.gen_range(scalars_min..=scalars_max)
        };
        for i in 0..n_scalars {
            // Same f64 encoding as ScalarFlood so debugging is uniform.
            let value: f64 = (self.tick_counter * u64::from(vpt) + u64::from(i)) as f64;
            ops.push(WriteOp {
                path: format!("/bench/mixed/scalar/{i}"),
                payload: value.to_le_bytes().to_vec(),
                leaf_count: 1,
                shape: WriteShape::Scalar,
            });
        }

        // ---- Step 2: arrays ----
        let after_scalars = vpt - n_scalars;
        // The locked spec says
        //   nA = rand(mixed_arrays_min,
        //             min(mixed_arrays_max, remaining / 2)).
        // The remaining/2 cap guarantees the recursion has at least
        // half the budget left for the dict tree (so dicts aren't
        // squeezed out entirely). When remaining is 0 or 1 the array
        // step contributes 0 leaves.
        let arr_cap_from_remaining = after_scalars / 2;
        let arr_cap = self.arrays_max.min(arr_cap_from_remaining);
        let arr_floor = self.arrays_min.min(arr_cap);
        let n_array_leaves: u32 = if arr_floor >= arr_cap {
            arr_floor
        } else {
            self.rng.gen_range(arr_floor..=arr_cap)
        };
        if n_array_leaves > 0 {
            // Number of array WriteOps to spread the n_array_leaves
            // across. Bounded above by self.arrays_max and by
            // n_array_leaves itself (each array must have >= 1 leaf).
            let k_cap = self.arrays_max.max(1).min(n_array_leaves);
            let n_arrays: u32 = if k_cap <= 1 {
                1
            } else {
                self.rng.gen_range(1..=k_cap)
            };
            let parts = uniform_partition(&mut self.rng, n_array_leaves, n_arrays);
            for (i, leaves) in parts.iter().enumerate() {
                if *leaves == 0 {
                    continue;
                }
                let mut payload = Vec::with_capacity(*leaves as usize * 8);
                for j in 0..*leaves {
                    let v: f64 = (self.tick_counter * u64::from(vpt)
                        + u64::from(n_scalars)
                        + u64::from(j)) as f64;
                    payload.extend_from_slice(&v.to_le_bytes());
                }
                ops.push(WriteOp {
                    path: format!("/bench/mixed/array/{i}"),
                    payload,
                    leaf_count: *leaves,
                    shape: WriteShape::Array,
                });
            }
        }

        // ---- Step 3: dicts (recursive) ----
        //
        // Whatever leaves remain go into nested-struct WriteOps. Each
        // recursion level either (a) emits a single flat struct
        // WriteOp carrying the bucket's leaves, or (b) splits into
        // `rand(1, dict_split_max)` children and recurses. Termination
        // is guaranteed by the depth bound below.
        let after_arrays = after_scalars - n_array_leaves;
        if after_arrays > 0 {
            let max_depth = Self::max_depth(vpt);
            let mut path_prefix = String::from("/bench/mixed/dict");
            self.expand_dict(after_arrays, 0, max_depth, &mut path_prefix, &mut ops);
        }

        ops
    }
}

impl MixedTypes {
    /// Recursively allocate `leaves` leaves into struct WriteOps,
    /// emitting them into `ops`. The path prefix accumulates one
    /// segment per recursion depth so the analyzer's `path` field is
    /// stable across re-runs.
    fn expand_dict(
        &mut self,
        leaves: u32,
        depth: u32,
        max_depth: u32,
        path: &mut String,
        ops: &mut Vec<WriteOp>,
    ) {
        if leaves == 0 {
            return;
        }
        // Base cases:
        // - `leaves == 1`: emit one scalar WriteOp (legal under the
        //   spec; a struct of one leaf is just that leaf).
        // - `leaves <= dict_split_max` OR `depth >= max_depth`: emit
        //   one flat struct WriteOp with `leaves` leaves and stop
        //   recursing. The `<= dict_split_max` rule is the natural
        //   termination ("we can no longer split further"); the depth
        //   bound is the hard safety stop.
        if leaves == 1 {
            // Emit as a 1-leaf scalar to keep `MixedTypes` total-leaf
            // accounting honest. The op is path-tagged as a scalar so
            // the analyzer per-shape histograms don't see misleading
            // 1-leaf struct rows.
            let value: f64 = (self.tick_counter * 1_000_003 + u64::from(depth) * 17) as f64;
            ops.push(WriteOp {
                path: format!("{path}/leaf"),
                payload: value.to_le_bytes().to_vec(),
                leaf_count: 1,
                shape: WriteShape::Scalar,
            });
            return;
        }
        if leaves <= self.dict_split_max || depth >= max_depth {
            // Flatten. The payload encodes `leaves` consecutive f64s.
            let mut payload = Vec::with_capacity(leaves as usize * 8);
            for i in 0..leaves {
                let v: f64 = (self.tick_counter * 7 + u64::from(depth) * 31 + u64::from(i)) as f64;
                payload.extend_from_slice(&v.to_le_bytes());
            }
            ops.push(WriteOp {
                path: format!("{path}/flat"),
                payload,
                leaf_count: leaves,
                shape: WriteShape::Struct,
            });
            return;
        }
        // Recursive case: pick a branching factor k in
        // [1, dict_split_max], partition `leaves` into k positive
        // integers, recurse for each child. k == 1 collapses into the
        // "flatten" base case on the next level (depth + 1), so the
        // depth bound still terminates.
        let k_cap = self.dict_split_max.min(leaves);
        let k: u32 = if k_cap <= 1 {
            1
        } else {
            self.rng.gen_range(1..=k_cap)
        };
        let parts = uniform_partition(&mut self.rng, leaves, k);
        let base_len = path.len();
        for (i, child_leaves) in parts.iter().enumerate() {
            if *child_leaves == 0 {
                continue;
            }
            path.truncate(base_len);
            path.push('/');
            path.push_str(&i.to_string());
            self.expand_dict(*child_leaves, depth + 1, max_depth, path, ops);
        }
        path.truncate(base_len);
    }
}

/// Partition `total` into exactly `parts` positive integers using a
/// uniform-random stars-and-bars allocation. Each returned bucket
/// holds at least 1 leaf when `parts <= total`.
///
/// Algorithm: pick `parts - 1` distinct cut points in `[1, total)`
/// uniformly at random, then take consecutive differences. This is the
/// canonical uniform random partition and avoids the bias of
/// repeated-`rand_range` approaches.
///
/// Edge cases:
/// - `parts == 0`: returns an empty Vec.
/// - `parts == 1`: returns `vec![total]`.
/// - `parts > total`: collapses to `parts == total` (one leaf per bucket
///   with trailing zero-buckets dropped); the caller is expected to
///   pre-clamp `parts` to `total` anyway.
fn uniform_partition(rng: &mut StdRng, total: u32, parts: u32) -> Vec<u32> {
    if parts == 0 {
        return Vec::new();
    }
    if parts == 1 || total == 0 {
        return vec![total];
    }
    if parts >= total {
        // One leaf per bucket; trailing buckets get nothing.
        let mut v = vec![1u32; total as usize];
        v.extend(std::iter::repeat_n(0u32, parts as usize - total as usize));
        return v;
    }
    // Sample `parts - 1` distinct cut points in `[1, total)`. Repeats
    // are rare on realistic inputs (total >= parts + 1) but possible;
    // reject + redraw is simpler than the floyd-style sampler and the
    // partitions we care about are small (<= a few hundred parts).
    let mut cuts: Vec<u32> = Vec::with_capacity(parts as usize - 1);
    let dist = rand::distributions::Uniform::from(1..total);
    while cuts.len() < parts as usize - 1 {
        let c = dist.sample(rng);
        if !cuts.contains(&c) {
            cuts.push(c);
        }
    }
    cuts.sort_unstable();
    cuts.push(total);
    let mut last = 0u32;
    let mut out = Vec::with_capacity(parts as usize);
    for c in cuts {
        out.push(c - last);
        last = c;
    }
    out
}

/// Create a workload profile by name with default (empty) params.
///
/// Back-compat helper: equivalent to
/// `create_workload_with_params(name, &WorkloadParams::default())`.
/// Existing callers (and the T18.2-era driver) use this. Once T19.3
/// lands the driver switches to `create_workload_with_params` so it
/// can pass through `--blob-size` / `--mixed-*` / `--workload-seed`.
///
/// Returns Err for `block-flood` / `mixed-types` -- those profiles
/// require parameters that this back-compat shim does not carry.
pub fn create_workload(name: &str) -> Result<Box<dyn Workload>> {
    create_workload_with_params(name, &WorkloadParams::default())
}

/// Create a workload profile by name with caller-supplied params.
///
/// - `scalar-flood`, `max-throughput`: ignore the params, produce a
///   `ScalarFlood` generator.
/// - `block-flood`: requires `params.blob_size`; returns Err if absent
///   or `0`.
/// - `mixed-types`: requires all five `mixed_*` params; returns Err if
///   any is absent. The seed is `params.workload_seed` when present,
///   otherwise derived from `params.variant + params.run` via
///   [`MixedTypes::derive_seed_from_spawn`].
pub fn create_workload_with_params(
    name: &str,
    params: &WorkloadParams,
) -> Result<Box<dyn Workload>> {
    match name {
        "scalar-flood" | "max-throughput" => Ok(Box::new(ScalarFlood::new())),
        "block-flood" => {
            let blob = params
                .blob_size
                .ok_or_else(|| anyhow::anyhow!("block-flood requires --blob-size"))?;
            Ok(Box::new(BlockFlood::new(blob)?))
        }
        "mixed-types" => {
            let scalars_min = params
                .mixed_scalars_min
                .ok_or_else(|| anyhow::anyhow!("mixed-types requires --mixed-scalars-min"))?;
            let scalars_max = params
                .mixed_scalars_max
                .ok_or_else(|| anyhow::anyhow!("mixed-types requires --mixed-scalars-max"))?;
            let arrays_min = params
                .mixed_arrays_min
                .ok_or_else(|| anyhow::anyhow!("mixed-types requires --mixed-arrays-min"))?;
            let arrays_max = params
                .mixed_arrays_max
                .ok_or_else(|| anyhow::anyhow!("mixed-types requires --mixed-arrays-max"))?;
            let dict_split_max = params
                .mixed_dict_split_max
                .ok_or_else(|| anyhow::anyhow!("mixed-types requires --mixed-dict-split-max"))?;
            let seed = params.workload_seed.unwrap_or_else(|| {
                MixedTypes::derive_seed_from_spawn(&params.variant, &params.run)
            });
            Ok(Box::new(MixedTypes::new(
                scalars_min,
                scalars_max,
                arrays_min,
                arrays_max,
                dict_split_max,
                seed,
            )?))
        }
        _ => bail!("unknown workload profile: {}", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- ScalarFlood (existing behaviour, unchanged) -----

    #[test]
    fn test_scalar_flood_count() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(5);
        assert_eq!(ops.len(), 5);
    }

    #[test]
    fn test_scalar_flood_paths() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(3);
        assert_eq!(ops[0].path, "/bench/0");
        assert_eq!(ops[1].path, "/bench/1");
        assert_eq!(ops[2].path, "/bench/2");
    }

    #[test]
    fn test_scalar_flood_payload_size() {
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(1);
        assert_eq!(ops[0].payload.len(), 8, "payload should be 8 bytes (f64)");
    }

    #[test]
    fn test_scalar_flood_emits_scalar_shape() {
        // E19 invariant: scalar-flood produces leaf_count=1,
        // shape=Scalar on every WriteOp. Analysis backfill of legacy
        // logs depends on this default.
        let mut wl = ScalarFlood::new();
        let ops = wl.generate(3);
        for op in &ops {
            assert_eq!(op.leaf_count, 1);
            assert_eq!(op.shape, WriteShape::Scalar);
        }
    }

    #[test]
    fn test_create_workload_valid() {
        let wl = create_workload("scalar-flood");
        assert!(wl.is_ok());
    }

    #[test]
    fn test_create_workload_unknown() {
        let wl = create_workload("nonexistent");
        assert!(wl.is_err());
    }

    // ----- BlockFlood (E19 / T19.2) -----

    #[test]
    fn test_block_flood_generates_correct_op_count_and_metadata() {
        // blob_size=100, vpt=1000 -> 10 WriteOps, each leaf_count=100,
        // shape=Array, payload.len()=800.
        let mut wl = BlockFlood::new(100).unwrap();
        let ops = wl.generate(1000);
        assert_eq!(ops.len(), 10, "vpt/blob_size = 1000/100 = 10 ops");
        for op in &ops {
            assert_eq!(op.leaf_count, 100);
            assert_eq!(op.shape, WriteShape::Array);
            assert_eq!(
                op.payload.len(),
                800,
                "100 f64 leaves * 8 bytes each = 800 bytes"
            );
        }
    }

    #[test]
    fn test_block_flood_total_leaves_equal_vpt() {
        // E19 vpt invariant: across all profiles, sum(leaf_count) ==
        // vpt. Block-flood is the easy case (uniform leaf_count =
        // blob_size).
        let mut wl = BlockFlood::new(50).unwrap();
        let ops = wl.generate(500);
        let total: u32 = ops.iter().map(|o| o.leaf_count).sum();
        assert_eq!(total, 500);
    }

    #[test]
    fn test_block_flood_paths_are_stable() {
        // Path scheme: /bench/block/{op_index}. Two consecutive ticks
        // should hit the same paths so the receiver can de-duplicate
        // by path the way it does for scalar-flood.
        let mut wl = BlockFlood::new(10).unwrap();
        let ops1 = wl.generate(100);
        let ops2 = wl.generate(100);
        let paths1: Vec<&str> = ops1.iter().map(|o| o.path.as_str()).collect();
        let paths2: Vec<&str> = ops2.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths1, paths2);
    }

    #[test]
    fn test_block_flood_indivisible_vpt_emits_no_ops() {
        // The driver validates vpt % blob_size == 0 at startup
        // (T19.3). If a caller bypasses that, BlockFlood::generate
        // returns an empty Vec rather than emitting WriteOps with
        // off-by-one leaf counts.
        let mut wl = BlockFlood::new(300).unwrap();
        let ops = wl.generate(1000); // 1000 % 300 != 0
        assert!(ops.is_empty());
    }

    #[test]
    fn test_block_flood_rejects_zero_blob_size() {
        let err = BlockFlood::new(0).unwrap_err();
        assert!(err.to_string().contains("blob_size"));
    }

    #[test]
    fn test_create_workload_with_params_block_flood() {
        let params = WorkloadParams {
            blob_size: Some(100),
            ..WorkloadParams::default()
        };
        let mut wl = create_workload_with_params("block-flood", &params).unwrap();
        let ops = wl.generate(1000);
        assert_eq!(ops.len(), 10);
        for op in &ops {
            assert_eq!(op.shape, WriteShape::Array);
            assert_eq!(op.leaf_count, 100);
        }
    }

    #[test]
    fn test_create_workload_with_params_block_flood_missing_blob_size() {
        let params = WorkloadParams::default();
        let err = match create_workload_with_params("block-flood", &params) {
            Err(e) => e,
            Ok(_) => panic!("expected Err for block-flood without --blob-size"),
        };
        assert!(err.to_string().contains("blob-size"));
    }

    // ----- MixedTypes (E19 / T19.2) -----

    fn mixed_params() -> WorkloadParams {
        WorkloadParams {
            variant: "test".to_string(),
            run: "r1".to_string(),
            mixed_scalars_min: Some(10),
            mixed_scalars_max: Some(50),
            mixed_arrays_min: Some(0),
            mixed_arrays_max: Some(100),
            mixed_dict_split_max: Some(4),
            workload_seed: Some(42),
            ..Default::default()
        }
    }

    #[test]
    fn test_mixed_types_total_leaves_equal_vpt() {
        let mut wl = create_workload_with_params("mixed-types", &mixed_params()).unwrap();
        let ops = wl.generate(1000);
        let total: u32 = ops.iter().map(|o| o.leaf_count).sum();
        assert_eq!(total, 1000, "sum(leaf_count) must equal vpt");
    }

    #[test]
    fn test_mixed_types_total_leaves_for_various_n() {
        // The E19 task locks this: for N in {1, 10, 100, 1000} the
        // total must always equal N.
        let p = WorkloadParams {
            variant: "v".to_string(),
            run: "r".to_string(),
            mixed_scalars_min: Some(0),
            mixed_scalars_max: Some(10),
            mixed_arrays_min: Some(0),
            mixed_arrays_max: Some(10),
            mixed_dict_split_max: Some(3),
            workload_seed: Some(0xDEAD_BEEF),
            ..Default::default()
        };
        for &n in &[1u32, 10, 100, 1000] {
            let mut wl = create_workload_with_params("mixed-types", &p).unwrap();
            let ops = wl.generate(n);
            let total: u32 = ops.iter().map(|o| o.leaf_count).sum();
            assert_eq!(total, n, "vpt={n}: sum(leaf_count)={total} != {n}");
        }
    }

    #[test]
    fn test_mixed_types_shape_distribution_is_heterogeneous() {
        // With non-zero arrays_max and dict_split_max, mixed-types
        // should produce at least one WriteOp of each non-Scalar
        // shape over a few ticks. We don't assert exact counts (rng-
        // driven) -- just that the generator can produce non-Scalar
        // ops.
        let mut wl = create_workload_with_params("mixed-types", &mixed_params()).unwrap();
        let mut seen_scalar = false;
        let mut seen_array = false;
        let mut seen_struct = false;
        for _ in 0..10 {
            for op in wl.generate(1000) {
                match op.shape {
                    WriteShape::Scalar => seen_scalar = true,
                    WriteShape::Array => seen_array = true,
                    WriteShape::Struct => seen_struct = true,
                }
            }
            if seen_scalar && seen_array && seen_struct {
                return;
            }
        }
        panic!(
            "after 10 ticks expected all three shapes, got scalar={seen_scalar} \
             array={seen_array} struct={seen_struct}"
        );
    }

    #[test]
    fn test_mixed_types_determinism_same_seed() {
        // Identical seeds (and identical (variant, run, params)) must
        // produce identical WriteOp sequences across two
        // independently-constructed generators.
        let p = mixed_params();
        let mut a = create_workload_with_params("mixed-types", &p).unwrap();
        let mut b = create_workload_with_params("mixed-types", &p).unwrap();
        for _ in 0..3 {
            let ops_a = a.generate(500);
            let ops_b = b.generate(500);
            assert_eq!(ops_a.len(), ops_b.len(), "op count must match");
            for (oa, ob) in ops_a.iter().zip(ops_b.iter()) {
                assert_eq!(oa.path, ob.path);
                assert_eq!(oa.payload, ob.payload);
                assert_eq!(oa.leaf_count, ob.leaf_count);
                assert_eq!(oa.shape, ob.shape);
            }
        }
    }

    #[test]
    fn test_mixed_types_seed_fallback_from_spawn_is_deterministic() {
        // When --workload-seed is omitted, the seed is derived from
        // (variant, run). Two generators constructed with the same
        // (variant, run) must produce identical sequences; changing
        // either field must produce a different sequence.
        let mut p1 = mixed_params();
        p1.workload_seed = None;
        p1.variant = "alpha".to_string();
        p1.run = "r1".to_string();
        let p2 = p1.clone();
        let mut p3 = p1.clone();
        p3.variant = "beta".to_string();

        let mut a = create_workload_with_params("mixed-types", &p1).unwrap();
        let mut b = create_workload_with_params("mixed-types", &p2).unwrap();
        let mut c = create_workload_with_params("mixed-types", &p3).unwrap();
        let ops_a = a.generate(200);
        let ops_b = b.generate(200);
        let ops_c = c.generate(200);

        // p1 == p2 -> identical output.
        let collect = |ops: &[WriteOp]| -> Vec<(String, Vec<u8>)> {
            ops.iter()
                .map(|o| (o.path.clone(), o.payload.clone()))
                .collect()
        };
        assert_eq!(collect(&ops_a), collect(&ops_b));
        // p3 differs in `variant` -> different sequence (very high
        // probability under DefaultHasher; deterministic per-toolchain).
        assert_ne!(collect(&ops_a), collect(&ops_c));
    }

    #[test]
    fn test_mixed_types_termination_under_pathological_branching() {
        // dict_split_max = 2 with vpt = 1024 forces the recursion to
        // bottom out under the depth bound rather than the
        // <= dict_split_max base case (since 1024 / 2 chains 10 deep).
        // The depth bound (log2(1024) + 4 = 14) terminates the
        // recursion well before any infinite expansion risk.
        let p = WorkloadParams {
            variant: "v".to_string(),
            run: "r".to_string(),
            mixed_scalars_min: Some(0),
            mixed_scalars_max: Some(0),
            mixed_arrays_min: Some(0),
            mixed_arrays_max: Some(0),
            mixed_dict_split_max: Some(2),
            workload_seed: Some(7),
            ..Default::default()
        };
        let mut wl = create_workload_with_params("mixed-types", &p).unwrap();
        let ops = wl.generate(1024);
        let total: u32 = ops.iter().map(|o| o.leaf_count).sum();
        assert_eq!(total, 1024);
    }

    #[test]
    fn test_mixed_types_max_depth_formula() {
        assert_eq!(MixedTypes::max_depth(1), 4);
        assert_eq!(MixedTypes::max_depth(2), 5);
        // log2(1000) = ~9.97; ceil = 10; max_depth = 14.
        assert_eq!(MixedTypes::max_depth(1000), 14);
        // 1024 -> log2 ceil = 10 -> 14.
        assert_eq!(MixedTypes::max_depth(1024), 14);
        // 1025 -> log2 ceil = 11 -> 15.
        assert_eq!(MixedTypes::max_depth(1025), 15);
    }

    fn expect_err<T>(r: Result<T>) -> anyhow::Error {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn test_mixed_types_missing_params_errors() {
        let mut p = WorkloadParams::default();
        // No mixed-* fields set.
        let err = expect_err(create_workload_with_params("mixed-types", &p));
        assert!(err.to_string().contains("mixed-scalars-min"));
        // Set one, leave the rest unset.
        p.mixed_scalars_min = Some(0);
        let err = expect_err(create_workload_with_params("mixed-types", &p));
        assert!(err.to_string().contains("mixed-scalars-max"));
    }

    #[test]
    fn test_mixed_types_rejects_dict_split_max_lt_2() {
        let p = WorkloadParams {
            mixed_scalars_min: Some(0),
            mixed_scalars_max: Some(0),
            mixed_arrays_min: Some(0),
            mixed_arrays_max: Some(0),
            mixed_dict_split_max: Some(1),
            workload_seed: Some(0),
            ..Default::default()
        };
        let err = expect_err(create_workload_with_params("mixed-types", &p));
        assert!(err.to_string().contains("dict_split_max"));
    }

    // ----- WriteShape contract -----

    #[test]
    fn test_write_shape_strings_are_canonical() {
        // The strings are wire identifiers; the analyzer indexes them
        // into the shape_intern dictionary.
        assert_eq!(WriteShape::Scalar.as_str(), "scalar");
        assert_eq!(WriteShape::Array.as_str(), "array");
        assert_eq!(WriteShape::Struct.as_str(), "struct");
    }

    #[test]
    fn test_write_shape_indices_match_intern_table() {
        for (i, &name) in SHAPE_INTERN.iter().enumerate() {
            let shape = match i {
                0 => WriteShape::Scalar,
                1 => WriteShape::Array,
                2 => WriteShape::Struct,
                _ => panic!("SHAPE_INTERN should only have 3 entries"),
            };
            assert_eq!(shape.as_u8() as usize, i);
            assert_eq!(shape.as_str(), name);
        }
    }

    // ----- uniform_partition primitive -----

    #[test]
    fn test_uniform_partition_sum_equals_total() {
        let mut rng = StdRng::seed_from_u64(1);
        for &(total, parts) in &[(10u32, 1u32), (100, 5), (1000, 17), (50, 50)] {
            let p = uniform_partition(&mut rng, total, parts);
            let sum: u32 = p.iter().sum();
            assert_eq!(sum, total, "total={total} parts={parts}");
            assert_eq!(p.len(), parts as usize);
        }
    }

    #[test]
    fn test_uniform_partition_minimum_one_per_bucket_when_room() {
        let mut rng = StdRng::seed_from_u64(2);
        // total >= parts -> every bucket should be >= 1
        let p = uniform_partition(&mut rng, 100, 10);
        for v in &p {
            assert!(*v >= 1);
        }
    }

    #[test]
    fn test_uniform_partition_one_bucket() {
        let mut rng = StdRng::seed_from_u64(3);
        assert_eq!(uniform_partition(&mut rng, 42, 1), vec![42]);
    }
}
