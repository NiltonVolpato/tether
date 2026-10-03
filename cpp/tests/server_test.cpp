// The server and router, against a plain Link standing in for the client. The
// wire between them is lossless (the link's own tests cover loss), so a call's
// whole exchange completes in `pump()`, without the time passing.

#include "tether/server.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <array>
#include <optional>
#include <vector>

#include "rig.h"
#include "tether/router.h"
#include "tether/typed.h"

namespace tether {
namespace {

using namespace std::chrono_literals;
using ::testing::ElementsAre;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::IsEmpty;
using ::testing::Optional;
using ::testing::SizeIs;
using namespace tether::testing;

// A dispatcher that records what it's given.
class Recorder final : public Dispatcher {
 public:
  struct Call {
    MethodId method;
    std::vector<std::byte> request;
    RawReply reply;
  };
  struct Channel {
    MethodId method;
    std::vector<std::byte> request;
    RawSink sink;
  };
  struct Cancelled {
    CallId call;
    MethodId method;

    friend bool operator==(const Cancelled&, const Cancelled&) = default;
  };

  void call(MethodId method, std::span<const std::byte> request,
            RawReply reply) override {
    calls.push_back({.method = method,
                     .request = {request.begin(), request.end()},
                     .reply = reply});
  }
  void open(MethodId method, std::span<const std::byte> request,
            RawSink sink) override {
    channels.push_back({.method = method,
                        .request = {request.begin(), request.end()},
                        .sink = sink});
  }
  void cancelled(CallId call, MethodId method) override {
    cancellations.push_back({.call = call, .method = method});
  }

  std::vector<Call> calls;
  std::vector<Channel> channels;
  std::vector<Cancelled> cancellations;
};

constexpr MethodId kMethod{3, 7};

class ServerTest : public ::testing::Test {
 protected:
  ServerTest() { rig.server.set_dispatcher(recorder); }

  Recorder recorder;
  Rig rig;
};

TEST_F(ServerTest, UnaryCallIsDispatchedAndAnswered) {
  rig.request(10, kMethod, bytes({1, 2, 3}));
  ASSERT_THAT(recorder.calls, SizeIs(1));
  EXPECT_THAT(recorder.calls[0].method, Eq(kMethod));
  EXPECT_THAT(recorder.calls[0].request,
              ElementsAre(std::byte(1), std::byte(2), std::byte(3)));
  EXPECT_THAT(recorder.calls[0].reply.call_id(), Eq(CallId{10}));
  EXPECT_THAT(rig.server.open_calls(), Eq(1U));

  ASSERT_TRUE(recorder.calls[0].reply.send(bytes({9, 8})));
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
  EXPECT_THAT(got[0].header.call_id, Eq(10U));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::OK));
  EXPECT_THAT(got[0].payload, ElementsAreArray(bytes({9, 8})));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
}

TEST_F(ServerTest, ACallIsAnsweredOnce) {
  rig.request(10, kMethod);
  const RawReply reply = recorder.calls[0].reply;
  ASSERT_TRUE(reply.send({}));
  EXPECT_THAT(reply.send({}), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(reply.fail(WireStatus::NOT_FOUND),
              Eq(std::unexpected(CallError::Closed)));
  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(1));
}

TEST_F(ServerTest, FailSendsTheStatusWithoutAPayload) {
  rig.request(10, kMethod);
  rig.request(11, kMethod);
  ASSERT_TRUE(recorder.calls[0].reply.fail(WireStatus::NOT_FOUND));
  // OK isn't an error; a handler passing it is answered INTERNAL.
  ASSERT_TRUE(recorder.calls[1].reply.fail(WireStatus::OK));
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(2));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::NOT_FOUND));
  EXPECT_THAT(got[0].payload, IsEmpty());
  EXPECT_THAT(got[1].header.status, Eq(WireStatus::INTERNAL));
}

