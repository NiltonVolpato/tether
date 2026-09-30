# Developing tether

## Layout

| Path             | What                                                         |
|------------------|--------------------------------------------------------------|
| `tether/`        | `tether`: the API (traits and data objects) apps depend on   |
| `core/`          | `tether-core`: the sans-IO Rust core (framing, link, client, server) |
| `gen/`           | `tether-gen`: generates typed clients and services from a schema |
| `gen/flatbuffers/` | flatbuffers v25.12.19 (a submodule), whose schema parser `tether-gen` links |
| `schema/`        | `wire.fbs` (the wire format) and `tether.fbs` (attributes apps include) |
| `golden/`        | Golden frames: the wire format as bytes, shared by both cores |
| `cpp/tether/`    | The C++ core, an ESP-IDF component                           |
| `cpp/tests/`     | Its host tests (GoogleTest)                                  |
| `cpp/test_app/`  | An ESP-IDF app for integration tests against the Rust core   |

Builds go to `target/` (Cargo) and `build/` (everything else). Building
`tether-gen` needs the submodule: clone with `--recursive`, or run
`git submodule update --init`.

## Rust

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --all        # .rustfmt.toml uses unstable options
```

`core/tests/qemu.rs` shows as ignored: it needs the test app running (see
[Integration tests](#integration-tests)).

## C++ on the host

Needs CMake 3.22+, Ninja and a C++23 compiler. The first build fetches
GoogleTest and nlohmann/json.

```sh
make -C cpp test        # configure, build and run the tests, in build/cpp
make -C cpp fmt         # clang-format; fmt-check only checks
make -C cpp tidy        # clang-tidy
```

`fmt` and `tidy` use LLVM from `/opt/homebrew/opt/llvm@22`; point elsewhere
with `make -C cpp tidy LLVM_BIN=/path/to/llvm/bin`. clangd finds the compile
commands through `cpp/.clangd` once `build/cpp` exists.

## Integration tests

The test app runs the C++ core on an ESP32 and talks to the Rust tests in
`core/tests/qemu.rs` over UART1. Needs ESP-IDF 6's environment (`idf.py`, from
its export script) and its QEMU (`idf_tools.py install qemu-xtensa`).

```sh
make -C cpp qemu-test
```

This builds the app, boots it in QEMU with UART1 on a pty at
`build/test_app/uart1`, runs the Rust tests against it, and stops QEMU. The
device's console (UART0: its logs, at debug level) goes to
`build/test_app/qemu.log`; `tail -f` it while the tests run.

On a board: flash the app, wire UART1 (TX GPIO17, RX GPIO16) to a USB-UART
adapter, and run the same tests against it:

```sh
idf.py -C cpp/test_app -B build/test_app flash monitor
TETHER_UART=/dev/cu.usbserial-XXXX cargo test -p tether-core --test qemu -- --ignored
```

## Generated code

Generated code is checked in, so none of this is needed to build or test; it
is when a schema or `tether-gen` changes. It needs flatc 25.12.19 exactly: the
same version as the `flatbuffers` crate, the headers pinned in
`cpp/tether/third_party/flatbuffers` and the parser linked into `tether-gen`
(which itself needs no flatc). And nightly rustfmt.

```sh
make            # regenerates what's out of date
```

| Output                                        | From                          |
|-----------------------------------------------|-------------------------------|
| `core/src/wire_generated.rs`                  | `schema/wire.fbs`             |
| `cpp/tether/generated/tether/wire_generated.h`| `schema/wire.fbs`             |
| `core/tests/generated/coprocessor_generated.rs` | `core/tests/coprocessor.fbs`, via flatc |
| `core/tests/generated/coprocessor_rpc.rs`     | `core/tests/coprocessor.fbs`, via `tether-gen` |

### Golden frames

A wire format change also changes `golden/frames.json`, and the golden tests
fail until it's regenerated (this also needs flatc):

```sh
UPDATE_GOLDEN=1 cargo test -p tether-core --test golden
```

Then make the C++ core pass `make -C cpp test` and `make -C cpp qemu-test`
again. See `golden/README.md` for the file's format.
