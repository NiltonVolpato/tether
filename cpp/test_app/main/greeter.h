// The test app's service, written by hand the way tether-gen is to generate
// them: a table of what it offers, and a Service on top of the app's code.
// Payloads are raw bytes, which the core doesn't look into anyway.
//
//   Echo(bytes): bytes                      unary: answers with the request
//   Countdown(n: u8): u8 (streaming)        n, n-1, ..., 1, as credit allows
//   Stats(): [calls: u32, cancelled: u32]   unary, little-endian
//
// Echo with no bytes is fine; Countdown without exactly one byte is
// INVALID_ARGUMENT.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>

#include "tether/descriptor.h"
#include "tether/router.h"

namespace greeter {

inline constexpr uint8_t kId = 0;
inline constexpr uint8_t kEcho = 0;
inline constexpr uint8_t kCountdown = 1;
inline constexpr uint8_t kStats = 2;

inline constexpr std::array<std::optional<tether::MethodInfo>, 3> kMethods{
    tether::MethodInfo{.name = "Echo", .streaming = false},
    tether::MethodInfo{.name = "Countdown", .streaming = true},
    tether::MethodInfo{.name = "Stats", .streaming = false}};

// The server's table: this one service.
inline constexpr std::array<std::optional<tether::ServiceInfo>, 1> kTable{
    tether::ServiceInfo{.name = "Test.Greeter", .methods = kMethods}};

class Greeter final : public tether::Service {
 public:
  [[nodiscard]] uint8_t id() const override { return kId; }

  void call(uint8_t method, std::span<const std::byte> request,
            tether::Reply reply) override {
    ++calls_;
    switch (method) {
      case kEcho:
        (void)reply.send(request);
        break;
      case kStats:
        stats(reply);
        break;
      default:
        (void)reply.fail(tether::WireStatus::UNIMPLEMENTED);
        break;
    }
  }

  void open(uint8_t method, std::span<const std::byte> request,
            tether::Sink sink) override {
    ++calls_;
    if (method != kCountdown) {
      (void)sink.end(tether::WireStatus::UNIMPLEMENTED);
      return;
    }
    if (request.size() != 1) {
      (void)sink.end(tether::WireStatus::INVALID_ARGUMENT);
      return;
    }
    for (auto& slot : countdowns_) {
      if (!slot) {
        slot = Countdown{.sink = sink,
                         .next = std::to_integer<uint8_t>(request[0])};
        pump();
        return;
      }
    }
    (void)sink.end(tether::WireStatus::RESOURCE_EXHAUSTED);
  }

  void cancelled(tether::CallId call) override {
    ++cancelled_;
    for (auto& slot : countdowns_) {
      if (slot && slot->sink.call_id() == call) {
        slot.reset();
      }
    }
  }

  // Sends what the channels have credit for. Called after anything arrives,
  // which is when credit does.
  void pump() {
    for (auto& slot : countdowns_) {
      if (!slot) {
        continue;
      }
      while (slot->next > 0 &&
             slot->sink.send(std::as_bytes(std::span(&slot->next, 1)))) {
        --slot->next;
      }
      if (slot->next == 0 && slot->sink.end()) {
        slot.reset();
      }
    }
  }

 private:
  struct Countdown {
    tether::Sink sink;
    // The next item, and how many are left.
    uint8_t next;
  };

  void stats(tether::Reply reply) const {
    std::array<std::byte, 8> out{};
    for (int i = 0; i < 4; ++i) {
      out[i] = std::byte(calls_ >> (8 * i));
      out[4 + i] = std::byte(cancelled_ >> (8 * i));
    }
    (void)reply.send(out);
  }

  uint32_t calls_ = 0;
  uint32_t cancelled_ = 0;
  // At most as many as the server has streams.
  std::array<std::optional<Countdown>, 2> countdowns_;
};

}  // namespace greeter
