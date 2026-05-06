# Live Link and Omniverse — Exploration Notes

Drafted 2026-05-06. Companion to `variant-candidates.md`. Documents how
Unreal Engine's Live Link and NVIDIA Omniverse work, why they are not
suitable as benchmark variants, and what role (if any) they could play
in a follow-up exploration.

## TL;DR

Neither Live Link nor Omniverse is a peer-to-peer transport that
competes with our existing variants. Both are **destination-oriented
integrations** built around a specific consumer (a UE editor / runtime,
or an Omniverse / Nucleus-aware tool) and rely on engine-side plugins
written in C++ or Python to consume the data.

The realistic role for either of them in this project is as a
**downstream sink** — one of our existing variants publishes data, and
a small engine-side plugin (UE Live Link Source, Omniverse Kit
extension) subscribes and routes the data into the engine's data model.
That is a different project shape from "another transport variant" and
does not fit the current benchmark scope.

## Unreal Engine Live Link

### What it is

Live Link is a UE plugin framework that lets external sources stream
data — primarily skeletal animation, transforms, camera, and light
data — into the editor or a packaged runtime in real time, where it
gets bound to objects in the scene through a Live Link Component or
asset references.

It is a **plugin SDK**, not a wire protocol. A Live Link "source" can
be implemented over any transport the source author chooses; the SDK
just defines the C++ interfaces a source plugin must implement so that
UE can subscribe to it and pull frames at the engine tick rate.

### What transport it actually uses

Out of the box, UE ships an "Apple ARKit Face Tracking" source, an
"OSC" source, and the bundled **MessageBus** source. The MessageBus
source is the closest thing to a built-in default and is what most
people mean when they say "Live Link":

- Unreal's `MessageBus` is a UE-internal pub/sub layer built on UDP
  multicast (default group `230.0.0.1`, port `6666`) for discovery and
  small messages, with a TCP fallback for larger payloads.
- The wire format is UE-specific and not officially documented as a
  public spec. Open-source projects like `LiveLinkFreeD` and
  `UnrealMessageBus`-style libraries have reverse-engineered enough of
  it to interoperate, but the format is tightly coupled to UE's
  reflection / `UProperty` system. New UE versions occasionally tweak
  framing.
- Sources can also use OSC, JSON over TCP/UDP, or shared-memory if
  the plugin implements them — there is no requirement to use
  MessageBus.

### Comparison to our experiments

- **Topology**: Live Link is unidirectional — sources push, UE pulls.
  Our system is leaderless peer-to-peer with single-writer ownership.
  Live Link is closer to a producer-consumer pattern.
- **Data model**: Live Link is keyed by *role* (transform, camera,
  animation, basic) with strongly typed frame structs. Our system is
  a generic key-value tree on `arora_types::Value`. There is no clean
  one-to-one mapping; you would need a translation layer.
- **QoS**: Live Link does not expose per-message reliability. The
  MessageBus transport is best-effort by default with TCP fallback for
  large frames. There is no equivalent to our QoS 1-4 split.
- **Discovery**: MessageBus uses UDP multicast like Zenoh and our
  custom-UDP variant. The mechanism is similar but the wire format is
  proprietary.

### Could it contribute to our benchmark?

As a **transport variant**: not really. The most honest implementation
would be either:

1. Implement a Rust client of the MessageBus wire protocol — significant
   reverse-engineering effort, target moves between UE versions, and
   the resulting numbers would only matter if you specifically care
   about UE interop. Low value per unit of effort.
2. Implement an OSC-over-UDP variant that matches the OSC Live Link
   source UE ships. OSC is a small, documented protocol — but it is
   essentially "UDP with a typed message envelope" and would mostly
   re-measure what custom-udp + a serialization layer already
   measures. Low novelty.

As a **downstream sink**: yes, if there is a real use case. A UE-side
Live Link Source plugin (C++) that subscribes to one of our existing
variants (Zenoh's pub/sub fits cleanest, Hybrid's TCP path also works)
and republishes frames into UE through the Live Link API would be a
useful integration demonstration. The benchmark numbers would still
come from our existing variants; the Live Link side adds latency from
the engine tick boundary that is not really part of the transport
question.

**Recommendation**: do not add a Live Link variant. If a UE consumer
becomes a real requirement, scope it as a separate "engine
integration" follow-up that consumes Zenoh.

## NVIDIA Omniverse

### What it is

Omniverse is a platform of products, the relevant pieces being:

- **Omniverse Kit** — a Python/C++ application framework built around
  USD (Universal Scene Description) and the Omniverse RTX renderer.
- **Nucleus** — a server / collaboration backend that hosts USD stages
  and brokers live edits among connected clients.
- **Connectors** — plugins for DCC tools (Maya, Houdini, Unreal,
  Blender) that talk to Nucleus.
