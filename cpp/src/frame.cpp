#include "rpc/frame.h"

#include <algorithm>
#include <cstdlib>

#include "rpc/crc32.h"

namespace rpc {
namespace {

constexpr std::size_t kCrcSize = 4;
constexpr std::size_t kSizePrefix = 4;
constexpr std::size_t kPayloadAlign = 8;

// Hands a FlatBufferBuilder one fixed buffer instead of the heap. It's large
// enough for any Header, so the builder never asks for more.
class ArenaAllocator final : public flatbuffers::Allocator {
 public:
  static constexpr std::size_t kSize = 2 * kMaxHeaderSize;

  uint8_t* allocate(std::size_t size) override {
    if (size > arena_.size()) {
      std::abort();  // A Header outgrew kMaxHeaderSize.
    }
    return arena_.data();
  }

  void deallocate(uint8_t* /*p*/, std::size_t /*size*/) override {}

 private:
  alignas(8) std::array<uint8_t, kSize> arena_{};
};

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

}  // namespace

std::expected<std::size_t, FrameError> encode(
    const Header& header, std::span<const std::byte> payload,
    std::span<std::byte> out) {
  ArenaAllocator arena;
  flatbuffers::FlatBufferBuilder fbb(ArenaAllocator::kSize, &arena);
  fbb.FinishSizePrefixed(::Rpc::CreateHeader(
      fbb, header.kind, header.seq, header.call_id, header.service,
      header.method, header.credit, header.status));
  const auto fb_header =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  if (fb_header.size() > kMaxHeaderSize) {
    std::abort();  // max_wire_size would be wrong.
  }

  constexpr std::array<std::byte, kPayloadAlign - 1> kZeros{};
  const std::size_t header_end = kCrcSize + fb_header.size();
  const auto pad = std::span(kZeros).first(
      payload.empty()
          ? 0
          : (kPayloadAlign - header_end % kPayloadAlign) % kPayloadAlign);

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
  if (!::Rpc::VerifySizePrefixedHeaderBuffer(verifier)) {
    return std::unexpected(FrameError::BadHeader);
  }
  const auto* h = ::Rpc::GetSizePrefixedHeader(fb_header);
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

}  // namespace rpc
