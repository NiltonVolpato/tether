// The generated server code, from the test application's schema
// (core/tests/coprocessor.fbs; see generated/): a handler implemented against
// the generated interfaces, served through the generated service and table,
// called with messages built the way a client would.

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include <optional>
#include <string>
#include <vector>

#include "generated/coprocessor_rpc.h"
#include "rig.h"
#include "tether/router.h"
#include "tether/typed.h"

namespace tether {
namespace {

using namespace tether::testing;
using namespace CoprocessorProto;
using ::testing::ElementsAre;
using ::testing::Eq;
using ::testing::IsEmpty;
using ::testing::SizeIs;

// A message as a client would send it.
template <typename Build>
std::vector<std::byte> message(Build&& build) {
  flatbuffers::FlatBufferBuilder fbb;
  fbb.Finish(build(fbb));
  const auto data =
      std::as_bytes(std::span(fbb.GetBufferPointer(), fbb.GetSize()));
  return {data.begin(), data.end()};
}

std::vector<std::byte> empty() {
  return message([](auto& fbb) { return CreateEmpty(fbb); });
}

template <typename T>
const T* read(const Received& received) {
  return verify<T>(received.payload);
}

class FakeWifi final : public wifi::Handler {
 public:
  void connect(Reply<Empty> reply, const WifiConnectRequest& request) override {
    ssid = request.ssid() != nullptr ? request.ssid()->str() : "";
    (void)reply.send([](auto& fbb) { return CreateEmpty(fbb); });
  }

  void watch(Sink<WifiStatus> sink, const Empty& /*request*/) override {
    watcher = sink;
    publish();
  }

  void start_provisioning(Reply<Empty> reply,
                          const StartProvisioningRequest& request) override {
    timeout_seconds = request.timeout_seconds();
    (void)reply.fail(WireStatus::UNAVAILABLE);
  }

  void stop_provisioning(Reply<Empty> reply,
                         const Empty& /*request*/) override {
    (void)reply.send([](auto& fbb) { return CreateEmpty(fbb); });
  }

  void watch_provisioning(Sink<ProvisioningStatus> sink,
                          const Empty& /*request*/) override {
    (void)sink.end(WireStatus::UNIMPLEMENTED);
  }

  void cancelled(CallId call) override { cancellations.push_back(call); }

  // Tells the watcher, if any, the current status.
  void publish() {
    if (!watcher) {
      return;
    }
    (void)watcher->set_latest([&](flatbuffers::FlatBufferBuilder& fbb) {
      return CreateWifiStatusDirect(fbb, connected, "10.0.0.7", ssid.c_str(),
                                    -52);
    });
  }

  std::string ssid;
  bool connected = false;
  uint32_t timeout_seconds = 0;
  std::optional<Sink<WifiStatus>> watcher;
  std::vector<CallId> cancellations;
};

class GeneratedTest : public ::testing::Test {
 protected:
  GeneratedTest() { router.add(wifi_service); }

  Rig rig;
  StaticRouter<kCoprocessor.size()> router{rig.server, kCoprocessor};
  FakeWifi wifi_handler;
  wifi::Service wifi_service{wifi_handler};
};

TEST(GeneratedTable, ListsServicesAndMethodsWithTheirIds) {
  EXPECT_THAT(kCoprocessor.size(), Eq(5U));
  EXPECT_FALSE(kCoprocessor[3]);  // The removed Weather.
  EXPECT_THAT(wifi::kId, Eq(0));
  EXPECT_THAT(clock::kId, Eq(1));
  EXPECT_THAT(dashboard::kId, Eq(2));
  EXPECT_THAT(sonos::kId, Eq(4));
  EXPECT_THAT(wifi::kWatch, Eq(MethodId(0, 1)));
  EXPECT_THAT(wifi::kStartProvisioning, Eq(MethodId(0, 3)));
  EXPECT_THAT(sonos::kAlbumArt, Eq(MethodId(4, 1)));

  const ServerTable table = kCoprocessor;
  const auto watch = lookup(table, wifi::kWatch);
  ASSERT_TRUE(watch);
  EXPECT_THAT(watch->service.name, Eq("CoprocessorProto.Wifi"));
  EXPECT_THAT(watch->method.name, Eq("Watch"));
  EXPECT_TRUE(watch->method.streaming);
  EXPECT_FALSE(lookup(table, MethodId(0, 2)));  // The deprecated Scan.
  EXPECT_FALSE(lookup(table, MethodId(3, 0)));
}

TEST_F(GeneratedTest, ACallReachesTheHandlerTypedAndIsAnswered) {
  rig.request(1, wifi::kConnect, message([](auto& fbb) {
                return CreateWifiConnectRequestDirect(fbb, "home", "secret");
              }));
  EXPECT_THAT(wifi_handler.ssid, Eq("home"));
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::OK));
  EXPECT_NE(read<Empty>(got[0]), nullptr);
}

