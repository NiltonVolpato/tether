// The link between two C++ ends over simulated lossy pipes, the way
// core/tests/sim.rs tests the Rust one. The simulation is event driven, as the
// device's task will be: an end is only polled when bytes arrive for it or at
// its next_deadline, so these also check that next_deadline is never late.

#include "tether/link.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <algorithm>
#include <deque>
#include <functional>
#include <memory>
#include <vector>

namespace tether {
namespace {

using namespace std::chrono_literals;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::Optional;

constexpr std::size_t kMaxPayload = 256;
using TestLink = StaticLink<kMaxPayload>;

class Rng {
 public:
  explicit Rng(uint64_t seed) : state_(seed | 1) {}

  // xorshift64*, as in the Rust simulation.
  uint64_t next() {
    state_ ^= state_ >> 12;
    state_ ^= state_ << 25;
    state_ ^= state_ >> 27;
    return state_ * 0x2545F4914F6CDD1DULL;
  }
  uint64_t below(uint64_t n) { return next() % n; }
  bool percent(uint64_t p) { return below(100) < p; }

 private:
  uint64_t state_;
};

// One direction of the UART.
class Pipe {
 public:
  Pipe(uint64_t seed, uint64_t drop_pct, uint64_t corrupt_pct)
      : rng_(seed), drop_pct_(drop_pct), corrupt_pct_(corrupt_pct) {}

  // Frames to drop unconditionally, counted from the start.
  int drop_first = 0;
  // Drops everything: a disconnected wire or a hung peer.
  bool cut = false;

  void push(Millis now, std::span<const std::byte> frame) {
    if (cut || (drop_first > 0 && drop_first-- > 0) ||
        rng_.percent(drop_pct_)) {
      return;
    }
    std::vector<std::byte> bytes(frame.begin(), frame.end());
    if (rng_.percent(corrupt_pct_)) {
      // Any byte, delimiter included.
      bytes[rng_.below(bytes.size())] ^= std::byte(1U << rng_.below(8));
    }
    in_flight_.emplace_back(now + Millis(1 + rng_.below(3)), std::move(bytes));
  }

  [[nodiscard]] std::optional<Millis> next_arrival() const {
    if (in_flight_.empty()) {
      return std::nullopt;
    }
    return in_flight_.front().first;
  }

  // Bytes due at `now`, in arbitrary read-sized chunks.
  std::vector<std::vector<std::byte>> deliver(Millis now) {
    std::vector<std::byte> bytes;
    while (!in_flight_.empty() && in_flight_.front().first <= now) {
      auto& frame = in_flight_.front().second;
      bytes.insert(bytes.end(), frame.begin(), frame.end());
      in_flight_.pop_front();
    }
    std::vector<std::vector<std::byte>> chunks;
    for (std::size_t i = 0; i < bytes.size();) {
      const std::size_t n =
          std::min<std::size_t>(1 + rng_.below(64), bytes.size() - i);
      chunks.emplace_back(bytes.begin() + i, bytes.begin() + i + n);
      i += n;
    }
    return chunks;
  }

