#!/usr/bin/env bash
# Runs tether's integration test: builds the test app, boots it in QEMU with
# UART1 on a pty, and runs the Rust side (core/tests/qemu.rs) against it.
# QEMU's console log is left in the build directory.

set -euo pipefail
set -m  # QEMU gets its own process group, so it can be stopped as a whole.

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
build="$root/build/test_app"
uart="$build/uart1"  # QEMU links it to the pty it creates.

idf.py -C "$here" -B "$build" build

rm -f "$uart"
# The first serial port (UART0, the console) is idf.py's; this is UART1.
idf.py -C "$here" -B "$build" qemu --qemu-extra-args="-serial pty:$uart" \
  > "$build/qemu.log" 2>&1 < /dev/null &
qemu=$!
trap 'kill -- -"$qemu" 2>/dev/null || true' EXIT

cd "$root"
TETHER_UART="$uart" TETHER_UART_BAUD=0 \
  cargo test --offline -p tether-core --test qemu -- --ignored "$@"