TEST_F(GeneratedTest, AnErrorFromTheHandlerIsTheStatus) {
  rig.request(1, wifi::kStartProvisioning, message([](auto& fbb) {
                return CreateStartProvisioningRequest(fbb, 90);
              }));
  EXPECT_THAT(wifi_handler.timeout_seconds, Eq(90U));
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::UNAVAILABLE));
  EXPECT_THAT(got[0].payload, IsEmpty());
}

TEST_F(GeneratedTest, ARequestThatDoesntVerifyIsInvalidArgument) {
  for (const auto& garbage : {bytes({}), bytes({1, 2, 3}),
                              bytes({0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0})}) {
    rig.request(1, wifi::kConnect, garbage);
    rig.open(2, wifi::kWatch, 1, garbage);
    const auto got = rig.take();
    ASSERT_THAT(got, SizeIs(2));
    EXPECT_THAT(got[0].header.kind, Eq(Kind::Response));
    EXPECT_THAT(got[0].header.status, Eq(WireStatus::INVALID_ARGUMENT));
    EXPECT_THAT(got[1].header.kind, Eq(Kind::End));
    EXPECT_THAT(got[1].header.status, Eq(WireStatus::INVALID_ARGUMENT));
  }
  EXPECT_THAT(wifi_handler.ssid, IsEmpty());
  EXPECT_FALSE(wifi_handler.watcher);
  EXPECT_THAT(rig.server.open_calls(), Eq(0U));
}

TEST_F(GeneratedTest, AChannelStreamsTheLatestStatus) {
  wifi_handler.connected = true;
  wifi_handler.ssid = "home";
  rig.open(2, wifi::kWatch, 1, empty());
  ASSERT_TRUE(wifi_handler.watcher);
  auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  const auto* first = read<WifiStatus>(got[0]);
  ASSERT_NE(first, nullptr);
  EXPECT_TRUE(first->connected());
  EXPECT_THAT(first->ssid()->str(), Eq("home"));
  EXPECT_THAT(first->ip()->str(), Eq("10.0.0.7"));
  EXPECT_THAT(first->rssi(), Eq(-52));

  // Two changes while the client has no room: only the newest is kept.
  wifi_handler.ssid = "work";
  wifi_handler.publish();
  wifi_handler.ssid = "cafe";
  wifi_handler.publish();
  rig.grant(2, 1);
  got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(read<WifiStatus>(got[0])->ssid()->str(), Eq("cafe"));
}

TEST_F(GeneratedTest, ACancellationReachesTheHandler) {
  rig.open(2, wifi::kWatch, 1, empty());
  rig.cancel(2);
  EXPECT_THAT(wifi_handler.cancellations, ElementsAre(CallId{2}));
}

TEST_F(GeneratedTest, WhatTheTableDoesntHaveIsUnimplemented) {
  // A deprecated method, a removed service, and services nobody added.
  for (const MethodId method : {MethodId(0, 2), MethodId(3, 0), clock::kWatch,
                                dashboard::kReportBattery}) {
    rig.request(1, method, empty());
    const auto got = rig.take();
    ASSERT_THAT(got, SizeIs(1));
    EXPECT_THAT(got[0].header.status, Eq(WireStatus::UNIMPLEMENTED));
  }
  rig.open(2, clock::kWatch, 1, empty());
  const auto got = rig.take();
  ASSERT_THAT(got, SizeIs(1));
  EXPECT_THAT(got[0].header.kind, Eq(Kind::End));
  EXPECT_THAT(got[0].header.status, Eq(WireStatus::UNIMPLEMENTED));
}

}  // namespace
}  // namespace tether