 private:
  Rng rng_;
  uint64_t drop_pct_;
  uint64_t corrupt_pct_;
  std::deque<std::pair<Millis, std::vector<std::byte>>> in_flight_;
};

struct Received {
  Header header;
  std::vector<std::byte> payload;
};

struct End {
  std::unique_ptr<TestLink> link;
  std::vector<Received> received;
};

class Sim {
 public:
  explicit Sim(uint64_t seed = 1, uint64_t loss_pct = 0)
      : a2b(seed * 3, loss_pct, loss_pct), b2a(seed * 7, loss_pct, loss_pct) {
    a.link = std::make_unique<TestLink>(
        static_cast<uint32_t>((0xA000'0000 ^ seed) | 1));
    b.link = std::make_unique<TestLink>(
        static_cast<uint32_t>((0xB000'0000 ^ seed) | 1));
  }

  End a;
  End b;
  Pipe a2b;
  Pipe b2a;
  Millis now{0};

  // Runs until `done` holds; fails after `max` of simulated time.
  void run_until(Millis max, const std::function<bool()>& done) {
    const Millis deadline = now + max;
    while (!done()) {
      ASSERT_LT(now, deadline) << "condition not met in time";
      step(deadline);
    }
  }

  void run_for(Millis duration) {
    const Millis end = now + duration;
    run_until(duration + 1ms, [&] { return now >= end; });
  }

  void linked() {
    run_until(5s, [&] {
      return a.link->state() == LinkState::Linked &&
             b.link->state() == LinkState::Linked;
    });
  }

 private:
  static void transmit(End& end, Pipe& out, Millis now) {
    while (const auto wire = end.link->poll_transmit(now)) {
      out.push(now, *wire);
    }
  }

  static void receive(End& end, Pipe& in, Millis now) {
    for (const auto& chunk : in.deliver(now)) {
      end.link->receive(chunk, now, [&](const Frame& f) {
        end.received.push_back(
            {.header = f.header,
             .payload = {f.payload.begin(), f.payload.end()}});
      });
    }
  }

  // Jumps to the next arrival or deadline, delivers what's arrived, and sends
  // what's due, so `done` sees the state at the time it changed.
  void step(Millis limit) {
    Millis next = limit;
    for (const auto t : {a2b.next_arrival(), b2a.next_arrival(),
                         a.link->next_deadline(), b.link->next_deadline()}) {
      if (t) {
        next = std::min(next, *t);
      }
    }
    // Everything due at `now` went out in the last step.
    now = std::max(next, now + 1ms);
    receive(b, a2b, now);
    receive(a, b2a, now);
    transmit(a, a2b, now);
    transmit(b, b2a, now);
  }
};

std::vector<std::byte> payload_of(std::size_t size, uint8_t seed) {
  std::vector<std::byte> payload(size);
  for (std::size_t i = 0; i < size; ++i) {
    payload[i] = std::byte(static_cast<uint8_t>(seed + i * 7));
  }
  return payload;
}

TEST(Link, Links) {
  Sim sim;
  sim.linked();
  EXPECT_THAT(sim.a.link->peer_boot_id(), Optional(0xB000'0001U));
  EXPECT_THAT(sim.b.link->peer_boot_id(), Optional(0xA000'0001U));
}

TEST(Link, DeliversExactlyOnceInOrderOverLossyLink) {
  for (uint64_t seed = 1; seed <= 30; ++seed) {
    SCOPED_TRACE(seed);
    Sim sim(seed, 15);
    constexpr int kFrames = 40;
    // Both ends send at once, sizes across every padding and up to the max;
    // queued before linking, and whenever the queue has room.
    int sent_a = 0;
    int sent_b = 0;
    const auto header = [](int i) {
      return Header{.kind = Kind::Item, .call_id = static_cast<uint32_t>(i)};
    };
    const auto size = [](int i) { return (i * 37) % (kMaxPayload + 1); };
    const auto fill = [&](End& end, int& sent) {
      while (sent < kFrames &&
             end.link->send(header(sent), payload_of(size(sent), sent))) {
        ++sent;
      }
    };
    sim.run_until(120s, [&] {
      fill(sim.a, sent_a);
      fill(sim.b, sent_b);
      return sim.a.received.size() == kFrames &&
             sim.b.received.size() == kFrames;
    });
    for (End* end : {&sim.a, &sim.b}) {
      for (int i = 0; i < kFrames; ++i) {
        const auto& got = end->received[i];
        EXPECT_THAT(got.header.call_id, Eq(static_cast<uint32_t>(i)));
        EXPECT_THAT(got.payload, ElementsAreArray(payload_of(size(i), i)));
      }
    }
    const auto& stats = sim.a.link->stats();
    EXPECT_GT(stats.retransmits, 0U);
    EXPECT_GT(stats.crc_errors + stats.cobs_errors, 0U);
    EXPECT_THAT(sim.a.link->state(), Eq(LinkState::Linked));
  }
}

TEST(Link, LostHelloReplyIsNotAReboot) {
  Sim sim;
  sim.b2a.drop_first = 3;
  sim.linked();
  sim.run_for(500ms);
  EXPECT_THAT(sim.a.link->state(), Eq(LinkState::Linked));
  EXPECT_THAT(sim.b.link->state(), Eq(LinkState::Linked));
}

TEST(Link, QueuedHellosFromOneBootLinkOnce) {
  // The S3 keeps retrying Hello while the co-processor reboots, so it comes
  // up to several of them queued. Only a new boot id means another reboot.
  const LinkConfig config;
  TestLink client(0xC2, config);
  std::vector<std::vector<std::byte>> hellos;
  for (int i = 0; i < 4; ++i) {
    const auto wire = client.poll_transmit(i * config.hello_interval);
    ASSERT_TRUE(wire);
    hellos.emplace_back(wire->begin(), wire->end());
  }
  TestLink server(0x52, config);
  for (const auto& hello : hellos) {
    server.receive(hello, 0ms, [](const Frame&) {});
  }
  EXPECT_THAT(server.state(), Eq(LinkState::Linked));
  EXPECT_THAT(server.peer_boot_id(), Optional(0xC2U));
}

TEST(Link, PeerRebootIsDetected) {
  Sim sim;
  sim.linked();
  sim.b.link = std::make_unique<TestLink>(0xBEEF);
  sim.run_until(1s,
                [&] { return sim.a.link->state() == LinkState::PeerRebooted; });
  // The rebooted end must not have linked to the doomed one.
  EXPECT_THAT(sim.b.link->state(), Eq(LinkState::Connecting));
  EXPECT_THAT(sim.a.link->next_deadline(), Eq(std::nullopt));
}

TEST(Link, IdleLinkPingsSparingly) {
  Sim sim;
  sim.linked();
  sim.run_for(10s);
  const uint32_t pings = sim.a.link->stats().pings + sim.b.link->stats().pings;
  // One per interval (40 in 10 s), and up to twice that when both ends' timers
  // expire within a latency of each other and their pings cross.
  EXPECT_GE(pings, 40U);
  EXPECT_LE(pings, 80U);
  EXPECT_THAT(sim.a.link->state(), Eq(LinkState::Linked));
}

TEST(Link, SilentPeerIsLost) {
  Sim sim;
  sim.linked();
  sim.b2a.cut = true;
  const Millis cut_at = sim.now;
  sim.run_until(2s, [&] { return sim.a.link->state() == LinkState::PeerLost; });
  // An idle link: one ping interval, then the retransmits.
  const LinkConfig config;
  std::array<std::byte, max_wire_size(0)> ping{};
  const auto ping_size = encode({.kind = Kind::Ping, .seq = 1}, {}, ping);
  ASSERT_TRUE(ping_size);
  EXPECT_LE(sim.now - cut_at,
            config.ping_interval +
                (config.max_retransmits + 1) *
                    sim.a.link->retransmit_timeout(*ping_size) +
                10ms);
  EXPECT_THAT(sim.a.link->next_deadline(), Eq(std::nullopt));
  EXPECT_THAT(sim.a.link->poll_transmit(sim.now), Eq(std::nullopt));
}

TEST(Link, QueueHoldsManySmallFramesAndReportsFull) {
  TestLink link(1);
  int queued = 0;
  while (link.send({.kind = Kind::Cancel}, {})) {
    ++queued;
  }
  EXPECT_GT(queued, 10);
  EXPECT_THAT(link.send({.kind = Kind::Cancel}, {}),
              Eq(std::unexpected(SendError::QueueFull)));

  TestLink other(2);
  ASSERT_TRUE(other.send({.kind = Kind::Item}, payload_of(kMaxPayload, 1)));
  ASSERT_TRUE(other.send({.kind = Kind::Item}, payload_of(kMaxPayload, 2)));
  EXPECT_THAT(other.send({.kind = Kind::Item}, payload_of(kMaxPayload, 3)),
              Eq(std::unexpected(SendError::QueueFull)));

  TestLink small(3);
  EXPECT_THAT(small.send({.kind = Kind::Item}, payload_of(4 * kMaxPayload, 1)),
              Eq(std::unexpected(SendError::TooLarge)));
}

}  // namespace
}  // namespace tether