TEST_F(ServerTest, ChannelItemsUseCredit) {
  rig.open(20, kMethod, 2, bytes({5}));
  ASSERT_THAT(recorder.channels, SizeIs(1));
  const RawSink sink = recorder.channels[0].sink;
  EXPECT_THAT(recorder.channels[0].request, ElementsAre(std::byte(5)));
  EXPECT_THAT(sink.credit(), Eq(2U));

  ASSERT_TRUE(sink.send(bytes({1})));
  ASSERT_TRUE(sink.send(bytes({2})));
  EXPECT_THAT(sink.credit(), Eq(0U));
  EXPECT_THAT(sink.send(bytes({3})), Eq(std::unexpected(CallError::NoCredit)));

  rig.grant(20, 1);
  EXPECT_THAT(sink.credit(), Eq(1U));
  ASSERT_TRUE(sink.send(bytes({3})));
  ASSERT_TRUE(sink.end());
  rig.pump();

  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(4));
  for (int i = 0; i < 3; ++i) {
    EXPECT_THAT(got[i].header.kind, Eq(Kind::Item));
    EXPECT_THAT(got[i].header.call_id, Eq(20U));
    EXPECT_THAT(got[i].payload, ElementsAreArray(bytes({uint8_t(i + 1)})));
  }
  EXPECT_THAT(got[3].header.kind, Eq(Kind::End));
  EXPECT_THAT(got[3].header.status, Eq(WireStatus::OK));

  // Ended: the sink is closed.
  EXPECT_THAT(sink.credit(), Eq(0U));
  EXPECT_THAT(sink.send(bytes({4})), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(sink.end(), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
}

TEST_F(ServerTest, EndCarriesItsStatus) {
  rig.open(20, kMethod, 1);
  ASSERT_TRUE(recorder.channels[0].sink.end(WireStatus::ABORTED));
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::ABORTED));
}

TEST_F(ServerTest, CreditSaturates) {
  rig.open(20, kMethod, UINT16_MAX - 1);
  rig.grant(20, 10);
  EXPECT_THAT(recorder.channels[0].sink.credit(), Eq(UINT16_MAX));
}

TEST_F(ServerTest, CreditsForOtherCallsAreIgnored) {
  rig.open(20, kMethod, 1);
  rig.request(21, kMethod);
  // Unknown, unary and ended calls have no credit to grant.
  rig.grant(99, 5);
  rig.grant(21, 5);
  EXPECT_THAT(recorder.channels[0].sink.credit(), Eq(1U));
  EXPECT_THAT(rig.server.open_calls(), Eq(2U));
}

TEST_F(ServerTest, MalformedCreditsAreIgnored) {
  rig.open(20, kMethod, 1);
  rig.send({.kind = Kind::Credit}, bytes({1, 2, 3, 4, 5, 6, 7, 8}));
  rig.send({.kind = Kind::Credit});
  EXPECT_THAT(recorder.channels[0].sink.credit(), Eq(1U));
}

TEST_F(ServerTest, OneCreditsFrameGrantsManyChannels) {
  rig.open(20, kMethod, 0);
  rig.open(21, kMethod, 0);
  flatbuffers::FlatBufferBuilder fbb;
  const std::vector<wire::Grant> grants{wire::Grant(20, 3), wire::Grant(21, 4)};
  fbb.Finish(wire::CreateCreditsDirect(fbb, &grants));
  const auto data =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  rig.send({.kind = Kind::Credit}, {data.begin(), data.end()});
  EXPECT_THAT(recorder.channels[0].sink.credit(), Eq(3U));
  EXPECT_THAT(recorder.channels[1].sink.credit(), Eq(4U));
}

TEST_F(ServerTest, TooManyStreamsAreRejected) {
  static_assert(kMaxStreams == 2);
  rig.open(20, kMethod, 1);
  rig.open(21, kMethod, 1);
  rig.open(22, kMethod, 1);
  EXPECT_THAT(recorder.channels, SizeIs(2));
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::End));
  EXPECT_THAT(got[0].header.call_id, Eq(22U));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::RESOURCE_EXHAUSTED));

  // Unary calls still have room, and a closed stream frees one.
  rig.request(23, kMethod);
  EXPECT_THAT(recorder.calls, SizeIs(1));
  ASSERT_TRUE(recorder.channels[0].sink.end());
  rig.open(24, kMethod, 1);
  EXPECT_THAT(recorder.channels, SizeIs(3));
}

TEST_F(ServerTest, ACallTableThatIsFullRejects) {
  for (uint32_t call = 1; call <= kMaxCalls; ++call) {
    rig.request(call, kMethod);
  }
  EXPECT_THAT(recorder.calls, SizeIs(kMaxCalls));
  EXPECT_THAT(rig.take(), IsEmpty());

  rig.request(5, kMethod);
  rig.open(6, kMethod, 1);
  EXPECT_THAT(recorder.calls, SizeIs(kMaxCalls));
  EXPECT_THAT(recorder.channels, IsEmpty());
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(2));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::RESOURCE_EXHAUSTED));
  EXPECT_THAT(got[1].header.kind, Eq(Kind::End));
  EXPECT_THAT(got[1].header.status, Eq(WireStatus::RESOURCE_EXHAUSTED));

  ASSERT_TRUE(recorder.calls[0].reply.send({}));
  rig.request(7, kMethod);
  EXPECT_THAT(recorder.calls, SizeIs(kMaxCalls + 1));
}

