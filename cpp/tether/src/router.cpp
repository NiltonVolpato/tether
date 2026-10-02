#include "tether/router.h"

#include <cstdlib>

namespace tether {

Router::Router(Server& server, ServerTable table, std::span<Service*> services)
    : table_(table), services_(services) {
  if (services.size() != table.size()) {
    std::abort();
  }
  server.set_dispatcher(*this);
}

void Router::add(Service& service) {
  const uint8_t id = service.id();
  if (id >= table_.size() || !table_[id] || services_[id] != nullptr) {
    std::abort();
  }
  services_[id] = &service;
}

Service* Router::route(MethodId method, bool streaming) const {
  const auto found = lookup(table_, method);
  if (!found || found->method.streaming != streaming) {
    return nullptr;
  }
  return services_[method.service];
}

void Router::call(MethodId method, std::span<const std::byte> request,
                  RawReply reply) {
  if (Service* service = route(method, false)) {
    service->call(method.method, request, reply);
    return;
  }
  // If the queue is full the call stays open, until the client gives up.
  (void)reply.fail(WireStatus::UNIMPLEMENTED);
}

void Router::open(MethodId method, std::span<const std::byte> request,
                  RawSink sink) {
  if (Service* service = route(method, true)) {
    service->open(method.method, request, sink);
    return;
  }
  (void)sink.end(WireStatus::UNIMPLEMENTED);
}

void Router::cancelled(CallId call, MethodId method) {
  // The server only cancels calls it dispatched, which had a service.
  if (method.service < services_.size() && services_[method.service]) {
    services_[method.service]->cancelled(call);
  }
}

}  // namespace tether
