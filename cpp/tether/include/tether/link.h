// Reliable, in-order delivery of frames over a lossy byte stream: the same
// protocol as the Rust core's link (core/src/link.rs).
//
// A Hello handshake, then stop-and-wait ARQ: one sequenced frame in flight per
// direction, retransmitted until acked; the receiver drops duplicates, so the
// layer above sees every frame exactly once, in order. A Ping goes out when
// nothing has arrived for a while, and a frame retransmitted too often means
// the peer is gone.
//
// Sans-IO: the caller feeds received bytes in, writes out what poll_transmit
// returns, and passes the time in. next_deadline says when to call again if
// nothing arrives first.

#pragma once

#include <array>
#include <chrono>
#include <concepts>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <optional>
#include <span>

#include "tether/frame.h"

namespace tether {

// Time on a monotonic clock, from any epoch.
using Millis = std::chrono::milliseconds;

struct LinkConfig {
  Millis retransmit{20};
  // Retransmits of one frame without an ack before the peer is lost.
  uint32_t max_retransmits = 25;
  // Silence from the peer, while linked, before a Ping goes out.
  Millis ping_interval{250};
  Millis hello_interval{50};
};

enum class LinkState : uint8_t {
  Connecting,
  Linked,
  // Terminal: the peer sent a Hello with a new boot id. The owner is expected
  // to reboot; nothing is sent or delivered anymore.
  PeerRebooted,
  // Terminal: a frame went unacked through max_retransmits retransmits.
  PeerLost,
};

// Whether the link is down for good; the owner starts a new one.
[[nodiscard]] constexpr bool is_terminal(LinkState state) {
  return state == LinkState::PeerRebooted || state == LinkState::PeerLost;
}

struct LinkStats {
  uint32_t frames_tx = 0;
  uint32_t frames_rx = 0;
  uint32_t retransmits = 0;
  uint32_t duplicates = 0;
  uint32_t pings = 0;
  uint32_t cobs_errors = 0;
  uint32_t crc_errors = 0;
  uint32_t other_errors = 0;
  // The most of the send queue ever in use, in bytes.
  std::size_t max_queue = 0;
};

enum class SendError : uint8_t {
  // Not now: the queue frees as the peer acks.
  QueueFull,
  // Never: larger than the whole queue.
  TooLarge,
};

// The memory a Link works in. See StaticLink for a Link with its own.
struct LinkBuffers {
  // Frames as they arrive: its size bounds a frame's COBS-encoded size, and
  // it must be 8-aligned.
  std::span<std::byte> receive;
  // Frames waiting to be sent, encoded, and the one in flight.
  std::span<std::byte> queue;
};

namespace detail {

// Encoded frames in one buffer, oldest first, each as [size: u16][bytes].
class FrameQueue {
 public:
  explicit FrameQueue(std::span<std::byte> buffer) : buf_(buffer) {}

  [[nodiscard]] bool empty() const { return head_ == tail_; }
  [[nodiscard]] std::size_t used() const { return tail_ - head_; }

  // Where the next frame goes: all the room there is. Moves what's queued to
  // the start of the buffer, invalidating views of it.
  std::span<std::byte> space();
  // Queues the `size` bytes just written to space().
  void push(std::size_t size);

  [[nodiscard]] std::span<const std::byte> front() const;
  void pop();

 private:
  static constexpr std::size_t kSizePrefix = 2;

  std::span<std::byte> buf_;
  std::size_t head_ = 0;
  std::size_t tail_ = 0;
};

}  // namespace detail

class Link {
 public:
  // `boot_id` must be non-zero and should differ across boots (hardware RNG).
  Link(uint32_t boot_id, const LinkConfig& config, LinkBuffers buffers);
  Link(const Link&) = delete;
  Link& operator=(const Link&) = delete;

  [[nodiscard]] LinkState state() const { return state_; }
  // The peer's boot id, once linked.
  [[nodiscard]] std::optional<uint32_t> peer_boot_id() const;
  [[nodiscard]] const LinkStats& stats() const { return stats_; }

  // Queues a sequenced frame; frames queued before linking go out after.
  std::expected<void, SendError> send(const Header& header,
                                      std::span<const std::byte> payload);

  // Feeds bytes read from the peer at `now`. Each frame for the layer above
  // goes to `on_frame(const Frame&)`, exactly once and in order; its payload
  // is valid during the call.
  template <std::invocable<const Frame&> OnFrame>
  void receive(std::span<const std::byte> bytes, Millis now,
               OnFrame&& on_frame) {
    for (std::byte b : bytes) {
      if (const auto frame = push(b, now)) {
        on_frame(*frame);
      }
    }
  }

  // The next bytes to write to the peer, if any are due at `now`; valid until
  // the next call on the link. Call it until it returns nothing.
  std::optional<std::span<const std::byte>> poll_transmit(Millis now);

  // When poll_transmit next has something to send, unless something arrives
  // first. Nothing once the link is terminal.
  [[nodiscard]] std::optional<Millis> next_deadline() const;

 private:
  // Room for an Ack or a Hello, which aren't queued.
  static constexpr std::size_t kUnsequencedSize = max_wire_size(32);

  std::optional<Frame> push(std::byte b, Millis now);
  std::optional<Frame> handle(const Frame& frame, Millis now);
  void handle_hello(std::span<const std::byte> payload);
  std::span<const std::byte> unsequenced(const Header& header,
                                         std::span<const std::byte> payload);
  std::span<const std::byte> hello();
  std::span<const std::byte> transmit(std::span<const std::byte> wire);

  LinkConfig config_;
  uint32_t boot_id_;
  LinkState state_ = LinkState::Connecting;
  uint32_t peer_boot_id_ = 0;
  Deframer deframer_;
  LinkStats stats_{};
  // The latest time passed in.
  Millis now_{};

  // Transmit side. Frames get their seq when queued; the oldest is in flight
  // once sent, until acked.
  detail::FrameQueue queue_;
  uint16_t next_seq_ = 1;
  uint16_t oldest_seq_ = 1;
  bool in_flight_ = false;
  Millis sent_at_{};
  uint32_t retransmits_ = 0;
  std::optional<uint16_t> ack_due_;
  bool hello_reply_due_ = false;
  std::optional<Millis> last_hello_at_;
  std::array<std::byte, kUnsequencedSize> unsequenced_{};

  // Receive side.
  Millis last_heard_at_{};
  uint16_t expected_seq_ = 1;
};

namespace detail {

template <std::size_t ReceiveSize, std::size_t QueueSize>
struct LinkStorage {
  alignas(8) std::array<std::byte, ReceiveSize> receive_buffer{};
  std::array<std::byte, QueueSize> queue_buffer{};
};

}  // namespace detail

// A Link with its own buffers, for payloads of up to `MaxPayload` bytes. Its
// queue holds two frames that large, or many more small ones.
template <std::size_t MaxPayload>
class StaticLink
    : private detail::LinkStorage<max_wire_size(MaxPayload) - 1,
                                  2 * (max_wire_size(MaxPayload) + 2)>,
      public Link {
 public:
  StaticLink(uint32_t boot_id, const LinkConfig& config = {})
      : Link(boot_id, config,
             {.receive = this->receive_buffer, .queue = this->queue_buffer}) {}
};

}  // namespace tether
