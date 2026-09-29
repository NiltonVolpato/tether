//! Generated clients and handlers against a fake co-processor.

extern crate alloc;

mod common;
mod generated;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use common::{ServerApp, Sim};
use generated::coprocessor_generated::coprocessor_proto::*;
use generated::coprocessor_rpc::coprocessor_proto::{clock, dashboard, sonos, wifi};
use rpc_experiment::method_id;
use rpc_experiment::proto::Status;
use rpc_experiment::router::Router;
use rpc_experiment::server::{Server, ServerEvent, StreamError};
use rpc_experiment::typed::{Reply, Sink};

#[derive(Default)]
struct FakeWifi {
    status: WifiStatusT,
    watchers: Vec<Sink<WifiStatusT>>,
}

impl wifi::Handler for Rc<RefCell<FakeWifi>> {
    fn connect(&mut self, server: &mut Server, reply: Reply<EmptyT>, req: WifiConnectRequest<'_>) {
        reply.send(server, Ok(&EmptyT::default()));
        let mut this = self.borrow_mut();
        this.status = WifiStatusT {
            connected: true,
            ip: Some("10.0.0.7".into()),
            ssid: req.ssid().map(Into::into),
            rssi: -52,
        };
        let status = this.status.clone();
        this.watchers
            .retain(|w| w.set_latest(server, &status) != Err(StreamError::Closed));
    }

    fn watch(&mut self, server: &mut Server, sink: Sink<WifiStatusT>, _: Empty<'_>) {
        let mut this = self.borrow_mut();
        sink.set_latest(server, &this.status).unwrap();
        this.watchers.push(sink);
    }

    fn start_provisioning(
        &mut self,
        server: &mut Server,
        reply: Reply<EmptyT>,
        _: StartProvisioningRequest<'_>,
    ) {
        reply.send(server, Err(Status::UNAVAILABLE));
    }

    fn stop_provisioning(&mut self, server: &mut Server, reply: Reply<EmptyT>, _: Empty<'_>) {
        reply.send(server, Ok(&EmptyT::default()));
    }

    fn watch_provisioning(
        &mut self,
        server: &mut Server,
        sink: Sink<ProvisioningStatusT>,
        _: Empty<'_>,
    ) {
        sink.end(server, Status::UNIMPLEMENTED).unwrap();
    }

    fn cancelled(&mut self, _: &mut Server, call_id: u32) {
        self.borrow_mut().watchers.retain(|w| w.call_id() != call_id);
    }
}

const CHUNK: usize = 512;

#[derive(Default)]
struct FakeSonos {
    /// GENA subscriptions by call id.
    subscriptions: BTreeMap<u32, (String, Sink<GroupEventT>)>,
    unsubscribed: Vec<String>,
    art: Vec<u8>,
    /// Album art downloads in progress and how much each has sent.
    downloads: Vec<(Sink<AlbumArtChunkT>, usize)>,
}

impl FakeSonos {
    fn publish(&self, server: &mut Server, group: &str, volume: u8) {
        for (g, sink) in self.subscriptions.values() {
            if g == group {
                let event = GroupEventT { group: Some(g.clone()), volume, track: None };
                sink.set_latest(server, &event).unwrap();
            }
        }
    }

    /// Sends as many chunks as the clients have room for.
    fn pump(&mut self, server: &mut Server) {
        let art = &self.art;
        self.downloads.retain_mut(|(sink, sent)| {
            while *sent < art.len() && sink.credit(server).unwrap_or(0) > 0 {
                let end = (*sent + CHUNK).min(art.len());
                let chunk = AlbumArtChunkT {
                    total_size: art.len() as u32,
                    data: Some(art[*sent..end].to_vec()),
                };
                sink.send(server, &chunk).unwrap();
                *sent = end;
            }
            if *sent < art.len() {
                return true;
            }
            sink.end(server, Status::OK).unwrap();
            false
        });
    }
}

impl sonos::Handler for Rc<RefCell<FakeSonos>> {
    fn subscribe(&mut self, _: &mut Server, sink: Sink<GroupEventT>, req: SubscribeRequest<'_>) {
        let group = req.group().unwrap_or_default().to_string();
        self.borrow_mut().subscriptions.insert(sink.call_id(), (group, sink));
    }

    fn album_art(&mut self, _: &mut Server, sink: Sink<AlbumArtChunkT>, _: AlbumArtRequest<'_>) {
        self.borrow_mut().downloads.push((sink, 0));
    }

    fn cancelled(&mut self, _: &mut Server, call_id: u32) {
        let mut this = self.borrow_mut();
        if let Some((group, _)) = this.subscriptions.remove(&call_id) {
            this.unsubscribed.push(group); // UPnP UNSUBSCRIBE goes here
        }
        this.downloads.retain(|(sink, _)| sink.call_id() != call_id);
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
        let mut router = Router::new();
        router.add(wifi::Service(wifi)).add(sonos::Service(sonos.clone()));
        Self { router, sonos }
    }
}

impl ServerApp for Coprocessor {
    fn handle(&mut self, server: &mut Server, event: ServerEvent) {
        self.router.handle(server, event);
    }

    fn tick(&mut self, server: &mut Server) {
        self.sonos.borrow_mut().pump(server);
    }
}

fn sim() -> Sim<Coprocessor> {
    Sim::new(Coprocessor::new(), 1, 0, 0)
}

#[test]
fn method_ids_are_hashes_of_full_names() {
    assert_eq!(wifi::CONNECT, method_id("CoprocessorProto.Wifi/Connect"));
    assert_eq!(sonos::ALBUM_ART, method_id("CoprocessorProto.Sonos/AlbumArt"));
}

