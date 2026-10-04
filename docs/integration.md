# Integrating tether

How to put tether between two microcontrollers on a UART: a Rust client
on one (an ESP32-S3 running embassy, say) and a C++ server on the other (an
ESP32 running ESP-IDF). It's written for an application that already exists and
talks over the UART some other way, and moves to tether one service at a time.

The test app in `cpp/test_app` is a working example of the server side, and
`core/tests/qemu.rs` and `embassy/tests/e2e.rs` are examples of the client
side. Where this guide and they disagree, they're the ones that run.

## The pieces

```
  S3 (Rust, embassy)                              co-processor (C++, ESP-IDF)
 ┌───────────────────────────┐                   ┌───────────────────────────────┐
 │ app tasks                 │                   │ handlers (generated Handler)  │
 │   greeter::Client<&Shared>│                   │   app tasks: UartIo::wait     │
 │ tether (traits, Call,     │                   │ tether/typed.h (Reply, Sink)  │
 │   Channel, Message)       │                   │ Router + generated Service    │
 │ tether-core: Client, Link │                   │ tether: Server, Link          │
 │ tether-embassy:           │      UART         │ tether_idf: UartIo            │
 │   run_client(rx, tx) ─────┼───────────────────┼─ serve() on the I/O task      │
 └───────────────────────────┘                   └───────────────────────────────┘
```

- **The schema** (`.fbs`) declares the messages and the services. One copy of
  it generates both sides.
- **tether-gen** generates a typed client (Rust) and the server's interfaces:
  a `Handler` to implement and a `Service` that adapts it (C++ or Rust).
- **The core** on each side is sans-IO: it never touches the UART or the clock.
  The glue does: `tether-embassy` on the S3, `tether_idf` on the co-processor.
