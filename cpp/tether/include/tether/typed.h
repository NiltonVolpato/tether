// What generated code is built on: typed replies and sinks over the raw ones,
// and reading requests in place. The same layer as the Rust `tether` crate's
// `Reply` and `Sink`, over flatc's C++ tables.
//
// Without a heap, messages are built with a flatbuffers builder where they'll
// wait to be sent, instead of from owned objects (flatc's object API
// allocates): a handler passes a function that builds the message with the
// builder and returns its root.
//
//   reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
//     return CreateWifiStatusDirect(fbb, true, ip.c_str());
//   });
//
// The function runs only once the call is known to be open (and a channel to
// have credit) and there's room for the largest payload in the send queue;
// otherwise the send fails without calling it. It runs holding the server's
// lock, if it has one: keep it short, and send nothing from it.
//
// The builder needs no heap, and must stay within its memory: the largest
// payload (for set_latest, the channel's latest-value buffer). So no shared
// strings, and no flatbuffers objects of its own on the way. A message too
// large aborts.

#pragma once

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <expected>
#include <span>
#include <type_traits>

#include "flatbuffers/flatbuffers.h"
#include "tether/server.h"

namespace tether {

// Verifies `buffer` as a `T` and returns its root, which stays in `buffer`;
// null if it isn't one. The buffer must be 8-aligned, as requests are.
template <typename T>
[[nodiscard]] const T* verify(std::span<const std::byte> buffer) {
  const auto* data = reinterpret_cast<const uint8_t*>(buffer.data());
  flatbuffers::Verifier verifier(data, buffer.size());
  return verifier.VerifyBuffer<T>(nullptr) ? flatbuffers::GetRoot<T>(data)
                                           : nullptr;
}

namespace detail {

// Hands a FlatBufferBuilder one fixed memory instead of the heap.
class SpanAllocator final : public flatbuffers::Allocator {
 public:
  explicit SpanAllocator(std::span<std::byte> memory) : memory_(memory) {
    // The builder needs what it's given aligned for any scalar.
    if (reinterpret_cast<uintptr_t>(memory.data()) % 8 != 0) {
      std::abort();
    }
  }

  uint8_t* allocate(std::size_t size) override {
    if (size > memory_.size()) {
      std::abort();  // A message larger than its memory: see the top.
    }
    return reinterpret_cast<uint8_t*>(memory_.data());
  }

  void deallocate(uint8_t* /*p*/, std::size_t /*size*/) override {}

 private:
  std::span<std::byte> memory_;
};

// A MessageBuilder's function that builds a `T` with `build`, at the end of
// the memory it's given.
template <typename T, typename Build>
auto builder(Build& build) {
  return [&build](std::span<std::byte> memory) -> std::span<const std::byte> {
    SpanAllocator allocator(memory);
    flatbuffers::FlatBufferBuilder fbb(memory.size(), &allocator);
    fbb.Finish(build(fbb));
    return std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  };
}

// A function that builds a `T`, so `Builds<T> Build` constrains a parameter.
template <typename Build, typename T>
concept Builds = std::is_invocable_r_v<flatbuffers::Offset<T>, Build,
                                       flatbuffers::FlatBufferBuilder&>;

}  // namespace detail

// Answers one unary call with a `T`. A cheap handle, like RawReply.
template <typename T>
class Reply {
 public:
  // Answers no call: always Closed (see RawReply's).
  Reply() = default;
  explicit Reply(RawReply raw) : raw_(raw) {}

  [[nodiscard]] CallId call_id() const { return raw_.call_id(); }

  // See RawReply::send.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> send(Build&& build) const {
    return raw_.send_built(detail::builder<T>(build));
  }

  // See RawReply::fail.
  [[nodiscard]] std::expected<void, CallError> fail(WireStatus status) const {
    return raw_.fail(status);
  }

 private:
  RawReply raw_;
};

// The server's end of a channel of `T`s. Handlers keep it for as long as the
// channel lasts.
template <typename T>
class Sink {
 public:
  // The end of no channel: always Closed (see RawSink's).
  Sink() = default;
  explicit Sink(RawSink raw) : raw_(raw) {}

  [[nodiscard]] CallId call_id() const { return raw_.call_id(); }

  // See RawSink::send.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> send(Build&& build) const {
    return raw_.send_built(detail::builder<T>(build));
  }

  // See RawSink::set_latest_built: it's built in the channel's latest-value
  // buffer, and one larger than that aborts.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> set_latest(Build&& build) const {
    return raw_.set_latest_built(detail::builder<T>(build));
  }

  [[nodiscard]] uint16_t credit() const { return raw_.credit(); }

  // See RawSink::end.
  [[nodiscard]] std::expected<void, CallError> end(
      WireStatus status = WireStatus::OK) const {
    return raw_.end(status);
  }

 private:
  RawSink raw_;
};

}  // namespace tether
