// The server (co-processor) side: the same protocol as the Rust core's server
// (core/src/server.rs), over a Link.
//
// Calls are addressed by call id. A request or an open goes to the Dispatcher
// (see router.h) with a RawReply or a RawSink that answers it, and those can be
// kept for as long as the call lasts. The server tracks open calls in a fixed
// table, so a call that finds it full is rejected with RESOURCE_EXHAUSTED.
//
// Sans-IO, like the link: feed received bytes in, write out what
// poll_transmit returns, and call again at next_deadline. On its own it's for
// one thread; with ServerHooks (which the platform's glue implements, e.g.
// tether_idf), replies and sinks can be used from any thread.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <optional>
#include <span>

#include "tether/descriptor.h"
#include "tether/function_ref.h"
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

// Builds a payload in the memory it's given (8-aligned at both ends), and
// returns it: a part of that memory. The typed layer builds flatbuffers with
// one (see typed.h).
using MessageBuilder =
    FunctionRef<std::span<const std::byte>(std::span<std::byte>)>;

class Server;

// Answers one unary call, at most once. A cheap handle: copies answer the same
// call, and only the first answer is sent. It outlives its call safely: once
// the call is over, or the server restarted, it's Closed.
class RawReply {
 public:
  // Answers no call: always Closed. A placeholder, e.g. to receive into.
  RawReply() = default;

  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // `Closed` if the client cancelled meanwhile. `QueueFull` leaves the call
  // open, to answer again later.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> response) const;
  // As send, with the response built in the send queue by `build`: it gets
  // the server's largest payload's worth of memory. Nothing else can be sent
  // while it runs, from any thread: a send from inside it aborts.
  [[nodiscard]] std::expected<void, CallError> send_built(
      MessageBuilder build) const;
  // Answers with an error. `status` must not be OK: that's answered as
  // INTERNAL, a handler bug.
  [[nodiscard]] std::expected<void, CallError> fail(WireStatus status) const;

 private:
  friend class Server;
  RawReply(Server& server, uint32_t epoch, uint32_t call)
      : server_(&server), epoch_(epoch), call_(call) {}

  Server* server_ = nullptr;
  uint32_t epoch_ = 0;
  uint32_t call_ = 0;
};

// The server's end of one channel. A cheap handle, like RawReply.
class RawSink {
 public:
  // The end of no channel: always Closed. A placeholder, e.g. to receive into.
  RawSink() = default;

  [[nodiscard]] CallId call_id() const { return CallId{call_}; }

  // Uses one credit; `NoCredit` until the client consumes earlier items.
  [[nodiscard]] std::expected<void, CallError> send(
      std::span<const std::byte> item) const;
  // As send, with the item built in the send queue (see RawReply::send_built).
  [[nodiscard]] std::expected<void, CallError> send_built(
      MessageBuilder build) const;
  // Latest-value mode: sends now if there's credit and room in the send
  // queue; otherwise keeps `item` for when there is, replacing whatever value
  // was waiting, so the client sees the newest and skips the ones in between.
  // `TooLarge` if it must wait and doesn't fit the channel's latest-value
  // buffer (see StaticServer), which a value that can go now needn't.
  [[nodiscard]] std::expected<void, CallError> set_latest(
      std::span<const std::byte> item) const;
  // As set_latest, with the value built in the channel's latest-value buffer,
  // whether it waits or not: one larger than that aborts.
  [[nodiscard]] std::expected<void, CallError> set_latest_built(
      MessageBuilder build) const;
  // Items that can be sent right now; 0 once closed.
  [[nodiscard]] uint16_t credit() const;
  // Closes the channel, dropping a value still waiting for credit. `QueueFull`
  // leaves it open, to end again later.
  [[nodiscard]] std::expected<void, CallError> end(
      WireStatus status = WireStatus::OK) const;

 private:
  friend class Server;
  RawSink(Server& server, uint32_t epoch, uint32_t call)
      : server_(&server), epoch_(epoch), call_(call) {}

  Server* server_ = nullptr;
  uint32_t epoch_ = 0;
  uint32_t call_ = 0;
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

// Lets threads share a server: the platform's glue implements it (e.g.
// tether_idf, over FreeRTOS).
class ServerHooks {
 public:
  // Every call into the server holds the lock, the dispatcher's included, and
  // handlers use replies and sinks there: it must be recursive.
  virtual void lock() = 0;
  virtual void unlock() = 0;
  // Something was queued to send: wake the I/O loop, if it's waiting.
  virtual void wake() = 0;

 protected:
  ~ServerHooks() = default;
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
  // Shared equally by the slots, for values waiting in RawSink::set_latest;
  // 8-aligned.
  std::span<std::byte> latest;
};

struct ServerLimits {
  // The largest payload a reply or an item is built to, a multiple of 8: a
  // build starts only with room for a body this large in the send queue,
  // contiguous, and one that outgrows it aborts.
  std::size_t max_payload = 0;
  // At most this many channels are open at once.
  std::size_t max_streams = 0;
};

class Server {
 public:
  Server(uint32_t boot_id, const LinkConfig& config, ServerBuffers buffers,
         ServerLimits limits);
  Server(const Server&) = delete;
  Server& operator=(const Server&) = delete;

