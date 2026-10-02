// Routes the server's calls to the Services that own them, as the Rust core's
// router does (core/src/router.rs). A method that's unknown, deprecated, of a
// service never added, or of the other kind (unary vs. channel) is answered
// UNIMPLEMENTED. A service answers INVALID_ARGUMENT itself, when it can't read
// the request: generated services verify it before calling their handler.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <span>

#include "tether/descriptor.h"
#include "tether/server.h"

namespace tether {

// What the router dispatches to: one per `rpc_service`, implemented by
// generated code on top of a handler.
class Service {
 public:
  // The service's id in its server's table.
  [[nodiscard]] virtual uint8_t id() const = 0;
  // `method` is the method's number in the service. The request is valid
  // during the call only, and not verified yet.
  virtual void call(uint8_t method, std::span<const std::byte> request,
                    Reply reply) = 0;
  virtual void open(uint8_t method, std::span<const std::byte> request,
                    Sink sink) = 0;
  // The client dropped `call` before it finished.
  virtual void cancelled(CallId call) = 0;

 protected:
  ~Service() = default;
};

// The memory a Router works in: a slot per service in the table. See
// StaticRouter for a Router with its own.
class Router : public Dispatcher {
 public:
  // Becomes `server`'s dispatcher, so it must outlive the server's calls.
  // `services` has a null for each entry of `table`, which Router fills.
  Router(Server& server, ServerTable table, std::span<Service*> services);
  Router(const Router&) = delete;
  Router& operator=(const Router&) = delete;

  // Serves `service`. Aborts if its id isn't in the table, or it was added
  // before: a wiring bug, found at startup.
  void add(Service& service);

  void call(MethodId method, std::span<const std::byte> request,
            Reply reply) override;
  void open(MethodId method, std::span<const std::byte> request,
            Sink sink) override;
  void cancelled(CallId call, MethodId method) override;

 private:
  // The service for `method`, if it's known and of the kind asked for.
  [[nodiscard]] Service* route(MethodId method, bool streaming) const;

  ServerTable table_;
  std::span<Service*> services_;
};

namespace detail {

template <std::size_t Services>
struct ServiceStorage {
  std::array<Service*, Services> services{};
};

}  // namespace detail

// A Router for a table of `Services` entries (deprecated ones included).
template <std::size_t Services>
class StaticRouter : private detail::ServiceStorage<Services>, public Router {
 public:
  StaticRouter(Server& server, ServerTable table)
      : Router(server, table, this->services) {}
};

}  // namespace tether
