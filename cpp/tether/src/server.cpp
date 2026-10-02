#include "tether/server.h"

#include <algorithm>

namespace tether {
namespace {

using State = CallSlot::State;

}  // namespace

std::expected<void, CallError> Reply::send(
    std::span<const std::byte> response) const {
  return server_->respond(call_, WireStatus::OK, response);
}

std::expected<void, CallError> Reply::fail(WireStatus status) const {
  if (status == WireStatus::OK) {
    status = WireStatus::INTERNAL;
  }
  return server_->respond(call_, status, {});
}

std::expected<void, CallError> Sink::send(
    std::span<const std::byte> item) const {
  return server_->send_item(call_, item);
}

uint16_t Sink::credit() const { return server_->credit(call_); }

std::expected<void, CallError> Sink::end(WireStatus status) const {
  return server_->end_stream(call_, status);
}

Server::Server(uint32_t boot_id, const LinkConfig& config, LinkBuffers buffers,
               std::span<CallSlot> slots, std::size_t max_streams)
    : link_(boot_id, config, buffers),
      slots_(slots),
      max_streams_(max_streams) {}

std::size_t Server::open_calls() const {
  return static_cast<std::size_t>(std::ranges::count_if(
      slots_, [](const CallSlot& s) { return s.state != State::Free; }));
}

void Server::receive(std::span<const std::byte> bytes, Millis now) {
  link_.receive(bytes, now, [this](const Frame& frame) { dispatch(frame); });
  check_link();
}

std::optional<std::span<const std::byte>> Server::poll_transmit(Millis now) {
  const auto wire = link_.poll_transmit(now);
  check_link();
  return wire;
}

CallSlot* Server::find(uint32_t call) {
  const auto it = std::ranges::find_if(slots_, [call](const CallSlot& s) {
    return s.state != State::Free && s.call_id == call;
  });
  return it == slots_.end() ? nullptr : &*it;
}

CallSlot* Server::find(uint32_t call, State state) {
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
  if (slot == nullptr || (streaming && streams() >= max_streams_)) {
    reject(header.call_id, streaming, WireStatus::RESOURCE_EXHAUSTED);
    return;
  }
  const MethodId method{header.service, header.method};
  *slot = {.state = streaming ? State::Stream : State::Unary,
           .call_id = header.call_id,
           .method = method,
           .credit = streaming ? header.credit : uint16_t{0}};
  if (streaming) {
    dispatcher_->open(method, payload, Sink(*this, header.call_id));
  } else {
    dispatcher_->call(method, payload, Reply(*this, header.call_id));
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
    if (CallSlot* slot = find(grant->call_id(), State::Stream)) {
      slot->credit = static_cast<uint16_t>(
          std::min<uint32_t>(slot->credit + grant->credit(), UINT16_MAX));
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
  if (!queue(header, {})) {
    ++stats_.lost_rejections;
  }
}

void Server::check_link() {
  if (!is_terminal(link_.state())) {
    return;
  }
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

std::expected<void, CallError> Server::queue(
    const Header& header, std::span<const std::byte> payload) {
  const auto sent = link_.send(header, payload);
  if (sent) {
    return {};
  }
  return std::unexpected(sent.error() == SendError::QueueFull
                             ? CallError::QueueFull
                             : CallError::TooLarge);
}

std::expected<void, CallError> Server::respond(
    uint32_t call, WireStatus status, std::span<const std::byte> payload) {
  CallSlot* slot = find(call, State::Unary);
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

std::expected<void, CallError> Server::send_item(
    uint32_t call, std::span<const std::byte> item) {
  CallSlot* slot = find(call, State::Stream);
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

std::expected<void, CallError> Server::end_stream(uint32_t call,
                                                  WireStatus status) {
  CallSlot* slot = find(call, State::Stream);
  if (slot == nullptr) {
    return std::unexpected(CallError::Closed);
  }
  const auto sent =
      queue({.kind = Kind::End, .call_id = call, .status = status}, {});
  if (sent) {
    *slot = {};
  }
  return sent;
}

uint16_t Server::credit(uint32_t call) {
  const CallSlot* slot = find(call, State::Stream);
  return slot == nullptr ? uint16_t{0} : slot->credit;
}

}  // namespace tether
