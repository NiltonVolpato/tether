#include "tether/link.h"

#include <algorithm>
#include <cstdlib>
#include <cstring>

#include "arena.h"

namespace tether {
namespace {

// Sequence numbers run 1..=65535; 0 marks unsequenced frames.
uint16_t next(uint16_t seq) {
  return seq == UINT16_MAX ? 1 : static_cast<uint16_t>(seq + 1);
}

uint16_t prev(uint16_t seq) {
  return seq == 1 ? UINT16_MAX : static_cast<uint16_t>(seq - 1);
}

}  // namespace

namespace detail {

std::span<std::byte> FrameQueue::space() {
  if (empty()) {
    head_ = tail_ = 0;
  } else if (head_ > 0) {
    std::memmove(buf_.data(), buf_.data() + head_, tail_ - head_);
    tail_ -= head_;
    head_ = 0;
  }
  if (buf_.size() < tail_ + kSizePrefix) {
    return {};
  }
  return buf_.subspan(tail_ + kSizePrefix);
}

void FrameQueue::push(std::size_t size) {
  buf_[tail_] = std::byte(size);
  buf_[tail_ + 1] = std::byte(size >> 8);
  tail_ += kSizePrefix + size;
}

std::span<const std::byte> FrameQueue::front() const {
  const std::size_t size = std::to_integer<std::size_t>(buf_[head_]) |
                           std::to_integer<std::size_t>(buf_[head_ + 1]) << 8;
  return buf_.subspan(head_ + kSizePrefix, size);
}

void FrameQueue::pop() { head_ += kSizePrefix + front().size(); }

}  // namespace detail

Link::Link(uint32_t boot_id, const LinkConfig& config, LinkBuffers buffers)
    : config_(config),
      boot_id_(boot_id),
      deframer_(buffers.receive),
      queue_(buffers.queue) {
  if (boot_id == 0) {
    std::abort();  // 0 means "none yet" in a Hello.
  }
}

std::optional<uint32_t> Link::peer_boot_id() const {
  if (state_ == LinkState::Connecting) {
    return std::nullopt;
  }
  return peer_boot_id_;
}

std::expected<void, SendError> Link::send(const Header& header,
                                          std::span<const std::byte> payload) {
  const bool was_empty = queue_.empty();
  Header sequenced = header;
  sequenced.seq = next_seq_;
  const auto size = encode(sequenced, payload, queue_.space());
  if (!size) {
    return std::unexpected(was_empty ? SendError::TooLarge
                                     : SendError::QueueFull);
  }
  queue_.push(*size);
  next_seq_ = next(next_seq_);
  stats_.max_queue = std::max(stats_.max_queue, queue_.used());
  return {};
}

std::optional<std::span<const std::byte>> Link::poll_transmit(Millis now) {
  now_ = now;
  if (is_terminal(state_)) {
    return std::nullopt;
  }
  if (ack_due_) {
    const Header ack{.kind = Kind::Ack, .seq = *std::exchange(ack_due_, {})};
    return transmit(unsequenced(ack, {}));
  }
  const bool hello_due =
      state_ == LinkState::Connecting &&
      (!last_hello_at_ || now >= *last_hello_at_ + config_.hello_interval);
  if (std::exchange(hello_reply_due_, false) || hello_due) {
    last_hello_at_ = now;
    return transmit(hello());
  }
  if (state_ != LinkState::Linked) {
    return std::nullopt;
  }
  if (in_flight_) {
    if (now < sent_at_ + config_.retransmit) {
      return std::nullopt;
    }
    if (retransmits_ == config_.max_retransmits) {
      state_ = LinkState::PeerLost;
      return std::nullopt;
    }
    sent_at_ = now;
    ++retransmits_;
    ++stats_.retransmits;
    return transmit(queue_.front());
  }
  if (queue_.empty() && now >= last_heard_at_ + config_.ping_interval) {
    ++stats_.pings;
    // Can't fail: the queue is empty, and far larger than a Ping.
    (void)send({.kind = Kind::Ping}, {});
  }
  if (queue_.empty()) {
    return std::nullopt;
  }
  in_flight_ = true;
  sent_at_ = now;
  retransmits_ = 0;
  return transmit(queue_.front());
}

std::optional<Millis> Link::next_deadline() const {
  if (is_terminal(state_)) {
    return std::nullopt;
  }
  if (ack_due_ || hello_reply_due_) {
    return now_;
  }
  if (state_ == LinkState::Connecting) {
    return last_hello_at_ ? *last_hello_at_ + config_.hello_interval : now_;
  }
  if (in_flight_) {
    return sent_at_ + config_.retransmit;
  }
  if (!queue_.empty()) {
    return now_;
  }
  return last_heard_at_ + config_.ping_interval;
}

std::optional<Frame> Link::push(std::byte b, Millis now) {
  const auto result = deframer_.push(b);
  if (!result) {
    return std::nullopt;
  }
  if (!*result) {
    switch (result->error()) {
      case FrameError::Cobs:
        ++stats_.cobs_errors;
        break;
      case FrameError::Crc:
        ++stats_.crc_errors;
        break;
      default:
        ++stats_.other_errors;
        break;
    }
    return std::nullopt;
  }
  return handle(**result, now);
}

std::optional<Frame> Link::handle(const Frame& frame, Millis now) {
  ++stats_.frames_rx;
  if (is_terminal(state_)) {
    return std::nullopt;
  }
  now_ = std::max(now_, now);
  last_heard_at_ = now;
  if (frame.header.kind == Kind::Hello) {
    handle_hello(frame.payload);
    return std::nullopt;
  }
  if (state_ != LinkState::Linked) {
    // Sequenced traffic before our Hello got through.
    return std::nullopt;
  }
  const uint16_t seq = frame.header.seq;
  if (frame.header.kind == Kind::Ack) {
    if (in_flight_ && seq == oldest_seq_) {
      in_flight_ = false;
      queue_.pop();
      oldest_seq_ = next(oldest_seq_);
    }
    return std::nullopt;
  }
  if (seq == 0) {
    return std::nullopt;
  }
  if (seq == expected_seq_) {
    expected_seq_ = next(seq);
    ack_due_ = seq;
    if (frame.header.kind == Kind::Ping) {
      return std::nullopt;
    }
    return frame;
  }
  if (seq == prev(expected_seq_)) {
    // Our ack was lost; ack again.
    ++stats_.duplicates;
    ack_due_ = seq;
  }
  return std::nullopt;
}

void Link::handle_hello(std::span<const std::byte> payload) {
  const auto* data = reinterpret_cast<const uint8_t*>(payload.data());
  flatbuffers::Verifier verifier(data, payload.size());
  if (!verifier.VerifyBuffer<wire::Hello>()) {
    ++stats_.other_errors;
    return;
  }
  const auto* hello = flatbuffers::GetRoot<wire::Hello>(data);
  const uint32_t boot_id = hello->boot_id();
  const uint32_t seen = hello->peer_boot_id();
  switch (state_) {
    case LinkState::Connecting:
      // A stale reply addressed to our previous boot: linking to it would pair
      // us with a peer that is about to see our Hello and reboot.
      if (seen != 0 && seen != boot_id_) {
        return;
      }
      state_ = LinkState::Linked;
      peer_boot_id_ = boot_id;
      break;
    case LinkState::Linked:
      if (boot_id == peer_boot_id_) {
        break;
      }
      // Don't answer: the peer would link to us, then see our own new Hello
      // after we reboot and reboot again.
      state_ = LinkState::PeerRebooted;
      return;
    case LinkState::PeerRebooted:
    case LinkState::PeerLost:
      return;
  }
  // A retransmitted Hello (our reply was lost) has the same boot_id and is
  // answered again instead of being mistaken for a reboot.
  if (seen != boot_id_) {
    hello_reply_due_ = true;
  }
}

std::span<const std::byte> Link::unsequenced(
    const Header& header, std::span<const std::byte> payload) {
  const auto size = encode(header, payload, unsequenced_);
  if (!size) {
    std::abort();  // kUnsequencedSize is too small.
  }
  return std::span(unsequenced_).first(*size);
}

std::span<const std::byte> Link::hello() {
  detail::ArenaAllocator<64> arena;
  flatbuffers::FlatBufferBuilder fbb(decltype(arena)::kSize, &arena);
  const uint32_t peer = state_ == LinkState::Linked ? peer_boot_id_ : 0;
  fbb.Finish(wire::CreateHello(fbb, boot_id_, peer));
  return unsequenced(
      {.kind = Kind::Hello},
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize())));
}

std::span<const std::byte> Link::transmit(std::span<const std::byte> wire) {
  ++stats_.frames_tx;
  return wire;
}

}  // namespace tether