TEST_F(ServerTest, ReusingTheIdOfAnOpenCallIsIgnored) {
  rig.request(10, kMethod);
  rig.request(10, kMethod);
  rig.open(10, kMethod, 1);
  EXPECT_THAT(recorder.calls, SizeIs(1));
  EXPECT_THAT(recorder.channels, IsEmpty());
  EXPECT_THAT(rig.server.open_calls(), Eq(1U));
}

TEST_F(ServerTest, CancelReachesTheDispatcherAndClosesTheCall) {
  rig.request(10, kMethod);
  rig.open(20, MethodId{1, 2}, 1);
  rig.cancel(10);
  rig.cancel(20);
  EXPECT_THAT(recorder.cancellations,
              ElementsAre(Recorder::Cancelled{CallId{10}, kMethod},
                          Recorder::Cancelled{CallId{20}, MethodId{1, 2}}));
  EXPECT_THAT(recorder.calls[0].reply.send({}),
              Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(recorder.channels[0].sink.send({}),
              Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
  EXPECT_THAT(rig.take(), IsEmpty());
}

TEST_F(ServerTest, CancellingAnUnknownOrFinishedCallDoesNothing) {
  rig.cancel(99);
  rig.request(10, kMethod);
  ASSERT_TRUE(recorder.calls[0].reply.send({}));
  rig.cancel(10);
  EXPECT_THAT(recorder.cancellations, IsEmpty());
}

TEST_F(ServerTest, ALostLinkCancelsEveryOpenCall) {
  rig.request(10, kMethod);
  rig.open(20, kMethod, 1);
  ASSERT_THAT(rig.server.open_calls(), Eq(2U));
  // The client reboots: a Hello with a new boot id.
  ClientLink rebooted(0xA2);
  const auto hello = rebooted.poll_transmit(rig.now);
  ASSERT_TRUE(hello);
  rig.server.receive(*hello, rig.now);

  EXPECT_THAT(rig.server.link().state(), Eq(LinkState::PeerRebooted));
  EXPECT_THAT(recorder.cancellations, SizeIs(2));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
  EXPECT_THAT(recorder.calls[0].reply.send({}),
              Eq(std::unexpected(CallError::Closed)));
}

TEST_F(ServerTest, ALostPeerCancelsEveryOpenCall) {
  rig.request(10, kMethod);
  // Nothing comes back: the server pings, retransmits, and gives up.
  while (!is_terminal(rig.server.link().state())) {
    rig.now = *rig.server.next_deadline();
    while (rig.server.poll_transmit(rig.now)) {
    }
  }
  EXPECT_THAT(rig.server.link().state(), Eq(LinkState::PeerLost));
  EXPECT_THAT(recorder.cancellations,
              ElementsAre(Recorder::Cancelled{CallId{10}, kMethod}));
}

TEST_F(ServerTest, ABusyQueueIsBackpressure) {
  rig.open(20, kMethod, 100);
  const RawSink sink = recorder.channels[0].sink;
  const std::vector<std::byte> item(kMaxPayload);
  // Nothing is pumped, so nothing is acked: the queue fills.
  int sent = 0;
  while (sink.send(item)) {
    ++sent;
  }
  EXPECT_GE(sent, 2);
  EXPECT_THAT(sink.send(item), Eq(std::unexpected(CallError::QueueFull)));
  // A frame that didn't go out didn't use credit.
  EXPECT_THAT(sink.credit(), Eq(100 - sent));

  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(sent));
  ASSERT_TRUE(sink.send(item));
  ASSERT_TRUE(sink.end());
  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(2));
}

// The payloads of the items the client received, as numbers.
std::vector<int> items(const std::vector<Received>& got) {
  std::vector<int> out;
  for (const auto& r : got) {
    if (r.header.kind == Kind::Item) {
      out.push_back(r.payload.empty() ? -1
                                      : std::to_integer<int>(r.payload[0]));
    }
  }
  return out;
}

TEST_F(ServerTest, LatestGoesAtOnceWithCredit) {
  rig.open(20, kMethod, 2);
  const RawSink sink = recorder.channels[0].sink;
  ASSERT_TRUE(sink.set_latest(bytes({1})));
  ASSERT_TRUE(sink.set_latest(bytes({2})));
  EXPECT_THAT(sink.credit(), Eq(0U));
  rig.pump();
  EXPECT_THAT(items(rig.take()), ElementsAre(1, 2));
}

TEST_F(ServerTest, LatestWaitsForCreditAndReplacesWhatWaits) {
  rig.open(20, kMethod, 1);
  const RawSink sink = recorder.channels[0].sink;
  for (const uint8_t v : {1, 2, 3}) {
    ASSERT_TRUE(sink.set_latest(bytes({v})));
  }
  rig.pump();
  // The first used the credit; 2 was replaced by 3.
  EXPECT_THAT(items(rig.take()), ElementsAre(1));

  // The waiting value goes out once, and leaves the rest of the credit.
  rig.grant(20, 3);
  EXPECT_THAT(items(rig.take()), ElementsAre(3));
  EXPECT_THAT(sink.credit(), Eq(2U));
  rig.grant(20, 1);
  EXPECT_THAT(items(rig.take()), IsEmpty());
  EXPECT_THAT(sink.credit(), Eq(3U));
}

