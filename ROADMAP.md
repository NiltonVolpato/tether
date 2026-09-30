# Inter-MCU RPC framework

Goal: replace the ad hoc S3 ↔ co-processor protocol with a framework that is
independent of the application. The schema declares the services, and the
framework owns acknowledgements, status codes, flow control, cancellation and
metrics. Its core has no I/O, so everything is testable without hardware.
Rust on the S3 (client); C++ on the co-processor (server).

## Done

- [x] Framing: COBS, CRC32 first, size-prefixed header flatbuffer, payload 8-aligned
- [x] Link: Hello with boot ids, so queued Hellos can't cause a reboot loop
- [x] Link: stop-and-wait retransmission, duplicate detection, link stats
- [x] Unary calls with deadlines, and gRPC status codes
- [x] Server-streaming channels with credit flow control
- [x] Latest-value coalescing (`set_latest`)
- [x] Cancel on drop: dropping a `Channel` or `Call` cancels it on the server
- [x] Stream limit returns `RESOURCE_EXHAUSTED`
- [x] Simulated lossy link: drops, bit flips, random read sizes, many seeds
- [x] rpcgen reads `.bfbs` via flatbuffers-reflection
- [x] rpcgen: typed Rust clients, handler traits and services
- [x] Wire ids from `rpc_server` enums and method order; `deprecated` tombstones
- [x] Router: direct-indexed dispatch; automatic `UNIMPLEMENTED` and `INVALID_ARGUMENT`
- [x] Framework schemas kept apart from the test application's schema

## API (`rpc` crate: the generic middle layer; traits and data objects only)

Generated code is the thin typed layer on top of it, and the core the
schema-agnostic layer below it.

- [x] Client traits: `Transport`, `RawCall`, `RawChannel`; typed `Call`, `Channel`, `Message`
- [x] Server traits: `ServerTypes`, `RawReply`, `RawSink`, `Service`; typed `Reply`, `Sink`
- [x] Data objects: `MethodId`, `CallId`, `Status`, `StreamError`, `ServerTable`
- [x] Spec of generated code (a hand-written `Greeter`) and tests of user code against fakes
- [x] A proxy using only the generic layer forwards calls, channels, credit and cancellation (test)
- [ ] Router catch-all, so a proxy can serve services it doesn't know
- [x] Rename `api` to `rpc`; the core becomes `rpc-core`, in a virtual workspace
- [x] rpcgen generates code that depends only on `rpc`, as in the `Greeter` spec
- [x] The core implements `Transport` and `ServerTypes`; its router dispatches to `api::Service`
- [x] The core maps wire status codes to `api::Status`
- [ ] `rpc-testing` crate: ship the in-process `Loopback` fake to users

## Core (`rpc-core` crate)

- [ ] Choose the interior mutability behind `SharedClient` and `SharedServer` (a `RefCell` for now, i.e. one executor; depends on the S3 task/executor layout)
- [x] Liveness: ping and a retransmit limit, so a dead peer is detected (replaces Heartbeat)
- [x] Credits: the client scans its channels each poll (dropped ones are cancelled) and sends
      all grants in one Credit frame; replaces the handles' outbox
- [ ] Fixed-capacity memory: bounded tables and queues instead of `Vec`/`BTreeMap`/`VecDeque`
- [ ] Bounded send queue with backpressure
- [ ] Receive directly into 8-aligned buffers without copying
- [ ] Per-method metrics (calls, errors, latency), keyed by `MethodId`
- [ ] Logging hook that names methods via `router::lookup`

## C++ (co-processor)

- [ ] C++ core: framing (done: COBS, CRC32, header, Deframer; passes the goldens), link, server, router
- [ ] rpcgen C++ output: server table, handler interfaces, typed sinks and replies
- [x] Cross-language conformance: shared golden frames (`golden/frames.json`, decoded by flatc)
- [ ] Cross-language conformance: C++ core built into the Rust simulation tests

## Integration

- [ ] S3: embassy UART glue and task layout; wake the I/O task when an app consumes or drops
- [ ] Recovery from a terminal link (`PeerLost`, `PeerRebooted`): new client with a new boot id,
      or reset the co-processor
- [ ] Co-processor: FreeRTOS UART task glue
- [ ] Port Wifi and provisioning to services
- [ ] Port time sync and battery to services
- [ ] Remove the old Heartbeat/Hello code on both sides
- [ ] System channel the S3 opens at boot, for co-processor-initiated commands (e.g. timezone set from the web)
- [ ] Sonos: GENA subscriptions as channels
- [ ] Sonos: album art streamed in 512-byte chunks
- [ ] Measure the UART error rate on the device at 921600, 2M and 5M baud; pick the baud rate
- [ ] Move the schemas and build rules into t-encoder

## Later

- [ ] Move the framework to its own repository
- [ ] rpcgen bundles flatc (via FFI): one tool from `.fbs` to generated code, with
      `rpc_attributes.fbs` on the include path automatically; then a test crate's
      `build.rs` replaces the Makefile's test-schema rules and the `build/` directory

## Not planned

- Acks carried in outgoing frames: saves ~5% on the busy direction of a full-duplex link
- Schema evolution checks against the previous release (`flatc --conform`)
