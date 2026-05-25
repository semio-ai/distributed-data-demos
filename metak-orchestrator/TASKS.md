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

## Visualization Follow-ups (analysis)

### T11.4: Latency CDF chart + relax epsilon clamp in comparison plot

**Repo**: `analysis/`
**Status**: done — landed in `6488362` (clamp lowered to 1e-5 ms +
NaN-on-non-positive), `a05fd62` (downsampled latency samples on
`PerformanceResult`, cap 50_000), `2891f63` (CDF chart + CLI wire-up,
emits `latency_cdf.png` next to `comparison.png` under `--diagrams`).
142 tests pass. Worker report at STATUS.md commit `c95a5c0`. Visual
reproduction of the user-reported sub-µs floor wasn't possible
against `two-machines-all-variants-01` (zero deliveries for
custom-udp/quic in that dataset); mechanism verified via unit test
`test_nonpositive_p95_renders_as_nan_bar` and the new CDF view.
**Depends on**: nothing — independent enhancement.

User feedback: the current `generate_comparison_plot` latency subplots
pin sub-microsecond transports (custom-udp, quic) at the
`_LATENCY_EPSILON_MS = 1e-3` floor, hiding signal in the µs region and
making it impossible to compare those transports against each other.
Add a CDF view that exposes distribution shape, and lower the epsilon
clamp so genuine sub-µs values aren't pancaked.

#### Scope

1. **Add a CDF visualisation** to `analysis/plots.py`. New entry point
   `generate_latency_cdf_plot(results, output_path)` that produces a
   per-QoS row of CDF subplots (one column per QoS, or N rows × 1 col
   — pick whichever stays legible at 4 QoS levels and ~6 transport
   families). One line per `(transport, workload)` combo, x = latency
   in ms (log scale), y = empirical CDF in [0, 1]. Reuse the family
   colormap / tone scheme from `_FAMILY_COLORMAPS` so it reads
   consistently with the bar chart.
   - Source data: per-message `latency_ms` column already aggregated
     by `performance.py`. Confirm whether the raw delivery records
     are exposed on `PerformanceResult`; if only percentiles are kept,
     extend the result struct minimally to also carry a sampled
     latency vector (cap at e.g. 50k samples per result to bound
     memory) and have the cache rebuild forward it.
   - Wire it into the analysis CLI alongside the existing comparison
     plot — same flag/output dir, separate file (e.g.
     `latency_cdf.png`).

2. **Relax the epsilon clamp** in `generate_comparison_plot`.
   - Lower `_LATENCY_EPSILON_MS` from `1e-3` to `1e-5` ms (10 ns) —
     well below any plausible measurement, so it only kicks in to
     avoid log-axis warnings on negative/zero quantiles from clock
     noise.
   - Where a percentile is ≤ 0, prefer skipping the bar (NaN) over
     clamping, so the chart visually communicates "no positive data"
     rather than implying ~1 µs.

#### Acceptance criteria

- `latency_cdf.png` is generated alongside `comparison.png` for an
  existing logs dir; CDF curves for custom-udp / quic at qos1-2 show
  visible separation in the µs region rather than collapsing.
- The existing `comparison.png` no longer pins distinct sub-µs
  results to the same 1e-3 ms floor; running it on the same logs as
  the chart the user shared shows custom-udp and quic at lower
  positions than before, with their relative ordering visible.
- Existing analysis tests pass; add at least one unit test for the
  CDF computation (e.g. monotonic non-decreasing y, bounded in
  [0, 1], correct length).
- No changes outside `analysis/`.

#### Out of scope

- Changing the bar-chart layout itself (faceted multiples, scatter,
  heatmap) — captured as separate follow-ups if desired.
- Touching variant code or the JSONL schema.

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

1. Skip-with-clear-message if `<repo-root>/target/release/runner.exe`
   does not exist (point user at `cargo build --release -p runner`).
2. Skip-with-clear-message if `<repo-root>/target/release/variant-<name>.exe`
   does not exist.
