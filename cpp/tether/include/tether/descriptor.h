// What a server offers, as generated from its schema: the same data objects as
// the Rust `tether` crate (tether/src/descriptor.rs).

#pragma once

#include <cstdint>
#include <optional>
#include <span>
#include <string_view>

namespace tether {

// A method's wire id: its service's value in the server's `rpc_server` enum
// and its position within that `rpc_service`.
struct MethodId {
  constexpr MethodId() = default;
  constexpr MethodId(uint8_t service, uint8_t method)
      : service(service), method(method) {}

  uint8_t service = 0;
  uint8_t method = 0;

  friend constexpr bool operator==(MethodId, MethodId) = default;
};

// Identifies one call on a server, from start to finish.
enum class CallId : uint32_t {};

struct MethodInfo {
  std::string_view name;
  bool streaming = false;
};

struct ServiceInfo {
  // Full name, e.g. "CoprocessorProto.Wifi".
  std::string_view name;
  // Indexed by method number. Deprecated methods are empty.
  std::span<const std::optional<MethodInfo>> methods;
};

// A server's services, generated from its `rpc_server` enum and indexed by
// service id. Deprecated services are empty.
using ServerTable = std::span<const std::optional<ServiceInfo>>;

struct MethodRef {
  const ServiceInfo& service;
  const MethodInfo& method;
};

// Looks up a method; empty if it's unknown or deprecated.
[[nodiscard]] constexpr std::optional<MethodRef> lookup(ServerTable table,
                                                        MethodId id) {
  if (id.service >= table.size() || !table[id.service]) {
    return std::nullopt;
  }
  const ServiceInfo& service = *table[id.service];
  if (id.method >= service.methods.size() || !service.methods[id.method]) {
    return std::nullopt;
  }
  return MethodRef{.service = service, .method = *service.methods[id.method]};
}

}  // namespace tether
