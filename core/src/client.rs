//! The client (S3) side. Calls return handles the app owns; the client keeps
//! only `Weak` references. Each poll it scans them: a dropped handle cancels
//! its call, and slots the app freed go back to the server as credit.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::task::{Context, Poll, Waker};

use tether::{MethodId, RawCall, RawChannel, Status, Transport};

use crate::frame::{Frame, Header};
use crate::link::{Link, LinkConfig, LinkState, LinkStats};
use crate::notify::SharedNotify;
use crate::tether_status;
use crate::wire::{Credits, CreditsArgs, Grant, Kind};

#[derive(Default)]
struct Slot {
    items: VecDeque<Vec<u8>>,
    end: Option<Result<(), Status>>,
    waker: Option<Waker>,
}

type SlotRef = Rc<RefCell<Slot>>;

/// Who to wake once the client and the slots are no longer borrowed. A waker
/// is someone else's code: one that polled its task right away would find
/// them borrowed.
#[derive(Default)]
struct Pending {
    wakers: Vec<Waker>,
    io: Option<SharedNotify>,
}

impl Pending {
    fn add(&mut self, slot: &mut Slot) {
        self.wakers.extend(slot.waker.take());
    }

    fn wake(self) {
        self.wakers.into_iter().for_each(Waker::wake);
        if let Some(io) = self.io {
            io.notify();
        }
    }
}

/// Receiving end of a server-streaming call. Dropping it cancels the call.
pub struct Channel {
    slot: SlotRef,
    io: SharedNotify,
}

impl Channel {
    /// An item if one is waiting.
    pub fn try_recv(&self) -> Option<Vec<u8>> {
        let item = self.slot.borrow_mut().items.pop_front();
        if item.is_some() {
            // The slot it freed goes back to the server as credit.
            self.io.notify();
        }
        item
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.io.notify();
    }
}

impl RawChannel for Channel {
    type Buf = Vec<u8>;

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        if let Some(item) = self.try_recv() {
            return Poll::Ready(Some(item));
        }
        let mut slot = self.slot.borrow_mut();
        if slot.end.is_some() {
            return Poll::Ready(None);
        }
        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn end(&self) -> Option<Result<(), Status>> {
        let slot = self.slot.borrow();
        if slot.items.is_empty() { slot.end } else { None }
    }
}

/// A pending unary call. Dropping it before the response cancels the call.
pub struct Call {
    slot: SlotRef,
    io: SharedNotify,
}

impl Drop for Call {
    fn drop(&mut self) {
        self.io.notify();
    }
}

impl Call {
    /// `None` while in progress. Can be asked again after it's done.
    pub fn try_result(&self) -> Option<Result<Vec<u8>, Status>> {
        let slot = self.slot.borrow();
        Some(slot.end?.map(|()| slot.items.front().cloned().unwrap_or_default()))
    }
}

impl RawCall for Call {
    type Buf = Vec<u8>;

    fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Status>> {
        match self.try_result() {
            Some(result) => Poll::Ready(result),
            None => {
                self.slot.borrow_mut().waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientStats {
    pub calls: u32,
    pub cancels: u32,
    /// Items that arrived for a dropped channel before its Cancel went out.
    pub stale_items: u32,
    pub deadlines_exceeded: u32,
    /// Items beyond the granted credit (a server bug).
    pub overruns: u32,
}

struct Entry {
    slot: Weak<RefCell<Slot>>,
    streaming: bool,
    capacity: usize,
    /// Credit the server holds: granted, and not used by an item yet. Buffered
    /// items plus this never exceed `capacity`.
    held: usize,
    deadline: Option<u64>,
}

pub struct Client {
    link: Link,
    calls: BTreeMap<u32, Entry>,
    next_call_id: u32,
    stats: ClientStats,
    now: u64,
    io: SharedNotify,
    pending: Pending,
}

impl Client {
    pub fn new(boot_id: u32, cfg: LinkConfig) -> Self {
        Self {
            link: Link::new(boot_id, cfg),
            calls: BTreeMap::new(),
            next_call_id: 1,
            stats: ClientStats::default(),
            now: 0,
            io: SharedNotify::default(),
            pending: Pending::default(),
        }
    }

    /// Runs `f`, then wakes whoever it left pending.
    fn waking<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let result = f(self);
        core::mem::take(&mut self.pending).wake();
        result
    }

    /// Notified when the app does something that needs the I/O task: starts a
    /// call, consumes an item, or drops a call.
    pub fn io(&self) -> SharedNotify {
        self.io.clone()
    }

    /// When `poll_transmit` next has something to do, unless something arrives
    /// or the app notifies `io` first: the link's next deadline, or a call's.
    /// Nothing once the link is terminal.
    pub fn next_deadline(&self) -> Option<u64> {
        let link = self.link.next_deadline()?;
        let call = self.calls.values().filter_map(|e| e.deadline).min();
        Some(call.map_or(link, |c| c.min(link)))
    }

    pub fn link_state(&self) -> LinkState {
        self.link.state()
    }

    pub fn link_stats(&self) -> LinkStats {
        self.link.stats()
    }

    pub fn stats(&self) -> ClientStats {
        self.stats
    }

    /// Calls the server has not finished yet, including dropped ones not
    /// scanned yet.
    pub fn open_calls(&self) -> usize {
        self.calls.len()
    }

    /// Starts a unary call that fails with `DeadlineExceeded` after
    /// `timeout_ms`, counted from the last `poll_transmit` time.
    pub fn call(&mut self, method: MethodId, payload: Vec<u8>, timeout_ms: u64) -> Call {
        self.waking(|c| c.start_call(method, payload, timeout_ms))
    }

    /// Opens a server-streaming channel buffering up to `capacity` items.
    pub fn open(&mut self, method: MethodId, payload: Vec<u8>, capacity: u16) -> Channel {
        self.waking(|c| c.start_open(method, payload, capacity))
    }

    pub fn receive(&mut self, bytes: &[u8]) {
        self.waking(|c| c.feed(bytes));
    }

    pub fn poll_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        self.waking(|c| c.step(now))
    }

    // The methods below leave their wakes pending, for `waking` or for
    // `SharedClient::with`.

    fn start_call(&mut self, method: MethodId, payload: Vec<u8>, timeout_ms: u64) -> Call {
        let deadline = Some(self.now + timeout_ms);
        Call {
            slot: self.start(Kind::Request, method, payload, 1, deadline),
            io: self.io.clone(),
        }
    }

    fn start_open(&mut self, method: MethodId, payload: Vec<u8>, capacity: u16) -> Channel {
        assert!(capacity > 0);
        Channel {
            slot: self.start(Kind::Open, method, payload, capacity, None),
            io: self.io.clone(),
        }
    }

    fn start(
        &mut self,
        kind: Kind,
        MethodId { service, method }: MethodId,
        payload: Vec<u8>,
        credit: u16,
        deadline: Option<u64>,
    ) -> SlotRef {
        let call_id = self.next_call_id;
        self.next_call_id += 1;
        let slot = SlotRef::default();
        if self.link.state().is_terminal() {
            slot.borrow_mut().end = Some(Err(Status::Unavailable));
            return slot;
        }
        // Registered before the request is queued, so there's always an entry
        // for anything the server sends back.
        self.calls.insert(
            call_id,
            Entry {
                slot: Rc::downgrade(&slot),
                streaming: kind == Kind::Open,
                capacity: credit.into(),
                held: credit.into(),
                deadline,
            },
        );
        let header = Header { call_id, service, method, credit, ..Header::new(kind) };
        self.link.send(header, payload);
        self.stats.calls += 1;
        self.pending.io = Some(self.io.clone());
        slot
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.link.receive(bytes);
        while let Some(frame) = self.link.poll_receive() {
            self.dispatch(frame);
        }
        self.check_link();
    }

    fn step(&mut self, now: u64) -> Option<Vec<u8>> {
        self.now = now;
        self.scan();
        self.expire_deadlines(now);
        let wire = self.link.poll_transmit(now);
        self.check_link();
        wire
    }

    /// Once the link is down for good, fails every pending call.
    fn check_link(&mut self) {
        if !self.link.state().is_terminal() {
            return;
        }
        for (_, entry) in core::mem::take(&mut self.calls) {
            if let Some(slot) = entry.slot.upgrade() {
                let mut slot = slot.borrow_mut();
                slot.end = Some(Err(Status::Unavailable));
                self.pending.add(&mut slot);
            }
        }
    }

    /// Cancels calls whose handles were dropped, and grants every channel the
    /// slots its app freed, all in one Credit frame.
    fn scan(&mut self) {
        let mut dropped = Vec::new();
        let mut grants = Vec::new();
        for (&call_id, entry) in &mut self.calls {
            let Some(slot) = entry.slot.upgrade() else {
                dropped.push(call_id);
                continue;
            };
            if !entry.streaming {
                continue;
            }
            let free = entry.capacity - slot.borrow().items.len() - entry.held;
            if free > 0 {
                entry.held += free;
                grants.push(Grant::new(call_id, free as u16));
            }
        }
        for call_id in dropped {
            self.cancel(call_id);
        }
        if !grants.is_empty() {
            let mut fbb = flatbuffers::FlatBufferBuilder::new();
            let grants = fbb.create_vector(&grants);
            let credits = Credits::create(&mut fbb, &CreditsArgs { grants: Some(grants) });
            fbb.finish(credits, None);
            self.link.send(Header::new(Kind::Credit), fbb.finished_data().to_vec());
        }
    }

    fn cancel(&mut self, call_id: u32) {
        if self.calls.remove(&call_id).is_some() {
            self.stats.cancels += 1;
            self.link.send(Header { call_id, ..Header::new(Kind::Cancel) }, Vec::new());
        }
    }

    fn expire_deadlines(&mut self, now: u64) {
        let expired: Vec<u32> = self
            .calls
            .iter()
            .filter(|(_, e)| e.deadline.is_some_and(|d| now >= d))
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            if let Some(slot) = self.calls.get(&id).and_then(|e| e.slot.upgrade()) {
                let mut slot = slot.borrow_mut();
                slot.end = Some(Err(Status::DeadlineExceeded));
                self.pending.add(&mut slot);
            }
            self.stats.deadlines_exceeded += 1;
            self.cancel(id);
        }
    }

    fn dispatch(&mut self, frame: Frame) {
        let h = frame.header;
        let expects = |e: &Entry| match h.kind {
            Kind::Response => !e.streaming,
            Kind::Item | Kind::End => e.streaming,
            _ => false,
        };
        let Some(entry) = self.calls.get_mut(&h.call_id).filter(|e| expects(e)) else {
            // Unknown or already cancelled: make sure the server stops.
            if h.kind == Kind::Item {
                self.stats.stale_items += 1;
                self.link
                    .send(Header { call_id: h.call_id, ..Header::new(Kind::Cancel) }, Vec::new());
            }
            return;
        };
        let Some(slot) = entry.slot.upgrade() else {
            // Dropped, and not scanned yet: cancel it now, or not at all if
            // the server just finished the call anyway.
            if h.kind == Kind::Item {
                self.stats.stale_items += 1;
                self.cancel(h.call_id);
            } else {
                self.calls.remove(&h.call_id);
            }
            return;
        };
        let mut slot = slot.borrow_mut();
        match h.kind {
            Kind::Item if entry.held == 0 => self.stats.overruns += 1,
            Kind::Item => {
                entry.held -= 1;
                slot.items.push_back(frame.payload);
            }
            _ => {
                let result = tether_status(h.status);
                if result.is_ok() && h.kind == Kind::Response {
                    slot.items.push_back(frame.payload);
                }
                slot.end = Some(result);
                self.calls.remove(&h.call_id);
            }
        }
        self.pending.add(&mut slot);
    }
}

/// The client as apps and the I/O task share it, by reference or `Rc`.
///
/// Each method borrows the client only while it runs and none of them is
/// async, so no borrow can be held across an `.await`. Wakers are called after
/// the borrow ends, so a task they poll right away can use the client too.
///
/// It isn't thread-safe: every app must run on the executor that also feeds
/// it bytes.
pub struct SharedClient(RefCell<Client>);

impl SharedClient {
    pub fn new(client: Client) -> Self {
        Self(RefCell::new(client))
    }

