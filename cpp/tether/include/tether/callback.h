// An object and one of its `void()` methods, to call back later: what a sink
// or a reply calls when it's ready (see RawSink::on_ready). No heap, no
// captures: state lives in the object, which must outlive the callback.
// Unlike FunctionRef, it's meant to be kept.

#pragma once

#include <concepts>

namespace tether {

class Callback {
 public:
  // Calls nothing: empty.
  Callback() = default;

  // e.g. `Callback(this, &AlbumArtHandler::ready)`. The method may be a public
  // or protected one of a base class.
  template <class T, class U>
    requires std::derived_from<T, U>
  Callback(T* object, void (U::*method)())
      : object_(static_cast<U*>(object)),
        // Converting a member pointer and back is lossless: invoke<U>
        // restores it.
        method_(reinterpret_cast<AnyMethod>(method)),
        invoke_(&invoke<U>) {}

  void operator()() const { invoke_(object_, method_); }

  explicit operator bool() const { return invoke_ != nullptr; }

 private:
  class Any;
  using AnyMethod = void (Any::*)();

  template <class U>
  static void invoke(void* object, AnyMethod method) {
    (static_cast<U*>(object)->*reinterpret_cast<void (U::*)()>(method))();
  }

  void* object_ = nullptr;
  AnyMethod method_ = nullptr;
  void (*invoke_)(void*, AnyMethod) = nullptr;
};

}  // namespace tether
