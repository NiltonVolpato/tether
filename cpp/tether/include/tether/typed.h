// What generated code is built on: typed replies and sinks over the raw ones,
// and reading requests in place. The same layer as the Rust `tether` crate's
// `Reply` and `Sink`, over flatc's C++ tables.
//
// Without a heap, messages are built with a flatbuffers builder in the
// server's scratch memory instead of from owned objects (flatc's object API
// allocates): a handler passes a function that builds the message with the
// builder and returns its root.
//
//   reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
//     return CreateWifiStatusDirect(fbb, true, ip.c_str());
//   });
//
// The builder must stay within the scratch memory, which is as large as a
// payload can be, and needs no heap: no shared strings, and no flatbuffers
// objects of its own on the way. A message too large for it aborts.

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
      std::abort();  // A message larger than any payload.
    }
    return reinterpret_cast<uint8_t*>(memory_.data());
  }

  void deallocate(uint8_t* /*p*/, std::size_t /*size*/) override {}

 private:
  std::span<std::byte> memory_;
};

// Builds a `T` with `build` in `scratch`, and returns it: valid until the
// scratch is used again.
template <typename T, typename Build>
std::span<const std::byte> build_message(std::span<std::byte> scratch,
                                         Build&& build) {
  SpanAllocator allocator(scratch);
  flatbuffers::FlatBufferBuilder fbb(scratch.size(), &allocator);
  fbb.Finish(build(fbb));
  return std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
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
  explicit Reply(RawReply raw) : raw_(raw) {}

  [[nodiscard]] CallId call_id() const { return raw_.call_id(); }

  // See RawReply::send.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> send(Build&& build) const {
    return raw_.send(
        detail::build_message<T>(raw_.scratch(), std::forward<Build>(build)));
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
  explicit Sink(RawSink raw) : raw_(raw) {}

  [[nodiscard]] CallId call_id() const { return raw_.call_id(); }

  // See RawSink::send.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> send(Build&& build) const {
    return raw_.send(
        detail::build_message<T>(raw_.scratch(), std::forward<Build>(build)));
  }

  // See RawSink::set_latest.
  template <detail::Builds<T> Build>
  [[nodiscard]] std::expected<void, CallError> set_latest(Build&& build) const {
    return raw_.set_latest(
        detail::build_message<T>(raw_.scratch(), std::forward<Build>(build)));
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
