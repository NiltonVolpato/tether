#include "tether/cobs.h"

#include <algorithm>

namespace tether {

std::optional<std::size_t> cobs_decode_in_place(std::span<std::byte> bytes) {
  std::size_t read = 0;
  std::size_t written = 0;
  while (read < bytes.size()) {
    const auto code = std::to_integer<uint8_t>(bytes[read++]);
    if (code == 0) {
      return std::nullopt;
    }
    const std::size_t len = code - 1U;
    if (len > bytes.size() - read) {
      return std::nullopt;
    }
    const auto block = bytes.subspan(read, len);
    if (std::ranges::contains(block, std::byte{0})) {
      return std::nullopt;
    }
    // `written` never passes `read`, so this only moves bytes backwards.
    std::ranges::copy(block,
                      bytes.begin() + static_cast<std::ptrdiff_t>(written));
    read += len;
    written += len;
    if (code != 0xFF && read < bytes.size()) {
      bytes[written++] = std::byte{0};
    }
  }
  return written;
}

void CobsEncoder::write(std::span<const std::byte> bytes) {
  for (std::byte b : bytes) {
    write(b);
  }
}

void CobsEncoder::write(std::byte b) {
  // A full block ends without implying a zero. It's only ended once more
  // bytes follow, so data that ends with one needs no empty block after it.
  if (code_ == 0xFF) {
    end_block();
  }
  if (b == std::byte{0}) {
    end_block();
    return;
  }
  put(b);
  ++code_;
}

std::optional<std::size_t> CobsEncoder::finish() {
  end_block();
  // end_block() reserved a code byte for the next block; the delimiter goes
  // there instead.
  if (overflow_ || code_at_ >= out_.size()) {
    return std::nullopt;
  }
  out_[code_at_] = std::byte{0};
  return code_at_ + 1;
}

void CobsEncoder::put(std::byte b) {
  if (next_ < out_.size()) {
    out_[next_] = b;
  } else {
    overflow_ = true;
  }
  ++next_;
}

void CobsEncoder::end_block() {
  if (code_at_ < out_.size()) {
    out_[code_at_] = std::byte{code_};
  } else {
    overflow_ = true;
  }
  code_at_ = next_++;
  code_ = 1;
}

}  // namespace tether
