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

### R6: WebSocket (browser-compatible reliable transport)

- **Approach**: TCP transport with the WebSocket framing/upgrade layer on
  top. One connection per peer pair, full-duplex, binary frames carrying
  the same compact header as the other variants.
- **Crates**: `tungstenite` (sync) over `std::net::TcpStream`, plus
  `socket2` for buffer tuning. No tokio.
- **Transport**: TCP with the WebSocket frame format. Reliable QoS only
  (3-4); the variant returns a clear error and exits non-zero if asked
  to publish at QoS 1-2. UDP for unreliable QoS is intentionally not
  duplicated here — Hybrid already covers that. Added 2026-05-06.
- **Discovery**: Runner-injected `--peers` (E9). No mDNS, no signaling
  side-channel.
- **QoS**: Reliable-ordered (L3/L4) only.
- **Latency**: Expected to track Hybrid TCP closely (1-5 ms warm on LAN
  with `TCP_NODELAY`) plus the cost of the WebSocket masking and frame
  header — typically tens of nanoseconds per message at our payload
  sizes, but worth measuring rather than assuming.
- **Throughput**: Bounded by TCP back-pressure, same as Hybrid TCP.
  The masking step on the client side is the only material per-message
  cost difference vs raw TCP at our payload sizes.
- **Fit**: Excellent for the question it answers (framing overhead vs
  raw TCP). Poor as a general-purpose variant because half of the QoS
  matrix is unsupported. That is an acceptable trade-off — the design
  brief is "compare implementations", not "every variant must cover
  every QoS".
- **Concerns**: WebSocket is a client/server protocol. Symmetric peer
  pairing requires choosing a client and a server per pair — we use
  sorted-name index, lower initiates. Same pairing rule as Hybrid TCP.
- **Windows**: Fully supported.
- **Complexity**: Low. Smaller surface area than Hybrid (no UDP path).

**Why include**: Isolates the WebSocket framing tax. Comparing E3f vs
E3e (Hybrid) at QoS 4 directly measures the cost of a framing layer
that is widely used in production systems (real-time dashboards,
collaborative editors, game-server channels). Cheap to build because
it reuses Hybrid's TCP design verbatim and just swaps the framing.

### R7: WebRTC DataChannels (browser stack on a LAN)

- **Project**: webrtc-rs | https://github.com/webrtc-rs/webrtc
- **Crate**: `webrtc` (Rust-native, port of pion/webrtc).
- **Transport**: SCTP-over-DTLS-over-UDP with DataChannels. Each
  DataChannel is configurable as ordered/unordered and reliable /
  `maxRetransmits=N`. We use one channel per QoS level per peer pair.
- **Discovery**: Runner-injected `--peers`, host ICE candidates only.
  No STUN/TURN/mDNS — LAN-only.
- **Signaling**: Direct variant-to-variant TCP signaling channel for
  SDP offer/answer + ICE candidate exchange. Closes once DataChannels
  open. The runner does NOT participate.
- **QoS**: All four levels mapped natively:
  - L1 / L2: unordered + `maxRetransmits=0` (lossy datagram-like)
  - L3 / L4: ordered + default reliable
- **Latency**: Higher than QUIC at cold start (DTLS handshake + SCTP
  init); warm latency expected to be in the same ballpark as QUIC
  (1-5 ms on LAN). The SCTP layer adds a small per-message cost vs
  raw QUIC streams.
- **Throughput**: SCTP is the practical bottleneck. Should reach our
  100K msg/s target on a LAN but with more variance than QUIC or
  Hybrid because of DTLS framing and SCTP windowing.
- **Fit**: The only candidate that natively maps to all four QoS
  levels with zero application-layer reliability code on our side.
  Heaviest stack of any candidate. Added 2026-05-06.
- **Concerns**: Build size and dependency footprint are significant.
  Setup latency dominates connect-time measurements (acceptable — we
  log it). Windows builds of `webrtc-rs` are known to work but the
  worker should validate early.
- **Windows**: Supported. Validate early.
- **Complexity**: High. Sync-to-async bridge same as QUIC.

**Why include**: WebRTC is the only widely-deployed off-the-shelf
mechanism that gives applications a reliable+unreliable mux from a
single session. Measuring its cost on a LAN — versus QUIC (which
multiplexes streams over a single QUIC connection but has no
unreliable-with-ordering equivalent) and versus a hand-rolled
hybrid — fills a real gap in the comparison matrix. It is also the
only variant whose protocol is implementable in a browser, which
matters for any future "browser-as-peer" experiments.

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
| WebSocket | Framed TCP | 1-5 ms (TCP) + framing cost | Native (`tungstenite`) | Runner --peers | L3-4 only | Low |
| WebRTC | DTLS+SCTP/UDP | 1-5 ms warm; high cold | Native (`webrtc-rs`) | Runner --peers + variant-to-variant SDP | All 4 native | High |

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
