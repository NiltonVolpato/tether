//! The byte-level client and server over simulated lossy pipes.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use common::{ServerApp, Sim};
use rpc_experiment::client::{Channel, Client};
use rpc_experiment::frame::{self, Deframer, FrameError, Header};
use rpc_experiment::link::{LinkConfig, LinkState};
use rpc_experiment::method_id;
use rpc_experiment::proto::{Kind, Status};
use rpc_experiment::server::{Server, ServerEvent, StreamError};

/// Unary: responds with the payload reversed.
const ECHO: u32 = method_id("Test.Svc/Echo");
/// Streaming: sends payload[0] items numbered from 0, then ends OK.
const COUNT: u32 = method_id("Test.Svc/Count");
/// Streaming, stays open; the test pushes items through `Server` directly.
const SUBSCRIBE: u32 = method_id("Test.Svc/Subscribe");
/// Unary that never responds.
const BLACK_HOLE: u32 = method_id("Test.Svc/BlackHole");

#[derive(Default)]
struct App {
    unary_calls: u32,
    /// Remaining (next, total) for COUNT streams.
    counters: BTreeMap<u32, (u8, u8)>,
    subscriptions: Vec<u32>,
    cancelled: Vec<u32>,
}

impl ServerApp for App {
    fn handle(&mut self, server: &mut Server, event: ServerEvent) {
        match event {
            ServerEvent::Call { call_id, method: ECHO, payload } => {
                self.unary_calls += 1;
                server.respond(call_id, Ok(payload.into_iter().rev().collect()));
            }
            ServerEvent::Call { method: BLACK_HOLE, .. } => self.unary_calls += 1,
            ServerEvent::Call { call_id, .. } => {
                server.respond(call_id, Err(Status::UNIMPLEMENTED))
            }
            ServerEvent::Open { call_id, method: COUNT, payload } => {
                self.counters.insert(call_id, (0, payload[0]));
            }
            ServerEvent::Open { call_id, method: SUBSCRIBE, .. } => {
                self.subscriptions.push(call_id);
            }
            ServerEvent::Open { call_id, .. } => {
                server.end(call_id, Status::UNIMPLEMENTED).unwrap();
            }
            ServerEvent::Cancelled { call_id } => {
                self.counters.remove(&call_id);
                self.subscriptions.retain(|&id| id != call_id);
                self.cancelled.push(call_id);
            }
        }
    }

    fn tick(&mut self, server: &mut Server) {
        let mut done = Vec::new();
        for (&call_id, (next, total)) in &mut self.counters {
            while *next < *total {
                match server.send(call_id, vec![*next]) {
                    Ok(()) => *next += 1,
                    Err(StreamError::NoCredit) => break,
                    Err(StreamError::Closed) => unreachable!("cancel removes the counter"),
                }
            }
            if next == total {
                server.end(call_id, Status::OK).unwrap();
                done.push(call_id);
            }
        }
        for id in done {
            self.counters.remove(&id);
        }
    }
}

fn clean() -> Sim<App> {
    Sim::new(App::default(), 1, 0, 0)
}

fn lossy(seed: u64, pct: u64) -> Sim<App> {
    Sim::new(App::default(), seed, pct, pct)
}

fn drain(ch: &Channel) -> Vec<u8> {
    std::iter::from_fn(|| ch.try_recv()).flatten().collect()
}

// --- Framing ---

#[test]
fn frame_roundtrip_and_alignment() {
    let header = Header {
        kind: Kind::Item,
        seq: u16::MAX,
        call_id: u32::MAX,
        method: u32::MAX,
        credit: u16::MAX,
        status: Status::DATA_LOSS,
    };
    for len in [0, 1, 7, 8, 9, 511] {
        let payload: Vec<u8> = (0..len).map(|i| i as u8 | 1).collect();
        let wire = frame::encode(&header, &payload);
        assert_eq!(wire.last(), Some(&0));
        assert_eq!(wire.iter().filter(|&&b| b == 0).count(), 1);
        let decoded = frame::decode(&wire[..wire.len() - 1]).unwrap();
        assert_eq!(decoded.header, header);
        assert_eq!(decoded.payload, payload);

        let body = cobs::decode_vec(&wire[..wire.len() - 1]).unwrap();
        let hdr_len = 4 + u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
        assert_eq!(hdr_len % 4, 0, "header must keep 4-byte alignment after the CRC");
        if len > 0 {
            assert_eq!((body.len() - len) % 8, 0, "payload must start 8-aligned");
        }
    }
}

