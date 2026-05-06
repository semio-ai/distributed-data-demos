# variants/webrtc Structure

## Final layout (T3g.2)

```
variants/webrtc/
  AGENTS.md          -- agent rules for this variant
  CUSTOM.md          -- detailed design instructions
  STRUCT.md          -- this file
  Cargo.toml         -- binary crate (variant-base, webrtc 0.8, tokio,
                        anyhow, clap, rand, serde, serde_json, bytes)
  src/
    main.rs          -- CLI parsing, port derivation, build WebRtcVariant,
                        call run_protocol
    webrtc.rs        -- WebRtcVariant struct implementing Variant trait;
                        internal tokio runtime, mpsc bridge, host-only
                        ICE config, four DataChannels per peer, send loop,
                        EOT on reliable channel
    signaling.rs     -- per-pair TCP signaling: length-prefixed JSON
                        envelopes carrying SDP offer/answer + trickle ICE
    pairing.rs       -- sorted-name pairing, port derivation
                        (signaling and media), initiator/responder roles
    protocol.rs      -- compact binary header (data + EOT frames),
                        identical layout to hybrid / custom-udp / websocket
  tests/
    integration.rs   -- subprocess tests: loopback exit, missing-arg,
                        runner-not-in-peers
    fixtures/
      loopback.toml  -- single-process loopback config (qos=1)
```

Only the binary `variant-webrtc` is produced. No library targets.

## Key design points

- `tokio` features: `rt-multi-thread`, `macros`, `sync`, `net`, `time`,
  `io-util` (intentionally not `enable_all()` per project rule).
- `webrtc = "0.8"`. Do not bump to a 0.20 alpha -- the API is in flux.
- `RTCConfiguration::ice_servers` is empty (no STUN, no TURN).
- `SettingEngine` disables mDNS, restricts network types to `Udp4`,
  and pins the UDP port via `EphemeralUDP::new(port, port)`. ICE
  produces `typ host` candidates only.
- Lower-sorted runner is the signaling initiator (creates four
  DataChannels and sends the SDP offer); higher-sorted runner is the
  responder (registers `on_data_channel`, replies with the SDP
  answer). Channel labels are `qos{1..4}-...` so the responder can
  recover the QoS from the inbound channel.
- DataChannel options:
  - L1, L2: `ordered=false`, `max_retransmits=Some(0)`.
  - L3, L4: `ordered=true`, no retransmit limit.
  L2's latest-value semantics are layered on the receiver side via a
  per-`(writer, path)` highest-seen-seq watermark before frames reach
  the driver.
- Sync-to-async bridge: `connect`, `disconnect`, `signal_end_of_test`,
  and `poll_peer_eots` use `runtime.block_on(...)` (only `disconnect`
  actually waits on async work). `publish` enqueues onto an mpsc
  channel and the per-runtime `send_loop` task dispatches via
  `RTCDataChannel::send`. `poll_receive` is `try_recv` against the
  shared inbound mpsc fed by the `on_message` callbacks.
- EOT marker is always sent on the reliable QoS-4 DataChannel,
  regardless of the spawn's primary `--qos`. This avoids the
  unreliable-EOT-loss deadlock failure mode.
