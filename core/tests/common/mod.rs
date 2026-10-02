//! A client and a server connected through simulated lossy pipes.

#![allow(dead_code)]

use std::collections::VecDeque;

use tether_core::client::{Client, SharedClient};
use tether_core::link::{LinkConfig, LinkState};
use tether_core::router::Router;
use tether_core::server::{Server, ServerEvent, SharedServer};

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn percent(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

/// One direction of the UART.
pub struct Pipe {
    rng: Rng,
    drop_pct: u64,
    corrupt_pct: u64,
    /// Frames to drop unconditionally, counted from the start.
    pub drop_first: usize,
    /// Drops everything: a disconnected wire or a hung peer.
    pub cut: bool,
    /// When set, collects every frame written to the pipe.
    pub tap: Option<Vec<Vec<u8>>>,
    in_flight: VecDeque<(u64, Vec<u8>)>,
}

impl Pipe {
    pub fn new(seed: u64, drop_pct: u64, corrupt_pct: u64) -> Self {
        Self {
            rng: Rng(seed | 1),
            drop_pct,
            corrupt_pct,
            drop_first: 0,
            cut: false,
            tap: None,
            in_flight: VecDeque::new(),
        }
    }

    pub fn push(&mut self, now: u64, mut bytes: Vec<u8>) {
        if let Some(tap) = &mut self.tap {
            tap.push(bytes.clone());
        }
        if self.cut {
            return;
        }
        if self.drop_first > 0 {
            self.drop_first -= 1;
            return;
        }
        if self.rng.percent(self.drop_pct) {
            return;
        }
        if self.rng.percent(self.corrupt_pct) {
            // Any byte, delimiter included.
            let i = self.rng.below(bytes.len() as u64) as usize;
            bytes[i] ^= 1 << self.rng.below(8);
        }
        let latency = 1 + self.rng.below(3);
        self.in_flight.push_back((now + latency, bytes));
    }

    /// When the next bytes arrive.
    pub fn next_arrival(&self) -> Option<u64> {
        self.in_flight.front().map(|(t, _)| *t)
    }

    /// Bytes due at `now`, in arbitrary read-sized chunks.
    pub fn deliver(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut bytes = Vec::new();
        while self.in_flight.front().is_some_and(|(t, _)| *t <= now) {
            bytes.extend(self.in_flight.pop_front().unwrap().1);
        }
        let mut chunks = Vec::new();
        while !bytes.is_empty() {
            let n = (1 + self.rng.below(64) as usize).min(bytes.len());
            chunks.push(bytes.drain(..n).collect());
        }
        chunks
    }
}

/// What runs on the co-processor on top of `Server`.
pub trait ServerApp {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent);
    /// Called once per simulated millisecond, after events.
    fn tick(&mut self, _server: &SharedServer) {}
}

impl ServerApp for Router {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
        Router::handle(self, server, event);
    }
}

pub struct Sim<A> {
    pub now: u64,
    pub client: SharedClient,
    pub server: SharedServer,
    pub app: A,
    pub c2s: Pipe,
    pub s2c: Pipe,
}

impl<A: ServerApp> Sim<A> {
    pub fn new(app: A, seed: u64, drop_pct: u64, corrupt_pct: u64) -> Self {
        let cfg = LinkConfig::default();
        Self {
            now: 0,
            client: SharedClient::new(Client::new(0xC11E_0000 ^ seed as u32 | 1, cfg)),
            server: Server::new(0x5E4E_0000 ^ seed as u32 | 1, cfg, 4).shared(),
            app,
            c2s: Pipe::new(seed.wrapping_mul(3), drop_pct, corrupt_pct),
            s2c: Pipe::new(seed.wrapping_mul(7), drop_pct, corrupt_pct),
        }
    }

    pub fn step(&mut self) {
        while let Some(b) = self.client.borrow_mut().poll_transmit(self.now) {
            self.c2s.push(self.now, b);
        }
        while let Some(b) = self.server.borrow_mut().poll_transmit(self.now) {
            self.s2c.push(self.now, b);
        }
        for chunk in self.c2s.deliver(self.now) {
            self.server.borrow_mut().receive(&chunk);
        }
        for chunk in self.s2c.deliver(self.now) {
            self.client.borrow_mut().receive(&chunk);
        }
        loop {
            let Some(event) = self.server.borrow_mut().poll_event() else { break };
            self.app.handle(&self.server, event);
        }
        self.app.tick(&self.server);
        self.now += 1;
    }

    /// Steps until `poll` returns something; panics after `max_ms`.
    pub fn wait<R>(&mut self, max_ms: u64, mut poll: impl FnMut() -> Option<R>) -> R {
        let deadline = self.now + max_ms;
        loop {
            if let Some(r) = poll() {
                return r;
            }
            assert!(self.now < deadline, "nothing within {max_ms} ms");
            self.step();
        }
    }

    pub fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step();
        }
    }

    /// Steps until `done` holds; panics after `max_ms`.
    pub fn run_until(&mut self, max_ms: u64, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = self.now + max_ms;
        while !done(self) {
            assert!(self.now < deadline, "condition not met within {max_ms} ms");
            self.step();
        }
    }

    pub fn linked(&mut self) -> &mut Self {
        self.run_until(5_000, |s| {
            matches!(s.client.borrow().link_state(), LinkState::Linked { .. })
                && matches!(s.server.borrow().link_state(), LinkState::Linked { .. })
        });
        self
    }
}
