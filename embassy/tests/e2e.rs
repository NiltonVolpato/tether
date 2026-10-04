//! The client and the server, each with its I/O task from tether-embassy, and
//! apps on the same executor, over simulated lossy pipes. Time is embassy's mock
//! clock, advanced a millisecond at a time whenever every task is waiting.

extern crate alloc;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, PoisonError};

use embassy_futures::select::select;
use embassy_time::{Duration, Instant, MockDriver, Timer};
use embedded_io_async::{ErrorType, Read, Write};
use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;
use tether::{MethodId, RawCall, RawChannel, Status, Transport};
use tether_core::client::{Client, SharedClient};
use tether_core::link::{LinkConfig, LinkState};
use tether_core::notify::Notify;
use tether_core::server::{Server, ServerEvent, SharedServer};
use tether_embassy::{ServerApp, run_client, run_server};

const ECHO: MethodId = MethodId::new(0, 0);
const COUNTDOWN: MethodId = MethodId::new(0, 1);

// --- A lossy pipe: one direction of a UART.

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        // xorshift64*, as in the other simulations.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

struct Wire {
    rng: Rng,
    loss_pct: u64,
    /// Drops everything: a disconnected wire or a hung peer.
    cut: bool,
    /// Frames written to it.
    writes: usize,
    in_flight: VecDeque<(u64, Vec<u8>)>,
    /// Arrived, and not read yet.
    ready: VecDeque<u8>,
}

#[derive(Clone)]
struct Pipe {
    wire: Rc<RefCell<Wire>>,
    /// Notified when something is written.
    arrived: Rc<Notify>,
}

fn pipe(seed: u64, loss_pct: u64) -> Pipe {
    let wire = Rc::new(RefCell::new(Wire {
        rng: Rng(seed | 1),
        loss_pct,
        cut: false,
        writes: 0,
        in_flight: VecDeque::new(),
        ready: VecDeque::new(),
    }));
    Pipe { wire, arrived: Rc::new(Notify::default()) }
}

impl ErrorType for Pipe {
    type Error = Infallible;
}

impl Write for Pipe {
    /// A write is a frame: it's dropped or corrupted whole, and arrives a few
    /// milliseconds later.
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Infallible> {
        let mut wire = self.wire.borrow_mut();
        wire.writes += 1;
        let lost = wire.cut || wire.rng.below(100) < wire.loss_pct;
        if !lost {
            let mut bytes = buf.to_vec();
            if wire.rng.below(100) < wire.loss_pct {
                let (i, bit) = (wire.rng.below(bytes.len() as u64), wire.rng.below(8));
                bytes[i as usize] ^= 1 << bit;
            }
            let at = Instant::now().as_millis() + 1 + wire.rng.below(3);
            wire.in_flight.push_back((at, bytes));
            self.arrived.notify();
        }
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

impl Read for Pipe {
    /// Whatever has arrived, in chunks of a random size.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
        loop {
            let next = {
                let mut wire = self.wire.borrow_mut();
                let now = Instant::now().as_millis();
                while wire.in_flight.front().is_some_and(|(at, _)| *at <= now) {
                    let (_, bytes) = wire.in_flight.pop_front().unwrap();
                    wire.ready.extend(bytes);
                }
                if !wire.ready.is_empty() {
                    let n = (1 + wire.rng.below(64) as usize).min(buf.len()).min(wire.ready.len());
                    for slot in &mut buf[..n] {
                        *slot = wire.ready.pop_front().unwrap();
                    }
                    return Ok(n);
                }
                wire.in_flight.front().map(|(at, _)| *at)
            };
            match next {
                Some(at) => {
                    select(Timer::at(Instant::from_millis(at)), self.arrived.notified()).await;
                }
                None => self.arrived.notified().await,
            }
        }
    }
}

// --- What runs on the server.

#[derive(Default)]
struct App {
    /// Channels being served: call id -> the next number.
    countdowns: BTreeMap<u32, u8>,
    cancelled: Rc<RefCell<Vec<u32>>>,
    answers: bool,
}

impl ServerApp for App {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
        match event {
            ServerEvent::Call { call_id, payload, .. } => {
                if self.answers {
                    server.respond(call_id, Ok(payload));
                }
            }
            ServerEvent::Open { call_id, payload, .. } => {
                self.countdowns.insert(call_id, payload[0]);
            }
            ServerEvent::Cancelled { call_id } => {
                self.countdowns.remove(&call_id);
                self.cancelled.borrow_mut().push(call_id);
            }
        }
    }

