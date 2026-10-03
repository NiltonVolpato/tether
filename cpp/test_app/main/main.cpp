// The device side of tether's integration test. It serves the Rust tests in
// core/tests/qemu.rs over UART1; UART0 is the console.
//
// It runs a server with the Greeter service (greeter_app.h) behind a router,
// on an I/O task of its own, the way an application would (see
// docs/integration.md). When the link ends (the host restarted, or went
// quiet), it starts a new one, as after a reboot.

#include <cstddef>
#include <cstdint>

#include "esp_log.h"
#include "esp_random.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "greeter_app.h"
#include "tether/router.h"
#include "tether/server.h"
#include "tether_idf/uart_io.h"

namespace {

constexpr char kTag[] = "tether_test";

constexpr uint32_t kBaudRate = 921600;
constexpr std::size_t kMaxPayload = 1024;
constexpr std::size_t kMaxCalls = 4;
constexpr std::size_t kMaxStreams = 2;

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

uint32_t new_boot_id() { return esp_random() | 1; }

// Everything, for as long as the app runs.
struct App {
  tether::idf::UartIo io{{
      .port = UART_NUM_1,
      .tx_pin = 17,
      .rx_pin = 16,
      .baud_rate = kBaudRate,
  }};
  tether::StaticServer<kMaxPayload, kMaxCalls, greeter_app::kMaxLatest> server{
      new_boot_id(), {.baud_rate = kBaudRate}, kMaxStreams};
  tether::StaticRouter<Test::kServer.size()> router{server, Test::kServer};
  greeter_app::GreeterApp greeter{io};
  Test::greeter::Service service{greeter};
};

void log_end(const App& app) {
  const auto& server = app.server;
  const auto& stats = server.link().stats();
  ESP_LOGW(kTag,
           "link %s (tx %lu, rx %lu, retransmits %lu, duplicates %lu, crc "
           "errors %lu, cobs errors %lu, other errors %lu, overflows %lu)",
           name(server.link().state()),
           static_cast<unsigned long>(stats.frames_tx),
           static_cast<unsigned long>(stats.frames_rx),
           static_cast<unsigned long>(stats.retransmits),
           static_cast<unsigned long>(stats.duplicates),
           static_cast<unsigned long>(stats.crc_errors),
           static_cast<unsigned long>(stats.cobs_errors),
           static_cast<unsigned long>(stats.other_errors),
           static_cast<unsigned long>(app.io.stats().overflows));
}

// The I/O task: one link after another.
[[noreturn]] void serve(void* arg) {
  App& app = *static_cast<App*>(arg);
  for (;;) {
    ESP_LOGI(kTag, "new link, boot id %08lx",
             static_cast<unsigned long>(app.server.link().boot_id()));
    app.io.serve(app.server);
    log_end(app);
    app.server.restart(new_boot_id());
    app.greeter.new_link();
  }
}

}  // namespace

extern "C" void app_main() {
  // Verbose while the integration test is young.
  esp_log_level_set(kTag, ESP_LOG_DEBUG);
  // Too large for a stack.
  static App app;
  app.router.add(app.service);
  app.greeter.start();
  // Above the apps', so acks go out promptly.
  xTaskCreate(serve, "tether_io", 4096, &app, 10, nullptr);
}
