#include "rpc/crc32.h"

#include <gtest/gtest.h>

#include <string_view>

namespace rpc {
namespace {

constexpr uint32_t crc_of(std::string_view s) {
  Crc32 crc;
  for (char c : s) {
    const std::byte b{static_cast<uint8_t>(c)};
    crc.update(std::span(&b, 1));
  }
  return crc.value();
}

// The standard check value of CRC-32/ISO-HDLC.
static_assert(crc_of("123456789") == 0xCBF43926);
static_assert(crc_of("") == 0);

TEST(Crc32, IncrementalMatchesOneShot) {
  const std::string_view text = "The quick brown fox jumps over the lazy dog";
  const auto bytes = std::as_bytes(std::span(text));
  Crc32 crc;
  crc.update(bytes.first(10));
  crc.update(bytes.subspan(10));
  EXPECT_EQ(crc.value(), Crc32::of(bytes));
  EXPECT_EQ(crc.value(), 0x414FA339U);
}

}  // namespace
}  // namespace rpc