#[test]
fn deframer_resyncs_after_garbage_and_overflow() {
    let good = frame::encode(&Header::new(Kind::Cancel), &[1, 2, 3]);
    let mut bytes = vec![0x55, 0x66, 0x00];
    bytes.extend(std::iter::repeat_n(0xAA, 2000));
    bytes.push(0);
    bytes.extend(&good);

    let mut d = Deframer::new(1024);
    let results: Vec<_> = bytes.iter().filter_map(|&b| d.push(b)).collect();
    assert_eq!(results.len(), 3);
    assert!(results[0].is_err());
    assert_eq!(results[1], Err(FrameError::Overflow));
    assert_eq!(results[2].as_ref().unwrap().payload, vec![1, 2, 3]);
}

// --- Link ---

#[test]
fn lost_hello_reply_is_not_a_reboot() {
    let mut sim = clean();
    sim.s2c.drop_first = 3;
    sim.linked();
    sim.run(500);
    assert!(matches!(sim.client.link_state(), LinkState::Linked { .. }));
    assert!(matches!(sim.server.link_state(), LinkState::Linked { .. }));
}

#[test]
fn queued_hellos_from_one_boot_link_once() {
    // The S3 keeps retrying Hello while the co-processor reboots, so it comes
    // up to several of them queued. Only a new boot id means another reboot.
    let cfg = LinkConfig::default();
    let mut client = Client::new(0xC2, cfg);
    let hellos: Vec<_> = (0..4)
        .map(|i| client.poll_transmit(i * cfg.hello_interval_ms).unwrap())
        .collect();
    let mut server = Server::new(0x52, cfg, 4);
    for hello in &hellos {
        server.receive(hello);
    }
    assert_eq!(server.link_state(), LinkState::Linked { peer_boot_id: 0xC2 });
}

#[test]
fn peer_reboot_is_detected() {
    let mut sim = clean();
    sim.linked();
    sim.server = Server::new(0xBEEF, LinkConfig::default(), 4);
    sim.run_until(1_000, |s| s.client.link_state() == LinkState::PeerRebooted);
    // The rebooted server must not have linked to the doomed client.
    assert_eq!(sim.server.link_state(), LinkState::Connecting);
}

// --- Unary ---

#[test]
fn unary_call() {
    let mut sim = clean();
    let call = sim.client.call(ECHO, vec![1, 2, 3], 1_000);
    sim.run_until(1_000, |_| call.try_result().is_some());
    assert_eq!(call.try_result(), Some(Ok(vec![3, 2, 1])));
    assert_eq!(sim.client.open_calls(), 0);
}

#[test]
fn unknown_method_returns_status() {
    let mut sim = clean();
    let call = sim.client.call(99, vec![], 1_000);
    sim.run_until(1_000, |_| call.try_result().is_some());
    assert_eq!(call.try_result(), Some(Err(Status::UNIMPLEMENTED)));
}

#[test]
fn deadline_exceeded_cancels_server_side() {
    let mut sim = clean();
    sim.linked();
    let call = sim.client.call(BLACK_HOLE, vec![], 100);
    sim.run_until(1_000, |s| !s.app.cancelled.is_empty());
    assert_eq!(call.try_result(), Some(Err(Status::DEADLINE_EXCEEDED)));
    assert_eq!(sim.server.open_calls(), 0);
}