- **OmniClient / Omniverse Client Library** — the C++ SDK clients use
  to talk to Nucleus.

Live data flow inside Omniverse happens through the **USD live layer**:
clients open a stage in "live" mode, edits are diffed and broadcast
through Nucleus to all other connected clients, who apply them to
their local USD stage.

### What transport it actually uses

The Nucleus protocol is **proprietary**. Public information is
limited:

- Connections from a client to Nucleus are over Omniverse's
  authentication-aware client protocol — TCP-based, encrypted, with
  authentication tokens. There is no public wire-format spec.
- The OmniClient Library (C++/Python) is the supported access path.
  There is no published Rust binding, no public protocol documentation,
  and no community reverse-engineering project comparable to UE's
  MessageBus situation.
- For real-time data exchange that does not need USD semantics,
  Omniverse encourages using **OmniGraph** nodes with custom
  extensions, or — in newer releases — bridging through standard
  protocols (DDS, ROS 2, MQTT) via dedicated extensions.

### Comparison to our experiments

- **Topology**: Omniverse is broker-mediated (Nucleus is a server).
  This is a hard mismatch with our leaderless-by-design constraint.
- **Data model**: USD prim/property hierarchy, not a generic key-value
  tree. Mapping `arora_types::Value` updates onto USD attributes is
  possible (USD has typed attributes including primitives, arrays, and
  `dictionary`-like custom data) but the granularity and the way
  changes are batched are very different from our per-key fan-out.
- **QoS**: USD live layers are eventually-consistent with diff-merge
  semantics. There is no per-update QoS knob analogous to ours.
- **Discovery**: clients connect to a known Nucleus server — no LAN
  zero-conf model.

### Pure-Rust feasibility (the user's specific question)

Without OmniClient or USD libraries, **a Rust-only Omniverse client is
not realistic**:

- The USD library itself is a large C++ codebase with no production
  Rust binding. The closest projects (e.g. `usd-rs` experiments) are
  research-grade and incomplete.
- The Nucleus protocol has no public spec. Reverse-engineering it
  would be a very large project with the additional risk that NVIDIA
  changes the protocol between Omniverse releases.
- Pulling in the OmniClient C++ SDK via FFI would not satisfy the
  "just Rust packages" constraint and would also bring in significant
  build dependencies on Windows.

The only direction that fits "no installed dependencies on the
machine" is going the **other way around**: have an Omniverse Kit
extension (Python, runs inside Kit and uses the in-process USD API)
subscribe to one of our existing variants and write the received
updates into the live USD stage. The Rust side stays untouched; the
heavy dependencies live inside the Omniverse install which is the user's
choice to install or not.

### Could it contribute to our benchmark?

As a **transport variant**: no. The protocol is closed, the topology
clashes with ours, and the model is push-broker-pull rather than
peer-to-peer.

As a **downstream sink**: yes, in the same shape as the Live Link
suggestion. A Kit extension that subscribes to Zenoh (Zenoh has
official Python bindings, so this is one Python `pip install` inside
Kit, no C++ work) and writes received updates into a USD live layer
would demonstrate end-to-end "Rust producers → Omniverse visualisation"
without changing anything on our side.

**Recommendation**: do not add an Omniverse variant. If Omniverse
visualisation becomes a real requirement, scope it as a separate
"downstream consumer" project that subscribes to Zenoh from inside an
Omniverse Kit extension.

## Summary table

| System    | Public protocol spec? | Pure-Rust impl realistic? | Topology fit | Useful as variant? | Useful as sink? |
|-----------|-----------------------|---------------------------|--------------|--------------------|-----------------|
| Live Link | No (MessageBus is partially reverse-engineered) | Partially (OSC source, yes; MessageBus, no) | Producer→UE | No | Yes, as UE-side plugin consuming Zenoh |
| Omniverse | No                    | No                        | Broker (Nucleus) | No              | Yes, as Kit-side Python extension consuming Zenoh |

## What to actually do (if anything)

A realistic follow-up — scoped as its own initiative outside this
benchmark — would be a small "downstream consumers" project:

1. **UE Live Link Source plugin** that subscribes to Zenoh and republishes
   incoming key-value updates as Live Link frames typed by a config
   mapping (e.g. `bench/transforms/<name>` → Live Link Transform role).
2. **Omniverse Kit extension (Python)** that subscribes to Zenoh and
   writes incoming updates into a live USD stage at configurable prim
   paths.

Neither is on the current TASKS.md backlog. Both are interesting
demonstrations but they are integration work, not transport
benchmarking. If the user wants to pursue this, it should be scoped
as a separate project (e.g. `consumers/unreal-live-link/`,
`consumers/omniverse-kit/`) with its own AGENTS.md and CUSTOM.md, and
the work split off so the benchmark stays clean.
