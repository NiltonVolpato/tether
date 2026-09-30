// Decodes golden/frames.json, the wire format as the Rust core writes it, with
// flatc's decoding of each frame as the expected values.

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <fstream>
#include <nlohmann/json.hpp>
#include <string>
#include <vector>

#include "support.h"
#include "tether/frame.h"

namespace tether {
namespace {

using ::nlohmann::json;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::SizeIs;
using ::tether::testing::AlignedBuffer;
using ::tether::testing::hex;
using ::tether::testing::unhex;

std::vector<json> golden_frames() {
  std::ifstream in(TETHER_GOLDEN_FRAMES);
  return json::parse(in).get<std::vector<json>>();
}

std::vector<json> valid_golden_frames() {
  auto frames = golden_frames();
  std::erase_if(frames, [](const json& f) { return f.contains("error"); });
  return frames;
}

std::string error_name(FrameError e) {
  switch (e) {
    case FrameError::Cobs:
      return "Cobs";
    case FrameError::TooShort:
      return "TooShort";
    case FrameError::Crc:
      return "Crc";
    case FrameError::BadHeader:
      return "BadHeader";
    case FrameError::BadLayout:
      return "BadLayout";
    case FrameError::Overflow:
      return "Overflow";
  }
  return "?";
}

// A header as flatc prints it.
json header_json(const Header& h) {
  return {
      {"kind", wire::EnumNameKind(h.kind)},
      {"seq", h.seq},
      {"call_id", h.call_id},
      {"service", h.service},
      {"method", h.method},
      {"credit", h.credit},
      {"status", wire::EnumNameStatus(h.status)},
  };
}

// A framework payload as flatc prints it; anything else as hex.
json payload_json(Kind kind, std::span<const std::byte> payload) {
  const auto* data = reinterpret_cast<const uint8_t*>(payload.data());
  flatbuffers::Verifier verifier(data, payload.size());
  switch (kind) {
    case Kind::Hello: {
      if (!verifier.VerifyBuffer<wire::Hello>()) {
        return "unverifiable Hello";
      }
      const auto* hello = flatbuffers::GetRoot<wire::Hello>(data);
      return {{"boot_id", hello->boot_id()},
              {"peer_boot_id", hello->peer_boot_id()}};
    }
    case Kind::Credit: {
      if (!verifier.VerifyBuffer<wire::Credits>()) {
        return "unverifiable Credits";
      }
      json grants = json::array();
      for (const auto* g :
           *flatbuffers::GetRoot<wire::Credits>(data)->grants()) {
        grants.push_back({{"call_id", g->call_id()}, {"credit", g->credit()}});
      }
      return {{"grants", grants}};
    }
    default:
      return hex(payload);
  }
}

class GoldenFrame : public ::testing::TestWithParam<json> {
 protected:
  // The wire bytes without the delimiter.
  [[nodiscard]] std::vector<std::byte> cobs_frame() const {
    auto wire = unhex(GetParam()["wire"].get<std::string>());
    EXPECT_THAT(wire.back(), Eq(std::byte{0}));
    wire.pop_back();
    return wire;
  }

  [[nodiscard]] bool is_error() const { return GetParam().contains("error"); }
};

TEST_P(GoldenFrame, DecodesAsFlatcDoes) {
  const json& golden = GetParam();
  AlignedBuffer buffer(cobs_frame());
  const auto frame = decode(buffer.span());
  if (is_error()) {
    ASSERT_FALSE(frame.has_value()) << "decoded an invalid frame";
    EXPECT_THAT(error_name(frame.error()),
                Eq(golden["error"].get<std::string>()));
    return;
  }
  ASSERT_TRUE(frame.has_value()) << error_name(frame.error());
  EXPECT_THAT(header_json(frame->header), Eq(golden["header"]));
  EXPECT_THAT(payload_json(frame->header.kind, frame->payload),
              Eq(golden["payload"]));
  EXPECT_THAT(reinterpret_cast<std::uintptr_t>(frame->payload.data()) % 8,
              Eq(0U));
}

TEST_P(GoldenFrame, DecodesFromAByteStream) {
  const auto wire = unhex(GetParam()["wire"].get<std::string>());
  StaticDeframer<1024> deframer;
  std::vector<std::expected<std::pair<Header, std::string>, FrameError>>
      results;
  for (std::byte b : wire) {
    if (auto result = deframer.push(b)) {
      results.push_back(result->transform(
          [](const Frame& f) { return std::pair(f.header, hex(f.payload)); }));
    }
  }
  ASSERT_THAT(results, SizeIs(1));
  AlignedBuffer buffer(cobs_frame());
  const auto frame = decode(buffer.span());
  if (is_error()) {
    EXPECT_THAT(results[0], Eq(std::unexpected(frame.error())));
  } else {
    EXPECT_THAT(results[0], Eq(std::pair(frame->header, hex(frame->payload))));
  }
}

class ValidGoldenFrame : public GoldenFrame {};

// Our encoder's bytes may differ from the Rust encoder's, but they must decode
// to the same frame.
TEST_P(ValidGoldenFrame, ReencodesToAnEquivalentFrame) {
  AlignedBuffer buffer(cobs_frame());
  const auto frame = decode(buffer.span());
  ASSERT_TRUE(frame.has_value());

  std::vector<std::byte> wire(max_wire_size(frame->payload.size()));
  const auto size = encode(frame->header, frame->payload, wire);
  ASSERT_TRUE(size.has_value());
  AlignedBuffer again(std::span(wire).first(*size - 1));
  const auto reencoded = decode(again.span());
  ASSERT_TRUE(reencoded.has_value()) << error_name(reencoded.error());
  EXPECT_THAT(reencoded->header, Eq(frame->header));
  EXPECT_THAT(reencoded->payload, ElementsAreArray(frame->payload));
}

auto frame_name(const ::testing::TestParamInfo<json>& info) {
  return info.param["name"].get<std::string>();
}

INSTANTIATE_TEST_SUITE_P(Golden, GoldenFrame,
                         ::testing::ValuesIn(golden_frames()), frame_name);
INSTANTIATE_TEST_SUITE_P(Golden, ValidGoldenFrame,
                         ::testing::ValuesIn(valid_golden_frames()),
                         frame_name);

}  // namespace
}  // namespace tether
