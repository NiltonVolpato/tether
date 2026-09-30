//! Reliable, in-order delivery of frames over a lossy byte stream.
//!
//! Hello handshake, then stop-and-wait ARQ: one sequenced frame in flight per
//! direction, retransmitted until acked; the receiver drops duplicates. The RPC
//! layer on top can assume every frame arrives exactly once, in order.
//!
//! Liveness: a Ping goes out when nothing has arrived from the peer for a
//! while, so an idle link is probed too. A frame retransmitted too often
//! without an ack means the peer is gone.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::frame::{self, Deframer, Frame, FrameError, Header};
use crate::proto::{Hello, HelloArgs, Kind};

#[derive(Clone, Copy, Debug)]
pub struct LinkConfig {
    pub retransmit_ms: u64,
    /// Retransmits of one frame without an ack before the peer is lost.
    pub max_retransmits: u32,
    /// Silence from the peer, while linked, before a Ping goes out.
    pub ping_interval_ms: u64,
    pub hello_interval_ms: u64,
    /// Largest COBS frame accepted, excluding the delimiter.
    pub max_frame: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            retransmit_ms: 20,
            max_retransmits: 25,
            ping_interval_ms: 250,
            hello_interval_ms: 50,
            max_frame: 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Linked {
        peer_boot_id: u32,
    },
    /// Terminal: the peer sent a Hello with a new boot id. The owner is
    /// expected to reboot; nothing is sent or delivered anymore.
    PeerRebooted,
    /// Terminal: a frame went unacked through `max_retransmits` retransmits.
    /// Nothing is sent or delivered anymore.
    PeerLost,
}

impl LinkState {
    /// Whether the link is down for good; the owner starts a new one.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::PeerRebooted | Self::PeerLost)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub frames_tx: u32,
    pub frames_rx: u32,
    pub retransmits: u32,
    pub duplicates: u32,
    pub pings: u32,
    pub cobs_errors: u32,
    pub crc_errors: u32,
    pub other_errors: u32,
    pub max_queue: usize,
}

struct InFlight {
    seq: u16,
    wire: Vec<u8>,
    sent_at: u64,
    retransmits: u32,
}

pub struct Link {
    cfg: LinkConfig,
    boot_id: u32,
    state: LinkState,
    deframer: Deframer,
    stats: LinkStats,
    // TX
    queue: VecDeque<(Header, Vec<u8>)>,
    in_flight: Option<InFlight>,
    next_seq: u16,
    ack_due: Option<u16>,
    hello_reply_due: bool,
    last_hello_at: Option<u64>,
    // RX
    /// A valid frame arrived since the last `poll_transmit`, which has the time.
    heard: bool,
    last_heard_at: u64,
    expected_seq: u16,
    delivered: VecDeque<Frame>,
}

fn next(seq: u16) -> u16 {
    if seq == u16::MAX { 1 } else { seq + 1 }
}

fn prev(seq: u16) -> u16 {
    if seq == 1 { u16::MAX } else { seq - 1 }
}

impl Link {
    /// `boot_id` must be non-zero and should differ across boots (hardware RNG).
    pub fn new(boot_id: u32, cfg: LinkConfig) -> Self {
        assert_ne!(boot_id, 0);
        Self {
            cfg,
            boot_id,
            state: LinkState::Connecting,
            deframer: Deframer::new(cfg.max_frame),
            stats: LinkStats::default(),
            queue: VecDeque::new(),
            in_flight: None,
            next_seq: 1,
            ack_due: None,
            hello_reply_due: false,
            last_hello_at: None,
            heard: false,
            last_heard_at: 0,
            expected_seq: 1,
            delivered: VecDeque::new(),
        }
    }

    pub fn state(&self) -> LinkState {
        self.state
    }

    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// Queues a sequenced frame. `seq` is assigned when it's first sent.
    pub fn send(&mut self, header: Header, payload: Vec<u8>) {
        self.queue.push_back((header, payload));
        self.stats.max_queue = self.stats.max_queue.max(self.queue.len());
    }

