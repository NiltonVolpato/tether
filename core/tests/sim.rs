//! The byte-level client and server over simulated lossy pipes.

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use common::{ServerApp, Sim};
use tether::{MethodId, RawCall, RawChannel, Status, StreamError, Transport};
use tether_core::client::{Call, Channel, Client, SharedClient};
use tether_core::frame::{self, Deframer, FrameError, Header};
use tether_core::link::{LinkConfig, LinkState};
use tether_core::server::{Server, ServerEvent, SharedServer};
use tether_core::wire::{self, Kind};

/// Unary: responds with the payload reversed.
const ECHO: MethodId = MethodId::new(0, 0);
/// Streaming: sends payload[0] items numbered from 0, then ends OK.
const COUNT: MethodId = MethodId::new(0, 1);
/// Streaming, stays open; the test pushes items through `Server` directly.
const SUBSCRIBE: MethodId = MethodId::new(0, 2);
/// Unary that never responds.
const BLACK_HOLE: MethodId = MethodId::new(0, 3);

#[derive(Default)]
struct App {
    unary_calls: u32,
    /// Remaining (next, total) for COUNT streams.
    counters: BTreeMap<u32, (u8, u8)>,
    subscriptions: Vec<u32>,
    cancelled: Vec<u32>,
}

impl ServerApp for App {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
        let mut server = server.borrow_mut();
        match event {
            ServerEvent::Call { call_id, method: ECHO, payload } => {
                self.unary_calls += 1;
                server.respond(call_id, Ok(payload.into_iter().rev().collect()));
            }
            ServerEvent::Call { method: BLACK_HOLE, .. } => self.unary_calls += 1,
            ServerEvent::Call { call_id, .. } => {
                server.respond(call_id, Err(Status::Unimplemented))
            }
            ServerEvent::Open { call_id, method: COUNT, payload } => {
                self.counters.insert(call_id, (0, payload[0]));
            }
            ServerEvent::Open { call_id, method: SUBSCRIBE, .. } => {
                self.subscriptions.push(call_id);
            }
            ServerEvent::Open { call_id, .. } => {
                server.end(call_id, Err(Status::Unimplemented)).unwrap();
            }
            ServerEvent::Cancelled { call_id } => {
                self.counters.remove(&call_id);
                self.subscriptions.retain(|&id| id != call_id);
                self.cancelled.push(call_id);
            }
        }
    }

    fn tick(&mut self, server: &SharedServer) {
        let mut server = server.borrow_mut();
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
                server.end(call_id, Ok(())).unwrap();
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
        service: u8::MAX,
        method: u8::MAX,
        credit: u16::MAX,
        status: wire::Status::DATA_LOSS,
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
    assert!(matches!(sim.server.borrow().link_state(), LinkState::Linked { .. }));
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
    let call = sim.client.call(BLACK_HOLE, &[], 60_000);
    sim.run(100);
    sim.server = Server::new(0xBEEF, LinkConfig::default(), 4).shared();
    sim.run_until(1_000, |s| s.client.link_state() == LinkState::PeerRebooted);
    // The rebooted server must not have linked to the doomed client.
    assert_eq!(sim.server.borrow().link_state(), LinkState::Connecting);
    assert_eq!(call.try_result(), Some(Err(Status::Unavailable)));
}

#[test]
fn idle_link_pings_sparingly() {
    let mut sim = clean();
    sim.linked();
    sim.run(10_000);
    let pings = sim.client.link_stats().pings + sim.server.borrow().link_stats().pings;
    // A ping and its ack keep both sides quiet, so ideally one ping goes out
    // per interval (40 in 10 s). Pings cross when both sides' timers expire
    // within one latency of each other, which adds up to one more per interval.
    assert!((40..=80).contains(&pings), "{pings} pings in 10 s");
    assert!(matches!(sim.client.link_state(), LinkState::Linked { .. }));
    assert!(matches!(sim.server.borrow().link_state(), LinkState::Linked { .. }));
}

#[test]
fn idle_lossy_link_is_not_lost() {
    for seed in 1..=30 {
        let mut sim = lossy(seed, 15);
        sim.linked();
        sim.run(10_000);
        assert!(matches!(sim.client.link_state(), LinkState::Linked { .. }), "seed {seed}");
        assert!(
            matches!(sim.server.borrow().link_state(), LinkState::Linked { .. }),
            "seed {seed}"
        );
    }
}

