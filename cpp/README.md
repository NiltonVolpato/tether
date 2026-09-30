# tether's C++ core

The co-processor's side of tether, in C++23: framing so far; the link, server
and router to follow. It implements the same wire format as the Rust core
(`../core`), checked against the shared golden frames (`../golden/frames.json`).

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

On the host it's a plain CMake library, with GoogleTest tests:

```sh
make test       # configure, build and run the tests (in ../build/cpp)
make fmt        # clang-format
make tidy       # clang-tidy
```

`tether/generated/tether/wire_generated.h` comes from `../schema/wire.fbs`; the
top-level Makefile regenerates it. `tether/third_party/flatbuffers` pins the
flatbuffers runtime headers to the version of flatc that generates it.
