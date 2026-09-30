// The device side of tether's integration test. It serves the Rust tests in
// core/tests/qemu.rs over UART1; UART0 is the console.
//
// For now it echoes frames: each one it decodes, it encodes back. The payloads
// the framework defines (Hello, Credits) are verified with flatbuffers here on
// the target first, alignment checks included; a failure comes back as
// DATA_LOSS in the echoed header's status.

#include <algorithm>
#include <array>
#include <cstddef>
#include <span>

#include "driver/uart.h"
#include "esp_log.h"
#include "tether/frame.h"

namespace {

constexpr char kTag[] = "tether_test";

constexpr uart_port_t kPort = UART_NUM_1;
constexpr int kTxPin = 17;
constexpr int kRxPin = 16;
constexpr int kBaudRate = 921600;
constexpr int kUartBufferSize = 4096;

// COBS-encoded frames up to this size, delimiter excluded.
constexpr std::size_t kMaxFrame = 1024;

void uart_init() {
  const uart_config_t config{
      .baud_rate = kBaudRate,
      .data_bits = UART_DATA_8_BITS,
      .parity = UART_PARITY_DISABLE,
      .stop_bits = UART_STOP_BITS_1,
      .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
      .rx_flow_ctrl_thresh = 0,
      .rx_glitch_filt_thresh = 0,
      .source_clk = UART_SCLK_DEFAULT,
      .flags = {},
  };
  ESP_ERROR_CHECK(uart_driver_install(kPort, kUartBufferSize, kUartBufferSize,
                                      0, nullptr, 0));
  ESP_ERROR_CHECK(uart_param_config(kPort, &config));
  ESP_ERROR_CHECK(uart_set_pin(kPort, kTxPin, kRxPin, UART_PIN_NO_CHANGE,
                               UART_PIN_NO_CHANGE));
}

void send(const tether::Header& header, std::span<const std::byte> payload) {
  static std::array<std::byte, tether::max_wire_size(kMaxFrame)> wire;
  const auto size = tether::encode(header, payload, wire);
  if (!size) {
    ESP_LOGE(kTag, "frame too large to send");
    return;
  }
  uart_write_bytes(kPort, wire.data(), *size);
}

// Whether a framework payload passes the flatbuffers verifier.
bool verifies(tether::Kind kind, std::span<const std::byte> payload) {
  flatbuffers::Verifier verifier(
      reinterpret_cast<const uint8_t*>(payload.data()), payload.size());
  switch (kind) {
    case tether::Kind::Hello:
      return verifier.VerifyBuffer<tether::wire::Hello>();
    case tether::Kind::Credit:
      return verifier.VerifyBuffer<tether::wire::Credits>();
    default:
      return true;
  }
}

void echo(const tether::Frame& frame) {
  tether::Header header = frame.header;
  if (!verifies(header.kind, frame.payload)) {
    ESP_LOGW(kTag, "%s payload failed to verify",
             tether::wire::EnumNameKind(header.kind));
    header.status = tether::WireStatus::DATA_LOSS;
  }
  send(header, frame.payload);
}

}  // namespace

extern "C" void app_main() {
  // Verbose while the integration test is young.
  esp_log_level_set(kTag, ESP_LOG_DEBUG);
  uart_init();
  ESP_LOGI(kTag, "ready on UART%d", static_cast<int>(kPort));
  // Tells the host it may start.
  send({.kind = tether::Kind::Ping}, {});

  static tether::StaticDeframer<kMaxFrame> deframer;
  std::array<std::byte, 256> rx{};
  for (;;) {
    // uart_read_bytes waits for all the bytes asked for, so ask for what's
    // buffered, or wait for one.
    std::size_t buffered = 0;
    ESP_ERROR_CHECK(uart_get_buffered_data_len(kPort, &buffered));
    const int n = uart_read_bytes(
        kPort, rx.data(), std::clamp<std::size_t>(buffered, 1, rx.size()),
        portMAX_DELAY);
    if (n <= 0) {
      continue;
    }
    ESP_LOGD(kTag, "read %d bytes", n);
    ESP_LOG_BUFFER_HEXDUMP(kTag, rx.data(), n, ESP_LOG_DEBUG);
    for (std::byte b : std::span(rx).first(n)) {
      const auto result = deframer.push(b);
      if (!result) {
        continue;
      }
      if (!*result) {
        ESP_LOGW(kTag, "dropped a bad frame (error %d)",
                 static_cast<int>(result->error()));
        continue;
      }
      ESP_LOGD(kTag, "echoing %s, %zu payload bytes",
               tether::wire::EnumNameKind((*result)->header.kind),
               (*result)->payload.size());
      echo(**result);
    }
  }
}
