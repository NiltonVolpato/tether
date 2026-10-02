// The test app's service: the application's code behind the interface
// tether-gen generated from ../greeter.fbs (see that file for what the methods
// do).

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <optional>

#include "generated/greeter_rpc.h"
#include "tether/typed.h"

namespace greeter_app {

// The latest-value memory the server needs for Burst: a Number.
inline constexpr std::size_t kMaxLatest = 32;

class GreeterApp final : public Test::greeter::Handler {
 public:
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
    for (auto& slot : countdowns_) {
      if (!slot) {
        slot = Countdown{.sink = sink, .next = request.n()};
        pump();
        return;
      }
    }
    (void)sink.end(tether::WireStatus::RESOURCE_EXHAUSTED);
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

  void cancelled(tether::CallId call) override {
    ++cancelled_;
    for (auto& slot : countdowns_) {
      if (slot && slot->sink.call_id() == call) {
        slot.reset();
      }
    }
  }

  // Sends what the countdowns have credit for. Called after anything arrives,
  // which is when credit does.
  void pump() {
    for (auto& slot : countdowns_) {
      if (!slot) {
        continue;
      }
      while (slot->next > 0 &&
             slot->sink.send([&](flatbuffers::FlatBufferBuilder& fbb) {
               return Test::CreateNumber(fbb, slot->next);
             })) {
        --slot->next;
      }
      if (slot->next == 0 && slot->sink.end()) {
        slot.reset();
      }
    }
  }

 private:
  struct Countdown {
    tether::Sink<Test::Number> sink;
    // The next item, and how many are left.
    uint8_t next;
  };

  uint32_t calls_ = 0;
  uint32_t cancelled_ = 0;
  // At most as many as the server has streams.
  std::array<std::optional<Countdown>, 2> countdowns_;
};

}  // namespace greeter_app