TEST_F(ServerTest, LatestThatMustWaitHasToFitItsBuffer) {
  rig.open(20, kMethod, 1);
  const RawSink sink = recorder.channels[0].sink;
  // Not waiting, so nothing to store.
  ASSERT_TRUE(sink.set_latest(std::vector<std::byte>(kMaxLatest + 100)));
  EXPECT_THAT(sink.set_latest(std::vector<std::byte>(kMaxLatest + 1)),
              Eq(std::unexpected(CallError::TooLarge)));
  EXPECT_TRUE(sink.set_latest(std::vector<std::byte>(kMaxLatest)));
  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(1));
  rig.grant(20, 1);
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].payload, SizeIs(kMaxLatest));
}

TEST_F(ServerTest, LatestWaitsForRoomInTheSendQueue) {
  rig.open(20, kMethod, 100);
  const RawSink sink = recorder.channels[0].sink;
  const std::vector<std::byte> big(kMaxPayload);
  // Full to the last small frame, not only too full for a big one.
  int sent = 0;
  while (sink.send(big)) {
    ++sent;
  }
  while (sink.send(bytes({0}))) {
    ++sent;
  }
  ASSERT_THAT(sink.send(bytes({0})), Eq(std::unexpected(CallError::QueueFull)));
  // It can wait, so unlike send it isn't refused; a newer value replaces it.
  // One that can't wait is: it'd have to go now.
  ASSERT_TRUE(sink.set_latest(bytes({7})));
  ASSERT_TRUE(sink.set_latest(bytes({8})));
  EXPECT_THAT(sink.set_latest(big), Eq(std::unexpected(CallError::QueueFull)));
  EXPECT_THAT(sink.credit(), Eq(100 - sent));

  // The acks make room, and then it goes.
  rig.pump();
  const auto got = items(rig.take());
  ASSERT_THAT(got, SizeIs(sent + 1));
  EXPECT_THAT(got.back(), Eq(8));
  EXPECT_THAT(sink.credit(), Eq(100 - sent - 1));
}

TEST_F(ServerTest, EndDropsTheWaitingLatest) {
  rig.open(20, kMethod, 0);
  const RawSink sink = recorder.channels[0].sink;
  ASSERT_TRUE(sink.set_latest(bytes({5})));
  ASSERT_TRUE(sink.end());
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::End));
  // A later channel in the same slot doesn't inherit it.
  rig.open(21, kMethod, 1);
  rig.pump();
  EXPECT_THAT(rig.take(), IsEmpty());
}

TEST_F(ServerTest, LatestOnAClosedChannelIsClosed) {
  rig.open(20, kMethod, 0);
  rig.cancel(20);
  EXPECT_THAT(recorder.channels[0].sink.set_latest({}),
              Eq(std::unexpected(CallError::Closed)));
}

TEST_F(ServerTest, ReplyTooLargeForTheQueueNeverFits) {
  rig.request(10, kMethod);
  const RawReply reply = recorder.calls[0].reply;
  const std::vector<std::byte> huge(4 * kMaxPayload);
  EXPECT_THAT(reply.send(huge), Eq(std::unexpected(CallError::TooLarge)));
  // The call is still open, to answer with an error instead.
  ASSERT_TRUE(reply.fail(WireStatus::OUT_OF_RANGE));
}

TEST(ServerWithoutDispatcher, EverythingIsUnimplemented) {
  Rig rig;
  rig.request(10, kMethod);
  rig.open(20, kMethod, 1);
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(2));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::UNIMPLEMENTED));
  EXPECT_THAT(got[1].header.kind, Eq(Kind::End));
  EXPECT_THAT(got[1].header.status, Eq(WireStatus::UNIMPLEMENTED));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
}

