//! Fakes of the `tether` traits, written the way an application's tests would.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use tether::{
    CallId, MethodId, Pack, RawCall, RawChannel, RawReply, RawSink, ServerTypes, Service, Status,
    StreamError, Transport,
};

// --- Client side ---

/// A canned channel: its items, then how it ends.
type Stream = (Vec<Vec<u8>>, Result<(), Status>);

/// Answers calls and opens channels from canned data, recording requests.
/// Unknown methods are `Unimplemented`.
#[derive(Default)]
pub struct FakeTransport {
    pub requests: RefCell<Vec<(MethodId, Vec<u8>)>>,
    responses: RefCell<BTreeMap<MethodId, Result<Vec<u8>, Status>>>,
    channels: RefCell<BTreeMap<MethodId, Stream>>,
}

impl FakeTransport {
    pub fn respond(&self, method: MethodId, result: Result<&impl Pack, Status>) {
        self.respond_raw(method, result.map(Pack::to_bytes));
    }

    pub fn respond_raw(&self, method: MethodId, result: Result<Vec<u8>, Status>) {
        self.responses.borrow_mut().insert(method, result);
    }

    pub fn stream_raw(&self, method: MethodId, items: Vec<Vec<u8>>, end: Result<(), Status>) {
        self.channels.borrow_mut().insert(method, (items, end));
    }
}

impl Transport for FakeTransport {
    type Call = ReadyCall;
    type Channel = QueuedChannel;

    fn call(&self, method: MethodId, request: &[u8], _timeout_ms: u32) -> ReadyCall {
        self.requests.borrow_mut().push((method, request.to_vec()));
        let result = self.responses.borrow().get(&method).cloned();
        ReadyCall(Some(result.unwrap_or(Err(Status::Unimplemented))))
    }

    fn open(&self, method: MethodId, request: &[u8], _capacity: u16) -> QueuedChannel {
        self.requests.borrow_mut().push((method, request.to_vec()));
        let (items, end) = self
            .channels
            .borrow()
            .get(&method)
            .cloned()
            .unwrap_or((vec![], Err(Status::Unimplemented)));
        QueuedChannel { items: items.into(), end }
    }
}

pub struct ReadyCall(Option<Result<Vec<u8>, Status>>);

impl RawCall for ReadyCall {
    type Buf = Vec<u8>;

    fn poll_result(&mut self, _: &mut Context<'_>) -> Poll<Result<Vec<u8>, Status>> {
        Poll::Ready(self.0.take().expect("polled after completion"))
    }
}

pub struct QueuedChannel {
    items: VecDeque<Vec<u8>>,
    end: Result<(), Status>,
}

impl RawChannel for QueuedChannel {
    type Buf = Vec<u8>;

    fn poll_recv(&mut self, _: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        Poll::Ready(self.items.pop_front())
    }

    fn end(&self) -> Option<Result<(), Status>> {
        self.items.is_empty().then_some(self.end)
    }
}

// --- Server side ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    Reply(CallId, Result<Vec<u8>, Status>),
    Item(CallId, Vec<u8>),
    End(CallId, Result<(), Status>),
}

/// Makes replies and sinks that record what a handler sends.
#[derive(Clone, Default)]
pub struct Recorder {
    sent: Rc<RefCell<Vec<Sent>>>,
    next_call: Rc<Cell<u32>>,
}

pub struct Recording;

impl ServerTypes for Recording {
    type Reply = RecordingReply;
    type Sink = RecordingSink;
}

impl Recorder {
    pub fn take(&self) -> Vec<Sent> {
        self.sent.take()
    }

    fn call_id(&self) -> CallId {
        self.next_call.set(self.next_call.get() + 1);
        CallId(self.next_call.get())
    }

    pub fn reply(&self) -> RecordingReply {
        RecordingReply { call: self.call_id(), sent: self.sent.clone() }
    }

