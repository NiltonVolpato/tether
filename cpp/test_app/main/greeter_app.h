// The test app's service: the application's code behind the interface
// tether-gen generated from ../greeter.fbs (see that file for what the methods
// do).
//
// It shows both ways to answer. Echo, Stats and Burst answer in the handler,
// on the I/O task. Countdown hands its sink to a task of its own, which waits
// for credit with UartIo::wait, as a producer that blocks (on Wi-Fi, a sensor,
// a file) would.

#pragma once

#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>

#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "generated/greeter_rpc.h"
#include "tether/typed.h"
#include "tether_idf/uart_io.h"

namespace greeter_app {

using namespace std::chrono_literals;

// The latest-value memory the server needs for Burst: a Number.
inline constexpr std::size_t kMaxLatest = 32;

class GreeterApp final : public Test::greeter::Handler {
 public:
  explicit GreeterApp(tether::idf::UartIo& io) : io_(io) {}

  // Starts the task that runs the countdowns.
  void start() {
    countdowns_ = xQueueCreateStatic(kQueued, sizeof(Countdown),
                                     queue_memory_.data(), &queue_);
    xTaskCreateStatic(
        [](void* app) { static_cast<GreeterApp*>(app)->run_countdowns(); },
        "countdown", kStackSize, this, 5, stack_.data(), &task_);
  }

  void echo(tether::Reply<Test::Blob> reply,
            const Test::Blob& request) override {
    ++calls_;
    (void)reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
      // The vector is there if the request's was, empty or not.
      const auto* data = request.data();
      return Test::CreateBlob(
          fbb,
          data == nullptr ? 0 : fbb.CreateVector(data->data(), data->size()));
    });
  }

  void countdown(tether::Sink<Test::Number> sink,
                 const Test::Count& request) override {
    ++calls_;
    const Countdown countdown{.sink = sink, .from = request.n()};
    if (xQueueSend(countdowns_, &countdown, 0) != pdTRUE) {
      (void)sink.end(tether::WireStatus::RESOURCE_EXHAUSTED);
    }
  }

  void stats(tether::Reply<Test::Counters> reply,
             const Test::Empty& /*request*/) override {
    ++calls_;
    (void)reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
      return Test::CreateCounters(fbb, calls_, cancelled_);
    });
  }

  void burst(tether::Sink<Test::Number> sink,
             const Test::Count& request) override {
    ++calls_;
    // As fast as it can: whatever the client can't take yet is replaced.
    for (uint8_t value = 1; value <= request.n(); ++value) {
      (void)sink.set_latest([&](flatbuffers::FlatBufferBuilder& fbb) {
        return Test::CreateNumber(fbb, value);
      });
    }
  }

  // A cancelled countdown needs nothing: its task finds the sink closed.
  void cancelled(tether::CallId /*call*/) override { ++cancelled_; }

  // Stats counts a link's calls: from the I/O task, between links.
  void new_link() {
    calls_ = 0;
    cancelled_ = 0;
  }

 private:
  struct Countdown {
    tether::Sink<Test::Number> sink;
    uint8_t from = 0;
  };

  static constexpr std::size_t kQueued = 4;
  static constexpr std::size_t kStackSize = 4096;

  // One countdown at a time, each item as the client grants credit for it.
  [[noreturn]] void run_countdowns() {
    for (;;) {
      Countdown countdown;
      xQueueReceive(countdowns_, &countdown, portMAX_DELAY);
      const auto& sink = countdown.sink;
      std::expected<void, tether::CallError> sent;
      for (uint8_t n = countdown.from; n > 0 && sent; --n) {
        sent = io_.wait(
            [&] {
              return sink.send([&](flatbuffers::FlatBufferBuilder& fbb) {
                return Test::CreateNumber(fbb, n);
              });
            },
            10s);
      }
      if (sent) {
        sent = io_.wait([&] { return sink.end(); }, 10s);
      }
      if (!sent && sent.error() != tether::CallError::Closed) {
        // The client stopped taking items.
        (void)sink.end(tether::WireStatus::DEADLINE_EXCEEDED);
      }
    }
  }

  tether::idf::UartIo& io_;
  // Counted on the I/O task.
  uint32_t calls_ = 0;
  uint32_t cancelled_ = 0;

  QueueHandle_t countdowns_ = nullptr;
  StaticQueue_t queue_{};
  std::array<uint8_t, kQueued * sizeof(Countdown)> queue_memory_{};
  StaticTask_t task_{};
  std::array<StackType_t, kStackSize> stack_{};
};

}  // namespace greeter_app
