# variants/quic Structure

```
variants/quic/
  Cargo.toml          -- crate manifest (binary: variant-quic)
  AGENTS.md           -- agent rules for this repo
  CUSTOM.md           -- design guidance and architecture notes
  STRUCT.md           -- this file
  src/
    main.rs           -- CLI parsing (incl. runner-injected --peers, variant-specific
                         --base-port), identity resolution from --runner against the
                         sorted peer list, port derivation per the
                         runner_stride/qos_stride convention, QuicVariant construction,
                         run_protocol entry point
    quic.rs           -- QuicVariant struct implementing Variant trait; message encoding/decoding;
                         async-to-sync bridge via tokio runtime and mpsc channels;
                         background send/receive tasks; QUIC datagram (QoS 1-2) and
                         stream (QoS 3-4) transport; skip-verification TLS config
    certs.rs          -- self-signed certificate generation using rcgen
  tests/
    loopback.rs       -- integration tests: single self-peer lifecycle (binds, runs,
                         produces a log file), missing-base-port failure, runner-not-
                         in-peers failure
```