TEST(ServerQueue, RejectionsThatDontFitAreCounted) {
  // A send queue with room for one rejection, and no dispatcher to answer.
  std::array<std::byte, max_body_size(0)> body{};
  const auto rejection = write_body({.kind = Kind::Response,
                                     .seq = 1,
                                     .call_id = 1,
                                     .status = WireStatus::UNIMPLEMENTED},
                                    {}, body);
  ASSERT_TRUE(rejection);
  alignas(8) std::array<std::byte, max_wire_size(32) - 1> rx{};
  alignas(8) std::array<std::byte, 64> tx{};
  ASSERT_GE(tx.size(), 2 + *rejection);
  ASSERT_LT(tx.size(), 2 * (2 + *rejection));
  std::array<CallSlot, 2> slots{};
  Server server(
      0xB1, {},
      {.link = {.receive = rx, .queue = tx}, .slots = slots, .latest = {}},
      {.max_payload = 32, .max_streams = 1});
  ClientLink client(0xA1);
  const Millis now{0};
  while (server.link().state() != LinkState::Linked) {
    if (const auto wire = client.poll_transmit(now)) {
      server.receive(*wire, now);
    }
    if (const auto wire = server.poll_transmit(now)) {
      client.receive(*wire, now, [](const Frame&) {});
    }
  }

  // Requests the server doesn't answer in between, as when the wire is slow.
  std::array<std::byte, max_wire_size(0)> wire{};
  for (uint16_t seq = 1; seq <= 3; ++seq) {
    const auto size =
        encode({.kind = Kind::Request, .seq = seq, .call_id = seq}, {}, wire);
    ASSERT_TRUE(size);
    server.receive(std::span(wire).first(*size), now);
  }
  EXPECT_THAT(server.stats().lost_rejections, Eq(2U));
  EXPECT_THAT(server.open_calls(), Eq(0U));
}

// A service that records what it's given.
class FakeService final : public Service {
 public:
  explicit FakeService(uint8_t id) : id_(id) {}

  [[nodiscard]] uint8_t id() const override { return id_; }
  void call(uint8_t method, std::span<const std::byte> request,
            RawReply reply) override {
    calls.push_back({.method = method,
                     .request = {request.begin(), request.end()},
                     .reply = reply});
  }
  void open(uint8_t method, std::span<const std::byte> request,
            RawSink sink) override {
    channels.push_back({.method = method,
                        .request = {request.begin(), request.end()},
                        .sink = sink});
  }
  void cancelled(CallId call) override { cancellations.push_back(call); }

  struct Call {
    uint8_t method;
    std::vector<std::byte> request;
    RawReply reply;
  };
  struct Channel {
    uint8_t method;
    std::vector<std::byte> request;
    RawSink sink;
  };

  std::vector<Call> calls;
  std::vector<Channel> channels;
  std::vector<CallId> cancellations;

 private:
  uint8_t id_;
};

// Service 0 has a unary method 0 and a channel 1; service 1 was deprecated;
// service 2 has a unary method 0 and a deprecated 1; service 3 is never added.
constexpr std::array<std::optional<MethodInfo>, 2> kGreeterMethods{
    MethodInfo{.name = "SayHello", .streaming = false},
    MethodInfo{.name = "Countdown", .streaming = true}};
constexpr std::array<std::optional<MethodInfo>, 2> kOtherMethods{
    MethodInfo{.name = "Poke", .streaming = false}, std::nullopt};
constexpr std::array<std::optional<MethodInfo>, 1> kLonelyMethods{
    MethodInfo{.name = "Hi", .streaming = false}};
constexpr std::array<std::optional<ServiceInfo>, 4> kTable{
    ServiceInfo{.name = "Test.Greeter", .methods = kGreeterMethods},
    std::nullopt, ServiceInfo{.name = "Test.Other", .methods = kOtherMethods},
    ServiceInfo{.name = "Test.Lonely", .methods = kLonelyMethods}};

class RouterTest : public ::testing::Test {
 protected:
  RouterTest() {
    router.add(greeter);
    router.add(other);
  }

  void expect_unimplemented(Kind kind) {
    const auto got = rig.take();
    ASSERT_THAT(got, SizeIs(1));
    EXPECT_THAT(got[0].header.kind, Eq(kind));
    EXPECT_THAT(got[0].header.status, Eq(WireStatus::UNIMPLEMENTED));
    EXPECT_THAT(rig.server.open_calls(), Eq(0U));
  }

  Rig rig;
  StaticRouter<kTable.size()> router{rig.server, kTable};
  FakeService greeter{0};
  FakeService other{2};
};

TEST_F(RouterTest, RoutesCallsAndChannelsToTheirService) {
  rig.request(1, {0, 0}, bytes({7}));
  rig.open(2, {0, 1}, 3, bytes({8}));
  rig.request(3, {2, 0});
  ASSERT_THAT(greeter.calls, SizeIs(1));
  EXPECT_THAT(greeter.calls[0].method, Eq(0));
  EXPECT_THAT(greeter.calls[0].request, ElementsAre(std::byte(7)));
  ASSERT_THAT(greeter.channels, SizeIs(1));
  EXPECT_THAT(greeter.channels[0].method, Eq(1));
  EXPECT_THAT(greeter.channels[0].request, ElementsAre(std::byte(8)));
  EXPECT_THAT(greeter.channels[0].sink.credit(), Eq(3U));
  ASSERT_THAT(other.calls, SizeIs(1));
  EXPECT_THAT(other.channels, IsEmpty());

  ASSERT_TRUE(greeter.calls[0].reply.send(bytes({1})));
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
}

