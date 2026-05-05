# Task Board

## Current Sprint — E12: End-of-Test Handshake

See `EPICS.md` E12 and `metak-shared/api-contracts/eot-protocol.md` for
the driving design. Critical path: T12.1 (foundation) gates everything.
T12.2-T12.6 run in parallel after T12.1 lands. T12.7 closes the loop
once T12.2-T12.6 are in.

### T12.1: variant-base — EOT trait methods, driver phase, log events, CLI flag

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: contract review by user (landed; see EPICS.md E12).

Foundational task. Adds the EOT phase to the protocol driver, the two
new trait methods with no-op defaults, the three new JSONL events, the
new `phase=eot` value, and the `--eot-timeout-secs` CLI flag. After
T12.1 lands, every existing variant compiles and runs unchanged
(falling through to no-op default impls that produce an `eot_timeout`
diagnostic event but otherwise complete the spawn).

#### Scope

1. **Trait** (`variant-base/src/variant.rs` or wherever the trait
   lives):
   - Add `signal_end_of_test(&mut self) -> anyhow::Result<u64>` with
     default impl `Ok(0)`. Returns the `eot_id`.
   - Add `poll_peer_eots(&mut self) -> anyhow::Result<Vec<PeerEot>>`
     with default impl `Ok(Vec::new())`.
   - Add the `PeerEot { writer: String, eot_id: u64 }` struct.
   - Module / re-export plumbing so variants can `use variant_base::PeerEot`.

2. **Protocol driver** (`variant-base/src/protocol.rs` or equivalent):
   - Insert the EOT phase between operate and silent. Pseudocode is
     in `eot-protocol.md` "Driver pseudocode in the EOT phase".
   - Phase logging: emit `phase` event with `phase: "eot"` at the
     start of the EOT phase (existing pattern, just a new value).
   - Compute `eot_timeout` from CLI: default = `max(operate_secs, 5)`
     if `--eot-timeout-secs` is unset.
   - Log `eot_sent` once with the returned `eot_id`. Log
     `eot_received` for each NEW (writer, eot_id) the variant returns;
     dedup-by-writer is the driver's job. Log `eot_timeout` only if
     the wait expired with peers still missing.
   - Sleep ~10 ms between polls when no new EOT was returned (avoid a
     busy spin).
   - Self-loopback edge case (single runner, no peers): `expected`
     is empty, the wait loop terminates immediately, no `eot_timeout`
     is logged. Test this against `variant-dummy`.

3. **Logger** (`variant-base/src/logger.rs` or equivalent):
   - Add three convenience methods (or extend existing structured-log
     helper) for `eot_sent`, `eot_received`, `eot_timeout` per the
     schema in `metak-shared/api-contracts/jsonl-log-schema.md`.

4. **CLI** (`variant-base/src/cli.rs` or `clap` derive struct):
   - Add `--eot-timeout-secs <integer>` (optional). When unset, the
     driver computes the default at runtime as
     `max(operate_secs, 5)`.

5. **VariantDummy** (`variant-base/src/dummy.rs` or equivalent):
   - Override `signal_end_of_test` to return a fixed test eot_id (e.g.
     `0xDEADBEEF`) and `poll_peer_eots` to return self-as-peer (since
     dummy is loopback). Or keep the no-op defaults if the dummy
     truly has no peers — pick whichever makes the existing tests
     simpler. Document the choice in CUSTOM.md.

#### Tests (in `variant-base/`)

- Unit: trait default impls return `Ok(0)` / `Ok(vec![])` for a stub
  variant.
- Unit: driver eot-phase emits `phase=eot`, `eot_sent` (with the
  `eot_id` the variant returned), and `eot_timeout` (with the full
  peer set as `missing`) when the variant has no override and at
  least one expected peer.
- Unit: driver eot-phase emits `eot_received` exactly once per peer
  even if the variant returns the same peer twice (defensive dedup).
- Unit: driver eot-phase wait terminates immediately when expected
  set is empty (single-runner case).
- Integration (existing `variant-dummy` end-to-end): the dummy
  binary, run with `--peers self=127.0.0.1`, completes its lifecycle
  with the new EOT phase visible in the log file. No `eot_timeout`
  fires (single-runner -> empty expected set).
- Logger emits the new event types with the expected JSON shapes per
  `jsonl-log-schema.md`.

#### Validation against reality

- `cargo test --release -p variant-base` -> all-green.
- `cargo clippy --release -p variant-base --all-targets -- -D warnings`
  clean.
- `cargo fmt -- --check` clean.
- Run `variant-dummy` directly with a synthetic CLI invocation and
  capture the JSONL file. Verify `phase=eot`, `eot_sent`, no
  `eot_timeout`, and `phase=silent` appear in that order. Show the
  log lines in the completion report.
- Build the runner (`cargo build --release -p runner`) and run a
  single-runner config against `variant-dummy` end-to-end; verify
  exit 0 + same log shape.

#### Acceptance criteria

- [ ] `signal_end_of_test` and `poll_peer_eots` added to the trait
      with no-op default impls
- [ ] `PeerEot` struct added and re-exported
- [ ] Driver inserts the EOT phase between operate and silent
- [ ] `phase=eot`, `eot_sent`, `eot_received`, `eot_timeout` logged
      per the schema
- [ ] `--eot-timeout-secs` CLI flag added; default
      `max(operate_secs, 5)` when unset
- [ ] `variant-dummy` lifecycle still passes end-to-end with the new
      phase
- [ ] All existing `variant-base` tests still pass
- [ ] New unit tests for the EOT phase logic land
- [ ] `cargo test`, `cargo clippy`, `cargo fmt --check` clean
- [ ] STATUS.md updated under T12.1

#### Out of scope

