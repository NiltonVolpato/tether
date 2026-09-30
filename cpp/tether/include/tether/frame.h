// Frame encoding: COBS([CRC32][size-prefixed Header][pad to 8][payload]) 0x00.
// See schema/wire.fbs for the layout.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <optional>
#include <span>
#include <utility>

#include "tether/cobs.h"
#include "tether/wire_generated.h"

static_assert(__cplusplus >= 202302L, "tether needs C++23");

namespace tether {

using Kind = wire::Kind;
// A gRPC status code as it travels in the header; 0 is OK.
using WireStatus = wire::Status;

// The fields of a `tether.wire.Header`.
struct Header {
  Kind kind = Kind::Hello;
  uint16_t seq = 0;
  uint32_t call_id = 0;
  uint8_t service = 0;
  uint8_t method = 0;
  uint16_t credit = 0;
  WireStatus status = WireStatus::OK;

  friend bool operator==(const Header&, const Header&) = default;
};

enum class FrameError : uint8_t {
  Cobs,
  TooShort,
  Crc,
  // The CRC passed but the header doesn't fit or doesn't verify.
  BadHeader,
  // Non-zero padding or a payload that starts past the end of the frame.
  BadLayout,
  // Larger than the buffer it was meant for.
  Overflow,
};

// A decoded frame. The payload is a view into the buffer it was decoded in,
// and 8-aligned in memory, so flatbuffers can read it in place.
struct Frame {
  Header header;
  std::span<const std::byte> payload;
};

// The largest size-prefixed Header: every field set.
inline constexpr std::size_t kMaxHeaderSize = 64;

// The largest frame on the wire, delimiter included, for a payload of
// `payload_size` bytes.
[[nodiscard]] constexpr std::size_t max_wire_size(std::size_t payload_size) {
  constexpr std::size_t kCrcAndPad = 4 + 7;
  return cobs_max_size(kCrcAndPad + kMaxHeaderSize + payload_size) + 1;
}

// Encodes a frame into `out`, including the trailing 0x00 delimiter. Returns
// its size, or `Overflow` if it doesn't fit; `max_wire_size` always does.
[[nodiscard]] std::expected<std::size_t, FrameError> encode(
    const Header& header, std::span<const std::byte> payload,
    std::span<std::byte> out);

// Decodes one COBS frame, without its 0x00 delimiter, in place. `cobs_frame`
// must start 8-aligned in memory, and the frame's payload points into it.
[[nodiscard]] std::expected<Frame, FrameError> decode(
    std::span<std::byte> cobs_frame);

// Splits a byte stream on 0x00 delimiters, decoding each frame in `buffer`:
// its size bounds a frame's COBS-encoded size, and it must be 8-aligned. See
// StaticDeframer for one with its own buffer.
class Deframer {
 public:
  explicit Deframer(std::span<std::byte> buffer);
  Deframer(const Deframer&) = delete;
  Deframer& operator=(const Deframer&) = delete;

  // Feeds one byte; returns a result when a delimiter completes a frame. The
  // frame points into the buffer, and is valid until the next `push`.
  std::optional<std::expected<Frame, FrameError>> push(std::byte b);

 private:
  std::span<std::byte> buf_;
  std::size_t len_ = 0;
  bool overflowed_ = false;
};

namespace detail {

// Storage that a class can take as its first base, so it's constructed before
// the base that uses it.
template <std::size_t Size>
struct AlignedBytes {
  alignas(8) std::array<std::byte, Size> bytes{};
};

}  // namespace detail

// A Deframer with its own buffer, for frames of up to `MaxFrame` bytes
// (COBS-encoded, without the delimiter).
template <std::size_t MaxFrame>
class StaticDeframer : private detail::AlignedBytes<MaxFrame>, public Deframer {
 public:
  StaticDeframer() : Deframer(this->bytes) {}
};

}  // namespace tether
