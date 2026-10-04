//! Generated clients and handlers on the framework, against a fake
//! co-processor, over the simulated link.

extern crate alloc;

mod common;
mod generated;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use common::{ServerApp, Sim};
use generated::coprocessor_generated::coprocessor_proto::*;
use generated::coprocessor_rpc::coprocessor_proto::{COPROCESSOR, clock, dashboard, sonos, wifi};
use tether::{CallId, MethodId, Reply, Sink, Status, StreamError, Transport, lookup};
use tether_core::router::Router;
use tether_core::server::{Server, ServerEvent, ServerReply, ServerSink, SharedServer};

#[derive(Default)]
struct FakeWifi {
    status: WifiStatusT,
    watchers: Vec<Sink<ServerSink, WifiStatusT>>,
}

impl wifi::Handler<Server> for Rc<RefCell<FakeWifi>> {
    fn connect(&mut self, reply: Reply<ServerReply, EmptyT>, req: WifiConnectRequest<'_>) {
        reply.send(Ok(&EmptyT::default()));
        let mut this = self.borrow_mut();
        this.status = WifiStatusT {
            connected: true,
            ip: Some("10.0.0.7".into()),
            ssid: req.ssid().map(Into::into),
            rssi: -52,
        };
        let status = this.status.clone();
        this.watchers.retain(|w| w.set_latest(&status) != Err(StreamError::Closed));
    }

    fn watch(&mut self, sink: Sink<ServerSink, WifiStatusT>, _: Empty<'_>) {
        let mut this = self.borrow_mut();
        sink.set_latest(&this.status).unwrap();
        this.watchers.push(sink);
    }

    fn start_provisioning(
        &mut self,
        reply: Reply<ServerReply, EmptyT>,
        _: StartProvisioningRequest<'_>,
    ) {
        reply.send(Err(Status::Unavailable));
    }

    fn stop_provisioning(&mut self, reply: Reply<ServerReply, EmptyT>, _: Empty<'_>) {
        reply.send(Ok(&EmptyT::default()));
    }

    fn watch_provisioning(&mut self, sink: Sink<ServerSink, ProvisioningStatusT>, _: Empty<'_>) {
        sink.end(Err(Status::Unimplemented));
    }

    fn cancelled(&mut self, call: CallId) {
        self.borrow_mut().watchers.retain(|w| w.call_id() != call);
    }
}

const CHUNK: usize = 512;

#[derive(Default)]
struct FakeSonos {
    /// GENA subscriptions by call.
    subscriptions: BTreeMap<CallId, (String, Sink<ServerSink, GroupEventT>)>,
    unsubscribed: Vec<String>,
    art: Vec<u8>,
    /// Album art downloads in progress and how much each has sent.
    downloads: Vec<(Sink<ServerSink, AlbumArtChunkT>, usize)>,
}

impl FakeSonos {
    fn publish(&self, group: &str, volume: u8) {
        for (g, sink) in self.subscriptions.values() {
            if g == group {
                let event = GroupEventT { group: Some(g.clone()), volume, track: None };
                sink.set_latest(&event).unwrap();
            }
        }
    }

    /// Sends as many chunks as the clients have room for.
    fn pump(&mut self) {
        let mut waiting = Vec::new();
        for (sink, mut sent) in self.downloads.drain(..) {
            while sent < self.art.len() && sink.credit() > 0 {
                let end = (sent + CHUNK).min(self.art.len());
                let chunk = AlbumArtChunkT {
                    total_size: self.art.len() as u32,
                    data: Some(self.art[sent..end].to_vec()),
                };
                sink.send(&chunk).unwrap();
                sent = end;
            }
            if sent < self.art.len() {
                waiting.push((sink, sent));
            } else {
                sink.end(Ok(()));
            }
        }
        self.downloads = waiting;
    }
}

impl sonos::Handler<Server> for Rc<RefCell<FakeSonos>> {
    fn subscribe(&mut self, sink: Sink<ServerSink, GroupEventT>, req: SubscribeRequest<'_>) {
        let group = req.group().unwrap_or_default().to_string();
        self.borrow_mut().subscriptions.insert(sink.call_id(), (group, sink));
    }

