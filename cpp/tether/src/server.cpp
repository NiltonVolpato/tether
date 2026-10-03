#include "tether/server.h"

#include <algorithm>
#include <cstdlib>
#include <memory>

namespace tether {
namespace {

using State = CallSlot::State;

// What a default-made handle answers.
constexpr std::unexpected kClosed(CallError::Closed);

}  // namespace

std::expected<void, CallError> RawReply::send(
    std::span<const std::byte> response) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->respond(epoch_, call_, WireStatus::OK, response);
}

std::expected<void, CallError> RawReply::send_built(
    MessageBuilder build) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->respond(epoch_, call_, WireStatus::OK, build);
}

std::expected<void, CallError> RawReply::fail(WireStatus status) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  if (status == WireStatus::OK) {
    status = WireStatus::INTERNAL;
  }
  return server_->respond(epoch_, call_, status, std::span<const std::byte>{});
}

std::expected<void, CallError> RawSink::send(
    std::span<const std::byte> item) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->send_item(epoch_, call_, item);
}

std::expected<void, CallError> RawSink::send_built(MessageBuilder build) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->send_item(epoch_, call_, build);
}

std::expected<void, CallError> RawSink::set_latest(
    std::span<const std::byte> item) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->set_latest(epoch_, call_, item);
}

std::expected<void, CallError> RawSink::set_latest_built(
    MessageBuilder build) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->set_latest_built(epoch_, call_, build);
}

uint16_t RawSink::credit() const {
  return server_ == nullptr ? uint16_t{0} : server_->credit(epoch_, call_);
}

std::expected<void, CallError> RawSink::end(WireStatus status) const {
  if (server_ == nullptr) {
    return kClosed;
  }
  return server_->end_stream(epoch_, call_, status);
}

Server::Server(uint32_t boot_id, const LinkConfig& config,
               ServerBuffers buffers, ServerLimits limits)
    : link_buffers_(buffers.link),
      link_(boot_id, config, buffers.link),
      slots_(buffers.slots),
      latest_(buffers.latest),
      limits_(limits) {
  if (limits.max_payload % 8 != 0 ||
      reinterpret_cast<std::uintptr_t>(latest_.data()) % 8 != 0) {
    std::abort();
  }
}

std::size_t Server::open_calls() const {
  const Guard guard(*this);
  return static_cast<std::size_t>(std::ranges::count_if(
      slots_, [](const CallSlot& s) { return s.state != State::Free; }));
}

void Server::receive(std::span<const std::byte> bytes, Millis now) {
  const Guard guard(*this);
  link_.receive(bytes, now, [this](const Frame& frame) { dispatch(frame); });
  // The acks in `bytes` may have made room in the send queue.
  for (CallSlot& slot : slots_) {
    flush_latest(slot);
  }
  check_link();
}

std::optional<std::span<const std::byte>> Server::poll_transmit(Millis now) {
  const Guard guard(*this);
  const auto wire = link_.poll_transmit(now);
  check_link();
  return wire;
}

std::optional<Millis> Server::next_deadline() const {
  const Guard guard(*this);
  return link_.next_deadline();
}

void Server::restart(uint32_t boot_id) {
  const Guard guard(*this);
  if (building_) {
    std::abort();  // From inside a build.
  }
  cancel_all();
  ++epoch_;
  // A new link in the same memory; the old one's frames are dropped.
  const LinkConfig config = link_.config();
  std::destroy_at(&link_);
  std::construct_at(&link_, boot_id, config, link_buffers_);
}

CallSlot* Server::find(uint32_t call) {
  const auto it = std::ranges::find_if(slots_, [call](const CallSlot& s) {
    return s.state != State::Free && s.call_id == call;
  });
  return it == slots_.end() ? nullptr : &*it;
}

CallSlot* Server::find(uint32_t epoch, uint32_t call, State state) {
  if (epoch != epoch_) {
    return nullptr;
  }
  CallSlot* slot = find(call);
  return slot != nullptr && slot->state == state ? slot : nullptr;
}

std::size_t Server::streams() const {
  return static_cast<std::size_t>(std::ranges::count_if(
      slots_, [](const CallSlot& s) { return s.state == State::Stream; }));
}

