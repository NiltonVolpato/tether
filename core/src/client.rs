//! The client (S3) side. Calls return handles the app owns; the client keeps
//! only `Weak` references, so dropping a handle cancels the call.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::task::{Context, Poll, Waker};

use rpc::{MethodId, RawCall, RawChannel, Status, Transport};

use crate::rpc_status;
use crate::frame::{Frame, Header};
use crate::link::{Link, LinkConfig, LinkState, LinkStats};
use crate::proto::Kind;

/// What app handles ask the client to send on their behalf. They can't reach
/// the client itself: it may be borrowed when they're dropped.
enum Control {
    Credit(u32),
    Cancel(u32),
}

type Outbox = Rc<RefCell<Vec<Control>>>;

#[derive(Default)]
struct Slot {
    items: VecDeque<Vec<u8>>,
    end: Option<Result<(), Status>>,
    waker: Option<Waker>,
}

type SlotRef = Rc<RefCell<Slot>>;

fn wake(slot: &mut Slot) {
    if let Some(w) = slot.waker.take() {
        w.wake();
    }
}

/// Receiving end of a server-streaming call. Dropping it cancels the call.
pub struct Channel {
    call_id: u32,
    slot: SlotRef,
    outbox: Outbox,
}

impl Channel {
    /// An item if one is waiting.
    pub fn try_recv(&self) -> Option<Vec<u8>> {
        let item = self.slot.borrow_mut().items.pop_front()?;
        self.outbox.borrow_mut().push(Control::Credit(self.call_id));
        Some(item)
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

impl Drop for Channel {
    fn drop(&mut self) {
        if self.slot.borrow().end.is_none() {
            self.outbox.borrow_mut().push(Control::Cancel(self.call_id));
        }
    }
}

/// A pending unary call. Dropping it before the response cancels the call.
pub struct Call {
    call_id: u32,
    slot: SlotRef,
    outbox: Outbox,
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

impl Drop for Call {
    fn drop(&mut self) {
        if self.slot.borrow().end.is_none() {
            self.outbox.borrow_mut().push(Control::Cancel(self.call_id));
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
    deadline: Option<u64>,
}

pub struct Client {
    link: Link,
    calls: BTreeMap<u32, Entry>,
    next_call_id: u32,
    outbox: Outbox,
    stats: ClientStats,
    now: u64,
}

impl Client {
    pub fn new(boot_id: u32, cfg: LinkConfig) -> Self {
        Self {
            link: Link::new(boot_id, cfg),
            calls: BTreeMap::new(),
            next_call_id: 1,
            outbox: Rc::default(),
            stats: ClientStats::default(),
            now: 0,
        }
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

    /// Calls the server has not finished yet, including dropped ones whose
    /// Cancel hasn't gone out.
    pub fn open_calls(&self) -> usize {
        self.calls.len()
    }

    /// Starts a unary call that fails with `DeadlineExceeded` after
    /// `timeout_ms`, counted from the last `poll_transmit` time.
    pub fn call(&mut self, method: MethodId, payload: Vec<u8>, timeout_ms: u64) -> Call {
        let deadline = Some(self.now + timeout_ms);
        let (call_id, slot) = self.start(Kind::Request, method, payload, 1, deadline);
        Call { call_id, slot, outbox: self.outbox.clone() }
    }

    /// Opens a server-streaming channel buffering up to `capacity` items.
    pub fn open(&mut self, method: MethodId, payload: Vec<u8>, capacity: u16) -> Channel {
        assert!(capacity > 0);
        let (call_id, slot) = self.start(Kind::Open, method, payload, capacity, None);
        Channel { call_id, slot, outbox: self.outbox.clone() }
    }

    fn start(
        &mut self,
        kind: Kind,
        MethodId { service, method }: MethodId,
        payload: Vec<u8>,
        credit: u16,
        deadline: Option<u64>,
    ) -> (u32, SlotRef) {
        let call_id = self.next_call_id;
        self.next_call_id += 1;
        let slot = SlotRef::default();
        // Registered before the request is queued, so there's always an entry
        // for anything the server sends back.
        self.calls.insert(
            call_id,
            Entry {
                slot: Rc::downgrade(&slot),
                streaming: kind == Kind::Open,
                capacity: credit.into(),
                deadline,
            },
        );
        let header = Header { call_id, service, method, credit, ..Header::new(kind) };
        self.link.send(header, payload);
        self.stats.calls += 1;
        (call_id, slot)
    }

    pub fn receive(&mut self, bytes: &[u8]) {
        self.link.receive(bytes);
        while let Some(frame) = self.link.poll_receive() {
            self.dispatch(frame);
        }
    }

    pub fn poll_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        self.now = now;
        self.flush_outbox();
        self.expire_deadlines(now);
        self.link.poll_transmit(now)
    }

    fn flush_outbox(&mut self) {
        let controls = core::mem::take(&mut *self.outbox.borrow_mut());
        let mut credits: BTreeMap<u32, u16> = BTreeMap::new();
        for c in controls {
            match c {
                Control::Credit(id) => *credits.entry(id).or_default() += 1,
                Control::Cancel(id) => {
                    credits.remove(&id);
                    self.cancel(id);
                }
            }
        }
        for (call_id, credit) in credits {
            if self.calls.contains_key(&call_id) {
                self.link
                    .send(Header { call_id, credit, ..Header::new(Kind::Credit) }, Vec::new());
            }
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
                wake(&mut slot);
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
        let Some(entry) = self.calls.get(&h.call_id).filter(|e| expects(e)) else {
            // Unknown or already cancelled: make sure the server stops.
            if h.kind == Kind::Item {
                self.stats.stale_items += 1;
                self.link
                    .send(Header { call_id: h.call_id, ..Header::new(Kind::Cancel) }, Vec::new());
            }
            return;
        };
        let Some(slot) = entry.slot.upgrade() else {
            // Dropped, and its Cancel is still in the outbox: send it now, or
            // not at all if the server just finished the call anyway.
            if h.kind == Kind::Item {
                self.stats.stale_items += 1;
                self.cancel(h.call_id);
            } else {
                self.calls.remove(&h.call_id);
            }
            return;
        };
        let capacity = entry.capacity;
        let mut slot = slot.borrow_mut();
        match h.kind {
            Kind::Item if slot.items.len() >= capacity => self.stats.overruns += 1,
            Kind::Item => slot.items.push_back(frame.payload),
            _ => {
                let result = rpc_status(h.status);
                if result.is_ok() && h.kind == Kind::Response {
                    slot.items.push_back(frame.payload);
                }
                slot.end = Some(result);
                self.calls.remove(&h.call_id);
            }
        }
        wake(&mut slot);
    }
}

/// The client as apps share it. It's a `RefCell`, so every app must run on
/// the executor that also feeds it bytes.
pub struct SharedClient(RefCell<Client>);

impl SharedClient {
    pub fn new(client: Client) -> Self {
        Self(RefCell::new(client))
    }
}

impl core::ops::Deref for SharedClient {
    type Target = RefCell<Client>;

    fn deref(&self) -> &RefCell<Client> {
        &self.0
    }
}

impl Transport for SharedClient {
    type Call = Call;
    type Channel = Channel;

    fn call(&self, method: MethodId, request: &[u8], timeout_ms: u32) -> Call {
        self.borrow_mut().call(method, request.to_vec(), timeout_ms.into())
    }

    fn open(&self, method: MethodId, request: &[u8], capacity: u16) -> Channel {
        self.borrow_mut().open(method, request.to_vec(), capacity)
    }
}