3. Allocate a `tempfile::tempdir()` for `log_dir`. Read the fixture
   file, substitute the line `log_dir = "./logs"` with
   `log_dir = "<tmpdir>"`, write to `<tmpdir>/config.toml`. (The
   fixture's `binary` path is `target/release/variant-<name>...`
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
   binary = "target/release/variant-dummy"

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

---

## E3f: WebSocket Variant

See `EPICS.md` E3f and `metak-shared/variant-candidates.md` R6 for the
driving design. Single task — the variant is small enough that splitting
it adds more coordination overhead than it removes.

### T3f.1: variants/websocket — implement WebSocket variant end-to-end

**Repo**: `variants/websocket/`
**Status**: done (2026-05-06; see STATUS.md). All acceptance criteria
met: 60/60 tests pass, clippy/fmt clean, QoS 1/2 rejection works,
two-runner localhost run at QoS 3+4 produced 100.00% delivery across
all 16 spawns with clean EOT events. One Windows-specific transient
io error (`os error 997` / `ERROR_IO_PENDING`) discovered and fixed
during validation by extending the `is_transient_io_error` classifier
alongside `WSAEWOULDBLOCK` / `WSAETIMEDOUT`.
**Depends on**: E1, E2, E9, E12. Folder is already scaffolded
(AGENTS.md, CUSTOM.md, STRUCT.md, .claude/CLAUDE.md present); all that
exists in `src/` is empty directories.

Implement the WebSocket variant per `variants/websocket/CUSTOM.md` and
`metak-shared/variant-candidates.md` R6.

#### Scope

1. Initialise the binary crate (`Cargo.toml`, `src/main.rs`).
   Dependencies: `variant-base` (path), `tungstenite` (sync, no
   `tokio-tungstenite`), `socket2`, `anyhow`, plus whatever
   `variant-base` re-exports (`clap`, etc.). No `tokio` dependency.
2. Implement `WebSocketVariant` (`src/websocket.rs`) per the trait,
   the symmetric pairing rules, and the port-derivation strides
   documented in CUSTOM.md.
3. Implement the binary header in `src/protocol.rs` matching the
   format used by `variants/hybrid` and `variants/custom-udp` (do
   not invent a new header).
4. Implement pairing / port derivation in `src/pairing.rs`.
5. CLI: parse `--ws-base-port`, `--peers`, `--runner`, `--qos`. If
   `--qos` is 1 or 2, log a clear error and exit non-zero before any
   I/O. All other CLI args are handled by `variant-base`.
6. Implement EOT (`signal_end_of_test`, `poll_peer_eots`) per
   `metak-shared/api-contracts/eot-protocol.md`. Use the same TCP-frame
   marker scheme as Hybrid; encode the EOT into a WebSocket binary
   frame body using the reserved header value defined by the contract.
7. Tests:
   - Unit: header serialization round-trip.
   - Unit: pairing / port derivation across a few `--peers` shapes.
   - Unit: `publish` at QoS 1 or 2 returns an error.
   - Integration (single-process, `--peers self=127.0.0.1`): bind +
     listen + framing exercised; full peer flow validated by T3f.4.
8. Add a sample TOML config under `variants/websocket/tests/fixtures/`
   for single-process loopback (analogous to the other variants'
   fixtures).
9. Add a project-level config `configs/two-runner-websocket-all.toml`
   that exercises QoS 3 and 4 across two runners on localhost,
   modelled on `configs/two-runner-hybrid-all.toml`.
10. Run the two-runner localhost test and validate delivery is
    ≥ 99% over the operate window for both QoS 3 and QoS 4.

#### Acceptance criteria

- [ ] `Cargo.toml` lists only the dependencies in CUSTOM.md (no
      `tokio`, no `tokio-tungstenite`).
- [ ] `cargo build --release -p variant-websocket` succeeds on Windows.
- [ ] `cargo test --release -p variant-websocket` all-green.
- [ ] `cargo clippy --release -p variant-websocket --all-targets -- -D warnings` clean.
- [ ] `cargo fmt -p variant-websocket -- --check` clean.
- [ ] Variant exits non-zero with a clear stderr message if launched
      with `--qos 1` or `--qos 2`.
- [ ] EOT events (`eot_sent`, `eot_received`) appear in JSONL logs
      from both runners on the localhost two-runner run.
- [ ] Localhost two-runner run produces JSONL logs with delivery ≥ 99%
      at both QoS 3 and QoS 4.
- [ ] STRUCT.md remains accurate (update if file layout differs from
      the scaffold).
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`.

---

## E3g: WebRTC DataChannel Variant

See `EPICS.md` E3g and `metak-shared/variant-candidates.md` R7 for the
driving design. Split into two tasks because the build risk on the
`webrtc` crate is real and worth de-risking before sinking
implementation effort.

### T3g.1: variants/webrtc — crate scaffold + dependency build smoke test

**Repo**: `variants/webrtc/`
**Status**: done (2026-05-06; see STATUS.md). `webrtc = "0.8"` builds
clean on Windows in ~90 s, runtime smoke (construct + close
RTCPeerConnection) exits 0 in ~1 s. No version pinning or workarounds
needed — the pure-Rust `rustls 0.19` path that webrtc 0.8 uses dodges
the historical `openssl-sys` Windows trap.
**Depends on**: nothing (folder is already scaffolded with AGENTS.md,
CUSTOM.md, STRUCT.md, .claude/CLAUDE.md).

De-risk the `webrtc-rs` build before committing implementation effort.

#### Scope

1. Initialise the binary crate (`Cargo.toml`, minimal `src/main.rs`
   that prints a banner and exits 0).
2. Add the dependency on `webrtc` (latest stable), `tokio` with
   `rt-multi-thread`, `anyhow`, and `variant-base` (path).
3. `cargo build --release -p variant-webrtc` from the repo root on
   Windows. If it fails, do NOT spend
   hours debugging — capture the exact error, list the candidate
   workarounds you researched (pinning OpenSSL / ring versions,
   alternative crate version), and stop. Report findings via
   STATUS.md so the orchestrator can decide between fixing it,
   pinning, or reconsidering the variant.
4. If the build succeeds, also run a tiny tokio + webrtc smoke
   inside `src/main.rs` (e.g. construct an `RTCPeerConnection` with
   no peer, immediately close it) just to confirm the crate
   initialises at runtime, not only at link time.

#### Acceptance criteria

- [ ] `Cargo.toml` declares the listed dependencies.
- [ ] `cargo build --release -p variant-webrtc` succeeds on Windows
      (or, on failure, a STATUS.md entry documents the failure mode and
      the proposed remediation paths).
- [ ] If the smoke main is added, the binary runs to exit 0 in under
      10 s.
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`.

### T3g.2: variants/webrtc — implement WebRTC variant end-to-end

**Repo**: `variants/webrtc/`
**Status**: done (2026-05-06; see STATUS.md). All acceptance criteria
met: 40/40 tests pass (36 unit + 4 integration), clippy/fmt clean,
ICE host-only verified, two-runner localhost run at all four QoS
levels produced **100.00% delivery on every (writer, reader, QoS)
pair across 32 spawns**, including QoS 1 max-throughput at 1.1 M
messages. `eot_sent` / `eot_received` present on every log; zero
`eot_timeout`. Known limitation captured: one peer per spawn
(documented in CUSTOM.md and enforced with a clear error). See
follow-up T-config.1 below for a small config-path inconsistency
discovered during validation.
**Depends on**: T3g.1 (build proven on Windows), E1, E2, E9, E12.

Implement the WebRTC variant per `variants/webrtc/CUSTOM.md` and
`metak-shared/variant-candidates.md` R7.

#### Scope

1. Implement `WebRtcVariant` (`src/webrtc.rs`) per the trait, with the
   sync-to-async bridge documented in CUSTOM.md (mirror the QUIC
   variant's pattern).
2. Implement the per-pair TCP signaling exchange (`src/signaling.rs`)
   per the envelope format documented in CUSTOM.md.
3. Implement pairing / port derivation (`src/pairing.rs`).
4. Implement the binary header in `src/protocol.rs` matching the
   format used by `variants/hybrid` and `variants/custom-udp`.
5. Configure ICE for **host candidates only** — disable STUN, TURN,
   and mDNS providers in webrtc-rs.
6. Per peer: open four DataChannels (one per QoS) with the QoS-
   appropriate ordered/reliable settings as documented in CUSTOM.md.
7. CLI: parse `--signaling-base-port`, `--media-base-port`, `--peers`,
   `--runner`, `--qos`. All other CLI args are handled by
   `variant-base`.
8. Implement EOT (`signal_end_of_test`, `poll_peer_eots`) per
   `metak-shared/api-contracts/eot-protocol.md`. Always send the EOT
   marker on the **reliable** DataChannel (L3/L4) regardless of the
   spawn's `--qos`, otherwise unreliable EOTs could deadlock the wait.
9. Tests:
   - Unit: header serialization round-trip.
   - Unit: pairing / port derivation across a few `--peers` shapes.
   - Unit: signaling envelope encode / decode.
   - Integration (single-process, `--peers self=127.0.0.1`): exercises
     CLI parsing, port derivation, and the runtime startup path. Full
     peer flow is validated by step 11 below.
10. Add a sample TOML config under `variants/webrtc/tests/fixtures/`
    for single-process loopback.
11. Add a project-level config `configs/two-runner-webrtc-all.toml`
    that exercises all four QoS levels across two runners on
    localhost, modelled on `configs/two-runner-quic-all.toml`. Run it
    and validate:
    - Delivery ≥ 95% on QoS 3 and QoS 4 over the operate window.
    - QoS 1 and QoS 2 at moderate rates show low loss; record what you
      measure (no hard threshold — this is a baseline measurement).
    - EOT events appear and the spawn terminates without an
      `eot_timeout`.

#### Acceptance criteria

- [ ] `cargo test --release -p variant-webrtc` all-green.
- [ ] `cargo clippy --release -p variant-webrtc --all-targets -- -D warnings` clean.
- [ ] `cargo fmt -p variant-webrtc -- --check` clean.
- [ ] ICE produces only host candidates (verified via signaling logs
      at debug level — no `srflx`, no `relay`, no `mdns`).
- [ ] Localhost two-runner run produces JSONL logs with the four QoS
      levels separated by spawn name and delivery ≥ 95% on QoS 3-4.
- [ ] EOT events (`eot_sent`, `eot_received`) appear in JSONL logs
      from both runners with no `eot_timeout` events.
- [ ] STRUCT.md remains accurate (update if file layout differs from
      the scaffold).
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`.

---

## Cross-cutting follow-ups (discovered during T3f.1 / T3g.2)

### T-config.1: Standardise variant binary paths in configs

**Repo**: `configs/` (project-level), no source-code changes.
**Status**: done (see "Workspace target convention" sweep). Every
`configs/*.toml` now points at `target/release/variant-<name>.exe`,
matching the Cargo workspace layout. Per-subfolder builds are
abandoned; see CUSTOM.md files in `runner/`, `variant-base/`, and
`variants/<name>/` for the workspace-rooted build commands.
**Depends on**: nothing.

The repo is a Cargo workspace, so `cargo build --release --workspace`
(or `-p variant-X`) from the repo root puts every binary in
`target/release/variant-X.exe`. All `configs/*.toml` files reference
that single path; there is no `variants/<name>/target/` directory
in the convention any more.

The historical incident behind this task: per-subfolder builds
created stray `variants/<name>/target/` and `runner/target/` trees,
which the configs then pointed into. This regularly caused stale-
binary skew on a secondary machine where some sub-crates had been
rebuilt and others had not — manifesting as silent loss of features
that had been added in the most recent commit (e.g. clock sync, EOT
markers).

#### Acceptance criteria

- [x] All `configs/*.toml` files use `target/release/variant-<name>.exe`.
- [x] Clean `cargo build --release --workspace` from repo root +
      `runner --config configs/two-runner-<any>-all.toml` succeeds
      without manual binary copies.
- [x] `usage-guide.md`, `README.md`, and every `CUSTOM.md` updated to
      teach workspace-rooted builds only.

### T-windows.1: Back-port `os error 997` classifier to hybrid

**Repo**: `variants/hybrid/`
**Status**: pending
**Depends on**: nothing.
**Priority**: low (latent — hybrid's workloads run hot enough that
the read-after-deadline path rarely fires; no observed failure yet).

Discovered during T3f.1 validation. The websocket variant added an
`is_transient_io_error` helper that classifies Windows `os error 997`
(`ERROR_IO_PENDING`) alongside `WSAEWOULDBLOCK` (10035) and
`WSAETIMEDOUT` (10060) as transient retries. See
`metak-shared/LEARNED.md` for full context.

The hybrid variant uses the same SO_RCVTIMEO + cloned read handle
pattern but lacks the 997 case. It has the same latent bug; on a
slow-enough hybrid workload (e.g. `--values-per-tick 1` at 10 Hz),
the read loop could mis-classify a timed-out read as a hard failure
and bail.

#### Scope

1. Copy the `is_transient_io_error` helper from
   `variants/websocket/src/websocket.rs` to the matching site in
   `variants/hybrid/src/tcp.rs` (or wherever the read poll lives).
2. Replace any direct `ErrorKind::WouldBlock | ErrorKind::TimedOut`
   match in the hybrid TCP read loop with the helper.
3. Add a unit test that constructs an `io::Error` from
   `Error::from_raw_os_error(997)` and verifies the classifier
   returns `true`.
4. Run the existing hybrid integration tests to confirm no regression.

#### Acceptance criteria

- [ ] Hybrid TCP read loop uses the same transient-error classifier
      as websocket.
- [ ] Unit test covers the 997 case explicitly.
- [ ] `cargo test --release -p variant-hybrid` clean.
- [ ] `cargo clippy --release -p variant-hybrid --all-targets -- -D warnings` clean.

### T-config.2: Variant templates + array expansion for tick_rate_hz / values_per_tick

**Repo**: `runner/` (parser + spawn-job expansion) and `configs/`
(refactor existing all-variants config + add new 10-peer config).
**Status**: pending
**Depends on**: contract update landed (`metak-shared/api-contracts/toml-config-schema.md`
and `variant-cli.md` updated). Touches the same code path as E9's QoS
expansion (`runner/src/spawn_job.rs`).

Driving need: existing configs duplicate ~12 lines per variant case across
many `[[variant]]` entries that only differ in `tick_rate_hz` and
`values_per_tick`. The next phase of testing is a 4-machine, 10-peer setup
(Windows PC × 3 peers, Windows PC × 4 peers, Raspberry Pi × 1 peer, old
Mac × 2 peers) where this duplication will only get worse. Add two
mechanisms — variant templates and Cartesian array expansion — that keep
backwards compatibility with every existing config while allowing radical
size reduction in new configs.

#### Scope

1. **Parser changes** (`runner/src/config.rs`):
   - Add `[[variant_template]]` top-level array. Same fields as
     `[[variant]]` but `name` is just a template identifier (not a spawn
     name). Templates do not spawn.
   - Add `template: Option<String>` field to `VariantConfig`.
   - Resolution pass after parse / before validation:
     - Validate template names are unique.
     - For every `[[variant]]` with `template = "X"`: look up `X`,
       deep-merge `[variant_template.common]` and
       `[variant_template.specific]` into the variant entry's matching
       sections (variant entry's keys win), and fall through to the
       template's `binary` / `timeout_secs` if the variant entry omits
       them.
     - After resolution every `[[variant]]` must have a non-empty
       `binary`. Validation runs on the resolved values.
   - Internal: keep `BenchConfig` shape stable for downstream code. Either
     mutate `variant` in place after merging, or expose a resolved view —
     pick whichever is cleaner; the spawn-job loop should see fully
     resolved entries.

2. **Tick-rate and VPT array support** (`runner/src/config.rs`):
   - Mirror the existing `qos_spec()` / `QosSpec` pattern. Add `tick_rate_spec()`
     and `values_per_tick_spec()` returning ascending-deduped Vec<u32>
     (or u64 — match existing types). Accept integer or array; require
     positive values; non-empty arrays.
   - Validation rejects non-positive values, empty arrays, non-integer
     elements, and out-of-range u32 values.

3. **Spawn-job expansion** (`runner/src/spawn_job.rs`):
   - Replace the qos-only loop with a triple-nested expansion in this
     stable order: `tick_rate_hz` (outer) → `values_per_tick` (middle) →
     `qos` (inner). Output is the Cartesian product, in that order, so
     spawn ordering is deterministic and grouped naturally for human reading.
   - Add `tick_rate_hz: u32` and `values_per_tick: u32` (or matching
     existing types) to `SpawnJob`.
   - Auto-naming per the contract:
     - Base = post-resolution `variant.name`.
     - Append `-<vpt>x<hz>hz` if `tick_rate_hz` OR `values_per_tick`
       expanded (i.e. > 1 effective value). Both numbers always shown
       even when only one dimension expanded.
     - Append `-qos<N>` if `qos` expanded (existing behaviour).
   - Existing helper-naming tests must still pass; add new tests for the
     three-dimensional expansion.

4. **CLI arg construction** (`runner/src/cli_args.rs`):
   - The runner currently passes `--tick-rate-hz` and `--values-per-tick`
     verbatim from `[variant.common]`. Now they may be arrays. The
     spawn-job carries the concrete scalar — at CLI-construction time
     emit the per-spawn scalar from `SpawnJob`, NOT the array from
     `[variant.common]`. Same pattern as `--qos`.

5. **Inter-spawn grace period** (`runner/src/main.rs` or wherever the
   loop lives): the existing `inter_qos_grace_ms` grace currently
   inserts between consecutive QoS spawns from one source entry. Apply
   it between every consecutive pair of spawns derived from the same
   source entry (i.e. across all dimensions, not just QoS pairs).
   Variants that bind ports may collide otherwise.

6. **Tests** (in `runner/`):
   - Unit: template resolution merges common / specific tables correctly,
     variant key wins, missing keys come from template, top-level scalars
     fall through, missing template name is a validation error,
     duplicate template names is a validation error.
   - Unit: `tick_rate_spec` and `values_per_tick_spec` accept scalar,
     accept array, reject empty array, reject non-positive integers,
     reject non-integer elements, dedup + sort.
   - Unit: `expand_variant` produces the right Cartesian product in the
     documented stable order, with the right auto-name suffixes for
     scalar-scalar, array-scalar (vpt), scalar-array (hz), and array-array
     cases, combined with single-qos vs multi-qos.
   - Unit: single-element array on hz / vpt counts as scalar (no suffix).
   - Integration: end-to-end spawn loop using a small synthetic config
     that exercises both templates and array expansion, verifying every
     expected spawn name appears in the runner's logged spawn-job list.
     A live `variant-dummy` run is fine — the existing integration test
     scaffolding can be reused.

7. **Config refactor**: rewrite `configs/two-runner-all-variants.toml` to
   use templates + array expansion. The expanded set of spawns must
   exactly match what the current config produces, in the same order.
   Recommended structure:
   - One `[[variant_template]]` per variant binary capturing the shared
     common + specific fields (binary, multicast/port settings, workload
     defaults, stabilize/operate/silent durations, log_dir).
   - One `[[variant]]` per (workload, vpt-group) cluster, e.g. for
     custom-udp:
     - `vpt = 1000, hz = [10, 100]` → 2 spawns.
     - `vpt = 100, hz = [10, 100, 1000]` → 3 spawns.
     - `vpt = 10, hz = [100, 1000]` → 2 spawns.
     - `vpt = 1000, hz = 100, workload = "max-throughput"` → 1 spawn.
     Total 8 — same as today. With qos omitted (current default for the
     all-variants config) each multiplies by 4. Verify the resulting
     spawn-job count matches the original config's count. Keep the
     header comment block, update it to describe the new template form.

8. **New 10-peer config**: `configs/multi-machine-10peer-all.toml` (name
   subject to revision if a clearer convention exists). Targets the
   4-machine layout described above. Use peer names that identify their
   host machine:

   ```toml
   runners = [
     "winA-1", "winA-2", "winA-3",       # Windows PC A
     "winB-1", "winB-2", "winB-3", "winB-4",  # Windows PC B
     "rpi-1",                             # Raspberry Pi
     "mac-1", "mac-2",                    # Old Mac
   ]
   ```

   Pick conservative durations (e.g. `stabilize_secs = 5, operate_secs = 30,
   silent_secs = 5`) and a reasonable `default_timeout_secs` (180 — leaves
   headroom for slow startup on the Pi and Mac). Use the templated form
   throughout. Cover all 5 working variants (custom-udp, hybrid, quic,
   zenoh, websocket; webrtc supports only one peer per spawn per E3g and
   should be excluded — note this in the header comment) at a moderate
   load profile (e.g. `tick_rate_hz = [10, 100]`, `values_per_tick = [10, 100]`,
   qos omitted) plus per-variant `max-throughput` cases.

   Header comment must describe:
   - The 10-peer layout (names → machines).
   - Why webrtc is excluded.
   - Operator instructions: launch `runner --name <peer-name> --config configs/multi-machine-10peer-all.toml`
     on the appropriate machine for each name in `runners`. Multiple
     peers per machine = multiple `runner` processes on that machine,
     each with a different `--name`.
   - Reminder that ports are derived per (runner_index, qos) per the
     port-stride convention; with 10 runners and qos_stride = 10 the
     reserved port range per variant is `base_port .. base_port + 40`.

#### Validation against reality

- `cargo build --release --workspace` from repo root — all crates
  compile clean.
- `cargo test --release -p runner` — full runner test suite green
  including the new tests.
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.
- Build `variant-base` (`cargo build --release -p variant-base`) so
  `variant-dummy` is available; then run a SINGLE-RUNNER smoke against
  the refactored `configs/two-runner-all-variants.toml` (or a small
  subset of it) using `variant-dummy` swapped in for one variant entry,
  just to confirm the expansion produces the expected spawn names. Show
  the runner stdout's spawn-job list in the completion report.
- For the new 10-peer config: structural-only validation. The user owns
  cross-machine execution. Run `cargo run --release -p runner -- --name winA-1 --config configs/multi-machine-10peer-all.toml --validate-only`
  IF such a flag exists; otherwise just parse the config in a unit test
  fixture or note in the completion report that live validation is
  pending the multi-machine setup.
- All other existing configs must still parse cleanly without
  modification (`for f in configs/*.toml; do cargo run -p runner -- --name x --config "$f" --validate-only; done` if the flag exists, else write a quick unit test or rely on the existing per-config test scaffolding).

#### Acceptance criteria

- [ ] `[[variant_template]]` and `template = "..."` parse and resolve
      per the contract.
- [ ] `tick_rate_hz` and `values_per_tick` accept arrays and expand
      Cartesian-style with `qos`.
- [ ] Spawn auto-naming follows `<name>[-<vpt>x<hz>hz][-qos<N>]`.
- [ ] Sequential spawn execution order: hz outer, vpt middle, qos
      inner (all ascending).
- [ ] `inter_qos_grace_ms` applies between every consecutive pair of
      spawns derived from one source entry, not only QoS pairs.
- [ ] Single-element arrays count as scalar (no suffix).
- [ ] All new unit tests land in `runner/`.
- [ ] `configs/two-runner-all-variants.toml` rewritten to the
      template + array form; expanded spawn count + names match the
      pre-rewrite config exactly (worker should produce a side-by-side
      list in the completion report).
- [ ] `configs/multi-machine-10peer-all.toml` added with the runner
      naming convention above and a clear header comment.
- [ ] Every existing `configs/*.toml` still parses without modification.
- [ ] `cargo test`, `cargo clippy`, `cargo fmt --check` clean for
      `runner/`.
- [ ] Completion report appended to `metak-orchestrator/STATUS.md` with
      spawn-list comparison + new-config layout summary.

#### Out of scope

- Variant code changes — variants still receive scalar `--tick-rate-hz`,
  `--values-per-tick`, `--qos`. The new mechanisms are entirely runner-
  side.
- Refactoring the smaller per-variant `configs/two-runner-<v>-all.toml`
  files. They already use single-qos arrays where it matters; extending
  the template form to them is a follow-up if/when worth it.
- Cross-machine validation of the new 10-peer config. User owns the
  multi-machine execution.
- Re-enabling webrtc for N>2 peers. Flag the limitation in the header
  comment of the 10-peer config and leave it.

---

### T-resume.1: runner — `--resume` flag and ResumeManifest coordination

**Repo**: `runner/`
**Status**: done (commits `16476d3 Add resume` + `6d9a53e Fix bugs leading
to inaccurate results`, 2026-05-07). Multi-machine live validation owned
by user.
**Depends on**: contract update in
`metak-shared/api-contracts/runner-coordination.md` (landed; Phase 1
discover-message extension, Phase 1.25 ResumeManifest, Phase 2 skip rule,
clock-sync append behavior).

User goal: pick up an interrupted multi-machine benchmark without
redoing completed spawns. Runs that crashed mid-spawn produce empty or
partial log files — those must be cleanly re-run, not silently kept.

#### Scope

1. **CLI flag** in [runner/src/main.rs](runner/src/main.rs):
   - Add `#[arg(long, default_value_t = false)] resume: bool` to `Cli`.
   - Surface it through to discovery and to the new resume-inventory
     phase.

2. **Discover message extension**
   ([runner/src/message.rs](runner/src/message.rs)):
   - Add `resume: bool` to `Message::Discover` (placed alongside the
     existing `log_subdir`).
   - Add a new `Message::ResumeManifest { name: String, run: String,
     complete_jobs: Vec<String> }` variant (snake_case JSON tag).
   - Update existing tests to include `resume: false` in the
     pre-existing fixtures; add roundtrip + JSON-format tests for
     `ResumeManifest`. Mirror the structure of `discover_*` tests.

3. **Log subfolder selection**
   ([runner/src/main.rs](runner/src/main.rs:104-105) and `protocol.rs`):
   - Resolve `base_log_dir` the same way main.rs already does
     (lines 132-138). Move that resolution earlier so the resume
     branch can use it.
   - Fresh mode: keep current behavior
     (`<bench_config.run>-<now_ts>`).
   - Resume mode: enumerate `base_log_dir` for entries whose name
     starts with `<bench_config.run>-`; pick the lexicographically
     greatest one (the timestamp suffix sorts correctly). Abort with
     a clear error if none exists.
   - Pass the proposal into `Coordinator::new` exactly as today.

4. **Discovery: resume-flag agreement and folder verification**
   ([runner/src/protocol.rs](runner/src/protocol.rs)):
   - The coordinator already negotiates `log_subdir` via the leader's
     proposal. Extend the discover handler to ALSO verify peers'
     `resume` flag matches this runner's; if any peer disagrees,
     abort with a clear error message naming the disagreeing peer.
   - After the agreed `log_subdir` is known, in resume mode, verify
     that `<base_log_dir>/<agreed_log_subdir>/` exists locally. If
     not (i.e. follower's latest folder name differs from the leader's
     pick), abort with a clear error.

5. **Phase 1.25 — ResumeManifest exchange** (new module
   `runner/src/resume.rs` is fine; or extend `protocol.rs`):
   - Computes the local manifest BEFORE broadcasting:
     - Expand the config into spawn jobs the same way as Phase 2
       (use [runner/src/spawn_job.rs](runner/src/spawn_job.rs)
       `expand_variant`).
     - For each `effective_name`, check
       `<run_log_dir>/<effective_name>-<self_name>-<run>.jsonl`:
       - non-empty → include in `complete_jobs`
       - empty → DELETE the file and exclude
       - missing → exclude
   - Broadcasts the `ResumeManifest` and listens for one from each
     peer in `runners` (excluding self). Re-broadcasts every 500 ms
     for loss recovery. Reuse the existing UDP coordination socket and
     the existing `recv_with_timeout`-style loop pattern from
     discovery / barriers.
   - Validates received manifests: `run` must equal this runner's
     run id; ignore manifests with a wrong run id (defensive — they
     should not exist after discovery agreement, but be safe).
   - Computes the intersection: a job is "skip" iff it appears in
     every runner's `complete_jobs` (including this runner's own).
   - Cleanup: for each spawn job NOT in the skip set, delete this
     runner's
     `<run_log_dir>/<effective_name>-<self_name>-<run>.jsonl` if
     present (regardless of size).
   - Returns the skip set (`HashSet<String>` of effective names) to
     the main loop.
   - Single-runner mode: skip the network exchange (intersection is
     trivially the local manifest). Empty-file cleanup still applies.

6. **Phase 1.5 / per-variant clock-sync — append mode**
   ([runner/src/clock_sync_log.rs](runner/src/clock_sync_log.rs) or
   wherever `open_clock_sync_log` lives):
   - Resume mode passes a flag (or always opens with
     `OpenOptions::new().create(true).append(true)`). Verify the
     existing implementation; if it already appends, no change needed
     beyond confirming it. If it truncates, fix it to append in
     resume mode and unit-test that prior content is preserved.
   - The fail-fast `require_initial_sync_complete` check in
     [runner/src/main.rs:263](runner/src/main.rs#L263) still applies
     in resume mode: cross-machine resumes still need correct
     offsets.

7. **Phase 2 skip integration**
   ([runner/src/main.rs](runner/src/main.rs:293-413)):
   - Thread the skip set into the variant loop. For each `job`,
     if `skip_set.contains(&job.effective_name)`, log
     `[runner:<name>] skipping '<effective_name>' (resume: complete on all peers)`
     and `continue` — no ready barrier, no spawn, no resync, no
     done barrier. Add a row to the summary with status `"skipped"`
     and exit_code `0`.
   - Make sure `print_summary` and the final exit-code logic treat
     `"skipped"` as success (not as a failure). The end-of-run
     summary should distinguish `success` and `skipped` rows so the
     operator can see what was reused.
   - Inter-spawn grace logic
     ([runner/src/main.rs:408-411](runner/src/main.rs#L408-L411))
     should still apply between two consecutive non-skipped jobs of
     the same source entry, but skipped jobs should not consume a
     grace period (they're effectively instant).

8. **Final summary clarity**: after the run, print a one-line
   summary like
   `Resume: 7 spawns reused, 4 spawns executed, 0 failed.` (only when
   `--resume` was set). Keep the existing per-row table.

#### Tests (in `runner/`)

- `message.rs` unit tests:
  - `ResumeManifest` roundtrip + JSON-format test (mirror existing
    `discover_*` tests).
  - `Discover` JSON now includes `resume: false` by default in
    fixtures.

- `resume.rs` (or wherever the inventory logic lives) unit tests
  using a temp dir:
  - Empty file gets deleted and excluded from complete_jobs.
  - Non-empty file gets included.
  - Missing file is excluded without error.
  - Intersection rule with a fixture of three peers' manifests:
    correctly picks the all-three-agree subset.
  - Single-runner intersection equals the local manifest.
  - Cleanup: incomplete-set files deleted; complete-set files
    preserved.

- Latest-folder picker unit test using a temp dir with mixed
  prefixes:
  - Multiple `run01-YYYYMMDD_HHMMSS` folders → greatest selected.
  - Wrong-prefix folders ignored.
  - No matching folder → returns an error (so main.rs aborts).

- Integration test (extend
  `runner/tests/integration.rs`): single-runner `--resume`
  end-to-end with `variant-dummy`. Two runs:
  1. First run, 2 spawns, both complete with non-empty JSONL.
  2. Second run with `--resume`: both spawns must be skipped, exit
     code 0, summary reports both as `skipped`.
  3. Variant: same setup but truncate one of the JSONL files to
     zero bytes between runs; the second run must DELETE that file
     and re-execute that one spawn (exit code 0, mixed summary).

- Multi-runner integration test if the harness supports it (the
  existing two-runner same-machine test in `tests/integration.rs`
  is the model). If too costly, document why and rely on the
  unit-tested intersection logic + a manual two-runner check by
  the user.

#### Validation against reality

- `cargo build --release -p runner` (workspace-rooted, per
  CUSTOM.md) and `cargo build --release -p variant-base`.
- `cargo test --release -p runner` — full runner test suite green
  including new tests.
- `cargo clippy --release -p runner --all-targets -- -D warnings`
  clean.
- `cargo fmt -p runner -- --check` clean.
- Live single-runner smoke against an existing log dir:
  1. Run `runner --name alice --config configs/two-runner-test.toml`
     once and let it finish. Note the produced log subfolder.
  2. Re-run with `--resume`. Confirm all spawns are reported
     `skipped` and exit code is 0.
  3. Truncate one JSONL file to 0 bytes. Re-run with `--resume`.
     Confirm that one spawn re-runs and the empty file is deleted
     and replaced.
  4. Show the runner stdout for each step in the completion report.
- A multi-runner same-machine resume run is the user's
  responsibility. The worker should describe what to test, but
  not block on running it.

#### Acceptance criteria

- [ ] `runner --resume` parses cleanly; default off.
- [ ] Discover message carries `resume: bool` and `log_subdir`;
      mismatch on either is a clear-error abort.
- [ ] Latest-folder picker selects the lexicographically greatest
      `<run>-*` subfolder; no match → abort.
- [ ] Phase 1.25 `ResumeManifest` exchange: every runner sends and
      collects from every peer; intersection rule used to compute the
      skip set.
- [ ] Empty JSONL log files are deleted in Phase 1.25 (regardless
      of intersection).
- [ ] Incomplete-set files are deleted before Phase 2 starts.
- [ ] Phase 2 bypasses ready, spawn, per-variant resync, and done
      barriers for skipped jobs. Skipped rows appear in the summary
      with status `skipped` and exit_code `0`, treated as success
      by the final exit-code logic.
- [ ] Clock-sync log file is opened in append mode in resume mode
      (existing measurements preserved); initial sync still
      enforced fail-fast.
- [ ] `cargo test --release -p runner`, clippy, fmt all green.
- [ ] Live single-runner smoke (3 steps above) demonstrated in the
      completion report.
- [ ] Completion report appended to
      `metak-orchestrator/STATUS.md` summarizing implementation,
      tests run, and any deviations from this brief.

#### Out of scope

- Variant-side changes. Variants are unaware of resume; they
  continue to receive identical CLI args.
- Cross-machine multi-runner live execution. The user owns that
  validation; the worker should leave clear instructions.
- A `--resume <log_subdir>` form that lets the operator name a
  specific folder. Latest-only is the agreed scope; flag if the
  user later wants explicit selection.
- Resuming a run with a config different from the original (e.g.
  newly added variants). The intersection naturally handles this:
  new jobs simply don't appear in any old runner's manifest, so
  they fall outside the skip set and run normally. No special
  handling required, but call this out in the completion report.

### T-fairness.1: variant-base — bound the receive-drain in the driver loop

**Repo**: `variant-base/`
**Status**: pending
**Priority**: P0 — gates re-running same-machine-all-variants. All
current "max-throughput" benchmark numbers are contaminated by this
bug.

#### Problem

In `variant-base/src/driver.rs:61-96`, each operate-phase iteration
publishes a tick's worth of writes, then runs an unbounded
`while let Some(update) = variant.poll_receive()? { ... }`. When a
peer publishes faster than the local variant can drain, that inner
`while` never exits and the writer is starved. Confirmed symptoms in
`logs/same-mchine-all-variants-01-20260506_223254/`:

- `hybrid-max-qos4-alice` writes seq 1-1000 in 19 ms then logs only
  receives for 60+ s. Bob writes 429,000 normally.
- `quic-max-qos2-alice` writes 1000 then receives 6,861,000.
- `custom-udp` qos1-3 are visibly slower than qos4 because qos1-3's
  `recv_udp` exhaustively drains the socket per call while qos4's
  `recv_tcp` reads one frame per stream.

#### Scope

Bound the inner receive-drain by **two** independent budgets, whichever
trips first:

1. **Message-count budget**: drain at most `N` messages per outer
   iteration. Default `N = 2 * values_per_tick` (so a fair drain still
   keeps up with a peer that writes at our rate). Plumb the value
   from the workload profile if accessible; else hardcode 2000 with a
   `// TODO` to plumb it.
2. **Wallclock budget**: drain for at most `D` (default `1ms` —
   small enough to not let receive starve publish, large enough to
   avoid syscall thrash). Use `tokio::time::Instant::now()` checks
   inside the drain loop.

After either budget trips, **break out and continue to the next
publish tick**, even if `poll_receive` would still return `Some`. The
remaining queued messages stay in the variant's internal buffer and
are drained on subsequent iterations.

Add the same two-budget pattern to the EOT-phase wait loop if it has
the same shape — but DO NOT change EOT semantics (it's allowed to
spend longer there).

#### Acceptance criteria

- [ ] Driver code changed to bound receive-drain by both message
      count and wallclock per outer iteration.
- [ ] Existing `variant-base` tests still pass:
      `cargo test --release -p variant-base`
- [ ] New unit test that simulates a stub variant whose
      `poll_receive` always returns `Some`: confirms the operate
      loop still calls `publish` at least once per `D` wallclock
      budget.
- [ ] Live smoke: rebuild the workspace and run
      `target/release/runner --config configs/two-runner-all-variants.toml`
      with two runners on the same machine for at least the
      `hybrid-max-qos4` and `quic-max-qos2` rows. Verify in the
      resulting JSONL logs that BOTH alice and bob have substantially
      more than 1000 writes (target: at least 100k each over the
      operate window). Attach the relevant log filenames and
      head/tail timestamps to the completion report.
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`.

#### Out of scope

- Changing variant-internal threading models (each variant can stay
  single- or multi-threaded as it currently is).
- Changing the driver's overall phase structure.
- Re-running the full benchmark suite (the operator owns that).

### T-zenoh.1: variants/zenoh — eliminate first-tick declare storm + tune runtime

**Repo**: `variants/zenoh/`
**Status**: pending
**Priority**: P1 — independent of T-fairness.1, can run in parallel.

#### Problem

`zenoh-1000x100hz-qos1-alice` writes 8,361 messages in ~80 ms
(≈100k/s instantaneous) then **hangs** for the rest of the operate
phase. The chart's apparent "8k/s sustained" is `8361 ÷ 30s
operate-phase`. Hang shape and count match
`PUBLISH_CHANNEL_CAPACITY = 8192` at
`variants/zenoh/src/zenoh.rs:183`. The publisher task is stuck in
~1000 first-tick `declare_publisher().await` calls
(`zenoh.rs:407, 428`) on a 2-worker tokio runtime
(`zenoh.rs:676-680`) shared with subscribers and EOT.

`metak-shared/LEARNED.md:62-66` already records a same-host zenoh
hang. This is the same class of bug.

#### Scope

1. **Pre-declare publishers during `connect`/stabilize** rather than
   lazily on first `publish`. The workload's path set is known up
   front (`variant-base` workload profile exposes `paths`); declare
   one `Publisher` per path before the operate phase starts. Cache
   them in a `HashMap<String, Publisher>`. Lazy fallback for unknown
   paths is fine, but the `1000x100hz` and `100x1000hz` profiles
   must hit zero declares during operate.
2. **Bump tokio worker_threads** at `zenoh.rs:676-680` from 2 to
   `num_cpus::get().max(4)`. Add `num_cpus` if not already present.
3. **Reuse encode buffer**: replace the per-call
   `MessageCodec::encode -> Vec<u8>` allocation
   (`zenoh.rs:756, 776-794`) with a thread-local or per-task
   reusable `Vec<u8>` cleared at start of each encode. Confirm the
   zenoh API accepts `&[u8]`/`Bytes` such that the buffer can be
   handed off without forcing a `Vec` move per call.
4. **Right-size `PUBLISH_CHANNEL_CAPACITY`** to a smaller value
   (256-1024) once 1-3 are in. Goal: back-pressure shows up at the
   writer instead of being absorbed into a deep queue that inflates
   p95 latency.

#### Acceptance criteria

- [ ] All four scope items implemented OR explicitly justified as
      not needed (with evidence in the completion report).
- [ ] `cargo test --release -p variant-zenoh` (or the variant's test
      target) still passes.
- [ ] Live smoke: with T-fairness.1 NOT YET LANDED is acceptable —
      run `zenoh-1000x100hz-qos1` and `zenoh-max-qos1` two-runner
      same-machine. Verify alice writes substantially more than
      8,300 messages over the operate phase (target: at least 200k
      and continuing to write at the end of the operate window, not
      bunched in the first 80 ms).
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`
      with before/after write-count numbers, and a note on whether
      sustained throughput now scales with the workload's nominal
      rate.

#### Out of scope

- Switching zenoh from peer to client mode.
- Replacing the bridge architecture (D7) with direct synchronous
  publish.
- Cross-machine validation (operator owns that).

### T-analysis.1: analysis — handle clock_sync_sample debug shards

**Repo**: `analysis/`
**Status**: pending
**Priority**: P2 — cosmetic chart fix; independent of the other two.

#### Problem

`analysis/cache.py:566-579 _is_clocksync_shard` only matches
`event == "clock_sync"`, but the debug clock-sync shards
(`alice/bob-clock-sync-debug-all-variants-01.jsonl`) emit
`event == "clock_sync_sample"`. Their first row has `variant == ""`,
so cache discovery registers a bogus `("", "all-variants-01")`
group. `plots._split_variant_name("")` returns
`("other", "", None)`, producing the spurious `n/a` 5th row in the
comparison chart with the "other" transport family.

#### Scope

1. Extend `_is_clocksync_shard` to recognise BOTH `clock_sync` and
   `clock_sync_sample` events as clock-sync-only shards.
2. As a defence-in-depth fallback, also skip any shard whose first
   row has `variant == ""` from group discovery (treat as
   broadcast-only). Document in code why both checks exist.
3. Re-run the analysis cache build against
   `logs/same-mchine-all-variants-01-20260506_223254/` and confirm
   the regenerated `comparison.png` no longer has an `n/a` row.

#### Acceptance criteria

- [ ] `_is_clocksync_shard` matches both event names.
- [ ] Empty-variant fallback added with an explanatory comment.
- [ ] Existing tests still pass: `pytest -q` in `analysis/`.
- [ ] New unit test for `_is_clocksync_shard` covering both event
      values and the empty-variant case.
- [ ] Regenerated `comparison.png` for the affected logs directory
      attached to the completion report (path only, do NOT commit
      the PNG).
- [ ] Completion report appended to `metak-orchestrator/STATUS.md`.

#### Out of scope

- Changing the JSONL log schema or the variant-side debug shard
  emission.
- Restructuring the cache.

### T-coord.1: runner — diagnose mid-run coordination hang between spawn N done and spawn N+1 ready

**Repo**: `runner/`
**Status**: pending — **investigation task**, not a direct fix
**Depends on**: nothing

#### Field report (2026-05-07)

User ran a full-matrix Hybrid benchmark on alice + bob over the LAN
(commits `6d9a53e` / `16476d3+dirty`). Both runners successfully
completed every spawn through `hybrid-1000x10hz-qos1..4`,
`hybrid-1000x100hz-qos1..4`, and `hybrid-100x1000hz-qos1..4` (each
side reported `status=success, exit_code=0`). Then both runners
stopped making progress with the following terminal state:

- **alice's last log line**: `[runner:alice] ready barrier for spawn 'hybrid-100x100hz-qos1' (hz=100, vpt=100, qos=1)`. Alice has finished spawn N's done barrier, completed inter-spawn grace, and is waiting at the ready barrier for spawn N+1.
- **bob's last log line**: `[runner:bob] 'hybrid-100x1000hz-qos4' finished: status=success, exit_code=0`. Bob's variant child exited cleanly but bob never emitted any further log line — no inter-spawn grace, no clock resync, no `ready barrier` line.

So the deadlock sits in the **transition between spawn N done and
spawn N+1 ready** on bob's side, while alice has already moved past
it. The state was static long enough that the user is confident it's
a real hang.

The user resolved the immediate situation by killing both runners
and restarting with `--resume`. This task is about diagnosing why
the hang happened.

#### Why this is suspicious

The runner protocol already had an analogous bug fixed during E2
post-delivery (see STATUS.md "Fix 1: Discovery protocol race"): the
fast runner stopped sending Discover messages after its own
discovery completed, so slow peers never received them. The fix was
a 2-second linger to keep broadcasting after completion. The same
class of bug is plausible at the done-barrier transition: alice
broadcasts her `Done` for spawn N, immediately moves on to
inter-spawn-grace + ready-barrier-N+1, and stops re-broadcasting
`Done` messages for spawn N. If bob never receives alice's `Done`
(UDP loss, ARP cache miss, brief NIC stall), bob will wait at the
done barrier forever, and alice's later `Ready` for spawn N+1
won't unstick it.

#### Scope

This task is **investigation, not fix.** Deliverable is a written
diagnosis plus a follow-up fix task filed in TASKS.md if a code
change is needed. T-coord.2 (timeout safety net) lands in parallel
regardless of this task's outcome.

1. **Read the done-barrier loop** in [runner/src/protocol.rs](runner/src/protocol.rs) and trace exactly when this runner stops broadcasting its own `Done` for spawn N. Is there a "linger" pattern equivalent to the discovery fix?
2. **Read the inter-spawn handoff** in [runner/src/main.rs](runner/src/main.rs). Confirm whether the runner continues to handle inbound `Done` messages for spawn N during the inter-spawn grace + clock-resync setup phases, or drops them silently.
3. **Evaluate hypotheses:**
   - **H1 — fast peer stops broadcasting:** alice sends `Done` for N once (or only briefly), then moves to grace / ready-N+1. Bob never receives it; nothing pulls bob out.
   - **H2 — message-type or variant-name filter mismatch:** bob's coordinator filters inbound by `variant` field. If alice's `Done` for N is stamped with the wrong `effective_name` (off-by-one on the QoS expansion, stale variable), bob silently discards it. Trace the `Message::Done` construction site and confirm the variant name matches what bob expects.
   - **H3 — receive-window race:** bob received alice's `Done` for N but bob's barrier loop had already exited (e.g. if its own `Done` broadcast crossed alice's in flight). Bob is now in some "post-N" state that doesn't drive forward to ready-N+1. Look for a state machine where exiting the done loop leaves the runner without a clear next-step trigger.
   - **H4 — Windows socket-state side effect:** bob's variant teardown logged `os error 997` ("Overlapped I/O operation is in progress") on the variant TCP socket. Coordination is UDP, so probably unrelated, but confirm the coordination socket isn't shared / inherited / affected by variant child socket cleanup.
4. **If reproducible from the existing logs alone**, capture and quote the exact sequence: alice's stdout from spawn N's done through to the `ready barrier` line (with timestamps), bob's stdout from spawn N's variant exit through to silence, and any wall-clock idle confirming a hang rather than a slow phase.
5. **If not reproducible from logs**, build a minimal targeted reproducer:
   - Smallest config that exercises QoS-expansion + per-variant clock-sync + back-to-back-spawn (likely a 2-3 spawn config is enough — the bug is at the transition, not at scale).
   - Run on the user's two-machine setup (or single-machine two-runner if the hang reproduces on loopback) with verbose coordination tracing. Add a `--verbose-coord` flag if one doesn't already exist; instrument both sides of every barrier (broadcast sent, message received, peer-name accepted/rejected, state transition).
6. **Write up the diagnosis** in `metak-orchestrator/DECISIONS.md` (new entry):
   - Which hypothesis matched (or "could not reproduce — see follow-up").
   - What's broken (file:line).
   - What the fix should look like (estimated lines / files).
   - Whether the fix should land as a follow-up to this task (T-coord.1b) or whether T-coord.2's timeout makes it acceptable to defer.

#### Validation against reality

- Reproducer (if written) actually triggers the hang on at least one configuration; document the success rate.
- Verbose-coord tracing (if added) is gated behind a flag and produces no output in the default path. Don't ship permanent stderr noise.

#### Acceptance criteria

- [ ] H1-H4 each evaluated with a clear verdict (confirmed / ruled out / inconclusive-with-reason).
- [ ] If a reproducer was written, it lives at a stable path (e.g. `runner/tests/fixtures/coord-hang-repro.toml`) with a one-liner README on how to run it.
- [ ] Diagnosis entry added to `metak-orchestrator/DECISIONS.md`.
- [ ] Follow-up fix task filed in TASKS.md (e.g. T-coord.1b) if a code change is warranted, OR a clear note in DECISIONS explaining why deferring to T-coord.2's safety net is acceptable.
- [ ] `metak-orchestrator/STATUS.md` updated.

#### Out of scope

- Writing the fix. That's a follow-up.
- Implementing the timeout safety net. That's T-coord.2; intentionally decoupled so the timeout lands regardless of root cause.
- Any change to the coordination message format that would require updating `runner-coordination.md`. If the diagnosis points there, flag it; the orchestrator will file a contract task.

### T-coord.2: runner — barrier timeouts + exit-on-timeout + auto-resume wrapper scripts

**Repo**: `runner/` plus new `scripts/` at repo root.
**Status**: pending
**Depends on**: nothing (intentionally decoupled from T-coord.1).

User goal: when any post-discovery coordination barrier exceeds a
generous wall-clock budget, the runner exits cleanly with a
recognisable non-zero code and a clear stderr line telling the
operator to restart with `--resume`. A small wrapper script
(PowerShell + bash) implements the auto-restart loop on top of that
exit code, so auto-restart is **opt-in via the wrapper, not implicit
in the runner**.

The current behavior — silent unbounded wait at the ready barrier or
done barrier — converted a transient lost-message into hours of
stuck terminals during the 2026-05-07 Hybrid full-matrix run (see
T-coord.1 field report). This task adds the safety net independently
of root-cause investigation.

Design rationale (orchestrator decision, 2026-05-07): the runner
itself does NOT self-exec or auto-loop. Reasons: (a) on Windows,
self-exec is fiddly; (b) auto-restart inside the process can mask
real bugs by silently retrying — a wrapper is easy to disable when
debugging; (c) "agreeing to restart" over a broken coordination
channel is unreliable, so each runner has to time out independently
anyway. The wrapper pattern keeps the runner's state machine simple
and gives operators a clear opt-out.

#### Scope

1. **Per-barrier timeout in the runner state machine.** Apply to:
   - Phase 1.25 ResumeManifest exchange.
   - Per-variant clock resync wait.
   - Phase 2 ready barrier (per variant).
   - Phase 2 done barrier (per variant).
   - **Not Phase 1 discovery.** Discovery has its own bounded retry pattern, and a stuck discovery indicates "wrong config / firewall / peer never started," for which auto-resume is the wrong recovery. Worker: confirm by reading the discovery loop and document the exclusion in `runner/CUSTOM.md`.

2. **CLI flag** in [runner/src/main.rs](runner/src/main.rs):
   - `--barrier-timeout-secs <integer>` (optional). Default: **120**, with the budget chosen to be comfortably larger than any expected per-barrier slowdown the user has observed; revisit only if the timeout falsely fires. Worker may pick a different default with justification in `runner/CUSTOM.md`.
   - The flag is the wall-clock cap on each barrier wait, not cumulative across phases.

3. **On timeout, exit cleanly:**
   - `eprintln!` exactly one line: `[runner:<name>] coordination barrier '<barrier>' timed out after <N>s; exiting (re-run with --resume to continue)`. Replace `<barrier>` with `ready/<effective_name>`, `done/<effective_name>`, `clock_resync/<effective_name>`, or `resume_manifest`.
   - **Exit code 75** (`EX_TEMPFAIL` from `<sysexits.h>` — canonical for "transient failure, retry"). Document the choice in `runner/CUSTOM.md` and `metak-shared/api-contracts/runner-coordination.md` so wrapper scripts and operators can rely on it. Exit 1 is reserved for real config / panic errors and must NOT be used here.
   - Before exiting: terminate any in-flight variant child this runner spawned (only relevant on the done barrier; reuse the existing kill path used by per-variant timeout). Don't leave zombie children.
   - Flush any open log writers (clock-sync log, summary line if started).

4. **No auto-restart in the runner.** Runner's responsibility ends at "clean exit with code 75."

5. **Wrapper scripts** at `scripts/`:
   - `scripts/runner-resume.ps1` — Windows PowerShell wrapper. Must work under Windows PowerShell 5.1 (the user's default). No PS 7-only syntax (no `??`, no `?:`, no `?.`).
   - `scripts/runner-resume.sh` — bash wrapper for Linux/macOS.
   - **Behavior of both:**
     - Take the same args as `runner` plus an optional `--max-restarts <N>` (default 5) and `--restart-backoff-secs <N>` (default 2). Both are wrapper-only flags — strip them before forwarding to runner.
     - First invocation: run `runner` with the user's args. Do NOT pre-append `--resume` on the first call (so a fresh run starts fresh).
     - On exit:
       - exit 0 → propagate exit 0 and stop.
       - exit 75 → log a clear line (`[wrapper] runner exited with code 75 (coordination barrier timeout); restart attempt N/M with --resume after Xs`), sleep the backoff, then re-invoke `runner` with the original args + `--resume` appended. Deduplicate if the user already passed `--resume`.
       - any other non-zero exit → propagate as-is and stop. The wrapper does NOT auto-restart on panic, config error, or variant-level failure.
       - Hitting `--max-restarts` → exit non-zero with a final log line including restart count.
   - Target <80 lines each.

6. **Update [usage-guide.md](usage-guide.md)** with one short section on auto-resume wrappers:
   - When you might want them (long multi-machine runs).
   - Exact wrapper invocation (one example for each OS).
   - The opt-in framing: bare `runner` exits cleanly on barrier timeout; the wrapper is what loops.
   - One sentence reminding that auto-restart loops can mask real bugs — disable the wrapper if a barrier timeout recurs at the same job twice in a row, and file a task instead.

7. **Update the contract** [metak-shared/api-contracts/runner-coordination.md](metak-shared/api-contracts/runner-coordination.md):
   - Add a "Barrier Timeout" subsection (single section is fine — apply uniformly to ready / done / clock-resync / resume-manifest) documenting the per-barrier timeout, the default value, the exit-75 contract, and the exclusion of Phase 1 discovery.

8. **Update [runner/CUSTOM.md](runner/CUSTOM.md)** with a short "Coordination barrier timeouts" subsection mirroring the contract update plus the implementation entry points.

#### Tests (in `runner/`)

- Unit: a barrier-wait wrapper returns the timeout error after exactly `N` ms when no message arrives (use a short test value, e.g. 50 ms, with a sham channel that never delivers).
- Unit: the timeout error translates to exit code 75 in the main-level helper (test via the helper directly, not via spawning a process).
- Integration: extend [runner/tests/integration.rs](runner/tests/integration.rs) with a configuration that drives a barrier wait with no peer responses. The runner should exit 75 within the timeout window (assert with a generous slack of e.g. 5s on a 1s configured timeout to avoid CI flakes). Confirm exit code is 75 specifically, not 0 or 1.
- Wrapper smoke test: a tiny stub `runner` binary (or shell script masquerading as one) that exits 75 on first call and 0 on second. Run the wrapper against it and verify it loops once with `--resume` appended on the retry. Bash-side test goes in `scripts/` (or wherever `cargo test` won't pick it up); document the manual PowerShell counterpart in `scripts/README.md` if a PS test harness isn't readily available.

#### Validation against reality

- `cargo build --release -p runner` clean (workspace-rooted).
- `cargo test --release -p runner` green including new tests.
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.
- Live smoke: kill bob mid-run on the user's two-machine setup; alice should exit 75 within `--barrier-timeout-secs` rather than hanging forever. Record the runtime in the completion report.
- Wrapper smoke: run `scripts/runner-resume.ps1` against a stub that exits 75 once and 0 once. Confirm `--resume` is appended on the retry. Paste the wrapper log lines in the completion report.

#### Acceptance criteria

- [ ] `--barrier-timeout-secs` flag added to runner CLI; default 120 (or worker-justified alternative documented in `runner/CUSTOM.md`).
- [ ] Ready, done, clock-resync, and resume-manifest barriers all honour the timeout.
- [ ] Discovery is intentionally NOT timed out by this flag (documented in `runner-coordination.md` and `runner/CUSTOM.md`).
- [ ] Timeout exit is code 75 with a single clear stderr line naming the barrier and effective_name.
- [ ] In-flight variant children are cleaned up on timeout exit (no zombies).
- [ ] `scripts/runner-resume.ps1` and `scripts/runner-resume.sh` land at the repo root, both implement the loop-on-75-with-resume semantics, both are <80 lines, both work under their target shells with no version-specific traps.
- [ ] `usage-guide.md` has a short auto-resume-wrapper section with one example per OS.
- [ ] `metak-shared/api-contracts/runner-coordination.md` updated.
- [ ] `runner/CUSTOM.md` updated.
- [ ] All existing runner tests still pass; new unit + integration tests for the timeout path pass.
- [ ] `metak-orchestrator/STATUS.md` updated.

#### Out of scope

- Self-exec / auto-restart inside the runner process. Wrapper-only.
- Tuning the default timeout based on observed slow runs. Pick a generous value, justify it, revisit only if it falsely fires.
- Any change to discovery's existing retry semantics.
- Investigating the root cause of the 2026-05-07 hang. That's T-coord.1, deliberately decoupled.

### T-coord.1b: runner — fix done-barrier hang by re-broadcasting Done from ready_barrier on demand

**Repo**: `runner/`
**Status**: pending — follow-up to T-coord.1's diagnosis (see
`metak-orchestrator/DECISIONS.md` D9).
**Depends on**: nothing (parallel with T-coord.2's safety net).

#### Background

T-coord.1's investigation confirmed H1: when a fast peer (alice)
completes spawn N's done_barrier and moves on to spawn N+1's
ready_barrier, alice silently drops any inbound `Done` for spawn N.
A slow peer (bob) that enters done_barrier-N after alice's 2-second
linger has expired will broadcast `Done` forever and never receive
alice's matching `Done`. See DECISIONS.md D9 for the full code-path
trace.

#### Scope

1. **Track the most-recent-completed (variant_name, run, status, exit_code)
   per runner.** Bob never asks for Done from anyone other than alice;
   the cache only needs the immediately preceding variant — a single
   `Option<(String, String, String, i32)>` field on `Coordinator`.
   Update it from the tail of `done_barrier` just before returning
   (after the linger).

2. **In `ready_barrier`, on inbound `Done` whose `(variant, run)`
   matches the most-recent-completed entry**, immediately re-broadcast
   our own `Done` for that same variant via `self.send(...)`. This
   gives bob's done-barrier loop a fresh response to lock onto. Do
   NOT update `seen` or otherwise affect the ready_barrier's progress
   towards spawn N+1.

3. **Apply the same rule in `exchange_resume_manifest` and the
   discovery linger** for completeness — these phases come after a
   completed previous run isn't really a concern, but the cache value
   is unchanged across them so the cost is one extra match arm.

4. **Unit test**: invert the assertion in
   `runner/src/protocol.rs::done_barrier_hang_repro_when_peer_already_advanced`
   so the test now requires bob's done_barrier to complete within the
   6-second window after alice has parked in ready_barrier(spawn_n_plus_1).
   Update the doc-comment to describe the locked-in fixed behaviour.

5. **Optional: add a second test** asserting that the most-recent-completed
   cache is NOT used to satisfy a request for an OLDER variant — bob
   asking for Done on `spawn_n_minus_1` (two spawns ago) should still
   hang. This locks in the bounded-cache semantics.

#### Validation

- `cargo build --release -p runner` clean.
- `cargo test --release -p runner` green, including the inverted
  reproducer test and any new tests.
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.
- Live smoke (optional, on the user's two-machine setup): re-run the
  Hybrid full-matrix or a synthetic config with deliberately skewed
  per-machine variants. Confirm no hang at the spawn N → N+1 boundary.

#### Acceptance criteria

- [ ] `Coordinator` carries a most-recent-completed cache populated by
      `done_barrier`.
- [ ] `ready_barrier` re-emits `Done` for cached entries on demand.
- [ ] Reproducer test inverted; passes.
- [ ] Existing `barrier_linger_prevents_slow_peer_hang` test still
      passes (the new behaviour is strictly additive).
- [ ] Contract `metak-shared/api-contracts/runner-coordination.md`
      updated to document the "ready barrier responds to stale done
      requests" rule (one short subsection under Phase 2).
- [ ] `metak-orchestrator/STATUS.md` updated.

#### Out of scope

- Replacing the linger pattern wholesale.
- Cross-variant cache (we only need the immediately preceding spawn).
- T-coord.2's barrier timeout / wrapper script work (filed separately).

### T-coord.3: runner — fix discovery panic when bob never receives leader's Discover

**Repo**: `runner/`
**Status**: pending — field report from 2026-05-07 17:00.
**Depends on**: nothing (parallel with T-coord.1b).

#### Background

User launched alice + bob locally with `configs/two-runner-all-variants.toml`.
Bob panicked during discovery with:

```
leader log_subdir should be known after discovery
```

This is the `.expect(...)` at the tail of `Coordinator::discover` in
`runner/src/protocol.rs:395`.

#### Diagnosis

Discovery's exit condition is `seen == self.expected && hosts_known`.
The `seen` set is populated by **any** message type (Discover, Ready,
Done, ResumeManifest) — see lines 295–353. Only `Discover` carries the
`log_subdir` field, however, and `leader_log_subdir` is set only when
the leader's `Discover` is received (lines 327–330).

Failure path:

1. Alice (leader, runners[0]) starts first. Broadcasts `Discover`,
   sees bob, hits the 2 s linger, returns.
2. Alice advances to `clock_sync` and the Phase 2 ready barrier; her
   barrier loop drops `Discover` messages (`_ => {}` arms at lines 470,
   589, 691).
3. Bob starts after alice has already exited her discovery linger.
4. Bob broadcasts `Discover`. Bob's first inbound message from alice
   is a `Ready` (or `Done`), not a `Discover`. Bob marks alice as
   seen, but `leader_log_subdir` stays `None`.
5. Bob's `seen == expected && hosts_known` becomes true. Bob enters
   the 2 s discovery linger. During the linger bob keeps broadcasting
   `Discover`, but alice (in a barrier) ignores them.
6. Bob's linger ends, hits `.expect("leader log_subdir should be known
   after discovery")` → **panic**.

This is the same bug class as T-coord.1 (the done-barrier hang) but
for `Discover` instead of `Done`: peers in a later phase silently drop
inbound messages from earlier phases, leaving the slow peer with no
way to obtain a piece of state it needs from the fast peer.

T-coord.2's barrier-timeout safety net does NOT cover discovery (by
design — see T-coord.2 scope item 1, the discovery exclusion). T-coord.1b
covers `Done` re-emission only. Neither addresses this gap.

#### Scope

1. **Add a `last_log_subdir` cache field** on `Coordinator` storing
   the agreed-upon log subfolder once discovery completes (every
   runner — leader writes its own proposal, non-leaders write the
   leader's proposal). Single-runner mode populates it from the
   constructor's `log_subdir` argument.

2. **Re-emit `Discover` on demand** from every coordination phase that
   runs after discovery — `ready_barrier`, `done_barrier`,
   `exchange_resume_manifest`. When one of these loops receives an
   inbound `Discover` from a peer in `expected`, broadcast our own
   `Discover` (with `log_subdir = cached log_subdir`, `resume = self.resume`,
   `config_hash = self.config_hash`) so the slow peer can populate
   its `leader_log_subdir`.
   - Mirrors the `maybe_reemit_stale_done` pattern T-coord.1b is
     introducing for `Done`. Suggested helper: `maybe_reemit_discover`,
     called from the `_ => {}` arm (matching `Some(Message::Discover { .. })`)
     in each barrier loop.
   - Errors swallowed (`let _ = self.send(...)`) — best-effort,
     cannot abort the active barrier.

3. **Remove the `.expect("leader log_subdir should be known after
   discovery")` panic** at `protocol.rs:395`. Replace with a graceful
   fallback: if `leader_log_subdir` is still `None` after the linger,
   either (a) keep looping until it arrives (but with an internal
   bounded retry of, say, 30 s before bailing with a clear `bail!`)
   or (b) extend non-Discover message handling so the leader's
   `log_subdir` gets carried on `Ready`/`Done`/`ResumeManifest` as a
   fallback (more invasive — needs schema bump).
   - Pick (a) unless there's a strong reason against. Keep the loop
     bounded with a reasonable timeout so a misconfigured peer doesn't
     hang discovery forever (the discovery exclusion is justified by
     "config / firewall problems where retry won't help" — but this
     is a coordination-protocol bug, not a config error, and once
     fixed the loop terminates within the first re-broadcast cycle).
   - Document the chosen retry budget in `runner/CUSTOM.md`.

4. **Reproducer test** in `runner/src/protocol.rs::tests`. Construct
   two `Coordinator` instances. Drive alice through `discover` (clean
   exit) and into a parked `ready_barrier(spawn_n)`. Then start bob's
   `discover()`. Without the fix, bob's discover panics. With the fix,
   bob's discover returns `Ok(<alice's proposed log_subdir>)` within
   the discovery linger plus one re-broadcast cycle (~3 s). Use the
   existing `multicast_test_lock` to serialise with other multicast
   tests. Cap the test at ~10 s.

5. **Optional: add a second test** asserting that the bounded retry
   in scope item 3 fires when the peer never re-emits `Discover` (e.g.
   peer permanently stuck in a barrier with the fix-emitting code
   gated off). Confirms the fallback bound and the failure message.

#### Validation

From workspace root (the worktree root):
- `cargo build --release -p runner` clean.
- `cargo test --release -p runner` green, including the new
  reproducer.
- Existing tests still pass — `barrier_linger_prevents_slow_peer_hang`
  is a critical regression target.
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.

#### Acceptance criteria

- [ ] `last_log_subdir` cache on `Coordinator`, populated by
      `discover()` just before returning.
- [ ] `ready_barrier`, `done_barrier`, `exchange_resume_manifest`
      each re-emit `Discover` for inbound `Discover` messages.
- [ ] `protocol.rs:395` `.expect(...)` panic replaced by a bounded
      retry that returns `Ok(...)` once the leader's `Discover`
      arrives, and a clean `bail!` if the budget elapses.
- [ ] New reproducer test passes (asserting the fix; not the bug).
- [ ] All existing tests still green (`barrier_linger_prevents_slow_peer_hang`,
      `done_barrier_hang_repro_when_peer_already_advanced`, the
      T-coord.2 timeout suite).
- [ ] `metak-shared/api-contracts/runner-coordination.md` updated
      with a short "Discovery responds to late-arriving discoveries"
      subsection.
- [ ] `runner/CUSTOM.md` updated.
- [ ] `metak-orchestrator/STATUS.md` updated with completion report.

#### Out of scope

- Carrying `log_subdir` on non-Discover message types. Schema-bump
  is too invasive for this fix.
- Touching T-coord.1b's `Done` re-emission infrastructure. The two
  fixes are independent and may land in either order.
- Removing the discovery-not-subject-to-barrier-timeout policy. That
  policy still holds; the bounded retry inside `discover()` is
  internal and not the same thing as T-coord.2's external barrier
  timeout.

---

## Realism sprint — pre-rerun fixes (T-impl.*)

Goal: get the all-variants matrix into a state where its rows reflect
**transport behaviour** rather than benchmark-harness limits. Filed
2026-05-11 after diagnosing the same-machine `_183143` run. See
`metak-shared/presentation-brief.md` §§5–6 for what each fix targets.

### T-impl.1: runner — capture variant stderr per spawn

**Repo**: `runner/`
**Status**: pending.

#### Background

When the Zenoh `1000x100hz-qos3` spawn was investigated, bob's JSONL
log was truncated mid-write — the variant child died/was-killed during
`operate` and there is **no record of why** because the spawn-monitor
discards child stderr. This blocks every "Zenoh under load" question.

#### Scope

1. In `runner/src/spawn.rs` (or wherever `Command::spawn` is invoked),
   redirect the child's stderr to a per-spawn file:
   `<log_subdir>/<effective_name>-<runner_name>-stderr.txt`.
2. Capture must be **non-blocking** for the spawn-monitor: use
   `Stdio::piped()` plus a dedicated thread that copies child stderr
   to the file, OR `Stdio::from(File::create(...))` if the child's
   stderr can go directly to disk (simpler — prefer this if
   `inherit_stderr` isn't a requirement somewhere).
3. The file must exist even if the spawn is killed for timeout. Use
   line-buffered writes if the implementation supports it.
4. **Do NOT** suppress stderr from the runner's own console — only
   the variant child's stderr should be redirected. Operators still
   need to see runner-side panics.
5. Update `runner/CUSTOM.md` with a short subsection naming the file
   convention so analysis / debugging tooling can find them.

#### Tests

- Unit test: spawn a stub child that writes a known string to stderr
  and exits; assert the file is created with the expected content.
- Integration test: spawn a child that prints to stderr and then
  panics with a recognisable message; assert the file contains both
  the print AND the panic message.

#### Acceptance criteria

- [ ] Per-spawn stderr file appears under the log subfolder.
- [ ] Variant panic / abort messages survive to the file.
- [ ] No deadlock on spawn-monitor when child closes stderr cleanly.
- [ ] No deadlock when child is killed mid-write.
- [ ] `runner/CUSTOM.md` updated with file-naming convention.
- [ ] All existing runner tests pass.

### T-impl.2: variants — bump UDP socket buffers on all UDP transports

**Repo**: `variant-base/`, `variants/custom-udp/`, `variants/hybrid/`,
`variants/quic/`, `variants/webrtc/`, `variants/zenoh/`.
**Status**: pending.

#### Background

Windows default UDP recv buffer is ~64 KB. At sustained 100 K pkt/s
kernel buffers overflow within milliseconds, producing apparent
"loss" that is really kernel-side drop. This affects every same-host
high-rate row in the matrix.

#### Scope

1. Add a small helper in `variant-base/` that bumps `SO_RCVBUF` and
   `SO_SNDBUF` to **8 MiB** on a `socket2::Socket` (or `UdpSocket`
   wrapped equivalently). The helper should be cross-platform; on
   Windows the actual achieved size may be capped by the OS, so the
   helper should query the achieved size back and log a single
   warning line if it is below 1 MiB.
2. Apply the helper at every UDP socket-creation site in:
   - `variants/custom-udp/`
   - `variants/hybrid/` (its UDP path; do NOT change the TCP path)
   - `variants/quic/` (the underlying UDP socket quinn builds on)
   - `variants/webrtc/` (the underlying ICE / DTLS UDP socket)
3. For `variants/zenoh/`: Zenoh's session config has transport-layer
   queue tuning. Set the relevant fields so the session-level
   send/recv queues are large enough to absorb similar bursts.
   Document the exact field path in `variants/zenoh/CUSTOM.md`. If
   Zenoh's config-only knobs do not match the 8 MiB target,
   document the closest equivalent.

#### Tests

- Unit: create a UDP socket via the helper, query `SO_RCVBUF` and
  `SO_SNDBUF`, assert both are >= 1 MiB (a conservative floor that
  should work on every reasonable kernel).
- Per-variant: each variant's existing smoke / integration test
  must still pass.

#### Acceptance criteria

- [ ] Helper exists in `variant-base/`; both buffer dimensions set.
- [ ] Helper invoked at every UDP creation site in the five UDP-using
      crates.
- [ ] Zenoh session config updated equivalently and documented.
- [ ] Achieved size logged when below 1 MiB.
- [ ] All existing tests pass; new unit test passes.

### T-impl.3: runner + variant-base — raise EOT timeout default + config passthrough

**Repo**: `runner/`, `variant-base/`.
**Status**: pending.

#### Background

The EOT phase default budget is `max(operate_secs, 5)` = 30 s at our
current config. For hybrid TCP at 100 K writes/s this is too short:
~3 M backlogged messages cannot drain in 30 s.

#### Scope

1. In `variant-base/`: change the default computation for the
   `--eot-timeout-secs` flag from `max(operate_secs, 5)` to
   **`max(3 * operate_secs, 30)`** (so 30 s operate -> 90 s drain).
   Document in `variant-base/CUSTOM.md`.
2. In `runner/`: ensure the config's per-variant `eot_timeout_secs`
   (if present) is passed through as `--eot-timeout-secs <N>` on the
   variant child's command line. If absent, the runner does NOT pass
   the flag (variant uses its default from step 1).
3. Note the optional override in
   `metak-shared/api-contracts/toml-config-schema.md`.

#### Tests

- Unit in `variant-base/`: with `operate_secs = 30` and
  `eot_timeout_secs = None`, the driver runs the EOT phase for at
  least 90 s if peers never EOT.
- Unit in `variant-base/`: with `eot_timeout_secs = Some(5)`, the
  EOT phase fires `eot_timeout` after ~5 s.
- Integration in `runner/`: a config with
  `[variant.common].eot_timeout_secs = 7` results in the variant
  child's command line containing `--eot-timeout-secs 7`.

#### Acceptance criteria

- [ ] Default raised in `variant-base/` to `max(3 * operate_secs, 30)`.
- [ ] Runner passes the config field through as a CLI flag.
- [ ] Config schema doc notes the field.
- [ ] All existing tests pass; new unit + integration tests pass.

### T-impl.4: variant-websocket — same-host port assignment

**Repo**: `variants/websocket/`.
**Status**: pending.

#### Background

On a single host both alice's and bob's websocket variants try to bind
the same server port and one of them fails. Every `websocket-*` row in
the same-machine run shows 0 writes / 100 % loss as a result.

#### Scope

1. The variant already receives `--runner <name>` from the runner.
   Compute the runner's index in the `--peers` list (or accept a new
   `--runner-index <N>` injected by the runner — pick whichever
   matches the existing CLI passing pattern; extend the runner side
   too if needed).
2. Bind the websocket server to `base_port + runner_index`.
3. Update the variant's peer-connect logic to compute the *peer's*
   server port the same way and connect to
   `peer_host:base_port + peer_index`.
4. Document the offset convention in `variants/websocket/CUSTOM.md`.

#### Tests

- Unit: with `--runner alice --peers alice=127.0.0.1,bob=127.0.0.1`
  and a base port of 19200, alice binds 19200, bob (when running
  separately with the same args) binds 19201.
- Smoke: two-runner same-host websocket spawn for
  `websocket-100x100hz-qos3`, both processes produce non-zero
  `write` and `receive` counts.

#### Acceptance criteria

- [ ] Variant computes its own server port from runner index.
- [ ] Variant computes each peer's port the same way.
- [ ] Same-host websocket spawn delivers non-zero data.
- [ ] `variants/websocket/CUSTOM.md` updated.
- [ ] All existing tests pass.

### T-impl.5: variant-webrtc — signaling robustness investigation + fix

**Repo**: `variants/webrtc/`.
**Status**: pending — investigation first, fix second.

#### Background

Many webrtc spawns at higher rates produce 0 writes / 0 ms because the
DataChannel handshake has not completed before `operate` begins.
Spawns that do connect look fine.

#### Scope (investigation phase)

1. Spawn `webrtc-100x100hz-qos1` same-host with verbose logging.
   Capture per-peer signaling timeline.
2. Inspect: discovery / signaling channel, handshake timeouts, whether
   the variant's `connect` phase awaits `data_channel_open`.
3. Write a one-paragraph diagnosis to `metak-orchestrator/DECISIONS.md`
   (next available ID).

#### Scope (fix phase, only if diagnosis points to an actionable issue)

4. Most likely fixes (in order): await `data_channel_open` in
   `connect`; bump signaling timeout; properly sequence ICE candidate
   gathering vs stabilize.
5. Apply the fix; document in `variants/webrtc/CUSTOM.md`.

#### Tests

- Smoke: run `webrtc-100x100hz-qos1` same-host 3 times in a row;
  at least 2 of 3 produce non-zero `write` AND `receive` counts.

#### Acceptance criteria

- [ ] Diagnosis entry in DECISIONS.md.
- [ ] If fix applied: >= 67 % of same-host high-rate webrtc spawns
      produce non-zero data in a 3-run smoke test.
- [ ] If no fix possible: documented in `variants/webrtc/CUSTOM.md`
      and rerun decision accepts "WebRTC not characterised at high
      rates."
- [ ] All existing tests pass.

---

## Realism sprint — Tier 2 (writer backpressure)

### T-impl.6: variant-base — `try_write` trait method + driver respect

**Repo**: `variant-base/`.
**Status**: pending — gates T-impl.7.

#### Background

The matrix sweeps `vpt x tick_rate_hz` write rates regardless of what
the receiver can sustain. At 100 K writes/sec on this hardware every
transport's kernel buffer overflows; the resulting rows tell us about
buffer sizing, not transport throughput.

Goal: shift workload semantics from "writer always emits
`vpt x tick_rate_hz`" to "writer emits up to `vpt x tick_rate_hz` if
the transport reports it is not currently backpressured."

#### Scope

1. Add `fn try_write(&mut self, path: &str, value: &[u8], qos: Qos)
   -> Result<bool>` to the `Variant` trait. Returns `Ok(true)` if the
   write was accepted, `Ok(false)` if the transport is currently
   backpressured (no error, just "not now"). Errors still propagate.
2. Default impl: call the existing `write(...)` and return `Ok(true)`.
3. Driver: in the operate-phase tick loop, call `try_write` instead
   of `write`. If it returns `Ok(false)`, log a
   `backpressure_skipped` event with path and qos, continue to the
   next value. No retry within the same tick.
4. Schema: add `backpressure_skipped` event to
   `metak-shared/api-contracts/jsonl-log-schema.md`.
5. Analysis: surface a `backpressure_skipped_count` per
   `(variant, qos)` in the integrity report.

#### Tests

- Unit: stub variant whose `try_write` always returns `Ok(false)`
  produces zero `write` events and >= 1 `backpressure_skipped` per
  intended tick.
- Unit: default impl behaves identically to `write`.
- Integration: `variant-dummy` lifecycle still exits cleanly.

#### Acceptance criteria

- [ ] Trait method added with default impl.
- [ ] Driver calls `try_write` and logs `backpressure_skipped`.
- [ ] Schema doc updated.
- [ ] Analysis exposes `backpressure_skipped_count`.
- [ ] All existing tests pass.

### T-impl.7: variants — implement `try_write` per transport

**Repo**: all six variant crates.
**Status**: pending — depends on T-impl.6.

#### Per-variant scope

Each transport detects backpressure differently. The override should
be cheap and honest (never `Ok(true)` if the data would be dropped).

- **Custom UDP** — non-blocking send; `WouldBlock` -> `Ok(false)`.
- **Hybrid** — same on UDP path; on TCP path non-blocking send and
  `WouldBlock` -> `Ok(false)`. Independent per QoS.
- **QUIC** — `Connection::send_datagram` returns
  `SendDatagramError::Blocked` for unreliable. Stream sends use
  poll/try semantics.
- **Zenoh** — configure each Publisher with
  `congestion_control = Drop` and check write-side return; or use
  Publisher pending depth if available. Document the chosen knob.
- **WebRTC** — `RTCDataChannel::buffered_amount()` > 4 MiB ->
  backpressured.
- **WebSocket** — non-blocking TCP send; `WouldBlock` -> `Ok(false)`.

#### Tests

- Per-variant unit: synthesize a write loop that fills the send
  buffer; assert `try_write` returns `Ok(false)` at some point
  before crashing.
- Existing tests must pass.

#### Acceptance criteria

- [ ] Each variant overrides `try_write`.
- [ ] No variant returns `Ok(true)` when the kernel / library would
      drop the data.
- [ ] Per-variant `CUSTOM.md` documents the signal used.
- [ ] All existing tests pass.

#### Out of scope (T-impl.6 + T-impl.7)

- Receiver-driven backpressure across variants.
- Token-bucket / rate limiting on the writer side.

### T-impl.8: variant-base — self-pacing for max-throughput (yield then sleep fallback)

**Repo**: `variant-base/`.
**Status**: pending.
**Depends on**: T-impl.6 (try_publish trait), T-impl.7 (per-variant overrides). Both landed.

#### Background

The `max-throughput` workload profile runs the operate phase **without
any tick-rate sleep** so each transport's headline rate is measured.
Without pacing the writer drowns the receiver and the spawn either
hits `eot_timeout` or shows ~99 % loss. T-impl.7 added per-variant
`try_publish` returning `Ok(false)` when the local transport is
backpressured, but the driver currently just logs
`backpressure_skipped` and continues to the next value — there is no
back-off, so the next `try_publish` is almost certainly `Ok(false)`
too, and the loop burns CPU without giving the receiver any chance
to drain.

For **`scalar-flood`** this is fine: the explicit tick-rate sleep
already paces the writer. For **`max-throughput`** we want the
writer to back off briefly on `Ok(false)` so the receiver can
catch up.

#### Scope (max-throughput only — do NOT change `scalar-flood`)

In `variant-base/src/driver.rs`, identify the operate-phase loop
that runs when `workload_profile = MaxThroughput`. The current
behaviour after the T-impl.6 changes is:

```
loop {
    if elapsed >= operate_secs { break; }
    let seq = next_seq();
    let path = next_path();
    let payload = next_payload();
    match variant.try_publish(path, payload, qos, seq)? {
        true  => logger.log_write(...);
        false => logger.log_backpressure_skipped(path, qos);
    }
}
```

Change the `false` branch to introduce a two-tier back-off:

1. **First `Ok(false)` since the last `Ok(true)`**: log
   `backpressure_skipped` AND call `std::thread::yield_now()`. Don't
   sleep. The yield costs less than 100 µs on Windows but releases the
   timeslice so the receiver thread can be scheduled.
2. **Second consecutive `Ok(false)`** (the immediately next iteration
   also returned `Ok(false)`): log `backpressure_skipped` AND call
   `std::thread::sleep(Duration::from_millis(1))`. On Windows this
   actually sleeps ~15 ms by default (timer resolution); on Linux it's
   ~1 ms. Either way it's a substantially longer back-off than the
   yield gave us.
3. **Third and subsequent consecutive `Ok(false)`**: same as #2 (just
   `sleep(1ms)`). No further escalation.
4. **`Ok(true)` resets the consecutive-counter to 0**, so the very
   next `Ok(false)` after any successful write goes back to yield.

The consecutive-counter is a `u32` (or `usize`) local to the operate
loop; no need for thread-safety. Reset on the first successful publish.

Do NOT change the behaviour when `Ok(false)` happens under
`scalar-flood` — that path keeps the current "log and continue"
behaviour because the inter-tick sleep already provides pacing.

#### Tests (in `variant-base/src/driver.rs::tests`)

1. **Yield path**: a stub variant whose `try_publish` returns
   `Ok(false)` only on the first call, then `Ok(true)`. With
   `MaxThroughput` profile and `operate_secs = 0.1` (or whatever's
   the minimum the test scaffolding supports), assert that
   exactly one `backpressure_skipped` event is logged, multiple
   `write` events follow, AND `std::thread::sleep` was NOT called
   on that path. (You can avoid mocking sleep by checking the
   wall-clock: if the test takes < 5 ms it can't have called
   sleep(1ms); if it takes > 10 ms, it did.) Be tolerant of
   scheduler noise — use generous bounds.
2. **Sleep fallback path**: a stub variant whose `try_publish`
   always returns `Ok(false)`. After ~50 ms wall-clock, the
   `backpressure_skipped` count should be in the low tens, NOT in
   the thousands — because the sleep is rate-limiting. (At
   1 ms sleep, ~50 events. At Windows' 15 ms granularity, ~3.) Use
   bounds like "more than 5, less than 200" to absorb both
   scenarios.
3. **Reset behaviour**: a stub variant whose `try_publish` returns
   the pattern `[false, true, false, true, false, true, ...]`.
   Each `false` should be paired with a yield (not a sleep), so
   the test should complete much faster than if every `false` had
   triggered a sleep. Same wall-clock bounding.
4. **`scalar-flood` is unchanged**: a stub variant with always-false
   `try_publish` under `ScalarFlood` profile must behave as today
   (one `backpressure_skipped` per tick × vpt; no yield/sleep added
   beyond the existing inter-tick sleep).

#### Validation (MANDATORY)

From workspace root:
- `cargo build --release -p variant-base` clean.
- `cargo test --release -p variant-base` all-green (existing 58+2 plus
  the new 4 tests).
- `cargo test --release --workspace` all-green (variants still pass —
  they don't override the driver loop, so nothing should regress).
- `cargo clippy --release --workspace --all-targets -- -D warnings`
  zero warnings.
- `cargo fmt --check` clean.

#### Docs

- `variant-base/CUSTOM.md`: new "Self-pacing in max-throughput
  (T-impl.8)" subsection documenting the two-tier back-off, why
  yield-first / sleep-fallback, and the Windows timer-granularity
  caveat (~15 ms actual sleep when asking for 1 ms).
- `metak-shared/api-contracts/jsonl-log-schema.md`: no schema change
  needed (the `backpressure_skipped` event is unchanged); add a
  sentence noting that under `max-throughput` these events are now
  *also* a pacing signal, not just a drop count.

#### Acceptance criteria

- [ ] Driver's `max-throughput` operate loop has the two-tier back-off.
- [ ] Counter resets on `Ok(true)`.
- [ ] `scalar-flood` operate loop is unchanged.
- [ ] All four new unit tests pass.
- [ ] All existing tests pass (no regressions in variants).
- [ ] `variant-base/CUSTOM.md` documents the back-off + Windows caveat.
- [ ] Schema doc gets the one-sentence note.

#### Out of scope

- Changing the back-off duration adaptively (e.g. exponential).
- Receiver-driven explicit backpressure signal across the wire.
- Token-bucket rate limiting (still out of scope).
- Adjusting Windows timer resolution via `timeBeginPeriod`.

### T-impl.9: runner -- surface diagnostics on spawn failure -- done

**Repo**: `runner/`.
**Status**: done 2026-05-11. All acceptance criteria met.

#### Background

Triggered by a real diagnostic session on `configs/two-runner-websocket-qos4.toml`. A websocket variant on bob hit the 60 s runner timeout, was TerminateProcess'd, and produced an empty stderr file plus a JSONL log truncated mid-record. The runner's only output was a single `'<name>' finished: status=timeout, exit_code=-1` line with no pointer to where to look.

#### Implementation

On non-success spawn outcome (`failed` or `timeout`), the runner now prints, immediately after the existing status line:
1. Absolute path to the per-spawn stderr capture file.
2. Absolute path to the variant's JSONL log file (skipped silently if file missing).
3. A `---- stderr tail (last 20 lines) ----` block reading the last 20 lines (capped at last 64 KiB from EOF), or an `(stderr capture is empty -- child likely killed before writing any output)` notice if the file is empty.

Helpers `tail_stderr_file` and `jsonl_log_path` live in `runner/src/spawn.rs`. Call site in `runner/src/main.rs` via `print_failure_diagnostics` right after the existing status line. `success` and `skipped` spawns stay silent (unchanged behaviour).

#### Acceptance criteria

- [x] On `failed` or `timeout` spawn, runner stderr includes capture file path, JSONL log path (if present), and either tail or empty-capture notice.
- [x] Tail capped at 20 lines and bounded by ~64 KiB from EOF.
- [x] `success` / `skipped` outcomes preserve silent behaviour.
- [x] 4 new integration tests + 8 new unit tests pass.
- [x] `cargo test --release -p runner` all-green.
- [x] `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- [x] `cargo fmt -p runner -- --check` clean.
- [x] End-to-end smoke on `configs/two-runner-websocket-qos4.toml` shows the new diagnostic block in both runners' terminals.

Commits: `d614a43`, `c8c1808`, `d501ec9`, `f5587b7`, `a101fd3`.

### T-impl.10: variant-base -- adaptive receive-drain in operate loop -- code done, fixture acceptance partial

**Repo**: `variant-base/`.
**Status**: code done 2026-05-11 (committed). Fixture acceptance partial -- end-to-end repro still fails. Follow-up T-impl.11 (websocket-specific) needed; direction pending user pick.

#### Background

A two-runner `configs/two-runner-websocket-qos4.toml` run at `tick_rate_hz=100, values_per_tick=1000` (100 K msg/s symmetric) deadlocked ~130 ms into the operate phase. alice: 6126 writes / 1139 receives, then `WSAECONNRESET` -> exit 1. bob: 6823 writes / 1075 receives, then runner-timeout TerminateProcess. The driver's per-tick drain budget (`1 ms` wallclock, `2 * values_per_tick` messages) was hypothesised as the bottleneck.

#### Implementation

In `variant-base/src/driver.rs`:
- New `compute_operate_drain_time_budget()` helper. scalar-flood: `max(1ms, (next_tick - now) - 1ms safety margin)`. max-throughput: flat 5 ms.
- `drain_msg_budget` bumped from `2 * vpt` to `4 * vpt`, floor at 1.
- EOT-phase drain unchanged.
- Four new unit tests in `driver::tests`; T-impl.8 tests untouched and still pass.
- `variant-base/CUSTOM.md` "Operate-loop drain budgets (T-impl.10)" subsection added documenting the change and the 2026-05-11 incident that motivated it.

#### Acceptance criteria

- [x] Drain budget logic updated.
- [x] All four new unit tests pass.
- [x] Existing variant-base tests pass unchanged.
- [x] `cargo test --release --workspace` all-green; no integrity-gate regression in any variant test suite.
- [x] `variant-base/CUSTOM.md` documents the new behaviour.
- [x] No changes to `metak-shared/api-contracts/`.
- [ ] End-to-end `websocket-1000x100hz` two-runner repro completes successfully. **FAILED**: same `WSAECONNRESET` failure mode recurs. Post-fix: alice 6211w / 1049r, bob 7291w / 1334r (bob's receives +24% vs pre-fix; ratio unchanged at ~5.5:1). Driver fix helped marginally but the dominant bottleneck is per-message WS frame-parse cost, not driver drain budget. See `metak-orchestrator/STATUS.md` worker completion report for full repro logs. The driver change stays landed -- it is a real (though dose-limited) robustness improvement and has no regressions. T-impl.11 will address the websocket-specific cap.

Commits: `e9457eb`, `a397450`, `73e89af`.

#### Out of scope

- Interleaving `try_publish` and `poll_receive` within a tick.
- Per-variant tuning of drain budgets.
- Investigating Zenoh T10.2 timeouts under the same hypothesis (stays in its own task).
- Re-running hybrid's high-rate fixtures as a baseline (separate measurement task).

---

## E14: Threading-Mode Dimension and Receive-Centric Analysis

See `EPICS.md` E14 for the full epic description, motivation (WASM
compilation target + T-impl.10 residual failure), and acceptance gates.

**Contract dependency.** T14.1 and T14.8 require updates to
`metak-shared/api-contracts/variant-cli.md` and
`metak-shared/api-contracts/toml-config-schema.md`. Draft proposals
appended to those files under "DRAFT -- E14 additions". User review
required before any worker is spawned against the new contract.

**Spawn ordering**:
- T14.1 lands first (foundational; defines the trait surface).
- T11.5 may start IN PARALLEL with T14.1 (analysis pivot does not
  depend on the threading-mode dimension; it only benefits from it
  once data flows).
- T14.2 - T14.7 (variant implementations) can spawn in parallel
  after T14.1 lands and the contracts are agreed.
- T14.8 (runner + config expansion) needs T14.1's
  `supported_threading_modes()` API to exist before it can probe
  variants. Spawn after T14.1 but in parallel with T14.2 - T14.7.

### T14.1: variant-base -- threading-mode infrastructure + recv-buffer arg

**Repo**: `variant-base/`.
**Status**: pending. Foundational; gates T14.2 - T14.8.

#### Background

E14 introduces a `threading_mode` dimension so each variant can be
measured under both single-threaded sync (no tokio, WASM-friendly) and
multi-threaded (per-peer reader thread) execution. The dimension is
declared per-variant via a new trait method; the driver passes the
chosen mode to `Variant::connect`; each variant decides what the mode
means inside its own implementation. A new `--recv-buffer-kb` injected
arg lets every variant size its OS-level recv buffer uniformly.

#### Scope

1. **New type** in `variant-base/src/variant_trait.rs` (or wherever the
   trait lives -- worker to locate):
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
   pub enum ThreadingMode { Single, Multi }
   ```
   Implement `FromStr` (accept `"single"` / `"multi"`, case-insensitive)
   and `Display` (lowercase). Serde tags as `"single"` / `"multi"`.

2. **Trait extensions** in `Variant`:
   ```rust
   fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
       &[ThreadingMode::Single]
   }
   ```
   Default: single only. Variants that need it (websocket, custom-udp,
   hybrid) override with `&[Single, Multi]`. Async-only variants (quic,
   webrtc, zenoh) override with `&[Multi]`. Order does not matter;
   runner does declared-set membership checks.

   ```rust
   fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()> {
       Ok(()) // default: no-op for Single mode; Multi-supporting variants override
   }
   fn stop_reader_threads(&mut self) -> Result<()> { Ok(()) }
   ```
   The driver calls `start_reader_threads(mode)` immediately AFTER
   `connect` returns successfully, and `stop_reader_threads` during
   the `disconnect` path (BEFORE calling `disconnect` itself so the
   reader threads can drain pending receives cleanly). If the variant
   does not override, both are silent no-ops.

3. **Driver passes mode to connect**: extend `Variant::connect` signature
   to accept `threading_mode: ThreadingMode` as an additional arg.
   Variants that don't care can ignore it. Variants that branch on
   mode (websocket, custom-udp, hybrid) use it to decide whether to
   spawn reader threads.

   This is a breaking change to the trait. Existing variant implementations
   must be updated -- but for variants that don't yet support Multi mode
   the change is just adding an unused arg. T14.2 - T14.7 will update
   each variant; for now T14.1 ships the trait change PLUS a default
   implementation that the existing variant code compiles against.
   Concretely: the trait signature changes, every variant gets the
   new arg added to its `impl Variant`, but only `VariantDummy` is
   updated in this task; the other six variants need updating in
   T14.2 - T14.7. Use `cargo check --workspace` to find the touch
   points. If trait-default-impls let us avoid touching all variants
   in this task, prefer that route.

4. **CLI args** in `variant-base/src/common_cli.rs` (or equivalent):
   - `--threading-mode <single|multi>` -- required (runner-injected).
   - `--recv-buffer-kb <u32>` -- optional, default `4096`, range
     `64..=65536` (64 KiB to 64 MiB). 64 KiB is below the Windows
     default but harmless; 64 MiB is generous on a Raspberry Pi 4.
     Tighten or loosen as the worker discovers what variants need.

5. **Driver plumbing**: parse the new args, pass `ThreadingMode` to
   `Variant::connect`, call `start_reader_threads(mode)` after connect
   returns, call `stop_reader_threads` before disconnect. The
   `recv_buffer_kb` value is passed via `CommonCliArgs` and variants
   read it from there -- no new trait method needed.

6. **JSONL schema**: add a `threading_mode` field to the existing
   `connected` event so every log file records which mode the spawn
   ran in. Optional now; promoted to required once T14.8 lands.
   Schema doc update in `metak-shared/api-contracts/jsonl-log-schema.md`
   appendix (worker writes the change, orchestrator reviews).

7. **VariantDummy update**: dummy has no real I/O so it trivially
   supports both modes. Declare `[Single, Multi]` capabilities. Both
   modes do the same thing internally (in-process data board); the
   point is to exercise the new infrastructure end-to-end.

#### Tests (in `variant-base/src/`)

1. Unit: `ThreadingMode` parse/display roundtrip.
2. Unit: default `supported_threading_modes()` returns `[Single]`.
3. Unit: default `start_reader_threads` / `stop_reader_threads` are
   no-ops returning `Ok(())`.
4. Integration: `VariantDummy` runs end-to-end in both modes; both
   produce the expected `connected` / `phase` / `eot_sent` / `write` /
   `receive` event sequence; the `connected` event carries the
   correct `threading_mode` field.
5. Integration: protocol-driver test asserts `start_reader_threads`
   and `stop_reader_threads` are called in the right order relative
   to `connect` / `disconnect`.

#### Validation (MANDATORY)

From workspace root:
- `cargo build --release -p variant-base` clean.
- `cargo test --release -p variant-base` all-green.
- `cargo test --release --workspace` all-green -- this is where any
  variant whose `connect` signature broke would surface. **If a
  variant fails to compile, that is a known T14.2-T14.7 follow-up:
  the worker should add the new arg to each affected variant's
  `connect` signature as the minimal change to keep the workspace
  compiling, but should NOT implement Multi mode for those variants
  in this task.**
- `cargo clippy --release --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --check` clean.

#### Acceptance criteria

- [ ] `ThreadingMode` type + parse/display + serde.
- [ ] `supported_threading_modes` trait method with default.
- [ ] `start_reader_threads` / `stop_reader_threads` trait methods with
  default no-op.
- [ ] `Variant::connect` accepts `ThreadingMode`.
- [ ] `--threading-mode` and `--recv-buffer-kb` CLI args.
- [ ] Driver calls reader-thread hooks around connect/disconnect.
- [ ] `VariantDummy` declares `[Single, Multi]` capabilities and works
  end-to-end in both modes.
- [ ] `metak-shared/api-contracts/jsonl-log-schema.md` documents the new
  `threading_mode` field on `connected`.
- [ ] `metak-shared/api-contracts/variant-cli.md` documents the new
  injected args.
- [ ] All existing workspace tests pass after the worker's minimal
  signature updates to other variants.
- [ ] `variant-base/CUSTOM.md` "Threading-mode dimension (T14.1)"
  subsection added.

#### Out of scope

- Actually implementing `Multi` mode for any variant other than
  `VariantDummy`. Each variant gets its own task (T14.2 - T14.7).
- Runner config-expansion (T14.8).
- Analysis-side changes (T11.5).
- Touching the EOT phase, clock-sync, or runner-runner coordination.

---

### T14.2: variants/websocket -- implement Multi threading mode

**Repo**: `variants/websocket/`.
**Status**: pending. Depends on T14.1.
**Closes**: the T-impl.10 residual failure on
`configs/two-runner-websocket-qos4.toml`.

#### Scope

- Declare `supported_threading_modes() = &[Single, Multi]`.
- In `connect(threading_mode)`, when `mode == Multi`: spawn one OS
  thread per peer WS connection. Each thread does blocking
  `WebSocket::read_message` in a loop, parses the binary header, and
  sends `ReceivedUpdate` over a bounded `mpsc::Sender<ReceivedUpdate>`.
- `poll_receive` for `Multi` mode: try-recv on the shared `Receiver`.
  For `Single` mode: existing behaviour (inline read + parse).
- `stop_reader_threads`: signal threads to exit (close the mpsc on the
  send side, set an `AtomicBool`), join them with a short timeout.
- Apply `SO_RCVBUF = recv_buffer_kb * 1024` on the underlying TCP socket
  immediately after the WS handshake completes. Same for both modes.
- Channel bound: `4 * values_per_tick * peer_count` slots (over-provision
  to absorb bursts; bounded so a stuck consumer doesn't OOM).
- Update `variants/websocket/CUSTOM.md`: new "Threading modes (T14.2)"
  section explaining when each mode is chosen, the reader-thread
  ownership model, and the channel-bounding rationale. Remove the
  "Backpressure semantics (T-impl.7)" / "Cross-reference: T-impl.10"
  sections OR mark them historical -- the new `Multi` mode supersedes
  the T-impl.7 "default is intentional" conclusion for high-rate
  symmetric workloads while leaving the rationale correct for
  `Single` mode.

#### Tests

- Unit: threading-mode capability declaration.
- Unit: reader-thread lifecycle (`start_reader_threads` creates
  threads; `stop_reader_threads` joins them).
- Integration (existing two-runner regression): run the existing
  fixture in both modes; assert non-zero writes and receives in both.
- Integration (new): a two-runner localhost fixture analogous to
  `configs/two-runner-websocket-qos4.toml` but trimmed to the single
  `1000x100hz` spawn (or, ideally, use the existing fixture);
  `threading_modes = ["single", "multi"]`. Assert Multi mode delivers
  >= 99 % at 100 K msg/s symmetric. Single mode may show <100 %
  delivery -- record what it actually delivers without asserting a
  threshold (this is a measurement, not a gate).

#### Validation (MANDATORY)

- `cargo test --release -p variant-websocket` all-green.
- `cargo test --release -p variant-websocket -- --ignored two_runner_regression` all-green in both modes.
- End-to-end localhost repro of `configs/two-runner-websocket-qos4.toml`
  first spawn in Multi mode completes with delivery >= 99 % on both
  sides. Single mode completes (may be <100 %; record actual).
- Clippy + fmt clean.

#### Acceptance criteria

- [ ] Variant declares `[Single, Multi]`.
- [ ] Multi mode uses per-peer reader threads + bounded mpsc.
- [ ] `SO_RCVBUF` configured from `--recv-buffer-kb` in both modes.
- [ ] Existing single-mode behaviour unchanged in `Single`.
- [ ] Two-runner regression test passes in both modes.
- [ ] End-to-end repro at 100 K msg/s symmetric in Multi mode:
  delivery >= 99 % on both sides.
- [ ] CUSTOM.md updated; obsolete T-impl.7 / T-impl.10 sections marked
  historical or removed.

#### Out of scope

- TLS / wss://, subprotocols, extensions.
- Changing the publish path or the binary header format.
- Tuning the channel bound beyond the formula above.

---

### T14.3: variants/custom-udp -- implement Multi threading mode

**Repo**: `variants/custom-udp/`.
**Status**: pending. Depends on T14.1.

#### Scope

- Declare `[Single, Multi]`.
- Multi mode: one recv thread for the UDP multicast socket; one recv
  thread per active TCP connection (QoS 4 path). Each thread parses
  the binary header and pushes to a shared bounded mpsc.
- Apply `SO_RCVBUF` from `--recv-buffer-kb` to both UDP and TCP sockets.
- Update `variants/custom-udp/CUSTOM.md` with the threading-mode
  documentation.
- Tests: same shape as T14.2 (capability declaration, reader-thread
  lifecycle, two-runner regression in both modes).

#### Acceptance criteria

- [ ] Multi mode implemented per scope.
- [ ] Single-mode behaviour unchanged.
- [ ] All existing tests pass in both modes.
- [ ] CUSTOM.md updated.

---

### T14.4: variants/hybrid -- audit + implement Multi threading mode

**Repo**: `variants/hybrid/`.
**Status**: pending. Depends on T14.1.

#### Scope

- **Audit step (do this first)**: read the current Hybrid implementation
  and determine what threading model it already uses for the TCP path
  and the UDP multicast path. STATUS.md L30 hints Hybrid handles
  high-rate qos4 today; if it already uses a reader thread, T14.4
  reduces to wiring up the `ThreadingMode` API on top of existing
  behaviour. Report findings in STATUS.md before implementing.
- Declare `[Single, Multi]`.
- Multi mode: per-peer TCP reader thread + single UDP multicast recv
  thread, pushing to a shared bounded mpsc.
- Single mode: pure inline blocking I/O on the driver thread (may
  require disabling existing reader threads -- the audit will reveal
  if this is a code change or already the case).
- Apply `SO_RCVBUF` from `--recv-buffer-kb`.
- Update CUSTOM.md.
- Tests: capability declaration + reader-thread lifecycle + two-runner
  regression in both modes.

#### Acceptance criteria

- [ ] Audit findings posted to STATUS.md.
- [ ] Both modes implemented per scope.
- [ ] Existing Hybrid tests pass in Multi mode.
- [ ] Single mode passes a less-demanding fixture (worker picks; e.g.
  `hybrid-10x100hz-qos4`). High-rate symmetric is allowed to be
  lossy in Single mode -- record actual delivery without asserting.
- [ ] CUSTOM.md updated.

---

### T14.5: variants/quic -- declare Multi-only capability

**Repo**: `variants/quic/`.
**Status**: pending. Depends on T14.1.

#### Scope

- Override `supported_threading_modes()` to return `&[Multi]`.
- `connect(ThreadingMode::Single)` returns a clear error before any I/O.
- `connect(ThreadingMode::Multi)` is the existing behaviour -- no code
  change.
- Apply `SO_RCVBUF` from `--recv-buffer-kb` to the underlying UDP
  socket(s) if quinn exposes a way to do this; otherwise document
  why not.
- Update `variants/quic/CUSTOM.md` with a "Threading modes" section
  explaining: quinn is fundamentally async; a sync single-threaded
  QUIC would be a significant rewrite that does not match the
  benchmark's purpose. We declare Multi only.
- Tests: unit assertion that `connect(Single)` errors cleanly.

#### Acceptance criteria

- [ ] Capability declared `[Multi]`.
- [ ] `connect(Single)` errors before I/O.
- [ ] `--recv-buffer-kb` plumbed if possible.
- [ ] CUSTOM.md updated.
- [ ] All existing tests pass.

---

### T14.6: variants/webrtc -- declare Multi-only capability

**Repo**: `variants/webrtc/`.
**Status**: pending. Depends on T14.1.

Identical shape to T14.5. Webrtc-rs is fundamentally async + has its
own task pool. Declare `[Multi]` only; `connect(Single)` errors.

#### Acceptance criteria

- [ ] Capability declared `[Multi]`.
- [ ] `connect(Single)` errors before I/O.
- [ ] CUSTOM.md updated.
- [ ] All existing tests pass.

---

### T14.7: variants/zenoh -- declare Multi-only capability

**Repo**: `variants/zenoh/`.
**Status**: pending. Depends on T14.1.

Identical shape to T14.5 / T14.6. Zenoh has internal threads we cannot
disable from the client; declaring Single would be dishonest.

#### Acceptance criteria

- [ ] Capability declared `[Multi]`.
- [ ] `connect(Single)` errors before I/O.
- [ ] CUSTOM.md updated.
- [ ] All existing tests pass.

---

### T14.8: runner + TOML schema -- threading_modes expansion

**Repo**: `runner/`. Contract impact: `metak-shared/api-contracts/toml-config-schema.md`.
**Status**: pending. Depends on T14.1 (needs `supported_threading_modes`
to exist). Can spawn in parallel with T14.2-T14.7.

#### Scope

- TOML schema: `[variant.common]` accepts `threading_modes` as either
  a string (`"single"` or `"multi"`) or a list of strings. Default
  when absent: `["single"]` (backwards-compatible with existing configs).
- Runner expansion: cross-product over `qos` and `threading_modes`.
  A variant entry with `qos = [3, 4]` and
  `threading_modes = ["single", "multi"]` expands to four spawns:
  `<name>-qos3-single`, `<name>-qos3-multi`, `<name>-qos4-single`,
  `<name>-qos4-multi`. Naming convention: `qos` segment first, then
  `threading_mode` segment.
- Capability gating: how the runner learns each variant's
  `supported_threading_modes`. Two options for the worker to choose
  between:
  - **Static declaration in TOML**: each variant entry declares
    `supported_modes = ["single", "multi"]` and the runner validates
    against that. Simple, no runtime dependency.
  - **Probe via variant binary**: runner invokes
    `<binary> --print-capabilities` once at startup, parses JSON output.
    More accurate (single source of truth in the variant code) but
    adds a per-variant startup cost.
  Worker picks one and documents the choice in the completion report
  and in CUSTOM.md.
- Unsupported-mode handling: if a config requests a mode the variant
  doesn't support, skip the spawn with an `eprintln!` notice
  `[runner:<name>] skipping <effective_name>: variant does not support threading_mode=<mode>`.
  Do not fail the run. The spawn does not appear in the summary table.
- The injected `--threading-mode` arg passes the chosen mode to the
  child variant. The `--recv-buffer-kb` arg is also injected (default
  4096 if absent from TOML; configurable per-spawn via
  `[variant.common] recv_buffer_kb = 8192`).
- JSONL filename convention extends to include the threading_mode
  suffix: `<effective_name>-<runner>-<run>.jsonl`.

#### Tests

- Unit: TOML expansion for the four-spawn cross-product case.
- Unit: TOML expansion with `threading_modes` absent defaults to `["single"]`.
- Unit: unsupported-mode skip emits the eprintln and does not appear
  in the summary.
- Integration: a fixture config with both modes runs end-to-end through
  the dummy variant.

#### Acceptance criteria

- [ ] TOML schema accepts `threading_modes`.
- [ ] Cross-product expansion works.
- [ ] Backwards-compat: existing configs default to `["single"]` and
  run unchanged.
- [ ] Unsupported-mode skip prints the notice and continues.
- [ ] `--recv-buffer-kb` injected with default 4096.
- [ ] Contract update in `metak-shared/api-contracts/toml-config-schema.md`.
- [ ] Tests pass.

#### Out of scope

- Backwards-compat shims for the existing `qos`-only fixtures (they
  default to `["single"]`, which IS backwards-compat).
- Re-running every existing fixture in Multi mode. New end-to-end
  validation belongs in E7.

---

### T11.6: analysis -- revisit cache rebuild RSS after T11.5 schema bump

**Repo**: `analysis/`.
**Status**: pending, low priority. Filed as a skeleton 2026-05-12 by the
orchestrator after T11.5 worker flagged the issue.

#### Background

T11.5 (receive-headline pivot) bumped `analysis/schema.py` `SCHEMA_VERSION`
from `"2"` to `"3"` to force a one-time full cache rebuild on existing
datasets, since the new `threading_mode`, `recv_buffer_kb`, late-tail
fields could not be back-filled from old shards. On the ~80 GB regression
dataset the rebuild took **~30 min wall-time with peak RSS ~8.5 GB**.

This exceeds the original Phase 1.5 (E11) acceptance gate of **<4 GB peak**
on the canonical 40 GB acceptance dataset
(`inter-machine-all-variants-01-20260501_150858`).

Steady-state subsequent runs (no JSONL changes) should be fine, since the
cache is on-disk Parquet shards. Only the **one-time** rebuild on each
existing dataset spikes RSS.

#### Scope

- Reproduce the RSS spike against the E11 acceptance dataset
  (`logs/inter-machine-all-variants-01-20260501_150858/`).
- Decide whether the spike is acceptable as a documented one-time cost,
  or whether the rebuild pipeline needs per-shard memory caps (e.g.
  process shards sequentially rather than in a polars lazy plan that
  loads several into memory).
- If a fix is needed: bounded per-shard processing, smaller in-flight
  group sets, or polars streaming mode.
- Update E11 acceptance to acknowledge the one-time cost, or restore
  the <4 GB gate after a fix.

#### Acceptance criteria (rough)

- [ ] Reproduction documented with peak RSS measured on the E11
  acceptance dataset.
- [ ] Either (a) the rebuild fits in <4 GB peak RSS, or (b) E11
  acceptance is updated to document the one-time cost with a clear
  rationale.

#### Out of scope

- Reverting T11.5's schema bump. The new fields are needed for E14
  data; the bump is unavoidable.
- Optimising steady-state analysis runtime. That is a separate
  concern.

---

### T14.10: variants/websocket -- log-from-reader to lift delivery cliff

**Repo**: `variants/websocket/` (may generalise to other Multi-mode variants).
**Status**: pending. Filed 2026-05-12 after T14.2 end-to-end repro.

#### Background

T14.2 closed the T-impl.10 deadlock by introducing per-peer reader
threads + bounded mpsc + drop-on-full. End-to-end smoke on the original
`websocket-1000x100hz` symmetric flood (100 K msg/s × 2 peers, qos 4)
runs deadlock-free, but **delivery is ~25-33%** in each direction.
The dropped frames are silently discarded by the reader (drop-on-full
when the channel is at its `4 * vpt * peer_count` cap).

The user's stated intent (recorded in `metak-shared/overview.md`
"Cross-cutting goals") is: *"if a single threaded op cant handle a
certain throughput we should just ensure we configure receive buffers
to be as large as possible so that we can then just log all the
messages being received with a very bad latency whenever the
throughput couldn't be atchieved... it's especially important to
present the receive throughput because that's what really matters"*.

T14.2's drop-on-full violates the "log all the messages being received"
half of that intent. The reader thread parses every frame off the wire,
but only those that fit in the channel before the driver drains them
get logged; the rest never reach JSONL.

#### Root cause

Per-tick budget (10 ms at 100 Hz) is dominated by the variant's publish
path. At 1000 vpt = 1 publish per ~10 us; the driver spends almost all
its budget publishing and has microseconds left to drain the mpsc. The
reader produces messages faster than the driver can consume them, the
bounded channel fills, and the reader drops.

#### Scope (option-3 path from T14.2's STATUS.md)

Move JSONL `receive` logging **out of the driver thread** and into the
reader thread(s). The driver's `poll_receive` becomes purely an
in-process state-update mechanism for callers who need the
`ReceivedUpdate` (e.g. variants doing app-layer NACK in Multi mode),
not the logging mechanism.

Concrete shape:
- The websocket reader thread, after decoding each frame, writes the
  `receive` event directly to JSONL via a thread-safe logger handle
  (the existing `Logger` is already `Arc<Mutex<...>>`-friendly or
  needs a thin shim).
- The reader still pushes to the mpsc for the driver to observe
  EOT-related items (`Eot`, `PeerDropped`) and any future protocol
  logic, but `Data` items can be log-then-forget.
- Channel bound can be smaller now -- it only needs to hold lifecycle
  items, not data.
- This changes the architecture from "single-writer JSONL via driver"
  to "multi-writer JSONL with logger as the synchronisation point".
  Logger mutex contention becomes the new bottleneck, but writes are
  microseconds-cheap so the cliff moves far above 100 K msg/s.

#### Considerations and risks

- **Ordering**: receives logged from N reader threads may interleave
  in JSONL by writer. Existing analysis groups by (writer, seq) so
  the offline ordering is unchanged. The wall-clock-ordering across
  writers is already non-deterministic on a multicore machine.
- **Generalisation**: if this works for websocket, the same pattern
  applies to custom-udp and hybrid in Multi mode. Both currently
  rely on the driver-side drain of their mpsc. T14.10 should land
  for websocket first, validate on the high-rate fixture, then
  generalise.
- **Out of scope**: removing the mpsc entirely. Lifecycle items
  (`Eot`, `PeerDropped`) still need driver visibility.

#### Acceptance criteria

- [ ] Reader thread writes `receive` events directly to JSONL.
- [ ] Driver `poll_receive` in Multi mode no longer consumes `Data`
      items from the mpsc (only lifecycle items).
- [ ] End-to-end repro of `websocket-1000x100hz-qos4-multi` on
      `configs/two-runner-websocket-qos4-multi-1000x100hz.toml`
      shows **delivery >= 99 %** -- not because frames magically
      arrive faster, but because the reader logs all parsed frames
      regardless of driver-drain cadence.
- [ ] Existing tests still pass (`cargo test --release -p
      variant-websocket`).
- [ ] No regression on the lower-rate `two_runner_websocket_both_modes_qos3_smoke`.

#### Out of scope

- Per-direction loss budgets, latency targets, or fairness guarantees.
  Just "log everything that came off the wire".
- Generalising to custom-udp / hybrid as part of THIS task; file a
  follow-up T14.11 if websocket result motivates it.
- Receiver-driven flow control (would push back on the writer; not
  what the user wants).

---

### T14.11: flaky test -- variant-quic try_publish_qos1_reports_backpressure_under_burst

**Repo**: `variants/quic/`.
**Status**: pending, low priority. Filed 2026-05-12.

#### Background

`variants/quic/src/quic.rs::tests::test_try_publish_qos1_reports_backpressure_under_burst`
is timing-sensitive. The T14.5+6+7 worker and the T14.3 worker both
independently observed it failing under CPU contention from concurrent
sibling workers but passing in isolation. Not a regression introduced
by E14; a pre-existing flakiness surfaced by parallel worker execution.

#### Scope

- Reproduce under load and identify the timing assumption (likely a
  spin-loop bound or sleep duration that's too tight on a contended host).
- Make the test deterministic OR mark `#[ignore]` and rely on
  manual-run validation.

#### Acceptance criteria

- [ ] Test passes 10/10 runs under `cargo test --release --workspace`
      with all other workspace tests running concurrently.
- [ ] If made `#[ignore]`, document why in the test's rustdoc and
      provide a one-line manual-run command.

#### Out of scope

- Other flaky tests in the workspace.

---

### T14.13: variants/quic -- investigate ordering failures at qos4 reliable streams

**Repo**: `variants/quic/`.
**Status**: pending, filed 2026-05-12 by orchestrator after E14 smoke.

#### Background

The 2026-05-12 E14 smoke (`configs/two-runner-smoke-e14.toml`) ran QUIC
at 100 vpt x 100 Hz x qos 4 (reliable). The T11.5 integrity report
flagged both directions with `[FAIL: ordering]`:

```
quic-100x100hz-multi  alice->bob  qos 4  100,100 / 100,100  100.00%  41911 out-of-order
quic-100x100hz-multi  bob->alice  qos 4  100,100 / 100,100  100.00%  41471 out-of-order
```

100% delivery (no loss) but ~42K out-of-order messages out of ~100K
total per direction. That's a strong signal that the QUIC variant is
emitting messages on parallel reliable streams without per-stream
ordering downstream, or that the multi-stream interleave isn't being
preserved in receive order.

QoS 4 is supposed to be "reliable, ordered". If the variant uses one
QUIC reliable stream end-to-end the kernel guarantees ordering; if it
uses multiple parallel streams, ordering is per-stream only.

#### Scope

- Read `variants/quic/src/quic.rs` to determine the qos 4 stream
  strategy.
- If qos 4 uses multiple streams: either consolidate to one stream
  per writer for true ordering, or accept per-stream ordering and
  document the analysis-tool ordering check as not-applicable to
  QUIC qos 4.
- Update CUSTOM.md with the chosen semantics.

#### Acceptance criteria

- [ ] Root cause documented.
- [ ] Either ordering is enforced or the integrity-check semantics
  for QUIC qos 4 are adjusted with rationale.
- [ ] T11.5 integrity report no longer flags `[FAIL: ordering]` for
  QUIC qos 4 (or the gate is intentional and documented).

#### Out of scope

- QUIC qos 1-3 ordering. Different code paths; revisit if needed.

---

### T14.14: runner -- diagnose same-host coordination glitch on later spawns

**Repo**: `runner/`.
**Status**: pending, filed 2026-05-12 by orchestrator after E14 smoke.

#### Background

The 2026-05-12 E14 smoke (`configs/two-runner-smoke-e14.toml`) hit an
asymmetric coordination glitch at the 8th of 9 spawns
(`zenoh-100x100hz-multi`):

- alice: stuck on `ready` barrier for zenoh-multi, timed out after 120s,
  exited EX_TEMPFAIL (75).
- bob: completed clock_sync (with unusually-high `rtt_ms=59.327` vs
  ~0.3 ms for prior spawns), ran the variant to success
  (`status=success, exit_code=0`), then timed out on the `done` barrier
  waiting for alice, exited EX_TEMPFAIL.

The 7 spawns before this all completed cleanly on both sides with
sub-millisecond clock_sync RTTs. The webrtc spawn (9th) never ran
because both runners aborted. The 59 ms RTT on bob's side suggests
serious scheduler / network-stack contention at the moment of the
zenoh-multi ready barrier — likely from prior spawns' lingering
TIME_WAIT sockets or zenoh's startup work.

The runner-runner discovery protocol seems to allow one side to
proceed past `ready` while the other side never sees the confirmation.

#### Scope

- Reproduce (likely flaky; may need multiple smoke runs).
- Determine whether the ready-barrier protocol drops UDP messages
  under same-host port pressure, or whether one side cleans up too
  quickly between spawns, or whether a clock_sync stall masks a
  protocol bug.
- Either harden the protocol (e.g. acknowledgement retries) or
  document a same-host limitation.

#### Acceptance criteria

- [ ] Root cause identified.
- [ ] Either a fix lands or a clear "known limitation" entry is added
      to `metak-shared/api-contracts/runner-coordination.md`.

#### Out of scope

- Cross-machine coordination. This bug is same-host specific.

---

### T14.15: variants/hybrid -- characterise Single-mode catastrophic latency at 10K msg/s

**Repo**: `variants/hybrid/` (research / analysis only; may produce no
code change).
**Status**: pending, filed 2026-05-12 by orchestrator after E14 smoke.

#### Background

The 2026-05-12 E14 smoke ran hybrid in Single mode at 100 vpt x 100 Hz
x qos 4 (10K msg/s symmetric, qos 4 = TCP path). Result:

```
hybrid-100x100hz-single  multi  19,996 receives/s  99.95%  p50 1.11ms p99 9.61ms  Loss 0.05%
hybrid-100x100hz-single  single    499.8 receives/s   3.46%  p50 8765.1ms p99 40643.6ms  Loss 96.54%
```

Receive throughput cratered from 20K msg/s to 500 msg/s. p50 latency
8.7 seconds, p99 latency 40 seconds. CPU pegged at 100%. This matches
T14.4 worker's smoke finding (3.90 / 3.99 % delivery) and is far more
catastrophic than websocket Single (which dropped throughput to ~13K
but kept 100% delivery and millisecond latency).

The behaviour is consistent with hybrid's inline single-threaded TCP
read loop being unable to keep up with the publisher even at 10K
msg/s on the same machine. The user's stated intent
("log all the messages being received with a very bad latency")
applies here: we ARE logging everything, the latency just balloons
to ~40 s on the tail.

#### Scope

This task is primarily INVESTIGATIVE. Questions to answer:
- Is the 40-s tail latency consistent across runs, or is it
  pathologically variable?
- At what tick_rate_hz x vpt does hybrid Single transition from
  "low latency" to "buffer-accumulation regime"?
- Is the binding bottleneck the inline TCP read in `poll_receive`,
  or the inline UDP path, or the blocking TCP write back-pressuring
  the publisher?
- Should hybrid be reworked to log-from-reader in Single mode too
  (effectively making Single mode a single-thread-with-callback
  model rather than fully inline)? Or do we accept that Single mode
  is for low-rate workloads only and document the threshold?

#### Acceptance criteria

- [ ] Investigation report appended to
      `metak-orchestrator/STATUS.md` documenting the answers.
- [ ] Decision: rework, document threshold, or leave as-is.
- [ ] If rework: separate implementation task filed.

#### Out of scope

- WebSocket Single mode (different ceiling, file separately if needed).
- Custom UDP Single mode (its smoke result was effectively identical
  to Multi at 10K msg/s, so it isn't motivated yet).

---

### T14.16: variants/custom-udp + variants/hybrid -- EOT survives reader saturation

**Repos**: `variants/custom-udp/`, `variants/hybrid/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the all-variants
qos2 smoke surfaced an asymmetric same-host timeout.

#### Background

The user ran `configs/two-runner-all-variants.toml` on localhost. The
`custom-udp-1000x100hz-qos2-multi` spawn (100 K msg/s symmetric UDP,
latest-value path) showed asymmetric stderr:

- **alice**: variant printed many
  `[custom-udp] multi: reader channel full -- dropping frame (receiver saturated)`
  lines; runner reported `status=timeout, exit_code=-1` at 120 s.
- **bob**: ran the same spawn cleanly to `status=success, exit_code=0`
  with no `reader channel full` messages.

Root cause (per orchestrator analysis): at 100 K msg/s same-host UDP,
scheduling luck consistently gives one side slightly faster publish
throughput; the other side's reader thread can't drain its bounded
mpsc fast enough; `try_send` fails and frames are dropped at the
`ReaderItem` enum level. **And because `Eot` shares the same bounded
mpsc with `Data`, the peer's EOT marker is also drop-on-fulled.** The
saturated side never observes EOT, waits 120 s for the runner's
done-barrier, gets killed.

The "data may drop, EOT must not" invariant is what's missing.

#### Scope

Both `variants/custom-udp/src/udp.rs` (or wherever T14.3 added the
reader-thread mpsc) and `variants/hybrid/src/reader.rs` (T14.4) share
the same architectural shape: one bounded `mpsc::sync_channel` for
all `ReaderItem` variants. The fix in both is:

1. **Split the channel into two**:
   - `data_tx` -- existing bounded mpsc, carries `ReaderItem::Data`
     only. Drop-on-full is acceptable here (UDP is best-effort by
     definition; for TCP qos4, kernel TCP backpressure already prevents
     the recv buffer from filling pathologically). Keep the existing
     `4 * vpt * peer_count` (custom-udp) / `4096`-fixed (hybrid) bound.
   - `lifecycle_tx` -- new **UNBOUNDED** `std::sync::mpsc::channel`
     for lifecycle items (`Eot`, `TcpPeerDropped`). Unbounded is safe
     because lifecycle items are infrequent (O(peers) per spawn for
     `Eot`, O(peers) total for `TcpPeerDropped` across a session).
     The reader thread `send`s (blocking, but it never blocks because
     unbounded) lifecycle items here.
2. **Nack handling** (custom-udp only -- hybrid has no NACK protocol):
   `ReaderItem::Nack` is used for QoS-3 NACK-based reliable UDP. Nacks
   under load matter for reliability. The cleanest treatment is to put
   Nacks on the lifecycle channel too -- they're rare (only fired on
   gap detection) and losing them would silently break QoS-3 reliability.
   Worker can choose: separate `nack_tx`, or fold into `lifecycle_tx`.
3. **Driver-side consumption**: the variant's `poll_receive` now drains
   both channels per call: drain `lifecycle_tx` first (priority --
   reading EOT promptly is the whole point), then drain `data_tx`.
   Existing budget logic stays for `data_tx`; lifecycle is unbounded
   so just drain to empty.
4. **stderr message refinement**: the `reader channel full -- dropping
   frame (receiver saturated)` line should make clear it refers to
   the DATA channel only -- e.g. rename to
   `data channel full -- dropping Data frame (receiver saturated)`.
   Operators will then trust that EOT loss is no longer a possibility
   if they see only this message.

#### Tests

Per variant:

- Unit: stub-variant test that floods Data items into a small (e.g.
  64-slot) `data_tx` while interleaving one `Eot` after N saturations.
  Driver-side `poll_receive` sequence must observe the Eot regardless
  of how many Data drops occur.
- Integration regression: existing two-runner same-host high-rate
  fixtures should NOT regress in delivery percentages, AND both runners
  should reach EOT cleanly with `status=success`. The failure case
  observed in the user's all-variants run -- one side `status=success`,
  the other `status=timeout` -- must become both `status=success`.

#### Validation (MANDATORY)

From workspace root:
- `cargo build --release -p variant-custom-udp -p variant-hybrid` clean.
- `cargo test --release -p variant-custom-udp -p variant-hybrid` all-green.
- `cargo test --release --workspace` no regression.
- Clippy and fmt clean for both crates.
- **End-to-end repro (MANDATORY)**: re-run the all-variants config's
  failing case, OR a focused smoke fixture, that puts both runners at
  ~100 K msg/s symmetric UDP qos2. Both runners must reach `eot_sent`
  AND `eot_received` and complete `status=success`. Capture stdouts +
  per-side write/receive counts in the completion report.

#### Acceptance criteria

- [ ] Channel split landed in custom-udp.
- [ ] Channel split landed in hybrid.
- [ ] `poll_receive` drains lifecycle channel first.
- [ ] stderr message disambiguates "data channel full" vs anything
      else.
- [ ] Same-host symmetric 100 K msg/s qos2 repro: both runners exit
      `status=success` with `eot_sent` + `eot_received` on both sides.
- [ ] Existing tests pass; clippy / fmt clean.
- [ ] Each variant's `CUSTOM.md` documents the two-channel architecture.

#### Out of scope

- Generalising T14.10's full log-from-reader pattern to UDP variants
  (separate task; T14.18 if motivated). T14.16 keeps Data on the mpsc
  for UDP-family because the analysis-time gap-detection / receive
  correlation logic in the driver needs to observe Data items for
  QoS-3 NACK-based reliability and for integrity checks.
- Generalising to websocket. T14.10 already moved Data off the mpsc;
  websocket's mpsc is already lifecycle-only.
- Changing what "EOT" means or how many EOT markers are sent. The
  current "send EOT, wait for peer EOT" semantics from E12 stay
  unchanged.

---

### T14.17: analysis -- classify timeout cause (deadlock vs eot-lost vs other)

**Repo**: `analysis/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the
all-variants qos2 smoke surfaced EOT-loss-induced timeouts that look
identical to deadlocks in the current integrity report.

#### Background

A `status=timeout` spawn outcome currently shows up in T11.5's integrity
report as a generic FAIL. But there are several distinct mechanisms:

1. **Deadlock / crash**: variant got stuck or panicked; JSONL ends
   mid-record; no `eot_sent` on the writer's own log.
2. **EOT-lost (this run)**: variant published happily, reached
   `eot_sent` on its own log, but the peer's reader saturated and
   dropped the EOT marker. Peer never sees EOT, peer times out.
   Asymmetric: one side completes, the other times out.
3. **Variant-rejected**: variant exited cleanly with a non-zero exit
   code BEFORE operate phase (e.g. unsupported QoS, port collision).
   T-impl.9's stderr-tail capture surfaces these.
4. **EOT-timeout (variant-side)**: variant correctly received some
   EOTs but not all, hit its own `--eot-timeout-secs`, exited cleanly
   with that diagnostic in JSONL.

The current analysis bins all four into a single FAIL. The operator
has to manually inspect stderr captures and JSONL files to tell them
apart. The receive-headline pivot (T11.5) is much more useful if the
analysis can also TYPE the failure.

#### Scope

In `analysis/integrity.py` (or wherever the integrity report is
assembled), add a `timeout_classification` field (or `failure_kind` if
we want to broaden the categorisation to non-timeout failures too).
Values:

- `"completed"` -- spawn ran to `status=success` and `eot_sent` +
  matching peer `eot_received` events are present.
- `"deadlock"` -- spawn `status=timeout`, writer's JSONL has no
  `eot_sent` event, AND the writer's JSONL ends mid-record (final
  line is incomplete JSON). Signals: process was killed mid-operate,
  no graceful exit.
- `"eot_lost"` -- spawn `status=timeout` on AT LEAST ONE side, but
  that side's writer JSONL DOES contain `eot_sent`. Strong signal
  that the side timed-out because the OTHER side never confirmed --
  i.e. EOT was published but not observed back. Bonus: if the
  asymmetric side (the one that succeeded) has many
  `reader channel full` lines in its stderr capture, attach
  `eot_lost_likely_saturation` as a sub-tag.
- `"variant_rejected"` -- spawn exited non-zero with a clear stderr
  message before operate (T-impl.9 stderr capture present and
  non-empty; JSONL has no `phase=operate` event). Look for known
  rejection patterns: `does not support single-threaded mode`,
  `does not support QoS`, etc.
- `"eot_timeout_internal"` -- variant exited cleanly with the
  `eot_timeout` event in JSONL (per E12 EOT protocol). Different
  from `eot_lost` because the variant itself decided to give up
  rather than being externally killed.
- `"unknown"` -- doesn't match any of the above. Worth investigating
  manually.

Add `timeout_classification` as a column in the integrity report and
as a tag in the per-spawn diagnostic detail. Update `metak-shared/
ANALYSIS.md` to document the new field.

#### Tests

Synthetic-fixture-driven unit tests:

- Synthesize a JSONL pair where the writer has `eot_sent` but the
  peer's JSONL doesn't have `eot_received`. Classification: `eot_lost`.
- Synthesize a JSONL ending with a truncated record and no
  `eot_sent`. Classification: `deadlock`.
- Synthesize a stderr capture containing
  `variant does not support single-threaded mode`.
  Classification: `variant_rejected`.
- Synthesize a clean run with `eot_sent` + `eot_received` on both
  sides. Classification: `completed`.
- Synthesize an `eot_timeout` event in JSONL. Classification:
  `eot_timeout_internal`.

#### Validation

- `pytest analysis/tests/` all-green.
- Run analysis against the existing
  `logs/all-variants-01-20260512_083021/` (the run that motivated
  this task). At least one `custom-udp-1000x100hz-qos2-multi` spawn
  should classify as `eot_lost` for the timed-out side.
- Run against the existing `logs/smoke-e14-20260512_040533/`. The
  successful 7 spawns should classify as `completed`.
- ruff format + ruff check clean.

#### Acceptance criteria

- [ ] `timeout_classification` field computed and reported.
- [ ] Five classification rules above are correctly implemented.
- [ ] All five new unit tests pass.
- [ ] Existing analysis tests still pass.
- [ ] `metak-shared/ANALYSIS.md` documents the new field.
- [ ] Re-run on existing failed-spawn datasets correctly types the
      failures (orchestrator review will spot-check).

#### Out of scope

- Plotting / diagram changes (separate E5/E6 follow-up if motivated).
- Auto-recovery / retry logic (analysis is offline; retry happens
  via `--resume` at the runner).
- Cross-spawn correlation (e.g. detecting whether the same kind of
  failure recurs across spawns). Each spawn classifies independently.
- Suggesting fixes per classification. Just type the failure; the
  human reads the table and decides.

---

### T14.18: variants/custom-udp + variants/hybrid -- TCP side-channel for EOT control

**Repos**: `variants/custom-udp/`, `variants/hybrid/`. Plus a small
contract addition in `metak-shared/api-contracts/eot-protocol.md`
documenting the side-channel pattern.

**Status**: pending, filed 2026-05-12 by orchestrator after the
all-variants run (`logs/all-variants-01-20260512_093124/`) showed
`custom-udp` Single mode losing EOT under symmetric UDP saturation
across all QoS levels (qos1-3 classified as `eot_lost`; qos4 as
`deadlock`).

#### Background

The user's run surfaced a real architectural failure mode in Single
mode: at 100K msg/s symmetric on a single host, the kernel UDP recv
buffer (default 8 MiB via T-impl.2 `tune_udp_buffers`) fills faster
than the inline `poll_receive` can drain it. New datagrams -- including
the EOT marker that's sent over the same multicast socket -- are dropped
at the **kernel** level before the application sees them. The driver
times out waiting for an EOT that was dropped before it ever reached
userspace. T11.5/T14.17 classified the asymmetric outcomes as
`eot_lost` exactly as designed.

Multi mode avoids this because the dedicated reader thread drains the
kernel buffer continuously. But Single mode is a first-class WASM
requirement (see `metak-shared/overview.md` cross-cutting goals), so
we can't just say "use Multi mode" -- the user explicitly needs Single
to work.

User clarification 2026-05-12 (recorded in conversation transcript):
*"single vs multi applies only to the message exchange per se ... we'd
be able to at least handle the control flow separately and solidly,
even if we need to runners to exchange control messages using tcp or
another thread."*

So the design space is wider than I'd been treating it: the data path
remains constrained by `threading_mode`, but control plane (EOT and
similar) can use any reliable transport regardless of QoS or threading.

#### Scope

Add a per-peer-pair **TCP control connection** to both `custom-udp` and
`hybrid` Multi+Single. The connection is QoS-independent, established
once at `connect()` time, and carries ONLY control messages (currently
just `eot_sent` / `eot_received`; future control bus point if needed).

Concrete design for the worker:

1. **Control port derivation**: each variant adds a new `--control-base-port`
   CLI arg (or reuses an existing `*_base_port` field with a fixed offset
   -- worker decides, but document the choice in CUSTOM.md). Same
   per-runner stride (`runner_stride = 1`) as Hybrid/QUIC/WebSocket so
   two same-host runners get different listen ports. **No qos stride**
   -- one control port per (runner, variant binary), not per (runner,
   variant, qos).

2. **Connection lifecycle**:
   - At `connect()`: lower-sorted-name peer is server (binds, listens),
     higher-sorted is client (connects). Same pairing convention as
     Hybrid TCP. Set `TCP_NODELAY` immediately.
   - One bidirectional control connection per peer pair. NOT per QoS.
   - At `disconnect()`: send a length-prefixed `bye` frame, half-close
     the write side, drain the read side until peer closes or
     `--eot-timeout-secs` elapses, then close. Same shape as websocket's
     EOT drain.

3. **EOT routing**: replace the current `signal_end_of_test` /
   `wait_for_peer_eots` paths so that:
   - The local `eot_sent` JSONL event is unchanged (still emitted by
     the variant when it signals).
   - The on-wire EOT marker is sent as a length-prefixed binary frame
     over the **control TCP connection** to each peer, NOT over the
     data transport.
   - Incoming EOTs are read from the control connection and surfaced
     via the existing `poll_peer_eots` trait method or its equivalent
     in `variant-base`. No more EOT-on-multicast or EOT-on-data-stream.

4. **Threading**:
   - Multi mode: a dedicated OS thread per peer's control connection
     reads incoming EOT frames and pushes them onto the existing
     `lifecycle_tx` channel (T14.16). The Data path is unchanged.
   - Single mode: control connection is non-blocking with a short
     `SO_RCVTIMEO` (~1 ms); the variant's inline `poll_receive` polls
     the control fd in addition to the data path. **No new threads in
     Single mode unless absolutely necessary** -- the per-tick budget
     for polling one extra socket is microseconds. Worker MAY use a
     dedicated control-only thread in Single mode too if non-blocking
     polling is too awkward; **document the choice** and confirm it
     doesn't make the variant's WASM-compatibility story worse for
     the Single mode (the user explicitly OK'd one aux thread for
     control plane regardless of data threading mode, per the
     2026-05-12 transcript).

5. **Per-variant adjustments**:
   - **custom-udp**: removes EOT-over-multicast for qos1-3 and the
     current EOT-on-TCP-stream marker for qos4 (or keeps qos4
     unchanged if the existing per-pair TCP is convenient -- worker
     decides). EOT path is now the control connection regardless of
     QoS.
   - **hybrid**: same change. Removes EOT-over-multicast for qos1-2,
     keeps qos3-4's reliable behaviour (or unifies it via the control
     connection -- preferred for cleanliness).

6. **Contract update**: `metak-shared/api-contracts/eot-protocol.md`
   gains a new section "Control side-channel (T14.18)" documenting
   that variants MAY use a dedicated TCP control connection for EOT.
   No change to the on-wire EOT semantics or the JSONL event types.

#### Tests

- Unit: control-connection pairing logic (which side connects vs
  accepts; port derivation).
- Unit: stub-variant scenario where the data path is fully saturated
  (multicast drops all data); EOT MUST still be observed via the
  control channel.
- Integration regression: existing two-runner same-host fixtures
  unchanged in delivery numbers, EOT exchange unchanged in success.
- New integration: a high-rate symmetric fixture at qos2 multi/single
  that previously caused `eot_lost`. Both runners must complete
  `status=success`, both sides must have `eot_sent` AND `eot_received`,
  T14.17 classifier must mark all rows `completed`.

#### Validation (MANDATORY)

From workspace root:
1. `cargo build --release -p variant-custom-udp -p variant-hybrid` clean.
2. `cargo test --release -p variant-custom-udp -p variant-hybrid` all-green.
3. `cargo test --release -p variant-custom-udp -p variant-hybrid -- --ignored`
   all-green (existing + new T14.18 regression).
4. `cargo test --release --workspace` all-green.
5. Clippy + fmt clean for both crates.
6. **End-to-end repro (MANDATORY)**: re-run the original failing
   case. Use `configs/two-runner-t1416-repro.toml` (qos2 multi 100K
   msg/s symmetric) as the smoke fixture, OR construct a fresh
   single-spawn fixture at qos2 single that previously classified
   `eot_lost`. Both sides must complete with `Timeout=completed` per
   the T14.17 classifier. Capture both runners' stdouts + per-side
   write/receive + EOT events in the completion report.

#### Acceptance criteria

- [ ] Control TCP connection landed in custom-udp.
- [ ] Control TCP connection landed in hybrid.
- [ ] EOT now uses the control channel for both variants regardless of QoS.
- [ ] Single-mode Single-thread-data-path constraint preserved (no new
      threads in Single mode unless explicitly documented and approved).
- [ ] Eot-protocol contract addition for the side-channel pattern.
- [ ] T14.17 classifier reports `completed` on previously-`eot_lost` fixtures.
- [ ] Existing tests pass; clippy / fmt clean.
- [ ] CUSTOM.md updates for both variants.

#### Out of scope

- Generalising to WebRTC (whose DataChannel uses SCTP; would need
  separate investigation of whether EOT loss is a real issue there).
- Generalising to WebSocket (already TCP-based, no EOT loss issue).
- Generalising to QUIC (post-T14.13 it has reliable streams that don't
  lose).
- Generalising to Zenoh (already declared Multi-only; T14.9 has its
  own path).
- Changing the on-wire EOT semantics or JSONL event types.
- The qos4 deadlock case observed in `logs/all-variants-01-20260512_093124/`
  -- different failure mode (alice JSONL truncated mid-record, likely
  TCP send wedge). File separately if needed.

---

### T14.19: characterise TCP-single-mode deadlock at high symmetric rates

**Repos**: `variants/custom-udp/`, `variants/websocket/` (data gathering);
possibly `variants/hybrid/` (comparison study).
**Status**: pending, filed 2026-05-12 by orchestrator after the
stress-e14 run (`logs/stress-e14-20260512_122xxx/`).

#### Background

The 2026-05-12 stress run at 1000 vpt x 100 Hz (100K msg/s symmetric)
revealed a new failure mode that the T14.18 control-channel fix does
NOT address:

- `custom-udp-1000x100hz-qos4-single`: status=timeout, classified
  **deadlock**. 172K writes, 180 receives, both sides killed by
  runner timeout.
- `websocket-1000x100hz-qos3-single` and `qos4-single`: same
  pattern, both deadlock.

Mechanism: in Single mode, `publish` and `poll_receive` share one
thread. At 100K msg/s symmetric TCP, the inline `publish` eventually
blocks on TCP send back-pressure (peer's recv buffer full). While
blocked, the thread can't call `poll_receive`, so the variant's own
recv buffer fills. Peer's writes start blocking too. Both sides
permanently wedge inside a blocking syscall. T14.18's TCP control
channel can't help because the variant thread is stuck in `send` --
EOT is never sent over the control channel.

#### Curious asymmetry

`hybrid-1000x100hz-qos4-single` SURVIVED (status=success,
classification=`completed`, 309K writes, 373 receives = 0.12 %
delivery). Hybrid handles the same TCP symmetric flood without
deadlocking; deliveries are catastrophically low, but the variant
exits cleanly. This is the user's "log everything with bad latency"
intent working AS DESIGNED.

Difference in implementation must explain this. Candidates:
- Hybrid uses a shorter TCP write timeout (or non-blocking writes
  with retry).
- Hybrid's `poll_receive` interleaves differently with `publish`.
- Hybrid's TCP send buffer is sized differently.

#### Scope

This is an **investigative task** first; the fix (if any) follows.

1. Audit how hybrid Single mode handles TCP write+read interleaving
   vs custom-udp Single mode vs websocket Single mode.
2. Identify the specific code-level difference that lets hybrid
   survive.
3. Decide: (a) port the hybrid pattern to the other two variants, or
   (b) document the architectural limit and accept that TCP Single
   mode is restricted to lower throughput regimes.
4. If (a): produce a small focused fix per variant (likely
   non-blocking write + retry with short SO_SNDTIMEO).
5. If (b): document the limit in each variant's CUSTOM.md and adjust
   the all-variants config's `default_timeout_secs` or remove
   high-rate Single mode from canonical fixtures.

#### Acceptance criteria

- [ ] Audit findings posted to STATUS.md identifying the
      hybrid-vs-others code difference.
- [ ] Decision: (a) implement, or (b) document.
- [ ] If (a): high-rate stress smoke shows custom-udp + websocket
      Single mode `completed` (delivery may be ~0% but no deadlock).
- [ ] If (b): each affected variant's CUSTOM.md documents the
      throughput cliff for Single mode.

#### Out of scope

- Improving high-rate Single mode delivery PERCENTAGES. The user's
  stated intent ("log all messages received with bad latency")
  doesn't promise high delivery; it promises that what arrives is
  logged. Achieving "completed" status is the only goal here.
- Zenoh and webrtc asymmetric timeouts. Different root causes.
- Multi mode behaviour. Unchanged in this task.

---

### T14.20: variants/websocket -- T14.18-style TCP control side-channel for EOT

**Repo**: `variants/websocket/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the
2nd stress-e14 run.

#### Background

Post-T14.19 the websocket Single-mode at 100K msg/s symmetric no
longer deadlocks (SO_SNDTIMEO breaks the wedge), but its EOT still
can't get through the saturated TCP/WS stream. T14.17 classifies as
`eot_timeout_internal` -- the variant correctly notices it can't
deliver EOT, gives up on its own timeout, and exits cleanly. This
is much better than deadlock, but it's not as clean as the
`completed` outcome that custom-udp Single qos4 achieves via T14.18's
TCP control side-channel.

#### Scope

Apply the T14.18 pattern to websocket: add a dedicated per-peer-pair
TCP control connection at `connect()` time, route EOT markers over
it, leave the data WebSocket untouched. Same shape as T14.18 in
custom-udp and hybrid.

Post-T14.20 the websocket Single qos3-4 spawns should classify
`completed` rather than `eot_timeout_internal` at 100K msg/s
symmetric.

#### Acceptance criteria

- [ ] WebSocket has a TCP control connection per peer pair,
      QoS-independent.
- [ ] EOT routes over the control connection in both Single and
      Multi modes.
- [ ] Post-T14.20 stress fixture run: websocket-1000x100hz-qos3-single
      and qos4-single classify `completed` (delivery may remain
      ~17%, that's not the bar).
- [ ] Existing tests pass; clippy / fmt clean.

#### Out of scope

- Changing the data path (WS frame format unchanged).
- Generalising further. T14.18 already covers custom-udp + hybrid;
  T14.20 covers websocket. WebRTC and Zenoh out of scope.
- T14.9 (Zenoh router-RPC) is its own deferred path.

---

### T15.10: runner -- investigate ready-barrier desync at high stress rate

**Repo**: `runner/`.
**Status**: pending, filed 2026-05-13 after T15.8 stress repro.

#### Background

After T15.8 landed (E15 cleanup), re-running
`configs/two-runner-stress-e14.toml` succeeded on the first 8 spawns
(all custom-udp 1000x100hz × multi+single) then alice hit:

```
FATAL: barrier 'ready' for variant 'hybrid-1000x100hz-qos1-multi'
  timed out after 120.0s waiting for peer(s): ["bob"]
```

T15.8 worker's audit of the surrounding context: alice's clock_sync
RTT for that spawn jumped to 56.9 ms (vs 0.3 ms baseline everywhere
else), indicating a transient host-scheduler stall. The runner-coord
code was untouched by T15.8.

Same class of issue as **T14.14** (UDP-coord desync at later spawns)
-- present in the codebase since the start but only manifests under
symmetric load. The smoke fixture (10K msg/s) doesn't trigger it;
the stress fixture (100K msg/s) does, intermittently.

#### Scope

Audit the ready / done barrier protocol vs T14.24's TCP-per-peer
resume_manifest fix. T14.24 documented that the UDP-coord protocol's
`MAX_MSG_SIZE = 4096` truncation only mattered for large payloads
(resume_manifest), so ready/done barriers stayed on UDP. But under
same-host symmetric flood there's apparently a different failure
mode (datagram loss, not truncation).

Two paths:

A. **Harden the UDP barrier**: add per-peer ACKs + retries on
   the ready/done barrier protocol. Reuses current transport.
B. **Move ready/done to TCP per peer pair**: mirror T14.24's
   resume_manifest fix. Larger change, removes the UDP-coord
   fragility entirely for control-plane messages. The
   user-suggested architecture (E15) already advocates for TCP
   control channels; this completes the application of that
   principle to all runner-coord messages.

Worker decides based on audit. Path B is recommended unless audit
finds a small UDP fix.

#### Acceptance criteria

- [ ] Audit findings posted to STATUS.md (which path is the
      transient failure: lost datagrams? kernel buffer fill?).
- [ ] Path chosen and implemented.
- [ ] Stress fixture (`configs/two-runner-stress-e14.toml`) runs
      end-to-end without ready/done barrier timeouts.
- [ ] Existing tests pass; clippy + fmt clean.

#### Out of scope

- T14.14's broader same-host coord investigation (still parked).
- T14.9 Zenoh router-RPC (separate path).

---

### T14.9: variants/zenoh -- single-threaded client via Zenoh-router sidecar (SPLIT)

**Status**: split 2026-05-14 into T14.9a + T14.9b after audit
(see commit `e70a01f` for the full audit). Original scope was
~600-1500 LOC and triggered two of the brief's hard-stop conditions
(non-cargo binary distribution, RPC surface design). Split allows
each sub-task to commit to one design decision in isolation.

**Sub-tasks**:
- **T14.9a**: sidecar lifecycle only (binary discovery, spawn/kill,
  port allocation, OS-level cleanup). ~250 LOC. Doesn't change the
  variant's user-visible capability.
- **T14.9b**: sync RPC client via `zenoh-plugin-rest` (HTTP PUT +
  SSE) using `ureq`. Depends on T14.9a. Flips capability to
  `[Single, Multi]`. ~350 LOC.

The audit-derived design choices (REST+SSE first cut, variant-owned
lifecycle, REST plugin's static linking) are recorded in
`metak-orchestrator/STATUS.md`'s "T14.9 -- AUDIT findings" section
(commit `e70a01f`). T14.9a / T14.9b worker prompts inherit those
choices unless explicitly revisited.

---

### T14.9a: variants/zenoh -- zenohd sidecar lifecycle (Single mode scaffolding)

**Repo**: `variants/zenoh/`.
**Status**: pending, filed 2026-05-14 from T14.9 audit split.

#### Background

T14.9's audit identified three independent risks. T14.9a handles
**sidecar lifecycle only**: binary discovery, spawn at connect, kill
at disconnect, port allocation, per-platform child-process cleanup.
The variant remains `[Multi]`-only at the capability surface — Single
mode publish/poll_receive still error out cleanly. T14.9b adds the
sync RPC client and flips the capability.

#### Scope

- **`zenohd` binary discovery**: check `ZENOHD_PATH` env var, then
  PATH, fail with a clear actionable error if neither finds the
  binary. Document the install command in CUSTOM.md
  (`cargo install zenohd --version 1.9.0`).
- **Sidecar lifecycle (variant-owned)**: at `connect`, spawn `zenohd`
  with a per-spawn-unique config (REST plugin enabled on a derived
  port; clean working dir; minimal stderr). At `disconnect`, send
  SIGTERM (or Windows equivalent) + wait, then SIGKILL if needed.
- **Port allocation**: derive REST plugin port + Zenoh listen port
  from a new `--zenoh-sidecar-base-port <u16>` CLI arg using the
  same runner-stride convention as T14.18 / T15.10 control ports.
- **Per-platform cleanup** (so a SIGKILLed variant doesn't orphan
  zenohd):
  - Windows: Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
  - Linux: `prctl(PR_SET_PDEATHSIG, SIGTERM)` in the child.
  - macOS: best-effort process-group kill.
- **`supported_threading_modes()` stays `&[Multi]`** in T14.9a.
  T14.9b flips it.
- **Single-mode publish/poll_receive**: return a clear error
  ("Single mode RPC client not yet implemented; pending T14.9b").
  This is intermediate scaffolding; users won't actually exercise
  Single until T14.9b lands.

#### Tests

- Unit: binary-discovery falls back through `ZENOHD_PATH` → PATH →
  clean error.
- Unit: port derivation per runner stride.
- Integration (`#[ignore]`): connect spawns a real `zenohd`, the
  REST plugin port responds to a simple HTTP GET, disconnect cleans
  it up.

#### Validation

- `cargo build --release -p variant-zenoh` clean (no new deps).
- `cargo test --release -p variant-zenoh` all-green (existing tests
  unchanged; the few new T14.9a tests added).
- Clippy + fmt clean.

#### Acceptance criteria

- [ ] `zenohd` binary discovery + clear-error path.
- [ ] Variant-owned sidecar lifecycle implemented + tested.
- [ ] Per-platform child cleanup (Job Object / `PR_SET_PDEATHSIG`).
- [ ] `--zenoh-sidecar-base-port` CLI arg.
- [ ] CUSTOM.md "Single mode scaffolding (T14.9a)" subsection
      documenting the install command + the staged plan (T14.9b
      adds the RPC client).
- [ ] No tokio additions; existing Multi mode unaffected.

#### Out of scope

- The sync RPC client (T14.9b).
- Flipping capability to `[Single, Multi]` (T14.9b).
- Multiple variants sharing a single sidecar (deferred — variant-
  owned is fine for the first cut).
- Runner-owned sidecar lifecycle (would require contract change;
  reconsider only if multiple variant families need sidecars later).

---

### T14.9b: variants/zenoh -- sync RPC client via REST+SSE (T14.9 completion)

**Repo**: `variants/zenoh/`.
**Status**: pending, filed 2026-05-14 from T14.9 audit split.
**Depends on**: T14.9a.

#### Background

With T14.9a's sidecar lifecycle in place, T14.9b wires the variant's
publish/poll_receive through the `zenoh-plugin-rest` HTTP+SSE
surface using sync Rust (`ureq` for HTTP PUT; `ureq` or a small
SSE reader for the subscription). The variant's Single mode becomes
genuinely single-threaded sync — no tokio in that code path. Flips
capability to `[Single, Multi]`.

#### Scope

- Add `ureq` as a dep, gated to Single mode if necessary (or always
  on; the binary already has tokio for Multi mode so the tokio-free
  story is about the call graph, not the dependency tree).
- **publish**: HTTP PUT to `http://localhost:<rest_port>/<key>` with
  the binary value as the body. Synchronous, blocking.
- **poll_receive**: dedicated OS thread (NOT tokio) that consumes
  the SSE stream from
  `http://localhost:<rest_port>/<key_expr>?_method=SUB`. Parses each
  SSE event into a `ReceivedUpdate`, pushes onto an
  `mpsc::sync_channel` that `poll_receive` drains. Same pattern as
  T14.10 (websocket log-from-reader) and T15.3 (progress_coord) —
  reuse the established invariants.
- **`supported_threading_modes()` flips to `&[Single, Multi]`**.
- **`connect(Single)`**: spawns `zenohd` (T14.9a) + opens the HTTP
  client + spawns the SSE reader thread. `connect(Multi)`: unchanged.
- **`disconnect`**: stops the SSE reader thread, closes the HTTP
  client, kills the sidecar (T14.9a).
- **Tokio-free verification in Single mode**: confirm via call-graph
  audit + a doc comment in `connect(Single)` listing the dependency
  chain (`ureq` → `rustls`/`native-tls` ... → no tokio). Document
  the verification method in CUSTOM.md.

#### Tests

- Unit: HTTP PUT request shape (mock server returns 200; assert the
  PUT body + headers).
- Unit: SSE parser handles complete and partial events correctly.
- Integration (`#[ignore]`): two-runner localhost regression in
  `Single` mode at a modest workload (10 vpt × 100 Hz = 1K msg/s,
  NOT the high-rate stress). Both runners reach `runner_idle_terminated`
  via the new sidecar-routed path.

#### Validation

- `cargo build --release -p variant-zenoh` clean.
- `cargo test --release -p variant-zenoh` all-green including the
  new `#[ignore]`-d Single-mode regression.
- `cargo tree -e features -p variant-zenoh` annotated in CUSTOM.md
  showing the Single-mode call graph.
- Clippy + fmt clean.
- **End-to-end smoke**: a focused fixture with one Zenoh entry at
  `threading_modes = ["single", "multi"]` at 10 vpt × 100 Hz qos1.
  Both modes complete with `runner_idle_terminated`. T14.9b doesn't
  claim to fix the qos3/qos4 stall — that's left to T15.11
  (variant-side watchdog).

#### Acceptance criteria

- [ ] HTTP PUT publish path via `ureq`.
- [ ] SSE poll_receive path via dedicated sync thread + mpsc.
- [ ] `supported_threading_modes() = &[Single, Multi]`.
- [ ] Connect / disconnect lifecycle composes T14.9a's sidecar with
      the RPC client.
- [ ] Tokio-free Single-mode call graph verified + documented.
- [ ] Two-runner regression passes in Single mode at modest workload.
- [ ] CUSTOM.md rewritten for the post-T14.9 architecture.

#### Out of scope

- High-rate symmetric workloads in Single mode (the deadlock is
  Zenoh-internal; T15.11 watchdog converts to clean exit).
- Swapping to `zenoh-plugin-remote-api` (WebSocket). REST+SSE is the
  first cut; revisit only if WASM target reveals concrete
  limitations of SSE over `fetch`/`EventSource`.
- Generalising the sidecar pattern across other variants.

---

### T11.5: analysis -- promote receive throughput to headline metric

**Repo**: `analysis/`.
**Status**: pending. **Can start IN PARALLEL with T14.1** (does not
depend on threading-mode dimension; only benefits from it once data
flows).

#### Background

Project goal per `metak-shared/overview.md`: "keep multiple peers in
sync under huge change diffs with lowest latency possible". The metric
that decides "in sync" is **receive throughput**, not write throughput.
Writers ship at requested rate almost always; receivers are the actual
sync bottleneck. The current analysis tool reports both throughputs as
peers in the summary table without highlighting which one matters.

#### Scope

- **Summary tables**: receive throughput leads the table. Per
  `(writer, receiver, variant, qos, threading_mode)` grouping:
  - Column 1: receive throughput (msg/s) -- **headline**
  - Column 2: write throughput (msg/s) -- "requested rate" context
  - Column 3: delivery percentage (receive / write)
  - Existing latency / jitter / loss columns follow
  - `threading_mode` is a new grouping dimension; when not present in
    logs (pre-T14.8 data), default to `"single"` and the column is
    constant for that dataset.
- **New metric: late-receive tail count**. For each
  `(writer, receiver, qos, variant)` group, count receives whose
  `receive_ts - write_ts` (clock-corrected per E8) exceeds 10x the
  99th-percentile latency of that group. Report as a count and a
  percentage of total receives. Add to the integrity report.
- Existing CLI tables continue to compute and print everything they
  print today; this task only changes ORDER and EMPHASIS. No metric
  is removed.
- Diagrams (E5/E6) are out of scope for this task; their reordering
  is a follow-up.
- Update `metak-shared/ANALYSIS.md` to document the new ordering and
  the late-receive-tail metric.

#### Tests

- Unit: existing test datasets continue to produce the same numeric
  values; only output ordering changes. Snapshot tests if they exist
  can be updated.
- Unit: synthetic dataset with known write/receive counts produces the
  expected receive-headline column order.
- Unit: late-receive-tail computation matches a hand-computed example.

#### Validation

- Run the analysis tool against the existing
  `logs/same-machine-all-variants-01-20260511_104934/` dataset and
  produce the new summary tables. Verify no value changed numerically
  (only column order). Save the new output for comparison.
- Run against any logs from this session
  (`logs/websocket-all-20260511_*` / `logs/websocket-first-only-*`) to
  verify the tool handles configs that aborted mid-run.

#### Acceptance criteria

- [ ] Receive throughput leads the summary table.
- [ ] Write throughput becomes the "requested rate" column.
- [ ] Late-receive-tail metric computed and reported.
- [ ] `threading_mode` column added; defaults to `"single"` for
  pre-T14.8 data.
- [ ] All existing metrics still computed.
- [ ] Pre-existing test datasets produce numerically-identical output
  (modulo ordering).
- [ ] `metak-shared/ANALYSIS.md` updated.

#### Out of scope

- Diagram reordering (separate E5/E6 task).
- New plot types beyond what E5/E6 already define.
- Adding columns for any metric not already computed.
- Re-baselining historical results.

---

### T14.21: analysis -- warn on job-run cases with incomplete samples

**Repo**: `analysis/`.
**Status**: pending, filed 2026-05-12 by orchestrator from a user
request: "when I run the analyse script I want to see a warning
whenever there is any job run case that has incomplete samples".

#### Background

The analysis pipeline already classifies spawns (T14.17 -- 
`timeout_classification`) and tracks per-`(variant, run)` late tails
(T11.5 -- `late_receives_tail_pct`), and the integrity table already
reports per-`(variant, run, writer, receiver)` `delivery_pct`. None of
these surface as an explicit warning today; they sit as columns in
the report and a tag on integrity rows. The operator has to scan the
tables row-by-row to find offenders.

This task adds an explicit, end-of-run summary of every job-run case
that contains "incomplete samples", printed prominently to stderr so
it cannot be missed.

#### What counts as "incomplete samples"

A job-run case warrants a warning when any of the following holds
(any combination -- multiple flags should be reported together for the
same case):

1. **Spawn did not finish gracefully.** The per-spawn
   `timeout_classification` is anything other than `"completed"`
   (i.e. one of `deadlock`, `eot_lost`, `variant_rejected`,
   `eot_timeout_internal`, `unknown`). Granularity: per
   `(variant, run, runner)` writer-side spawn. Note that each integrity
   row carries the writer-side classification today -- dedupe to one
   warning per spawn rather than one per `(writer -> receiver)` pair.

2. **Delivery shortfall.** Any integrity row with
   `delivery_pct < 100.0` -- **all QoS levels**, including the
   loss-tolerant ones (QoS 1 and 2). Even though the variant treats
   loss as acceptable for those QoS tiers, the user wants visibility
   into any case where samples are actually missing. Granularity: per
   `(variant, run, writer, receiver)` row.

3. **Late-tail present.** Any `PerformanceResult` with
   `late_receives_tail_pct > 0`. Granularity: per `(variant, run)`.

#### Scope

In `analysis/analyze.py` (or a new small helper module if it keeps
the entry point clean), after the integrity and performance tables
are produced and before the function returns:

1. Walk `integrity_results` and `performance_results` collecting all
   offending cases under the three rules above.
2. Aggregate by case identity:
   - For rule 1, the case key is `(variant, run, writer)` -- the
     spawn.
   - For rule 2, the case key is `(variant, run, writer, receiver)`.
   - For rule 3, the case key is `(variant, run)`.
   When more than one rule fires for overlapping cases, render the
   warnings adjacent (grouped by `(variant, run)`) so an operator can
   see the full picture for a given run at a glance. Exact grouping
   layout is the worker's call; favour readability.
3. Print to **stderr** (not stdout -- the tables on stdout must
   remain pipe-clean). Each offending case is one line prefixed with
   `WARN:`. Include the case identity (variant / run / runner or
   writer->receiver as applicable) and which rule(s) tripped, with the
   concrete numbers (classification string, delivery pct, late-tail
   pct).
4. Print a final aggregate line: `WARN: <N> job-run case(s) with
   incomplete samples (<n1> not-completed, <n2> delivery shortfall,
   <n3> late tail).` When `N == 0`, print nothing (no noise on clean
   runs).
5. Do **not** change the process exit code -- a warning is not an
   error. The user explicitly called this a "warning".
6. Suggested ordering in the output: warnings should appear AFTER the
   integrity + performance tables (so they're the last thing the user
   sees on stderr) but BEFORE the diagram-saved messages, OR right
   before the function returns. Worker's call -- whichever reads more
   naturally.

#### Tests (in `analysis/tests/`)

Add a focused unit test module (e.g. `tests/test_incomplete_warnings.py`)
covering, with synthetic `IntegrityResult` / `PerformanceResult`
fixtures (no need to round-trip through real JSONL):

- Clean run (all rows `completed`, all `delivery_pct == 100.0`, all
  `late_receives_tail_pct == 0.0`) -> no warning output, no aggregate
  line.
- Single not-completed spawn -> one `WARN:` line + aggregate line
  showing 1 not-completed.
- Single delivery shortfall on QoS 1 -> one `WARN:` line that
  *includes* the QoS-1 case (proving loss-tolerant QoS is not
  filtered out).
- Single late-tail case -> one `WARN:` line.
- Combined case (one `(variant, run)` triggers rules 1 + 2 + 3) ->
  warnings for that case appear grouped, and the aggregate counts all
  three.
- Spawn produces multiple integrity rows (one writer, two receivers)
  with non-completed classification: rule 1 dedupes to ONE warning for
  the spawn, not two. Rule 2 still produces one warning per
  `(writer, receiver)` row if both have shortfalls.

Use `capsys` (or `capfd`) to capture stderr in the tests. Do not
assert on exact whitespace -- assert on substring presence and line
counts so the tests stay robust to formatting tweaks.

Also extend `tests/test_integration.py` lightly so the end-to-end
integration run against
`logs/same-machine-20260430_140856/` exercises the new code path
(no behaviour assertion required beyond "does not crash and produces
either zero or some `WARN:` lines on stderr").

#### Validation against reality

Run the analyse script against a real logs directory and verify the
warnings line up with what the integrity / performance tables report:

```
# PowerShell (Windows) -- user's environment
cd analysis
python analyze.py ..\logs\same-machine-all-variants-01-20260511_104934 --summary 2>&1 | Select-String 'WARN:'
```

Show the captured `WARN:` lines in the completion report and
cross-check at least one of them against the corresponding row in
the integrity / performance tables (e.g. `WARN: ... delivery 87.3%`
must match the `delivery_pct=87.3` cell on the cited row).

If no recent logs are available locally, run the worker's own test
suite (which exercises real fixture logs) and report the warning
output for the integration test as the validation evidence.

#### Acceptance criteria

- [ ] Running `analyze.py` on a clean dataset prints no `WARN:`
      lines.
- [ ] Running it on a dataset with any of the three triggers prints
      at least one `WARN:` line per offending case + a final
      aggregate line.
- [ ] Warnings go to stderr, not stdout. Stdout tables unchanged.
- [ ] Loss-tolerant QoS (1, 2) is included in delivery-shortfall
      detection.
- [ ] One spawn = one rule-1 warning (deduped across receivers).
- [ ] Exit code unchanged (still 0 on warnings; still 1 on the
      pre-existing error paths).
- [ ] New `tests/test_incomplete_warnings.py` covers the cases above.
- [ ] `pytest -v` passes for `analysis/tests/`.
- [ ] `ruff format --check .` and `ruff check .` clean.
- [ ] STATUS.md updated with completion report including captured
      `WARN:` lines from a real-logs validation run (or, fallback,
      from the integration test).

#### Out of scope

- Changing the integrity / performance tables themselves.
- Promoting any of these conditions to errors / non-zero exit codes.
- Adding new metrics or new triggers beyond the three above.
- Changes to other tools (runner, variants, plots).

---
### T14.22: variants/custom-udp -- port connect_with_retry to qos4 TCP setup

**Repo**: `variants/custom-udp/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the
all-variants-01-20260512_152156 run.

#### Background

The run's `custom-udp-100x1000hz-qos4-multi` spawn produced asymmetric
stderr:

- **alice**: `[custom-udp] error: multi: timed out waiting for 1 TCP
  peer(s) on 0.0.0.0:19830`
- **bob**: `[custom-udp] warning: failed to connect to peer
  127.0.0.1:19830: ... target machine actively refused it (os
  error 10061)` -- single attempt, no retry; 3M writes accumulated
  in bob's JSONL but never delivered.

Same-host TCP startup race: bob's `connect()` fired before alice's
`listen()` was accepting. ECONNREFUSED on first attempt. Custom-udp
gives up immediately and the variant proceeds in disconnected state.

Hybrid handled this exact race in T14.4 via `connect_with_retry`
(`variants/hybrid/src/tcp.rs:476-509`). Custom-udp's qos4 TCP setup
doesn't have the equivalent retry loop. The 1000 Hz tick rate may
exacerbate the race (variant has less time to retry naturally).

#### Scope

Port the `connect_with_retry` pattern from hybrid to custom-udp's
qos4 TCP setup. Same shape:
- Retry budget 10-30 s (T14.4 follow-up commit `c163042` bumped
  hybrid's budget to 30 s after observing transient delays in
  practice; pick the same).
- Retry on `ConnectionRefused` only. Other errors propagate.
- Short sleep between attempts (~50 ms).

Update `variants/custom-udp/CUSTOM.md` with a "Startup retry (T14.22)"
subsection.

#### Tests

- Unit: stub TCP setup that refuses the first N connect attempts then
  accepts. The retry loop should succeed within the budget.
- Integration: focused two-runner localhost fixture at qos4 multi
  100x1000hz exercising the race. Both runners must complete
  `status=success`.

#### Validation (MANDATORY)

- `cargo build --release -p variant-custom-udp` clean.
- `cargo test --release -p variant-custom-udp` all-green.
- Clippy + fmt clean.
- End-to-end repro on a focused fixture at qos4 multi 100x1000hz:
  both runners complete `status=success`.

#### Acceptance criteria

- [ ] `connect_with_retry`-equivalent pattern in custom-udp's qos4
      TCP setup.
- [ ] Existing tests pass.
- [ ] Focused regression test passes.
- [ ] CUSTOM.md updated.

#### Out of scope

- Generalising to other variants. Hybrid already has the pattern.
- The disconnected-state behaviour where bob accumulated 3M writes
  with no peer. Separate question (should the variant fail-fast on
  no peers? Or keep trying?). Filed if motivated.

---

### T14.23: runner -- resume.rs requires completion marker for "complete" classification

**Repo**: `runner/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the
all-variants-01-20260512_152156 resume failure.

#### Background

The user tried `--resume` on the previous all-variants run. Both
runners' manifests computed but disagree on the count:

```
[runner:alice] resume: local manifest has 191 complete job(s)
[runner:bob]   resume: local manifest has 192 complete job(s)
```

The single disagreement is `zenoh-max-qos4-multi`. Bob has a
3.4M-event JSONL (variant ran for many seconds before runner-timeout
killed it; the file is non-empty but contains NO `eot_sent` and NO
`phase=silent` event). Alice has no log file at all (variant exited
before opening the log).

`runner/src/resume.rs:92-104` currently classifies any **non-empty**
file as "complete":

```
if meta.len() == 0 {
    // crashed prior attempt -- delete and exclude
} else {
    complete.insert(name.clone());  // counted as complete
}
```

This is too lenient. Bob's `zenoh-max-qos4-multi` is non-empty but
crashed mid-spawn -- it should NOT be classified as complete.

#### Scope

In `runner/src/resume.rs::compute_local_manifest`, tighten the
classification:

1. **Empty file** (size == 0): delete + exclude. Unchanged.
2. **Non-empty file** containing `"event":"eot_sent"` (or
   `"phase":"silent"` -- worker picks one and documents): classify
   as complete.
3. **Non-empty file MISSING the completion marker**: classify as
   crashed mid-spawn. Delete + exclude. Add the deleted path to
   `LocalManifest::deleted_empty` (or a new `deleted_partial` field
   for clarity in the operator-facing stderr message).

The marker scan can be cheap: read up to the last ~4 KiB of the
file and look for the marker substring. Avoid loading the whole file
(can be GB at high rates).

#### Tests (`runner/src/resume.rs::tests`)

- Synthetic JSONL files exercising each classification:
  - Empty: deleted + excluded.
  - Non-empty with `eot_sent` near EOF: complete.
  - Non-empty without `eot_sent` (partial -- e.g. just `write` events):
    deleted + excluded.
  - Non-empty file with `eot_sent` only in early bytes (unlikely in
    practice but documents the tail-scan policy -- worker decides
    whether to fall back to a full scan).
  - Missing file: excluded (unchanged).

#### Validation (MANDATORY)

- `cargo test --release -p runner` all-green.
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.
- **Real-data regression**: re-run `--resume` on
  `logs/all-variants-01-20260512_152156/` (the actual failure case).
  Both runners should now compute the same manifest count (191 jobs
  each, excluding the partial `zenoh-max-qos4-multi` which gets
  deleted from bob's side). The barrier may still time out (T14.24)
  but the manifests will agree on content.

#### Acceptance criteria

- [ ] `compute_local_manifest` requires the completion marker.
- [ ] Partial files are deleted + excluded just like empty files.
- [ ] All new unit tests pass.
- [ ] Existing runner tests pass.
- [ ] Manual resume on the real failure case shows alice and bob
      agree on the manifest count.

#### Out of scope

- The resume barrier-timeout bug (T14.24).
- Marker choice debate beyond "pick one and document".

---

### T14.24: runner -- harden resume_manifest barrier against UDP-coord desync

**Repo**: `runner/`.
**Status**: pending, filed 2026-05-12 by orchestrator after the
all-variants-01-20260512_152156 resume failure.

#### Background

Even with T14.23 fixing the manifest content, the `resume_manifest`
barrier itself is timing out:

```
[runner:alice] FATAL: barrier 'resume_manifest' timed out after 120.0s
  waiting for peer(s): ["bob"] -- exiting 75 (EX_TEMPFAIL)
[runner:bob]   FATAL: barrier 'resume_manifest' timed out after 120.0s
  waiting for peer(s): ["alice"] -- exiting 75 (EX_TEMPFAIL)
```

This is the same UDP-coordination-protocol weakness that T14.14
documents (the `ready` and `done` barriers can desync on the same
host). The `resume_manifest` barrier is implemented on the same
UDP-multicast-with-acks protocol and is just as vulnerable.

Symptom: both runners say they're waiting for the OTHER one. Both
have computed their manifests locally. Neither sees the other's
manifest broadcast within the 120 s window.

#### Scope

INVESTIGATIVE first; the fix follows.

1. Audit the resume_manifest barrier's protocol. Find it in
   `runner/src/protocol.rs` (or wherever the barrier impl lives).
   Compare to the ready/done barrier protocol shape. Identify
   whether they share code, and where the desync window is.
2. Determine the failure mode under same-host load. Lost UDP
   datagrams (kernel buffer overflow)? TIME_WAIT pollution from
   prior runs? Race between socket setup and broadcast? Compare
   timing against the prior all-variants run that DID complete its
   resume_manifest barriers (if any).
3. Decide:
   - **Path A**: harden the barrier with explicit per-peer ACKs +
     retries until convergence (or a long-but-finite deadline).
   - **Path B**: switch the resume_manifest exchange to TCP per peer
     pair, mirroring T14.18's variant-EOT control channel. The
     barrier is rare (once per resume invocation) so reliability >>
     speed. Cleaner conceptually because it applies the
     reliable-control-channel lesson at the runner layer too.

Path B is recommended unless the audit finds a small UDP fix.

If T14.24 finds the issue is broader than just the resume_manifest
barrier (i.e. the ready/done barriers have the same fragility), file
a separate generalisation task -- don't bundle. T14.14 already
documents the same root-cause family for ready/done.

#### Acceptance criteria

- [ ] Audit findings posted to STATUS.md.
- [ ] Path chosen + implemented.
- [ ] `--resume` on the real failure dataset
      (`logs/all-variants-01-20260512_152156/`) converges. No barrier
      timeout.
- [ ] Existing tests pass; clippy + fmt clean.

#### Out of scope

- Generalising the fix to ready/done barriers (file separately).
- T14.14's broader same-host coord investigation.

---
### T15.1: variant-base -- progress emission to stdout

**Repo**: `variant-base/`.
**Status**: pending. Foundational; gates T15.2-T15.9.

#### Background

E15 introduces stdout-based progress emission so the runner can
observe variant state without reaching inside the variant.

#### Scope

- New CLI arg: `--progress-stdout-interval-ms <u32>` (default `1000`;
  `0` disables emission entirely for back-compat).
- New stdout protocol: variant emits one JSON line per interval,
  flushed immediately, shape:
  ```
  {"event":"progress","ts":"RFC3339-with-ns","phase":"connect|stabilize|operate|eot|silent|done","sent":<u64>,"received":<u64>,"eot_sent":<bool>,"eot_received":<bool>}
  ```
  - `phase` reflects the current protocol-driver phase.
  - `sent` / `received` are monotonic per-spawn aggregates across all
    peers. Per-peer breakdown stays in JSONL (`writer` field on
    `receive` events).
  - `eot_sent` flips to `true` once the variant has emitted the
    `eot_sent` event to its JSONL (per T15.5 -- when local idle
    fires).
  - `eot_received` flips to `true` once the variant has observed all
    expected peer EOTs. Under E15 the variant-base default behaviour
    is that this flag is set when the runner-coord channel tells the
    variant (or when the variant infers it from its own
    received-counter idling). T15.4 / T15.5 finalize the exact
    trigger.
- Thread-safe emission: stdout writes from the protocol driver MUST
  NOT interleave with anything else printed to stdout. Variant-base
  emits all progress; no concrete variant prints to stdout.
- Existing stderr output (variant build banner, T-impl.1 stderr
  capture) is unchanged.
- The JSON line is the ONLY stdout output from a variant. Any other
  diagnostic that variants emit today via `println!` should be
  ported to `eprintln!` (variant code audit). This guarantees the
  runner can parse stdout line-by-line as JSON.

#### Tests

- Unit: `progress_emitter` (or wherever the emitter lives) produces
  exactly one line per interval, well-formed JSON, monotonic ts.
- Unit: counters increment when expected (stub publisher driving
  fake writes/receives).
- Unit: `--progress-stdout-interval-ms 0` disables emission.
- Integration: `variant-dummy` smoke -- run with the arg, capture
  stdout, assert ~N lines per second, each parses as JSON, phase
  field transitions through `connect -> stabilize -> operate -> ...`.

#### Validation (MANDATORY)

- `cargo build --release -p variant-base` clean.
- `cargo test --release -p variant-base` all-green.
- `cargo test --release --workspace` all-green (no other variants
  should break -- they don't print to stdout in normal operation;
  if they do, audit + fix).
- Clippy + fmt clean.

#### Acceptance criteria

- [ ] `--progress-stdout-interval-ms` CLI arg added with default 1000.
- [ ] Progress JSON emission per interval implemented.
- [ ] Phase field correctly reflects driver state.
- [ ] Counters are accurate.
- [ ] Unit tests pass.
- [ ] Workspace tests pass.
- [ ] `metak-shared/api-contracts/variant-cli.md` documents the new
      arg and the stdout JSON schema (worker writes the contract
      update).

#### Out of scope

- Per-peer counter breakdown in the stdout (stays in JSONL only).
- Runner-side parsing (T15.2).
- Variant-side idle detection (T15.5).
- Removing the on-wire EOT exchange (T15.8 cleanup).

---

### T15.2: runner -- read child stdout, parse progress events

**Repo**: `runner/`.
**Status**: pending. Depends on T15.1.

#### Scope

- Per-spawn: capture child stdout via `Stdio::piped()` (mirror of
  T-impl.1's stderr-piped approach but for stdout).
- Read line-by-line on a dedicated thread per child. Parse each line
  as JSON (the T15.1 progress schema). Non-JSON lines are surfaced
  as a warning but do NOT abort the spawn.
- Maintain `LocalProgressTracker { runner, spawn, phase, sent,
  received, eot_sent, eot_received, last_sent_change_ts,
  last_received_change_ts }`. Update on each parsed progress event.
- Expose a snapshot API for T15.3 (runner-coord) to broadcast.
- Persist nothing to disk (the JSONL log is the source of truth for
  analysis; the stdout stream is for live runner control only).
- Graceful shutdown: when the child exits, the stdout reader thread
  joins cleanly.

#### Tests

- Unit: a stub child binary that emits a known sequence of progress
  events; assert the tracker state matches.
- Unit: malformed JSON line is ignored (with a warning) and does
  not break the reader.
- Integration: spawn `variant-dummy` (post-T15.1) with progress
  emission; assert the runner's tracker reflects the emitted state.

#### Validation

- Standard cargo gates.
- Integration test with real `variant-dummy` produces the expected
  tracker state.

#### Acceptance criteria

- [ ] Child stdout captured per spawn.
- [ ] Progress events parsed and tracked.
- [ ] Malformed-line tolerance.
- [ ] Unit + integration tests pass.

---

### T15.3: runner-coord -- exchange RemoteProgressSnapshot every ~1s

**Repo**: `runner/`. Contract touch: `runner-coordination.md`.
**Status**: pending. Depends on T15.2.

#### Scope

- Extend the runner-runner coord channel to exchange per-spawn
  progress snapshots between runners.
- Use the **same TCP-per-peer transport that T14.24 introduced for
  resume_manifest**. Long-lived; one connection per peer pair;
  reliable; large-payload friendly.
- New protocol message: `ProgressUpdate { runner, spawn,
  sent, received, eot_sent, eot_received, phase, ts }`.
- Cadence: ~1 second per active spawn. Buffer / batch ok.
- Worker decides whether to reuse the resume_manifest TCP port or
  open a new long-lived connection. Reusing is simpler if the
  connection can be kept open across spawns; otherwise a new
  dedicated port (e.g. `base + offset`) per pair.
- Update `metak-shared/api-contracts/runner-coordination.md` with
  the new message + the cadence.

#### Tests

- Unit: serialization roundtrip.
- Integration: two runner subprocesses; one's tracker becomes visible
  to the other within `2 * interval` seconds.

#### Acceptance criteria

- [ ] `ProgressUpdate` message defined + serialized.
- [ ] Cross-runner exchange implemented over TCP per peer pair.
- [ ] Each runner has a `RemoteProgressView { peer -> spawn -> snapshot }`.
- [ ] Contract updated.
- [ ] Tests pass.

---

### T15.4: runner -- phase-aware termination state machine

**Repo**: `runner/`.
**Status**: pending. Depends on T15.2 + T15.3.

#### Scope

- Replace the current per-spawn wall-clock timeout as the PRIMARY
  termination signal with phase-aware activity detection. Keep a
  high `max_spawn_secs` (default 300) as a fallback safety net.
- Per-spawn state machine driven by `LocalProgressTracker` +
  `RemoteProgressView`:
  - **`stabilize`**: variant transitions naturally after
    `stabilize_secs` (its own clock); runner just observes.
  - **`operate`**: when local AND every remote runner reports its
    variant's `(sent, received)` counters have not advanced for
    `operate_idle_secs` (default 5), runner notes "operate done
    locally". When ALL runners agree (via T15.3's coord exchange),
    the runner does not need to do anything explicit -- the variant
    is independently observing the same idle condition (T15.5) and
    will transition itself.
  - **`silent`**: short drain; variant transitions to `done`
    naturally.
  - **`done`**: variant exits.
  - At any phase: if `max_spawn_secs` elapses without `done`, kill
    the child (existing behaviour, just a fallback).
- New CLI / config args: `operate_idle_secs` (default 5),
  `max_spawn_secs` (default 300), `progress_stdout_interval_ms`
  (default 1000, passed through to the variant).

#### Tests

- Unit: state-machine transitions given synthetic progress streams.
- Unit: safety net kills the spawn after `max_spawn_secs`.
- Unit: `operate_idle_secs` actually fires when idle.
- Integration: two-runner localhost run shows activity-based
  termination of variant-dummy with no wall-clock timeout fires.

#### Acceptance criteria

- [ ] State machine implemented per scope.
- [ ] Tests pass.
- [ ] Existing `default_timeout_secs` remains as the
      `max_spawn_secs` fallback (with the new name or kept as alias).

---

### T15.5: variant-base -- variant-side idle detection + JSONL eot_sent emission

**Repo**: `variant-base/`.
**Status**: pending. Depends on T15.1.

#### Scope

- In the protocol driver's operate phase, observe the variant's own
  `sent` and `received` counters. When both have not advanced for
  `operate_idle_secs` (default 5; same as runner's), emit
  `eot_sent` to the JSONL log (the marker analysis needs) and
  transition internally to `silent` phase.
- Progress stdout emission continues; `eot_sent` flips to `true` on
  the next progress event after the transition.
- `eot_received` flag: simplified. Variant doesn't need to know
  whether peers have stopped -- the runner observes that and
  terminates the spawn naturally. Variant can set `eot_received`
  optimistically once its OWN `eot_sent` fires (the runner is the
  authoritative observer of cross-peer agreement).
- No on-wire EOT exchange. The transport carries data only.
- Reuse of the existing `eot_sent` JSONL event type for backwards
  compatibility with analysis (T11.5 / T14.17).

#### Tests

- Unit: stub variant whose received counter idles for >5s in
  operate; assert `eot_sent` event emitted to logger and phase
  transitions to silent.
- Unit: variant whose received counter keeps incrementing; assert
  NO premature `eot_sent`.
- Integration: variant-dummy lifecycle exits cleanly via
  idle-detection path, no on-wire EOT involved.

#### Acceptance criteria

- [ ] Variant-side idle detection implemented.
- [ ] `eot_sent` JSONL event emitted on idle transition.
- [ ] Phase transitions to `silent` on idle.
- [ ] No on-wire EOT messages sent (variants stop calling
      `signal_end_of_test` / `wait_for_peer_eots` -- the methods
      may remain on the trait until T15.8 cleanup; default impls
      become no-op).

---

### T15.6: analysis -- adapt T11.5 + T14.17 to the new architecture

**Repo**: `analysis/`.
**Status**: pending. Depends on T15.5 (variants must emit `eot_sent`
to JSONL on idle for analysis to keep working).

#### Scope

- T11.5 receive-headline pivot: already uses `eot_sent.ts` to bound
  the operate window. With T15.5 the variant still emits `eot_sent`
  on its own idle transition, so this keeps working unchanged.
- T14.17 timeout classifier: most current classifications (`eot_lost`,
  `eot_timeout_internal`, `deadlock` via JSONL truncation) become
  rare or impossible because the runner terminates spawns cleanly via
  T15.4. Add a new classification `runner_idle_terminated` for the
  common clean-exit case in the new architecture.
- Keep all existing classifications -- they're still useful for
  pre-E15 datasets and for the rare safety-net `max_spawn_secs`
  kill in the new architecture.
- Add a small test on a synthetic dataset matching the new
  JSONL shape (idle-detection emitted eot_sent, no eot_received
  from peer).

#### Acceptance criteria

- [ ] `runner_idle_terminated` classification added with rule:
      writer's JSONL has `eot_sent`, peer's JSONL has matching
      `eot_received` or its own `eot_sent`, both exit cleanly.
- [ ] Synthetic-fixture tests pass.
- [ ] Existing classifications still work on pre-E15 datasets
      (backwards compat).

---

### T15.7: contracts -- variant-cli, runner-coord, jsonl-log-schema, architecture

**Repo**: `metak-shared/`.
**Status**: pending. Should land alongside T15.1 - T15.5 as those
implementations stabilise.

#### Scope

- `metak-shared/api-contracts/variant-cli.md`:
  - Document `--progress-stdout-interval-ms` arg.
  - Document the stdout JSON schema.
- `metak-shared/api-contracts/runner-coordination.md`:
  - Document the new `ProgressUpdate` message + cadence.
- `metak-shared/api-contracts/jsonl-log-schema.md`:
  - Clarify that `eot_sent` is now emitted by the variant on its
    own idle detection (not via on-wire EOT exchange).
- `metak-shared/architecture.md`:
  - Retract the "No IPC between runner and variant" sentence.
  - Update the data-flow section to note that the runner reads
    one-way progress events from the variant's stdout.
  - Rationale update: one-way variant->runner stdout is
    observational only; the runner does NOT direct the variant.

#### Acceptance criteria

- [ ] All four contract docs updated.
- [ ] Changes are consistent with the implementation that landed
      in T15.1-T15.5.

---

### T15.8: cleanup -- remove EOT control channels + on-wire EOT after E15 stabilises

**Repo**: `variant-base/`, all `variants/*/`, plus contract retractions
in `metak-shared/api-contracts/eot-protocol.md` (mark historical).
**Status**: pending. **DEFERRED until T15.1 - T15.7 land and prove
stable in the stress fixture.** Do NOT start until orchestrator
greenlights.

#### Scope

- Remove `signal_end_of_test` / `wait_for_peer_eots` from the
  `Variant` trait (and from all six variants' implementations).
- Remove the per-variant TCP control connections introduced by:
  - **T14.18** (custom-udp's `controltcp.rs`, hybrid's `controltcp.rs`).
  - T14.20 was cancelled before landing so no code to remove.
- Remove `--control-base-port` CLI arg from custom-udp + hybrid.
- Remove `--eot-timeout-secs` CLI arg from variant-base.
- Mark `eot-protocol.md` "Control side-channel (T14.18)" section as
  historical.
- Remove or simplify the per-variant CUSTOM.md "Threading modes"
  EOT-routing subsections.

#### Acceptance criteria

- [ ] Trait surface cleaned.
- [ ] Control TCP connections removed.
- [ ] CLI args removed.
- [ ] Contract docs updated (historical markers).
- [ ] All tests pass after removal.
- [ ] Stress fixture re-run confirms no regression.

#### Out of scope

- Anything outside the "remove now-redundant code" theme. New
  features go in their own tasks.

---

### T15.9: test adaptation -- existing tests under the new architecture

**Repo**: cross-cutting.
**Status**: pending. Should be incremental as T15.1-T15.5 land.

#### Scope

- Audit existing integration tests across `runner/`, `variant-base/`,
  and `variants/*/` for any that assume:
  - On-wire EOT exchange happens (some tests may assert EOT marker
    is observed by the receiver via the transport).
  - Per-spawn wall-clock timeout fires under specific conditions.
  - Variant control TCP connection is established at connect time.
- Update each affected test to either:
  - Assert the equivalent E15-architecture state (progress events
    received; runner-coord agreement reached; idle detection fires).
  - Or be removed if it tested behaviour that no longer exists.
- New tests required at the state-machine level (T15.4) -- those
  are filed inside T15.4 itself.

#### Acceptance criteria

- [ ] Workspace tests pass post-E15.
- [ ] No test is silently skipped or muted to accommodate the new
      architecture.
- [ ] Test inventory documented in STATUS.md (which tests were
      updated vs removed).

---

### T15.11: variant-base -- watchdog thread for internal-stall self-exit

**Repo**: `variant-base/`.
**Status**: pending, filed 2026-05-14 after the post-T15.10 stress
analysis showed Zenoh qos3/qos4 multi still classifying as `deadlock`
because alice's variant process wedges inside Zenoh's library code
and gets killed by the runner's safety-net (JSONL truncated mid-record).

#### Background

T15.5 added variant-side idle detection inside the protocol driver's
operate loop. But if the driver thread itself is BLOCKED inside a
transport library call (e.g. Zenoh's internal async runtime
deadlocking under symmetric flood), the idle detector never runs --
the driver loop doesn't tick. T15.1's `ProgressEmitter` runs on its
own thread so progress lines keep going out, just with frozen
counters. The runner eventually trips the `max_spawn_secs` safety
net and `TerminateProcess`/`SIGKILL`s the variant, leaving JSONL
truncated mid-record. T14.17 classifies the outcome as `deadlock`.

A small watchdog thread inside `variant-base` can convert this
failure mode from "runner kills the variant + truncated JSONL" to
"variant self-exits cleanly with a clear diagnostic + JSONL flushed
via Drop impls":

#### Scope

- New OS thread spawned at variant connect time, parented by the
  protocol driver. Lives until `disconnect`.
- The watchdog reads the same monotonic counters the
  `ProgressEmitter` already exposes (sent / received).
- Every 1 second (configurable), the watchdog checks:
  - Is the current phase `operate`?
  - Has either counter changed since the last check?
  - If `operate` AND no progress for `watchdog_secs` (default 60),
    fire.
- On fire:
  - Log to stderr: `[variant] watchdog: no progress in <N>s during
    operate phase -- internal stall; self-exiting`.
  - Call `std::process::exit(2)` (or whatever the chosen
    self-stall exit code is). Drop impls run on the way out so
    JSONL is flushed cleanly.
- New CLI arg `--watchdog-secs <u32>` (default 60; 0 disables the
  watchdog).
- Variants don't need to do anything special. The watchdog is
  variant-base-only.

#### Tests

- Unit: stub variant whose counters intentionally don't advance for
  >watchdog_secs in operate phase; assert `std::process::exit` is
  called (mock the exit fn or use a process-level integration test).
- Unit: stub variant whose counters DO advance; assert watchdog
  doesn't fire.
- Unit: `--watchdog-secs 0` disables the watchdog.
- Integration: a synthetic variant that calls `std::thread::park()`
  inside `try_publish` to deadlock; runner spawns it; variant
  self-exits via watchdog after ~60s; runner observes a clean exit
  with a non-zero exit code; JSONL ends cleanly (not truncated).

#### Analysis side (T14.17 classifier extension)

Add a new classification value `variant_self_killed_idle`. Rule:
- spawn `status=failed` with the watchdog's exit code (worker picks
  exit code, documents in CUSTOM.md, T14.17 reads it)
- variant's JSONL ends cleanly (last line parses) with no
  `eot_sent` event
- variant's stderr capture contains the
  `watchdog: no progress in <N>s` line

This classification is **mutually exclusive** with `deadlock` (which
requires JSONL truncated mid-record) and `eot_lost` (which requires
peer-side EOT failure).

Whether T15.11 ships the T14.17 rule too, or files it as a small
analysis follow-up, is the worker's call. Recommendation: ship it
together — same patch, same conceptual change.

#### Validation (MANDATORY)

- `cargo build --release -p variant-base` clean.
- `cargo test --release -p variant-base` all-green.
- `cargo test --release --workspace` no regression.
- Clippy + fmt clean.
- **End-to-end repro (MANDATORY)**: rebuild, re-run the stress
  fixture `configs/two-runner-stress-e14.toml`. The 2 currently-
  `deadlock`-classifying Zenoh qos3/qos4 multi spawns should now
  classify as `variant_self_killed_idle` (or whatever name lands)
  instead. Capture the post-T15.11 classifier output for those rows
  in the completion report.

#### Acceptance criteria

- [ ] Watchdog thread implemented in `variant-base`.
- [ ] `--watchdog-secs` CLI arg.
- [ ] Watchdog converts deadlocked variants to clean self-exit with
      flushed JSONL.
- [ ] T14.17 classifier recognises the new outcome (either in this
      patch or a sibling follow-up).
- [ ] Stress fixture's Zenoh qos3/qos4 multi rows reclassify away
      from `deadlock`.
- [ ] All existing tests pass.

#### Out of scope

- Actually fixing Zenoh's internal deadlock (that's T14.9 territory
  if WASM compat is wanted; otherwise it stays as an honest cliff).
- Making the watchdog phase-aware beyond "fire only in operate".
  Stabilize / silent phases have well-defined durations; the
  watchdog doesn't need to interfere with them.
- Tuning `watchdog_secs` adaptively. Static default is fine.

---


### T14.9c: variants/zenoh -- enable HTTP keep-alive in Single-mode ureq Agent

**Repo**: `variants/zenoh/`.
**Status**: pending, filed 2026-05-14 after stress fixture surfaced
ephemeral port exhaustion in T14.9b's HTTP client.

#### Background

T14.9b shipped the sync RPC client via `ureq` for Zenoh Single mode.
Manual smoke + low-rate two-runner integration test both passed
(99.97% / 99.67% delivery at 1K msg/s). But the stress fixture
(`configs/two-runner-stress-e14.toml`) at 100K msg/s qos2-4 Single
failed within ~1 second with:

```
Error: HTTP PUT http://127.0.0.1:20100/bench/486 failed:
  io: Only one usage of each socket address (protocol/network
  address/port) is normally permitted. (os error 10048)
```

This is **WSAEADDRINUSE** — Windows' ephemeral port range is ~16K;
at 100K msg/s every `publish()` opens a fresh TCP connection to
zenohd, exhausting the pool. Linux has 28K range default, would hit
the same wall around 30K msg/s.

Root cause: T14.9b's `ureq::Agent` was configured with **keep-alive
disabled**. With keep-alive enabled, the Agent pools and reuses
connections to the same host:port; outbound TCP socket count drops
to ~1 instead of N-per-publish. The qos1 single case (1K msg/s)
fits in the port pool so it works; qos2-4 single (100K msg/s)
exhaust immediately.

#### Scope

In `variants/zenoh/src/rest_client.rs`:

- Construct the `ureq::Agent` with **keep-alive ENABLED** (or, more
  precisely, don't explicitly disable it; the ureq default is on).
- Verify via packet capture or log inspection that subsequent
  `publish()` calls reuse the same TCP connection. The smoke test
  should report "1 TCP connection observed" rather than N.
- Document in `CUSTOM.md` (T14.9b section) the explicit reason for
  keep-alive, and the empirical port-exhaustion failure mode that
  motivated the fix.

#### Tests

- Existing T14.9b smoke + integration tests should continue passing.
- Add an `#[ignore]`'d two-runner regression test at moderate-high
  rate (e.g. 100 vpt × 100 Hz × 5 s = 50K total msgs/spawn, 10K
  msg/s) in Single mode. Both runners should complete
  `runner_idle_terminated` without `os error 10048`.
- Worker considers whether to extend the existing two_runner
  regression with a higher-rate parameterised case rather than a
  brand-new test.

#### Validation (MANDATORY)

1. `cargo build --release -p variant-zenoh` clean.
2. `cargo test --release -p variant-zenoh` all-green.
3. `cargo test --release -p variant-zenoh -- --ignored` all-green.
4. Clippy + fmt clean.
5. **End-to-end repro (MANDATORY)**: re-run `configs/two-runner-stress-e14.toml`.
   The 4 currently-failing Zenoh Single rows (qos2/qos3/qos4 + their
   delivery numbers) should no longer fail with `os error 10048`.
   Worker reports the actual delivery numbers in the completion
   report.

#### Acceptance criteria

- [ ] `ureq::Agent` constructed with keep-alive enabled.
- [ ] Empirical verification of connection reuse (worker picks
      method: small unit test mocking a counted-connection server,
      OR documented via Wireshark / netstat observation).
- [ ] CUSTOM.md updated with the rationale + the os error 10048
      failure mode.
- [ ] Stress fixture's Zenoh Single rows no longer hit
      WSAEADDRINUSE.
- [ ] All existing tests pass.

#### Out of scope

- Tuning ureq pool size (the default should be fine).
- Handling the case where keep-alive is forcibly closed by zenohd
  (worker may need to handle "connection reset" → reconnect, but
  that's reactive — start with just enabling keep-alive).
- Whether the SSE reader thread (poll_receive) also needs
  connection management — it's already long-lived, so a single
  connection per spawn is the expected behaviour.

---

---

## E16 — Diagnostic Cleanup from 2026-05-14 Full-Matrix Analysis

Filed 2026-05-14. Source: 
`logs/same-machine-all-variants-01-20260514_084636/analysis/analyze_report.md`.

### T16.1 — Analysis: suppress `<100.0%` warnings on rows that round to 100% [low]

**Repo**: `analysis/`
**Owner**: orchestrator → spawn worker.
**Goal**: Remove ~30 false-positive lines like `bob->alice qos3 delivery 100.0% (<100.0%)` from the `emit_incomplete_warnings` and `format_incomplete_warnings` paths. Underlying value is e.g. 99.99988% (875999/876000); display rounds to 100.00 but the comparison uses the raw float.

**Implementation hint**: in `analysis/incomplete_warnings.py`, change the delivery-shortfall trigger from `delivery_pct < 100.0` to `delivery_pct < 99.995` (i.e. anything that would round-down-to-display as 100.0% is treated as 100%). The `[FAIL: completeness]` annotation on the integrity table row should remain unchanged.

**Tests**: add a fixture row at 99.999% and assert it no longer triggers a warning; keep the existing test for 99.0% so it does trigger.

**Acceptance**: re-running analyze on the 2026-05-14 dataset emits zero `100.0% (<100.0%)` lines. Warning total drops from 267 to ~235.

### T16.2 — Websocket-multi: fix negative latencies [high]

**Repo**: `variants/websocket/`
**Owner**: orchestrator → spawn worker.
**Goal**: `websocket-multi` reports negative mean latencies on QoS 3/4 cells (e.g. `p50 = -0.0253 ms` on `100x100hz qos3`, several others around `-0.01`..`-0.06 ms`). The writer's `write_ts` is being captured after the receive thread logs `receive_ts` for the loopback receive on the same machine. This violates the schema contract that `receive_ts >= write_ts` and blocks T11.5 from using receive throughput as the headline metric.

**Investigation steps**:
1. Identify where `write_ts` is captured in the websocket variant's publish path. Look for any non-blocking dispatch that returns to the caller before the timestamp is taken.
2. Identify where `receive_ts` is captured in the multi-mode reader thread. Look for `Instant::now()` calls that may happen before the writer's timestamp is taken.
3. Hypothesis to validate: `write_ts` should be captured *before* the message is handed to the WS send path. `receive_ts` should be captured at the first point after the bytes are pulled from the socket into user space.

**Fix**: ensure both timestamps are taken from the same monotonic clock (variant-base helper if there is one) and that `write_ts` is taken before any inter-thread handoff in the send path.

**Reproducer**: `variants/websocket/tests/fixtures/two-runner-websocket-qos4.toml` (already in tree) or the freshly added `two-runner-websocket-t14-20-stress.toml`.

**Acceptance**: re-running the reproducer shows non-negative p50/mean for every QoS 3/4 cell on websocket-multi; latency percentiles are monotonically non-decreasing (p50 ≤ p95 ≤ p99 ≤ max).

### T16.3 — Hybrid-single TCP back-pressure under load [high]

**Repo**: `variants/hybrid/`
**Owner**: orchestrator → spawn worker.
**Goal**: `hybrid-single` QoS 3/4 fails catastrophically (0.1–24.7% delivery, multi-second tail latency) at workloads as low as `10x100hz` (1 000 msg/s). The multi-thread version of hybrid is fine on the same workloads — so the regression is single-mode specific.

**Investigation steps**:
1. Read `variants/hybrid/CUSTOM.md` for the historical context on blocking writes / SO_SNDTIMEO.
2. Read T14.4 (Hybrid multi) and T14.19 (TCP single-mode SO_SNDTIMEO) in this TASKS.md file for what was supposed to land.
3. Audit whether the single-mode code path actually configures SO_SNDTIMEO and uses blocking writes for TCP. Common regression: the back-pressure handling code is gated on `ThreadingMode::Multi` when it should apply to both modes.
4. Check CPU% in the failing runs (`logs/same-machine-all-variants-01-20260514_084636/analysis/summary_performance.md` shows hybrid-single QoS 3/4 spawns pegged at 99% — one thread blocking on TCP write).

**Fix**: apply SO_SNDTIMEO + blocking-write semantics to TCP in single mode (matching multi mode).

**Reproducer**: a small two-runner-hybrid-qos4 config at `10x100hz` or `100x100hz`. If none exists, create one as `variants/hybrid/tests/fixtures/two-runner-hybrid-single-qos4-repro.toml`.

**Acceptance**: hybrid-single QoS 3/4 reaches >=99% delivery on the `10x100hz` reproducer and >=80% on `100x100hz`.

### T16.4 — Custom-UDP multi vs single regression [medium]

**Repo**: `variants/custom-udp/`
**Owner**: orchestrator → spawn worker (after T16.2, T16.3, T16.5 are running).
**Goal**: `custom-udp-multi` shows worse delivery than `custom-udp-single` at high path counts:
- `1000x10hz qos3`: single 64.0%, multi 16.1%
- `1000x100hz qos3`: single 55.8%, multi 10.6%

This contradicts the expected behaviour that multi-mode should be at least as good as single-mode. Likely a reader-thread contention or socket-buffer mismanagement on the NACK feedback path.

**Investigation steps**:
1. Inspect the per-socket recv thread setup in custom-udp multi mode.
2. Check whether the NACK feedback path goes through the multi-thread reader or a separate thread.
3. Profile the multi-mode reproducer to see where the bottleneck is.

**Fix**: likely a reader-thread coordination issue; could be socket buffer too small per-thread or a lock contention on the NACK state.

**Acceptance**: custom-udp-multi reaches at least 90% of custom-udp-single delivery on the `1000x10hz qos3` reproducer.

### T16.5 — Zenoh 1000-path asymmetric collapse [high]

**Repo**: `variants/zenoh/`
**Owner**: orchestrator → spawn worker.
**Goal**: At 1 000-path workloads, Zenoh shows extreme asymmetry: one peer writes ~3 M messages, the other writes ~2 000 and accumulates ~500 000 `backpressure_skipped` events. Nine spawns terminate via `variant_self_killed_idle`. One direction of delivery reads 0%.

**Investigation steps**:
1. Inspect the Zenoh declaration/subscription propagation for 1 000 keys (vs 10-100 keys which work).
2. Look at the variant's `bench/__eot__/<writer>` keyspace usage — does declaration order matter?
3. Check if Zenoh's congestion-control / drop-when-pushing-on-network mode applies and how the variant handles a `Push` returning a drop.

**Reproducer**: a small 1 000-path Zenoh config (could be `configs/two-runner-zenoh-1000x10hz-qos3.toml` — check if it exists, otherwise create).

**Acceptance**: symmetric writer counts (delta < 10%) on the reproducer; one-direction delivery > 50% at QoS 1/2 and > 90% at QoS 3/4. Zero `variant_self_killed_idle` on the reproducer.

### T16.6 — Document Zenoh 400% multicast loopback ratio [low]

**Repo**: `metak-shared/` (docs only)
**Owner**: orchestrator.
**Goal**: Add a paragraph to `metak-shared/ANALYSIS.md` pivot-table section explaining why `zenoh-multi` shows ~400% multicast loopback ratio (vs 200% for other multicast variants). The ratio is correct, but operators reading the pivot tables for the first time may flag it as a bug.

**Acceptance**: a code reviewer understands the 400% ratio after reading the doc.

### T16.7 — Surface Ratio% alongside Delivery% in warnings [low]

**Repo**: `analysis/`
**Owner**: orchestrator → spawn worker.
**Goal**: When emitting a delivery-shortfall warning, include the Ratio% (writer-side shortfall) so operators see both numbers. Example: `delivery 100.0% (ratio 9.3% — writer-side shortfall)`.

**Acceptance**: warning lines for groups with `ratio_pct < 50%` include the ratio annotation.

---


### T16.8 — Runner progress events undercount multi-mode receives [low]

**Repo**: `variant-base/` (most likely the progress-emission path inside the protocol driver / multi-mode reader thread).
**Owner**: orchestrator → spawn worker once T16.3 / T16.5 settle.

Observed during T16.2 verification (see STATUS.md 2026-05-14):
- Multi-mode spawn final progress: `sent=30100 received=0` (RUNNER stdout).
- Same spawn's JSONL has 30 100 receive events that analyze tallies as 100% delivery.
- Single-mode spawn on the same workload reports `sent=20400 received=19800` correctly.

The variant's receive counter (used to emit the periodic progress JSON to stdout) does not see the multi-mode reader thread's receives. Likely the counter is owned by the driver thread and the reader threads don't increment it. The fix is to switch the counter to an `Arc<AtomicU64>` or an mpsc-counted channel that the reader threads bump.

**Acceptance**: re-running the T16.2 reproducer shows `received` matching `sent` (modulo 1 drop) in the runner's multi-mode progress line.

**Out of scope**: any behaviour change in the runner. This is variant-side only.


### T16.9 — Cleanup: dead_code on LoggerProxy::log_write after T16.2 [low]

**Repo**: `variant-base/`
**Owner**: orchestrator → spawn worker.

Pre-existing clippy `dead_code` error flagged by the T16.5 worker after a workspace build with `-D warnings`. After T16.2 the driver call site uses `log_write_at` and the legacy `LoggerProxy::log_write` (if it's the proxy variant) is no longer reachable. Either delete the dead method or annotate `#[allow(dead_code)]` if it's part of the public API surface the contract requires.

**Acceptance**: `cargo clippy --release --workspace --all-targets -- -D warnings` is clean.


### T16.10 — Zenoh QoS 3/4 ordering regression after T16.5 [high] — done 2026-05-24

**Repo**: `variants/zenoh/`
**Status**: filed 2026-05-15 00:39 by orchestrator after the E16 verification smoke.

**Symptom**: On the E16 smoke fixture `zenoh-1000x10hz-qos3-multi`, the integrity report shows ~17 000 out-of-order receives per (writer→receiver) direction on 51 000 receives, with `[FAIL: ordering]` annotated on all four directions. QoS 3 is the reliable-ordered tier — out-of-order arrivals violate the contract.

**Likely cause**: T16.5 changed `publisher_task` from a single async loop that awaits each `publisher.put(...).await` inline to one that clones the `Arc<Publisher>` and `tokio::spawn`s each put independently (bounded by a 4096-slot semaphore). With ordered QoS this is wrong — concurrent `put` calls on the same key/publisher can complete in any order, and the receiver sees them out of sequence.

**Evidence**:
- Pre-T16.5: in `logs/same-machine-all-variants-01-20260514_084636/`, `zenoh-100x100hz-qos3-multi` rows had `Out-of-order 0` across the board.
- Post-T16.5 (this smoke): all four directions show 16 000-17 000 out-of-order on 51 000.
- Pre-T16.5 the variant could not reach 100% delivery at 1000 paths, but it WAS preserving ordering when delivery did happen. T16.5 traded ordering for throughput.

**Fix direction (for the worker — do not act on this without re-reading the trade-off carefully)**:
- For QoS 1/2 (best-effort / latest-value): the per-publish `tokio::spawn` is fine. Keep it.
- For QoS 3/4 (reliable-ordered): publishes on the same key must be serialised. Either:
  - Maintain a per-key sequential queue (mpsc per key) inside `publisher_task`, OR
  - Keep the original sequential inline `await` for QoS 3/4 publishers, OR
  - Use Zenoh's own ordering primitive if one exists for parallel publish.
- The 500 ms post-spawn settle from T16.5 should be preserved.

**Reproducer**: same as T16.5 — `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x10hz-qos3-repro.toml`.

**Acceptance**:
- `zenoh-1000x10hz qos3` and `qos4` reproducers show Out-of-order 0 across all four (writer→receiver) directions.
- Delivery and writer-symmetry results from T16.5 are preserved (≥90% delivery in both directions, no `variant_self_killed_idle`).
- QoS 1/2 throughput improvements from T16.5 are preserved.

**Out of scope**: QoS 1/2 throughput characterisation. They are best-effort/latest-value; ordering is not contractually required there.


### T16.10b — Zenoh QoS 3/4 collapse on cross-WiFi two-machine run [high]

**Repo**: `variants/zenoh/` (investigation first; a fix task will be
filed only after the failure mode is named).
**Status**: filed 2026-05-24 by orchestrator after the user surfaced the
`comparison-qos4.png` plot from
`C:\repo\shared\ddd\two-machines-all-variants-01-20260523_083845`.
This is the first cross-**WiFi** (vs wired-LAN) two-machine dataset on the
orchestrator's record.

**Symptom**: On a two-machine same-AP WiFi run, every
`zenoh-1000x100hz-{block,mixed,scalar}-{qos3,qos4}-multi` spawn produces
~0 receive throughput in the qos3/qos4 comparison plots. The dataset
contains the per-spawn JSONL for every zenoh QoS 3/4 multi spawn but
**no matching `.compact.parquet`** — confirmed by directory listing —
indicating the variant exited via `variant_self_killed_idle` (or similar)
before the digest phase. Other variants (custom-udp, hybrid, quic,
websocket) finish on the same run and produce compact shards.

**What's new vs T16.10**: T16.10's reproducer
`variants/zenoh/tests/fixtures/two-runner-zenoh-1000x10hz-qos3-repro.toml`
runs at 1 000×10 Hz on wired loopback / wired LAN. This dataset is at
**1 000×100 Hz over WiFi** — 10× the rate AND a new transport medium.
WiFi adds link-layer retransmits, airtime contention with the AP, and
bursty drops that the wired reproducer cannot exhibit. The collapse mode
may be the same family as T16.10 (ordering or per-publish parallelism
from T16.5) compounded by link loss, OR a distinct receive-side
back-pressure interaction with the kernel's WiFi NIC buffer. We don't
yet know which.

**Dataset**: `C:\repo\shared\ddd\two-machines-all-variants-01-20260523_083845`

Relevant artefacts inside that directory:
- `analysis/comparison-qos3.png`, `analysis/comparison-qos4.png`,
  `analysis/drop-rate-qos[34].png`, `analysis/latency-cdf-qos[34].png`
- `zenoh-1000x100hz-{block,mixed,scalar}-{qos3,qos4}-multi-{alice,bob}-all-variants-01.jsonl`
- `zenoh-1000x100hz-{block,mixed,scalar}-{qos3,qos4}-multi-{alice,bob}-stderr.txt`
- `analysis/summary_integrity.md`, `analysis/summary_warnings.md`

**Investigation steps**:
1. Read every zenoh qos3/qos4 stderr in the dataset and classify the
   exit reason per spawn (`variant_self_killed_idle`,
   `runner_idle_terminated`, panic, normal exit, ...).
2. For each spawn, summarise from the JSONL: total `write` events
   (writer-side), total `receive` events before exit, and the `phase`
   timeline (`connect / stabilize / operate / eot / silent / done`).
   Compare alice vs bob — asymmetric collapse from T16.5 is a known
   failure mode and will show up as one peer writing M messages while
   the other writes ~0.
3. Count `backpressure_skipped` events at QoS 3/4 across the six
   spawns. Per T17.9, these are contract violations once present.
4. Decision tree:
   - Exit via idle-termination AND writes still flowing on one peer →
     same family as T16.5/T16.10; WiFi just amplifies it.
   - Exit via `runner_idle_terminated` with both peers idle for ≥5 s →
     true throughput collapse (kernel WiFi NIC buffer saturation,
     Zenoh's internal congestion control giving up, ack-window
     exhaustion, or similar).
   - Panic / crash in stderr → distinct bug.
5. Re-run **one** small reproducer locally on **wired** loopback at the
   same workload shape (`zenoh-1000x100hz-scalar-qos3-multi`,
   `zenoh-1000x100hz-scalar-qos4-multi`) — same machines, ethernet
   instead of WiFi — and confirm whether the failure reproduces wired
   or is WiFi-specific. This disambiguation is the **key output**.

**Acceptance**:
- Investigation report appended to `metak-orchestrator/STATUS.md` with:
  - Exit-reason histogram for the 6 zenoh-1000x100hz qos3/qos4 multi
    spawns (3 workload shapes × 2 QoS).
  - Per-spawn writer / receiver symmetry table
    (alice-writes / bob-writes / alice-receives / bob-receives).
  - `backpressure_skipped` counts per QoS.
  - Conclusion: is the WiFi failure the same family as T16.10
    (ordering / asymmetric collapse) or distinct?
- If **same family as T16.10**: a one-line addition to T16.10's
  acceptance requiring a WiFi re-validation pass after T16.10's fix
  lands.
- If **distinct**: a follow-up task T16.10c filed with a concrete fix
  direction (NACK-aware back-pressure, kernel buffer tuning, alternate
  Zenoh congestion-control mode, etc.).

**Out of scope**:
- Any code change inside `variants/zenoh/`. This is investigation only.
- Re-running the full 16-spawn cross-WiFi matrix. One targeted wired
  reproducer at the same workload shape is enough to disambiguate.
- Tuning the runner's idle-detection threshold (E15 territory).
- T16.10 itself — it remains open and should proceed in parallel; this
  task only decides whether T16.10's wired-LAN acceptance is sufficient
  to call the variant "fixed" on WiFi.

**Dependencies**: T16.10 still open. T16.10b does not block T16.10's
fix; the two run in parallel.


### T16.10c — Zenoh: break publisher↔peer-subscriber coupling for WiFi resilience [high] — investigation done 2026-05-25, awaiting user cross-WiFi re-validation

**Repo**: `variants/zenoh/` (primary); `runner/` (if a sidecar router is
spawned alongside the variant); docs in `metak-shared/`.
**Status**: filed 2026-05-24 by orchestrator after the T16.10b worker
named the failure as WiFi-specific symmetric deadlock and recommended
this direction. See STATUS.md "T16.10b completion report — 2026-05-24".

**2026-05-25 update — investigation phase complete (commits `b9ffcfc`,
`1669577`, `1d0a928`)**:
- Wired-LAN baseline on current main (post-T16.10d) is clean — no
  regression.
- Localhost synthetic subscriber-jitter (env-var gated) **does**
  reproduce the cross-WiFi deadlock signature: one peer
  `sent=N received=0`, other runner-idle-terminates. New reusable
  diagnostic tool: `ZENOH_TEST_SUB_JITTER_MS` / `_EVERY`.
- All Option B (FIFO/ack-window/gate-fail-safe) variants either
  regressed the wired-LAN baseline or sat downstream of the actual
  deadlock (which lives at the bridge mpsc / `publisher.put().await`
  CC=Block chain).
- Aggressive synthetic burst (500 ms / 50) deadlocks; mild burst
  (50 ms / 1000, ~5 %) passes at 98 %. Real WiFi may sit in the
  middle.
- Worker stopped at the `runner/` boundary as instructed. Option A
  (router sidecar) is not implemented.

**Open decision** — needs user-driven cross-WiFi re-run on current
main (binary at HEAD ≥ `1d0a928`) using the PowerShell incantation
in STATUS.md "T16.10c worker completion summary". Two outcomes:
- 12/12 cross-WiFi spawns clean-exit → close T16.10c with no Option A.
- Same T16.10b deadlock signature → authorise Option A (router
  sidecar in `runner/`).

**Problem** (one paragraph, from T16.10b's analysis):

The Multi-mode variant uses peer-to-peer Zenoh sessions; each peer's
publisher `put().await` is directly coupled to the remote peer's
subscriber routing thread keeping up. T16.10's `dbb9c70` fix widened the
subscriber FIFO to ~131 K slots and T17.8 caps the in-flight ack window
at 2048. On wired LAN that is enough. On WiFi — link-layer retransmits
(200-500 ms tail bursts), AP airtime contention (multi-second flow
parking under unrelated traffic), bursty 802.11 drops — both peers'
subscriber routing threads park simultaneously, both publishers wedge on
CC=Block, the ack-window watermark stalls at `min_peer_ack`, and neither
peer makes any progress. The T15.5 watchdog fires identically on both
sides after 30 s. 12/12 cross-WiFi qos3/qos4 multi spawns in
`C:\repo\shared\ddd\two-machines-all-variants-01-20260523_083845` show
this signature exactly.

**Two candidate directions** (worker evaluates and picks):

**(A) Local zenohd router sidecar — strategic.** Each runner spawns a
local `zenohd` process at startup; the variant operates in Zenoh
**client mode** against `tcp/127.0.0.1:<router-port>`. The publisher's
`put().await` now resolves at local-router rate; the router absorbs
WiFi-side burstiness in its own queue. Reuses the existing T14.9
(deferred under E14) sidecar idea — and would unlock T14.9's
WASM-single-threaded-client benefit as a side effect. The runner gains
a small lifecycle responsibility (spawn router on `connect`, tear it
down on variant exit). Out-of-process means an extra TCP hop on the
critical path; need to measure whether the wired-LAN throughput regression
is acceptable (rough budget: <20 % throughput degradation at QoS 3/4 on
the wired 1 000×100 Hz workload).

**(B) Larger FIFO + larger ack-window — tactical.** Bump the subscriber
FIFO and T17.8 ack-window budget so they can absorb multi-second WiFi
stalls. Cheap to implement, doesn't change topology. Pushes per-spawn
RSS up (subscriber FIFO at 131 K slots × ~150 B average payload ≈ 20 MB
per peer; at 1 M slots that's ~150 MB per peer — material on a
constrained host). Does NOT address the underlying coupling — the
deadlock just becomes harder to reach, not impossible.

Worker should evaluate (A) first with a small prototype on the wired
reproducer to characterise the router-overhead cost, then decide. Pick
(A) if the wired throughput cost is acceptable; pick (B) only if (A) is
infeasible on Windows (e.g. zenohd packaging issues). Document the
decision and the wired-overhead measurement in
`variants/zenoh/CUSTOM.md`.

**Acceptance**:
- A cross-WiFi two-runner run on the T16.10b dataset's workload
  (`zenoh-1000x100hz-{scalar,mixed,block}-{qos3,qos4}-multi`) completes
  with **all 12** (spawn × runner) pairs exiting via
  `runner_idle_terminated` (clean EOT). Zero `variant_self_killed_idle`.
- Cross-WiFi delivery ≥ 80 % in both directions at QoS 3/4 across all
  three workload shapes. (80 % not 99 % because WiFi is genuinely lossy
  — measure the actual link drop rate and document it.)
- Wired loopback regression suite still passes:
  `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x10hz-qos3-repro.toml`
  (T16.10's fixture) and the qos4 sibling continue to show Out-of-order 0
  per T16.10's acceptance, and the wired 1 000×100 Hz scalar qos3 multi
  reproducer continues to clean-exit.
- `variants/zenoh/CUSTOM.md` documents the chosen topology and the
  wired-throughput cost of the change.
- If direction (A) was chosen: a one-line note in EPICS.md § E14
  "Future work" section saying T14.9 is closed by T16.10c (router
  infrastructure now exists in-tree).

**Dependencies**:
- T16.10 (ordering fix) must land first — T16.10c builds on T16.10's
  publisher path. Verify before spawning a T16.10c worker.
- E14 T14.9 (deferred Zenoh router-RPC) is functionally adjacent; if
  direction (A) is chosen, this task closes T14.9.

**Out of scope**:
- N>2-peer scaling. WebRTC's 1-peer constraint from E3g stays
  out-of-scope and so does N-peer Zenoh router work.
- Changing other variants' topologies. Hybrid / custom-udp / quic /
  websocket / webrtc are unaffected.
- Replacing `zenohd` with a custom router.
- Cross-WiFi CI infrastructure — that's T16.14.

**Validation strategy**:
- The user owns the cross-WiFi re-validation (per the
  `metak-shared/coding-standards.md` and prior worker convention —
  cross-machine validation is not automated). Worker drives the wired
  reproducer + the wired throughput-overhead measurement; user runs
  the cross-WiFi acceptance gate. Worker must clearly document the
  cross-WiFi command for the user to execute (PowerShell).


### T16.10d — Zenoh QoS 3/4 higher-rate ordering at 1 000×100 Hz wired [medium] — done 2026-05-24

**Repo**: `variants/zenoh/`
**Status**: filed 2026-05-24 by orchestrator after T16.10 closed but
T16.10b's wired-loopback evidence showed residual ordering drift at a
workload outside T16.10's acceptance window.

**Observation**: T16.10 verified Out-of-order = 0 on its official
fixtures (`tests/fixtures/two-runner-zenoh-1000x10hz-qos3-repro.toml`
and the qos4 sibling) at **1 000 paths × 10 Hz**. T16.10b's
independent wired-loopback at **1 000 paths × 100 Hz** (10× the rate),
on a binary that already includes the T16.10 fix and the 2026-05-21
self-writer filter, reports:

```
Variant                                           Path        QoS  Sent      Rcvd       Out-of-order  Dupes
zenoh-1000x100hz-scalar-qos3-multi  alice->bob    3    1,422,000 1,443,981  6149          22959
zenoh-1000x100hz-scalar-qos3-multi  bob->alice    3    1,423,000 1,442,000  130           19000
```

That is ~0.43 % reorder rate alice→bob, ~0.01 % bob→alice (asymmetric)
and ~1.6 % apparent duplicate rate. T16.10's `tokio::spawn`-per-publish
serialiser is still on the table for QoS 3/4 — at 10 Hz it's clean, at
100 Hz some QoS 3/4 publishes can still race. The asymmetric reorder
rate (alice→bob ≫ bob→alice) suggests this is publish-side, not
receive-side.

**Caveat on the Dupes line**: T16.10's worker flagged a `3-duplicate`
analyser trace artefact on its own qos1/qos3/qos4 runs (exactly 3
dupes per direction). That artefact is far below the ~22 K / ~19 K
counts here, so the dupes in this evidence row are NOT just the
analyser artefact. T16.16 will characterise the analyser side; T16.10d
should investigate the variant side independently.

**Investigation steps**:
1. Reproduce locally: create
   `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x100hz-qos3-repro.toml`
   mirroring the T16.10b workload. Run two-runner localhost; collect
   the analyzer integrity row. Confirm reorder and dupe counts in the
   same ballpark as the T16.10b evidence.
2. Inspect `publisher_task` in `variants/zenoh/src/zenoh.rs` at QoS 3/4
   path. The T16.10 fix made QoS 3/4 publishes serialised via whatever
   mechanism the worker picked (per-key mpsc queue, inline await, or
   Zenoh primitive). At 100 Hz tick rate with 1 000 keys = 100 K
   publish/s, that serialisation may be funnel-overloaded and falling
   back to a parallel path.
3. If a parallel path is the cause: tighten the serialiser so 100 K
   publish/s holds per-key order. If a transport-layer reorder: check
   whether Zenoh's Multi-mode session offers a stronger
   per-key-ordered guarantee that the variant isn't opting into.

**Acceptance**:
- A 1 000×100 Hz two-runner localhost reproducer (qos3 + qos4 multi)
  shows Out-of-order = 0 across all four (writer→receiver) directions.
- T16.10's original 1 000×10 Hz reproducers still pass with
  Out-of-order = 0.
- Variant-side Dupes count drops below 100 per direction at the same
  workload (the analyser-side T16.16 may explain a residual single
  digits).
- No QoS 1/2 throughput regression vs current main.

**Out of scope**:
- The analyser 3-dupe trace artefact — that's T16.16.
- Cross-WiFi acceptance — that's T16.10c.

**Dependencies**:
- T16.10 (done).
- Should run BEFORE T16.10c starts: T16.10c will measure wired-throughput
  overhead of the router topology; doing that on a known-good wired
  ordering baseline is essential.


### T16.11 — Flake in T16.2 test write_ts_is_captured_before_try_publish [low]

**Repo**: `variant-base/`
**Owner**: orchestrator → spawn worker.

Filed 2026-05-15 by T16.9 worker. The T16.2 regression guard
`driver::tests::write_ts_is_captured_before_try_publish` asserts
`first_write_ts < observed_inside_publish` (strict less-than) between
two `Utc::now()` reads. On Windows the QPC-backed clock can return
identical nanosecond timestamps for back-to-back calls. Test flakes.

**Fix**: relax to `<=` (the contract is "write_ts is captured no later
than the publish call observes it"), OR insert a `std::hint::spin_loop`
between the two ts captures inside the mock so the second clock read
strictly advances. `<=` is cleaner and matches the actual semantic
guarantee.

**Acceptance**: 100 iterations of `cargo test --release -p variant-base
driver::tests::write_ts_is_captured_before_try_publish` pass without
flake.


### T16.12 — variant-base: best-effort compact-parquet flush on watchdog-self-kill [medium]

**Repo**: `variant-base/`
**Status**: filed 2026-05-24 by orchestrator from T16.10b incidental
finding #1.

**Problem**: When the T15.5 internal-stall watchdog fires
(`variant_self_killed_idle`), the variant exits without flushing its
in-process compact buffers, so the dataset has no `.compact.parquet`
for the affected spawn. T16.10b investigation could not directly count
writes, receives, or `backpressure_skipped` events for the six failing
cross-WiFi zenoh spawns — the diagnostic gap forced deductive analysis
from JSONL lifecycle events alone.

**Fix**: Wrap the watchdog's exit path with a best-effort
`digest::flush()` (or equivalent), **bounded** — e.g. 2 s hard timeout
on the flush so a frozen background thread cannot hang shutdown.
Whichever rows reached the in-process buffer are written; anything that
didn't is dropped. The flush should not block on EOT — it's a salvage
operation, not a graceful close.

**Acceptance**:
- A synthetic test (e.g. force the watchdog by stalling the operate
  loop) shows that a partial compact-parquet IS written and contains
  the writes that were buffered before the stall.
- Watchdog still exits within `watchdog_timeout_secs + 2 s`. Worst case
  must not exceed 35 s.
- Existing T15.x tests still pass; no regression on the
  clean-exit / EOT path.

**Out of scope**:
- Changing the watchdog timeout itself.
- Restructuring the compact buffer to be lock-free / always-flushable.

**Dependencies**: None blocking — independent of T16.10 / T16.10c.


### T16.13 — Treat JSONL `resource` cadence as a liveness heartbeat [low]

**Repo**: `analysis/` (read-side) and `metak-shared/api-contracts/jsonl-log-schema.md` (contract clarification).
**Status**: filed 2026-05-24 by orchestrator from T16.10b incidental
finding #2.

**Problem**: T16.10b's `block`-shape spawns logged 18 resource samples
post-`operate` (cleanly limping for ~1.8 s before deadlock); `mixed`
and `scalar` shapes logged **zero** resource samples post-`operate` —
i.e. the resource thread starved within ~10 ms of operate entry. This
cadence is already in the data; it just isn't surfaced as a liveness
signal. If `resource` events were treated as a 100 ms heartbeat, the
analyzer could pinpoint freeze-points without needing a partial parquet
flush (T16.12 complementary, not redundant).

**Scope**:
- `metak-shared/api-contracts/jsonl-log-schema.md`: pin the `resource`
  event as a **mandatory** 100 ms heartbeat in `operate` phase (or
  whatever the current cadence is — confirm and document). Today the
  contract probably documents it as periodic but not as a heartbeat.
- `analysis/` (likely `integrity.py` or a new module): add an
  integrity check "resource-cadence gap > 500 ms during operate phase"
  that flags the gap with a timestamp.
- A synthetic JSONL fixture with a 1 s gap mid-operate should produce
  the new `[FAIL: resource-gap]` annotation.

**Acceptance**:
- Re-running the analyzer over the T16.10b dataset surfaces the
  freeze-point timestamp per cross-WiFi zenoh qos3/qos4 multi spawn,
  matching the empirical observation in T16.10b's report.
- No false positives on the wired loopback regression run (clean exits
  have continuous resource cadence).

**Out of scope**:
- Changing what the variant emits — `resource` is already there. Only
  the contract wording and the analyzer's interpretation change.

**Dependencies**: None blocking.


### T16.14 — Cross-WiFi as a first-class validation surface [low]

**Repo**: orchestrator docs (`metak-orchestrator/`, `metak-shared/`).
No code change.
**Status**: filed 2026-05-24 by orchestrator from T16.10b incidental
finding #3.

**Problem**: Before this week the validation matrix was wired-LAN and
loopback only. The cross-WiFi dataset
(`C:\repo\shared\ddd\two-machines-all-variants-01-20260523_083845`)
is the project's first WiFi run on record and immediately found a
real Zenoh-Multi-mode failure (now tracked as T16.10c) that no wired
test could have caught. Going forward, WiFi should be a periodic
validation surface, not a one-off.

**Scope** (orchestrator-only — no worker spawn):
- Decide cadence: one full cross-WiFi matrix per release wave, OR a
  reduced cross-WiFi smoke (e.g. one spawn per variant × qos3/qos4
  multi only).
- Identify a feasible WiFi setup that the user can re-run on demand
  (laptop on hotspot + workstation on same AP? Two laptops on the
  same AP?). Document the topology in `metak-shared/`.
- Update `metak-shared/BENCHMARK.md` (S5? S6?) to add a "transport
  medium" dimension to the matrix doc, noting that wired-LAN and
  WiFi behave differently and the validation matrix should hit both.
- Add a one-line acceptance to each future variant epic stating
  "wired-LAN AND WiFi cross-machine validation required before
  marking done".

**Acceptance**:
- `metak-shared/BENCHMARK.md` updated with the WiFi validation
  expectation.
- A one-page `metak-shared/wifi-validation-setup.md` describing the
  topology, what to run, and how to interpret the result.
- `metak-orchestrator/CUSTOM.md` references the new doc so future
  orchestrator sessions know about the matrix dimension.

**Out of scope**:
- Setting up cross-machine CI (orchestrator-owned decision; the user
  drives cross-machine runs today, and that's fine).
- Buying / configuring hardware. The user already has the kit.
- Backfilling the existing variants' epics with WiFi validation —
  doing that on `main` would be a multi-PR retroactive sweep. Only
  forward-looking variant epics get the new acceptance line.

**Dependencies**: None blocking. Worth doing once T16.10c lands so the
WiFi spawn list is known-good baseline-wise.


### T16.15 — Port `variants/zenoh/tests/two_runner_regression.rs` to compact-Parquet [medium] — done 2026-05-24

**Repo**: `variants/zenoh/`
**Status**: filed 2026-05-24 by orchestrator from T16.10 worker
follow-up #1.

**Problem**: 5 of 6 ignored tests in
`variants/zenoh/tests/two_runner_regression.rs` —
`1000paths_no_deadlock`, `max_throughput_no_deadlock`,
`single_mode_t149b`, `single_mode_t149c_no_port_exhaustion`,
`t17_8_qos3_100pct_delivery` — currently fail because the in-test
parser reads `write` / `receive` events out of JSONL, but T18.2b
moved those events to compact Parquet. Each subprocess still exits
cleanly with `sent>0` in its stderr; the test's parser counts 0
and the assertion fails.

This is purely a test-side issue — no variant code change is
needed. The tests are `#[ignore]`-gated so they don't break CI,
but the zenoh regression suite is effectively broken until they're
updated.

**Scope**:
- Update the helper in `tests/two_runner_regression.rs` that
  currently parses JSONL events to instead read the per-spawn
  `.compact.parquet` file. Use the contract documented in
  `metak-shared/api-contracts/compact-log-schema.md` (event kinds
  0 = write, 1 = receive, 2 = backpressure_skipped).
- The runner-stdout-driven counts (`sent=N received=M` in stderr /
  runner stdout) are an easier alternative if the test only needs
  totals — consider that if the parquet path adds significant
  dependency weight.
- Verify all 6 ignored tests run green:
  ```powershell
  cargo test --release -p variant-zenoh --test two_runner_regression -- --ignored --nocapture
  ```
- Keep the existing `t17_8_qos3_100pct_delivery` 100% delivery
  threshold (or per-spawn equivalent).

**Acceptance**:
- All 6 ignored tests in
  `variants/zenoh/tests/two_runner_regression.rs` pass.
- No additional crates added unless absolutely necessary; if `polars`
  or `arrow` is required, justify in the commit message.
- `cargo clippy --release --workspace --all-targets -- -D warnings`
  remains clean.

**Out of scope**:
- Adding new test cases or new fixtures.
- Variant code changes.
- Other variants' regression suites — only `variants/zenoh/` is
  affected by this specific T18.2b interaction.

**Dependencies**: None blocking.


### T16.16 — analyzer: investigate 3-duplicate trace at EOT window [low] — done 2026-05-24

**Repo**: `analysis/`
**Status**: filed 2026-05-24 by orchestrator from T16.10 worker
follow-up #2.

**Observation**: T16.10's worker observed exactly 3 duplicates per
direction across qos1, qos3, and qos4 reproducer runs. Same count
across three independent QoS tiers strongly suggests an analyser-side
double-count rather than a Zenoh on-wire duplicate. The most likely
culprit: the EOT-window boundary in `integrity.py` (or wherever
correlate joins write↔receive over the operate window) counting a
small region of receives twice — possibly the boundary inclusive on
one side and the EOT-marker itself triple-counted.

**Investigation steps**:
1. Reproduce: run the qos3 reproducer at low rate and inspect the
   3 dupe-flagged receive rows in the analyser pipeline. Identify the
   `seq` / `writer` / `receive_ts` of each.
2. Check whether each dupe-flagged row corresponds to a `seq` that
   appears exactly once in the write stream — if so, the dupe is
   purely analyser-side (the receive_ts is being matched twice).
3. Inspect the EOT-window boundary logic in
   `analysis/correlate.py` and `analysis/integrity.py`. Look for
   any `<=` vs `<` boundary mismatch on the `operate_window_end`
   timestamp.
4. Fix the boundary or the join to be strict.

**Acceptance**:
- Re-running the T16.10 qos3 / qos4 reproducers under the patched
  analyser shows Dupes = 0 for every (writer, receiver) direction.
- No regression on `analysis/tests/` (full pytest suite green).
- Existing dataset integrity rows (`logs/same-machine-all-variants-*`)
  are not materially worsened by the change.

**Out of scope**:
- The much-larger ~22 K / ~19 K dupe trace from T16.10b's
  1 000×100 Hz wired reproducer — that count is far above any
  boundary artefact and belongs to T16.10d's variant-side
  investigation.

**Dependencies**: None blocking.


### T16.17 — Audit threading-mode coverage across all variant fixtures [medium]

**Repos**: `variants/*/tests/fixtures/`, `configs/`, plus
`metak-orchestrator/EPICS.md` for E14 task acceptance.
**Status**: filed 2026-05-24 by orchestrator after T16.10d and T16.15
workers independently surfaced the same coverage gap.

**Problem**: E14 T14.8 changed the default `threading_modes` to
`["single"]` for backwards-compatibility. Any fixture that does NOT
explicitly set `threading_modes` now silently runs in Single mode.
Two independent workers on 2026-05-24 discovered:

1. **T16.10d worker**: T16.10's original 10 Hz qos3/qos4 reproducer
   fixtures omit `threading_modes` and default to Single. T16.10's
   `publisher_task` fix (which lives in the Multi-mode in-process
   path) was thus never exercised end-to-end by the official fixture
   set — the test passed because it ran the zenohd sidecar HTTP-PUT
   path, which trivially preserves ordering.
2. **T16.15 worker**: Existing zenoh `1000paths`, `max`, and `t149b`
   fixtures default to Single mode after T14.8, but their thresholds
   were sized for Multi (the only mode pre-T14.8). T16.15 pinned them
   to Multi in the test harness (`materialize_fixture`) as a
   short-term mitigation, but the underlying fixture files are still
   silently wrong.

This is a **project-wide audit** task because the same gap likely
affects fixtures in `variants/custom-udp/`, `variants/hybrid/`,
`variants/quic/`, `variants/websocket/`, and possibly `configs/`.

**Scope**:
- Inventory every `*.toml` under `variants/*/tests/fixtures/` and
  `configs/` and identify which omit `threading_modes`.
- For each, decide: should this fixture exercise Single, Multi, or
  both? Document the decision in a one-line comment at the top of
  each fixture file.
- Add an explicit `threading_modes = [...]` to every fixture that
  matters. Don't rely on the default for any fixture intended to
  validate a specific code path.
- Audit any test threshold that was sized for one mode but now
  silently runs the other. Adjust threshold sizing OR pin the mode
  explicitly.
- Update `metak-shared/coding-standards.md` (or a new fixture-style
  doc) to require explicit `threading_modes` on every fixture going
  forward.

**Acceptance**:
- Every fixture under `variants/*/tests/fixtures/` and `configs/`
  has an explicit `threading_modes` setting.
- A grep test in CI (or a documented pre-commit check) rejects any
  new fixture missing `threading_modes`.
- All variant test suites still pass (no threshold-sizing
  regressions exposed by the explicit mode).

**Out of scope**:
- Changing the T14.8 default itself. The default-to-Single is
  intentional for backwards-compat. The fix is to make fixtures
  explicit, not to change the language.
- Adding new fixture cases. Existing fixtures only.

**Dependencies**: T16.15 done (the pin-in-test-harness mitigation is
already in place; this task replaces it with a proper fixture-side
fix).


### T16.18 — Restore T16.15's relaxed test thresholds [low]

**Repo**: `variants/zenoh/tests/`.
**Status**: filed 2026-05-24 by orchestrator from T16.15 worker's
documented trade-off.

**Problem**: T16.15 ported `tests/two_runner_regression.rs` to
compact-Parquet but relaxed several per-direction thresholds because
the receive-clock-scoped denominator (the natural one for the
compact-Parquet read path) drops the writer's last tick of writes
relative to the legacy JSONL approach. The relaxations:

- `1000paths_no_deadlock`: per-direction `== 100%` → `>= 80%`.
- `max_throughput_no_deadlock`: `>= 80%` → `>= 20%`.
- `single_mode_t149b`: per-direction `>= 80%` → "at least one
  direction `>= 30%`".

These are real coverage rollbacks — the tests now run, but they
catch much less. The original 100% / ≥80% thresholds matched the
contract (QoS 3/4 are reliable tiers).

**Fix direction**: Replicate `analysis/performance.py`'s
`_write_receive_counts` write-clock-scoped denominator logic in the
test harness. That requires per-event `(writer, seq, path)`
correlation across the two compact files. Lift the helper out of
`analysis/performance.py` into a shared `analysis/lib/` module or
similar so the Rust test can shell out to a Python invocation; or
re-implement in Rust if the helper is short.

**Acceptance**:
- `1000paths_no_deadlock`: per-direction `== 100%` restored.
- `max_throughput_no_deadlock`: `>= 80%` restored.
- `single_mode_t149b`: per-direction `>= 80%` restored.
- All 6 ignored tests still pass.
- No new external Python invocation cost in the default test set
  (still `cargo test --release -p variant-zenoh` with no environment
  prep).

**Out of scope**:
- Changing the analysis-side `_write_receive_counts` algorithm.
- Adding new fixtures or new tests.
- Refactoring the test harness beyond what's needed for the
  threshold restoration.

**Dependencies**: T16.15 done.


### T16.19 — Investigate QoS 4 completeness at 1 000×100 Hz Multi mode [medium]

**Repo**: `variants/zenoh/`.
**Status**: filed 2026-05-24 by orchestrator from T16.10d worker's
observation.

**Problem**: T16.10d's `two-runner-zenoh-1000x100hz-qos4-repro.toml`
fixture shows OoO = 0 and Dupes = 0 (the fix worked) but completeness
is only 45-67 % per direction. The worker attributes this to
T17.8's credit window. That warrants verification — T17.8 was
designed to **back-pressure** the publisher cleanly, not to drop
deliveries below 100 %. If the credit window is genuinely capping
throughput, the publisher should slow down to match the receiver,
not drop writes; the variant should never emit a `write` event for a
sample that can't be delivered.

QoS 4 is the strictest reliability tier in the project's contract.
A 45 % completeness at QoS 4 on a wired-loopback 1 000-path workload
is, at face value, a contract violation.

**Investigation steps**:
1. Confirm via T17.8's design (see EPICS § E17 and STATUS.md
   "T17.8 — variants/zenoh") whether the credit window is supposed
   to back-pressure publish() or whether it's allowed to skip.
2. Inspect the publisher path in `variants/zenoh/src/zenoh.rs` and
   `subscriber_task` to determine whether incomplete deliveries
   correspond to writes that were emitted but never received, or
   writes that were silently skipped.
3. Cross-check with `backpressure_skipped` event counts in the
   compact-Parquet (T17.9 added the flag for contract violation
   detection). If non-zero at QoS 4 on these runs, that's an
   immediate FAIL signal.
4. Conclude: is this a real bug, an expected back-pressure latency
   (in which case the workload is intentionally over-rate and
   should be documented), or a credit-window sizing issue?

**Acceptance**:
- Either: 1 000×100 Hz qos4 Multi-mode reproducer shows ≥ 99 %
  per-direction completeness with OoO = 0 / Dupes = 0;
- Or: the project documents that 1 000×100 Hz qos4 Multi is
  intentionally over-rate and the credit-window-driven incomplete
  delivery is the expected response, with a clear note in
  `variants/zenoh/CUSTOM.md` and a relaxed acceptance threshold on
  the reproducer fixture.

**Out of scope**:
- Changing T17.8's credit/window design itself.
- Anything below 1 000 paths or below 100 Hz — those workloads have
  not shown this completeness issue.

**Dependencies**: T16.10d done.


---

## E17 — Strict No-Skip Contract for QoS 3 / QoS 4

See `metak-orchestrator/EPICS.md` § E17 for full motivation, contract
changes, dependencies, and acceptance. Sub-tasks T17.1-T17.8 are
**done**; T17.9 + T17.10 remain.

### T17.1-T17.8 — DONE

Captured in EPICS.md § E17 sub-tasks list with commit hashes. STATUS.md
has per-task completion reports.

### T17.9 — analysis: flag `backpressure_skipped` at QoS 3/4 as contract violation [medium]

**Repo**: `analysis/`
**Owner**: orchestrator → spawn worker (Wave 3).
**Goal**: Per the updated `metak-shared/api-contracts/jsonl-log-schema.md`,
`backpressure_skipped` at QoS 3/4 is a contract violation. Add an
analyzer integrity check that flags any such row.

**Files**:
- `analysis/integrity.py` — per-`(variant, run, qos)` count of
  `backpressure_skipped` where `qos >= 3`. Surface as `[FAIL:
  skip-at-reliable]` annotation on the integrity row.
- `analysis/incomplete_warnings.py` — emit a contract-violation warning
  for any non-zero count.
- `analysis/tests/` — synthetic fixture with a `backpressure_skipped`
  row at `qos=3` asserts FAIL annotation + warning.

**Acceptance**: synthetic fixture triggers warning; re-running on
post-E17 logs produces zero such warnings.

### T17.10 — runner: full-matrix re-run + acceptance heatmap [medium]

**Repo**: `runner/`, `configs/`, `logs/`.
**Owner**: **user** (runs the full matrix on real hardware).
**Wave**: 3.

**Procedure**:
1. `cargo build --release --workspace`
2. Mirror build to second machine.
3. `<runner> --name <a|b> --config configs/two-runner-all-variants.toml`
4. Collect into `logs/two-machines-all-variants-e17-<timestamp>/`.
5. `python analysis/analyze.py <log-dir> --summary --diagrams --output <log-dir>/analysis`

**Acceptance**: QoS 3/4 drop-rate heatmaps solid 0.0% across every
cell. QoS 1/2 unchanged (best-effort by design).

**Open items**:
- `silent_secs` per-variant: webrtc 30s, hybrid 10s (otherwise in-flight
  bytes truncate at disconnect). Bump matrix default to 30s (+~2h) or
  extend TOML schema for per-variant override.
- websocket duplicates anomaly (T17.5 follow-up — file as T17.11 if
  reproducible at matrix scale).


---

## E18 — Compact Log Format + Runner Log-Path + Auto-Analyze

See `metak-orchestrator/EPICS.md` § E18. User-approved 2026-05-18 to
**proceed in parallel with E17 Wave 3** (T17.9 + T17.10) rather than
strictly serialising.

### T18.1 — Contract: compact-log-schema.md — DONE

`metak-shared/api-contracts/compact-log-schema.md` filed 2026-05-18.
Apache Parquet binary format; per-spawn `.compact.parquet` files;
columnar `(ts, path_idx)` writes + `(ts, path_idx, writer_idx)`
receives; metainfo + path-intern + peer-intern + aux + resource +
connected + phase row groups; ordering-based correlation (no `seq`);
30-50× target size reduction.

### T18.2 — variant-base: in-memory buffers + digest phase + Parquet writer [high]

**Repo**: `variant-base/`
**Owner**: orchestrator → spawn worker (Wave 1 — foundation).
**Status**: ready to spawn.

**Goal**: Add the `digest` phase between `silent` and `done`. During
operate + silent the variant accumulates writes/receives in columnar
in-memory buffers. At `digest` it serializes the buffers to a single
`<variant>-<runner>-<run>.compact.parquet` file per the T18.1 schema
and exits cleanly.

**Files**:
- `variant-base/src/logger.rs` (or wherever the JSONL logger lives):
  add a parallel compact-buffer logger that captures the same events
  into columnar `Vec<i64> ts`, `Vec<u32> path_idx`, `Vec<u8>
  writer_idx` (receives only), plus a `HashMap<String, u32>` path
  intern table and a `Vec<String> writer_names` peer intern. Aux
  events (`gap_*`, `eot_*`, `clock_sync`, `backpressure_skipped`,
  `phase`, `resource`, `connected`) go into a tagged-union Vec.
- `variant-base/src/driver.rs`: new `digest` phase between `silent`
  and the final exit. Emits a `phase=digest` JSONL event (legacy
  stream, if enabled) so the runner sees the transition. Flushes the
  compact buffer to disk via the Parquet writer.
- Add a Parquet writer dep — recommended: `parquet = "53"` (arrow-rs
  family) with `arrow = "53"`. Worker picks based on workspace dep
  compatibility check.
- New CLI args (per `compact-log-schema.md` § Memory budget):
  - `--digest-mem-soft-mb <u32>` (default `1024`).
  - `--digest-mem-hard-mb <u32>` (default `2048`).
  - `--legacy-jsonl-events` (bool, default OFF) — if set, ALSO writes
    the legacy per-event JSONL stream alongside the compact file.
- Path intern: grow `Vec<String> paths` + `HashMap<String, u32>
  lookup` lazily during `log_write` / `log_receive`. Cap at u32::MAX;
  return error if exceeded (impossible at realistic workloads).
- Peer intern: same shape with `Vec<String> peers` +
  `HashMap<String, u8>` lookup. Cap at `u8::MAX` (255 peers).
- Soft / hard memory ceilings: log a warning on cross of soft, fail
  fast on cross of hard.
- The compact writer SHOULD use snappy compression. Worker MAY use
  zstd if a benchmark on a `1000x100hz × 30 s` spawn shows a
  meaningful win — document the choice in `variant-base/CUSTOM.md`.

**Tests** (mandatory):
- Unit: path intern grows correctly; duplicate path returns existing
  index.
- Unit: peer intern grows correctly; overflow at 256 peers returns
  an error.
- Unit: soft mem ceiling logs a warning; hard ceiling errors out.
- Unit: writes/receives buffer captures the right columns.
- Integration: VariantDummy spawn writes a `.compact.parquet`; reading
  it back via the `parquet` crate gives the expected row counts +
  column dtypes per `compact-log-schema.md`.
- Integration: `--legacy-jsonl-events` ON writes both files.
- Round-trip with the analysis tool's compact loader (T18.4) — defer
  to T18.4 if T18.4 hasn't landed yet.

**Build + test**:
```
cargo test --release -p variant-base
cargo clippy --release --workspace --all-targets -- -D warnings
cargo fmt --check
```

**Acceptance**:
- VariantDummy spawn produces a `.compact.parquet` whose `metainfo`
  row group matches the spawn identity.
- File size on a `100x100hz × 30 s` VariantDummy spawn is at least
  10× smaller than the equivalent legacy JSONL on the same workload.
- All variant-base tests pass.

**Commits**: worker commits as they go. Suggested: (1) buffer + intern
infrastructure + unit tests; (2) Parquet writer + digest phase +
integration test; (3) CUSTOM.md docs.

**Out of scope**:
- Variant changes — covered by T18.3.
- Analysis-side compact loader — covered by T18.4.
- Runner changes — covered by T18.5 + T18.6.

### T18.2b — variant-base: extend compact writer to cover lifecycle events [medium]

**Repo**: `variant-base/`
**Owner**: orchestrator → spawn worker.
**Status**: filed 2026-05-18 after T18.2 merge review surfaced a gap.

**Background**: T18.2 (commits `ac25cdb`, `cebdd0d`, `cde4744`,
`c02ed37` merged via `b992699`) shipped the compact buffer + Parquet
writer covering `write`, `receive`, `backpressure_skipped`,
`gap_detected`, `gap_filled` events. Lifecycle events (`phase`,
`connected`, `eot_*`, `resource`, `clock_sync`) currently stay on the
legacy JSONL stream only — they're NOT in the compact Parquet file.

This defeats E18's goal: the analyzer's existing pipeline depends on
`phase` events to find operate/silent/EOT boundaries (the T16.16
cross-clock fix uses them), on `connected` for connect-time metrics,
on `eot_sent` to bound the operate window, and on `resource` for
CPU/memory rollups. Without these in the parquet, compact-only mode
requires the operator to keep the JSONL files anyway, defeating the
30-50× size-reduction goal.

The post-T18.2 contract update (commit pending in this batch) defines
a single tagged-union `compact_events` table covering ALL event types
with a `kind` discriminator + polymorphic `extra_*` columns. T18.2's
implementation already uses the single-table shape; this task extends
the writer to ALSO log lifecycle events into it.

**Goal**: Every JSONL event type produced by `variant-base` is also
emitted as a row in the compact `compact_events` table, with the
column mapping from `compact-log-schema.md` § Event kinds.

**Files**:
- `variant-base/src/compact.rs` — `EventKind` enum gains the
  lifecycle variants (`Phase=5`, `Connected=6`, `EotSent=7`,
  `EotReceived=8`, `EotTimeout=9`, `Resource=10`, `ClockSync=11`).
- `variant-base/src/compact_writer.rs` — schema gets the
  `extra_f32`, `extra_f32_b`, `extra_i64`, `extra_utf8` columns per
  the contract. Writer null-handles the polymorphic columns.
- `variant-base/src/driver.rs` — every JSONL `log_*` call site that
  currently writes a lifecycle event ALSO routes through
  `sink.record_*` so the compact buffer captures it.
- `variant-base/tests/integration.rs` — extend the existing
  roundtrip test to assert that all lifecycle event kinds appear in
  the parquet file. Add explicit per-kind unit tests in
  `compact.rs::tests`.

**Tests**:
- Roundtrip via VariantDummy: spawn produces a parquet whose
  `compact_events` table contains rows for every lifecycle event the
  legacy JSONL stream emits.
- Per-kind unit tests: each `EventKind` round-trips through the
  buffer + writer + parquet reader with the right column slot
  populated.

**Acceptance**:
- VariantDummy spawn with `--legacy-jsonl-events` OFF produces a
  parquet whose `compact_events` table has rows for `phase` (one per
  transition), `connected` (one per peer), `eot_sent` / `eot_received`
  (one each when applicable), `resource` (periodic), plus the
  existing write/receive/skip/gap kinds.
- Reading the parquet via polars + filtering by `kind` gives the
  same `phase` / `connected` / etc rows the analyzer's existing
  pipeline expects on the JSONL stream.
- All previous tests still pass.
- `cargo clippy --release --workspace --all-targets -- -D warnings`
  clean. `cargo fmt --check` clean.

**Commits**: workers commit as they go. Suggested: (1) `EventKind` +
schema extension + unit tests, (2) driver wiring + integration test.

**Dependencies**:
- T18.2 (landed).
- T18.4 (analyzer compact loader) waits on T18.2b — without lifecycle
  events in the parquet, the analyzer can't operate on compact-only
  data.

### T18.3 — Variant audit: any variant bypassing variant-base logger [low] — done 2026-05-19

**Repo**: every `variants/*/`.
**Owner**: orchestrator → spawn worker(s) (Wave 2, after T18.2).
**Goal**: Each concrete variant should already route all `write` /
`receive` events through `variant-base`'s logger. T18.3 confirms this
and fixes any that don't.

**Acceptance**: each variant's `connect`/`publish`/`poll_receive`
paths use the `variant-base` logger; no variant writes JSONL or
custom files directly. Variants that need to skip the compact write
path for any structural reason (e.g. zenoh sidecar mode) are
documented in their CUSTOM.md.

**Outcome (2026-05-19)**: audit complete; 5 OK / 1 GAP. No variant
writes JSONL or opens custom log files directly. Websocket uses the
public `LoggerHandle::log_receive` from its Multi-mode reader thread
and Single-mode T17.5 retry helper -- legal under the literal
acceptance criterion, but those calls bypass the compact `EventBuffer`
introduced by T18.2 and would surface as missing receives in
`*.compact.parquet`. See `variants/T18.3-AUDIT.md` for the full
report and `metak-orchestrator/STATUS.md` § T18.3 for the per-variant
verdict and the recommended follow-up `T18.3a` (close the websocket
compact-buffer gap; option-set documented). The audit is closed; the
follow-up is orchestrator-filed.

### T18.3a — Close websocket compact-buffer gap [medium]

**Repo**: `variant-base/` (logger surface), `variants/websocket/` (call sites).
**Owner**: orchestrator → spawn worker.
**Status**: filed 2026-05-19. **Spawn after T18.5+T18.6 settles** to
keep concurrent-worker risk low.

**Background**: T18.3 audit (commit `892048a`) found that
`variants/websocket/src/websocket.rs` calls `Logger::log_receive`
directly from its Multi-mode `reader_thread_main` (line ~827) and
its Single-mode T17.5 `drain_current_peer_into_logger` helper (line
~682). `Logger::log_receive` writes the legacy JSONL line but does
NOT push into the driver's compact `EventBuffer`. After T18.2 +
T18.2b shipped the compact-default writer, those receives would be
missing from `*.compact.parquet`. The receive contract (delivery %,
latency, integrity) breaks on compact-only data from websocket.

The audit explicitly recommends option (ii) below to preserve the
T14.10 one-mutex-acquisition optimisation that motivated the
direct-`Logger::log_receive` pattern in the first place.

**Fix choice** (worker confirms during implementation, may override
with rationale):

- **Option (ii) — `LoggerHandle::record_receive` (RECOMMENDED)**: add a
  new method on the public `LoggerHandle` surface that performs both
  the JSONL line write AND the compact-buffer push under a single
  mutex acquisition. Internally it delegates to the existing `Logger`
  + the shared `EventSink`. Websocket's reader thread and T17.5
  drain helper switch from `Logger::log_receive` to
  `LoggerHandle::record_receive`. The one-mutex-acquisition cost the
  T14.10 design optimised for is preserved.
- **Option (i) — extend `Logger::log_receive` to also push compact**:
  simpler API but requires `Logger` to hold a reference to the
  compact buffer, which couples it more tightly to the driver's
  `EventSink` infrastructure. Likely the wrong layering.

**Files**:
- `variant-base/src/logger.rs` (or wherever `LoggerHandle` lives) —
  add `record_receive`. Internal call path: log the legacy JSONL line
  (if `--legacy-jsonl-events ON`), then push into the compact buffer.
- `variants/websocket/src/websocket.rs` — replace the two
  `Logger::log_receive` call sites with `LoggerHandle::record_receive`.
- Possibly `variant-base/src/driver.rs` if the `LoggerHandle` needs
  to learn about the `EventSink` (likely already wired by T18.2b).
- Unit tests in both crates for the new path.

**Tests are mandatory**:
```
cargo test --release -p variant-base -p variant-websocket
cargo clippy --release --workspace --all-targets -- -D warnings
cargo fmt --check
```

**Acceptance**:
- A websocket spawn at `1000x100hz qos4 multi` (T17.5 reproducer)
  produces a `.compact.parquet` whose receive count matches the
  legacy JSONL receive count exactly.
- Multi-mode delivery still 100% per T17.5 acceptance.
- Single-mode delivery still 100% per T17.5 acceptance.
- No regression in `cargo test -p variant-websocket`.

### T18.4 — analysis: load both compact and legacy formats [medium]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (Wave 2, after T18.2).
**Goal**: Add a format detector + compact loader to the analysis
pipeline. Existing pivot/integrity/plot code stays unchanged
downstream of the parse layer.

**Files**:
- `analysis/parse.py` — detect `.compact.parquet` vs `.jsonl` per
  spawn; pick the right loader.
- New `analysis/parse_compact.py` — read a compact-parquet file,
  expand its columnar arrays back to the `SHARD_SCHEMA` projection
  used by the existing polars pipeline.
- `analysis/cache.py` — bump `SCHEMA_VERSION` (legacy + compact rows
  map to a slightly different shape with the new `writer_idx` /
  `path_idx` mapping resolved).
- `analysis/tests/` — round-trip test against a small
  `.compact.parquet` fixture produced by VariantDummy. Compare
  expanded output to the equivalent legacy JSONL parse output for
  the same spawn — should be byte-equivalent in the columns that
  matter.

**Acceptance**: re-running analyze on a directory containing both
compact + legacy spawn files produces a coherent unified output.

### T18.5 — runner: `--log-dir <path>` arg [medium]

**Repo**: `runner/`.
**Owner**: orchestrator → spawn worker (Wave 2, parallelisable).
**Goal**: Add a runner CLI flag + TOML key for output log directory.
The runner uses it as the working directory for spawned variants
(so variant `--log-dir` resolves there) and as the destination for
the runner's own coordination logs.

**Files**:
- `runner/src/cli.rs` (or wherever) — new `--log-dir <path>` arg.
- TOML schema: new `[runner] log_dir = "..."` key.
- Cross-platform path handling — UNC paths on Windows
  (`\\server\share`), mounted NFS / SMB on Linux. Worker validates
  the path is writable at runner startup; fails loudly otherwise.

**Acceptance**: a runner pointed at a shared network folder writes
the spawn logs into that folder without errors. Both flag-driven
and TOML-driven entry produce identical behaviour.

### T18.6 — runner: `--analyze-full` arg [medium]

**Repo**: `runner/`.
**Owner**: orchestrator → spawn worker (Wave 2, parallelisable).
**Goal**: Add a runner CLI flag that, after every spawn in the matrix
completes, runs the analysis tool on the final log directory.

**Files**:
- `runner/src/cli.rs` — new `--analyze-full` flag.
- Runner main loop — after the matrix exits, shell out to
  `python analysis/analyze.py <log-dir> --summary --dump --diagrams
  --output <log-dir>/analysis`.
- Python interpreter resolution: prefer `python3`, fall back to
  `python`; fail loudly if neither is on PATH.
- Repo-root detection: walk up from the runner binary location until
  the `analysis/` folder is found.
- **Only ONE runner runs the analysis** — the lower-sorted-index
  runner (alice in the typical `alice` / `bob` pair), matching the
  pair-convention used by websocket / webrtc / hybrid.
- Captures Python stderr/stdout, surfaces to runner stderr.
- Non-zero Python exit → runner-level warning, not hard failure
  (the benchmark itself succeeded).

**Acceptance**: a matrix run with `--analyze-full` set produces the
expected `<log-dir>/analysis/` subfolder (`summary_*.md`, pivot
tables, drop-rate heatmaps).

### T18.7 — User-owned: re-run + validation [medium]

**Repo**: operational (no code).
**Owner**: **user** (runs on real hardware).
**Wave**: 3 (after T18.2-T18.6 land).

**Procedure**:
1. Build + deploy post-T18.6.
2. Re-run `configs/two-runner-all-variants.toml` with
   `--log-dir <shared-folder> --analyze-full`.
3. Confirm:
   - One `.compact.parquet` per spawn in the shared folder.
   - File sizes 30-50× smaller than the equivalent legacy run.
   - `<log-dir>/analysis/` subfolder produced automatically.
   - Pivot tables / heatmaps numerically match a parallel legacy-
     JSONL run on the same workloads.

**Acceptance**: user signs off on the new pipeline.

---

## E19 — Workload-Shape Dimension

See `EPICS.md` E19 for the locked spec, motivation, and sequencing.
Wave structure: Wave 1 = T19.2 + T19.5 + T19.7 (parallel, need only
contracts). Wave 2 = T19.3 + T19.6. Wave 3 = T19.4. Wave 4 = T19.8.

### T19.1 — Contract updates [orchestrator-self, DONE]

**Repo**: `metak-shared/api-contracts/` + `metak-shared/glossary.md`.
**Owner**: orchestrator (no worker spawned).
**Status**: landed with E19 filing 2026-05-19 — see "E19 additions"
sections at the bottom of:

- `jsonl-log-schema.md` — `leaf_count` + `shape` on `write` events.
- `compact-log-schema.md` — `leaf_count` column + `shape_intern` dict
  on the columnar event table.
- `toml-config-schema.md` — new flat `[variant.common]` keys
  (`blob_size`, `mixed_scalars_*`, `mixed_arrays_*`,
  `mixed_dict_split_max`, `workload_seed`).
- `variant-cli.md` — corresponding `--kebab-case` CLI args +
  validation rules.
- `glossary.md` — WriteOp, Leaf, Blob, leaf_count, shape,
  block-flood, mixed-types.

### T19.2 — variant-base: workload structs + WriteOp + logger [large]

**Repo**: `variant-base/`.
**Owner**: orchestrator → spawn worker (Wave 1).
**Goal**: Add the two new workload profiles and extend the
log-emission path to record the new shape metadata. Existing
`scalar-flood` and `max-throughput` paths remain unchanged.

**Files**:

- `variant-base/src/workload.rs` — extend `WriteOp` with `leaf_count:
  u32` and `shape: WriteShape` (enum `{ Scalar, Array, Struct }`).
  `ScalarFlood::generate` emits `leaf_count = 1, shape = Scalar`.
  New `BlockFlood` struct parameterized by `blob_size`, emits
  `leaf_count = blob_size, shape = Array` per WriteOp. New
  `MixedTypes` struct parameterized by
  `(mixed_scalars_min, mixed_scalars_max, mixed_arrays_min,
  mixed_arrays_max, mixed_dict_split_max, workload_seed)`, generates
  a per-tick mix per the EPICS.md E19 locked-spec algorithm.
- `variant-base/src/logger.rs` (and the compact emission path) —
  emit `leaf_count` and `shape` on every `write` event. Both JSONL
  and compact-Parquet writers updated. Pull `shape_intern` from
  `compact-log-schema.md` E19 additions.
- `variant-base/src/workload.rs::create_workload` — add the two new
  names. Both new profiles need a configurable random seed
  (`--workload-seed`).

**Mixed-types allocation algorithm** (T19.2 owns the termination
strategy; below is the locked-spec template):

1. Draw `nS = rand(mixed_scalars_min, mixed_scalars_max)`. Emit `nS`
   scalar WriteOps with random `f64` payloads.
2. `remaining = vpt - nS`. Draw
   `nA = rand(mixed_arrays_min, min(mixed_arrays_max,
   remaining / 2))`. Distribute `nA` total leaves across
   `k = rand(1, mixed_arrays_max)` array WriteOps using a uniform
   random partition; each array WriteOp has `shape = Array`.
3. `remaining = vpt - nS - nA`. Recursively allocate `remaining`
   leaves to nested-dict WriteOps:
   - If `remaining == 1`: emit one scalar WriteOp.
   - Else if `remaining <= mixed_dict_split_max`: emit one Struct
     WriteOp with `remaining` leaves (flat dict, terminates
     recursion).
   - Else: draw `k = rand(1, mixed_dict_split_max)`. Partition
     `remaining` into `k` positive integers; recurse for each
     child. Termination guarantee: depth bound =
     `log_2(vpt) + 4`; if reached, force a flat dict at that
     level.

Use the `rand` crate's `StdRng` seeded from `--workload-seed`. When
the seed is unset, derive deterministically from `--variant +
--run`.

**Tests** (in `variant-base/`):

- Unit: `BlockFlood::generate(1000)` with `blob_size = 100` emits 10
  WriteOps each with `leaf_count = 100, shape = Array,
  payload.len() == 800`.
- Unit: `BlockFlood::generate(vpt)` returns Err when
  `vpt % blob_size != 0`.
- Unit: `MixedTypes::generate(1000)` with a fixed seed produces
  WriteOps whose `leaf_count` values sum to exactly 1000.
- Unit: `MixedTypes::generate` with the same seed twice produces
  identical output (determinism).
- Unit: `MixedTypes::generate(N)` for N in {1, 10, 100, 1000}
  always sums leaves to N.
- Integration: VariantDummy spawn with `--workload block-flood
  --blob-size 100 --values-per-tick 1000`, JSONL contains writes
  with `leaf_count = 100, shape = "array"`.
- Integration: same with `--workload mixed-types` — JSONL contains
  a mix of scalar / array / struct shapes summing to `vpt` per
  tick.

**Acceptance**:

- All new unit + integration tests pass.
- Existing `scalar-flood` and `max-throughput` tests pass unchanged
  (no regression).
- `cargo clippy --release -p variant-base -- -D warnings` clean.
- `cargo fmt -p variant-base -- --check` clean.
- A `variant-dummy` smoke run at `mixed-types vpt=1000` produces a
  valid JSONL file (parsing verified after T19.5 lands).

### T19.3 — variant-base: CLI plumbing + validation [medium]

**Repo**: `variant-base/`.
**Owner**: orchestrator → spawn worker (Wave 2 — after T19.2).
**Goal**: Expose the new workload params on the variant CLI and
validate them at startup.

**Files**:

- `variant-base/src/cli.rs` — add fields:
  - `blob_size: Option<u32>` (`--blob-size`, default `100` when
    `--workload block-flood`)
  - `mixed_scalars_min: Option<u32>` (`--mixed-scalars-min`)
  - `mixed_scalars_max: Option<u32>` (`--mixed-scalars-max`)
  - `mixed_arrays_min: Option<u32>` (`--mixed-arrays-min`)
  - `mixed_arrays_max: Option<u32>` (`--mixed-arrays-max`)
  - `mixed_dict_split_max: Option<u32>` (`--mixed-dict-split-max`)
  - `workload_seed: Option<u64>` (`--workload-seed`)
- `variant-base/src/driver.rs::run_protocol` — pass these into the
  workload factory at the same point where `create_workload(name)`
  is called today. Existing workload selection unchanged for
  `scalar-flood` and `max-throughput`.

**Validation** (`driver::run_protocol` early-return with descriptive
Err, BEFORE any phase emission):

- `workload == "block-flood"` requires `blob_size > 0` and
  `values_per_tick % blob_size == 0`.
- `workload == "mixed-types"` requires all five `mixed_*` params to
  be set, with `mixed_scalars_max <= values_per_tick`,
  `mixed_arrays_max <= values_per_tick - mixed_scalars_min`, and
  `mixed_dict_split_max >= 2`.
- Block-size sanity warning (stderr, not Err): if `blob_size * 8 >
  65536`, emit a one-shot warning recommending the operator check
  variant-specific buffer hints.

**Smoke test against existing variants**: run each concrete variant
binary (variant-dummy, custom-udp; others optional in this task) at
`block-flood vpt=1000 blob_size=100`. Confirm no startup errors and
a non-empty JSONL. Validates the "no code changes needed in
variants" assumption; if any variant breaks, escalate to the
orchestrator before patching.

**Tests**:

- Unit: CLI parse with all new fields populated.
- Unit: validation rejects `vpt=1000 blob_size=300` (not divisible).
- Unit: validation rejects `mixed-types` with missing params.
- Integration: variant-dummy spawn at `block-flood vpt=1000
  blob_size=100` completes successfully.

**Acceptance**:

- All tests pass.
- Smoke test against existing variants passes for at least
  variant-dummy + custom-udp.
- `cargo clippy --release -p variant-base -- -D warnings` clean.

### T19.4 — runner: TOML schema + CLI forwarding [small]

**Repo**: `runner/`.
**Owner**: orchestrator → spawn worker (Wave 3 — after T19.3).
**Goal**: Recognize the new keys in `[variant.common]` and forward
them as `--kebab-case` CLI args. The runner does not interpret the
new params — it only forwards them.

**Files**:

- `runner/src/config.rs` (or wherever TOML is parsed) — add the new
  optional keys.
- `runner/src/spawn.rs` (or wherever variant CLI args are
  constructed) — forward the new keys as `--blob-size N`, etc.
  Follow the existing `snake_case` → `--kebab-case` convention.

**Tests**:

- Unit: TOML with `blob_size = 100` parses; spawned CLI line
  contains `--blob-size 100`.
- Unit: TOML missing the new keys still parses (backward compat).
- Integration: a two-runner config using `workload = "block-flood"`
  runs to completion using `variant-dummy` for speed.

**Acceptance**:

- Tests pass.
- `cargo clippy --release -p runner -- -D warnings` clean.

### T19.5 — analysis: parse + correlate + per-shape metrics [medium]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (Wave 1 — parallel with T19.2
and T19.7; only needs the contract).
**Goal**: Read the new `leaf_count` / `shape` fields, propagate them
onto correlated receives, surface leaves-vs-ops separately in the
headline metrics.

**Files**:

- `analysis/schema.py` + `analysis/parse.py` — read `leaf_count`
  (default `1`) and `shape` (default `"scalar"`) from JSONL `write`
  events and from compact-Parquet rows. The compact schema needs a
  column for `shape_idx` + a `shape_intern` dictionary in KV
  metadata — coordinate with the writer-side once T19.2 lands.
- `analysis/correlate.py` — when joining receives to writes by
  `(writer, seq, path)`, copy `leaf_count` and `shape` onto the
  receive row.
- `analysis/performance.py` — separate three throughput numbers:
  - `ops_per_sec` = receives / operate_secs.
  - `leaves_per_sec` = sum(leaf_count) / operate_secs.
  - `bytes_per_sec` = sum(bytes) / operate_secs.
- `analysis/tables.py` — add `shape` and `leaves_per_sec` to the
  pivot tables. Existing scalar-flood rows show `leaves_per_sec ==
  ops_per_sec`.
- `analysis/integrity.py` — leaves-lost rate: when seq gaps imply
  lost ops, multiply by `leaf_count` for total lost leaves.
- Cache version bump in `analysis/cache.py` if the SHARD_SCHEMA
  changes shape.

**Backfill**: legacy JSONL events without `leaf_count` / `shape`
default to `1` / `"scalar"`. Compact-Parquet files predating E19
default the same way.

**Tests**:

- Unit: parse a JSONL fixture with mixed `leaf_count` values;
  confirm correct propagation through correlate → performance.
- Unit: legacy JSONL (no `leaf_count` / `shape`) parses with
  defaults.
- Unit: a block-flood JSONL fixture produces `ops_per_sec ==
  leaves_per_sec / blob_size`.

**Acceptance**:

- All tests pass.
- Re-running `analyze.py` on an existing pre-E19 directory produces
  the same numbers as before plus new columns defaulting to `1` /
  `"scalar"`.

### T19.6 — analysis: plots [small-medium]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (Wave 2 — after T19.5).
**Goal**: Restructure the existing `comparison-qos` chart and add a
per-variant throughput-vs-shape chart, per the locked layout
2026-05-19.

**Files**:

- `analysis/plots.py` — chart updates:

  **(a) Existing `comparison-qos` chart restructure**:

  - Stack the two subplots **vertically** (matplotlib `nrows=2,
    ncols=1`), not side-by-side.
  - Preserve per-variant grouping on the x-axis.
  - Each per-variant group's bars subdivide by
    `(workload × threading_mode)`:
    - **Workload distinguished by fill pattern (matplotlib `hatch`)**:
      - `scalar-flood` — solid fill (no hatch, `hatch=""`).
      - `block-flood` — horizontal-line hatch (`hatch="-"` or
        `"---"` for density).
      - `mixed-types` — checkered hatch (`hatch="+"` or `"x"` —
        worker picks the most readable; document choice).
    - **Threading_mode distinguished by color** (preserve the
      existing `single` vs `multi` palette).
    - Variants that don't support multi threading_mode simply emit
      fewer bars in their group — no placeholder bars.
  - Legend lists workload × threading_mode combinations cleanly
    (use a 2-row legend or two separate legends — one for fill
    pattern, one for color).

  **(b) New chart `throughput_vs_workload_shape`**:

  - Per-variant subplot grid (one subplot per variant in the
    dataset).
  - X-axis: workload profile (sorted scalar-flood → block-flood-* →
    mixed-types).
  - Y-axis: `leaves_per_sec`.
  - One bar group per QoS within each subplot (or a sibling chart
    per QoS — worker picks based on what fits visually).
  - Same fill-pattern / color convention as (a) for cross-chart
    consistency.

- `analysis/pivot_tables.py` — add `workload` and `shape` as
  optional pivot dimensions; existing default behaviour unchanged.

**Tests**:

- Smoke test: fixture with three workload rows produces a valid PNG
  for both charts.
- Visual regression: verify both new fill patterns render
  distinctly (compare per-bar hatch attributes).

**Acceptance**:

- Tests pass.
- Generated charts render cleanly without overlap on the
  three-workload fixture.
- Manual inspection confirms the vertical stack and per-variant
  grouping in the restructured `comparison-qos`.

### T19.7 — docs: glossary + BENCHMARK.md § 6 [small]

**Repo**: `metak-shared/`.
**Owner**: orchestrator → spawn worker (Wave 1 — parallel with T19.2
and T19.5).
**Goal**: Bring the user-facing documentation in line with the
locked spec.

**Files**:

- `metak-shared/glossary.md` — already updated by T19.1 (verify the
  new terms render correctly; add any missing nuance).
- `metak-shared/BENCHMARK.md` § 6 — replace the workload-profile
  table with one that documents all four profiles (`scalar-flood`,
  `max-throughput`, `block-flood`, `mixed-types`) and their
  parameters per the locked E19 spec. Cross-reference EPICS.md E19
  for the locked algorithm.

**Acceptance**:

- New terms render cleanly.
- BENCHMARK.md § 6 matches the EPICS.md E19 locked-spec text.

### T19.8 — E2E validation [medium]

**Repo**: operational (no code unless a config tweak is needed).
**Owner**: orchestrator → spawn worker (Wave 4 — last, after
T19.2-T19.7 land).
**Goal**: Run a two-runner config with the three workload shapes
back-to-back and confirm the analysis produces a coherent
three-workload comparison.

**Procedure**:

1. Create or extend `configs/two-runner-workload-shapes.toml` with
   three `[[variant]]` entries:
   - `scalar-flood` (existing default).
   - `block-flood blob_size=100`.
   - `mixed-types` with reasonable defaults (e.g.
     `mixed_scalars_min=5, mixed_scalars_max=20, mixed_arrays_min=
     200, mixed_arrays_max=600, mixed_dict_split_max=4`).
   Use `variant-dummy` or `custom-udp` as the single variant —
   workload-shape sensitivity is the dimension under test, not
   transport diversity.
2. Run two runners on localhost. Capture the JSONL + compact-Parquet
   output.
3. Run `python analysis/analyze.py <log-dir> --summary --diagrams`.
4. Confirm:
   - Three rows in the pivot table, one per workload, with
     distinct `ops_per_sec` and identical (or very close)
     `leaves_per_sec` if the transport keeps up.
   - The new "throughput vs workload shape" chart renders.
   - The restructured `comparison-qos` chart renders with vertical
     stack + per-variant grouping + fill-pattern workload
     distinction.
   - No `backpressure_skipped` events at QoS 3/4 (would indicate
     regression in the E17 contract).

**Acceptance**:

- All three workload spawns complete cleanly.
- Analysis output is coherent and matches manual sanity checks
  (e.g. block-flood has fewer ops than scalar-flood at the same
  vpt).
- New charts pass visual inspection.

### T19.9 — analysis: post-validation UX fixes [small]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (Wave 5 — post-T19.8 acceptance).
**Goal**: Fix four analyzer presentation bugs surfaced by T19.8 E2E
validation. See T19.8's STATUS.md completion report for the exact
issue numbering.

**Scope**:

1. **Issue #2 + #4 (`pivot_tables.py` regex)** — the regex that
   extracts dimensions from variant names falls back to `n/a` when
   the name lacks the standard `-<vpt>x<hz>hz-qos<N>` suffix. Fix:
   prefer the actual data columns (`qos` is on every event row,
   `threading_mode` is on `connected` events) over name-parsing.
   The "family" / "base" name extraction should fall back to the
   full variant name when no suffix is detectable, not to `n/a`.
2. **Issue #3 (`throughput_vs_workload_shape` x-axis)** — change
   the chart x-axis labels to **workload name** (`scalar-flood`,
   `block-flood`, `mixed-types`), NOT shape. Mixed-types' dominant
   shape is `array`, which currently makes its bar land on the
   "array" tick — wrong intent.
3. **Issue #5 (sort order)** — canonical order:
   - Shape: `scalar` → `array` → `struct`.
   - Workload: `scalar-flood` → `block-flood` → `mixed-types` →
     `max-throughput`. Append unknown profiles alphabetically.
   - Make these `CANONICAL_SHAPE_ORDER` / `CANONICAL_WORKLOAD_ORDER`
     module-level constants.

**Verification**: re-run `analyze.py` on the T19.8 fixture session
(`logs/wlshapes-single-20260519_165233`). Confirm pivot cells
populate (no `n/a` columns) and chart labels match canonical order.

**Tests**:
- Unit: pivot_tables.py parses unsuffixed variant names without
  `n/a` fallback.
- Unit: canonical sort order applied.
- Visual regression: chart x-axis labels are
  `["scalar-flood", "block-flood", "mixed-types"]` when those three
  workloads are present.

**Acceptance**: re-running the T19.8 fixture produces a clean pivot
table and correctly-labeled charts.

### T19.10 — Legacy JSONL cleanup

User directive 2026-05-19 (post-T19.8 acceptance): "we don't have
or want to ever keep any legacy behaviour, clear it out please." The
target is the E18 dual-emission opt-in (`--legacy-jsonl-events`)
that flagged a bool-forwarding bug during T19.4. After this cleanup:

- **JSONL holds lifecycle events only** (`phase`, `connected`,
  `eot_*`, `resource`, `clock_sync`).
- **Per-event observations** (`write`, `receive`,
  `backpressure_skipped`, `gap_detected`, `gap_filled`) are
  **compact-Parquet only**.
- **Contracts already updated** (orchestrator-self, landed
  2026-05-19): jsonl-log-schema.md strips per-event sections;
  compact-log-schema.md replaces "Coexistence with legacy JSONL"
  with "Per-spawn file pair"; the aggregate-throughput +
  latency-unit narrative moved into compact-log-schema.md's E19
  additions.

Split into per-repo sub-tasks.

### T19.10a — variant-base: drop --legacy-jsonl-events + dual emission [medium]

**Repo**: `variant-base/`.
**Owner**: orchestrator → spawn worker (Wave 5 — can run in parallel
with T19.10b; both independent of T19.9).
**Goal**: Remove the variant-side opt-in path entirely. Per-event
data flows only to compact buffers / Parquet.

**Files**:
- `variant-base/src/cli.rs` — remove the `legacy_jsonl_events` field
  and its clap derive entry.
- `variant-base/src/driver.rs` — remove the dual-emission gate /
  branching on the flag.
- `variant-base/src/logger.rs` — drop the JSONL emission paths for
  `write`, `receive`, `backpressure_skipped`, `gap_detected`,
  `gap_filled`. **Keep** the JSONL paths for `phase`, `connected`,
  `eot_sent`, `eot_received`, `eot_timeout`, `resource` — those
  remain unchanged.
- `variant-base/tests/integration.rs` — remove tests that exercise
  the opt-in path; remove any fixtures that inspect per-event JSONL.
  Lifecycle JSONL inspection stays.
- `variant-base/STRUCT.md` — drop references to the flag.
- `variant-base/CUSTOM.md` — update the "Workload-shape dimension
  (T19.2 + T19.3 / E19)" section: "every `write` row (compact
  Parquet) carries `leaf_count` and `shape`" — not "JSONL + compact
  Parquet". Also drop any other `--legacy-jsonl-events` mentions.

**Validation**:
```
cargo test --release -p variant-base
cargo clippy --release -p variant-base -- -D warnings
cargo fmt -p variant-base -- --check
cargo build --release   # confirm all variants still compile
```

**Smoke test**: spawn variant-dummy at scalar-flood + block-flood,
inspect the produced files — JSONL should contain ONLY lifecycle
events, compact-Parquet should contain the per-event rows.

**Acceptance**: variant-base no longer accepts or recognizes
`--legacy-jsonl-events`. Per-event data appears exclusively in
compact-Parquet. Lifecycle events appear exclusively in JSONL.
All existing variant binaries continue to spawn and complete
cleanly (no source-code changes expected outside variant-base).

### T19.10b — runner: drop legacy_jsonl_events forwarding [small]

**Repo**: `runner/`.
**Owner**: orchestrator → spawn worker (Wave 5 — can run in parallel
with T19.10a; both independent of T19.9).
**Goal**: Remove forwarding of the `legacy_jsonl_events` TOML key.
Sidesteps the bool-forwarding bug that T19.4 surfaced.

**Files**:
- `runner/src/config.rs` (or wherever) — remove parsing of
  `legacy_jsonl_events`.
- `runner/src/cli_args.rs` (or wherever) — remove forwarding of the
  key. If the forwarding is via a generic key-skip list, ensure
  the runner does NOT forward this key (it's gone from variant-base
  CLI).
- `runner/CUSTOM.md` — remove any reference to the flag.

**Validation**:
```
cargo test --release -p runner
cargo clippy --release -p runner -- -D warnings
cargo fmt -p runner -- --check
```

**Acceptance**: a TOML config that sets `legacy_jsonl_events = true`
in `[variant.common]` either is rejected at parse time OR is silently
ignored — worker's call, but be explicit in the completion report.
Variant-base spawning works unchanged.

### T19.10c — analysis: drop per-event JSONL parser path [medium]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (Wave 6 — AFTER T19.9 lands;
both modify analysis/parse.py and cache.py).
**Goal**: Remove the analyzer's ability to parse per-event JSONL.
JSONL handling is now lifecycle-events-only. Per-event data comes
exclusively from compact-Parquet via `parse_compact.py`.

**User directive context**: "We won't ever need to load historic
data in jsonl, just use it for the lifecycle event log." So old
pre-T18.2 JSONL datasets that contain per-event rows are NO LONGER
supported. The analyzer may emit a warning if it encounters such
data, but should not attempt to load it.

**Files**:
- `analysis/parse.py` — strip the per-event JSONL branches (`write`,
  `receive`, `backpressure_skipped`, `gap_detected`, `gap_filled`).
  JSONL parsing keeps `phase`, `connected`, `eot_sent`,
  `eot_received`, `eot_timeout`, `resource`, `clock_sync`. If
  per-event types are encountered in a JSONL file, log a one-shot
  warning per spawn (`"<file>: ignoring N pre-T18.2 per-event
  JSONL rows; compact-Parquet is now the only path"`) and skip
  them.
- `analysis/schema.py` — `SHARD_SCHEMA` adjustments if the JSONL
  vs Parquet schemas diverged.
- `analysis/cache.py` — bump `SCHEMA_VERSION` (this is structural).
- `analysis/correlate.py` — if it had any logic to dedup
  JSONL-sourced rows against Parquet-sourced rows for the same
  spawn, remove it.
- `analysis/tests/` — remove fixtures that simulate per-event JSONL
  data. Keep lifecycle JSONL fixtures.
- `analysis/CUSTOM.md` — update the "Workload-shape dimension
  (T19.5 + T19.6 / E19)" section to drop the "legacy JSONL"
  reference: per-event data comes only from compact-Parquet.

**Validation**:
```
cd analysis && python -m pytest -v
```

**Acceptance**:
- All tests pass.
- Running `analyze.py` on a directory containing both lifecycle
  JSONL + compact-Parquet (the new post-cleanup norm) produces a
  coherent report.
- Running `analyze.py` on an old pre-T18.2 directory (per-event
  JSONL only, no compact-Parquet) produces a clear warning and
  empty per-event tables — no crash.
- T19.9's fixture re-run still passes.

**Sequencing**: must spawn AFTER T19.9 lands because both edit
`analysis/parse.py` and `analysis/cache.py`. T19.10c picks up T19.9's
schema version (whatever it ended at) and bumps from there.

### T19.11 — variant-base: remove vestigial `attach_compact_sink` bool param [small]

**Repo**: `variant-base/` + `variants/websocket/` (the only consumer of
the shim).
**Owner**: orchestrator → spawn worker (post-E19 polish).
**Context**: T19.10a kept `LoggerHandle::attach_compact_sink`'s second
`bool` argument as a no-op shim because removing it would have broken
the websocket variant's in-tree test at `variants/websocket/src/websocket.rs:1829`,
and that file was outside the T19.10a worker's variant-base/ scope.
This task closes the loop.

**Files**:
- `variant-base/src/logger.rs` — find `LoggerHandle::attach_compact_sink`,
  remove the no-op `bool` parameter. Confirm via grep that no other
  in-tree caller passes a meaningful argument.
- `variants/websocket/src/websocket.rs` (around line 1829) — update the
  call site to drop the `bool` argument.
- Any other call site that the orchestrator may have missed (grep
  `attach_compact_sink` across the workspace; expect only
  `variant-base/` callers + the one websocket test).

**Tests**:
- `cargo test --release -p variant-base`
- `cargo test --release -p variant-websocket` (the test that previously
  forced the shim must keep passing)
- `cargo clippy --release --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo build --release` (whole workspace)

**Acceptance**:
- No call sites pass the bool param anymore.
- All tests + clippy + fmt pass.
- `git grep -n attach_compact_sink` shows only the new (cleaner)
  signature and call sites.

### T19.12 — analysis: fix `Shape` column for mixed-types [small]

**Repo**: `analysis/`.
**Owner**: orchestrator → spawn worker (post-E19 polish).
**Context**: T19.8 flagged this as issue #6, and T19.9 explicitly
excluded it from scope. T19.5's worker had pointed at the right escape
hatch: `PerformanceResult.shape` is currently a single dominant value
per group, so when `mixed-types` happens to have `array` as its
dominant shape, the pivot row shows `Shape: array` for the mixed-types
row — misleading.

The fix: compute the Shape display value from the delivery-level
columns (which already carry per-WriteOp shape) instead of from
`PerformanceResult.shape`. If a group spans multiple shapes, render
`mixed` (or the canonical name of the workload, if available — worker
picks). For groups with a single shape, render that shape.

**Files**:
- `analysis/performance.py` and/or `analysis/tables.py` — adjust the
  Shape column derivation. The pivot should consult the underlying
  delivery rows for shape distribution rather than relying on a single
  scalar field.
- `analysis/tests/` — add coverage:
  - A `scalar-flood` fixture renders `Shape: scalar`.
  - A `block-flood` fixture renders `Shape: array`.
  - A `mixed-types` fixture renders `Shape: mixed` (or the workload
    name).

**Validation**:
```
cd analysis && python -m pytest -v
```

Net should still pass (T19.10c baseline was 433 / 6).

**Smoke check**: re-run the analyzer on the T19.8 fixture session
(`logs/wlshapes-single-20260519_165233`); confirm the mixed-types row
no longer shows `array`.

**Acceptance**:
- Mixed-types pivot row no longer displays a single dominant shape.
- Tests pass.
- Smoke check confirms.