- Implementing EOT in any concrete variant (that's T12.2-T12.5).
- Any analysis-tool wiring (T12.6).
- Cross-machine validation.

---

### T12.2: hybrid — implement EOT (TCP frame + UDP multicast)

**Repo**: `variants/hybrid/`
**Status**: pending
**Depends on**: T12.1 (trait + driver phase + log events). Independent
of T12.3-T12.5; can run in parallel.

Implement the two trait methods per the per-variant mechanics in
`metak-shared/api-contracts/eot-protocol.md` "Hybrid".

#### Scope

1. **TCP path (qos 3-4)**: send a tagged control frame on the same
   per-peer TCP stream after the last data frame. Encoding: extend
   the existing wire-format enum with a new `Eot { writer: String,
   eot_id: u64 }` variant. Receiver decodes frames as today; the
   read loop now produces either a `data` event (existing path) or
   an `eot` observation pushed onto an internal queue drained by
   `poll_peer_eots`.
2. **UDP path (qos 1-2)**: send a typed multicast packet, repeated 5
   times with 5 ms spacing between sends. Receivers maintain an
   internal `HashSet<(writer, eot_id)>` and dedupe.
3. `signal_end_of_test`:
   - Generate a 64-bit random `eot_id` (use `rand::random::<u64>()`;
     `rand` is already a workspace dep).
   - For each peer (TCP or UDP per current qos), send the EOT marker
     per the rules above.
   - Return the `eot_id`.
4. `poll_peer_eots`:
   - Drain the internal observation queue / set. Return the
     accumulated (writer, eot_id) pairs once per call. Subsequent
     calls return only newly-arrived observations.

#### Tests

- Unit: TCP wire-format roundtrip for the new `Eot` variant.
- Unit: UDP retry-and-dedup harness that simulates 5 sends from
  writer A and asserts the receiver returns A exactly once from
  `poll_peer_eots` regardless of which copies arrive.
- Unit: `signal_end_of_test` returns a non-zero `eot_id` and dispatches
  to all configured peers.
- Existing loopback integration test: still passes.

#### Validation against reality

- `cargo test --release -p variant-hybrid` -> all-green (existing
  tests + new ones).
- `cargo clippy`, `cargo fmt --check` clean.
- Run the existing two-runner-on-localhost loopback fixture
  (`tests/fixtures/two-runner-hybrid-only.toml`) manually with two
  runners; verify both JSONL files contain `phase=eot`, `eot_sent`,
  and `eot_received{writer=peer}` for the other runner. Show
  representative log lines in the completion report.

#### Acceptance criteria

- [ ] `Eot` variant added to wire format
- [ ] `signal_end_of_test` and `poll_peer_eots` overridden with the
      per-path mechanics above
- [ ] UDP retries (5 sends with 5 ms spacing) implemented and unit-
      tested
- [ ] Receiver dedupe by (writer, eot_id) implemented and unit-tested
- [ ] Existing tests still pass; new unit tests added
- [ ] Manual two-runner localhost run shows clean EOT exchange
- [ ] `cargo test`, `cargo clippy`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T12.3: custom-udp — implement EOT (TCP frame + UDP multicast)

**Repo**: `variants/custom-udp/`
**Status**: pending
**Depends on**: T12.1.

Same shape as T12.2, applied to custom-udp. The UDP path covers qos
1-3 (custom-udp uses UDP for L1-L3 and TCP for L4 only).

#### Scope

1. **TCP path (qos 4)**: same as Hybrid TCP. Extend the wire format
   in `src/protocol.rs` with an `Eot` frame variant.
2. **UDP path (qos 1-3)**: typed multicast packet, 5 retries with
   5 ms spacing. Receivers dedupe by (writer, eot_id).
3. `signal_end_of_test` / `poll_peer_eots`: same shape as T12.2.

Tests, validation, acceptance criteria mirror T12.2 with `qos 4` for
TCP and `qos 1-3` for UDP. Validate manually using the existing
`tests/fixtures/two-runner-custom-udp-qos4.toml` fixture for TCP and
the qos1-3 spawns of `configs/two-runner-all-variants.toml` for UDP
(or a small fixture if needed -- this would be a new fixture file
under `tests/fixtures/` with a single qos1 entry, NOT a sibling
log-dir).

---

### T12.4: quic — implement EOT (stream-end + datagram)

**Repo**: `variants/quic/`
**Status**: pending
**Depends on**: T12.1.

#### Scope

1. **Reliable streams (qos 3-4)**: close the data stream cleanly
   after the last write. The receiver treats stream-end (i.e.
   `recv_stream.read()` returning EOF cleanly) as EOT. Encode the
   `eot_id` in a small final-frame "trailer" before the stream close
   so the receiver can produce a `PeerEot` with the correct id.
2. **Datagrams (qos 1-2)**: typed datagram packet, 5 retries with
   5 ms spacing. Receivers dedupe.
3. `signal_end_of_test` / `poll_peer_eots`: per-qos branching.

Tests, validation, acceptance criteria mirror T12.2. Validate
manually using `variants/quic/tests/fixtures/two-runner-quic-only.toml`.

---

### T12.5: zenoh — implement EOT (sibling key)

**Repo**: `variants/zenoh/`
**Status**: pending
**Depends on**: T12.1.

#### Scope

1. **EOT publication**: publish a small message to
   `bench/__eot__/<self-runner-name>` from `signal_end_of_test`. The
   payload encodes the `eot_id` as a u64 (8-byte big-endian).
2. **EOT subscription**: in `connect`, declare a wildcard subscriber
   for `bench/__eot__/**` on the same session (separate subscriber
   from the data subscriber). When samples arrive, parse the writer
   from the key and the eot_id from the payload, push into an
   internal `HashSet<(writer, eot_id)>` for dedup + a queue for the
   poll method.
3. `signal_end_of_test`: generate eot_id, publish, return the id.
4. `poll_peer_eots`: drain the internal queue, return new pairs.

Tests, validation, acceptance criteria mirror T12.2. Validate
manually using both `1000paths` and `max-throughput` fixtures (the
T10.2b regression should still hold; EOT shouldn't reintroduce the
deadlock).

Important: ensure the EOT subscriber is declared on the SAME tokio
runtime + session as the data subscriber (per T10.2b's bridge
architecture). Don't open a second session.

---

### T12.6: analysis — operate-window scoping + late_receives metric

**Repo**: `analysis/`
**Status**: pending
**Depends on**: T12.1 (so the schema lands).
Independent of T12.2-T12.5; can run in parallel since the analysis
tool's behaviour only depends on the schema, not on whether any
particular variant has implemented EOT yet.

#### Scope

1. **Schema** (`analysis/schema.py`):
   - Add `eot_sent`, `eot_received`, `eot_timeout` to `KNOWN_EVENTS`.
   - Add `eot_id: pl.UInt64` to `SHARD_SCHEMA` (nullable; only
     populated for `eot_sent` and `eot_received` events).
   - The `missing` field on `eot_timeout` is variable-length; for
     the columnar shard, store it as a JSON-string `Utf8` column
     `eot_missing` (nullable).
   - Bump `SCHEMA_VERSION` to `"2"` so existing caches are
     invalidated cleanly.

2. **Parser** (`analysis/parse.py`): project the new fields per the
   schema, mirroring how `clock_sync` fields are projected today.

3. **Operate-window definition** (`analysis/correlate.py` and
   `analysis/performance.py`):
   - For each `(variant, run, runner)`, compute
     `eot_sent_ts = first ts where event=='eot_sent' and runner=<runner>`
     (i.e. each runner's own EOT send time).
   - Operate window per (writer, receiver):
     `[operate_start, writer.eot_sent_ts]` if `writer.eot_sent_ts` is
     present; else `[operate_start, silent_start]` (legacy fallback).
   - Loss% = 1 - (cross_peer_receives_in_window / writer_writes_in_window)
   - Late receives metric: count of receives with `ts > writer.eot_sent_ts`
     AND `ts <= silent_start`. Report this as a separate column in the
     performance table.

4. **Performance table** (`analysis/tables.py`): add a `Late` column
   showing the late_receives count (or `-` if no EOT data is
   available).

5. **Tests**: synthetic JSONL fixtures with `eot_sent`/`eot_received`
   events; assert operate-window scoping picks the right boundary;
   assert late_receives counts correctly; legacy fixtures (no EOT
   events) still produce results via the silent_start fallback.

#### Validation

- `python -m pytest tests/ -v` -> all-green; new tests added.
- Run on the existing small same-machine cache (no EOT events) ->
  output unchanged from current behaviour modulo whatever the new
  Late column shows (`-` for legacy logs).
- Once T12.2-T12.5 land, re-run on a freshly-collected dataset and
  confirm Late counts are sensible.

#### Acceptance criteria

- [ ] `SHARD_SCHEMA` updated with `eot_id`/`eot_missing`,
      `SCHEMA_VERSION` bumped to "2"
- [ ] Parser handles the three new event types
- [ ] Operate-window scoping uses `eot_sent_ts` when present, falls
      back to `silent_start` otherwise
- [ ] `late_receives` metric computed and surfaced in the
      performance table
- [ ] All existing analysis tests still pass
- [ ] New tests for operate-window scoping + late_receives land
- [ ] `ruff format --check` and `ruff check` clean
- [ ] STATUS.md updated

---

### T12.7: retighten T10.6 thresholds + 3-run validation

**Repo**: `variants/custom-udp/`, `variants/hybrid/`, `variants/zenoh/`
(and `variants/quic/` if T10.6 grew a quic test)
**Status**: pending
**Depends on**: T12.2, T12.3, T12.4, T12.5, T12.6 all done.

After EOT is implemented in every active variant and the analysis tool
scopes to the operate window, the T10.6 regression-test thresholds
are no longer constrained by silent_secs drain time. Retighten them
to the post-EOT contract.

#### Scope

For each variant's `tests/two_runner_regression.rs`:

1. Switch the JSONL parsing from "count writes vs receives across the
   whole spawn" to "count writes vs receives in the operate window"
   (i.e. mirror the analysis tool's logic in tests). The window
   boundaries come from the `eot_sent` event for the writer and
   `phase=operate` for the start.
2. Update the threshold constants per the contract:
   - **Hybrid TCP qos 3-4**: `>=99%`
   - **Hybrid UDP qos 1-2**: `>=99%` (correctness sweep) /
     `>=95%` (high-rate)
   - **Custom UDP TCP qos 4**: `>=99%`
   - **Custom UDP UDP qos 1-3**: per-fixture (start with `>=99%` for
     the existing reproducer, relax with rationale only if measured
     loss is structural)
   - **Zenoh `1000paths`**: `==100%` (already locked in)
   - **Zenoh `max-throughput`**: `>=80%` (documented mpsc-receive
     drop)

3. Re-run each test 3 times. All must pass deterministically. Capture
   wall-time and per-spawn delivery numbers in the completion report.

#### Acceptance criteria

- [ ] T10.6a, T10.6b, T10.6c regression tests scope counts to the
      operate window via `eot_sent`
- [ ] Threshold constants updated per the spec above
- [ ] Each test passes 3x deterministically; numbers documented
- [ ] STATUS.md updated under T12.7
- [ ] If any variant cannot meet the retightened threshold, do NOT
      relax the threshold silently -- file a follow-up task with
      the measured loss and the suspected root cause

#### Out of scope

- Cross-machine validation (user-owned).
- Adding new test fixtures (existing reproducers cover everything).
- Regressing the existing `--ignored` mark on these tests; they stay
  opt-in for `cargo test --release -- --ignored two_runner_regression`.

---

## Previous Sprint — E11: Analysis Tool — Large-Dataset Cache Rework

T11.1 is **done** (worker delivered the architecture before hitting a
session limit; orchestrator completed validation -- see STATUS.md).
T11.2 cleans up small residuals (lint, warm-run target overshoot, RSS
measurement, LEARNED entry).

### T11.2: Lint cleanup, warm-run optimisation, RSS measurement

**Repo**: `analysis/`
**Status**: pending
**Depends on**: T11.1 (landed)

T11.1 missed the final lint pass and a few small follow-throughs
because the worker hit a session limit before the tail of the work.
None of this changes the architecture; it polishes around the edges
and captures a learning.

#### Scope

1. **Lint cleanup** -- run `ruff format .` and commit the result.
   Files currently flagged: `analysis/cache.py`,
   `analysis/integrity.py`. Run `ruff check .` and remove the two
   unused imports in `analysis/tests/test_integration.py`
   (`scan_group` and `scan_shards` from `cache`). The helper
   `_all_groups` referenced them was scaffolded then left unused
   after `discover_groups` was added; if `_all_groups` is truly
   unused, delete it. Re-run `ruff check .` and `ruff format --check .`
   until both are clean.

2. **Warm-run target overshoot** -- bring the 40 GB warm run from
   ~37.6 s down under 30 s. Two suspected hot-spots in
   `analysis/cache.py`:

   - `update_cache` walks every sidecar even when nothing is stale.
     `_read_meta` does an `open` + `json.load` per file, so 128 files
     is ~128 syscalls + JSON parses per warm run. Cache the per-stem
     `ShardMeta` in the global sentinel so a fully-up-to-date cache
     short-circuits the per-file walk.
   - `discover_groups` reads the first row of every Parquet shard to
     recover its `(variant, run)` identity. That's 128 mini Parquet
     reads on every analysis. Persist the
     `(stem -> (variant, run))` map alongside the global sentinel and
     refresh only entries whose mtime changed.

   Both are pure-Python data-flow changes; neither perturbs the
   columnar pipeline. Re-run the warm benchmark and confirm wall-time
   <30 s. If the 30 s budget remains out of reach, document why in
   the completion report and propose either widening the target in
   EPICS.md or a follow-up task.

3. **Empirical peak-RSS measurement** -- add a one-shot RSS check
   (use `psutil.Process().memory_info().rss` in a separate sampler
   thread, spawned only when `analyze.py` is invoked with a new
   `--measure-peak-rss` flag). Default off so the pipeline stays
   instrumentation-free. Run the cold path against the 40 GB dataset
   with the flag on; record the peak in the T11.2 completion report
   and confirm it's under the 4 GB acceptance gate.

4. **`metak-shared/LEARNED.md` entry** -- orchestrator-owned (worker
   does NOT touch metak-shared). Worker should describe in the
   completion report what jitter-windowing decision was made and why
   it diverges slightly from Phase 1, and the orchestrator will
   transcribe a one-paragraph LEARNED entry from that text.

#### Tests

- `python -m pytest tests/ -v` -> all unit tests still pass (currently
  67 + 5 skipped). New tests if any: cover the mtime cache
  short-circuit in `update_cache` (two consecutive `update_cache`
  calls produce one shard build then no further reads beyond the
  sentinel) and the `--measure-peak-rss` flag round-trip.

#### Validation against reality

- `python analyze.py ../logs/inter-machine-all-variants-01-20260501_150858 --summary`
  warm run -> wall-time **<30 s** end-to-end (or document why not).
  Capture `time` output in the completion report.
- `python analyze.py ../logs/inter-machine-all-variants-01-20260501_150858 --summary --clear --measure-peak-rss`
  cold run -> wall-time still <10 min, peak RSS <4 GB, both reported.
  Note: rebuilds the entire 40 GB cache from scratch (~10 min and
  ~1.3 GB disk write); only run after the warm-path optimisation is
  functionally working, and tell the user before kicking it off.
- `ruff format --check .` and `ruff check .` clean.

#### Acceptance criteria

- [ ] `ruff format --check .` clean on `analysis/`
- [ ] `ruff check .` clean on `analysis/`
- [ ] `_all_groups` removed if unused, otherwise its imports preserved
- [ ] Warm 40 GB run wall-time <30 s OR rationale documented
- [ ] `--measure-peak-rss` flag implemented; cold 40 GB run reports
      peak RSS <4 GB
- [ ] `python -m pytest tests/ -v` still all-green (67+ passed,
      <=5 skipped)
- [ ] STATUS.md updated with timings and any deviations
- [ ] Worker-described jitter-divergence rationale provided so the
      orchestrator can write the LEARNED.md entry (orchestrator
      writes it, not the worker)

#### Out of scope

- Architecture changes to schema, cache layout, or polars pipeline.
- Plot or diagram changes (E5 territory).
- Anything touching `metak-shared/` (orchestrator-only).

---

### T11.3: Comparison-plot redesign — family colours, shared legend, readable scales

**Repo**: `analysis/`
**Status**: pending
**Depends on**: T11.1 (landed). Runs in parallel with T11.2 -- file overlap
is zero (T11.3 touches only `plots.py` and `tests/test_plots.py`; T11.2
touches `cache.py`, `integrity.py`, `analyze.py`, `tests/test_cache.py`,
`tests/test_integration.py`).

The current `plots.py` output is unreadable on the user's
all-variants-at-all-qos dataset. Concretely on the inter-machine 40 GB
run:

- 28 `tab10`-recycled bars per QoS group make every category visually
  identical (variants from different transport families end up with
  the same colour).
- Two separate `ax_*.legend()` calls produce two 28-row legend boxes
  that overlap each subplot.
- The variant-name parser splits on the last hyphen and so reads
  `custom-udp-1000x100hz-qos1` as transport=`custom-udp-1000x100hz`,
  load=`qos1` -- which is why qos ended up on the x-axis. With the
  E9 qos-expansion landed, the canonical name shape is now
  `<transport>-<workload>-qos<N>` and the parser needs to handle that
  shape explicitly.
- Latency y-axis is linear and the tens-of-ms qos1/qos2 bars (hybrid
  at high rate) crush the sub-ms qos3/qos4 reliable-transport bars
  to invisibility.

Goal: produce a single comparison figure that lets the user actually
read off relative performance at a glance.

#### Scope

1. **Variant-name parser** (`plots.py` -- new helper, e.g.
   `_split_variant_name`):
   - Recognise the four known transport prefixes: `custom-udp`,
     `hybrid`, `quic`, `zenoh`. (Source these from a module-level
     `TRANSPORT_FAMILIES` tuple so a future fifth family is easy to
     add.)
   - Strip a trailing `-qos<N>` suffix using a regex
     `re.compile(r"-qos(\d+)$")`. If absent, treat qos as None
     (single-qos legacy run -- still drawable).
   - Recognise the workload as the middle slice between the
     transport prefix and the qos suffix. Workloads observed in the
     current corpus: `10x100hz`, `10x1000hz`, `100x10hz`,
     `100x100hz`, `100x1000hz`, `1000x10hz`, `1000x100hz`, `max`.
     The parser should pass them through verbatim -- it does NOT
     need to validate the set, since that would block on adding new
     workloads later.
   - Variants that don't match any known transport prefix should be
     surfaced as transport=`other` with the full pre-qos string as
     workload -- don't crash.
   - Add 4-6 unit tests covering each transport, the no-qos legacy
     shape, the `max` workload, and the unknown-prefix fallback.

2. **Family-coloured palette**:
   - One sequential colormap per transport family. Suggested:
     - `custom-udp` -> matplotlib `Oranges`
     - `hybrid`     -> matplotlib `Purples`
     - `quic`       -> matplotlib `Blues`
     - `zenoh`      -> matplotlib `Greens`
   - Within each family, assign a tone per workload by sampling the
     colormap at evenly spaced positions in the range [0.4, 0.95] so
     the lightest tones don't disappear on white.
   - Workload ordering is deterministic and stable across families:
     sort by `(total_messages_per_second_estimate, name)` so the
     tone gradient corresponds to load intensity. Use a simple
     parser of `<vps>x<hz>` -> `vps * hz` plus a constant max-rank
     for `max`. If a workload doesn't parse, fall back to
     alphabetical -- don't crash.

3. **Layout**: keep the dual-metric figure but redesign so it scales
   to ~32 (transport, workload) pairs across 4 QoS values:
   - Worker's choice between two viable layouts -- pick whichever
     ends up cleanest after a real-data run-through:

     **Option A (preferred starting point)**: 1 row x 2 columns
     (Throughput | Latency), x-axis qos1..qos4, bars grouped by
     transport family, workloads as adjacent bars within each
     family-group. Bar width and group spacing tuned so the figure
     is wide enough that every bar is visible at typical rendering
     resolution (`figsize=(20, 7)` or larger is fine).

     **Option B**: 4 rows (one per transport) x 2 columns
     (Throughput | Latency). Per-transport y-axis auto-scaled.
     Yields 8 subplots; the per-row structure makes each family
     trivially readable but takes more vertical space.

   - Document the chosen layout in a top-of-file docstring with a
     one-line rationale.

4. **Single shared legend**:
   - Drop both `ax_*.legend()` calls.
   - Build one `fig.legend(...)` with the union of unique
     (transport, workload) handles, ordered by transport family
     then by workload load-intensity. Place it outside the plot
     area: `loc="lower center", bbox_to_anchor=(0.5, -0.02),
     ncol=<small>` -- pick `ncol` so the legend has roughly square
     footprint. Reserve room with `fig.subplots_adjust(bottom=...)`
     before saving so the legend isn't cropped.
   - Verify on the actual 40 GB output that no legend entries are
     clipped at the chosen DPI.

5. **Latency y-axis readability**:
   - Make the latency y-axis log-scale by default. Reliable transports
     report sub-millisecond latency at qos3/qos4 while hybrid at high
     rate spikes into the tens of ms; linear scale flattens the
     interesting values.
   - Whisker bars (currently encoding p95 +/- (p99-p95)) stay; verify
     they render correctly under log scale (a zero-or-negative lower
     bound on a log axis raises a `matplotlib` warning, so clamp the
     lower whisker to a small positive epsilon if needed).

6. **Robustness**:
   - When the input `results` is empty, keep the existing "No data
     to plot" placeholder behaviour.
   - When a single (transport, workload) pair has data for some QoS
     values but not others, render the missing ones as gaps rather
     than zero-height bars (so qos3-only entries don't collapse the
     y-axis at qos1/qos2).

#### Tests

- Add or extend tests in `analysis/tests/test_plots.py`:
  - `test_split_variant_name_*`: 4-6 cases as listed above.
  - `test_family_palette_returns_distinct_tones_per_workload`:
    given a transport and 4 workloads, sample colours; assert all
    distinct and within the [0.4, 0.95] range of their colormap.
  - `test_workload_load_ordering`: assert the load-intensity
    parser orders `(10x100hz, 100x100hz, 100x1000hz, 1000x100hz,
    max)` correctly.
  - `test_generate_comparison_plot_with_qos_expansion_data`:
    feed a synthetic `PerformanceResult` list covering 4
    transports x 2 workloads x 4 qos = 32 entries; assert the PNG
    is created, has more than zero size, and that
    `len(fig.axes)` matches the chosen layout (2 or 8).
  - `test_generate_comparison_plot_handles_missing_qos`: feed a
    result list missing some (transport, workload, qos)
    combinations; assert no exception, PNG produced.
  - `test_generate_comparison_plot_legend_outside_axes`: assert
    that `fig.legends` is non-empty and that no `ax.get_legend()`
    returns a legend (i.e. the per-axes legends are gone).

- Existing `test_generate_comparison_plot_*` tests need updates to
  match the new layout and palette. Do NOT delete coverage --
  rewrite tests where the underlying assertion still applies.

#### Validation against reality

- Run on the real 40 GB inter-machine cache:
  ```
  python analyze.py ../logs/inter-machine-all-variants-01-20260501_150858 --diagrams --output /tmp/t113-validation
  ```
  Open the resulting `comparison.png` and verify:
  - Each transport family is visually distinct from the others.
  - Within each family the tone gradient corresponds to load
    intensity in a way the user would call "obvious".
  - Latency log-scale shows the qos3/qos4 reliable bars (sub-ms)
    AND the qos1/qos2 high-rate spikes in the same panel.
  - The legend is fully visible (no clipping) and not overlapping
    any plot area.
- Also run on the small same-machine cache to confirm no regression
  on a less-dense dataset:
  ```
  python analyze.py ../logs/same-machine-20260430_140856 --diagrams --output /tmp/t113-small
  ```
- Capture both PNGs in the completion report (paths, file sizes,
  brief subjective verdict). Embed the actual files anywhere
  convenient under `/tmp/` -- do NOT commit PNG artifacts.

#### Acceptance criteria

- [ ] `_split_variant_name` parses `<transport>-<workload>-qos<N>`,
      no-qos legacy, and unknown-prefix shapes; covered by unit
      tests
- [ ] Family-coloured palette: 4 distinct colormaps, 8 distinct
      tones per family, all in the [0.4, 0.95] range
- [ ] Workload ordering by load intensity (with `max` last)
- [ ] Single `fig.legend(...)` outside the plot area; no
      `ax.legend()` calls remain
- [ ] Latency y-axis log-scale by default; whiskers render without
      warnings
- [ ] Missing (transport, workload, qos) combinations render as
      gaps, not zero bars
- [ ] PNG generated on the 40 GB dataset is visually readable per
      the criteria above (worker's subjective verdict + screenshot
      attached/described)
- [ ] All existing and new `test_plots.py` tests pass
- [ ] `ruff format --check .` and `ruff check .` clean on
      `analysis/`
- [ ] STATUS.md updated under a new T11.3 section

#### Out of scope

- Time-series plots, CDFs, histograms, radar charts (those are
  E5/E6 territory).
- Adding new metrics to `PerformanceResult` -- consume the existing
  fields.
- Re-running `--summary` -- this task only touches the
  `--diagrams` path.
- Anything touching `metak-shared/` (orchestrator-only).

---

### T11.1: Replace pickle cache with per-shard Parquet + lazy polars pipeline

**Repo**: `analysis/`
**Status**: done (see STATUS.md for the full completion report)
**Depends on**: nothing (E4 is shipped; E5/E6/E8 are not started or not
yet integrated, so there are no downstream consumers to coordinate with).

#### Background

Phase 1 (E4) builds a single `<logs-dir>/.analysis_cache.pkl` containing
every parsed `Event` dataclass instance, then `Cache.all_events()`
flattens that into one Python list and `sort()`s it by timestamp before
running correlation / integrity / performance. The whole flow assumes the
dataset fits comfortably in memory.

It does not. On the user's `logs/inter-machine-all-variants-01-20260501_150858/`
dataset (40 GB across 128 JSONL files, ~148 M lines) the pickle has grown
to 14.5 GB, `pickle.load()` is paging onto swap, and the run has not
produced output after hours.

This task replaces the pickle cache with per-source-file Parquet shards
and switches the analysis engine to polars lazy frames executed per
`(variant, run)` group, so peak memory is bounded by the largest single
group rather than by the whole dataset.

#### Scope

##### Storage and ingestion

1. **Cache directory** at `<logs-dir>/.cache/`:
   - One Parquet shard per source JSONL: `<jsonl-stem>.parquet`.
   - One sidecar per shard: `<jsonl-stem>.meta.json` with
     `{ "mtime": <float>, "row_count": <int>, "schema_version": "<str>" }`.
   - One global sentinel: `_cache_schema_version.json` with the current
     schema version string. Bumping the sentinel is the only way to
     force a rebuild across all shards (apart from `--clear`).
2. **Schema** — flat columnar, exactly the columns and dtypes listed in
   `metak-shared/ANALYSIS.md` § 4.1. Define it once as a Python constant
   `SHARD_SCHEMA: dict[str, pl.DataType]` and reference it from both the
   ingestion writer and the analysis readers. Set
   `SCHEMA_VERSION = "1"` initially.
3. **Stale detection** — a shard is stale when (any of):
   - sidecar missing, malformed, or `schema_version` mismatch
   - sidecar `mtime` < JSONL `mtime`
   - shard file missing
   Stale shards are rebuilt; orphan shards (no matching JSONL) are
   removed.
4. **Streaming parser** — replace `parse.parse_file` with a streaming
   ingester that reads the JSONL line by line, projects each line into
   the columnar schema, accumulates rows in a buffer (e.g. 100k rows),
   and writes them as a Parquet row group via `pl.DataFrame(...).write_parquet`
   in append mode (or accumulate to a list of small DataFrames and
   `pl.concat(..., how="vertical").write_parquet` once at end of file —
   pick whichever polars API yields the lowest peak memory; document the
   choice). Memory must be bounded by the buffer, not by the file size.
   Flush the sidecar only after the shard is fully written and synced.
5. **Legacy pickle migration** — on startup, if
   `<logs-dir>/.analysis_cache.pkl` exists, delete it and print a
   single line to stderr noting the deletion. Do not attempt to convert.
6. **`--clear`** — delete the entire `.cache/` directory before discovery.

##### Pipeline and analysis

7. **Driver (`analyze.py`)**:
   - After cache update, build a lazy frame:
     `lazy = pl.scan_parquet(str(logs_dir / ".cache" / "*.parquet"))`.
   - Discover groups: `groups = lazy.select(["variant", "run"]).unique().collect()`.
   - For each group, compute correlation, integrity (if requested),
     performance, and append to result lists.
   - Pass result lists to `tables.format_*` / `plots.generate_*` exactly
     as today.
8. **Correlation** (`correlate.py`):
   - Replace the dict-build / Python for-loop with a polars filter+join
     against the per-group lazy frame, producing a polars DataFrame with
     the same columns as the existing `DeliveryRecord` dataclass.
   - Keep a `DeliveryRecord` dataclass-shaped wrapper if needed for
     output-side compatibility, but the hot path is polars throughout.
9. **Integrity** (`integrity.py`):
   - Replace the Python loops with polars groupbys for completeness,
     ordering (using `seq.diff() < 0` after sort by `receive_ts`),
     duplicates (group by `(writer, seq, path)` count > 1), and gap
     events (filter event types and join detected vs filled).
   - The output remains a list of `IntegrityResult` dataclasses with the
     existing fields and error flags.
10. **Performance** (`performance.py`):
    - Replace per-pair Python aggregation with polars groupbys:
      `quantile` for percentiles, `groupby_dynamic` over `receive_ts`
      with a 1s window for jitter, count/duration for throughput,
      mean/max for resources, etc.
    - Output remains a list of `PerformanceResult` dataclasses (with
      embedded `ResourceMetric` list) with the existing fields. Keep
      these dataclasses so `tables.py` and the future plots module are
      unaffected.

##### Tests

11. **Unit tests**:
    - Stream-ingester writes a known Parquet shard and sidecar from a
      synthetic JSONL fixture. Verify schema, row count, and that
      re-running with unchanged mtime is a no-op.
    - Stale-detection logic (each of: missing sidecar, mtime drift,
      schema-version mismatch, orphan shard) yields the expected action.
    - `--clear` removes the cache directory.
    - Legacy pickle deletion: drop a stub `.analysis_cache.pkl`, run a
      build, verify it is removed.
    - Correlation join produces the same `DeliveryRecord` set on a
      synthetic fixture as the Phase 1 dict-based implementation
      (use the existing `correlate.correlate(events)` as ground truth
      against a small in-memory `events` list).
    - Integrity and performance results match Phase 1 output on a
      synthetic fixture for at least one case per QoS level.
12. **Regression test against the small real dataset**
    `logs/same-machine-20260430_140856/`:
    - Build cache from scratch, run `analyze.py --summary`, capture
      stdout.
    - Compare to a stored Phase 1 reference output (capture once before
      starting the rework — see "Pre-work" below). Equal modulo
      ordering of equal-key rows.

##### Validation against reality (mandatory before reporting done)

- Run `python analyze.py logs/same-machine-20260430_140856 --summary`
  end-to-end. Output must match the captured Phase 1 reference.
- Run `python analyze.py logs/inter-machine-all-variants-01-20260501_150858 --summary`
  end-to-end. Must complete in under 10 minutes wall-clock and stay
  under 4 GB peak RSS. Capture wall time and peak RSS in the completion
  report.
- Re-run the same command with the cache warm. Must complete in under
  30 seconds. Capture timing.

##### Pre-work (worker should do first, before deleting any code)

1. Capture Phase 1 reference output:
   `python analyze.py logs/same-machine-20260430_140856 --summary > /tmp/phase1_summary.txt 2>&1`
   and copy the file into `analysis/tests/fixtures/phase1_reference_summary.txt`.
   This is the ground-truth regression target.
2. Inspect the existing 14.5 GB `.analysis_cache.pkl` in the user's 40 GB
   logs directory to confirm it can be safely deleted. Do not delete it
   in the worker run — instead, document in the completion report that
   the file should be removed by the user once validation passes (it
   is a 14.5 GB pickle that the new build will replace with a
   .cache/ directory; the worker should NOT silently delete user data
   that large).

##### Out of scope

- Adding new metrics or output columns.
- Plot generation (`plots.py`) changes beyond what's needed to consume
  whatever shape `compute_performance` now returns. Existing plot
  output should remain functional on the small dataset.
- Multi-process orchestration. Polars's internal threading is sufficient
  per validation requirements.
- Clock-sync correction logic (T8.2). The schema reserves the columns
  but no analysis behaviour changes here.

#### CUSTOM.md update

Orchestrator updates `analysis/CUSTOM.md` separately to land:
- Polars added to allowed deps (justified — see ANALYSIS.md § 3.2 table).
- Updated tech stack list (polars, pyarrow as polars dep).
- Updated architecture description (per-shard Parquet under `.cache/`,
  lazy/per-group execution).
- Build/test commands updated if the polars install requires anything
  beyond `pip install polars`.

Worker should re-read `CUSTOM.md` after the orchestrator commits and
align.

#### Acceptance criteria

- [ ] `<logs-dir>/.cache/` Parquet shards + sidecars + global sentinel
      replace the monolithic pickle cache
- [ ] `SHARD_SCHEMA` constant and `SCHEMA_VERSION` defined in one place
      and referenced by ingester + readers
- [ ] Streaming ingester bounded by row-batch buffer (verify peak RSS
      during ingestion of the largest individual JSONL file
      `custom-udp-max-qos4-...jsonl` ~2.1 GB stays under 1 GB peak)
- [ ] Stale detection covers missing/mtime/schema/orphan cases
- [ ] Legacy `.analysis_cache.pkl` deleted on first run with stderr notice
- [ ] `--clear` removes `.cache/` directory
- [ ] `analyze.py` runs the analysis per `(variant, run)` group via
      `pl.scan_parquet` lazy frames; the full dataset is never
      materialized as Python objects
- [ ] `correlate.py` / `integrity.py` / `performance.py` reworked to
      polars; output dataclasses unchanged
- [ ] `tables.py` works unchanged on the new pipeline's output
- [ ] Phase 1 regression-output match on
      `logs/same-machine-20260430_140856/` (compare against captured
      reference in `analysis/tests/fixtures/phase1_reference_summary.txt`)
- [ ] User's 40 GB dataset analyses in <10 min cold / <30 s warm,
      <4 GB peak RSS — timings reported in completion report
- [ ] Existing 51 tests are updated or replaced as needed; new test
      total documented in completion report
- [ ] `ruff format --check` and `ruff check` clean on `analysis/`
- [ ] STATUS.md updated

---

## Current Sprint — E9: Peer Discovery Injection + QoS Expansion

See `EPICS.md` E9 for the rationale. Two tasks: T9.1 lands the runner
changes (new contract surface), T9.2 migrates the QUIC variant to consume
the new `--peers` arg and `--qos` per-spawn.

### T9.1: Runner — peer source IP capture, --peers injection, qos expansion

**Repo**: `runner/`
**Status**: pending
**Depends on**: contract updates in `metak-shared/api-contracts/` (already
landed: `runner-coordination.md` Phase 1 changes, `variant-cli.md`
`--peers` and `--qos` semantics, `toml-config-schema.md` qos optional/list
form + `<name>-qosN` expansion).

Bundles two coupled runner changes. They share spawn-construction code, so
it's cheaper to land them in one task than to land peer-injection and then
re-touch the same call sites for qos-expansion.

#### Part A — Peer source IP capture and --peers injection

Scope:
1. In `src/protocol.rs` discovery loop: switch from `recv` to `recv_from` so
   the source `SocketAddr` is available. Store `peer_hosts: HashMap<String, String>`
   on the `Coordinator` keyed by runner name; populate as `Discover` messages
   arrive.
2. New helper `src/local_addrs.rs`:
   - `pub fn local_interface_ips() -> HashSet<IpAddr>` — enumerate this
     machine's interface addresses. Use the `local-ip-address` crate
     (`list_afinet_netifas`) or `if-addrs`. Always include `127.0.0.1`.
   - Cache on first call.
3. Same-host detection: when storing a peer host, if the captured source IP
   is in `local_interface_ips()` OR equals `127.0.0.1`, store the string
   `"127.0.0.1"`. Otherwise store the source IP's string form.
4. Discovery completion criterion (in addition to existing checks): every
   name in `runners` has an entry in `peer_hosts`. Single-runner mode:
   self-populate with `127.0.0.1` and complete immediately.
5. Pass `peer_hosts` from `Coordinator` through to the spawn call site in
   `src/main.rs`. Format as `--peers name1=host1,name2=host2,...` (sorted by
   name for determinism). Inject in `src/cli_args.rs::build_variant_args`
   alongside the existing `--launch-ts`/`--variant`/`--runner`/`--run`.
6. Add Cargo dependency: `local-ip-address = "0.6"` (or `if-addrs = "0.13"`
   if preferred — pick one and document the choice in CUSTOM.md).

Tests:
- Unit: `local_interface_ips()` returns a non-empty set including
  `127.0.0.1`.
- Unit: same-host classifier correctly maps a local IP and a `127.0.0.1`
  source to `"127.0.0.1"`, and an arbitrary `192.168.x.y` to itself.
- Unit: `build_variant_args` includes `--peers` with sorted name=host pairs
  given a populated map.
- Integration (extend existing two-coordinator-on-localhost test in
  `tests/integration.rs`): after discovery, verify each coordinator's
  `peer_hosts` map contains every expected name and every value is
  `"127.0.0.1"` (since both run on the same host).
- Integration: end-to-end single-runner lifecycle with `variant-dummy` —
  verify the variant is invoked with a `--peers <self>=127.0.0.1` argument
  even though there are no peers.

#### Part B — qos expansion

Scope:
1. In `src/config.rs`: change `[variant.common].qos` field from `u8` to a
   custom enum/typed-form that accepts integer, array, or omission. Suggest:
   `pub enum QosSpec { Single(u8), Multi(Vec<u8>), All }` with a serde
   helper that maps `qos = N → Single(N)`, `qos = [..] → Multi(..)`,
   missing key → `All`. Validate elements are 1..=4.
2. New helper `pub fn QosSpec::levels(&self) -> Vec<u8>`:
   - `Single(n)` → `vec![n]`
   - `Multi(v)` → sorted unique copy of `v`
   - `All` → `vec![1, 2, 3, 4]`
3. In the main loop (`src/main.rs`), expand each `[[variant]]` entry by
   `qos_spec.levels()` into one or more "spawn jobs". A spawn job carries:
   - `effective_name`: original `name` if `levels.len() == 1`, otherwise
     `format!("{}-qos{}", name, qos)`.
   - `qos`: the concrete level for this spawn.
   - All other fields (binary, timeouts, common minus qos, specific) shared
     from the source entry.
4. Ready/done barriers and timeouts use `effective_name`. Each spawn is
   independent and runs the full stabilize/operate/silent cycle.
5. CLI arg construction in `src/cli_args.rs`: when building args for a spawn
   job, override `--qos` with the job's concrete level (so `[variant.common]`
   no longer needs to carry it as an integer) and use `effective_name` as
   `--variant`.
6. Sequential execution: spawn jobs from one source `[[variant]]` entry run
   in ascending QoS order before moving on to the next entry. Insert a small
   inter-job grace period (e.g. 250 ms) to let TCP/UDP sockets fully release
   before the next QoS spawn binds the same `base_port + qos_stride` port.
   Make this configurable via top-level `inter_qos_grace_ms` (optional,
   default 250).

Tests:
- Unit: `QosSpec` deserialization for all three forms (integer, array,
  omitted).
- Unit: `QosSpec::levels()` produces expected vectors; `Multi([3, 1, 1, 4])`
  → `[1, 3, 4]` (sorted unique).
- Unit: spawn-job expansion: a fixture with one entry having `qos = [1, 3]`
  produces 2 jobs with `effective_name` = `<name>-qos1` and `<name>-qos3`,
  both with the original `binary` and remaining common fields.
- Unit: a fixture with `qos = 2` (single integer) produces 1 job with
  unchanged `effective_name`.
- Unit: a fixture with `qos` omitted produces 4 jobs.
- Integration: build a single-runner config with `qos = [1, 2]` against
  `variant-dummy`, run end-to-end, verify two JSONL log files are
  produced (one per qos), each with the expected `qos` field in records.

#### Part C — Wiring + docs

1. Update `runner/CUSTOM.md` (orchestrator does this part — see below).
   Worker should re-read it after orchestrator updates and follow the
   guidance.
2. Update `runner/STRUCT.md` to reflect new `local_addrs.rs` module.

#### Validation against reality

- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Run `runner --name local --config <config-with-qos-omitted>` against a
  config that uses `variant-dummy` with `qos` omitted, and verify 4 log
  files appear with the correct `-qosN` naming.
- Run two runner instances on localhost in two terminals against the same
  config; verify both progress through all per-qos spawns in lockstep and
  both peer maps contain `"127.0.0.1"` for each peer.

#### Acceptance criteria

- [ ] `Coordinator` captures peer source IPs into `peer_hosts`
- [ ] Same-host detection collapses local-interface IPs and `127.0.0.1`
      sources to `"127.0.0.1"`
- [ ] `--peers <sorted name=host pairs>` injected into every variant spawn
- [ ] `QosSpec` accepts integer, array, or omitted; validation rejects
      out-of-range values
- [ ] Spawn-job expansion produces one job per QoS level; single-level
      keeps the original variant name
- [ ] Effective spawn name `<name>-qosN` used for `--variant`, ready/done
      barriers, log files
- [ ] Inter-job grace period applied between consecutive QoS spawns
- [ ] All unit tests for the new logic pass
- [ ] Integration test with `qos = [1, 2]` produces 2 distinct log files
- [ ] Two-runner-on-localhost integration still passes
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.2: QUIC variant — consume --peers, derive ports from base_port

**Repo**: `variants/quic/`
**Status**: pending
**Depends on**: T9.1 (needs the runner to inject `--peers` and pass `--qos`
per spawn).

Migrate the QUIC variant from explicit `bind_addr` and `peers` config
fields to a single `base_port` config field, with bind/connect addresses
derived at runtime from the runner-injected `--peers` and `--runner` args
plus the per-spawn `--qos`.

Scope:
1. CLI parsing (likely in `src/main.rs` or wherever QUIC's specific args
   are parsed): replace `--bind-addr` and `--peers` (the old variant-
   specific peers) with `--base-port <u16>`. Continue to accept the
   runner-injected `--peers <name=host,...>` (string).
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Treat any peer with the same
   name as self as the local entry. Fail loudly if `--runner` not in
   `--peers`.
3. Port derivation:
   - `runner_stride = 1`, `qos_stride = 10` (constants in code, documented
     in CUSTOM.md as matching the convention in `toml-config-schema.md`).
   - `my_bind_port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each peer (excluding self): `peer_port = base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind on `0.0.0.0:my_bind_port`; connect to `<peer_host>:peer_port` for
     every other peer.
4. Update `quic.rs` connect logic accordingly. Remove any code that depends
   on the old `--bind-addr` or variant-specific `--peers`.
5. Update `variants/quic/CUSTOM.md` to document `base_port` config and the
   port-derivation rules. Orchestrator will draft this — worker should
   re-read after the orchestrator commits the update.
6. Update the QUIC entries in `configs/two-runner-all-variants.toml`:
   - Remove `bind_addr` and `peers`.
   - Add `base_port = 19930`.
   - Drop the explicit `qos = 3` to let the runner expand to all 4 levels
     (the all-variants config is meant to be comprehensive). If the user
     wants only a subset, they can use `qos = [3]` form.
   - Keep the rest unchanged.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `base_port=19930`, `runner_index=1`, `qos=3`
  returns `19930 + 1 + 20 = 19951`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Loopback integration test: update the existing `tests/loopback.rs` to use
  the new CLI shape (synthesize `--peers self=127.0.0.1`, `--runner self`,
  `--base-port <free>`, `--qos 1` etc.). Verify the variant binds, connects
  to itself, and exchanges a message.

#### Validation against reality

- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Build the runner from T9.1 and the new QUIC binary. Run the updated
  `configs/two-runner-all-variants.toml` for QUIC entries only (e.g. comment
  out the others or use a small QUIC-only test config) on a single machine
  with two runners. Verify both runners cycle through QoS levels 1-4 and
  produce 8 JSONL log files (4 per runner).
- Spot-check a generated log file to confirm `qos` field matches the spawn
  name suffix.

#### Acceptance criteria

- [ ] QUIC `[variant.specific]` reduced to `base_port` (no `bind_addr`, no
      `peers` field)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] Bind/connect ports computed per the convention; off-by-one errors
      checked
- [ ] Same-host loopback test still passes with new CLI shape
- [ ] `configs/two-runner-all-variants.toml` QUIC entries updated to
      `base_port`-only with no explicit `qos`
- [ ] Two-runner end-to-end QUIC run produces correctly-named per-QoS
      JSONL files
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.3: Hybrid variant — consume --peers, derive TCP ports from tcp_base_port

**Repo**: `variants/hybrid/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` and `--qos` per spawn).