- **The link** under both is reliable: frames are acked and retransmitted, and
  each side's boot id tells the other when it rebooted. Calls on top of it have
  deadlines and status codes (gRPC's), channels have credit-based flow control,
  and dropping a call cancels it on the server.

Only the client calls, and only the server serves. For the co-processor to
start something, the S3 opens a channel that the co-processor sends events on
(see [Events from the co-processor](#events-from-the-co-processor)).

## 1. The schema

```fbs
// coprocessor.fbs
include "tether.fbs";

namespace Coprocessor;

table Empty {}

table WifiStatus {
    connected: bool;
    ip: string;
}

table Scan {
    max: ubyte;
}

table Network {
    ssid: string;
    rssi: byte;
}

rpc_service Wifi {
    /// The current connection.
    Status(Empty): WifiStatus;
    /// The networks in range, one item each, then the end.
    Scan(Scan): Network (streaming: "server");
    /// Every change of the connection, as it happens.
    Watch(Empty): WifiStatus (streaming: "server");
}

/// The services the co-processor serves: the wire ids.
enum Server : ubyte (rpc_server) {
    Wifi,
}
```

- `include "tether.fbs"` declares tether's attributes (`schema/tether.fbs`).
- A method is unary, or server-streaming with `(streaming: "server")`. That
  makes it a channel. Unary calls time out after 5 s unless the method says
  otherwise, e.g. `(timeout_ms: "1000")`.
- The `rpc_server` enum is the server's table. A service's wire id is its value
  there, and a method's is its position in the `rpc_service`. So both are
  **append-only**: to retire one, mark it `(deprecated)` instead of deleting
  it, and calls to it are answered `UNIMPLEMENTED`.
- Changing a table follows flatbuffers' rules: add fields at the end, and
  deprecate instead of deleting.

## 2. Generating code

Install `tether-gen` once (`cargo install --path gen` from this repository;
it links flatc's parser, so it needs no flatc). The types themselves still come
from flatc, which must be **25.12.19**, the version of the `flatbuffers` crate
and of the headers in `cpp/tether/third_party`.

```sh
# Rust (S3): the types (with the object API, which the client packs) and the client.
flatc --rust --gen-object-api --gen-all -I path/to/tether/schema -o src/generated/ coprocessor.fbs
tether-gen coprocessor.fbs -I path/to/tether/schema \
    --types crate::generated::coprocessor_generated > src/generated/coprocessor_rpc.rs

# C++ (co-processor): the types, and the server's interfaces.
flatc --cpp --cpp-std c++17 --scoped-enums --gen-all -I path/to/tether/schema -o main/generated/ coprocessor.fbs
tether-gen coprocessor.fbs -I path/to/tether/schema \
    --lang cpp --include coprocessor_generated.h > main/generated/coprocessor_rpc.h
```

tether checks its generated code in, and regenerates it with `make` when a
schema changes (see the top-level `Makefile`). That works well for an
application too.

## 3. The co-processor (C++, ESP-IDF)

### Components

Two components: `tether` (the core: portable, no platform code) and
`tether_idf` (the ESP-IDF glue). Point the project at them:

```cmake
# CMakeLists.txt of the project
set(EXTRA_COMPONENT_DIRS path/to/tether/cpp/tether path/to/tether/cpp/tether_idf)
```

```cmake
# main/CMakeLists.txt
idf_component_register(SRCS main.cpp wifi_service.cpp PRIV_REQUIRES tether tether_idf)
```

Both need C++23 (IDF 6 compiles as gnu++26), and neither uses exceptions,
RTTI or the heap, except that `UartIo` makes the UART driver and a queue set
once.

### Sizing the server

```cpp
// Payloads up to 512 bytes (a multiple of 8), 8 calls open at once, of which
// at most 4 channels; a set_latest value up to 64 bytes per channel.
using CoprocessorServer = tether::StaticServer<512, 8, 64>;
CoprocessorServer server(boot_id, {.baud_rate = 921600}, /*max_streams=*/4);
```

- **`MaxPayload`** bounds every message either way: requests it receives, and
  responses and items it builds. A build starts only when the send queue has
  room for a message this large, and one that grows past it **aborts**. That's
  a sizing bug, so it's loud. The memory is about 4× `MaxPayload`: a receive
  buffer, and a send queue that holds two frames that large (or many small
  ones).
- **`MaxCalls`** is the call table. A call that finds it full is answered
  `RESOURCE_EXHAUSTED`. So is a channel beyond `max_streams`.
- **`MaxLatest`** is the buffer each slot keeps for `set_latest`. The typed
  `set_latest` builds its value there, so a value larger than it aborts. The
  default, 0, means no `set_latest` at all.

It's large: make it `static`, or a member of something static, never a local.

### Handlers

tether-gen generates, per service, a `Handler` interface and a `Service` that
verifies requests and calls it:

```cpp
#include "generated/coprocessor_rpc.h"

class WifiHandler final : public Coprocessor::wifi::Handler {
 public:
  void status(tether::Reply<Coprocessor::WifiStatus> reply,
              const Coprocessor::Empty& /*request*/) override {
    (void)reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
      return Coprocessor::CreateWifiStatusDirect(fbb, connected_, ip_);
    });
  }

  void scan(tether::Sink<Coprocessor::Network> sink,
            const Coprocessor::Scan& request) override { ... }
  void watch(tether::Sink<Coprocessor::WifiStatus> sink,
             const Coprocessor::Empty& request) override { ... }

  // The client dropped the call, or the link went down. Optional.
  void cancelled(tether::CallId call) override { ... }
};
```

The request is a flatbuffer read in place. It's valid **during the call only**,
so copy what's needed later. A request that doesn't verify never reaches the
handler: the service answers `INVALID_ARGUMENT`.

A **message is built in place**, where it waits to be sent. The handler
passes a function that builds it with the builder it's given and returns its
root. The function only runs when the message can go: the call is still
open, the channel has credit, and the send queue has room. Otherwise the send
fails without calling it. Inside it:

- Send nothing else. A send from inside a build aborts.
- No `CreateSharedString`, and no flatbuffers objects with their own memory.
- Keep it short: it holds the server's lock (see [Threads](#threads)).

`reply.send`, `sink.send`, `sink.set_latest` and `sink.end` return
`std::expected<void, tether::CallError>`:

| Error       | Meaning                                                   | What to do |
|-------------|-----------------------------------------------------------|------------|
| `Closed`    | Cancelled by the client, answered already, or the link went down | Stop: nobody is listening |
| `NoCredit`  | (Channels) the client hasn't taken the earlier items       | Later: when credit arrives |
| `QueueFull` | The send queue is full until the peer acks                 | Later: on the next ack |
| `TooLarge`  | Never fits the send queue                                  | A bug: a smaller message |

A unary call is answered **exactly once**: `send`, or `fail(status)` with a
non-OK `WireStatus`. A channel sends any number of items and then `end()`s
(optionally with a status). Every send of an item uses a credit, which the
client grants as it takes items: `sink.credit()` says how many are left.
`set_latest` is for state, not events: when the client is slow, a newer value
replaces the one waiting, so it sees the latest and skips the ones in between.

`Reply` and `Sink` are small handles (a pointer and two integers). Copy them
and keep them as long as the call lasts. A handle outlives its call safely:
once the call is over, cancelled or from an earlier link, every send on it is
`Closed`. A default-made one is `Closed` too, which helps when declaring one to
fill in later or to receive from a FreeRTOS queue.

### Wiring it up

```cpp
#include "esp_random.h"
#include "tether/router.h"
#include "tether_idf/uart_io.h"

struct App {
  tether::idf::UartIo io{{.port = UART_NUM_1, .tx_pin = 17, .rx_pin = 16,
                          .baud_rate = 921600}};
  CoprocessorServer server{esp_random() | 1, {.baud_rate = 921600}, 4};
  tether::StaticRouter<Coprocessor::kServer.size()> router{server, Coprocessor::kServer};
  WifiHandler wifi;
  Coprocessor::wifi::Service wifi_service{wifi};
};

// The I/O task: one link after another.
[[noreturn]] void tether_io(void* arg) {
  App& app = *static_cast<App*>(arg);
  for (;;) {
    app.io.serve(app.server);               // Until the link ends for good.
    app.server.restart(esp_random() | 1);   // A new boot id for the new link.
  }
}

extern "C" void app_main() {
  static App app;                     // In app_main, not at static init:
  app.router.add(app.wifi_service);   // UartIo installs a driver.
  xTaskCreate(tether_io, "tether_io", 4096, &app, 10, nullptr);
}
```

- `router.add` aborts if a service's id isn't in the table, or if it's added
  twice. Methods of services never added are `UNIMPLEMENTED`.
- The I/O task's priority should be above the app tasks that send, so acks go
  out promptly. A late ack costs a retransmit.
- `serve` returns when the link ends for good. That's `PeerRebooted` (the S3
  restarted) or `PeerLost` (no ack after `max_retransmits` tries, ~1 s by
  default). `restart` cancels whatever was open (handlers get `cancelled`) and
  starts a new link. Handles from before stay `Closed`, even when the new
  client reuses their call ids. State that belongs to a link (subscriptions,
  counters) is reset here, after `restart`.

### Threads

The server isn't thread-safe on its own. `UartIo` makes it so: it's the
server's `ServerHooks`, a recursive mutex that every call into the server
takes, and a wake-up for the I/O task.

- **Handlers run on the I/O task**, holding the lock. They must not block.
  They answer at once, or keep the `Reply`/`Sink` and hand it to whatever does
  the work.
- **Any task can use a `Reply` or a `Sink`.** A send from another task takes
  the lock and wakes the I/O task to write it out.
- **Waiting for room or credit:** `io.wait(send, timeout)` calls `send`
  (a function returning the send's result) until it isn't `QueueFull` or
  `NoCredit`. It retries each time something arrives, which is when acks free
  the queue and credit comes in, until the timeout, and returns the last
  result. It's for app tasks only: from the I/O task, or holding the lock
  (inside a build or a handler), it would deadlock, so it aborts.

```cpp
// A task that streams scan results as the client takes them.
for (const auto& network : results) {
  const auto sent = io.wait([&] {
    return sink.send([&](flatbuffers::FlatBufferBuilder& fbb) {
      return Coprocessor::CreateNetworkDirect(fbb, network.ssid, network.rssi);
    });
  }, 5s);
  if (!sent) {
    break;  // Closed (cancelled), or the client stopped taking items.
  }
}
(void)io.wait([&] { return sink.end(); }, 5s);
```

- **Without a task of its own,** an app can do its sending on the I/O task:
  `io.serve(server, tick)` calls `tick()` after every read, which is when
  credit and queue room appear. The handler keeps the sink, and `tick` sends
  what the credit allows. This is the simplest choice for a producer that
  never blocks.

`cpp/test_app/main/greeter_app.h` does both: `Burst` answers in its handler,
and `Countdown` hands its sink to a task that waits.

### Events from the co-processor

The server never starts a call. For events (a button, a Wi-Fi change, a
setting changed from the web), the S3 opens a channel at boot, e.g.
`Watch(Empty): WifiStatus (streaming: "server")`, and the co-processor sends on
it when something happens. `set_latest` suits state (only the newest
matters). `send` suits events (each one matters) and returns `NoCredit` when
the client falls behind, so the app decides what to drop.

The handler keeps the sink. On `cancelled`, or when a send says `Closed`,
it forgets it. The S3 opens the channel again after a reconnect.

## 4. The S3 (Rust, embassy)

### Crates

```toml
[dependencies]
tether = { git = "https://github.com/NiltonVolpato/tether" }
tether-core = { git = "https://github.com/NiltonVolpato/tether" }
tether-embassy = { git = "https://github.com/NiltonVolpato/tether" }
flatbuffers = { version = "25.12.19", default-features = false }
```

All are `no_std`, but they **need `alloc`** (`Vec`, `Rc`, `BTreeMap` for now),
so a global allocator (`esp-alloc`). The client's shared state uses `RefCell`
and `Rc`, so it's not `Send`: the I/O task and every app task that calls it
run on **one executor**.

### The client and its I/O task

```rust
use esp_hal::Async;
use esp_hal::uart::{Config, Uart, UartRx, UartTx};
use static_cell::StaticCell;
use tether_core::client::{Client, SharedClient};
use tether_core::link::LinkConfig;

static CLIENT: StaticCell<SharedClient> = StaticCell::new();

let config = LinkConfig {
    baud_rate: 921_600,
    // The largest frame the co-processor sends: tether::max_wire_size(MaxPayload) - 1
    // on its side (333 for 256, 590 for 512, 1104 for 1024). Larger frames are
    // dropped as overflow, retransmitted, and the link is lost.
    max_frame: 590,
    ..LinkConfig::default()
};
let client = CLIENT.init(SharedClient::new(Client::new(boot_id, config)));

let uart = Uart::new(peripherals.UART1, Config::default().with_baudrate(921_600))
    .unwrap()
    .with_tx(peripherals.GPIO38)
    .with_rx(peripherals.GPIO48)
    .into_async();
let (rx, tx) = uart.split();
spawner.spawn(tether_io(client, rx, tx)).unwrap();

#[embassy_executor::task]
async fn tether_io(
    client: &'static SharedClient,
    rx: UartRx<'static, Async>,
    tx: UartTx<'static, Async>,
) {
    let state = tether_embassy::run_client(client, rx, tx).await;
    // The link ended for good: see "When the link ends".
}
```

`boot_id` must be non-zero and different on every boot: take it from the
hardware RNG. esp-hal's async `UartRx`/`UartTx` implement `embedded-io-async`
0.7, which is what `run_client` takes.

`run_client` reads and writes concurrently. It sleeps until bytes arrive, the
link's next deadline, or an app starts a call or takes an item, so an idle link
costs a ping every 250 ms and nothing else.

`SharedClient`'s methods take `&self` and none is async, so apps and the I/O
task share it freely, and nothing holds it across an `.await`. It isn't
thread-safe: apps must run on the I/O task's executor. A `StaticCell` lives for
ever; an `Rc<SharedClient>` is freed with its last user, which matters once the
co-processor can be put to sleep.

### Calling

```rust
use crate::generated::coprocessor_rpc::coprocessor::wifi;
use crate::generated::coprocessor_generated::coprocessor::{EmptyT, ScanT};

let wifi = wifi::Client(client);   // Any Transport: &SharedClient, Rc<SharedClient>, or a fake.

// Unary: await the call. Errors are tether::Status (gRPC's codes).
match wifi.status(&EmptyT {}).await {
    Ok(message) => {
        let status = message.get();   // A flatbuffer view of the response.
        info!("connected {} ip {:?}", status.connected(), status.ip());
    }
    Err(Status::DeadlineExceeded) => { /* the default 5 s, or the method's */ }
    Err(status) => { /* Unavailable when the link is down, etc. */ }
}

// A channel: `capacity` items buffered here; the server waits while it's full.
let mut networks = wifi.scan(&ScanT { max: 20 }, 4);
while let Some(network) = networks.recv().await {
    info!("{:?} {}", network.get().ssid(), network.get().rssi());
}
let how_it_ended = networks.end();   // Some(Ok(())) or Some(Err(status)).
```

- Dropping a `Call` or a `Channel` cancels it on the server (its handler
  gets `cancelled`).
- A response or item that doesn't verify is `Status::DataLoss`, and for a
  channel it also cancels the call.
- `try_result()` and `try_recv()` poll without waiting, for loops that don't
  await.
- Requests are packed from the object API (`EmptyT`, `ScanT`), which
  allocates. Responses are read in place.

A request must fit the co-processor's `MaxPayload`. Today nothing checks this
on the S3: a larger one is dropped by the co-processor as overflow, and the
link is eventually lost.

### When the link ends

`run_client` returns the link's final state: `PeerRebooted` (the co-processor
restarted) or `PeerLost` (it stopped answering). Calls in progress end with
`Status::Unavailable`, and new ones fail at once with it. The Rust client
can't restart yet: recovery means a new `Client` (with a new boot id) and a new
I/O task, and app code that holds the old client has to move to the new one.
On the C++ side, `Server::restart` does this in place.

## 5. Both ends together

These must agree, and nothing checks that they do:

| What | S3 (Rust) | Co-processor (C++) |
|------|-----------|--------------------|
| Baud rate | esp-hal `Config::with_baudrate`, `LinkConfig::baud_rate` | `UartConfig::baud_rate`, `LinkConfig::baud_rate` |
| Frames to the S3 | `LinkConfig::max_frame` ≥ … | … `tether::max_wire_size(MaxPayload) - 1` |
| Requests to the co-processor | every request's payload ≤ … | … `MaxPayload` |
| Pins | TX → | → RX, and RX ← TX: crossed |
| The schema | generated from the same `.fbs` | generated from the same `.fbs` |

The link's `baud_rate` sets its retransmit timeouts (how long a frame takes on
the wire), so it must be the line's real rate.

## 6. Testing

- **Handlers, on the host.** The generated Rust `Handler<S: ServerTypes>` is
  generic, so it runs against fakes (`core/tests/services.rs`). The C++ server
  runs on the host too: `cpp/tests/rig.h` links a `Server` to a client `Link`
  in memory, and `server_test.cpp` and `generated_test.cpp` show tests against
  it.
- **The wire, before the app.** Flash `cpp/test_app` on the co-processor and
  run `core/tests/qemu.rs` against it from a computer with a USB-UART adapter
  (see `cpp/test_app/README.md`). That checks wiring, baud rate and the
  co-processor's build before any application code is involved.
- **In QEMU.** `make -C cpp qemu-test` runs the C++ side on an emulated ESP32
  against the Rust tests. An application's own co-processor build can be
  tested the same way: copy `cpp/test_app/qemu-test.sh`.

## 7. When it doesn't work

- **It never links.** Both sides send a Hello every 50 ms until they link. No
  bytes arriving at all means pins or ground. Bytes arriving but no link means
  a baud rate mismatch. Check `link().stats()`: rising `cobs_errors` and
  `crc_errors` mean garbled bytes.
- **It links, then `PeerLost` after about a second.** A frame never got
  through. Usually it's larger than the other side's receive buffer (see the
  table above), or the I/O task is starved (its priority, or a handler that
  blocks).
- **`PeerRebooted` over and over.** One side really is restarting: check its
  console for a panic or an abort. An abort in tether is a bug it refuses to
  hide: a message larger than `MaxPayload` or `MaxLatest`, a send from inside a
  build, `wait()` from the I/O task, `router.add` of an unknown service.
- **`QueueFull` often.** The send queue is two large frames' worth. A build
  needs room for a whole `MaxPayload` message, even for a small one, so many
  small sends at once also fill it. Use `io.wait`, or send less at a time.
- **Stats to look at:** `server.link().stats()` (retransmits, duplicates,
  errors, the deepest the queue got), `io.stats()` (UART overflows: the I/O
  task fell behind), `server.stats().lost_rejections`, and on the S3
  `client.link_stats()` and `client.stats()`.
