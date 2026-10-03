// A callable passed without a template or the heap, as C++26's
// std::function_ref: it refers to the callable, so it's valid while that is,
// and is meant for parameters.

#pragma once

#include <functional>
#include <memory>
#include <type_traits>
#include <utility>

namespace tether {

template <typename Signature>
class FunctionRef;

template <typename R, typename... Args>
class FunctionRef<R(Args...)> {
 public:
  template <typename F>
    requires std::is_invocable_r_v<R, F&, Args...> &&
                 (!std::is_same_v<std::remove_cvref_t<F>, FunctionRef>)
  // NOLINTNEXTLINE(bugprone-forwarding-reference-overload): constrained above.
  FunctionRef(F&& f)  // NOLINT(google-explicit-constructor): a parameter type.
      : object_(const_cast<void*>(static_cast<const void*>(std::addressof(f)))),
        call_([](void* object, Args... args) -> R {
          return std::invoke(*static_cast<std::remove_reference_t<F>*>(object),
                             std::forward<Args>(args)...);
        }) {}

  R operator()(Args... args) const {
    return call_(object_, std::forward<Args>(args)...);
  }

 private:
  void* object_;
  R (*call_)(void*, Args...);
};

}  // namespace tether
