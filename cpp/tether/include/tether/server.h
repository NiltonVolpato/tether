// The server (co-processor) side: the same protocol as the Rust core's server
// (core/src/server.rs), over a Link.
//
// Calls are addressed by call id. A request or an open goes to the Dispatcher
// (see router.h) with a RawReply or a RawSink that answers it, and those can be
// kept for as long as the call lasts. The server tracks open calls in a fixed
// table, so a call that finds it full is rejected with RESOURCE_EXHAUSTED.
//
// Sans-IO, like the link: feed received bytes in, write out what
// poll_transmit returns, and call again at next_deadline. Everything here,
// replies and sinks included, must run on one thread.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <optional>
#include <span>

#include "tether/descriptor.h"
#include "tether/link.h"

namespace tether {

// Why a reply or an item wasn't sent.
enum class CallError : uint8_t {
  // The client cancelled, the call was already answered or ended, or the link
  // is down.
  Closed,
  // The client hasn't consumed earlier items; try again after it grants credit.
  NoCredit,
  // Not now: the send queue frees as the peer acks.
  QueueFull,
  // Never: larger than the whole send queue.
  TooLarge,
};

class Server;

// Answers one unary call, at most once. A cheap handle: copies answer the same
// call, and only the first answer is sent.
class RawReply {
 public:
  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // Memory to build a message in, for as long as nothing else on the server
  // uses it (see typed.h): the size of the largest payload.
  [[nodiscard]] std::span<std::byte> scratch() const;

  // `Closed` if the client cancelled meanwhile. `QueueFull` leaves the call
  // open, to answer again later.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> response) const;
  // Answers with an error. `status` must not be OK: that's answered as
  // INTERNAL, a handler bug.
  [[nodiscard]] std::expected<void, CallError> fail(WireStatus status) const;

 private:
  friend class Server;
  RawReply(Server& server, uint32_t call) : server_(&server), call_(call) {}

  Server* server_;
  uint32_t call_;
};

// The server's end of one channel. A cheap handle, like RawReply.
class RawSink {
 public:
  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // As RawReply::scratch.
  [[nodiscard]] std::span<std::byte> scratch() const;

  // Uses one credit; `NoCredit` until the client consumes earlier items.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> item) const;
  // Latest-value mode: sends now if there's credit and room in the send
  // queue; otherwise keeps `item` for when there is, replacing whatever value
  // was waiting, so the client sees the newest and skips the ones in between.
  // `TooLarge` if it must wait and doesn't fit the channel's latest-value
  // buffer (see StaticServer), which a value that can go now needn't.
  [[nodiscard]] std::expected<void, CallError> set_latest(
      std::span<const std::byte> item) const;
  // Items that can be sent right now; 0 once closed.
  [[nodiscard]] uint16_t credit() const;
  // Closes the channel, dropping a value still waiting for credit. `QueueFull`
  // leaves it open, to end again later.
  [[nodiscard]] std::expected<void, CallError> end(
      WireStatus status = WireStatus::OK) const;

 private:
  friend class Server;
  RawSink(Server& server, uint32_t call) : server_(&server), call_(call) {}

  Server* server_;
  uint32_t call_;
};

// What the server hands calls to: the Router, or a test.
class Dispatcher {
 public:
  // The request is valid during the call only: copy what's needed later.
  virtual void call(MethodId method, std::span<const std::byte> request,
                    RawReply reply) = 0;
  virtual void open(MethodId method, std::span<const std::byte> request,
                    RawSink sink) = 0;
  // The client dropped `call`, or the link went down, before it finished: stop
  // working on it (e.g. unsubscribe). Replies and sinks of `call` are closed.
  virtual void cancelled(CallId call, MethodId method) = 0;

 protected:
  ~Dispatcher() = default;
};

// An open call: waiting for its response, or a channel.
struct CallSlot {
  enum class State : uint8_t { Free, Unary, Stream };