Same migration shape as T9.2 (QUIC), applied to the Hybrid variant. Hybrid
currently takes an explicit `peers = "host:port,..."` field in TOML, which
breaks any inter-machine run (peer IPs hard-coded as `127.0.0.1`). With
T9.1 landed, the runner already injects `--peers <name=host,...>`; Hybrid
just needs to consume it and derive its own TCP ports.

UDP multicast is left as-is — same group on every runner, no QoS stride
needed (sequential per-spawn execution + `silent_secs` drain +
`inter_qos_grace_ms` provide cross-spawn isolation, and multicast doesn't
have TIME_WAIT). Only TCP gets per-runner / per-qos port derivation.

Scope:
1. CLI parsing (`src/hybrid.rs::HybridConfig::from_extra_args` and/or
   `src/main.rs`):
   - Remove `--bind-addr` and the variant-specific `--peers` (the
     comma-separated `host:port` list).
   - Keep `--multicast-group` and `--tcp-base-port`.
   - Parse the runner-injected `--peers <name=host,...>` from extra args.
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Fail loudly if `--runner`
   is not in `--peers`.
3. Port derivation (constants in code, mirror QUIC convention):
   - `runner_stride = 1`, `qos_stride = 10`
   - `my_tcp_listen = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each non-self peer:
     `peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind TCP listener on `0.0.0.0:my_tcp_listen`. Connect to
     `(peer_host, peer_tcp_port)` for every peer except self.