    /// Sends the items channels have credit for, and ends the finished ones.
    fn tick(&mut self, server: &SharedServer) {
        let ids: Vec<u32> = self.countdowns.keys().copied().collect();
        for id in ids {
            while let Some(left) = self.countdowns.get_mut(&id) {
                if *left == 0 {
                    let _ = server.end(id, Ok(()));
                    self.countdowns.remove(&id);
                } else if server.send(id, vec![*left]).is_ok() {
                    *left -= 1;
                } else {
                    break;
                }
            }
        }
    }
}

/// The mock clock is one for the whole process, so a test at a time has it.
static CLOCK: Mutex<()> = Mutex::new(());

struct Rig {
    _clock: MutexGuard<'static, ()>,
    pool: LocalPool,
    client: Rc<SharedClient>,
    server: SharedServer,
    c2s: Pipe,
    s2c: Pipe,
    cancelled: Rc<RefCell<Vec<u32>>>,
    /// How the I/O tasks ended.
    exits: Rc<RefCell<Vec<(&'static str, LinkState)>>>,
}

impl Rig {
    fn new(seed: u64, loss_pct: u64) -> Self {
        let clock = CLOCK.lock().unwrap_or_else(PoisonError::into_inner);
        MockDriver::get().reset();
        let cfg = LinkConfig::default();
        let client = Rc::new(SharedClient::new(Client::new(0xC11E_0000 ^ seed as u32 | 1, cfg)));
        let server = Server::new(0x5E4E_0000 ^ seed as u32 | 1, cfg, 4).shared();
        let (c2s, s2c) = (pipe(seed * 3, loss_pct), pipe(seed * 7, loss_pct));
        let cancelled = Rc::new(RefCell::new(Vec::new()));
        let exits = Rc::new(RefCell::new(Vec::new()));
        let pool = LocalPool::new();
        let spawner = pool.spawner();
        {
            let (client, exits, rx, tx) = (client.clone(), exits.clone(), s2c.clone(), c2s.clone());
            spawner
                .spawn_local(async move {
                    let state = run_client(&client, rx, tx).await.unwrap();
                    exits.borrow_mut().push(("client", state));
                })
                .unwrap();
        }
        {
            let (server, exits, rx, tx) = (server.clone(), exits.clone(), c2s.clone(), s2c.clone());
            let app =
                RefCell::new(App { cancelled: cancelled.clone(), answers: true, ..App::default() });
            spawner
                .spawn_local(async move {
                    let state = run_server(&server, &app, rx, tx).await.unwrap();
                    exits.borrow_mut().push(("server", state));
                })
                .unwrap();
        }
        Self { _clock: clock, pool, client, server, c2s, s2c, cancelled, exits }
    }

    fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        self.pool.spawner().spawn_local(task).unwrap();
    }

    /// Runs every task until they're all waiting, then lets a millisecond pass,
    /// until `done`; fails after `max_ms` of that.
    fn run_until(&mut self, max_ms: u64, mut done: impl FnMut() -> bool) {
        let start = Instant::now().as_millis();
        loop {
            self.pool.run_until_stalled();
            if done() {
                return;
            }
            let elapsed = Instant::now().as_millis() - start;
            assert!(elapsed < max_ms, "not done within {max_ms} ms");
            MockDriver::get().advance(Duration::from_millis(1));
        }
    }

    fn linked(&mut self) {
        let (client, server) = (self.client.clone(), self.server.clone());
        self.run_until(5_000, move || {
            matches!(client.link_state(), LinkState::Linked { .. })
                && matches!(server.link_state(), LinkState::Linked { .. })
        });
    }
}

/// A unary call, awaited.
async fn call(
    client: &SharedClient,
    method: MethodId,
    request: Vec<u8>,
) -> Result<Vec<u8>, Status> {
    let mut call = client.call(method, &request, 60_000);
    poll_fn(|cx| call.poll_result(cx)).await
}

