#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <span>

namespace rpc {

// CRC-32 (IEEE 802.3, reflected, as in zlib and Rust's crc32fast), computed
// incrementally.
class Crc32 {
 public:
  constexpr void update(std::span<const std::byte> bytes) {
    for (std::byte b : bytes) {
      state_ = kTable[(state_ ^ std::to_integer<uint32_t>(b)) & 0xFF] ^
               (state_ >> 8);
    }
  }

  [[nodiscard]] constexpr uint32_t value() const { return ~state_; }

  [[nodiscard]] static constexpr uint32_t of(std::span<const std::byte> bytes) {
    Crc32 crc;
    crc.update(bytes);
    return crc.value();
  }

 private:
  static constexpr std::array<uint32_t, 256> kTable = [] {
    std::array<uint32_t, 256> table{};
    for (uint32_t i = 0; i < table.size(); ++i) {
      uint32_t c = i;
      for (int bit = 0; bit < 8; ++bit) {
        c = (c & 1) != 0 ? 0xEDB88320 ^ (c >> 1) : c >> 1;
      }
      table[i] = c;
    }
    return table;
  }();

  uint32_t state_ = 0xFFFFFFFF;
};

}  // namespace rpc