    /// Runs `f` on the client, then wakes whoever it left pending.
    fn with<R>(&self, f: impl FnOnce(&mut Client) -> R) -> R {
        let (result, pending) = {
            let mut client = self.0.borrow_mut();
            let result = f(&mut client);
            (result, core::mem::take(&mut client.pending))
        };
        pending.wake();
        result
    }

    pub fn receive(&self, bytes: &[u8]) {
        self.with(|c| c.feed(bytes));
    }

    pub fn poll_transmit(&self, now: u64) -> Option<Vec<u8>> {
        self.with(|c| c.step(now))
    }

    /// See [`Client::next_deadline`].
    pub fn next_deadline(&self) -> Option<u64> {
        self.0.borrow().next_deadline()
    }

    /// See [`Client::io`].
    pub fn io(&self) -> SharedNotify {
        self.0.borrow().io()
    }

    pub fn link_state(&self) -> LinkState {
        self.0.borrow().link_state()
    }

    pub fn link_stats(&self) -> LinkStats {
        self.0.borrow().link_stats()
    }

    pub fn stats(&self) -> ClientStats {
        self.0.borrow().stats()
    }

    /// See [`Client::open_calls`].
    pub fn open_calls(&self) -> usize {
        self.0.borrow().open_calls()
    }
}

impl Transport for SharedClient {
    type Call = Call;
    type Channel = Channel;

    fn call(&self, method: MethodId, request: &[u8], timeout_ms: u32) -> Call {
        self.with(|c| c.start_call(method, request.to_vec(), timeout_ms.into()))
    }

    fn open(&self, method: MethodId, request: &[u8], capacity: u16) -> Channel {
        self.with(|c| c.start_open(method, request.to_vec(), capacity))
    }
}
