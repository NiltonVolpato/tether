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
- [x] tether-gen reads `.bfbs` via flatbuffers-reflection
- [x] tether-gen: typed Rust clients, handler traits and services
- [x] Wire ids from `rpc_server` enums and method order; `deprecated` tombstones
- [x] Router: direct-indexed dispatch; automatic `UNIMPLEMENTED` and `INVALID_ARGUMENT`
- [x] Framework schemas kept apart from the test application's schema

## API (`tether` crate: the generic middle layer; traits and data objects only)

Generated code is the thin typed layer on top of it, and the core the
schema-agnostic layer below it.

- [x] Client traits: `Transport`, `RawCall`, `RawChannel`; typed `Call`, `Channel`, `Message`
- [x] Server traits: `ServerTypes`, `RawReply`, `RawSink`, `Service`; typed `Reply`, `Sink`
- [x] Data objects: `MethodId`, `CallId`, `Status`, `StreamError`, `ServerTable`
- [x] Spec of generated code (a hand-written `Greeter`) and tests of user code against fakes
- [x] A proxy using only the generic layer forwards calls, channels, credit and cancellation (test)
- [ ] Router catch-all, so a proxy can serve services it doesn't know
- [x] Rename `api` to `rpc`; the core becomes `rpc-core`, in a virtual workspace
- [x] Working name `tether`: crates `tether`, `tether-core`, `tether-gen`; C++ `namespace tether`
- [x] tether-gen generates code that depends only on `tether`, as in the `Greeter` spec
- [x] The core implements `Transport` and `ServerTypes`; its router dispatches to `api::Service`
- [x] The core maps wire status codes to `api::Status`
- [ ] `tether-testing` crate: ship the in-process `Loopback` fake to users

## Core (`tether-core` crate)

- [ ] Choose the interior mutability behind `SharedClient` and `SharedServer` (a `RefCell` for now, i.e. one executor; depends on the S3 task/executor layout)
- [x] Liveness: ping and a retransmit limit, so a dead peer is detected (replaces Heartbeat)
- [x] Retransmit timeout sized to the frame: base + (frame + peer's largest frame) at the baud
      rate; a fixed 20 ms collapsed the link with ~1 KB frames at 921600 (found in QEMU)
- [ ] Exponential backoff on retransmits, with liveness as a time budget instead of a count;
      when needed
- [ ] Batching: several messages per frame and one ack for several frames, when many small
      messages queue up. Stop-and-wait pays ~44 B of framing, a 30 B ack and two turnarounds
      per frame: modeled goodput for 32 B payloads is ~20% at 921600 and ~8% at 5 Mbaud (512 B:
      80% and 58%), assuming 0.3 ms turnarounds
- [ ] Link observability, so a clogged link can be diagnosed on the device: queue depth and
      time in queue, round-trip times, retransmit and error rates
- [x] Credits: the client scans its channels each poll (dropped ones are cancelled) and sends
      all grants in one Credit frame; replaces the handles' outbox
- [ ] Fixed-capacity memory: bounded tables and queues instead of `Vec`/`BTreeMap`/`VecDeque`
- [ ] Bounded send queue with backpressure
- [ ] Receive directly into 8-aligned buffers without copying
- [ ] Per-method metrics (calls, errors, latency), keyed by `MethodId`
- [ ] Logging hook that names methods via `router::lookup`

## C++ (co-processor)

- [ ] C++ core: framing (done: passes the goldens), link (done: host tests, event-driven sim,
      and against the Rust link in QEMU), server, router
- [ ] tether-gen C++ output: server table, handler interfaces, typed sinks and replies
- [x] Cross-language conformance: shared golden frames (`golden/frames.json`, decoded by flatc)
- [ ] Cross-language conformance: C++ core built into the Rust simulation tests
- [x] Integration test app (`cpp/test_app`): the C++ core on an ESP32 in QEMU, echoing frames
      over UART1 to the Rust tests (`make -C cpp qemu-test`); grows with the C++ link and server

## Integration

- [ ] S3: embassy UART glue and task layout; wake the I/O task when an app consumes or drops
- [ ] `tether_idf` component: the FreeRTOS task glue, blocking on a queue set of the UART
      driver's event queue and a wake semaphore, with the link's next deadline as the timeout.
      Acks stay in that task (high priority); ack from the UART ISR only if measured ack latency
      limits 5 Mbps (it would need the link's seq state shared with the ISR)
- [ ] Recovery from a terminal link (`PeerLost`, `PeerRebooted`): new client with a new boot id,
      or reset the co-processor
- [ ] Port Wifi and provisioning to services
- [ ] Port time sync and battery to services
- [ ] Remove the old Heartbeat/Hello code on both sides
- [ ] System channel the S3 opens at boot, for co-processor-initiated commands (e.g. timezone set from the web)
- [ ] Sonos: GENA subscriptions as channels
- [ ] Sonos: album art streamed in 512-byte chunks
- [ ] Measure the UART error rate on the device at 921600, 2M and 5M baud, and goodput by
      message size (it pins down the turnaround the model guesses); pick the baud rate
- [ ] Move the schemas and build rules into t-encoder

## Later

- [ ] Move the framework to its own repository
- [ ] tether-gen bundles flatc (via FFI): one tool from `.fbs` to generated code, for both
      Rust and C++; then a test crate's `build.rs` replaces the Makefile's test-schema rules
      and the `build/` directory. Findings so far (flatbuffers v25.12.19):
  - Link flatbuffers' library APIs, which report errors, rather than flatc's command line:
    `FlatCompiler`'s error callback must not return (`flatc_main.cpp` calls `exit(1)`), so a
    bad schema would kill a build script.
  - `.bfbs`: `Parser::Parse(source, include_paths, filename)` (false, with `parser.error_`),
    then `Parser::Serialize()` leaves it in `parser.builder_`, as `flatc -b --schema` does
    (`src/flatc.cpp`, `options.schema_binary`). `opts.binary_schema_builtins` and
    `opts.binary_schema_comments` are `--bfbs-builtins` and `--bfbs-comments`.
  - App types: `NewRustCodeGenerator()` / `NewCppCodeGenerator()` (`src/idl_gen_rust.h`,
    `src/idl_gen_cpp.h`) return a `CodeGenerator`; `GenerateCode(parser, path, filename)`
    returns a `Status`.
  - No `include "tether.fbs"` at all: `Parser::known_attributes_` is public, and
    `known_attributes_["rpc_server"] = false` before parsing is what `attribute "rpc_server";`
    does (`src/idl_parser.cpp`). Check that a schema declaring them anyway still parses.
    Failing that, `SetLoadFileFunction` / `SetFileExistsFunction` (`util.h`) can serve an
    embedded `tether.fbs`.
  - Build with the `cc` crate: the library sources (`idl_parser.cpp`, `idl_gen_text.cpp`,
    `reflection.cpp`, `util.cpp`) plus the Rust and C++ generators and what they need, not
    the other languages. Sources from a git submodule pinned to the `v25.12.19` tag, which
    the published crate includes; flatc's version must match the `flatbuffers` crate and the
    headers in `cpp/tether/third_party`.
  - Not `flatc-fork` (crates.io; builds flatc with flatbuffers' whole CMake build):
    `flatc_fork::flatc()` is a path in Cargo's build directory, which `cargo install`
    deletes, and we need an installable CLI for ESP-IDF builds. It also pins a commit past
    the tag and re-releases daily.

## Not planned

- Acks carried in outgoing frames: saves ~5% on the busy direction of a full-duplex link
- Schema evolution checks against the previous release (`flatc --conform`)
