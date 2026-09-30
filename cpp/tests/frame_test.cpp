#include "tether/frame.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <limits>
#include <vector>

#include "support.h"

namespace tether {
namespace {

using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::Le;
using ::tether::testing::AlignedBuffer;

constexpr Header kMaxHeader{
    .kind = Kind::Item,
    .seq = std::numeric_limits<uint16_t>::max(),
    .call_id = std::numeric_limits<uint32_t>::max(),
    .service = std::numeric_limits<uint8_t>::max(),
    .method = std::numeric_limits<uint8_t>::max(),
    .credit = std::numeric_limits<uint16_t>::max(),
    .status = WireStatus::DATA_LOSS,
};

std::vector<std::byte> payload_of(std::size_t size, uint8_t first = 1) {
  std::vector<std::byte> payload(size);
  for (std::size_t i = 0; i < size; ++i) {
    payload[i] = std::byte(static_cast<uint8_t>(first + i));
  }
  return payload;
}

TEST(Frame, RoundTripsWithAlignedPayloadForEveryPadding) {
  for (std::size_t size : {0, 1, 7, 8, 9, 511}) {
    const auto payload = payload_of(size);
    std::vector<std::byte> wire(max_wire_size(size));
    const auto wire_size = encode(kMaxHeader, payload, wire);
    ASSERT_TRUE(wire_size.has_value());
    EXPECT_THAT(wire[*wire_size - 1], Eq(std::byte{0}));

    AlignedBuffer buffer(std::span(wire).first(*wire_size - 1));
    const auto frame = decode(buffer.span());
    ASSERT_TRUE(frame.has_value()) << "size " << size;
    EXPECT_THAT(frame->header, Eq(kMaxHeader));
    EXPECT_THAT(frame->payload, ElementsAreArray(payload));
    EXPECT_THAT(reinterpret_cast<std::uintptr_t>(frame->payload.data()) % 8,
                Eq(0U));
  }
}

// Payloads without zeros cost COBS the most.
TEST(Frame, MaxWireSizeIsEnough) {
  for (std::size_t size = 0; size <= 1100; ++size) {
    const std::vector<std::byte> payload(size, std::byte{0xA5});
    std::vector<std::byte> wire(max_wire_size(size));
    const auto wire_size = encode(kMaxHeader, payload, wire);
    ASSERT_TRUE(wire_size.has_value()) << "size " << size;
    EXPECT_THAT(*wire_size, Le(max_wire_size(size)));
  }
}

TEST(Frame, EncodeReportsOverflow) {
  std::array<std::byte, 16> small{};
  EXPECT_THAT(encode(Header{.kind = Kind::Ack}, {}, small),
              Eq(std::unexpected(FrameError::Overflow)));
}

TEST(Frame, DeframerResyncsAfterGarbageAndOverflow) {
  const auto payload = payload_of(3);
  std::vector<std::byte> good(max_wire_size(payload.size()));
  good.resize(encode(Header{.kind = Kind::Cancel}, payload, good).value());

  std::vector<std::byte> stream = {std::byte{0x55}, std::byte{0x66},
                                   std::byte{0x00}};
  stream.insert(stream.end(), 2000, std::byte{0xAA});
  stream.push_back(std::byte{0});
  stream.insert(stream.end(), good.begin(), good.end());

  StaticDeframer<1024> deframer;
  std::vector<std::expected<std::vector<std::byte>, FrameError>> results;
  for (std::byte b : stream) {
    if (auto result = deframer.push(b)) {
      results.push_back(result->transform([](const Frame& f) {
        return std::vector<std::byte>(f.payload.begin(), f.payload.end());
      }));
    }
  }
  ASSERT_THAT(results.size(), Eq(3U));
  EXPECT_FALSE(results[0].has_value());
  EXPECT_THAT(results[1], Eq(std::unexpected(FrameError::Overflow)));
  ASSERT_TRUE(results[2].has_value());
  EXPECT_THAT(*results[2], ElementsAreArray(payload));
}

TEST(Frame, DeframerSkipsEmptyFrames) {
  StaticDeframer<64> deframer;
  EXPECT_FALSE(deframer.push(std::byte{0}).has_value());
  EXPECT_FALSE(deframer.push(std::byte{0}).has_value());
}

}  // namespace
}  // namespace tether
