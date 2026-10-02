// The server (co-processor) side: the same protocol as the Rust core's server
// (core/src/server.rs), over a Link.
//
// Calls are addressed by call id. A request or an open goes to the Dispatcher
// (see router.h) with a Reply or a Sink that answers it, and those can be kept
// for as long as the call lasts. The server tracks open calls in a fixed table,
// so a call that finds it full is rejected with RESOURCE_EXHAUSTED.
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
class Reply {
 public:
  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // `Closed` if the client cancelled meanwhile. `QueueFull` leaves the call
  // open, to answer again later.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> response) const;
  // Answers with an error. `status` must not be OK: that's answered as
  // INTERNAL, a handler bug.
  [[nodiscard]] std::expected<void, CallError> fail(WireStatus status) const;

 private:
  friend class Server;
  Reply(Server& server, uint32_t call) : server_(&server), call_(call) {}

  Server* server_;
  uint32_t call_;
};

// The server's end of one channel. A cheap handle, like Reply.
class Sink {
 public:
  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // Uses one credit; `NoCredit` until the client consumes earlier items.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> item) const;
  // Items that can be sent right now; 0 once closed.
  [[nodiscard]] uint16_t credit() const;
  // Closes the channel. `QueueFull` leaves it open, to end again later.
  [[nodiscard]] std::expected<void, CallError> end(
      WireStatus status = WireStatus::OK) const;

 private:
  friend class Server;
  Sink(Server& server, uint32_t call) : server_(&server), call_(call) {}

  Server* server_;
  uint32_t call_;
};

// What the server hands calls to: the Router, or a test.
class Dispatcher {
 public:
  // The request is valid during the call only: copy what's needed later.
  virtual void call(MethodId method, std::span<const std::byte> request,
                    Reply reply) = 0;
  virtual void open(MethodId method, std::span<const std::byte> request,
                    Sink sink) = 0;
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
};

struct ServerStats {
  // Rejections (no dispatcher, no room in the call table, too many streams)
  // that couldn't be sent because the send queue was full: the client waits
  // for its deadline, or for ever on a channel until it gives up and cancels.
  uint32_t lost_rejections = 0;
};

class Server {
 public:
  // At most `max_streams` channels are open at once, and a call per slot.
  Server(uint32_t boot_id, const LinkConfig& config, LinkBuffers buffers,
         std::span<CallSlot> slots, std::size_t max_streams);
  Server(const Server&) = delete;
  Server& operator=(const Server&) = delete;

  // Where calls go; without one, they're all UNIMPLEMENTED.
  void set_dispatcher(Dispatcher& dispatcher) { dispatcher_ = &dispatcher; }

  [[nodiscard]] const Link& link() const { return link_; }
  [[nodiscard]] const ServerStats& stats() const { return stats_; }
  [[nodiscard]] std::size_t open_calls() const;

  // Feeds bytes read from the peer at `now`. Calls reach the dispatcher from
  // here. Once the link is down for good, so do cancellations.
  void receive(std::span<const std::byte> bytes, Millis now);

  // See Link::poll_transmit.
  std::optional<std::span<const std::byte>> poll_transmit(Millis now);

  // See Link::next_deadline.
  [[nodiscard]] std::optional<Millis> next_deadline() const {
    return link_.next_deadline();
  }

 private:
  friend class Reply;
  friend class Sink;

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
  std::expected<void, CallError> end_stream(uint32_t call, WireStatus status);
  uint16_t credit(uint32_t call);

  Link link_;
  std::span<CallSlot> slots_;
  std::size_t max_streams_;
  Dispatcher* dispatcher_ = nullptr;
  ServerStats stats_{};
};

namespace detail {

template <std::size_t Slots>
struct CallStorage {
  std::array<CallSlot, Slots> slots{};
};

}  // namespace detail

// A Server with its own memory, for payloads of up to `MaxPayload` bytes and
// `MaxCalls` open calls at a time.
template <std::size_t MaxPayload, std::size_t MaxCalls>
class StaticServer : private detail::LinkStorageFor<MaxPayload>,
                     private detail::CallStorage<MaxCalls>,
                     public Server {
 public:
  explicit StaticServer(uint32_t boot_id, const LinkConfig& config = {},
                        std::size_t max_streams = MaxCalls)
      : Server(boot_id, config,
               {.receive = this->receive_buffer, .queue = this->queue_buffer},
               this->slots, max_streams) {}
};

}  // namespace tether