    /// Feeds bytes read from the UART.
    pub fn receive(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.deframer.push(b) {
                None => {}
                Some(Ok(frame)) => self.handle_frame(frame),
                Some(Err(FrameError::Cobs)) => self.stats.cobs_errors += 1,
                Some(Err(FrameError::Crc)) => self.stats.crc_errors += 1,
                Some(Err(_)) => self.stats.other_errors += 1,
            }
        }
    }

    /// Next frame for the layer above, exactly once and in order.
    pub fn poll_receive(&mut self) -> Option<Frame> {
        self.delivered.pop_front()
    }

    /// Next bytes to write to the UART, if any are due at `now`.
    pub fn poll_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        let wire = self.next_transmit(now)?;
        self.stats.frames_tx += 1;
        Some(wire)
    }

    fn next_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        if core::mem::take(&mut self.heard) {
            self.last_heard_at = now;
        }
        if self.state.is_terminal() {
            return None;
        }
        if let Some(seq) = self.ack_due.take() {
            let header = Header { seq, ..Header::new(Kind::Ack) };
            return Some(frame::encode(&header, &[]));
        }
        let hello_due = match self.state {
            LinkState::Connecting => {
                self.last_hello_at.is_none_or(|t| now >= t + self.cfg.hello_interval_ms)
            }
            _ => false,
        };
        if core::mem::take(&mut self.hello_reply_due) || hello_due {
            self.last_hello_at = Some(now);
            return Some(self.hello());
        }
        if !matches!(self.state, LinkState::Linked { .. }) {
            return None;
        }
        if let Some(f) = &mut self.in_flight {
            if now < f.sent_at + self.cfg.retransmit_ms {
                return None;
            }
            if f.retransmits == self.cfg.max_retransmits {
                self.state = LinkState::PeerLost;
                return None;
            }
            f.sent_at = now;
            f.retransmits += 1;
            self.stats.retransmits += 1;
            return Some(f.wire.clone());
        }
        if self.queue.is_empty() && now >= self.last_heard_at + self.cfg.ping_interval_ms {
            self.stats.pings += 1;
            self.queue.push_back((Header::new(Kind::Ping), Vec::new()));
        }
        let (mut header, payload) = self.queue.pop_front()?;
        header.seq = self.next_seq;
        self.next_seq = next(self.next_seq);
        let wire = frame::encode(&header, &payload);
        self.in_flight =
            Some(InFlight { seq: header.seq, wire: wire.clone(), sent_at: now, retransmits: 0 });
        Some(wire)
    }

    fn hello(&self) -> Vec<u8> {
        let peer_boot_id = match self.state {
            LinkState::Linked { peer_boot_id } => peer_boot_id,
            _ => 0,
        };
        let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(32);
        let hello = Hello::create(&mut fbb, &HelloArgs { boot_id: self.boot_id, peer_boot_id });
        fbb.finish(hello, None);
        frame::encode(&Header::new(Kind::Hello), fbb.finished_data())
    }

    fn handle_frame(&mut self, frame: Frame) {
        self.stats.frames_rx += 1;
        if self.state.is_terminal() {
            return;
        }
        self.heard = true;
        match (self.state, frame.header.kind) {
            (_, Kind::Hello) => self.handle_hello(&frame.payload),
            (LinkState::Linked { .. }, Kind::Ack) => {
                if self.in_flight.as_ref().is_some_and(|f| f.seq == frame.header.seq) {
                    self.in_flight = None;
                }
            }
            (LinkState::Linked { .. }, _) if frame.header.seq != 0 => {
                let seq = frame.header.seq;
                if seq == self.expected_seq {
                    self.expected_seq = next(seq);
                    self.ack_due = Some(seq);
                    if frame.header.kind != Kind::Ping {
                        self.delivered.push_back(frame);
                    }
                } else if seq == prev(self.expected_seq) {
                    // Our ack was lost; ack again.
                    self.stats.duplicates += 1;
                    self.ack_due = Some(seq);
                }
            }
            // Sequenced traffic before our Hello got through, or garbage.
            _ => {}
        }
    }

    fn handle_hello(&mut self, payload: &[u8]) {
        let Ok(hello) = flatbuffers::root::<Hello>(payload) else {
            self.stats.other_errors += 1;
            return;
        };
        let boot_id = hello.boot_id();
        let seen = hello.peer_boot_id();
        match self.state {
            // A stale reply addressed to our previous boot: linking to it would
            // pair us with a peer that is about to see our Hello and reboot.
            LinkState::Connecting if seen != 0 && seen != self.boot_id => return,
            LinkState::Connecting => self.state = LinkState::Linked { peer_boot_id: boot_id },
            LinkState::Linked { peer_boot_id } if peer_boot_id == boot_id => {}
            // Don't answer: the peer would link to us, then see our own new
            // Hello after we reboot and reboot again.
            _ => {
                self.state = LinkState::PeerRebooted;
                return;
            }
        }
        // A retransmitted Hello (our reply was lost) has the same boot_id and
        // is answered again instead of being mistaken for a reboot.
        if seen != self.boot_id {
            self.hello_reply_due = true;
        }
    }
}
