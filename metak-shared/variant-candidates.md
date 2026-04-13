# Variant Candidates — E0 Research Results

Research conducted 2026-04-13. Evaluated 18 candidates across three
categories: pub/sub frameworks, raw protocol approaches, and shared
memory / niche transports.

## Evaluation Criteria (from DESIGN.md)

- Local network only (single subnet)
- Leaderless, self-organized topology (no broker)
- Push-based: writer fans out to all readers immediately
- ~100K value updates/sec aggregate
- Replication latency < 10 ms on LAN
- 100 Hz tick rate
- Rust support (native preferred)
- Small messages (8-128 bytes typical, occasionally larger)
- Peer discovery on LAN (zero-conf preferred)
- QoS flexibility (best-effort through reliable)
- Must work on Windows (dev environment) and Linux

---

## Recommended Variants

### R1: Zenoh (framework — best overall fit)

- **Project**: Eclipse Zenoh | https://zenoh.io/
- **Crate**: `zenoh` v1.7.2 (native Rust, actively maintained, monthly releases)
- **Transport**: Configurable hybrid — UDP unicast/multicast, TCP. Peer-to-peer
  mode with optional router nodes.
- **Discovery**: Zero-conf via UDP multicast (224.0.0.224:7446). Gossip fallback
  for non-multicast networks.
- **QoS**: Configurable per-operation reliability and ordering. Runtime QoS
  overwriting via interceptors.
- **Latency**: < 10 us one-way in peer-to-peer mode for packets < 16 KB.
  Outperforms CycloneDDS, MQTT, Kafka in published benchmarks.
- **Throughput**: Well above 100K msg/sec in benchmarks.
- **Fit**: Excellent. Leaderless P2P, zero-conf, push-based pub/sub matches
  single-writer topology perfectly. Native Rust.
- **Concerns**: Async-first API (has blocking wrappers). Newer ecosystem vs
  established alternatives.
- **Windows**: Supported.

**Why include**: The high-level framework baseline. If a mature pub/sub
framework can deliver our latency targets with minimal custom code, that's a
valuable data point.

### R2: Custom UDP (raw protocol — manual control)

- **Approach**: Hand-written protocol on UDP sockets. Multicast for fan-out,
  unicast for targeted messages and NACK recovery.
- **Crates**: `tokio::net::UdpSocket` or `std::net::UdpSocket`, `socket2`
  for advanced config, `mdns-sd` for discovery.
- **Transport**: UDP multicast groups for data distribution. One group per
  QoS level or per writer.
- **Discovery**: mDNS (RFC 6762/6763) via `mdns-sd` crate.
- **QoS**: All four levels implemented manually:
  - L1: fire-and-forget multicast
  - L2: sequence tracking, discard stale
  - L3: sequence gaps + NACK retransmit
  - L4: TCP connection per peer
- **Latency**: 2-5 ms for multicast on LAN. Sub-1ms achievable with tuning.
- **Throughput**: 100K+ msg/sec feasible; bottleneck is NIC, not protocol.
- **Fit**: Excellent. Full control over every layer. Implements the design
  exactly as described in DESIGN.md.
- **Concerns**: MTU fragmentation (keep payloads < 1472 bytes or implement
  app-layer fragmentation). More code to write (~200-400 lines core).
  IPv4 multicast loop option unreliable on Windows (use socket2).
- **Windows**: Works, minor multicast quirks documented.

**Why include**: The "from scratch" baseline. Shows the performance floor
when there's no framework overhead. Directly comparable to every other
variant since it implements the design with zero abstraction.

### R3: Aeron (finance-grade messaging)

- **Project**: Adaptive Aeron | https://aeron.io/
- **Crate**: `rusteron` / `rusteron-client` (community C bindings, unsafe)
- **Transport**: UDP multicast (LAN) or TCP. SPSC/MPSC ring buffers.
  Kernel bypass optional with Aeron Premium.
- **Discovery**: Built-in media driver handles peer coordination.
- **QoS**: Reliable and unreliable modes. Ordering guaranteed per stream.
- **Latency**: 21-39 us P50 at 100K-1M msg/sec (AWS 2025 benchmarks).
  57 us open-source at 100K msg/sec (Google Cloud).
