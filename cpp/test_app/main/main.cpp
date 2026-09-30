// The device side of tether's integration test. It serves the Rust tests in
// core/tests/qemu.rs over UART1; UART0 is the console.
//
// For now it runs a link and echoes every frame the link delivers back through
// the link. Credits payloads are verified with flatbuffers here on the target
// first, alignment checks included; a failure comes back as DATA_LOSS in the
// echoed header's status. When the link ends (the host restarted, or went
// quiet), it starts a new one, as a device would after rebooting.

#include <algorithm>
#include <array>
#include <chrono>
#include <cstddef>
#include <optional>
#include <span>

#include "driver/uart.h"
#include "esp_log.h"
#include "esp_random.h"
#include "tether/link.h"

namespace {

using tether::Millis;

constexpr char kTag[] = "tether_test";

constexpr uart_port_t kPort = UART_NUM_1;
constexpr int kTxPin = 17;
constexpr int kRxPin = 16;
constexpr int kBaudRate = 921600;
constexpr int kUartBufferSize = 4096;

constexpr std::size_t kMaxPayload = 1024;

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

Millis now() {
  return std::chrono::duration_cast<Millis>(
      std::chrono::steady_clock::now().time_since_epoch());
}

const char* name(tether::LinkState state) {
  switch (state) {
    case tether::LinkState::Connecting:
      return "connecting";
    case tether::LinkState::Linked:
      return "linked";
    case tether::LinkState::PeerRebooted:
      return "peer rebooted";
    case tether::LinkState::PeerLost:
      return "peer lost";
  }
  return "?";
}

// Whether a Credits payload passes the flatbuffers verifier.
bool verifies(std::span<const std::byte> payload) {
  flatbuffers::Verifier verifier(
      reinterpret_cast<const uint8_t*>(payload.data()), payload.size());
  return verifier.VerifyBuffer<tether::wire::Credits>();
}

void echo(tether::Link& link, const tether::Frame& frame) {
  tether::Header header = frame.header;
  ESP_LOGD(kTag, "echoing %s, %zu payload bytes",
           tether::wire::EnumNameKind(header.kind), frame.payload.size());
  if (header.kind == tether::Kind::Credit && !verifies(frame.payload)) {
    ESP_LOGW(kTag, "Credits payload failed to verify");
    header.status = tether::WireStatus::DATA_LOSS;
  }
  // The host waits for each echo before sending more, so there's room.
  if (!link.send(header, frame.payload)) {
    ESP_LOGE(kTag, "no room to echo");
  }
}

// Runs the link until it ends.
void serve(tether::Link& link) {
  std::array<std::byte, 256> rx{};
  tether::LinkState state = link.state();
  while (!tether::is_terminal(link.state())) {
    while (const auto wire = link.poll_transmit(now())) {
      uart_write_bytes(kPort, wire->data(), wire->size());
    }
    if (link.state() != state) {
      state = link.state();
      ESP_LOGI(kTag, "link %s", name(state));
    }
    // Sleep until bytes arrive or the link's next deadline. uart_read_bytes
    // waits for all the bytes asked for, so ask for what's buffered, or one.
    const Millis wait =
        std::max(Millis(0), link.next_deadline().value_or(now()) - now());
    std::size_t buffered = 0;
    ESP_ERROR_CHECK(uart_get_buffered_data_len(kPort, &buffered));
    const int n = uart_read_bytes(
        kPort, rx.data(), std::clamp<std::size_t>(buffered, 1, rx.size()),
        pdMS_TO_TICKS(wait.count()));
    if (n <= 0) {
      continue;
    }
    ESP_LOGD(kTag, "read %d bytes", n);
    ESP_LOG_BUFFER_HEXDUMP(kTag, rx.data(), n, ESP_LOG_DEBUG);
    link.receive(std::span(rx).first(n), now(),
                 [&](const tether::Frame& frame) { echo(link, frame); });
  }
  const auto& stats = link.stats();
  ESP_LOGW(kTag,
           "link %s (tx %lu, rx %lu, retransmits %lu, duplicates %lu, crc "
           "errors %lu, cobs errors %lu, other errors %lu)",
           name(link.state()), static_cast<unsigned long>(stats.frames_tx),
           static_cast<unsigned long>(stats.frames_rx),
           static_cast<unsigned long>(stats.retransmits),
           static_cast<unsigned long>(stats.duplicates),
           static_cast<unsigned long>(stats.crc_errors),
           static_cast<unsigned long>(stats.cobs_errors),
           static_cast<unsigned long>(stats.other_errors));
}

}  // namespace

extern "C" void app_main() {
  // Verbose while the integration test is young.
  esp_log_level_set(kTag, ESP_LOG_DEBUG);
  uart_init();
  // Too large for the stack.
  static std::optional<tether::StaticLink<kMaxPayload>> link;
  for (;;) {
    const uint32_t boot_id = esp_random() | 1;
    ESP_LOGI(kTag, "new link, boot id %08lx",
             static_cast<unsigned long>(boot_id));
    link.emplace(boot_id, tether::LinkConfig{.baud_rate = kBaudRate});
    serve(*link);
  }
}
