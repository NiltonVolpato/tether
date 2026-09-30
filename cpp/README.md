# C++ core

The co-processor's side of the RPC framework, in C++23: framing so far; the
link, server and router to follow. It implements the same wire format as the
Rust core (`../core`), checked against the shared golden frames
(`../golden/frames.json`).

It's written for embedded targets without giving up modern C++:

- No exceptions or RTTI (ESP-IDF's defaults): errors are `std::expected`.
- No heap: fixed buffers, sized at compile time where it matters.
- Sans-IO: callers feed received bytes in and pull bytes to send out.

## Using it from ESP-IDF (or PlatformIO with `framework = espidf`)

This directory is an ESP-IDF component. Declare it in the `idf_component.yml`
of the component that uses it (`main/`, or `src/` under PlatformIO):

```yaml
dependencies:
  rpc:
    path: ../path/to/cpp          # a local checkout
    # or, from git:
    # git: https://github.com/<owner>/<repo>.git
    # path: cpp
    # version: <tag or commit>
```

The component manager fetches it into `managed_components/`.

## Developing

On the host it's a plain CMake library, with GoogleTest tests:

```sh
make test       # configure, build and run the tests (in ../build/cpp)
make fmt        # clang-format
make tidy       # clang-tidy
```

`generated/rpc/rpc_generated.h` comes from `../schema/rpc.fbs`; the top-level
Makefile regenerates it. `third_party/flatbuffers` pins the flatbuffers runtime
headers to the version of flatc that generates it.
