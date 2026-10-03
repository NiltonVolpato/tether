#include "tether/cobs.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <array>
#include <cstdint>
#include <vector>

namespace tether {
namespace {

using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::Optional;

std::vector<std::byte> bytes(std::initializer_list<uint8_t> values) {
  std::vector<std::byte> out;
  for (uint8_t v : values) {
    out.push_back(std::byte{v});
  }
  return out;
}

std::vector<std::byte> run(std::size_t len, uint8_t value) {
  return std::vector<std::byte>(len, std::byte{value});
}

std::vector<std::byte> encode(std::span<const std::byte> data) {
  std::vector<std::byte> out(cobs_max_size(data.size()) + 1);
  CobsEncoder encoder(out);
  encoder.write(data);
  const auto size = encoder.finish();
  EXPECT_TRUE(size.has_value());
  out.resize(size.value_or(0));
  return out;
}

std::vector<std::byte> concat(
    std::initializer_list<std::vector<std::byte>> parts) {
  std::vector<std::byte> out;
  for (const auto& p : parts) {
    out.insert(out.end(), p.begin(), p.end());
  }
  return out;
}

// Vectors from the COBS paper and Wikipedia, which the Rust `cobs` crate
// produces too.
TEST(Cobs, EncodesKnownVectors) {
  EXPECT_THAT(encode({}), ElementsAreArray(bytes({0x01, 0x00})));
  EXPECT_THAT(encode(bytes({0x00})),
              ElementsAreArray(bytes({0x01, 0x01, 0x00})));
  EXPECT_THAT(encode(bytes({0x00, 0x00})),
              ElementsAreArray(bytes({0x01, 0x01, 0x01, 0x00})));
  EXPECT_THAT(encode(bytes({0x11, 0x22, 0x00, 0x33})),
              ElementsAreArray(bytes({0x03, 0x11, 0x22, 0x02, 0x33, 0x00})));
  EXPECT_THAT(encode(bytes({0x11, 0x00, 0x00, 0x00})),
              ElementsAreArray(bytes({0x02, 0x11, 0x01, 0x01, 0x01, 0x00})));
}

TEST(Cobs, EndsAFullBlockOnlyWhenMoreFollows) {
  EXPECT_THAT(
      encode(run(254, 0xA5)),
      ElementsAreArray(concat({bytes({0xFF}), run(254, 0xA5), bytes({0x00})})));
  EXPECT_THAT(encode(concat({run(254, 0xA5), bytes({0x00})})),
              ElementsAreArray(concat(
                  {bytes({0xFF}), run(254, 0xA5), bytes({0x01, 0x01, 0x00})})));
  EXPECT_THAT(encode(run(255, 0xA5)),
              ElementsAreArray(concat(
                  {bytes({0xFF}), run(254, 0xA5), bytes({0x02, 0xA5, 0x00})})));
}

TEST(Cobs, RoundTrips) {
  std::vector<std::vector<std::byte>> inputs = {
      {}, bytes({0}), run(253, 1), run(254, 1), run(255, 1), run(600, 7),
  };
  std::vector<std::byte> mixed;
  for (int i = 0; i < 1000; ++i) {
    mixed.push_back(std::byte(i % 7 == 0 ? 0 : i));
  }
  inputs.push_back(mixed);
  for (const auto& input : inputs) {
    auto wire = encode(input);
    ASSERT_THAT(wire.back(), Eq(std::byte{0}));
    wire.pop_back();
    EXPECT_THAT(wire, ::testing::Not(::testing::Contains(std::byte{0})));
    EXPECT_THAT(wire.size(), ::testing::Le(cobs_max_size(input.size())));
    const auto size = cobs_decode_in_place(wire);
    ASSERT_THAT(size, Optional(input.size()));
    wire.resize(*size);
    EXPECT_THAT(wire, ElementsAreArray(input));
  }
}

TEST(Cobs, RejectsInvalidInput) {
  auto short_block = bytes({0x05, 0x01});
  EXPECT_THAT(cobs_decode_in_place(short_block), Eq(std::nullopt));
  auto zero_code = bytes({0x02, 0x01, 0x00, 0x01});
  EXPECT_THAT(cobs_decode_in_place(zero_code), Eq(std::nullopt));
  auto zero_in_block = bytes({0x03, 0x01, 0x00});
  EXPECT_THAT(cobs_decode_in_place(zero_in_block), Eq(std::nullopt));
}

TEST(Cobs, ReportsOverflow) {
  std::array<std::byte, 4> out{};
  CobsEncoder encoder(out);
  encoder.write(bytes({1, 2, 3}));
  EXPECT_THAT(encoder.finish(), Eq(std::nullopt));
}

// The encoding of `data`, by a CobsStream in pieces of at most `piece` bytes.
std::vector<std::byte> stream(std::span<const std::byte> data,
                              std::size_t piece) {
  CobsStream cobs;
  std::vector<std::byte> out;
  std::vector<std::byte> buf(piece);
  while (const std::size_t n = cobs.next(data, buf)) {
    out.insert(out.end(), buf.begin(), buf.begin() + n);
  }
  EXPECT_THAT(cobs.next(data, buf), Eq(0U));
  return out;
}

TEST(Cobs, StreamEncodesAsTheEncoderInPiecesOfAnySize) {
  std::vector<std::vector<std::byte>> inputs = {
      {},
      bytes({0}),
      bytes({0, 0}),
      bytes({0x11, 0x22, 0x00, 0x33}),
      run(253, 1),
      run(254, 1),
      run(255, 1),
      concat({run(254, 0xA5), bytes({0x00})}),
      concat({run(254, 0xA5), bytes({0x00, 0x00})}),
      run(600, 7),
  };
  // Zeros at random, densely and sparsely.
  uint32_t state = 1;
  for (const uint32_t one_in : {2U, 7U, 300U}) {
    std::vector<std::byte> data;
    for (int i = 0; i < 1000; ++i) {
      state = state * 1664525U + 1013904223U;
      data.push_back((state >> 16) % one_in == 0 ? std::byte{0}
                                                 : std::byte(state >> 24 | 1));
    }
    inputs.push_back(data);
  }
  for (const auto& input : inputs) {
    SCOPED_TRACE(input.size());
    const auto expected = encode(input);
    for (const std::size_t piece : {1U, 2U, 3U, 64U, 255U, 256U, 2000U}) {
      SCOPED_TRACE(piece);
      EXPECT_THAT(stream(input, piece), ElementsAreArray(expected));
    }
  }
}

}  // namespace
}  // namespace tether