#[test]
fn silent_server_is_lost_and_calls_fail() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, &[], 1);
    let call = sim.client.call(BLACK_HOLE, &[], 60_000);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1 && s.app.unary_calls == 1);

    sim.s2c.cut = true;
    let cut_at = sim.now;
    sim.run_until(2_000, |s| s.client.link_state() == LinkState::PeerLost);
    // An idle link: one ping interval, then the retransmits.
    let cfg = LinkConfig::default();
    let ping = frame::encode(&Header { seq: 1, ..Header::new(Kind::Ping) }, &[]).len();
    let bound =
        cfg.ping_interval_ms + (u64::from(cfg.max_retransmits) + 1) * cfg.retransmit_timeout(ping);
    assert!(sim.now - cut_at <= bound + 10, "lost after {} ms", sim.now - cut_at);

    assert_eq!(call.try_result(), Some(Err(Status::Unavailable)));
    assert_eq!(ch.end(), Some(Err(Status::Unavailable)));
    assert_eq!(sim.client.open_calls(), 0);
    let late = sim.client.call(ECHO, &[1], 1_000);
    assert_eq!(late.try_result(), Some(Err(Status::Unavailable)));
}

#[test]
fn server_cancels_calls_when_client_is_lost() {
    let mut sim = clean();
    let _ch = sim.client.open(SUBSCRIBE, &[], 1);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];

    sim.c2s.cut = true;
    sim.run_until(2_000, |s| s.server.borrow().link_state() == LinkState::PeerLost);
    sim.step();
    assert_eq!(sim.app.cancelled, [call_id]);
    assert_eq!(sim.server.borrow().open_calls(), 0);
    assert_eq!(sim.server.borrow_mut().send(call_id, vec![1]), Err(StreamError::Closed));
}

// --- Unary ---

#[test]
fn unary_call() {
    let mut sim = clean();
    let call = sim.client.call(ECHO, &[1, 2, 3], 1_000);
    assert_eq!(sim.wait(1_000, || call.try_result()), Ok(vec![3, 2, 1]));
    assert_eq!(sim.client.open_calls(), 0);
}

#[test]
fn unknown_method_returns_status() {
    let mut sim = clean();
    let call = sim.client.call(MethodId::new(0, 99), &[], 1_000);
    assert_eq!(sim.wait(1_000, || call.try_result()), Err(Status::Unimplemented));
}

#[test]
fn deadline_exceeded_cancels_server_side() {
    let mut sim = clean();
    sim.linked();
    let call = sim.client.call(BLACK_HOLE, &[], 100);
    sim.run_until(1_000, |s| !s.app.cancelled.is_empty());
    assert_eq!(call.try_result(), Some(Err(Status::DeadlineExceeded)));
    assert_eq!(sim.server.borrow().open_calls(), 0);
}

#[test]
fn unary_calls_run_exactly_once_over_lossy_link() {
    for seed in 1..=30 {
        let mut sim = lossy(seed, 15);
        let calls: Vec<_> =
            (0..20u8).map(|i| (i, sim.client.call(ECHO, &[i, i + 1], 60_000))).collect();
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

#[test]
fn dropping_call_cancels_it() {
    let mut sim = clean();
    let call = sim.client.call(BLACK_HOLE, &[], 60_000);
    sim.run_until(1_000, |s| s.app.unary_calls == 1);
    drop(call);
    sim.run_until(1_000, |s| s.app.cancelled.len() == 1);
    assert_eq!(sim.client.open_calls(), 0);
    assert_eq!(sim.server.borrow().open_calls(), 0);
}

// --- Channels ---

#[test]
fn freed_slots_of_many_channels_go_out_in_one_credit_frame() {
    let mut sim = clean();
    let channels: Vec<_> = (0..3).map(|_| sim.client.open(SUBSCRIBE, &[], 1)).collect();
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 3);
    let subscriptions = sim.app.subscriptions.clone();
    for &call_id in &subscriptions {
        sim.server.borrow_mut().send(call_id, vec![1]).unwrap();
    }
    sim.run(100);

    sim.c2s.tap = Some(Vec::new());
    for ch in &channels {
        assert_eq!(ch.try_recv(), Some(vec![1]));
    }
    sim.run(100);
    let mut deframer = Deframer::new(1024);
    let credits: Vec<_> = (sim.c2s.tap.take().unwrap().concat().into_iter())
        .filter_map(|b| deframer.push(b)?.ok())
        .filter(|f| f.header.kind == Kind::Credit)
        .collect();
    assert_eq!(credits.len(), 1);
    let grants: Vec<_> = (flatbuffers::root::<wire::Credits>(&credits[0].payload).unwrap())
        .grants()
        .unwrap()
        .iter()
        .map(|g| (g.call_id(), g.credit()))
        .collect();
    assert_eq!(grants, subscriptions.iter().map(|&id| (id, 1)).collect::<Vec<_>>());
    for id in subscriptions {
        assert_eq!(sim.server.borrow().credit(id), Some(1));
    }
}

#[test]
fn stream_delivers_in_order_with_slow_consumer_over_lossy_link() {
    for seed in 1..=30 {
        let mut sim = lossy(seed, 10);
        let ch = sim.client.open(COUNT, &[200], 2);
        let mut got = Vec::new();
        sim.run_until(120_000, |s| {
            if s.now % 7 == 0 {
                got.extend(ch.try_recv().into_iter().flatten());
            }
            got.len() == 200
        });
        got.extend(drain(&ch));
        assert_eq!(got, (0..200).collect::<Vec<u8>>(), "seed {seed}");
        sim.run_until(1_000, |_| ch.end().is_some());
        assert_eq!(ch.end(), Some(Ok(())));
        assert_eq!(sim.client.stats().overruns, 0, "server ignored credit");
    }
}

#[test]
fn dropping_channel_unsubscribes() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, &[], 1);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];
    sim.server.borrow_mut().send(call_id, vec![42]).unwrap();
    sim.wait(1_000, || ch.try_recv());

    drop(ch);
    sim.run_until(1_000, |s| s.app.cancelled == [call_id]);
    assert_eq!(sim.server.borrow_mut().send(call_id, vec![43]), Err(StreamError::Closed));
    assert_eq!(sim.server.borrow().open_calls(), 0);
    assert_eq!(sim.client.open_calls(), 0);
    assert_eq!(sim.client.stats().stale_items, 0);
}