void Server::dispatch(const Frame& frame) {
  const Header& header = frame.header;
  switch (header.kind) {
    case Kind::Request:
      begin(header, frame.payload, false);
      break;
    case Kind::Open:
      begin(header, frame.payload, true);
      break;
    case Kind::Credit:
      grant(frame.payload);
      break;
    case Kind::Cancel:
      cancel(header.call_id);
      break;
    default:
      break;
  }
}

void Server::begin(const Header& header, std::span<const std::byte> payload,
                   bool streaming) {
  if (find(header.call_id) != nullptr) {
    return;  // A client reusing the id of a call that's still open.
  }
  if (dispatcher_ == nullptr) {
    reject(header.call_id, streaming, WireStatus::UNIMPLEMENTED);
    return;
  }
  const auto free = std::ranges::find(slots_, State::Free, &CallSlot::state);
  CallSlot* slot = free == slots_.end() ? nullptr : &*free;
  if (slot == nullptr || (streaming && streams() >= limits_.max_streams)) {
    reject(header.call_id, streaming, WireStatus::RESOURCE_EXHAUSTED);
    return;
  }
  const MethodId method{header.service, header.method};
  *slot = {.state = streaming ? State::Stream : State::Unary,
           .call_id = header.call_id,
           .method = method,
           .credit = streaming ? header.credit : uint16_t{0}};
  if (streaming) {
    dispatcher_->open(method, payload, RawSink(*this, epoch_, header.call_id));
  } else {
    dispatcher_->call(method, payload, RawReply(*this, epoch_, header.call_id));
  }
}

void Server::grant(std::span<const std::byte> payload) {
  const auto* data = reinterpret_cast<const uint8_t*>(payload.data());
  flatbuffers::Verifier verifier(data, payload.size());
  if (!verifier.VerifyBuffer<wire::Credits>()) {
    return;
  }
  const auto* grants = flatbuffers::GetRoot<wire::Credits>(data)->grants();
  if (grants == nullptr) {
    return;
  }
  for (const wire::Grant* grant : *grants) {
    // A channel that ended or was cancelled meanwhile.
    if (CallSlot* slot = find(epoch_, grant->call_id(), State::Stream)) {
      slot->credit = static_cast<uint16_t>(
          std::min<uint32_t>(slot->credit + grant->credit(), UINT16_MAX));
      flush_latest(*slot);
    }
  }
}

void Server::cancel(uint32_t call) {
  CallSlot* slot = find(call);
  if (slot == nullptr) {
    return;
  }
  const MethodId method = slot->method;
  *slot = {};
  if (dispatcher_ != nullptr) {
    dispatcher_->cancelled(CallId{call}, method);
  }
}

void Server::reject(uint32_t call, bool streaming, WireStatus status) {
  const Header header{.kind = streaming ? Kind::End : Kind::Response,
                      .call_id = call,
                      .status = status};
  if (!queue(header, std::span<const std::byte>{})) {
    ++stats_.lost_rejections;
  }
}

void Server::check_link() {
  if (is_terminal(link_.state())) {
    cancel_all();
  }
}

void Server::cancel_all() {
  for (CallSlot& slot : slots_) {
    if (slot.state == State::Free) {
      continue;
    }
    // Freed first, so the dispatcher's replies and sinks for it are closed.
    const CallId call{slot.call_id};
    const MethodId method = slot.method;
    slot = {};
    if (dispatcher_ != nullptr) {
      dispatcher_->cancelled(call, method);
    }
  }
}

namespace {

CallError call_error(SendError error) {
  return error == SendError::QueueFull ? CallError::QueueFull
                                       : CallError::TooLarge;
}

}  // namespace

std::expected<void, CallError> Server::queue(
    const Header& header, std::span<const std::byte> payload) {
  if (building_) {
    std::abort();  // A send from inside a build.
  }
  const auto sent = link_.send(header, payload);
  if (!sent) {
    return std::unexpected(call_error(sent.error()));
  }
  if (hooks_ != nullptr) {
    hooks_->wake();
  }
  return {};
}

std::expected<void, CallError> Server::queue(const Header& header,
                                             MessageBuilder build) {
  if (building_) {
    std::abort();  // A send from inside a build.
  }
  const auto memory = link_.reserve(limits_.max_payload);
  if (!memory) {
    return std::unexpected(call_error(memory.error()));
  }
  link_.commit(header, run_build(build, *memory));
  if (hooks_ != nullptr) {
    hooks_->wake();
  }
  return {};
}

