# tether's C++ core

The co-processor's side of tether, in C++23: framing, the link, the server and
the router. It implements the same protocol as the Rust core (`../core`), checked
against the shared golden frames (`../golden/frames.json`) and, in QEMU, against
the Rust client.

- `Link`: reliable, in-order frames over the UART.
- `Server`: open calls in a fixed table; hands each request to a `Dispatcher`
  with a `RawReply` or a `RawSink` to answer it, which are cheap handles to keep. A
  full send queue shows up as `CallError::QueueFull`, to retry. `RawSink::set_latest`
  keeps only the newest value for a slow client, in memory `StaticServer` sizes.
- `Router`: the `Dispatcher` that sends each call to the `Service` that owns
  its method, from the server's table (`descriptor.h`), and answers the rest
  `UNIMPLEMENTED`. `Service` is what tether-gen's output will implement.

- `typed.h`: `Reply<T>` and `Sink<T>` over the raw ones, and `verify<T>`. Messages
  are built with a flatbuffers builder in the server's scratch memory, since
  flatc's object API allocates.
- tether-gen's C++ output (`--lang cpp`): a `constexpr` table for each
  `rpc_server`, and for each `rpc_service` a `Handler` to implement and a
  `Service` that verifies requests and calls it. Servers only, for now.

`StaticLink`, `StaticServer` and `StaticRouter` bring their own memory, sized by
template parameters.

It's written for embedded targets without giving up modern C++:

- No exceptions or RTTI (ESP-IDF's defaults): errors are `std::expected`.
- No heap: fixed buffers, sized at compile time where it matters.
- Sans-IO: callers feed received bytes in and pull bytes to send out.

## Using it from ESP-IDF (or PlatformIO with `framework = espidf`)

`tether/` is an ESP-IDF component; everything else here is for developing it.
Declare it in the `idf_component.yml` of the component that uses it (`main/`,
or `src/` under PlatformIO):

```yaml
dependencies:
  tether:
    path: ../path/to/cpp/tether   # a local checkout
    # or, from git:
    # git: https://github.com/<owner>/<repo>.git
    # path: cpp/tether
    # version: <tag or commit>
```

A local component must live in a directory named after it, so `tether/` keeps
that name. From git, the component manager fetches it into
`managed_components/`.

## Developing

See [DEVELOPMENT.md](../DEVELOPMENT.md): `make test`, `fmt` and `tidy` here,
and `make qemu-test` for the integration tests on an emulated ESP32.