4. UDP multicast: bind on `multicast_group` directly (no stride). All
   runners share the group.
5. Remove the now-dead `Cargo.toml` `mdns-sd` dependency if it's still
   listed (no `discovery.rs` exists, but the dep may be hanging on).
6. Update `variants/hybrid/CUSTOM.md` reference to `discovery.rs` if any
   linger — orchestrator already cleaned the main spec, but worker should
   re-read after orchestrator commits and align if needed.
7. Update the Hybrid entries in `configs/two-runner-all-variants.toml`:
   - Remove `peers` lines.
   - Drop the explicit `qos = 2` so the runner expands to all 4 levels
     (the all-variants config is meant to be comprehensive — and Hybrid
     is exactly the variant where comparing UDP-path QoS 1-2 vs TCP-path
     QoS 3-4 is most interesting).
   - Keep `multicast_group` and `tcp_base_port`.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `tcp_base_port=19900`, `runner_index=1`,
  `qos=4` returns `19900 + 1 + 30 = 19931`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Existing integration test: update to the new CLI shape with
  `--peers self=127.0.0.1`, `--runner self`, `--qos <1..4>`. With a
  single-peer map there are no peers to connect to (self excluded by
  design); the test now exercises bind/listen + message framing only.
  Cross-peer flow is covered by the manual two-runner validation below.

Validation against reality:
- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Use a small Hybrid-only test fixture (or temporarily comment out the
  non-Hybrid entries in `two-runner-all-variants.toml`) and run two
  runners on localhost in two terminals.
- Verify both runners cycle through all 4 QoS levels and produce 8 JSONL
  log files (4 per runner, named e.g. `hybrid-1000x100hz-qos1...`
  through `...-qos4...`).
- Spot-check at least one QoS 1-2 file (UDP path) and one QoS 3-4 file
  (TCP path) to confirm the `qos` field matches the spawn-name suffix
  and that records are present (i.e. cross-runner delivery worked on both
  transport paths).

Acceptance criteria:
- [ ] Hybrid `[variant.specific]` reduced to `multicast_group` + `tcp_base_port`
      (no `peers`, no `bind_addr`)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] TCP bind/connect ports computed per the convention; off-by-one
      errors checked
- [ ] UDP multicast still binds the configured group with no stride
- [ ] Loopback test passes with new CLI shape
- [ ] `mdns-sd` dependency removed from `Cargo.toml` if present
- [ ] `configs/two-runner-all-variants.toml` Hybrid entries updated:
      `peers` removed, `qos` removed
- [ ] Two-runner end-to-end Hybrid run produces correctly-named per-QoS
      JSONL files; both UDP and TCP paths verified
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.4a: Zenoh variant — make extra-arg parser lenient

**Repo**: `variants/zenoh/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` to every variant).

Zenoh's `ZenohArgs::parse` at `src/zenoh.rs` currently bails on any
unknown `--<name>` token in extra args. Since T9.1, the runner injects
`--peers <name=host,...>` into every variant. Zenoh has its own discovery
(Zenoh scouting) and does not need peer info — but the strict parser now
breaks every Zenoh run that goes through the runner.

This was missed during T9.1 because validation only covered the
already-migrated variants. Caught when the user tried the full
`two-runner-all-variants.toml` on two real machines and saw Zenoh
(and custom-udp) fail.

Scope:
1. In `src/zenoh.rs::ZenohArgs::parse`, replace the `bail!("unknown
   Zenoh argument: ...")` arm with a lenient skip:
   - When an unknown `--<name>` token is seen, advance past it AND the
     following token (treat as a `--name value` pair so the parser stays
     in sync with the standard convention used by the runner). If the
     unknown token doesn't start with `--`, just skip it.
2. Update the existing test that asserts `parse(&["--unknown"])` errors
   so it now succeeds (returns defaults). Add a new test that asserts
   `parse(&["--peers", "alice=127.0.0.1,bob=192.168.1.10"])` succeeds and
   leaves `mode`/`listen` at defaults.

