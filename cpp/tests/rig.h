// A server and a client linked to it, for testing what runs on the server. The
// wire between them is lossless (the link's own tests cover loss), so an
// exchange completes in `pump()`, without the time passing.

#pragma once

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <span>
#include <utility>
#include <vector>

#include "tether/server.h"

namespace tether::testing {

using namespace std::chrono_literals;
using ::testing::Eq;

constexpr std::size_t kMaxPayload = 256;
constexpr std::size_t kMaxCalls = 4;
constexpr std::size_t kMaxStreams = 2;
constexpr std::size_t kMaxLatest = 96;

using TestServer = StaticServer<kMaxPayload, kMaxCalls, kMaxLatest>;
using ClientLink = StaticLink<kMaxPayload>;

inline std::vector<std::byte> bytes(std::initializer_list<uint8_t> values) {
  std::vector<std::byte> out;
  for (const uint8_t v : values) {
    out.push_back(std::byte(v));
  }
  return out;
}

struct Received {
  Header header;
  std::vector<std::byte> payload;
};

// A Credits payload granting `credit` more items on `call_id`.
inline std::vector<std::byte> credits(uint32_t call_id, uint16_t credit) {
  flatbuffers::FlatBufferBuilder fbb;
  const std::vector<wire::Grant> grants{wire::Grant(call_id, credit)};
  fbb.Finish(wire::CreateCreditsDirect(fbb, &grants));
  const auto data =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  return {data.begin(), data.end()};
}

// A server and a client linked to it.
class Rig {
 public:
  Rig() : server(0xB1, {}, kMaxStreams), client(0xA1) {
    pump();
    EXPECT_THAT(server.link().state(), Eq(LinkState::Linked));
  }

  TestServer server;
  ClientLink client;
  Millis now{0};
  // What the client has received, not counting the link's own frames.
  std::vector<Received> received;

  // Moves bytes both ways until nothing more is due.
  void pump() {
    for (bool moved = true; moved;) {
      moved = false;
      while (const auto wire = client.poll_transmit(now)) {
        server.receive(*wire, now);
        moved = true;
      }
      while (const auto wire = server.poll_transmit(now)) {
        client.receive(*wire, now, [&](const Frame& frame) {
          received.push_back(
              {.header = frame.header,
               .payload = {frame.payload.begin(), frame.payload.end()}});
        });
        moved = true;
      }
    }
  }

  void send(const Header& header, const std::vector<std::byte>& payload = {}) {
    ASSERT_TRUE(client.send(header, payload));
    pump();
  }

  void request(uint32_t call, MethodId method,
               const std::vector<std::byte>& payload = {}) {
    send({.kind = Kind::Request,
          .call_id = call,
          .service = method.service,
          .method = method.method},
         payload);
  }

  void open(uint32_t call, MethodId method, uint16_t credit,
            const std::vector<std::byte>& payload = {}) {
    send({.kind = Kind::Open,
          .call_id = call,
          .service = method.service,
          .method = method.method,
          .credit = credit},
         payload);
  }

  void grant(uint32_t call, uint16_t credit) {
    send({.kind = Kind::Credit}, credits(call, credit));
  }

  void cancel(uint32_t call) { send({.kind = Kind::Cancel, .call_id = call}); }

  // What the client received since the last call.
  std::vector<Received> take() { return std::exchange(received, {}); }
};

}  // namespace tether::testing
