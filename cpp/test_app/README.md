# Integration test app

An ESP-IDF app that runs tether's C++ core on an ESP32 and serves the Rust
tests in `core/tests/qemu.rs` over UART1. UART0 is the console. It's built the
way ESP-IDF's own component test apps are: a small project with `COMPONENTS
main`, pulling in the component from `../tether`.

The app runs a server and a router with one service, `Greeter`, written by
hand in `main/greeter.h` the way tether-gen is to generate them: `Echo` answers
with its request, `Countdown` streams `n..1` as the client grants credit, and
`Stats` reports the calls and cancellations it saw. The Rust tests call it with
a real client, so the two cores check each other, with the C++ one built by the
target's compiler, with ESP-IDF's settings, running on (emulated) xtensa.

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