#[test]
fn item_racing_the_cancel_is_dropped() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, &[], 4);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];

    // The server sends before it can know about the drop.
    drop(ch);
    sim.server.borrow_mut().send(call_id, vec![1]).unwrap();
    sim.run_until(1_000, |s| s.app.cancelled == [call_id]);
    sim.run(200);
    let stats = sim.client.stats();
    assert_eq!((stats.stale_items, stats.cancels), (1, 1));
    assert_eq!(sim.client.open_calls(), 0);
    assert_eq!(sim.server.borrow().open_calls(), 0);
}

#[test]
fn latest_value_coalesces_while_consumer_is_busy() {
    let mut sim = clean();
    let ch = sim.client.open(SUBSCRIBE, &[], 1);
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    let call_id = sim.app.subscriptions[0];

    for v in 0..100u8 {
        sim.server.borrow_mut().set_latest(call_id, vec![v]).unwrap();
        sim.step();
    }
    sim.run(100);
    // The first value used the only credit; the rest collapsed into the last.
    assert_eq!(ch.try_recv(), Some(vec![0]));
    assert_eq!(sim.wait(1_000, || ch.try_recv()), vec![99]);
    sim.run(100);
    assert_eq!(ch.try_recv(), None);
}

#[test]
fn stream_limit_returns_resource_exhausted() {
    let mut sim = clean();
    let channels: Vec<_> = (0..5).map(|_| sim.client.open(SUBSCRIBE, &[], 1)).collect();
    sim.run_until(1_000, |_| channels[4].end().is_some());
    assert_eq!(channels[4].end(), Some(Err(Status::ResourceExhausted)));
    assert!(channels[..4].iter().all(|c| c.end().is_none()));
}

#[test]
fn poll_recv_wakes_on_item() {
    struct CountWakes(AtomicU32);
    impl Wake for CountWakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut sim = clean();
    let mut ch = sim.client.open(SUBSCRIBE, &[], 1);
    let wakes = Arc::new(CountWakes(AtomicU32::new(0)));
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);

    assert!(ch.poll_recv(&mut cx).is_pending());
    sim.run_until(1_000, |s| s.app.subscriptions.len() == 1);
    sim.server.borrow_mut().send(sim.app.subscriptions[0], vec![7]).unwrap();
    sim.run_until(1_000, |_| wakes.0.load(Ordering::Relaxed) > 0);
    assert_eq!(ch.poll_recv(&mut cx), Poll::Ready(Some(vec![7])));
}

#[test]
fn a_waker_can_use_the_client_at_once() {
    // A waker that polls its task right away: it reads its call's result and
    // starts another call, while the client is still in `poll_transmit`.
    type Task = (Rc<SharedClient>, Call, Option<Result<Vec<u8>, Status>>);
    thread_local! {
        static TASK: RefCell<Option<Task>> = const { RefCell::new(None) };
    }
    struct PollAtOnce;
    impl Wake for PollAtOnce {
        fn wake(self: Arc<Self>) {
            TASK.with_borrow_mut(|task| {
                let (client, call, result) = task.as_mut().unwrap();
                *result = call.try_result();
                drop(client.call(ECHO, &[], 1_000));
            });
        }
    }

    let client = Rc::new(SharedClient::new(Client::new(0xC1, LinkConfig::default())));
    let mut call = client.call(BLACK_HOLE, &[], 10);
    let waker = Waker::from(Arc::new(PollAtOnce));
    assert!(call.poll_result(&mut Context::from_waker(&waker)).is_pending());
    TASK.set(Some((client.clone(), call, None)));
    client.poll_transmit(100);
    let (_, _, result) = TASK.take().unwrap();
    assert_eq!(result, Some(Err(Status::DeadlineExceeded)));
}