  // Where calls go; without one, they're all UNIMPLEMENTED. Set up before
  // serving, as are the hooks.
  void set_dispatcher(Dispatcher& dispatcher) { dispatcher_ = &dispatcher; }
  void set_hooks(ServerHooks& hooks) { hooks_ = &hooks; }

  // Not locked: for the I/O loop's thread, which is the one that changes them.
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
  [[nodiscard]] std::optional<Millis> next_deadline() const;

  // Starts over with a new link, once the old one is down for good (or to
  // drop it): open calls are cancelled, and replies and sinks from before stay
  // closed. `boot_id` as for the Link.
  void restart(uint32_t boot_id);

 private:
  friend class RawReply;
  friend class RawSink;

  // Holds the hooks' lock, if there are hooks.
  class Guard {
   public:
    explicit Guard(const Server& server) : hooks_(server.hooks_) {
      if (hooks_ != nullptr) {
        hooks_->lock();
      }
    }
    ~Guard() {
      if (hooks_ != nullptr) {
        hooks_->unlock();
      }
    }
    Guard(const Guard&) = delete;
    Guard& operator=(const Guard&) = delete;

   private:
    ServerHooks* hooks_;
  };

  CallSlot* find(uint32_t call);
  // The slot of a handle's call, if it's still open and in that state.
  CallSlot* find(uint32_t epoch, uint32_t call, CallSlot::State state);
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
  void cancel_all();

  // Queues a frame with `payload`, or with what `build` builds in the queue.
  std::expected<void, CallError> queue(const Header& header,
                                       std::span<const std::byte> payload);
  std::expected<void, CallError> queue(const Header& header,
                                       MessageBuilder build);
  // Runs `build` in `memory`. Aborts if a build is under way already.
  std::span<const std::byte> run_build(MessageBuilder build,
                                       std::span<std::byte> memory);

  template <typename Payload>
  std::expected<void, CallError> respond(uint32_t epoch, uint32_t call,
                                         WireStatus status, Payload payload);
  template <typename Payload>
  std::expected<void, CallError> send_item(uint32_t epoch, uint32_t call,
                                           Payload item);
  std::expected<void, CallError> set_latest(uint32_t epoch, uint32_t call,
                                            std::span<const std::byte> item);
  std::expected<void, CallError> set_latest_built(uint32_t epoch, uint32_t call,
                                                  MessageBuilder build);
  std::expected<void, CallError> end_stream(uint32_t epoch, uint32_t call,
                                            WireStatus status);
  uint16_t credit(uint32_t epoch, uint32_t call);
  // Sends a slot's waiting value, if there's credit and room.
  void flush_latest(CallSlot& slot);
  // The slot's part of the latest-value memory: 8-aligned at both ends.
  [[nodiscard]] std::span<std::byte> latest_buffer(const CallSlot& slot) const;

  LinkBuffers link_buffers_;
  Link link_;
  std::span<CallSlot> slots_;
  std::span<std::byte> latest_;
  ServerLimits limits_;
  Dispatcher* dispatcher_ = nullptr;
  ServerHooks* hooks_ = nullptr;
  ServerStats stats_{};
  // Counts restarts: a handle from an earlier link is closed.
  uint32_t epoch_ = 0;
  bool building_ = false;
};

namespace detail {

template <std::size_t Slots, std::size_t Latest>
struct CallStorage {
  std::array<CallSlot, Slots> slots{};
  alignas(8) std::array<std::byte, Slots*((Latest + 7) / 8 * 8)> latest{};
};

}  // namespace detail

// A Server with its own memory, for payloads of up to `MaxPayload` bytes (a
// multiple of 8) and `MaxCalls` open calls at a time. A channel can have a
// latest value of up to `MaxLatest` bytes waiting (none by default): that much
// for every call slot.
template <std::size_t MaxPayload, std::size_t MaxCalls,
          std::size_t MaxLatest = 0>
class StaticServer : private detail::LinkStorageFor<MaxPayload>,
                     private detail::CallStorage<MaxCalls, MaxLatest>,
                     public Server {
  static_assert(MaxPayload % 8 == 0, "MaxPayload must be a multiple of 8");

 public:
  explicit StaticServer(uint32_t boot_id, const LinkConfig& config = {},
                        std::size_t max_streams = MaxCalls)
      : Server(boot_id, config,
               {.link = {.receive = this->receive_buffer,
                         .queue = this->queue_buffer},
                .slots = this->slots,
                .latest = this->latest},
               {.max_payload = MaxPayload, .max_streams = max_streams}) {}
};

}  // namespace tether