TEST_F(RouterTest, UnknownMethodsAreUnimplemented) {
  rig.request(1, {0, 2});
  expect_unimplemented(Kind::Response);
  rig.open(2, {0, 9}, 1);
  expect_unimplemented(Kind::End);
  rig.request(3, {200, 0});
  expect_unimplemented(Kind::Response);
  EXPECT_THAT(greeter.calls, IsEmpty());
  EXPECT_THAT(greeter.channels, IsEmpty());
}

TEST_F(RouterTest, DeprecatedMethodsAndServicesAreUnimplemented) {
  rig.request(1, {2, 1});
  expect_unimplemented(Kind::Response);
  rig.request(2, {1, 0});
  expect_unimplemented(Kind::Response);
  EXPECT_THAT(other.calls, IsEmpty());
}

TEST_F(RouterTest, ServicesNeverAddedAreUnimplemented) {
  rig.request(1, {3, 0});
  expect_unimplemented(Kind::Response);
}

TEST_F(RouterTest, TheWrongKindOfCallIsUnimplemented) {
  rig.open(1, {0, 0}, 1);
  expect_unimplemented(Kind::End);
  rig.request(2, {0, 1});
  expect_unimplemented(Kind::Response);
  EXPECT_THAT(greeter.calls, IsEmpty());
  EXPECT_THAT(greeter.channels, IsEmpty());
}

TEST_F(RouterTest, CancellationReachesTheOwningService) {
  rig.request(1, {0, 0});
  rig.request(2, {2, 0});
  rig.open(3, {0, 1}, 1);
  rig.cancel(2);
  rig.cancel(3);
  EXPECT_THAT(greeter.cancellations, ElementsAre(CallId{3}));
  EXPECT_THAT(other.cancellations, ElementsAre(CallId{2}));
}

TEST_F(RouterTest, ALostLinkCancelsThroughTheRouter) {
  rig.request(1, {0, 0});
  rig.request(2, {2, 0});
  ClientLink rebooted(0xA2);
  const auto hello = rebooted.poll_transmit(rig.now);
  ASSERT_TRUE(hello);
  rig.server.receive(*hello, rig.now);
  EXPECT_THAT(greeter.cancellations, ElementsAre(CallId{1}));
  EXPECT_THAT(other.cancellations, ElementsAre(CallId{2}));
}

// Typed messages, with the framework's own Hello table as the message.
auto hello(uint32_t boot_id, uint32_t peer_boot_id) {
  return [=](flatbuffers::FlatBufferBuilder& fbb) {
    return wire::CreateHello(fbb, boot_id, peer_boot_id);
  };
}

const wire::Hello* as_hello(const std::vector<std::byte>& payload) {
  return verify<wire::Hello>(payload);
}

TEST(Typed, VerifyReadsAMessageInPlaceOrRefusesIt) {
  alignas(8) std::array<std::byte, 64> storage{};
  flatbuffers::FlatBufferBuilder fbb;
  fbb.Finish(wire::CreateHello(fbb, 7, 9));
  std::ranges::copy(
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize())),
      storage.begin());
  const auto message = std::span<const std::byte>(storage).first(fbb.GetSize());

  const auto* got = verify<wire::Hello>(message);
  ASSERT_NE(got, nullptr);
  EXPECT_THAT(got->boot_id(), Eq(7U));
  EXPECT_THAT(got->peer_boot_id(), Eq(9U));
  // In place: the table is in the buffer, not a copy.
  const auto* at = reinterpret_cast<const std::byte*>(got);
  EXPECT_TRUE(at >= message.data() && at < message.data() + message.size());
  EXPECT_EQ(verify<wire::Hello>(message.first(message.size() - 1)), nullptr);
  EXPECT_EQ(verify<wire::Hello>({}), nullptr);
  EXPECT_EQ(verify<wire::Hello>(std::span<const std::byte>(storage).first(8)),
            nullptr);
}

TEST_F(ServerTest, TypedReplyBuildsItsMessage) {
  rig.request(10, kMethod);
  const Reply<wire::Hello> reply(recorder.calls[0].reply);
  EXPECT_THAT(reply.call_id(), Eq(CallId{10}));
  ASSERT_TRUE(reply.send(hello(1, 2)));
  rig.pump();
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  const auto* message = as_hello(got[0].payload);
  ASSERT_NE(message, nullptr);
  EXPECT_THAT(message->boot_id(), Eq(1U));
  EXPECT_THAT(message->peer_boot_id(), Eq(2U));
  EXPECT_THAT(reply.fail(WireStatus::NOT_FOUND),
              Eq(std::unexpected(CallError::Closed)));
}