- **Throughput**: Designed for > 1M msg/sec.
- **Fit**: Strong. Cross-machine LAN, push-based, small messages ideal.
  Microsecond latency far exceeds our 10 ms target.
- **Concerns**: C bindings (unsafe Rust). Media driver process required.
  Premium features need licensing. Less idiomatic Rust.
- **Windows**: Supported but less optimized than Linux.

**Why include**: The performance ceiling reference. Finance-grade latency
and throughput. Interesting to see how much a purpose-built messaging
system outperforms general frameworks and raw UDP.

### R4: QUIC via quinn (modern protocol)

- **Project**: quinn-rs | https://github.com/quinn-rs/quinn
- **Crate**: `quinn` (production-grade, Rust-native)
- **Transport**: UDP-based QUIC protocol. Multiplexed streams per
  connection. Built-in TLS 1.3 encryption. Congestion control.
- **Discovery**: Not built-in; use mDNS like custom UDP.
- **QoS**: Streams map naturally to QoS levels. Unreliable datagrams
  for best-effort. Reliable streams for ordered delivery.
- **Latency**: 8-12 ms (connection setup), 2-4 ms warm. 0-RTT resumption
  available.
- **Throughput**: 100K+ msg/sec feasible.
- **Fit**: Moderate-good. Multiplexed streams avoid head-of-line blocking
  (unlike raw TCP). Built-in reliability reduces custom code.
- **Concerns**: Encryption overhead (~1-2% CPU). Higher complexity (500+
  lines). Connection setup latency.
- **Windows**: Supported. Kernel QUIC offload coming to Windows 11.

**Why include**: The "modern protocol" comparison. QUIC is increasingly
standard for low-latency reliable transport. Interesting to measure the
encryption overhead vs raw UDP, and whether multiplexed streams solve the
TCP head-of-line blocking problem from DESIGN.md QoS level 4.

### R5: Hybrid UDP/TCP (QoS-driven transport selection)

- **Approach**: Use UDP for best-effort traffic (QoS 1-2) and TCP for
  reliable traffic (QoS 3-4). The simplest correct implementation — no
  application-layer reliability at all.
- **Crates**: `std::net::{UdpSocket, TcpStream, TcpListener}`, `socket2`,
  `mdns-sd` for discovery.
- **Transport**: UDP multicast for QoS 1-2 (fire-and-forget, latest-value
  with receiver-side seq filtering). TCP connections per peer pair for
  QoS 3-4 (kernel handles ordering, retransmission, flow control).
- **Discovery**: mDNS, same as custom UDP.
- **QoS**: L1-L2 via UDP (same as custom UDP). L3-L4 via TCP (no NACK
  protocol, no gap detection — kernel does it all).
- **Latency**: UDP path same as custom UDP (2-5 ms). TCP path same as
  raw TCP (5-15 ms cold, ~1 ms warm with TCP_NODELAY).
- **Throughput**: Same as custom UDP for L1-L2. TCP path limited by
  kernel flow control at high rates.
- **Fit**: Excellent. Directly tests whether the NACK-based reliable-UDP
  (QoS 3 in custom UDP) is worth the complexity over TCP.
- **Concerns**: Head-of-line blocking on TCP connections stalls all paths
  sharing that connection. On a LAN (rare packet loss), this may never
  matter — which is exactly what the benchmark will measure.
- **Windows**: Fully supported, no platform quirks.
- **Complexity**: Low-medium. Simpler than custom UDP (no NACK logic)
  and QUIC (no TLS, no connection migration).

**Why include**: Answers the key design question from DESIGN.md S6.3:
is per-path independence (reliable-UDP with NACKs) actually worth the
complexity, or does TCP's kernel-managed reliability perform equally well
on a LAN? Comparing E3b (custom UDP with NACKs) vs E3e (hybrid with TCP
for reliable) at QoS 3 gives us the definitive answer.

---

## Considered but Not Recommended

### Dust DDS (pure Rust DDS)