  State state = State::Free;
  uint32_t call_id = 0;
  MethodId method;
  // Items the client has room for, on a channel.
  uint16_t credit = 0;
  // A value waiting for credit or queue room (see RawSink::set_latest), in this
  // slot's part of the server's latest-value memory.
  bool has_latest = false;
  uint16_t latest_size = 0;
};

struct ServerStats {
  // Rejections (no dispatcher, no room in the call table, too many streams)
  // that couldn't be sent because the send queue was full: the client waits
  // for its deadline, or for ever on a channel until it gives up and cancels.
  uint32_t lost_rejections = 0;
};

// The memory a Server works in. See StaticServer for a Server with its own.
struct ServerBuffers {
  LinkBuffers link;
  // A call per slot.
  std::span<CallSlot> slots;
  // Shared equally by the slots, for values waiting in RawSink::set_latest.
  std::span<std::byte> latest;
  // For building messages in: 8-aligned, and as large as the largest payload.
  std::span<std::byte> scratch;
};

class Server {
 public:
  // At most `max_streams` channels are open at once.
  Server(uint32_t boot_id, const LinkConfig& config, ServerBuffers buffers,
         std::size_t max_streams);
  Server(const Server&) = delete;
  Server& operator=(const Server&) = delete;

  // Where calls go; without one, they're all UNIMPLEMENTED.
  void set_dispatcher(Dispatcher& dispatcher) { dispatcher_ = &dispatcher; }

  [[nodiscard]] const Link& link() const { return link_; }
  [[nodiscard]] const ServerStats& stats() const { return stats_; }
  [[nodiscard]] std::size_t open_calls() const;

  // Feeds bytes read from the peer at `now`. Calls reach the dispatcher from
  // here, and waiting latest values go out as credit and queue room allow.
  // Once the link is down for good, cancellations reach it too.
  void receive(std::span<const std::byte> bytes, Millis now);

  // See Link::poll_transmit.
  std::optional<std::span<const std::byte>> poll_transmit(Millis now);

  // See Link::next_deadline.
  [[nodiscard]] std::optional<Millis> next_deadline() const {
    return link_.next_deadline();
  }

 private:
  friend class RawReply;
  friend class RawSink;

  CallSlot* find(uint32_t call);
  CallSlot* find(uint32_t call, CallSlot::State state);
  [[nodiscard]] std::size_t streams() const;

  void dispatch(const Frame& frame);
  void begin(const Header& header, std::span<const std::byte> payload,
             bool streaming);
  void grant(std::span<const std::byte> payload);
  void cancel(uint32_t call);
  // Answers a call that never got a slot.
  void reject(uint32_t call, bool streaming, WireStatus status);
  // Once the link is down for good, cancels every open call.
  void check_link();

  std::expected<void, CallError> queue(const Header& header,
                                       std::span<const std::byte> payload);
  std::expected<void, CallError> respond(uint32_t call, WireStatus status,
                                         std::span<const std::byte> payload);
  std::expected<void, CallError> send_item(uint32_t call,
                                           std::span<const std::byte> item);
  std::expected<void, CallError> set_latest(uint32_t call,
                                            std::span<const std::byte> item);
  std::expected<void, CallError> end_stream(uint32_t call, WireStatus status);
  uint16_t credit(uint32_t call);
  // Sends a slot's waiting value, if there's credit and room.
  void flush_latest(CallSlot& slot);
  // The slot's part of the latest-value memory.
  [[nodiscard]] std::span<std::byte> latest_buffer(const CallSlot& slot) const;

  Link link_;
  std::span<CallSlot> slots_;
  std::span<std::byte> latest_;
  std::span<std::byte> scratch_;
  std::size_t max_streams_;
  Dispatcher* dispatcher_ = nullptr;
  ServerStats stats_{};
};

namespace detail {

template <std::size_t Slots, std::size_t Latest, std::size_t Scratch>
struct CallStorage {
  std::array<CallSlot, Slots> slots{};
  std::array<std::byte, Slots * Latest> latest{};
  alignas(8) std::array<std::byte, (Scratch + 7) / 8 * 8> scratch{};
};

}  // namespace detail

// A Server with its own memory, for payloads of up to `MaxPayload` bytes and
// `MaxCalls` open calls at a time. A channel can have a latest value of up to
// `MaxLatest` bytes waiting (none by default): that much for every call slot.
template <std::size_t MaxPayload, std::size_t MaxCalls,
          std::size_t MaxLatest = 0>
class StaticServer
    : private detail::LinkStorageFor<MaxPayload>,
      private detail::CallStorage<MaxCalls, MaxLatest, MaxPayload>,
      public Server {
 public:
  explicit StaticServer(uint32_t boot_id, const LinkConfig& config = {},
                        std::size_t max_streams = MaxCalls)
      : Server(boot_id, config,
               {.link = {.receive = this->receive_buffer,
                         .queue = this->queue_buffer},
                .slots = this->slots,
                .latest = this->latest,
                .scratch = this->scratch},
               max_streams) {}
};

}  // namespace tether