Tests:
- `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean.
- The existing single-process Zenoh integration test still passes.

Acceptance criteria:
- [ ] `ZenohArgs::parse` ignores unknown `--<name> <value>` pairs without
      erroring
- [ ] Test for unknown-arg pass-through added
- [ ] Existing tests pass with the updated `--unknown` expectation
- [ ] `cargo test`, `cargo clippy`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.4b: Custom UDP variant — consume --peers, derive TCP port from tcp_base_port

**Repo**: `variants/custom-udp/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` and `--qos` per spawn).
Independent of T9.4a — can run in parallel.

Same migration shape as T9.3 (Hybrid), applied to the Custom UDP variant.
Custom UDP currently has its own `--peers` parser at `src/udp.rs:56-65`
expecting old-style `host:port,host:port`. Now that the runner injects
`--peers <name=host,...>`, that parser fails on the new format ("invalid
peer address: invalid socket address syntax"). It triggers regardless of
QoS because the parser runs unconditionally during config build.

Custom UDP uses TCP for QoS 4 only — for QoS 1-3 it's UDP-only and
peer-host info is not needed at the transport layer. So peer info is only
load-bearing for QoS 4, but the parser must succeed for ALL QoS values
(parse runs before connect).

UDP multicast is left as-is — same group on every runner, no QoS stride
needed. Only TCP gets per-runner / per-qos port derivation.

Scope:
1. In `src/udp.rs::UdpConfig::from_extra` (and any related main.rs
   plumbing):
   - Remove the old `--peers` (host:port list) and `--bind-addr` arg
     handling.
   - Add `--tcp-base-port <u16>` parsing (variant-specific, required).
   - Parse the runner-injected `--peers <name=host,...>` from extra args.
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Fail loudly if `--runner`
   is not in `--peers`.
3. TCP port derivation per spawn (only consumed at QoS 4):
   - `runner_stride = 1`, `qos_stride = 10`
   - `my_tcp_listen = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each non-self peer:
     `peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind TCP listener on `0.0.0.0:my_tcp_listen`. Connect to
     `(peer_host, peer_tcp_port)` for every peer except self.
4. UDP multicast: bind on `multicast_group` directly. NO runner stride,
   NO QoS stride. All runners share the group.
5. Remove `mdns-sd` from `Cargo.toml` if present.
6. Update the Custom UDP entries in `configs/two-runner-all-variants.toml`
   (8 entries):
   - Add `tcp_base_port = 19800` (or pick another free base — keep it
     distinct from Hybrid's `19900`).
   - The existing `multicast_group` and `buffer_size` stay.
   - `qos` stays omitted (T9.3 already removed it; runner expands to all
     four levels).
7. Update the loopback integration test to use the new CLI shape:
   `--peers self=127.0.0.1`, `--runner self`, `--multicast-group ...`,
   `--buffer-size ...`, `--tcp-base-port ...`, `--qos <1..4>`. With a
   single-peer map there are no peers to connect to; test exercises
   bind/listen + framing only.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `tcp_base_port=19800`, `runner_index=1`,
  `qos=4` returns `19800 + 1 + 30 = 19831`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Unit: parse succeeds at QoS 1 even though peer info is unused.
- Existing integration test: update to new CLI shape.

Validation against reality (orchestrator will run the cross-variant smoke
test in T9.4c — worker only needs to confirm same-machine):
- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Use a small Custom-UDP-only test fixture and run two runners on
  localhost. Verify both runners cycle through all 4 QoS levels and
  produce 8 JSONL log files. Spot-check at least one QoS 1-3 file (UDP
  path) and the QoS 4 file (TCP path) — the `qos` field must match the
  spawn-name suffix and there must be cross-runner receive records.

Acceptance criteria:
- [ ] Custom UDP `[variant.specific]` reduced to `multicast_group` +
      `buffer_size` + `tcp_base_port` (no `peers`, no `bind_addr`)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] Parse succeeds for all QoS values; TCP setup only runs at QoS 4
- [ ] TCP bind/connect ports computed per the convention
- [ ] UDP multicast still binds the configured group with no stride
- [ ] Loopback test passes with new CLI shape
- [ ] `mdns-sd` dependency removed from `Cargo.toml` if present
- [ ] `configs/two-runner-all-variants.toml` Custom UDP entries updated
      (`tcp_base_port = 19800` added)
- [ ] Two-runner-on-localhost end-to-end: 8 JSONL files, both UDP and
      TCP paths verified
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [ ] STATUS.md updated

---

### T9.4c: Cross-variant smoke run on the user's two-machine setup

**Repo**: top-level (no code; runs binaries)
**Status**: pending
**Depends on**: T9.4a, T9.4b, T9.3, T9.2, T9.1 (everything in E9).

Orchestrator-owned validation. Once T9.4a and T9.4b ship, run the full
`configs/two-runner-all-variants.toml` end-to-end across the user's two
machines (alice and bob) and confirm every variant × every QoS spawn
exits successfully and produces the expected JSONL log files.

Scope (orchestrator):
1. Confirm both `runner` and all four variant binaries (custom-udp,
   hybrid, quic, zenoh) are built in release on both machines.
2. Ask the user to run two runners against
   `configs/two-runner-all-variants.toml` and capture stdout.
3. Verify all 32 variant entries × QoS expansion produced log files on
   both machines without parse errors.
4. Spot-check one log file per variant family for receive records.
5. Add an entry to `metak-shared/LEARNED.md` summarising the regression
   uncovered by this run and the fix pattern.

Acceptance criteria:
- [ ] All-variants run completes without `[variant] error: ...` lines on
      either machine
- [ ] Per-variant per-QoS JSONL files appear on both machines
- [ ] Spot-checks confirm cross-runner delivery on each variant family
- [ ] Regression + fix pattern documented in LEARNED.md

---

## Current Sprint — E10: Variant Robustness Under Load

Variant-specific fixes for failures uncovered by the user's first
two-machine run of `configs/two-runner-all-variants.toml`. E9 contract
work is closed; this is implementation-level robustness in the variant
binaries themselves. The three tasks are independent and run in
parallel.

Background reading for any worker on this sprint:
- `metak-orchestrator/STATUS.md` "T9.4c: Cross-machine smoke run — done"
  has the full failure inventory.
- `metak-shared/LEARNED.md` "Cross-machine validation reveals failures
  invisible on localhost" explains why the failure modes diverge between
  loopback and real network.

### T10.1: Hybrid — robust UDP send and TCP send/read under load

**Repo**: `variants/hybrid/`
**Status**: pending
**Depends on**: nothing (E9 already shipped)

Hybrid variant fails 14/32 spawns at high throughput on the user's
two-machine run. Three concrete error patterns observed:

1. UDP send returns `WSAEWOULDBLOCK` (os error 10035) at high multicast
   rate (e.g. `hybrid-1000x100hz-qos1` and `qos2`, `hybrid-max-qos1`,
   `hybrid-max-qos2`, `hybrid-100x1000hz-qos2`). The Windows kernel send
   buffer fills faster than the NIC drains.
2. TCP write returns `WSAEWOULDBLOCK` (10035) for QoS 4 (e.g.
   `hybrid-1000x100hz-qos4`, `hybrid-max-qos4`). Send buffer pressure.
3. TCP write/read returns `CONNABORTED` (10053) or `CONNRESET` (10054)
   for QoS 3 (e.g. `hybrid-1000x100hz-qos3`, `hybrid-max-qos3`,
   `hybrid-100x1000hz-qos3`). Once one side bails on a WOULDBLOCK, the
   other side sees the connection drop and bails too.

The cascading aspect (3) is purely a downstream effect — fix (1) and (2)
properly and (3) largely goes away. But the TCP read loop should also
be hardened so a single peer disconnect doesn't fail the whole spawn.

The original [variants/hybrid/CUSTOM.md](../variants/hybrid/CUSTOM.md)
already specified:
> For `publish`, use blocking writes (small messages at ~1KB will fit
> in the kernel buffer and return immediately).

The TCP implementation diverged from that. UDP wasn't covered in the
spec but needs equivalent treatment.

Scope:
1. **TCP send (`src/tcp.rs` publish path)**: switch to **blocking writes**
   on the per-peer TCP socket as the original CUSTOM.md guidance
   specified. Keep `TCP_NODELAY` on. Blocking writes apply natural
   back-pressure on the writer when the kernel buffer fills, which is
   the right behaviour for the benchmark's "is TCP good enough?"
   question — back-pressure is part of TCP's reliability story and
   should be measured, not bypassed. If full conversion to blocking
   creates trouble for the polling read loop on the same socket, split
   into two `TcpStream` clones (one in blocking mode for send, one
   non-blocking for recv) — Windows allows this via `try_clone`.
2. **UDP send (`src/udp.rs` publish path)**: handle `WouldBlock` with a
   tight retry loop. Spin-yield (`std::thread::yield_now()`) up to a
   short cap (e.g. 1 ms wall-clock budget per send) before bailing.
   Document why making the UDP socket fully blocking is awkward
   (recv-side wants non-blocking) and that this hybrid approach is
   intentional. Optionally also bump `SO_SNDBUF` via `socket2` to
   reduce how often the retry triggers.
3. **TCP read loop (`src/tcp.rs` poll_receive path)**: when a per-peer
   stream returns `CONNABORTED`, `CONNRESET`, or unexpected EOF, log a
   single `eprintln!` warning, drop that peer's stream, and continue.
   Do NOT propagate the error up — one peer dropping must not fail the
   whole spawn (and at the protocol-driver layer the spawn should still
   complete its phases).
4. **CUSTOM.md update**: capture the UDP-retry policy and the
   "TCP read loop is fault-tolerant per-peer" rule alongside the
   existing TCP-blocking-write guidance, so future workers don't
   regress these decisions.

Tests:
- Unit: UDP retry loop returns `Ok(())` after a simulated WOULDBLOCK on
  the first attempt (use a `MockSocket` trait or a closure-based test
  shim around the send path). Asserts the retry happens and isn't an
  infinite loop.
- Unit: TCP read loop returns successfully (with the affected peer
  dropped) when one of two peer streams returns `ConnectionAborted`
  on read; the other peer's stream stays in the active set.
- Integration (existing two-runner-on-localhost loopback test): still
  passes.

Validation against reality (worker-owned):
- Run a Hybrid-only fixture (`tests/fixtures/two-runner-hybrid-only.toml`
  already exists from T9.3) with two runners on localhost and verify
  all 4 QoS spawns still succeed (regression check).
- Run a higher-throughput Hybrid-only fixture (clone with
  `tick_rate_hz = 100`, `values_per_tick = 1000`, `operate_secs = 5`)
  with two runners on localhost — both UDP (qos 1-2) and TCP (qos 3-4)
  paths must complete `status=success` for all 4 QoS levels.
- Cross-machine smoke is owned by the user (T10.5).

Acceptance criteria:
- [ ] TCP `publish` uses blocking writes; rationale documented in
      CUSTOM.md
- [ ] UDP `publish` retries on WOULDBLOCK with bounded budget; rationale
      documented in CUSTOM.md
- [ ] TCP read loop tolerates per-peer connection errors without failing
      the spawn
- [ ] Existing tests still pass; new unit tests for the retry/tolerance
      behaviour
- [ ] High-throughput hybrid-only two-runner-localhost run completes
      `status=success` across all 4 QoS spawns
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [ ] STATUS.md updated

---

### T10.2: Zenoh — investigation of path-count and max-throughput timeouts

**Repo**: `variants/zenoh/`
**Status**: pending — **investigation task**, not a direct fix
**Depends on**: nothing

Zenoh times out on 12/32 spawns of the user's two-machine run, with a
clear signature:

| values_per_tick × hz | total msg/s | distinct paths/tick | result |
|---|---|---|---|
| 10 × any | 1K-10K | 10 | succeed |
| 100 × 100/1000 | 10K-100K | 100 | succeed |
| 100 × 10 | 1K | 100 | succeed cross-machine (failed asymmetrically same-host — see LEARNED.md, that's a same-host artifact) |
| **1000 × any** | 10K-100K | **1000** | **all timeout** |
| max-throughput | very high | 100 | all timeout (separate failure mode) |

So the dominant signal is **distinct path count per tick**, not total
throughput. The `max-throughput` workload also times out but uses 100
paths — that's a separate code path worth a quick look.

This task is **scoped as investigation, not a one-shot fix.** The
deliverable is a clear diagnosis (and a small repro fixture), not
necessarily working code. The fix follows in a separate task once the
root cause is known.

Scope:
1. Build a minimal reproducer fixture: single Zenoh entry,
   `values_per_tick = 1000`, `tick_rate_hz = 10`, `operate_secs = 5`,
   `qos = 1`. Confirm two-runner-on-localhost reproduces the timeout
   (path-count failure should reproduce on loopback per LEARNED.md note
   3 — only the asymmetric `100x10hz` was a same-host artifact).
2. Add temporary diagnostic logging in the Zenoh variant covering:
   connect (session open, declare-publisher, declare-subscriber),
   publish hot path (per-key publisher creation if any), poll_receive
   (queue depth or backpressure signal), and disconnect (session close,
   teardown duration).
3. Run the repro and capture where time is spent. Common candidates:
   - One Zenoh keyexpression / publisher per path with per-publisher
     setup cost that scales with N keys (1000 setups in stabilize phase).
   - The internal tokio channel / FifoChannelHandler queue saturating
     and stalling polling.
   - `disconnect()` blocking on draining undelivered messages
     (silent_secs phase never returns).
4. Determine which it is. Write up the diagnosis in
   `metak-orchestrator/DECISIONS.md` (new entry) covering: what's wrong,
   what changed in the variant code that would fix it, estimated effort.
5. Separately for the `max-throughput` workload: run the same Zenoh
   variant against `workload = "max-throughput"` and capture which phase
   hangs. Note in the diagnosis whether it's the same root cause as
   path-count or a different one.

Out of scope for this task:
- Implementing the fix. That's a follow-up T10.2b once we know what
  to change.
- Optimising for raw Zenoh throughput. We just want spawns to terminate.

Acceptance criteria:
- [ ] Minimal reproducer fixture committed (under
      `variants/zenoh/tests/fixtures/`) and verified to reproduce
- [ ] Diagnosis written up in `metak-orchestrator/DECISIONS.md` with
      root cause for path-count timeouts AND `max-throughput` timeouts
      (could be same or different)
- [ ] Estimated scope of the follow-up fix (lines of code, files
      touched) documented
- [ ] Diagnostic logging used during investigation can be left in place
      behind a `--debug-trace` flag OR removed cleanly — pick one and
      justify
- [ ] STATUS.md updated with investigation outcome (and follow-up task
      T10.2b filed in TASKS.md if a fix is needed)

---

### T10.2b: Zenoh — fix path-count and max-throughput deadlock

**Repo**: `variants/zenoh/`
**Status**: pending
**Depends on**: T10.2 (investigation, done — see DECISIONS.md D7)

Zenoh times out on every spawn with `values_per_tick = 1000` and on
the `max-throughput` workload at the same path count. Root-cause
investigation in DECISIONS.md D7 traced this to `session.put().wait()`
hanging mid-tick on the synchronous routing path under symmetric
cross-peer high-fanout publishing. Both the
`tests/fixtures/two-runner-zenoh-1000paths.toml` and
`tests/fixtures/two-runner-zenoh-max.toml` reproduce the timeout
deterministically on localhost with two runners.

Three options were identified, in order of effort. Land them
incrementally — stop as soon as the 1000-paths fixture passes.

#### Option A (mandatory): cache per-path Publishers

Scope:
1. In `variants/zenoh/src/zenoh.rs`, add a
   `publishers: HashMap<String, Publisher<'static>>` field to
   `ZenohVariant` (drop the `'static` if lifetime ergonomics get
   ugly; you can use `Publisher<'_>` parameterised over the session
   if you store the session in an `Arc` or a `OnceLock`).
2. In `publish(path, ...)`, look up the publisher by `key` (the
   already-built `bench/<path>` string). On miss, call
   `session.declare_publisher(key.clone()).wait()`, store, then call
   `publisher.put(encoded).wait()` on either the cached or the
   freshly-declared one. On hit, just call `publisher.put(...).wait()`.
3. In `disconnect`, before undeclaring the subscriber, drain
   `self.publishers` (the `Drop` impl on `Publisher` undeclares
   automatically, but doing it explicitly via `for (_, p) in self.publishers.drain() { p.undeclare().wait()?; }` ensures errors surface and gives consistent teardown timing).
4. Fix the incidental double-prefix bug noted in DECISIONS.md D7:
   the workload paths come in as `/bench/N`, the variant strips the
   leading `/` and prepends `bench/` again, producing `bench/bench/N`.
   Either drop the extra prefix or change the strip target.
   Subscriber wildcard masks the bug today but it's wrong on its
   face. Adjust `bench/**` subscriber if the key shape changes.

Tests:
- Unit: extend the existing `test_message_codec_*` unit tests with
  a small assertion that the publish key derivation is consistent
  with what the subscriber wildcard matches (a no-op-cheap
  regression-protect after the prefix fix).
- The existing `tests/loopback.rs` integration test must still pass.

Validation against reality:
- `cargo test --release && cargo clippy --release -- -D warnings && cargo fmt --check`
  clean.
- Two-runner-on-localhost run against
  `tests/fixtures/two-runner-zenoh-1000paths.toml` — must complete
  `status=success` on both runners with the 60s timeout.
- Two-runner-on-localhost run against
  `tests/fixtures/two-runner-zenoh-max.toml` — must complete
  `status=success` on both runners.