std::span<const std::byte> Server::run_build(MessageBuilder build,
                                             std::span<std::byte> memory) {
  if (building_) {
    std::abort();  // A build from inside a build.
  }
  building_ = true;
  const auto built = build(memory);
  building_ = false;
  if (built.data() < memory.data() ||
      built.data() + built.size() > memory.data() + memory.size()) {
    std::abort();  // Not built in the memory given.
  }
  return built;
}

template <typename Payload>
std::expected<void, CallError> Server::respond(uint32_t epoch, uint32_t call,
                                               WireStatus status,
                                               Payload payload) {
  const Guard guard(*this);
  CallSlot* slot = find(epoch, call, State::Unary);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  const auto sent = queue(
      {.kind = Kind::Response, .call_id = call, .status = status}, payload);
  if (sent) {
    *slot = {};
  }
  return sent;
}

template <typename Payload>
std::expected<void, CallError> Server::send_item(uint32_t epoch, uint32_t call,
                                                 Payload item) {
  const Guard guard(*this);
  CallSlot* slot = find(epoch, call, State::Stream);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  if (slot->credit == 0) {
    return std::unexpected(CallError::NoCredit);
  }
  const auto sent = queue({.kind = Kind::Item, .call_id = call}, item);
  if (sent) {
    --slot->credit;
  }
  return sent;
}

std::expected<void, CallError> Server::set_latest(
    uint32_t epoch, uint32_t call, std::span<const std::byte> item) {
  const Guard guard(*this);
  CallSlot* slot = find(epoch, call, State::Stream);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  const std::span<std::byte> buffer = latest_buffer(*slot);
  if (slot->credit > 0) {
    const auto sent = queue({.kind = Kind::Item, .call_id = call}, item);
    if (sent) {
      --slot->credit;
      slot->has_latest = false;  // Older than what just went out.
      return {};
    }
    if (sent.error() == CallError::TooLarge) {
      return sent;
    }
    // The queue is full: it goes out when there's room, if it can wait.
    if (item.size() > buffer.size()) {
      return sent;
    }
  } else if (item.size() > buffer.size()) {
    return std::unexpected(CallError::TooLarge);
  }
  std::ranges::copy(item, buffer.begin());
  slot->latest_size = static_cast<uint16_t>(item.size());
  slot->has_latest = true;
  return {};
}

std::expected<void, CallError> Server::set_latest_built(uint32_t epoch,
                                                        uint32_t call,
                                                        MessageBuilder build) {
  const Guard guard(*this);
  CallSlot* slot = find(epoch, call, State::Stream);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  // It replaces what's waiting, which it's built over.
  slot->has_latest = false;
  const std::span<std::byte> buffer = latest_buffer(*slot);
  const auto value = run_build(build, buffer);
  // Where it waits starts the buffer.
  std::ranges::copy(value, buffer.begin());
  slot->latest_size = static_cast<uint16_t>(value.size());
  slot->has_latest = true;
  flush_latest(*slot);
  return {};
}

std::expected<void, CallError> Server::end_stream(uint32_t epoch, uint32_t call,
                                                  WireStatus status) {
  const Guard guard(*this);
  CallSlot* slot = find(epoch, call, State::Stream);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  const auto sent =
      queue({.kind = Kind::End, .call_id = call, .status = status},
            std::span<const std::byte>{});
  if (sent) {
    *slot = {};
  }
  return sent;
}

void Server::flush_latest(CallSlot& slot) {
  if (slot.state != State::Stream || !slot.has_latest || slot.credit == 0) {
    return;
  }
  const auto value = latest_buffer(slot).first(slot.latest_size);
  if (queue({.kind = Kind::Item, .call_id = slot.call_id}, value)) {
    --slot.credit;
    slot.has_latest = false;
  }
}

std::span<std::byte> Server::latest_buffer(const CallSlot& slot) const {
  if (slots_.empty()) {
    return {};
  }
  const std::size_t size =
      std::min<std::size_t>(latest_.size() / slots_.size(), UINT16_MAX) / 8 * 8;
  return latest_.subspan((&slot - slots_.data()) * size, size);
}

uint16_t Server::credit(uint32_t epoch, uint32_t call) {
  const Guard guard(*this);
  const CallSlot* slot = find(epoch, call, State::Stream);
  return slot == nullptr ? uint16_t{0} : slot->credit;
}

}  // namespace tether