    fn album_art(&mut self, sink: Sink<ServerSink, AlbumArtChunkT>, _: AlbumArtRequest<'_>) {
        self.borrow_mut().downloads.push((sink, 0));
    }

    fn cancelled(&mut self, call: CallId) {
        let mut this = self.borrow_mut();
        if let Some((group, _)) = this.subscriptions.remove(&call) {
            this.unsubscribed.push(group); // UPnP UNSUBSCRIBE goes here
        }
        this.downloads.retain(|(sink, _)| sink.call_id() != call);
    }
}

/// Wifi and Sonos are served; Clock and Dashboard are not.
struct Coprocessor {
    router: Router,
    sonos: Rc<RefCell<FakeSonos>>,
}

impl Coprocessor {
    fn new() -> Self {
        let wifi = Rc::new(RefCell::new(FakeWifi::default()));
        let sonos = Rc::new(RefCell::new(FakeSonos {
            art: (0..30_000u32).map(|i| (i * 7 % 251) as u8).collect(),
            ..Default::default()
        }));
        let mut router = Router::new(COPROCESSOR);
        router.add(wifi::Service(wifi)).add(sonos::Service(sonos.clone()));
        Self { router, sonos }
    }
}

impl ServerApp for Coprocessor {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
        self.router.handle(server, event);
    }

    fn tick(&mut self, _: &SharedServer) {
        self.sonos.borrow_mut().pump();
    }
}

fn sim() -> Sim<Coprocessor> {
    Sim::new(Coprocessor::new(), 1, 0, 0)
}

#[test]
fn wire_ids_come_from_the_server_enum_and_method_order() {
    // Weather (id 3) and Wifi.Scan (method 2) are deprecated.
    assert_eq!((wifi::ID, clock::ID, dashboard::ID, sonos::ID), (0, 1, 2, 4));
    assert_eq!(wifi::WATCH, MethodId::new(0, 1));
    assert_eq!(wifi::START_PROVISIONING, MethodId::new(0, 3));
    let (service, method) = lookup(COPROCESSOR, sonos::ALBUM_ART).unwrap();
    assert_eq!((service.name, method.name), ("CoprocessorProto.Sonos", "AlbumArt"));
}

#[test]
fn removed_services_and_methods_are_unimplemented() {
    let mut sim = sim();
    // A client built before Weather and Wifi.Scan were deprecated, and ids
    // no schema ever had.
    let stale = [
        MethodId::new(3, 0),
        MethodId::new(0, 2),
        MethodId::new(200, 0),
        MethodId::new(0, 9),
    ];
    let calls: Vec<_> = stale.iter().map(|&id| sim.client.call(id, &[], 1_000)).collect();
    sim.run_until(1_000, |_| calls.iter().all(|c| c.try_result().is_some()));
    for (id, call) in stale.iter().zip(&calls) {
        assert_eq!(call.try_result(), Some(Err(Status::Unimplemented)), "{id:?}");
    }
}

#[test]
fn connect_is_reported_on_watch() {
    let mut sim = sim();
    let mut watch = wifi::Client(&sim.client).watch(&EmptyT::default(), 1);
    let status = sim.wait(1_000, || watch.try_recv());
    assert!(!status.get().connected());

    let req = WifiConnectRequestT { ssid: Some("home".into()), password: Some("pw".into()) };
    let mut call = wifi::Client(&sim.client).connect(&req);
    assert!(sim.wait(1_000, || call.try_result()).is_ok());

    let status = sim.wait(1_000, || watch.try_recv());
    assert!(status.get().connected());
    assert_eq!(status.get().ssid(), Some("home"));
    assert_eq!(status.get().ip(), Some("10.0.0.7"));
}

