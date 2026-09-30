#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <string_view>
#include <vector>

namespace tether::testing {

// A receive buffer: 8-aligned, as the core requires.
class AlignedBuffer {
 public:
  explicit AlignedBuffer(std::span<const std::byte> bytes)
      : size_(bytes.size()) {
    std::ranges::copy(bytes, storage_.begin());
  }

  std::span<std::byte> span() { return std::span(storage_).first(size_); }

 private:
  alignas(8) std::array<std::byte, 4096> storage_{};
  std::size_t size_;
};

inline std::vector<std::byte> unhex(std::string_view hex) {
  std::vector<std::byte> out;
  for (std::size_t i = 0; i + 1 < hex.size(); i += 2) {
    out.push_back(
        std::byte(std::stoul(std::string(hex.substr(i, 2)), nullptr, 16)));
  }
  return out;
}

inline std::string hex(std::span<const std::byte> bytes) {
  static constexpr std::string_view kDigits = "0123456789abcdef";
  std::string out;
  for (std::byte b : bytes) {
    const auto v = std::to_integer<uint8_t>(b);
    out += kDigits[v >> 4];
    out += kDigits[v & 0xF];
  }
  return out;
}

}  // namespace tether::testing
