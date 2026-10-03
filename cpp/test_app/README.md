# Integration test app

An ESP-IDF app that runs tether's C++ core on an ESP32 and serves the Rust
tests in `core/tests/qemu.rs` over UART1. UART0 is the console. It's built the
way ESP-IDF's own component test apps are: a small project with `COMPONENTS
main`, pulling in the components from `../tether` and `../tether_idf`.

The app runs a server and a router with one service, `Greeter`, from
`greeter.fbs`, on an I/O task of its own (`tether_idf`'s `UartIo`), as
`docs/integration.md` describes. `Echo` answers with its request, `Countdown`
streams `n..1` as the client grants credit (from a task of its own, which waits
for the credit), `Burst` sets `1..n` as the latest value, and `Stats` reports
the calls and cancellations it saw on the current link. The interface it implements
(`main/generated/greeter_rpc.h`) is tether-gen's C++ output; the Rust tests call
it with the client tether-gen generates from the same schema. So the two cores
and the two generators check each other, with the C++ one built by the target's
compiler, with ESP-IDF's settings, running on (emulated) xtensa.

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