    pub fn sink(&self, credit: u16) -> RecordingSink {
        RecordingSink {
            call: self.call_id(),
            sent: self.sent.clone(),
            credit: Rc::new(Cell::new(credit)),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

pub struct RecordingReply {
    call: CallId,
    sent: Rc<RefCell<Vec<Sent>>>,
}

impl RawReply for RecordingReply {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn send(self, result: Result<&[u8], Status>) {
        self.sent.borrow_mut().push(Sent::Reply(self.call, result.map(<[u8]>::to_vec)));
    }
}

/// Clones share credit, so a test keeps one to `grant` more.
#[derive(Clone)]
pub struct RecordingSink {
    call: CallId,
    sent: Rc<RefCell<Vec<Sent>>>,
    credit: Rc<Cell<u16>>,
    closed: Rc<Cell<bool>>,
}

impl RecordingSink {
    /// The client consumed `n` items.
    pub fn grant(&self, n: u16) {
        self.credit.set(self.credit.get() + n);
    }
}

impl RawSink for RecordingSink {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn send(&self, item: &[u8]) -> Result<(), StreamError> {
        if self.closed.get() {
            return Err(StreamError::Closed);
        }
        if self.credit.get() == 0 {
            return Err(StreamError::NoCredit);
        }
        self.credit.set(self.credit.get() - 1);
        self.sent.borrow_mut().push(Sent::Item(self.call, item.to_vec()));
        Ok(())
    }

    fn set_latest(&self, item: &[u8]) -> Result<(), StreamError> {
        self.send(item)
    }

    fn credit(&self) -> u16 {
        if self.closed.get() { 0 } else { self.credit.get() }
    }

    fn end(self, result: Result<(), Status>) {
        if !self.closed.replace(true) {
            self.sent.borrow_mut().push(Sent::End(self.call, result));
        }
    }
}

// --- Both sides ---

/// Serves one `Service` in-process: the client's calls go straight to the
/// handler, with no framing or link.
pub struct Loopback<V> {
    service: RefCell<V>,
    next_call: Cell<u32>,
    /// Channels the client dropped, delivered by `flush_cancellations`.
    dropped: Rc<RefCell<Vec<CallId>>>,
}

pub struct Loop;

impl ServerTypes for Loop {
    type Reply = LoopReply;
    type Sink = LoopSink;
}

impl<V: Service<Loop>> Loopback<V> {
    pub fn new(service: V) -> Self {
        Self { service: RefCell::new(service), next_call: Cell::new(0), dropped: Rc::default() }
    }

    /// Runs `f` on the service, e.g. to let a handler do pending work.
    pub fn with_service<R>(&self, f: impl FnOnce(&mut V) -> R) -> R {
        f(&mut self.service.borrow_mut())
    }

    /// Tells the service about the channels the client dropped.
    pub fn flush_cancellations(&self) {
        for call in self.dropped.take() {
            self.service.borrow_mut().cancelled(call);
        }
    }

    fn call_id(&self) -> CallId {
        self.next_call.set(self.next_call.get() + 1);
        CallId(self.next_call.get())
    }
}

impl<V: Service<Loop>> Transport for Loopback<V> {
    type Call = LoopCall;
    type Channel = LoopChannel;

    fn call(&self, method: MethodId, request: &[u8], _timeout_ms: u32) -> LoopCall {
        self.flush_cancellations();
        let reply = LoopReply { call: self.call_id(), slot: Rc::default() };
        let call = LoopCall(reply.slot.clone());
        let mut service = self.service.borrow_mut();
        if method.service == service.id() {
            service.call(method.method, request, reply);
        } else {
            reply.send(Err(Status::Unimplemented));
        }
        call
    }

    fn open(&self, method: MethodId, request: &[u8], capacity: u16) -> LoopChannel {
        self.flush_cancellations();
        let state = Rc::new(RefCell::new(ChannelState { credit: capacity, ..Default::default() }));
        let sink = LoopSink { call: self.call_id(), state: state.clone() };
        let channel = LoopChannel { call: sink.call, state, dropped: self.dropped.clone() };
        let mut service = self.service.borrow_mut();
        if method.service == service.id() {
            service.open(method.method, request, sink);
        } else {
            sink.end(Err(Status::Unimplemented));
        }
        channel
    }
}

#[derive(Default)]
struct CallSlot {
    result: Option<Result<Vec<u8>, Status>>,
    waker: Option<Waker>,
}

pub struct LoopCall(Rc<RefCell<CallSlot>>);

impl RawCall for LoopCall {
    type Buf = Vec<u8>;

    fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Status>> {
        let mut slot = self.0.borrow_mut();
        match slot.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

pub struct LoopReply {
    call: CallId,
    slot: Rc<RefCell<CallSlot>>,
}

impl RawReply for LoopReply {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn send(self, result: Result<&[u8], Status>) {
        let mut slot = self.slot.borrow_mut();
        slot.result = Some(result.map(<[u8]>::to_vec));
        if let Some(waker) = slot.waker.take() {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct ChannelState {
    items: VecDeque<Vec<u8>>,
    credit: u16,
    latest: Option<Vec<u8>>,
    end: Option<Result<(), Status>>,
    dropped: bool,
    waker: Option<Waker>,
}

impl ChannelState {
    fn push(&mut self, item: Vec<u8>) {
        self.credit -= 1;
        self.items.push_back(item);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

#[derive(Clone)]
pub struct LoopSink {
    call: CallId,
    state: Rc<RefCell<ChannelState>>,
}

impl RawSink for LoopSink {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn send(&self, item: &[u8]) -> Result<(), StreamError> {
        let mut state = self.state.borrow_mut();
        if state.dropped || state.end.is_some() {
            return Err(StreamError::Closed);
        }
        if state.credit == 0 {
            return Err(StreamError::NoCredit);
        }
        state.push(item.to_vec());
        Ok(())
    }

    fn set_latest(&self, item: &[u8]) -> Result<(), StreamError> {
        match self.send(item) {
            Err(StreamError::NoCredit) => {
                self.state.borrow_mut().latest = Some(item.to_vec());
                Ok(())
            }
            result => result,
        }
    }

    fn credit(&self) -> u16 {
        let state = self.state.borrow();
        if state.dropped || state.end.is_some() { 0 } else { state.credit }
    }

    fn end(self, result: Result<(), Status>) {
        let mut state = self.state.borrow_mut();
        if !state.dropped && state.end.is_none() {
            state.end = Some(result);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }
}

pub struct LoopChannel {
    call: CallId,
    state: Rc<RefCell<ChannelState>>,
    dropped: Rc<RefCell<Vec<CallId>>>,
}

impl RawChannel for LoopChannel {
    type Buf = Vec<u8>;

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let mut state = self.state.borrow_mut();
        if let Some(item) = state.items.pop_front() {
            state.credit += 1;
            if let Some(latest) = state.latest.take() {
                state.push(latest);
            }
            return Poll::Ready(Some(item));
        }
        if state.end.is_some() {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn end(&self) -> Option<Result<(), Status>> {
        let state = self.state.borrow();
        if state.items.is_empty() { state.end } else { None }
    }
}

impl Drop for LoopChannel {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.end.is_none() {
            state.dropped = true;
            self.dropped.borrow_mut().push(self.call);
        }
    }
}