#[test]
fn unary_calls_run_exactly_once_over_lossy_link() {
    for seed in 1..=30 {
        let mut sim = lossy(seed, 15);
        let calls: Vec<_> =
            (0..20u8).map(|i| (i, sim.client.call(ECHO, vec![i, i + 1], 60_000))).collect();
        sim.run_until(60_000, |_| calls.iter().all(|(_, c)| c.try_result().is_some()));
        for (i, call) in &calls {
            assert_eq!(call.try_result(), Some(Ok(vec![i + 1, *i])), "seed {seed}");
        }
        assert_eq!(sim.app.unary_calls, 20, "seed {seed}: duplicate or lost call");
        let stats = sim.client.link_stats();
        assert!(stats.retransmits > 0 && stats.crc_errors + stats.cobs_errors > 0);
        assert_ne!(sim.client.link_state(), LinkState::PeerRebooted);
    }
}

// --- Channels ---

#[test]
fn stream_delivers_in_order_with_slow_consumer_over_lossy_link() {
    for seed in 1..=30 {
        let mut sim = lossy(seed, 10);
        let ch = sim.client.open(COUNT, vec![200], 2);
        let mut got = Vec::new();
        sim.run_until(120_000, |s| {
            if s.now % 7 == 0 {
                got.extend(ch.try_recv().into_iter().flatten());
            }
            ch.end_status().is_some() && got.len() == 200
        });
        got.extend(drain(&ch));
        assert_eq!(got, (0..200).collect::<Vec<u8>>(), "seed {seed}");
        assert_eq!(ch.end_status(), Some(Status::OK));
        assert_eq!(sim.client.stats().overruns, 0, "server ignored credit");
    }
}

#[test]
fn dropping_channel_unsubscribes() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, vec![], 1);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];
    sim.server.send(call_id, vec![42]).unwrap();
    sim.run_until(1_000, |_| ch.try_recv().is_some());

    drop(ch);
    sim.run_until(1_000, |s| s.app.cancelled == [call_id]);
    assert_eq!(sim.server.send(call_id, vec![43]), Err(StreamError::Closed));
    assert_eq!(sim.server.open_calls(), 0);
    assert_eq!(sim.client.open_calls(), 0);
    assert_eq!(sim.client.stats().stale_items, 0);
}

#[test]
fn item_racing_the_cancel_is_dropped() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, vec![], 4);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];

    // The server sends before it can know about the drop.
    drop(ch);
    sim.server.send(call_id, vec![1]).unwrap();
    sim.run_until(1_000, |s| s.app.cancelled == [call_id]);
    sim.run(200);
    assert_eq!(sim.client.stats().stale_items, 1);
    assert_eq!(sim.client.stats().cancels, 1);
    assert_eq!(sim.client.open_calls(), 0);
    assert_eq!(sim.server.open_calls(), 0);
}

#[test]
fn latest_value_coalesces_while_consumer_is_busy() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, vec![], 1);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];

    for v in 0..100u8 {
        sim.server.set_latest(call_id, vec![v]).unwrap();
        sim.step();
    }
    sim.run(100);
    // The first value used the only credit; the rest collapsed into the last.
    assert_eq!(ch.try_recv(), Some(vec![0]));
    sim.run_until(1_000, |_| ch.try_recv() == Some(vec![99]));
    sim.run(100);
    assert_eq!(ch.try_recv(), None);
}

#[test]
fn stream_limit_returns_resource_exhausted() {
    let mut sim = clean();
    let channels: Vec<_> = (0..5).map(|_| sim.client.open(SUBSCRIBE, vec![], 1)).collect();
    sim.run_until(1_000, |_| channels[4].end_status().is_some());
    assert_eq!(channels[4].end_status(), Some(Status::RESOURCE_EXHAUSTED));
    assert!(channels[..4].iter().all(|c| c.end_status().is_none()));
}

#[test]
fn async_recv_wakes_on_item() {
    struct CountWakes(AtomicU32);
    impl Wake for CountWakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, vec![], 1);
    let wakes = Arc::new(CountWakes(AtomicU32::new(0)));
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(ch.recv());

    assert!(fut.as_mut().poll(&mut cx).is_pending());
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    sim.server.send(sim.app.subscriptions[0], vec![7]).unwrap();
    sim.run_until(1_000, |_| wakes.0.load(Ordering::Relaxed) > 0);
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Some(vec![7])));
}