#[test]
fn handler_status_reaches_the_caller() {
    let mut sim = sim();
    let req = StartProvisioningRequestT { timeout_seconds: 60 };
    let mut call = wifi::Client(&sim.client).start_provisioning(&req);
    assert_eq!(sim.wait(1_000, || call.try_result()).err(), Some(Status::Unavailable));
}

#[test]
fn dropping_a_subscription_unsubscribes_only_that_group() {
    let mut sim = sim();
    let subscribe = |sim: &Sim<Coprocessor>, group: &str| {
        let req = SubscribeRequestT { group: Some(group.into()) };
        sonos::Client(&sim.client).subscribe(&req, 1)
    };
    let mut living = subscribe(&sim, "LIVING_ROOM");
    let mut kitchen = subscribe(&sim, "KITCHEN");
    let sonos = sim.app.sonos.clone();
    sim.run_until(1_000, |_| sonos.borrow().subscriptions.len() == 2);

    sonos.borrow().publish("LIVING_ROOM", 30);
    sonos.borrow().publish("KITCHEN", 12);
    let living_event = sim.wait(1_000, || living.try_recv());
    assert_eq!(living_event.get().group(), Some("LIVING_ROOM"));
    assert_eq!(living_event.get().volume(), 30);
    assert_eq!(sim.wait(1_000, || kitchen.try_recv()).get().volume(), 12);

    drop(living);
    sim.run_until(1_000, |_| !sonos.borrow().unsubscribed.is_empty());
    assert_eq!(sonos.borrow().unsubscribed, ["LIVING_ROOM"]);

    sonos.borrow().publish("KITCHEN", 13);
    assert_eq!(sim.wait(1_000, || kitchen.try_recv()).get().volume(), 13);
}

#[test]
fn album_art_arrives_in_chunks_over_lossy_link() {
    for seed in 1..=10 {
        let mut sim = Sim::new(Coprocessor::new(), seed, 10, 10);
        let art = sim.app.sonos.borrow().art.clone();
        let req = AlbumArtRequestT { url: Some("http://speaker/art.jpg".into()) };
        let mut ch = sonos::Client(&sim.client).album_art(&req, 2);
        let mut got: Vec<u8> = Vec::new();
        let mut chunks = 0;
        sim.run_until(60_000, |_| {
            while let Some(chunk) = ch.try_recv() {
                assert_eq!(chunk.get().total_size() as usize, art.len());
                got.extend(chunk.get().data().unwrap().bytes());
                chunks += 1;
            }
            ch.end().is_some()
        });
        assert_eq!(ch.end(), Some(Ok(())), "seed {seed}");
        assert_eq!(got, art, "seed {seed}");
        assert_eq!(chunks, art.len().div_ceil(CHUNK));
    }
}

#[test]
fn dropping_album_art_stops_the_download() {
    let mut sim = sim();
    let req = AlbumArtRequestT { url: Some("http://speaker/art.jpg".into()) };
    let mut ch = sonos::Client(&sim.client).album_art(&req, 1);
    sim.wait(1_000, || ch.try_recv());
    drop(ch);
    let sonos = sim.app.sonos.clone();
    sim.run_until(1_000, |_| sonos.borrow().downloads.is_empty());
    assert_eq!(sim.server.open_calls(), 0);
}

#[test]
fn unserved_services_are_unimplemented() {
    let mut sim = sim();
    let time = clock::Client(&sim.client).watch(&EmptyT::default(), 1);
    let battery = BatteryStatusT { millivolts: 3900, percent: 80, is_plugged: false };
    let mut call = dashboard::Client(&sim.client).report_battery(&battery);
    assert_eq!(sim.wait(1_000, || call.try_result()).err(), Some(Status::Unimplemented));
    sim.run_until(1_000, |_| time.end().is_some());
    assert_eq!(time.end(), Some(Err(Status::Unimplemented)));
}

#[test]
fn malformed_request_is_invalid_argument() {
    let mut sim = sim();
    let call = sim.client.call(wifi::CONNECT, &[0xFF; 3], 1_000);
    assert_eq!(sim.wait(1_000, || call.try_result()), Err(Status::InvalidArgument));
}
