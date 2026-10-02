//! The client and the server driven the way an I/O task drives them: only when
//! bytes arrive, or at `next_deadline`, never by polling every millisecond (the
//! other simulations do). A deadline that was late would stall these.

extern crate alloc;

mod common;

use std::collections::BTreeMap;

use common::Pipe;
use tether::{MethodId, RawChannel, Status};
use tether_core::client::{Channel, Client};
use tether_core::link::{LinkConfig, LinkState};
use tether_core::server::{Server, ServerEvent};

const ECHO: MethodId = MethodId::new(0, 0);
const COUNTDOWN: MethodId = MethodId::new(0, 1);

struct Sim {
    now: u64,
    client: Client,
    server: Server,
    c2s: Pipe,
    s2c: Pipe,
    /// Channels the server serves: call id -> items left.
    countdowns: BTreeMap<u32, u8>,
    /// How many times the I/O task woke up.
    wakeups: u64,
    /// Whether the server answers calls.
    answers: bool,
}

impl Sim {
    fn new(seed: u64, loss_pct: u64) -> Self {
        let cfg = LinkConfig::default();
        Self {
            now: 0,
            client: Client::new(0xC11E_0000 ^ seed as u32 | 1, cfg),
            server: Server::new(0x5E4E_0000 ^ seed as u32 | 1, cfg, 4),
            c2s: Pipe::new(seed.wrapping_mul(3), loss_pct, loss_pct),
            s2c: Pipe::new(seed.wrapping_mul(7), loss_pct, loss_pct),
            countdowns: BTreeMap::new(),
            wakeups: 0,
            answers: true,
        }
    }

    /// The next time anything is due.
    fn next(&self) -> Option<u64> {
        [
            self.client.next_deadline(),
            self.server.next_deadline(),
            self.c2s.next_arrival(),
            self.s2c.next_arrival(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Jumps to the next arrival or deadline, and does what's due there.
    fn step(&mut self) {
        let next = self.next().expect("a live link always has a deadline");
        self.now = next.max(self.now + 1);
        self.wakeups += 1;
        for chunk in self.c2s.deliver(self.now) {
            self.server.receive(&chunk);
        }
        for chunk in self.s2c.deliver(self.now) {
            self.client.receive(&chunk);
        }
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::Call { call_id, payload, .. } if self.answers => {
                    self.server.respond(call_id, Ok(payload));
                }
                ServerEvent::Call { .. } => {}
                ServerEvent::Open { call_id, payload, .. } => {
                    self.countdowns.insert(call_id, payload[0]);
                }
                ServerEvent::Cancelled { call_id } => {
                    self.countdowns.remove(&call_id);
                }
            }
        }
        self.serve_countdowns();
        self.transmit();
    }

    /// Sends the items channels have credit for, and ends the finished ones.
    fn serve_countdowns(&mut self) {
        let ids: Vec<u32> = self.countdowns.keys().copied().collect();
        for id in ids {
            while let Some(left) = self.countdowns.get_mut(&id) {
                if *left == 0 {
                    let _ = self.server.end(id, Ok(()));
                    self.countdowns.remove(&id);
                } else if self.server.send(id, vec![*left]).is_ok() {
                    *left -= 1;
                } else {
                    break;
                }
            }
        }
    }

    fn transmit(&mut self) {
        while let Some(b) = self.client.poll_transmit(self.now) {
            self.c2s.push(self.now, b);
        }
        while let Some(b) = self.server.poll_transmit(self.now) {
            self.s2c.push(self.now, b);
        }
    }

    /// Steps until `done`, which is also where the app acts and notifies the
    /// I/O task: it's woken at once, as a notification does.
    fn run_until(&mut self, max_ms: u64, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = self.now + max_ms;
        self.transmit();
        while !done(self) {
            self.transmit();
            assert!(self.now < deadline, "not done within {max_ms} ms");
            self.step();
        }
    }

    fn linked(&mut self) {
        self.run_until(5_000, |s| {
            matches!(s.client.link_state(), LinkState::Linked { .. })
                && matches!(s.server.link_state(), LinkState::Linked { .. })
        });
    }
}

#[test]
fn calls_complete_over_a_lossy_link() {
    for seed in 1..=20 {
        let mut sim = Sim::new(seed, 15);
        sim.linked();
        let calls: Vec<_> = (0..30u8)
            .map(|i| {
                let payload: Vec<u8> = (0..(i as usize * 37) % 400).map(|b| b as u8 ^ i).collect();
                (sim.client.call(ECHO, payload.clone(), 60_000), payload)
            })
            .collect();
        sim.run_until(120_000, |_| calls.iter().all(|(call, _)| call.try_result().is_some()));
        for (call, payload) in &calls {
            assert_eq!(call.try_result(), Some(Ok(payload.clone())), "seed {seed}");
        }
    }
}

#[test]
fn channels_flow_on_credit_over_a_lossy_link() {
    for seed in 1..=20 {
        let mut sim = Sim::new(seed, 15);
        sim.linked();
        let channel: Channel = sim.client.open(COUNTDOWN, vec![100], 4);
        let mut items = Vec::new();
        // Consuming frees credit, which the next transmit grants.
        sim.run_until(120_000, |_| {
            items.extend(std::iter::from_fn(|| channel.try_recv()).map(|i| i[0]));
            channel.end().is_some()
        });
        assert_eq!(channel.end(), Some(Ok(())), "seed {seed}");
        assert_eq!(items, (1..=100).rev().collect::<Vec<u8>>(), "seed {seed}");
    }
}

#[test]
fn an_idle_link_wakes_only_to_ping() {
    let mut sim = Sim::new(1, 0);
    sim.linked();
    let (start, wakeups) = (sim.now, sim.wakeups);
    sim.run_until(60_000, |s| s.now >= start + 10_000);
    assert!(matches!(sim.client.link_state(), LinkState::Linked { .. }));
    // A ping interval is 250 ms: about 40 in 10 s, each with its arrival and
    // its ack, not a poll every millisecond (10 000).
    let woke = sim.wakeups - wakeups;
    assert!((40..500).contains(&woke), "{woke} wakeups");
}

#[test]
fn a_call_with_a_deadline_expires_on_time() {
    let mut sim = Sim::new(1, 0);
    sim.linked();
    // The link is fine, but the call is never answered.
    sim.answers = false;
    let call = sim.client.call(ECHO, vec![1], 1_000);
    let asked = sim.now;
    sim.run_until(5_000, |_| call.try_result().is_some());
    assert_eq!(call.try_result(), Some(Err(Status::DeadlineExceeded)));
    // Not before its deadline, and not much after it.
    let late = sim.now - asked;
    assert!((1_000..1_100).contains(&late), "{late} ms");
}

#[test]
fn a_silent_peer_is_lost_on_schedule() {
    let mut sim = Sim::new(1, 0);
    sim.linked();
    sim.s2c.cut = true;
    let cut_at = sim.now;
    sim.run_until(10_000, |s| matches!(s.client.link_state(), LinkState::PeerLost));
    // One ping interval of silence, then the retransmits.
    let cfg = LinkConfig::default();
    let budget = cfg.ping_interval_ms
        + u64::from(cfg.max_retransmits + 1) * cfg.retransmit_timeout(32)
        + 500;
    assert!(sim.now - cut_at <= budget, "{} ms", sim.now - cut_at);
    assert_eq!(sim.client.next_deadline(), None);
}