TEST_F(ServerTest, TypedSinkBuildsItemsAndLatestValues) {
  rig.open(20, kMethod, 1);
  const Sink<wire::Hello> sink(recorder.channels[0].sink);
  ASSERT_TRUE(sink.send(hello(1, 0)));
  EXPECT_THAT(sink.send(hello(2, 0)), Eq(std::unexpected(CallError::NoCredit)));
  ASSERT_TRUE(sink.set_latest(hello(3, 0)));
  ASSERT_TRUE(sink.set_latest(hello(4, 0)));
  rig.pump();
  rig.grant(20, 1);
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(2));
  EXPECT_THAT(as_hello(got[0].payload)->boot_id(), Eq(1U));
  EXPECT_THAT(as_hello(got[1].payload)->boot_id(), Eq(4U));
  ASSERT_TRUE(sink.end(WireStatus::ABORTED));
}

TEST_F(ServerTest, ABuildRunsOnlyWhenItsMessageCanGo) {
  rig.request(10, kMethod);
  rig.open(20, kMethod, 0);
  rig.open(21, kMethod, 100);
  const Reply<wire::Hello> reply(recorder.calls[0].reply);
  const Sink<wire::Hello> starved(recorder.channels[0].sink);
  const Sink<wire::Hello> sink(recorder.channels[1].sink);
  int builds = 0;
  const auto counted = [&](flatbuffers::FlatBufferBuilder& fbb) {
    ++builds;
    return wire::CreateHello(fbb, 1, 2);
  };

  EXPECT_THAT(starved.send(counted), Eq(std::unexpected(CallError::NoCredit)));
  // Not until there's room for the largest payload, however small this is.
  const std::vector<std::byte> big(kMaxPayload);
  while (recorder.channels[1].sink.send(big)) {
  }
  EXPECT_THAT(sink.send(counted), Eq(std::unexpected(CallError::QueueFull)));
  EXPECT_THAT(reply.send(counted), Eq(std::unexpected(CallError::QueueFull)));
  EXPECT_THAT(builds, Eq(0));

  rig.pump();
  rig.take();
  ASSERT_TRUE(reply.send(counted));
  EXPECT_THAT(reply.send(counted), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(builds, Eq(1));
}

TEST_F(ServerTest, ABuiltLatestValueMustFitItsBufferEvenWithCredit) {
  rig.open(20, kMethod, 5);
  const Sink<wire::Credits> sink(recorder.channels[0].sink);
  const std::vector<wire::Grant> fits(kMaxLatest / 8 - 5);
  ASSERT_TRUE(sink.set_latest([&](flatbuffers::FlatBufferBuilder& fbb) {
    return wire::CreateCreditsDirect(fbb, &fits);
  }));
  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(1));
  const std::vector<wire::Grant> too_many(kMaxLatest / 8);
  EXPECT_DEATH((void)sink.set_latest([&](flatbuffers::FlatBufferBuilder& fbb) {
    return wire::CreateCreditsDirect(fbb, &too_many);
  }),
               "");
}

TEST_F(ServerTest, SendingFromInsideABuildAborts) {
  rig.request(10, kMethod);
  rig.request(11, kMethod);
  rig.open(20, kMethod, 5);
  const Reply<wire::Hello> reply(recorder.calls[0].reply);
  const RawReply other = recorder.calls[1].reply;
  const Sink<wire::Hello> sink(recorder.channels[0].sink);
  EXPECT_DEATH((void)reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
    (void)other.fail(WireStatus::ABORTED);
    return wire::CreateHello(fbb, 1, 2);
  }),
               "");
  EXPECT_DEATH((void)sink.set_latest([&](flatbuffers::FlatBufferBuilder& fbb) {
    (void)sink.send(hello(3, 4));
    return wire::CreateHello(fbb, 1, 2);
  }),
               "");
}