#[test]
fn connect_is_reported_on_watch() {
    let mut sim = sim();
    let watch = wifi::Client(&mut sim.client).watch(&EmptyT::default(), 1);
    let next_status = |sim: &mut Sim<Coprocessor>| {
        let mut got = None;
        sim.run_until(1_000, |_| {
            got = watch.try_recv();
            got.is_some()
        });
        got.unwrap().unwrap()
    };
    assert!(!next_status(&mut sim).get().connected());

    let req = WifiConnectRequestT { ssid: Some("home".into()), password: Some("pw".into()) };
    let call = wifi::Client(&mut sim.client).connect(&req);
    sim.run_until(1_000, |_| call.try_result().is_some());
    assert!(call.try_result().unwrap().is_ok());

    let status = next_status(&mut sim);
    assert!(status.get().connected());
    assert_eq!(status.get().ssid(), Some("home"));
    assert_eq!(status.get().ip(), Some("10.0.0.7"));
}

#[test]
fn handler_status_reaches_the_caller() {
    let mut sim = sim();
    let call = wifi::Client(&mut sim.client)
        .start_provisioning(&StartProvisioningRequestT { timeout_seconds: 60 });
    sim.run_until(1_000, |_| call.try_result().is_some());
    assert_eq!(call.try_result().unwrap().err(), Some(Status::UNAVAILABLE));
}

#[test]
fn dropping_a_subscription_unsubscribes_only_that_group() {
    let mut sim = sim();
    let subscribe = |sim: &mut Sim<Coprocessor>, group: &str| {
        let req = SubscribeRequestT { group: Some(group.into()) };
        sonos::Client(&mut sim.client).subscribe(&req, 1)
    };
    let living = subscribe(&mut sim, "LIVING_ROOM");
    let kitchen = subscribe(&mut sim, "KITCHEN");
    let sonos = sim.app.sonos.clone();
    sim.run_until(1_000, |_| sonos.borrow().subscriptions.len() == 2);

    sonos.borrow().publish(&mut sim.server, "LIVING_ROOM", 30);
    sonos.borrow().publish(&mut sim.server, "KITCHEN", 12);
    sim.run(50);
    let living_event = living.try_recv().unwrap().unwrap();
    assert_eq!(living_event.get().group(), Some("LIVING_ROOM"));
    assert_eq!(living_event.get().volume(), 30);
    assert_eq!(kitchen.try_recv().unwrap().unwrap().get().volume(), 12);

    drop(living);
    sim.run_until(1_000, |_| !sonos.borrow().unsubscribed.is_empty());
    assert_eq!(sonos.borrow().unsubscribed, ["LIVING_ROOM"]);

    sonos.borrow().publish(&mut sim.server, "KITCHEN", 13);
    sim.run(50);
    assert_eq!(kitchen.try_recv().unwrap().unwrap().get().volume(), 13);
}

#[test]
fn album_art_arrives_in_chunks_over_lossy_link() {
    for seed in 1..=10 {
        let mut sim = Sim::new(Coprocessor::new(), seed, 10, 10);
        let art = sim.app.sonos.borrow().art.clone();
        let req = AlbumArtRequestT { url: Some("http://speaker/art.jpg".into()) };
        let ch = sonos::Client(&mut sim.client).album_art(&req, 2);
        let mut got: Vec<u8> = Vec::new();
        let mut chunks = 0;
        sim.run_until(60_000, |_| {
            while let Some(chunk) = ch.try_recv() {
                let chunk = chunk.unwrap();
                assert_eq!(chunk.get().total_size() as usize, art.len());
                got.extend(chunk.get().data().unwrap().bytes());
                chunks += 1;
            }
            ch.end_status().is_some()
        });
        assert_eq!(ch.end_status(), Some(Status::OK), "seed {seed}");
        assert_eq!(got, art, "seed {seed}");
        assert_eq!(chunks, art.len().div_ceil(CHUNK));
    }
}

#[test]
fn dropping_album_art_stops_the_download() {
    let mut sim = sim();
    let req = AlbumArtRequestT { url: Some("http://speaker/art.jpg".into()) };
    let ch = sonos::Client(&mut sim.client).album_art(&req, 1);
    sim.run_until(1_000, |_| ch.try_recv().is_some());
    drop(ch);
    let sonos = sim.app.sonos.clone();
    sim.run_until(1_000, |_| sonos.borrow().downloads.is_empty());
    assert_eq!(sim.server.open_calls(), 0);
}

#[test]
fn unserved_services_are_unimplemented() {
    let mut sim = sim();
    let time = clock::Client(&mut sim.client).watch(&EmptyT::default(), 1);
    let battery = BatteryStatusT { millivolts: 3900, percent: 80, is_plugged: false };
    let call = dashboard::Client(&mut sim.client).report_battery(&battery);
    sim.run_until(1_000, |_| time.end_status().is_some() && call.try_result().is_some());
    assert_eq!(time.end_status(), Some(Status::UNIMPLEMENTED));
    assert_eq!(call.try_result().unwrap().err(), Some(Status::UNIMPLEMENTED));
}

#[test]
fn malformed_request_is_invalid_argument() {
    let mut sim = sim();
    let call = sim.client.call(wifi::CONNECT, vec![0xFF; 3], 1_000);
    sim.run_until(1_000, |_| call.try_result().is_some());
    assert_eq!(call.try_result(), Some(Err(Status::INVALID_ARGUMENT)));
}
