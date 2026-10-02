// The device side of tether's integration test. It serves the Rust tests in
// core/tests/qemu.rs over UART1; UART0 is the console.
//
// It runs a server with the Greeter service (greeter_app.h) behind a router.
// When the link ends (the host restarted, or went quiet), it starts a new one,
// as a device would after rebooting.

#include <algorithm>
#include <array>
#include <chrono>
#include <cstddef>
#include <optional>
#include <span>

#include "driver/uart.h"
#include "esp_log.h"
#include "esp_random.h"
#include "greeter_app.h"
#include "tether/router.h"
#include "tether/server.h"

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

constexpr std::size_t kMaxCalls = 4;
constexpr std::size_t kMaxStreams = 2;

// A server and what it serves, all of one link's life.
struct App {
  explicit App(uint32_t boot_id)
      : server(boot_id, tether::LinkConfig{.baud_rate = kBaudRate},
               kMaxStreams),
        router(server, Test::kServer) {
    router.add(service);
  }

  tether::StaticServer<kMaxPayload, kMaxCalls, greeter_app::kMaxLatest> server;
  tether::StaticRouter<Test::kServer.size()> router;
  greeter_app::GreeterApp greeter;
  Test::greeter::Service service{greeter};
};

// Runs the server until its link ends.
void serve(App& app) {
  tether::Server& server = app.server;
  std::array<std::byte, 256> rx{};
  tether::LinkState state = server.link().state();
  while (!tether::is_terminal(server.link().state())) {
    while (const auto wire = server.poll_transmit(now())) {
      uart_write_bytes(kPort, wire->data(), wire->size());
    }
    if (server.link().state() != state) {
      state = server.link().state();
      ESP_LOGI(kTag, "link %s", name(state));
    }
    // Sleep until bytes arrive or the link's next deadline. uart_read_bytes
    // waits for all the bytes asked for, so ask for what's buffered, or one.
    const Millis wait =
        std::max(Millis(0), server.next_deadline().value_or(now()) - now());
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
    server.receive(std::span(rx).first(n), now());
    // Credit may have arrived.
    app.greeter.pump();
  }
  const auto& stats = server.link().stats();
  ESP_LOGW(kTag,
           "link %s (tx %lu, rx %lu, retransmits %lu, duplicates %lu, crc "
           "errors %lu, cobs errors %lu, other errors %lu)",
           name(server.link().state()),
           static_cast<unsigned long>(stats.frames_tx),
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
  static std::optional<App> app;
  for (;;) {
    const uint32_t boot_id = esp_random() | 1;
    ESP_LOGI(kTag, "new link, boot id %08lx",
             static_cast<unsigned long>(boot_id));
    app.emplace(boot_id);
    serve(*app);
  }
}
