#include "tether/frame.h"

#include <algorithm>
#include <cstdlib>
#include <cstring>

#include "arena.h"
#include "tether/crc32.h"

namespace tether {
namespace {

constexpr std::size_t kCrcSize = 4;
constexpr std::size_t kSizePrefix = 4;
constexpr std::size_t kPayloadAlign = 8;

// Room for any Header.
using HeaderArena = detail::ArenaAllocator<2 * kMaxHeaderSize>;

uint32_t load_le32(std::span<const std::byte, 4> b) {
  return std::to_integer<uint32_t>(b[0]) |
         std::to_integer<uint32_t>(b[1]) << 8 |
         std::to_integer<uint32_t>(b[2]) << 16 |
         std::to_integer<uint32_t>(b[3]) << 24;
}

std::array<std::byte, 4> le32(uint32_t v) {
  return {std::byte(v), std::byte(v >> 8), std::byte(v >> 16),
          std::byte(v >> 24)};
}

constexpr std::array<std::byte, kPayloadAlign - 1> kZeros{};

// `header` as a size-prefixed flatbuffer, built in `arena`.
std::span<const std::byte> build_header(const Header& header,
                                        HeaderArena& arena) {
  flatbuffers::FlatBufferBuilder fbb(HeaderArena::kSize, &arena);
  fbb.FinishSizePrefixed(wire::CreateHeader(
      fbb, header.kind, header.seq, header.call_id, header.service,
      header.method, header.credit, header.status));
  const auto fb_header =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  if (fb_header.size() > kMaxHeaderSize) {
    std::abort();  // max_wire_size would be wrong.
  }
  // In the arena, which the builder leaves alone.
  return fb_header;
}

// The zeros between a header ending at `header_end` and the payload.
std::span<const std::byte> padding(std::size_t header_end,
                                   std::span<const std::byte> payload) {
  return std::span(kZeros).first(
      payload.empty()
          ? 0
          : (kPayloadAlign - header_end % kPayloadAlign) % kPayloadAlign);
}

}  // namespace

std::expected<std::size_t, FrameError> write_body(
    const Header& header, std::span<const std::byte> payload,
    std::span<std::byte> out) {
  HeaderArena arena;
  const auto fb_header = build_header(header, arena);
  const auto pad = padding(kCrcSize + fb_header.size(), payload);
  const std::size_t payload_at = kCrcSize + fb_header.size() + pad.size();
  if (payload_at + payload.size() > out.size()) {
    return std::unexpected(FrameError::Overflow);
  }
  // First, as the rest may overwrite where it is.
  if (!payload.empty()) {
    std::memmove(out.data() + payload_at, payload.data(), payload.size());
  }
  std::ranges::copy(fb_header, out.begin() + kCrcSize);
  std::ranges::copy(pad, out.begin() + kCrcSize + fb_header.size());
  const auto body = out.first(payload_at + payload.size());
  std::ranges::copy(le32(Crc32::of(body.subspan(kCrcSize))), body.begin());
  return body.size();
}

std::expected<std::size_t, FrameError> encode(
    const Header& header, std::span<const std::byte> payload,
    std::span<std::byte> out) {
  HeaderArena arena;
  const auto fb_header = build_header(header, arena);
  const auto pad = padding(kCrcSize + fb_header.size(), payload);

  Crc32 crc;
  crc.update(fb_header);
  crc.update(pad);
  crc.update(payload);

  CobsEncoder cobs(out);
  cobs.write(le32(crc.value()));
  cobs.write(fb_header);
  cobs.write(pad);
  cobs.write(payload);
  const auto size = cobs.finish();
  if (!size) {
    return std::unexpected(FrameError::Overflow);
  }
  return *size;
}

std::expected<Frame, FrameError> decode(std::span<std::byte> cobs_frame) {
  const auto body_size = cobs_decode_in_place(cobs_frame);
  if (!body_size) {
    return std::unexpected(FrameError::Cobs);
  }
  const auto body = cobs_frame.first(*body_size);
  if (body.size() < kCrcSize + kSizePrefix) {
    return std::unexpected(FrameError::TooShort);
  }
  if (load_le32(body.first<4>()) != Crc32::of(body.subspan(kCrcSize))) {
    return std::unexpected(FrameError::Crc);
  }

  const std::size_t header_size =
      load_le32(body.subspan<kCrcSize, kSizePrefix>());
  if (header_size > body.size() - kCrcSize - kSizePrefix) {
    return std::unexpected(FrameError::BadHeader);
  }
  const std::size_t header_end = kCrcSize + kSizePrefix + header_size;
  const auto* fb_header =
      reinterpret_cast<const uint8_t*>(body.data() + kCrcSize);
  flatbuffers::Verifier verifier(fb_header, header_end - kCrcSize);
  if (!wire::VerifySizePrefixedHeaderBuffer(verifier)) {
    return std::unexpected(FrameError::BadHeader);
  }
  const auto* h = wire::GetSizePrefixedHeader(fb_header);
  const Header header{
      .kind = h->kind(),
      .seq = h->seq(),
      .call_id = h->call_id(),
      .service = h->service(),
      .method = h->method(),
      .credit = h->credit(),
      .status = h->status(),
  };

  if (header_end == body.size()) {
    return Frame{.header = header, .payload = {}};
  }
  const std::size_t start =
      (header_end + kPayloadAlign - 1) / kPayloadAlign * kPayloadAlign;
  if (start >= body.size() ||
      !std::ranges::all_of(body.subspan(header_end, start - header_end),
                           [](std::byte b) { return b == std::byte{0}; })) {
    return std::unexpected(FrameError::BadLayout);
  }
  return Frame{.header = header, .payload = body.subspan(start)};
}

Deframer::Deframer(std::span<std::byte> buffer) : buf_(buffer) {
  if (reinterpret_cast<std::uintptr_t>(buffer.data()) % 8 != 0) {
    std::abort();  // Payloads wouldn't be aligned for flatbuffers.
  }
}

std::optional<std::expected<Frame, FrameError>> Deframer::push(std::byte b) {
  if (b != std::byte{0}) {
    if (len_ < buf_.size()) {
      buf_[len_++] = b;
    } else {
      overflowed_ = true;
    }
    return std::nullopt;
  }
  const std::size_t len = std::exchange(len_, 0);
  if (std::exchange(overflowed_, false)) {
    return std::unexpected(FrameError::Overflow);
  }
  if (len == 0) {
    return std::nullopt;
  }
  return decode(buf_.first(len));
}

}  // namespace tether
