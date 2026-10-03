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
//
// Frames wait in the send queue as bodies (see write_body), and are
// COBS-encoded a piece at a time as they go out, so a retransmit costs no
// memory. A payload can be built in the queue itself (reserve and commit),
// without a copy elsewhere first.

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
  // The wait for an ack beyond the time frames take on the wire: the peer's
  // latency to answer. See Link::retransmit_timeout.
  Millis retransmit{20};
  // The line's rate, for the time frames take on the wire (8N1: 10 bits a
  // byte).
  uint32_t baud_rate = 921600;
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
  // Frames waiting to be sent, and the one in flight: each takes its body's
  // size and 2 bytes. A payload built in it (see Link::reserve) needs room for
  // the largest body, contiguous.
  std::span<std::byte> queue;
};

namespace detail {

// Frame bodies in one buffer, oldest first, each as [size: u16][body].
class FrameQueue {
 public:
  // Uses the buffer up to its last 8-aligned address, so that what's built at
  // the end of space() is aligned.
  explicit FrameQueue(std::span<std::byte> buffer);

  [[nodiscard]] bool empty() const { return head_ == tail_; }
  [[nodiscard]] std::size_t used() const { return tail_ - head_; }
  [[nodiscard]] std::size_t capacity() const { return buf_.size(); }

  // Where the next body goes: all the room there is, ending 8-aligned. Moves
  // what's queued to the start of the buffer, invalidating views of it.
  std::span<std::byte> space();
  // Queues the `size` bytes just written to space().
  void push(std::size_t size);

  [[nodiscard]] std::span<const std::byte> front() const;
  void pop();

  static constexpr std::size_t kSizePrefix = 2;

 private:
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
  [[nodiscard]] uint32_t boot_id() const { return boot_id_; }
  // The peer's boot id, once linked.
  [[nodiscard]] std::optional<uint32_t> peer_boot_id() const;
  [[nodiscard]] const LinkStats& stats() const { return stats_; }
  [[nodiscard]] const LinkConfig& config() const { return config_; }

  // Queues a sequenced frame; frames queued before linking go out after.
  std::expected<void, SendError> send(const Header& header,
                                      std::span<const std::byte> payload);

  // Memory in the send queue to build a payload of up to `size` bytes in, a
  // multiple of 8; it ends 8-aligned, as a FlatBufferBuilder needs. Queue
  // what's built with commit(), which must come before anything else is sent.
  std::expected<std::span<std::byte>, SendError> reserve(std::size_t size);
  // Queues a sequenced frame whose payload was built in the memory reserve()
  // returned, and is somewhere in it.
  void commit(const Header& header, std::span<const std::byte> payload);

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

  // The next bytes to write to the peer, if any are due at `now`: a frame, or
  // a piece of one. Valid until the next call on the link. Call it until it
  // returns nothing.
  std::optional<std::span<const std::byte>> poll_transmit(Millis now);

  // When poll_transmit next has something to send, unless something arrives
  // first. Nothing once the link is terminal.
  [[nodiscard]] std::optional<Millis> next_deadline() const;

  // How long to wait for the ack of a frame of `wire_size` bytes: the base,
  // plus the frame's time on the wire and the time of the largest frame the
  // peer may have started sending just before, ahead of its ack. That largest
  // frame is the largest this end accepts.
  [[nodiscard]] Millis retransmit_timeout(std::size_t wire_size) const;

 private:
  // Room for the body of an Ack or a Hello, which aren't queued.
  static constexpr std::size_t kUnsequencedSize = max_body_size(32);
  // The most poll_transmit returns at once.
  static constexpr std::size_t kPieceSize = 128;

  // What's going out, a piece at a time.
  enum class Sending : uint8_t { Nothing, Unsequenced, Queued };

  std::optional<Frame> push(std::byte b, Millis now);
  std::optional<Frame> handle(const Frame& frame, Millis now);
  void handle_hello(std::span<const std::byte> payload);
  void unsequenced(const Header& header, std::span<const std::byte> payload);
  void hello();
  // Starts sending a frame, and returns its first piece.
  std::span<const std::byte> transmit(Sending what);
  // The next piece of what's being sent; nothing once it's all out.
  std::optional<std::span<const std::byte>> next_piece();

  LinkConfig config_;
  uint32_t boot_id_;
  LinkState state_ = LinkState::Connecting;
  uint32_t peer_boot_id_ = 0;
  Deframer deframer_;
  std::size_t max_frame_;
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
  Millis timeout_{};
  uint32_t retransmits_ = 0;
  std::optional<uint16_t> ack_due_;
  bool hello_reply_due_ = false;
  std::optional<Millis> last_hello_at_;
  std::array<std::byte, kUnsequencedSize> unsequenced_{};
  std::size_t unsequenced_size_ = 0;
  // A payload is being built in the queue.
  bool reserved_ = false;

  Sending sending_ = Sending::Nothing;
  CobsStream cobs_;
  // The queue's front was acked while it was going out: it's popped once out.
  bool pop_when_sent_ = false;
  std::array<std::byte, kPieceSize> piece_{};

  // Receive side.
  Millis last_heard_at_{};
  uint16_t expected_seq_ = 1;
};

namespace detail {

template <std::size_t ReceiveSize, std::size_t QueueSize>
struct LinkStorage {
  alignas(8) std::array<std::byte, ReceiveSize> receive_buffer{};
  alignas(8) std::array<std::byte, (QueueSize + 7) / 8 * 8> queue_buffer{};
};

// Buffers for payloads of up to `MaxPayload` bytes. The queue holds two frames
// that large, or many more small ones.
template <std::size_t MaxPayload>
using LinkStorageFor =
    LinkStorage<max_wire_size(MaxPayload) - 1,
                2 * (FrameQueue::kSizePrefix + max_body_size(MaxPayload))>;

}  // namespace detail

// A Link with its own buffers, for payloads of up to `MaxPayload` bytes.
template <std::size_t MaxPayload>
class StaticLink : private detail::LinkStorageFor<MaxPayload>, public Link {
 public:
  StaticLink(uint32_t boot_id, const LinkConfig& config = {})
      : Link(boot_id, config,
             {.receive = this->receive_buffer, .queue = this->queue_buffer}) {}
};

}  // namespace tether
