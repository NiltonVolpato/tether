#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>

namespace tether {

// Worst-case COBS encoding of `size` bytes, without the delimiter.
[[nodiscard]] constexpr std::size_t cobs_max_size(std::size_t size) {
  return size + size / 254 + 1;
}

// Decodes COBS in place (the output is never longer than the input). Returns
// the decoded size, or nothing if `bytes` isn't valid COBS. `bytes` excludes
// the delimiter.
[[nodiscard]] std::optional<std::size_t> cobs_decode_in_place(
    std::span<std::byte> bytes);

// COBS-encodes bytes fed in pieces into `out`.
class CobsEncoder {
 public:
  explicit CobsEncoder(std::span<std::byte> out) : out_(out) {}

  void write(std::span<const std::byte> bytes);
  void write(std::byte b);

  // Ends the frame with the 0x00 delimiter. Returns the frame's size, or
  // nothing if it didn't fit in `out`.
  [[nodiscard]] std::optional<std::size_t> finish();

 private:
  void put(std::byte b);
  void end_block();

  std::span<std::byte> out_;
  std::size_t code_at_ = 0;
  std::size_t next_ = 1;
  uint8_t code_ = 1;
  bool overflow_ = false;
};

}  // namespace tether