- If either fixture still hangs, escalate to Option B in the same
  task (do not file a separate task; we already know what's needed).

#### Option B (escalation if A insufficient): tokio-bridge architecture

Scope (only execute if Option A's localhost validation still hangs):
1. In `variants/zenoh/src/zenoh.rs`, replace the synchronous
   `Wait::wait()` call sites with a dedicated `tokio::runtime::Runtime`
   (multi-thread, 2 workers) owned by the `ZenohVariant`.
2. Move the `Session` and `Subscriber` ownership inside a tokio task.
   Bridge:
   - `publish(path, payload, qos, seq)` → send a `(path, payload, qos, seq)`
     message over a bounded `tokio::sync::mpsc::channel` (size: 4×values_per_tick
     or 4096, whichever is larger) to the publisher task. The
     publisher task awaits `publisher.put(...).await`. If the channel
     fills, `publish` blocks the main thread until space is available
     (intentional back-pressure, but on a real channel that the tokio
     runtime can drain in parallel).
   - `poll_receive()` → `try_recv` from a second
     `tokio::sync::mpsc::channel` populated by a tokio task awaiting
     `subscriber.recv_async().await`.
3. `connect` initialises the runtime and spawns the publisher and
   subscriber tasks. `disconnect` signals shutdown via a oneshot,
   joins the tasks, then drops the runtime.
4. Match the QUIC variant's bridge pattern in `variants/quic/src/quic.rs`
   for layout consistency.

Tests:
- Loopback integration test continues to pass (now exercising the
  bridge end-to-end in single-process mode).
- Add a stress unit test (gated `#[ignore]` so `cargo test` stays
  fast) that publishes 10000 messages back-to-back through the
  bridge and verifies all 10000 land in the receive channel, asserting
  the bridge doesn't drop or deadlock under sustained pressure.

Validation against reality:
- Same two fixtures as Option A. Both must complete
  `status=success` on localhost.

#### Out of scope

- Switching Zenoh to client mode + a separate `zenohd` broker. That's
  Option C in the diagnosis, deliberately not pursued because it
  changes the benchmark's identity (broker-mediated vs peer-to-peer).
- Optimising raw Zenoh throughput. We just want spawns to terminate
  with `status=success`. Any throughput gain is a bonus.
- Removing the `--debug-trace` flag or the trace macros — DECISIONS.md
  D7 records the choice to keep them as a forward-debugging hook.

#### Acceptance criteria

- [ ] Per-path `Publisher` cache implemented in `src/zenoh.rs` with
      lookup-then-declare-on-miss
- [ ] Double-prefix `bench/bench/...` key bug fixed
- [ ] Two-runner-on-localhost run of `two-runner-zenoh-1000paths.toml`
      completes `status=success` on both runners
- [ ] Two-runner-on-localhost run of `two-runner-zenoh-max.toml`
      completes `status=success` on both runners
- [ ] Existing tests still pass; new regression test for key-shape
      consistency added
- [ ] If Option B was needed, additional stress test for the bridge
- [ ] `cargo test --release`, `cargo clippy --release -- -D warnings`,
      `cargo fmt -- --check` clean
- [ ] STATUS.md updated; cross-reference DECISIONS.md D7
- [ ] Cross-machine validation owned by user (next round of T10.5
      or a new T10.5b once T10.2b lands)

---

### T10.4: Custom UDP — fix TCP framing panic at qos4

**Repo**: `variants/custom-udp/`
**Status**: pending
**Depends on**: nothing

`variants/custom-udp/src/udp.rs:233` panics with
`range end index 4 out of range for slice of length 0` on the user's
cross-machine run, on `custom-udp-10x1000hz-qos4` (TCP path,
10K msg/s). The panic site:

```rust
// line 224-233
let mut len_buf = [0u8; 4];
match stream.read_exact(&mut len_buf) {
    Ok(()) => {
        let total_len = u32::from_be_bytes(len_buf) as usize;
        if total_len > self.config.buffer_size {
            // ... too large, drop peer
        } else {
            let mut msg_buf = vec![0u8; total_len];
            msg_buf[..4].copy_from_slice(&len_buf);  // <-- panics if total_len < 4
            // ...
        }
    }
    // ...
}
```

The `if total_len > buffer_size` check exists, but no check that
`total_len >= 4`. When the TCP stream returns a torn read at peer
shutdown, `read_exact` may have actually succeeded with garbage bytes
that decode as a too-small length (or zero), and the slice into the
undersized vec panics.

Why cross-machine only: on loopback the OS atomically tears down both
ends of a TCP connection so reads either get a complete frame or a
clean EOF. Across the network there's a real window where a partial /
torn read returns 0-3 bytes that look like a valid `read_exact`
completion but the contents are stale or zero. See LEARNED.md.

Scope:
1. **Bounds check**: before the `vec![0u8; total_len]` allocation, also
   check `total_len >= header_min_size` (likely 4, but use whatever the
   minimum-valid-frame size actually is given the protocol). If too
   small, treat as a peer protocol violation: log a single `eprintln!`,
   drop the peer's stream (`keep = false`), and continue. Do NOT panic.
2. **Sanity check on any other framing slice**: scan
   `variants/custom-udp/src/udp.rs` and `variants/custom-udp/src/protocol.rs`
   for any other `[..N]` slice or `vec![0u8; n]` where `n` came off the
   wire. Apply equivalent bounds checks.
3. **Tests**:
   - Unit: feed a `Vec<u8>` containing a 4-byte length prefix encoding
     0, 1, 2, 3, and 4 into the framing reader. Each undersized value
     must result in a graceful "drop peer" outcome, not a panic.
   - Unit: feed a length prefix encoding `buffer_size + 1`. Must drop
     peer (existing behaviour, regression-protect it).
   - Unit: feed a valid length prefix and a short payload (read_exact
     of the body returns `WouldBlock`). Existing behaviour is to retain
     the stream — regression-protect.
4. **CUSTOM.md** update: under "Message format" or a new "Framing
   safety" section, document the rule that any length-prefixed reader
   must validate `len >= header_min` before allocating, and that
   anything else from the wire is a peer protocol violation handled
   by dropping the peer.

Validation against reality (worker-owned):
- Same-machine reproducer: hard to deterministically reproduce the
  exact race that hit the user (it's cross-machine TCP teardown). But
  the unit test of the slice bug is sufficient to prove the panic is
  fixed.
- Run a Custom-UDP-only fixture two runners on localhost with the same
  parameters as the failing entry (`tick_rate_hz = 1000`,
  `values_per_tick = 10`, `qos = 4`, `operate_secs = 5`). Verify both
  spawns complete `status=success` (regression check; this passed for
  the user on localhost too).
- Cross-machine smoke is owned by the user (T10.5).

Acceptance criteria:
- [ ] No `vec![0u8; total_len]` reachable with `total_len < 4` (or
      whatever the minimum frame size is)
- [ ] Undersized length-prefix reads drop the peer cleanly, no panic
- [ ] All existing tests still pass; new unit tests for boundary
      conditions
- [ ] Same-machine high-rate qos=4 regression check still passes
- [ ] CUSTOM.md updated with the framing-safety rule
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [ ] STATUS.md updated

---

### T10.6: Automated two-runner regression tests (per-variant)

**Repos**: `variants/custom-udp/`, `variants/hybrid/`, `variants/zenoh/`
**Status**: pending — three independent sub-tasks (T10.6a/b/c), spawnable
in parallel since file overlap is zero (each touches only its own variant
repo)
**Depends on**: T10.1 done, T10.2b done, T10.4 done. The reproducer
fixtures already exist for every failure mode (see "Common test pattern"
below); this task only adds automated cargo tests that exercise them
end-to-end.

#### Why this exists

T10.1 / T10.2b / T10.4 each shipped fixes for a specific cross-machine
failure (Hybrid WSAEWOULDBLOCK cascade, Zenoh deadlock at 1000 paths /
max-throughput, Custom UDP TCP framing panic). Each has unit-level
regression coverage (algorithmic checks at the transport / framing
layer). What is missing is end-to-end coverage: a `cargo test`
invocation that spawns two `runner` processes against the per-failure
reproducer fixture, lets them run to completion, and asserts both
exited successfully with cross-peer delivery in the expected range.

Today that step is manual (two terminals + eyeball). T10.6 closes the
loop so a `cargo test --release -- --ignored two_runner_regression`
sweep gives a yes/no on whether all three fixes still hold.

#### Common test pattern (all three sub-tasks follow this)

Add ONE new test file `tests/two_runner_regression.rs` per variant repo.
All test fns gated `#[ignore]` so default `cargo test` stays fast; user
runs them via `cargo test --release -- --ignored two_runner_regression`.

Test fn structure (pseudocode applies to all three):

1. Skip-with-clear-message if `<repo-root>/runner/target/release/runner.exe`
   does not exist (point user at `cargo build --release -p runner`).
2. Skip-with-clear-message if `<repo-root>/variants/<name>/target/release/variant-<name>.exe`
   does not exist.
3. Allocate a `tempfile::tempdir()` for `log_dir`. Read the fixture
   file, substitute the line `log_dir = "./logs"` with
   `log_dir = "<tmpdir>"`, write to `<tmpdir>/config.toml`. (The
   fixture's `binary` path is `variants/<name>/target/release/...`
   relative to the runner's CWD — leave it as-is.)
4. Spawn two `runner` child processes from CWD = repo root with
   `--name alice` / `--name bob` and `--config <tmpdir>/config.toml`.
   Capture stdout+stderr.
5. Wait for both with a generous timeout (~120 s; fixtures complete
   in 10-30 s normally). Hard-kill if exceeded; that is a test
   failure with a clear message.
6. Assert both children exited 0.
7. Glob the per-spawn JSONL files under
   `<tmpdir>/<run-name>-<launch-ts>/` (the runner auto-creates the
   session subfolder). For each `(spawn-name, runner)` log file,
   count `event:"write"` and `event:"receive" + writer:"<peer>"`.
   Assert receive-count >= write-count * (delivery-threshold) per
   the per-test specs below.
8. Print a one-line per-spawn summary to stdout
   (`alice -> bob qos1: 1005/1005 (100.00%)` etc) so the test output
   itself is the audit trail.

Use `serde_json` for JSONL parsing — it's already a transitive dep
in the workspace via `arora_types`. Add `tempfile` to dev-deps if not
already there.

#### Sub-task: T10.6a — custom-udp two-runner regression

**Repo**: `variants/custom-udp/`

One test fn, `two_runner_regression_qos4_no_panic`, against
`tests/fixtures/two-runner-custom-udp-qos4.toml`.

Asserts:
- Both runners exit 0
- For each runner's qos4 JSONL: receive-count from the OTHER runner
  is >= 99% of the OTHER runner's write count
- Stderr does NOT contain `panic` (case-insensitive)
- Stderr MAY contain `[custom-udp] TCP framing: dropping peer ...`
  (this is the expected cleanup message from the T10.4 fix and proves
  the regression-prone code path is exercised)

Background: T10.4 fixed `range end index 4 out of range for slice of
length 0` at `src/udp.rs:233`. The fixture pushes 50K writes per
runner at qos 4 (TCP path) and triggers TCP teardown that previously
panicked.

Validation (worker-owned):
- Build the runner: `cargo build --release -p runner` from repo root
- Build custom-udp: `cargo build --release -p variant-custom-udp`
- Run the test: `cargo test --release -p variant-custom-udp -- --ignored two_runner_regression`
- Capture wall-time and delivery numbers in the completion report

#### Sub-task: T10.6b — hybrid two-runner regression

**Repo**: `variants/hybrid/`

Two test fns:

1. `two_runner_regression_correctness_sweep` against
   `tests/fixtures/two-runner-hybrid-only.toml` — covers all 4 QoS
   levels at modest rate (100 Hz x 10 vps). Asserts both runners exit
   0; for each (spawn, runner) log file the cross-peer receive count
   is >= 99% of writes for QoS 1-2 (UDP best-effort/latest) and 100%
   for QoS 3-4 (TCP reliable).

2. `two_runner_regression_highrate_no_cascade` against
   `tests/fixtures/two-runner-hybrid-highrate.toml` — exercises the
   T10.1 fix at 100 Hz x 1000 vps (100K msg/s). Asserts both runners
   exit 0; for each (spawn, runner) log file the cross-peer receive
   count is >= 95% on UDP path (qos 1-2; some loss is expected at
   that rate) and >= 99% on TCP path (qos 3-4; back-pressure-driven
   not loss-driven). Stderr MAY contain
   `[hybrid] TCP read error from peer ... dropping` and similar
   per-peer-fault messages from the T10.1 fix; presence does NOT
   fail the test, only "all peers dropped on first tick" would
   (the spawn would have aborted).

Background: T10.1 fixed Windows `WSAEWOULDBLOCK` cascading drops on
both UDP and TCP send paths plus per-peer fault tolerance in the
read loop.

Validation: same as T10.6a.

#### Sub-task: T10.6c — zenoh two-runner regression

**Repo**: `variants/zenoh/`

Two test fns:

1. `two_runner_regression_1000paths_no_deadlock` against
   `tests/fixtures/two-runner-zenoh-1000paths.toml` — the
   deterministic deadlock trigger from T10.2b. Asserts both runners
   exit 0; cross-peer delivery == 100% (50000/50000 writes for
   each direction per the T10.2b validation report).

2. `two_runner_regression_max_throughput_no_deadlock` against
   `tests/fixtures/two-runner-zenoh-max.toml` — exercises the
   max-throughput tight loop. Asserts both runners exit 0; cross-peer
   delivery >= 80% (matches the documented receive-channel drop
   semantic; T10.2b's stress test asserts the same threshold for the
   same reason).

Background: T10.2b fixed deadlock at >225 writes per spawn via a
per-key Publisher cache + dedicated tokio runtime with mpsc bridge.

Validation: same as T10.6a.

#### Acceptance criteria (per sub-task)

- [ ] `tests/two_runner_regression.rs` exists with the per-sub-task
      test fns
- [ ] `tempfile` in dev-deps if newly added
- [ ] Each test fn `#[ignore]`-by-default
- [ ] Each test passes locally on the worker's machine; wall-time
      and delivery numbers documented in the completion report
- [ ] `cargo test --release` (without `--ignored`) still all-green
      (regression-protect: the new file must not break the default
      test set)
- [ ] `cargo clippy --release --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] STATUS.md updated under T10.6a / T10.6b / T10.6c

#### Out of scope

- Cross-machine validation. That stays user-owned (T10.5 / a future
  T10.5b once all of T10.6a-c land). The point of T10.6 is to give
  the user something to run locally before committing to a full
  cross-machine sweep.
- Adding new failure modes / new fixtures. The existing fixtures are
  the contract; if a future failure mode emerges, it gets its own
  fixture + its own test fn.
- Touching anything outside the worker's variant repo.

---

### T10.5: User cross-machine re-run after T10.1 + T10.4 land

**Repo**: top-level (no code; runs binaries)
**Status**: pending
**Depends on**: T10.1, T10.4 (T10.2 separately filed if it produces
a fix task T10.2b)

User-owned: re-run `configs/two-runner-all-variants.toml` on the alice
and bob machines once T10.1 and T10.4 ship. Confirm:

- Custom UDP: 32/32 succeed (no panic).
- Hybrid: 32/32 succeed (or document any residual high-throughput
  failures as expected back-pressure).
- QUIC: 32/32 still succeed (regression check).
- Zenoh: same baseline failure pattern as the previous run (pending
  T10.2 fix landing separately).

If failures persist on Custom UDP or Hybrid after T10.1/T10.4, file
follow-up tasks with the new error patterns rather than reopening these.

---

## Previous Sprint — E8: Application-Level Clock Synchronization

Cross-machine latency cannot be measured without correcting for clock skew
between runner machines. See `metak-shared/api-contracts/clock-sync.md`
for the full protocol.

### T8.1: Runner — clock-sync protocol implementation

**Repo**: `runner/`
**Status**: done — verified end-to-end on localhost smoke run 2026-05-03
**Depends on**: contract review by user

Implement the NTP-style offset measurement and persist results to a JSONL
log file. Variants are NOT touched.

Scope:
1. New module `src/clock_sync.rs`:
   - Add `ProbeRequest { from, to, id, t1 }` and
     `ProbeResponse { from, to, id, t1, t2, t3 }` variants to the existing
     `Message` enum in `src/message.rs`.
   - `pub struct ClockSyncEngine` holding the existing UDP socket handle
     plus a counter for probe IDs.
   - `pub fn measure_offsets(&self, peers: &[String], n_samples: usize) -> HashMap<String, OffsetMeasurement>`
     where `OffsetMeasurement` carries `offset_ms`, `rtt_ms`, `samples`,
     `min_rtt_ms`, `max_rtt_ms`.
   - Per peer: send `n_samples` `ProbeRequest`s with 5 ms inter-sample
     delay. Wait up to 100 ms per response. Pick the sample with smallest
     RTT. Compute offset and rtt as defined in `clock-sync.md`.
   - Always-respond logic: when the runner's UDP receive loop sees a
     `ProbeRequest` addressed to it, immediately send back a
     `ProbeResponse` with `t2` (receive time) and `t3` (send time). This
     must work even when the runner is in a barrier — add probe handling
     to the existing loops in `src/protocol.rs`.

2. New JSONL writer `src/clock_sync_log.rs`:
   - `pub fn open_clock_sync_log(log_dir: &Path, runner: &str, run: &str) -> ClockSyncLogger`.
   - File name: `<runner>-clock-sync-<run>.jsonl`. Same dir as variant logs.
   - `pub fn write(&mut self, variant: &str, peer: &str, m: &OffsetMeasurement)`.
   - JSONL schema per `jsonl-log-schema.md` `clock_sync` event.

3. Wire-in in `src/main.rs`:
   - After discovery completes, before first ready barrier: call
     `measure_offsets` for all peers, write each result with `variant=""`.
   - For each variant after its ready barrier and before spawn: call
     `measure_offsets` again, write with `variant=<name>`.
   - Single-runner runs skip both calls (no peers).

4. Tests:
   - Unit: offset math given known timestamps → expected `offset_ms` and
     `rtt_ms`. Min-RTT selection picks the right sample.
   - Unit: serialize/deserialize `ProbeRequest`/`ProbeResponse`.
   - Integration: two `ClockSyncEngine` instances on localhost — same
     machine so true offset is 0. Verify `|offset_ms| < 1.0` and
     `rtt_ms > 0`.
   - Integration: end-to-end runner launch on localhost (same machine) —
     verify the clock-sync JSONL file appears, has the expected number
     of entries (1 initial + N variants per peer), and offset is near zero.

Acceptance criteria:
- [ ] `Message` enum includes `ProbeRequest` and `ProbeResponse`
- [ ] `ClockSyncEngine::measure_offsets` returns per-peer measurements
- [ ] Probe responses are sent promptly even while in barrier loops
- [ ] `<runner>-clock-sync-<run>.jsonl` written with one entry per
      (peer, measurement_event)
- [ ] Initial sync runs after discovery; per-variant sync runs after each
      ready barrier
- [ ] Single-runner mode produces no clock-sync log (or an empty file)
- [ ] Localhost integration test: `|offset_ms| < 1.0`
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean

---

### T8.2: Analysis — apply offsets when computing cross-machine latency

**Repo**: `analysis/`
**Status**: done — verified end-to-end on localhost smoke run 2026-05-03
**Depends on**: contract review by user (can run in parallel with T8.1)

**Phase 1.5 architecture is in place** (E11 done). The `peer`, `offset_ms`,
and `rtt_ms` columns are already in `analysis/schema.py::SHARD_SCHEMA` and
`parse.py::project_line` already projects `clock_sync` events into them.
T8.2 builds on that foundation — it does NOT change the schema and does
NOT bump `SCHEMA_VERSION` (so no cache rebuild is forced).

Scope:
1. Verify `clock_sync` ingestion end-to-end:
   - Add a unit test in `tests/test_parse.py` for a `clock_sync` line ->
     correct columnar projection (peer, offset_ms, rtt_ms populated; the
     diagnostic fields samples / min_rtt_ms / max_rtt_ms are accepted but
     ignored).
   - Confirm `cache.py` picks up `*-clock-sync-*.jsonl` files from the run
     directory (it should — same `*.jsonl` glob — but verify with a small
     fixture).

2. New module `analysis/clock_offsets.py`:
   - `def build_offset_table(group_lazy: pl.LazyFrame) -> pl.DataFrame`:
     filter the lazy group for `event == "clock_sync"`, return a small
     DataFrame with `[ts, runner, peer, variant, offset_ms]` sorted by
     `(runner, peer, ts)`.
   - Semantics: a row with `runner=R`, `peer=P`, `offset_ms=X` means
     P's clock is X ms ahead of R's clock as observed by R. To convert a
     receive timestamp logged by R into the writer P's frame, add X.

3. Update `correlate.py::correlate_lazy`:
   - After the existing join produces the delivery LazyFrame, attach a
     per-row offset using polars `join_asof` keyed on
     `(run, receiver, writer)` <-> `(run, runner, peer)` with `receive_ts`
     <-> `ts` (latest `ts <= receive_ts`).
   - Prefer the per-variant resync: do the asof join twice — first with
     `variant == current_variant`, then with `variant == ""` for any rows
     that are still null, and coalesce.
   - For same-runner rows (`writer == receiver`), force `offset_ms = 0`
     and `offset_applied = True`.
   - For cross-runner rows where no offset row matched, set
     `offset_ms = null` and `offset_applied = False`. `latency_ms` stays
     uncorrected for these.
   - Replace `latency_ms` for cross-runner-with-offset rows with the
     corrected value: `base_latency_ms + offset_ms`.

4. `DeliveryRecord` dataclass gains:
   - `offset_ms: float | None`
   - `offset_applied: bool`

5. Tables: `tables.py::format_performance_table` should mark a row as
   `(uncorrected)` whenever any underlying delivery record had
   `offset_applied == False`. Append the marker to the latency cells.
   Keep the change minimal; do not add a new column.

6. Tests:
   - `tests/test_clock_offsets.py`: `build_offset_table` returns expected
     rows; latest-ts-with-variant-fallback semantics covered by
     correlate-level tests since that's where it's applied.
   - `tests/test_correlate.py`: synthetic fixture with two runners and a
     +50 ms clock_sync entry on the receiver side. Without correction,
     latency would be ~150 ms (50 ms skew + 100 ms real); after
     correction, ~100 ms. Per-variant entry preferred over initial sync.
   - `tests/test_correlate.py`: same-runner deliveries unaffected
     (`offset_applied == True`, `offset_ms == 0`).
   - `tests/test_correlate.py`: missing offset -> `offset_applied == False`,
     `offset_ms is None`, latency uncorrected, no exception.
   - `tests/test_integration.py`: end-to-end with a fixture run directory
     containing variant logs + a clock-sync log; corrected latency
     surfaces in the resulting tables.

Validation (must run before reporting done):
- `python -m pytest tests/ -v` — all 67 prior tests still pass plus new ones.
- Re-run `python analyze.py ../logs/<existing-run-without-clocksync>` —
  still works, latency cells annotated `(uncorrected)`.
- `ruff format --check` and `ruff check` clean.

Acceptance criteria:
- [ ] `tests/test_parse.py` covers `clock_sync` projection
- [ ] `clock_offsets.py` module exposes `build_offset_table`
- [ ] `correlate_lazy` applies offsets via asof join; per-variant preferred
- [ ] `DeliveryRecord` carries `offset_ms` and `offset_applied`
- [ ] Same-runner records unaffected (offset_applied=True, offset=0)
- [ ] Cross-runner records with available offset have corrected latency
- [ ] Missing-offset case: latency uncorrected, `offset_applied=False`,
      `(uncorrected)` annotation visible in CLI output
- [ ] All 67 prior tests still pass; new tests pass
- [ ] `ruff format --check` and `ruff check` clean
- [ ] Re-run on existing `logs/<run>` (no clock-sync) still works

---

### T8.4: Investigate occasional clock-sync offset outlier (follow-up)

**Repo**: `runner/`
**Status**: done — root cause identified as Windows clock quantization /
transient time correction; mitigated via 5σ outlier rejection +
median-of-three-lowest-RTT fallback. Stress harness (100 iter × 32 samples)
shows zero outliers; smoke re-run shows all 10 measurements within
±0.073 ms. Hypothesis 1 (stale ProbeResponse cross-talk) was eliminated by
audit: `(from, to, id)` triple is unique and verified. Defense-in-depth
`t1` echo check added. Per-sample debug log
(`<runner>-clock-sync-debug-<run>.jsonl`) wired through. Contract updated.
**Depends on**: T8.1 done

During T8.1 localhost validation (smoke-t94c-20260503_115309), one of
five alice→bob clock-sync measurements reported `offset_ms=-387.44`
despite `min_rtt_ms=0.18` (the LOWEST RTT of all alice's measurements in
that run). Bob's reciprocal measurement during the same window was
-0.13 ms. The other 9 measurements across both runners were tight (±0.3 ms).

Hypotheses:
1. **Stale ProbeResponse cross-talk**: a `ProbeResponse` for a probe ID
   from a previous measurement window arrived and was incorrectly matched.
   Audit `clock_sync.rs` ID-based matching to verify in-flight responses
   from a prior `measure_offsets` call cannot leak into a subsequent one.
2. **Windows clock quantization edge**: timestamp rounding could push
   `(t2 - t1)` and `(t3 - t4)` into asymmetric quantization buckets,
   though this should not produce ±400 ms deltas.
3. **Transient clock jump**: Windows w32time made a correction during
   the measurement window. If so, document mitigation (e.g. detect and
   discard samples whose offset deviates beyond 3σ from the rest).

Scope:
1. Reproduce on localhost (run several smoke benchmarks back-to-back to
   maximise chances of seeing the outlier).
2. Add additional fields to the JSONL diagnostic block: per-sample t1,
   t2, t3, t4 for the chosen sample plus the rejected one with the next-
   best RTT. Helps diagnose without re-running.
3. Implement an outlier-rejection step: if the chosen sample's offset
   deviates by > 5 × std-dev of the other samples' offsets, fall back to
   the median-of-three-lowest-RTT sample. Document the heuristic in
   `clock-sync.md`.
4. Verify with a longer run (~30 minutes, ~50 measurements) that no
   single outlier remains.

Acceptance criteria:
- [ ] Root cause identified (or definitively ruled out for hypothesis 1)
- [ ] If a protocol bug exists, it is fixed and tested
- [ ] If outlier rejection is added, the heuristic is documented and unit-tested
- [ ] Re-run does not produce ±100 ms outliers across 50+ measurements
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all clean

---

### T8.3: End-to-end two-machine validation

**Repo**: top-level (no code; runs binaries and analysis)
**Status**: pending
**Depends on**: T8.1, T8.2

Validate that on a real two-machine setup, latency numbers are no longer
dominated by clock skew.

Scope:
1. Run a short benchmark on two machines (e.g. `two-runner-quic-10x100.toml`).
2. Verify clock-sync log files are produced on both runners.
3. Verify reported `offset_ms` is reasonable (single-digit ms or less on a
   quiet LAN; large but consistent if clocks are far apart).
4. Verify reported latency is in the expected range (< 10 ms target per
   DESIGN.md), as opposed to the previous behavior where it would reflect
   the clock skew.
5. Document findings in `metak-shared/LEARNED.md`.

Acceptance criteria:
- [ ] Two-machine benchmark completes successfully
- [ ] Clock-sync logs produced on both machines
- [ ] Cross-machine latency in reasonable range; not dominated by skew
- [ ] Findings documented in LEARNED.md

---

### T8.5: Clock-sync silently fails across machines — root-cause + harden

**Repo**: `runner/` (with possible `metak-shared/` doc updates)
**Status**: in progress
**Depends on**: T8.1, T8.2, T8.4 (all done)

**Field report (2026-05-05).** User ran `configs/two-runner-all-variants.toml`
between alice and bob = `192.168.1.77` (real LAN, not localhost). Run id
`all-variants-01-20260505_171445`. Symptoms:

- **alice's stdout**: prints `[runner:alice] WARN: no clock-sync samples
  received from peer=bob for variant <name>` for *every* per-variant
  resync; the initial-sync line shows the same WARN. So `pick_best`
  returned `None` for every measurement (zero `ProbeResponse` ever
  matched).
- **alice's `logs/all-variants-01-20260505_171445/alice-clock-sync-all-variants-01.jsonl`**:
  exists but **0 lines**. The sibling `*-clock-sync-debug-*.jsonl` is
  also **0 lines**. So alice's `is_single_runner()` returned false
  (file was created), but no sample ever produced a row.
- **bob's machine**: no `bob-clock-sync-*.jsonl` file exists at all in
  the equivalent log directory, and bob's stdout has **no `clock_sync`
  log lines** (initial or per-variant). That implies bob's
  `coordinator.is_single_runner()` returned `true` on bob's side OR
  the file open failed silently OR bob's `peer_names` filter excluded
  alice — none of which should be possible if discovery saw alice.
- **Yet variants completed successfully** with `status=success,
  exit_code=0` on alice — meaning the ready/done barriers DID round-trip
  with bob over the same UDP coordination socket. So bidirectional
  coordination works at the message-exchange level, but `ProbeRequest`/
  `ProbeResponse` traffic is silently failing.

This means **every cross-machine latency number in this run is
uncorrected** (per-machine clocks differ by Windows-w32time-scale
amounts, swamping the 10 ms target). It also means the runner today
silently produces statistically invalid data when clock-sync fails —
unacceptable for benchmark trust.

**Scope** — three pieces, in this order:

1. **Diagnose the asymmetry between coordination messages (work) and
   probe messages (don't).**
   - Read `runner/src/protocol.rs` and `runner/src/clock_sync.rs`. The
     send path for probes is `socket.send_to` over `peer_addrs`
     (multicast + per-peer-localhost fan-out); the discover/ready/done
     messages use the *same* fan-out. Why would probes drop while
     barriers succeed?
   - Hypotheses to check by code inspection / instrumentation:
     - **Bob's `is_single_runner()` flips after discovery** (e.g. peer
       map cleared between Phase 1 and Phase 1.5; or
       `clock_sync_engine()` consults a different runners list than
       barriers do; or `peer_names` filter on bob is excluding alice
       because alice's name compares mismatch).
     - **Probe filter mismatch**: `ProbeRequest`'s `to` field is
       checked against `self_name`; if either side has a name with
       mismatched casing/whitespace from what's stored in the peer
       map, every probe gets dropped silently. Discover/ready/done
       barrier messages may key on different fields and not hit the
       same mismatch.
     - **`wait_for_response` blocks on `socket.recv_from` with a read
       timeout that interacts badly with the in-flight datagrams from
       barrier / discovery rebroadcast traffic** — ProbeResponse can
       arrive *after* the per-sample 100 ms deadline if the socket's
       receive queue has unrelated barrier traffic in front of it.
       Note `loop { match recv_from }` consumes one datagram per
       iteration; if non-Probe traffic is queued first the deadline
       can expire while still draining barrier messages.
     - **Bob's clock-sync engine is never instantiated**: trace
       `Coordinator::clock_sync_engine()` — does it return `None` on
       bob due to e.g. an empty peer_addrs list? If so, why does
       barrier coordination still work?
   - Add lightweight tracing (behind a `--verbose-clock-sync` flag or
     `RUST_LOG`-style toggle, NOT permanent stderr noise) that on bob
     prints: did `is_single_runner()` evaluate to true/false? did
     `clock_sync_engine()` return Some/None? on alice prints: per
     `wait_for_response` call, what datagrams were received during
     the wait window and why each was rejected (wrong `to`, wrong
     `from`, wrong `id`, wrong `t1`, parse failure, non-Probe variant).

2. **Harden against silent failure** — apply regardless of root cause:
   - **Initial-sync zero-sample = fatal.** If the initial sync produces
     zero samples for any listed peer, `eprintln!` a clear error and
     **exit non-zero** before the first ready barrier. Cross-machine
     latency without correction is meaningless; we must not let a
     run produce contaminated data silently. Per-variant resyncs may
     remain warnings (analysis falls back to the most recent valid
     measurement).
   - **Debug file logs every probe attempt, not just successful ones.**
     Today `clock_sync_log` only writes to the debug JSONL when a
     `ProbeResponse` is matched. That gives **zero signal** in the
     failure mode just observed (both files are empty when nothing
     matches). Extend the `RawSample` / debug-row schema with a
     `result: "ok" | "timeout" | "rejected_filter" | "rejected_t1" |
     "parse_error"` field, and write one row per `ProbeRequest`
     attempted, regardless of outcome. Update
     `metak-shared/api-contracts/clock-sync.md` to reflect the new
     debug-file schema.
   - **Bob-side log open should not depend on `is_single_runner()`
     alone.** If bob's `is_single_runner()` is wrongly returning true
     (one of the diagnosis hypotheses), the silent skip is the bug.
     Even if not the root cause, log a one-line "skipping clock-sync:
     single-runner mode" message to bob's stdout when this branch is
     taken so the user can see it.

3. **Reproduction / diagnostic instructions for the user to run on
   bob.** The user has explicitly offered to launch anything on bob's
   machine. Provide:
   - The exact runner command to run on bob with the new verbose
     flag enabled.
   - What stderr lines to capture and send back.
   - What files to look for (or confirm absent) in bob's log directory
     after the run.
   - Document this in `metak-shared/LEARNED.md` under a "Diagnosing
     clock-sync failure on a real LAN" section so it's reusable.

**Validate by:**
- Existing `cargo test` for runner stays green.
- Add at least one unit test for the new debug-row "result" variants
  (probe sent but no response → row with `result="timeout"`).
- Add at least one unit test that initial-sync zero-sample produces a
  non-zero exit (can be at the `main`-level helper or a function
  extracted from the new fail-fast logic).
- `cargo clippy -- -D warnings`, `cargo fmt -- --check` clean.

**Cannot fully validate without user action**: the cross-machine
reproduction requires alice + bob. Once code lands, ask the user to
re-run with the verbose flag enabled and report the captured output.
The worker writes a STATUS.md update including the exact commands and
what to look for.

**Acceptance criteria:**
- [ ] Root-cause hypothesis identified, with evidence pointing to which
      of the listed hypotheses (or a new one).
- [ ] Initial-sync zero-sample now causes a non-zero exit.
- [ ] Debug JSONL writes one row per probe attempt with `result` field.
- [ ] Bob-side `is_single_runner()` skip emits a visible log line.
- [ ] Verbose-tracing toggle implemented for both alice (probe-receive
      filter) and bob (engine-init / single-runner branch).
- [ ] Contract `clock-sync.md` updated for new debug-file schema and
      new fail-fast behavior.
- [ ] LEARNED.md updated with "Diagnosing clock-sync failure" section.
- [ ] Unit tests added covering the new behaviors.
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean in `runner/`.
- [ ] Completion report includes precise instructions for the user to
      reproduce on the alice/bob LAN with verbose tracing.

---

## Completed — E1: Variant Base Crate

All tasks done. See STATUS.md for completion report.

---

## Completed — E2: Benchmark Runner

### T1: Crate scaffold + TOML config parsing + CLI arg construction

**Repo**: `runner/`
**Status**: pending
**Depends on**: nothing

Scaffold the Rust binary crate and implement config parsing and the CLI arg
builder that converts TOML config sections into variant CLI arguments.

Scope:
1. Initialize `Cargo.toml` as a binary crate. Add dependencies: `clap`
   (derive), `toml`, `serde`, `serde_json`, `sha2`, `chrono`, `anyhow`,
   `socket2`.
2. CLI (`src/main.rs`): `runner --name <name> --config <path.toml>`.
   Validate that `--name` matches one of the runner names in the config.
3. Config struct (`src/config.rs`):
   - Top-level: `run` (String), `runners` (Vec<String>),
     `default_timeout_secs` (u64).
   - `[[variant]]`: `name` (String), `binary` (String),
     `timeout_secs` (Option<u64>), `common` (toml::Table),
     `specific` (Option<toml::Table>).
   - Parse from TOML file path. Run validation rules from the
     `toml-config-schema.md` contract.
   - `config_hash()` method: SHA-256 of the raw file bytes, hex-encoded.
4. CLI arg builder (`src/cli_args.rs`):
   - `fn build_variant_args(variant: &VariantConfig, run: &str, runner_name: &str, launch_ts: &str) -> Vec<String>`
   - Iterates `variant.common` table: for each key-value, converts
     `snake_case` key to `--kebab-case`, formats value as string.
   - Appends `variant.specific` entries the same way.
   - Appends runner-injected args: `--launch-ts`, `--variant`, `--runner`,
     `--run`.
   - Must match `api-contracts/variant-cli.md` exactly.
5. Unit tests:
   - Parse a sample TOML config, verify all fields.
   - Verify config hash is deterministic.
   - Verify CLI arg construction: given known config, produce expected
     arg vector. Check kebab-case conversion, value formatting, ordering.
   - Verify validation rejects: empty `run`, empty `runners`, duplicate
     variant names, missing `binary`.

Acceptance criteria:
- [ ] `Cargo.toml` with all listed dependencies
- [ ] CLI parses --name and --config, validates name is in runners list
- [ ] Config struct matches TOML schema contract exactly
- [ ] config_hash() returns deterministic SHA-256 hex
- [ ] CLI arg builder converts snake_case to --kebab-case correctly
- [ ] Runner-injected args (--launch-ts, --variant, --runner, --run) appended
- [ ] Validation catches invalid configs
- [ ] Unit tests pass, cargo clippy clean

---

### T2: Child process spawning and monitoring

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1 (needs config and CLI arg builder)

Implement child process lifecycle: spawn, monitor, timeout, collect exit code.

Scope:
1. Create `src/spawn.rs`.
2. `ChildOutcome` enum: `Success`, `Failed(i32)`, `Timeout`.
3. `fn spawn_and_monitor(binary: &str, args: &[String], timeout: Duration) -> Result<ChildOutcome>`:
   - Validate binary path exists before spawning.
   - Record `launch_ts` as RFC 3339 nanosecond timestamp immediately
     before `Command::new(binary).args(args).spawn()`.
   - Return the `launch_ts` alongside the outcome (caller needs it for
     the done barrier).
   - Wait for child exit. Use a separate thread or `child.try_wait()`
     polling loop to implement timeout.
   - On timeout: kill the child process (platform-appropriate), return
     `Timeout`.
   - On normal exit: return `Success` if exit code 0, `Failed(code)`
     otherwise.
4. Unit/integration test:
   - Spawn `variant-dummy` (from variant-base) with valid args, verify
     `Success` outcome.
   - Spawn a nonexistent binary, verify error.
   - Test timeout by spawning a process that sleeps longer than the
     timeout (e.g. `sleep 999` or a small script), verify `Timeout`.
     Use a very short timeout (2-3 seconds) to keep tests fast.

Note: the `variant-dummy` binary must be pre-built. The test should check
for its existence and skip with a clear message if not found.

Acceptance criteria:
- [ ] spawn_and_monitor returns Success/Failed/Timeout correctly
- [ ] launch_ts is recorded immediately before spawn
- [ ] Binary path is validated before spawning
- [ ] Timeout kills the child process
- [ ] Integration test with variant-dummy passes
- [ ] Timeout test passes (short timeout, child killed)
- [ ] cargo clippy clean

---

### T3: UDP coordination protocol

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1 (needs config for runner names and config hash)

Implement the leaderless discovery and barrier sync protocol over UDP
broadcast.

Scope:
1. Message types (`src/message.rs`):
   ```rust
   enum Message {
       Discover { name: String, config_hash: String },
       Ready { name: String, variant: String },
       Done { name: String, variant: String, status: String, exit_code: i32 },
   }
   ```
   Serialize/deserialize as JSON. Keep it simple.

2. Coordination engine (`src/protocol.rs`):
   - `Coordinator` struct holding a UDP broadcast socket (send + receive),
     this runner's name, the set of expected runner names, and the config
     hash.
   - **Port**: default 19876, configurable via `--port` CLI arg
     (add to clap struct).
   - **Bind**: `0.0.0.0:<port>`, broadcast to `255.255.255.255:<port>`.
     Use `socket2` for `SO_BROADCAST` and `SO_REUSEADDR`.

3. `discover(&mut self) -> Result<()>`:
   - Periodically broadcast `Discover` message (every 500ms).
   - Listen for `Discover` from other runners.
   - Verify config_hash matches; abort with clear error if mismatch.
   - Complete when all runner names seen with matching hash.
   - **Single-runner optimization**: if `runners` has only this runner's
     name, return immediately without any network I/O.

4. `ready_barrier(&mut self, variant_name: &str) -> Result<()>`:
   - Broadcast `Ready` for this variant, listen for Ready from all others.
   - Re-broadcast every 500ms until all runners have reported ready.
   - Single-runner: return immediately.

5. `done_barrier(&mut self, variant_name: &str, status: &str, exit_code: i32) -> Result<HashMap<String, (String, i32)>>`:
   - Broadcast `Done` for this variant with own status, listen for Done
     from all others.
   - Return a map of runner_name -> (status, exit_code) for reporting.
   - Single-runner: return immediately with own result.

6. Unit tests:
   - Serialize/deserialize each message type.
   - Single-runner discover, ready, done all return immediately.
   - Two-coordinator test on localhost: spawn two Coordinator instances
     on different ports (or same port with SO_REUSEADDR), verify they
     discover each other and complete barriers. Use threads.

Acceptance criteria:
- [ ] Message types serialize/deserialize correctly as JSON
- [ ] Single-runner mode completes all protocol steps without network I/O
- [ ] Discovery detects config hash mismatch and aborts
- [ ] Barriers complete when all runners have reported
- [ ] Re-broadcast handles UDP packet loss
- [ ] Two-runner localhost test passes
- [ ] cargo clippy clean

---

### T4: Main loop + integration tests

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1, T2, T3

Wire everything together and validate the full runner lifecycle.

Scope:
1. Main loop (`src/main.rs`):
   - Parse CLI, load and validate config.
   - Create Coordinator, run discovery.
   - For each variant in config order:
     a. Run ready barrier.
     b. Build CLI args from config.
     c. Spawn variant binary, monitor with timeout.
     d. Run done barrier with outcome.
     e. Print summary line (variant name, status, exit code per runner).
   - Exit 0 if all variants completed, exit 1 if any failed.

2. Sample config file (`tests/fixtures/single-runner.toml`):
   ```toml
   run = "test01"
   runners = ["local"]
   default_timeout_secs = 30

   [[variant]]
   name = "dummy"
   binary = "../variant-base/target/release/variant-dummy"

     [variant.common]
     tick_rate_hz = 10
     stabilize_secs = 0
     operate_secs = 2
     silent_secs = 0
     workload = "scalar-flood"
     values_per_tick = 5
     qos = 1
     log_dir = "./test-logs"

     [variant.specific]
   ```

3. Integration tests (`tests/integration.rs`):
   - **Single-runner lifecycle**: Run runner with single-runner.toml config
     and `--name local`. Verify exit 0, JSONL file produced in test-logs/,
     file contains expected events.
   - **Timeout handling**: Config with a variant binary that hangs (e.g.
     `sleep` or a script), short timeout (3s). Verify runner reports
     timeout and exits non-zero.
   - **Config validation**: Attempt to run with --name that isn't in
     runners list, verify error message.
   - **Multi-variant config**: Config with two variant entries (both
     pointing at variant-dummy with different names). Verify runner
     executes both in order, two JSONL files produced.

4. Create STRUCT.md describing the file layout.

5. Print a summary table to stdout after all variants complete:
   ```
   Benchmark run: test01
   Variant                  Runner   Status    Exit
   dummy                    local    success   0
   ```

Acceptance criteria:
- [ ] Single-runner lifecycle test passes end-to-end
- [ ] Runner spawns variant-dummy, waits for exit, reports success
- [ ] JSONL log files are produced in the configured log_dir
- [ ] Timeout test: runner kills hung variant and reports timeout
- [ ] Config validation: bad --name is rejected with clear error
- [ ] Multi-variant: both variants executed in order
- [ ] Summary table printed to stdout
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean, `cargo fmt -- --check` clean
- [ ] STRUCT.md exists and describes the file layout