- **Crate**: `dust_dds` (pure Rust, no unsafe, actively maintained)
- **Latency**: Millisecond-scale (no published sub-ms benchmarks)
- **Reason**: Interesting as a pure-Rust DDS, but no evidence it would
  outperform or provide different insights than Zenoh. DDS overhead
  without DDS ecosystem benefit. Consider as a stretch goal if time allows.

### ZeroMQ

- **Crate**: `zeromq` (pure Rust) or `zmq` (C bindings)
- **Latency**: < 1 ms on LAN
- **Reason**: No built-in discovery — requires Zyre (C library, UDP beacons)
  for zero-conf. PUB/SUB semantics are a good fit, but adding Zyre for
  discovery negates the simplicity advantage. ZeroMQ + Zyre is roughly
  equivalent complexity to custom UDP with mDNS, but with less control.

### CycloneDDS

- **Crate**: `cyclonedds-rs` v0.4.5 (C bindings, community-maintained)
- **Latency**: ~1.4 ms average
- **Reason**: C library dependency. Community bindings (not official).
  Zenoh covers the "high-level framework" slot with better Rust support
  and lower latency. CycloneDDS would be redundant.

### NATS

- **Reason**: Requires broker. Violates leaderless topology requirement.

### Redis pub/sub

- **Reason**: Requires broker. No durability. Not designed for replication.

### RTI Connext DDS

- **Reason**: Experimental Rust support. Commercial licensing. Overkill.

### io_uring approaches

- **Reason**: Linux only. Windows 11 is the dev environment.

### DPDK / kernel bypass

- **Reason**: Linux only. No stable Rust bindings.

### iceoryx2

- **Crate**: `iceoryx2` (native Rust, sub-100ns latency)
- **Reason**: Same-machine only. Cannot replicate across network.
  VariantDummy already serves as the zero-network baseline.

### Cap'n Proto RPC

- **Reason**: RPC framework, not a messaging transport. Would need to
  build replication logic on top.

### Manual shared memory

- **Reason**: Same-machine only. Interesting for baseline but VariantDummy
  already covers that role.

---

## Variant Comparison Matrix

| Variant | Approach | Latency (LAN) | Rust | Discovery | QoS Levels | Complexity |
|---------|----------|---------------|------|-----------|------------|------------|
| Zenoh | Framework | < 10 us | Native | Zero-conf | Configurable | Low |
| Custom UDP | Raw | 2-5 ms | Native | mDNS | All 4 manual | Medium |
| Aeron | Framework | 21-57 us | C bindings | Built-in | Reliable/unreliable | Medium |
| QUIC (quinn) | Protocol | 2-12 ms | Native | mDNS | Streams + datagrams | High |
| Hybrid UDP/TCP | Mixed | 2-5 ms (UDP) / 1-5 ms (TCP) | Native | mDNS | UDP L1-2, TCP L3-4 | Low-Medium |

## Impact on E1 (Variant Base Crate)

The current `Variant` trait is synchronous:

```rust
pub trait Variant {
    fn name(&self) -> &str;
    fn connect(&mut self) -> Result<()>;
    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()>;
    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>>;
    fn disconnect(&mut self) -> Result<()>;
}
```

### Compatibility Assessment

- **Custom UDP**: Perfect fit. Sync API maps directly.
- **Zenoh**: Async-first, but has blocking wrappers. Can use `block_on`
  inside trait impls. Works but wastes async benefits.
- **Aeron**: Callback-based C API. Would buffer callbacks into a queue
  and drain via `poll_receive`. Works.
- **QUIC (quinn)**: Async (tokio). Same as Zenoh — use `block_on` or
  run a tokio runtime internally.

### Recommended Trait Changes

**No breaking changes needed.** The sync trait works for all four
candidates. Async variants can internally spawn a tokio runtime and
bridge to sync at the trait boundary. For a benchmark measuring
transport latency, the sync driver with a controlled tick loop is
actually preferable — it eliminates async runtime scheduling noise from
measurements.

**One addition to consider**: a `fn configure(&mut self, extra_args: &[String]) -> Result<()>` method (or pass extra args to a constructor)
so variants can parse their specific CLI args. Currently variants handle
this in their binary's `main()` before constructing the variant, which
works fine. No change needed unless the pattern becomes awkward.
