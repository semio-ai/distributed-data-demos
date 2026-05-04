# Learned

Things discovered during development that are useful for future work.

<!-- When any agent discovers useful methods, procedures, or tricks, document them here. -->
<!-- Each entry should have a Discovery (what was learned) and an Implication (how to apply it). -->

## Cross-machine validation reveals failures invisible on localhost (E9 → E10)

**Discovery**: Same-machine two-runner runs and cross-machine two-runner runs
expose materially different failure modes. Three concrete differences caught
between the localhost and the alice/bob (192.168.1.80 ↔ 192.168.1.77) runs of
`configs/two-runner-all-variants.toml`:

1. **Cross-machine TCP teardown is slower than loopback.** The custom-udp
   panic at `src/udp.rs:233` (slice-out-of-range when the read frame-length
   prefix arrives torn or zero-valued) only reproduced cross-machine. On
   loopback the OS atomically tears down both ends so the read either gets
   a complete frame or a clean EOF; across the wire there's a real window
   where a partial read returns 0-3 bytes.
2. **Non-blocking UDP send returns `WSAEWOULDBLOCK` on Windows under load.**
   The hybrid variant's UDP send path bailed with error 10035 at high
   throughput across the network — but the same workload completed cleanly
   on loopback. Loopback effectively never fills the kernel send buffer
   because the receiver is the same kernel; cross-machine the NIC drains
   slower and the buffer fills.
3. **Same-host hangs are sometimes same-host artifacts.** Zenoh's
   asymmetric `100 vps × 10 hz` hang (alice always timed out, bob always
   succeeded) on localhost was a same-host artifact — both sides cleared
   on cross-machine. Don't chase deterministic-looking same-host hangs as
   real bugs without confirming on cross-machine.

**Implication**:
- Variant validation that only ran two runners on localhost is necessary
  but not sufficient. Always do at least one cross-machine smoke before
  declaring a transport-touching change "done."
- Treat partial reads, EOF, `WOULDBLOCK`, and `CONNABORTED`/`CONNRESET` as
  expected events at high throughput, not exceptions to bail on. Apply
  blocking semantics or retry loops at the transport layer; let the
  protocol driver decide what to do with them.
- A rough heuristic for the test matrix: `localhost(short, low-tput) →
  localhost(long, high-tput) → cross-machine(short, low-tput) →
  cross-machine(long, high-tput)`. Each step catches different failure
  modes; skipping the cross-machine ones leaves a real gap.

## Jitter windowing: fixed grid vs rolling start (E11)

**Discovery**: The Phase 1.5 polars implementation of jitter
(`analysis/performance.py::_jitter`) divides each group's `latency_ms`
series into non-overlapping 1-second windows by computing
`window_id = floor((receive_ts - receive_ts.min()) / 1s)` and
aggregating per-`window_id` sample standard deviation. Phase 1's
row-iterating implementation instead carried a "current window start"
cursor that advanced only when the current row's timestamp crossed
`current_start + 1s`, then reset `current_start` to that row's
timestamp. The two definitions agree exactly when receive timestamps
are uniformly distributed, but diverge by up to one window-boundary
per gap whenever the stream has idle stretches longer than one second:
Phase 1 re-anchors the next window to the first post-gap arrival,
while polars keeps the windows pinned to a fixed 1-second grid
relative to the group's first arrival. On the same-machine 3.6 GB
regression dataset this produced 4-15% relative differences on
`Jitter avg`/`Jitter p95` while leaving every other metric (delivery
counts, latency p50/p95/max, throughput, loss) byte-identical.

**Implication**: The polars form is preferred because it is O(n) in
Arrow buffers with no Python row iteration (essential for the 40 GB
dataset), and because a fixed grid is the standard interpretation of
"1-second windowed jitter" in the broader latency-measurement
literature. Phase 1's rolling-start-window form is not wrong, just
narrower in scope. Treat the small jitter-only deltas vs Phase 1
output as a deliberate, documented design choice; integrity counts
and other latency / throughput / loss metrics remain regression-
exact.

## Windows `FIONBIO` is socket-wide, not per-handle

**Discovery**: `TcpStream::try_clone` returns a second handle to the same kernel
socket. On Linux, calling `set_nonblocking(true)` on one clone and
`set_nonblocking(false)` on the other gives you a blocking write half + a
non-blocking read half (Linux uses `O_NONBLOCK` on the file descriptor and
`dup` gives independent flag state). On Windows, blocking mode is
controlled by `ioctlsocket(FIONBIO, ...)` which is a property of the
underlying socket, not the handle — so the second `set_nonblocking` call
silently overwrites the first for *both* handles. The hybrid variant's
T10.1 first attempt did exactly this and the symptom was insidious: TCP
spawns reported `status=success` but cross-runner delivery at high rate
collapsed to ~3 messages out of ~287000 (the writes were quietly hitting
`WSAEWOULDBLOCK` on the supposedly-blocking write half and getting dropped
by the cascading-peer-drop fault tolerance).

**Implication**:
- Don't rely on `try_clone` + per-handle blocking flags as a way to mix
  blocking and non-blocking I/O on a single TCP socket. It's a
  cross-platform footgun even though the API doesn't tell you that.
- Portable alternative when you want a "blocking write, polled read" pattern:
  keep the socket in **blocking** mode and put a small `set_read_timeout`
  (e.g. 1 ms) on the read clone. Polled reads then return either bytes,
  EOF, or `ErrorKind::TimedOut`/`WouldBlock` — same control flow as a
  non-blocking read, but the underlying socket stays blocking so writes
  apply real back-pressure.
- When validating "are TCP writes actually blocking under load," count
  delivered receive records on the peer side rather than trusting
  spawn-level `status=success`. A `success` exit can mask a 99.999%
  drop rate if the read loop tolerates per-peer write failures (which it
  should, but that masks the upstream symptom).

## E9 specifically: name collisions on injected CLI args

**Discovery**: When the runner started injecting `--peers <name=host,...>`
into every variant spawn (T9.1), two of the four variants broke not because
they couldn't handle the new contract but because they already had their own
`--peers` parser expecting a different shape (custom-udp's was old-style
`host:port,host:port`; zenoh strict-bailed on any unknown arg). Hybrid and
QUIC were migrated as part of E9 and tested fine; the gap was that E9's
acceptance only ran the two migrated variants, not a full all-variants
smoke. The collision was caught only on the user's first real two-machine
run.

**Implication**: when the runner introduces a new injected arg name, treat
it as a contract change touching every variant — not just the ones the new
arg is "for." Either run the full all-variants config end-to-end before
claiming the contract change done, or pick injected arg names that can't
collide with existing variant-specific args (e.g. `--runner-peers`).