#[test]
fn apps_call_and_stream_over_lossy_links() {
    for seed in 1..=10 {
        let mut rig = Rig::new(seed, 15);
        rig.linked();
        let results = Rc::new(RefCell::new(Vec::new()));
        for i in 0..30u8 {
            let (client, results) = (rig.client.clone(), results.clone());
            rig.spawn(async move {
                let payload: Vec<u8> = (0..(i as usize * 37) % 400).map(|b| b as u8 ^ i).collect();
                let result = call(&client, ECHO, payload.clone()).await;
                results.borrow_mut().push((result, payload));
            });
        }
        let numbers = Rc::new(RefCell::new(Vec::new()));
        let end = Rc::new(RefCell::new(None));
        {
            let (client, numbers, end) = (rig.client.clone(), numbers.clone(), end.clone());
            rig.spawn(async move {
                let mut channel = client.open(COUNTDOWN, &[100], 4);
                while let Some(item) = poll_fn(|cx| channel.poll_recv(cx)).await {
                    numbers.borrow_mut().push(item[0]);
                }
                *end.borrow_mut() = channel.end();
            });
        }
        let (results2, end2) = (results.clone(), end.clone());
        rig.run_until(120_000, move || results2.borrow().len() == 30 && end2.borrow().is_some());
        for (result, payload) in results.borrow().iter() {
            assert_eq!(result.as_ref(), Ok(payload), "seed {seed}");
        }
        assert_eq!(*end.borrow(), Some(Ok(())), "seed {seed}");
        assert_eq!(*numbers.borrow(), (1..=100).rev().collect::<Vec<u8>>(), "seed {seed}");
    }
}

#[test]
fn consuming_an_item_grants_credit_at_once() {
    // A channel with room for one waits for the client to take each item. If
    // the I/O task slept until its next deadline (a ping, every 250 ms) that
    // would take 100 round trips of that; woken by the app, it takes the link's.
    let mut rig = Rig::new(1, 0);
    rig.linked();
    let start = Instant::now().as_millis();
    let numbers = Rc::new(RefCell::new(0));
    {
        let (client, numbers) = (rig.client.clone(), numbers.clone());
        rig.spawn(async move {
            let mut channel = client.open(COUNTDOWN, &[100], 1);
            while poll_fn(|cx| channel.poll_recv(cx)).await.is_some() {
                *numbers.borrow_mut() += 1;
            }
        });
    }
    let done = numbers.clone();
    rig.run_until(120_000, move || *done.borrow() == 100);
    let took = Instant::now().as_millis() - start;
    assert!(took < 3_000, "{took} ms");
}

#[test]
fn dropping_a_call_cancels_it_at_once() {
    let mut rig = Rig::new(1, 0);
    rig.linked();
    let channel = rig.client.open(COUNTDOWN, &[255], 1);
    let done = rig.cancelled.clone();
    // The first item arrives; the server is now waiting for credit.
    rig.run_until(5_000, {
        let channel = &channel;
        move || channel.try_recv().is_some()
    });
    let dropped = Instant::now().as_millis();
    drop(channel);
    rig.run_until(5_000, move || !done.borrow().is_empty());
    // Within the link's round trip, not at the next ping.
    let took = Instant::now().as_millis() - dropped;
    assert!(took < 100, "{took} ms");
}

#[test]
fn an_idle_link_only_pings() {
    let mut rig = Rig::new(1, 0);
    rig.linked();
    let start = Instant::now().as_millis();
    rig.run_until(60_000, || Instant::now().as_millis() >= start + 10_000);
    // A ping each way every 250 ms, and their acks: about 160, not a write a
    // millisecond.
    let writes = rig.c2s.wire.borrow().writes + rig.s2c.wire.borrow().writes;
    assert!(writes < 400, "{writes} writes");
    assert!(matches!(rig.client.link_state(), LinkState::Linked { .. }));
}

#[test]
fn the_tasks_end_with_the_link() {
    let mut rig = Rig::new(1, 0);
    rig.linked();
    // One way only: each side's frames to the other still arrive, but acks
    // don't, so each side loses the other in its own time.
    rig.s2c.wire.borrow_mut().cut = true;
    let exits = rig.exits.clone();
    rig.run_until(10_000, move || exits.borrow().len() == 2);
    let mut exits = rig.exits.borrow().clone();
    exits.sort_by_key(|(who, _)| *who);
    assert_eq!(exits, [("client", LinkState::PeerLost), ("server", LinkState::PeerLost)]);
    // The client's calls fail, rather than waiting for ever.
    let result = rig.client.call(ECHO, &[], 1_000).try_result();
    assert_eq!(result, Some(Err(Status::Unavailable)));
}
