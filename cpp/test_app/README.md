# Integration test app

An ESP-IDF app that runs tether's C++ core on an ESP32 and serves the Rust
tests in `core/tests/qemu.rs` over UART1. UART0 is the console. It's built the
way ESP-IDF's own component test apps are: a small project with `COMPONENTS
main`, pulling in the component from `../tether`.

For now the app echoes frames: whatever it decodes, it encodes back. It first
verifies the framework's own payloads (Hello, Credits) with flatbuffers, on the
target; a failure comes back as `DATA_LOSS` in the echoed header. So the tests
check the two cores against each other with the C++ one built by the target's
compiler, with ESP-IDF's settings, running on (emulated) xtensa. It'll grow into
the link, calls and channels as the C++ core does.

## In QEMU

Needs ESP-IDF's environment (`idf.py`, and QEMU installed with `idf_tools.py`):

```sh
make -C cpp qemu-test
```

`qemu-test.sh` builds the app into `build/test_app`, starts QEMU with UART1 on
a pty linked at `build/test_app/uart1`, and runs the Rust tests against it.
QEMU's console output ends up in `build/test_app/qemu.log`.

## On a board

Flash it (`idf.py -C cpp/test_app -B build/test_app flash monitor`), wire
UART1 (TX GPIO17, RX GPIO16) to a USB-UART adapter, and point the tests at it:

```sh
TETHER_UART=/dev/cu.usbserial-XXXX \
  cargo test -p tether-core --test qemu -- --ignored
```

It runs at 921600 baud; `TETHER_UART_BAUD` changes what the tests use.
