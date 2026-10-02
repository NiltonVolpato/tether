// The server and router, against a plain Link standing in for the client. The
// wire between them is lossless (the link's own tests cover loss), so a call's
// whole exchange completes in `pump()`, without the time passing.

#include "tether/server.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <array>
#include <optional>
#include <vector>

#include "tether/router.h"

namespace tether {
namespace {

using namespace std::chrono_literals;
using ::testing::ElementsAre;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::IsEmpty;
using ::testing::SizeIs;

constexpr std::size_t kMaxPayload = 256;
constexpr std::size_t kMaxCalls = 4;
constexpr std::size_t kMaxStreams = 2;

using TestServer = StaticServer<kMaxPayload, kMaxCalls>;
using ClientLink = StaticLink<kMaxPayload>;

std::vector<std::byte> bytes(std::initializer_list<uint8_t> values) {
  std::vector<std::byte> out;
  for (const uint8_t v : values) {
    out.push_back(std::byte(v));
  }
  return out;
}

struct Received {
  Header header;
  std::vector<std::byte> payload;
};

// A Credits payload granting `credit` more items on `call_id`.
std::vector<std::byte> credits(uint32_t call_id, uint16_t credit) {
  flatbuffers::FlatBufferBuilder fbb;
  const std::vector<wire::Grant> grants{wire::Grant(call_id, credit)};
  fbb.Finish(wire::CreateCreditsDirect(fbb, &grants));
  const auto data =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  return {data.begin(), data.end()};
}

// A server and a client linked to it.
class Rig {
 public:
  Rig() : server(0xB1, {}, kMaxStreams), client(0xA1) {
    pump();
    EXPECT_THAT(server.link().state(), Eq(LinkState::Linked));
  }

  TestServer server;
  ClientLink client;
  Millis now{0};
  // What the client has received, not counting the link's own frames.
  std::vector<Received> received;

  // Moves bytes both ways until nothing more is due.
  void pump() {
    for (bool moved = true; moved;) {
      moved = false;
      while (const auto wire = client.poll_transmit(now)) {
        server.receive(*wire, now);
        moved = true;
      }
      while (const auto wire = server.poll_transmit(now)) {
        client.receive(*wire, now, [&](const Frame& frame) {
          received.push_back(
              {.header = frame.header,
               .payload = {frame.payload.begin(), frame.payload.end()}});
        });
        moved = true;
      }
    }
  }

  void send(const Header& header, const std::vector<std::byte>& payload = {}) {
    ASSERT_TRUE(client.send(header, payload));
    pump();
  }

  void request(uint32_t call, MethodId method,
               const std::vector<std::byte>& payload = {}) {
    send({.kind = Kind::Request,
          .call_id = call,
          .service = method.service,
          .method = method.method},
         payload);
  }

  void open(uint32_t call, MethodId method, uint16_t credit,
            const std::vector<std::byte>& payload = {}) {
    send({.kind = Kind::Open,
          .call_id = call,
          .service = method.service,
          .method = method.method,
          .credit = credit},
         payload);
  }

  void grant(uint32_t call, uint16_t credit) {
    send({.kind = Kind::Credit}, credits(call, credit));
  }

  void cancel(uint32_t call) { send({.kind = Kind::Cancel, .call_id = call}); }

  // What the client received since the last call.
  std::vector<Received> take() { return std::exchange(received, {}); }
};

// A dispatcher that records what it's given.
class Recorder final : public Dispatcher {
 public:
  struct Call {
    MethodId method;
    std::vector<std::byte> request;
    Reply reply;
  };
  struct Channel {
    MethodId method;
    std::vector<std::byte> request;
    Sink sink;
  };
  struct Cancelled {
    CallId call;
    MethodId method;

    friend bool operator==(const Cancelled&, const Cancelled&) = default;
  };

  void call(MethodId method, std::span<const std::byte> request,
            Reply reply) override {
    calls.push_back({.method = method,
                     .request = {request.begin(), request.end()},
                     .reply = reply});
  }
  void open(MethodId method, std::span<const std::byte> request,
            Sink sink) override {
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
  const Reply reply = recorder.calls[0].reply;
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
  const Sink sink = recorder.channels[0].sink;
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
  const Sink sink = recorder.channels[0].sink;
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

TEST_F(ServerTest, ReplyTooLargeForTheQueueNeverFits) {
  rig.request(10, kMethod);
  const Reply reply = recorder.calls[0].reply;
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
  // A send queue with room for one small frame, and no dispatcher to answer.
  alignas(8) std::array<std::byte, max_wire_size(32) - 1> rx{};
  std::array<std::byte, 2 + max_wire_size(0) + 10> tx{};
  std::array<CallSlot, 2> slots{};
  Server server(0xB1, {}, {.receive = rx, .queue = tx}, slots, 1);
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
            Reply reply) override {
    calls.push_back({.method = method,
                     .request = {request.begin(), request.end()},
                     .reply = reply});
  }
  void open(uint8_t method, std::span<const std::byte> request,
            Sink sink) override {
    channels.push_back({.method = method,
                        .request = {request.begin(), request.end()},
                        .sink = sink});
  }
  void cancelled(CallId call) override { cancellations.push_back(call); }

  struct Call {
    uint8_t method;
    std::vector<std::byte> request;
    Reply reply;
  };
  struct Channel {
    uint8_t method;
    std::vector<std::byte> request;
    Sink sink;
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