TEST_F(ServerTest, RestartCancelsAndClosesWhatCameBefore) {
  rig.request(10, kMethod);
  rig.open(20, kMethod, 5);
  const RawReply reply = recorder.calls[0].reply;
  const RawSink sink = recorder.channels[0].sink;
  rig.server.restart(0xB2);
  EXPECT_THAT(recorder.cancellations, SizeIs(2));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
  EXPECT_THAT(rig.server.link().state(), Eq(LinkState::Connecting));

  // A new client, whose ids start over: the old handles don't reach its calls.
  rig.reboot_client(0xA2);
  ASSERT_THAT(rig.server.link().state(), Eq(LinkState::Linked));
  EXPECT_THAT(rig.server.link().peer_boot_id(), Optional(0xA2U));
  rig.request(10, kMethod);
  rig.open(20, kMethod, 5);
  EXPECT_THAT(reply.send({}), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(sink.send({}), Eq(std::unexpected(CallError::Closed)));
  EXPECT_THAT(sink.credit(), Eq(0U));
  rig.pump();
  EXPECT_THAT(rig.take(), IsEmpty());
  ASSERT_TRUE(recorder.calls[1].reply.send({}));
  ASSERT_TRUE(recorder.channels[1].sink.send({}));
  rig.pump();
  EXPECT_THAT(rig.take(), SizeIs(2));
}

// Hooks that check every call into the server holds the lock.
class CheckingHooks final : public ServerHooks {
 public:
  void lock() override { ++depth; }
  void unlock() override {
    ASSERT_GT(depth, 0);
    --depth;
  }
  void wake() override {
    EXPECT_GT(depth, 0);
    ++wakes;
  }

  int depth = 0;
  int wakes = 0;
};

// A dispatcher that answers at once, and checks it's called locked.
class LockedAnswerer final : public Dispatcher {
 public:
  explicit LockedAnswerer(const CheckingHooks& hooks) : hooks_(hooks) {}

  void call(MethodId /*method*/, std::span<const std::byte> /*request*/,
            RawReply reply) override {
    EXPECT_GT(hooks_.depth, 0);
    EXPECT_TRUE(Reply<wire::Hello>(reply).send(hello(1, 2)));
  }
  void open(MethodId /*method*/, std::span<const std::byte> /*request*/,
            RawSink sink) override {
    EXPECT_GT(hooks_.depth, 0);
    sinks.push_back(sink);
  }
  void cancelled(CallId /*call*/, MethodId /*method*/) override {
    EXPECT_GT(hooks_.depth, 0);
  }

  std::vector<RawSink> sinks;

 private:
  const CheckingHooks& hooks_;
};

TEST(ServerHooks, EverythingRunsLockedAndSendsWake) {
  Rig rig;
  CheckingHooks hooks;
  LockedAnswerer answerer(hooks);
  rig.server.set_hooks(hooks);
  rig.server.set_dispatcher(answerer);

  rig.request(10, kMethod);
  EXPECT_THAT(hooks.wakes, Eq(1));
  rig.open(20, kMethod, 1);
  ASSERT_THAT(answerer.sinks, SizeIs(1));
  const Sink<wire::Hello> sink(answerer.sinks[0]);
  // From outside, as another thread would.
  ASSERT_TRUE(sink.set_latest(hello(3, 4)));
  ASSERT_TRUE(sink.set_latest(hello(5, 6)));  // Waits for credit: no wake.
  EXPECT_THAT(hooks.wakes, Eq(2));
  EXPECT_THAT(sink.credit(), Eq(0U));
  ASSERT_TRUE(sink.end());
  EXPECT_THAT(hooks.wakes, Eq(3));
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
  rig.request(11, kMethod);
  rig.open(21, kMethod, 1);
  rig.server.restart(0xB2);
  EXPECT_THAT(hooks.depth, Eq(0));
  EXPECT_THAT(rig.take(), SizeIs(4));
}

TEST_F(ServerTest, ABuiltMessageBiggerThanAnyPayloadAborts) {
  rig.request(10, kMethod);
  const Reply<wire::Credits> reply(recorder.calls[0].reply);
  const std::vector<wire::Grant> grants(kMaxPayload);
  EXPECT_DEATH((void)reply.send([&](flatbuffers::FlatBufferBuilder& fbb) {
    return wire::CreateCreditsDirect(fbb, &grants);
  }),
               "");
}

TEST(RouterDeathTest, WiringMistakesAbortAtStartup) {
  Rig rig;
  StaticRouter<kTable.size()> router{rig.server, kTable};
  FakeService greeter{0};
  FakeService deprecated{1};
  FakeService beyond{9};
  router.add(greeter);
  EXPECT_DEATH(router.add(greeter), "");
  EXPECT_DEATH(router.add(deprecated), "");
  EXPECT_DEATH(router.add(beyond), "");
}

TEST(Descriptor, LookupFindsLiveMethodsOnly) {
  const ServerTable table = kTable;
  const auto found = lookup(table, {0, 1});
  ASSERT_TRUE(found);
  EXPECT_THAT(found->service.name, Eq("Test.Greeter"));
  EXPECT_THAT(found->method.name, Eq("Countdown"));
  EXPECT_TRUE(found->method.streaming);
  EXPECT_FALSE(lookup(table, {0, 2}));
  EXPECT_FALSE(lookup(table, {1, 0}));
  EXPECT_FALSE(lookup(table, {2, 1}));
  EXPECT_FALSE(lookup(table, {4, 0}));
}

}  // namespace
}  // namespace tether
