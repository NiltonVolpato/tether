// Internal: a FlatBufferBuilder allocator over a fixed buffer.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstdlib>

#include "flatbuffers/flatbuffers.h"

namespace tether::detail {

// Hands a FlatBufferBuilder one fixed buffer instead of the heap. Sized for
// what's built in it, so the builder never asks for more; if it does, that's a
// bug, and there's nothing to return.
template <std::size_t Size>
class ArenaAllocator final : public flatbuffers::Allocator {
 public:
  static constexpr std::size_t kSize = Size;

  uint8_t* allocate(std::size_t size) override {
    if (size > arena_.size()) {
      std::abort();
    }
    return arena_.data();
  }

  void deallocate(uint8_t* /*p*/, std::size_t /*size*/) override {}

 private:
  alignas(8) std::array<uint8_t, Size> arena_{};
};

}  // namespace tether::detail
