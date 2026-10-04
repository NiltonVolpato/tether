//! The server (co-processor) side. Calls are addressed by call id; `Router`
//! hands services `ServerReply`s and `ServerSink`s that address them.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use tether::{CallId, MethodId, RawReply, RawSink, ServerTypes, Status, StreamError};

use crate::frame::{Frame, Header};
use crate::link::{Link, LinkConfig, LinkState, LinkStats};
use crate::notify::SharedNotify;
use crate::wire::{Credits, Kind};
use crate::wire_status;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    Call {
        call_id: u32,
        method: MethodId,
        payload: Vec<u8>,
    },
    Open {
        call_id: u32,
        method: MethodId,
        payload: Vec<u8>,
    },
    /// The client dropped the call, or the link went down; stop working on
    /// it (e.g. UNSUBSCRIBE).
    Cancelled {
        call_id: u32,
    },
}

struct Stream {
    credit: u16,
    /// Value waiting for credit in latest-value mode; newer values replace it.
    latest: Option<Vec<u8>>,
}

pub struct Server {
    link: Link,
    max_streams: usize,
    calls: BTreeMap<u32, Option<Stream>>,
    events: VecDeque<ServerEvent>,
    io: SharedNotify,
    /// Whether to notify `io` once the server is no longer borrowed.
    notify_io: bool,
}

impl Server {
    pub fn new(boot_id: u32, cfg: LinkConfig, max_streams: usize) -> Self {
        Self {
            link: Link::new(boot_id, cfg),
            max_streams,
            calls: BTreeMap::new(),
            events: VecDeque::new(),
            io: SharedNotify::default(),
            notify_io: false,
        }
    }

    /// Runs `f`, then notifies `io` if it queued something.
    fn waking<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let result = f(self);
        if core::mem::take(&mut self.notify_io) {
            self.io.notify();
        }
        result
    }

    /// Notified when a reply or an item is queued, for the I/O task.
    pub fn io(&self) -> SharedNotify {
        self.io.clone()
    }

    /// When `poll_transmit` next has something to do, unless something arrives
    /// or `io` is notified first. Nothing once the link is terminal.
    pub fn next_deadline(&self) -> Option<u64> {
        self.link.next_deadline()
    }

    pub fn shared(self) -> SharedServer {
        SharedServer(Rc::new(RefCell::new(self)))
    }

    pub fn link_state(&self) -> LinkState {
        self.link.state()
    }

    pub fn link_stats(&self) -> LinkStats {
        self.link.stats()
    }

    pub fn open_calls(&self) -> usize {
        self.calls.len()
    }

    pub fn receive(&mut self, bytes: &[u8]) {
        self.waking(|s| s.feed(bytes));
    }

    pub fn respond(&mut self, call_id: u32, result: Result<Vec<u8>, Status>) {
        self.waking(|s| s.queue_response(call_id, result));
    }

    pub fn send(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        self.waking(|s| s.queue_item(call_id, payload))
    }

    /// Latest-value mode: sends now if there's credit, otherwise replaces
    /// whatever value is still waiting.
    pub fn set_latest(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        self.waking(|s| s.queue_latest(call_id, payload))
    }

    pub fn end(&mut self, call_id: u32, result: Result<(), Status>) -> Result<(), StreamError> {
        self.waking(|s| s.queue_end(call_id, result))
    }

    // The methods below leave `io` to notify, for `waking` or for
    // `SharedServer::with`.

    fn feed(&mut self, bytes: &[u8]) {
        self.link.receive(bytes);
        while let Some(frame) = self.link.poll_receive() {
            self.dispatch(frame);
        }
        self.check_link();
    }

    pub fn poll_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        let wire = self.link.poll_transmit(now);
        self.check_link();
        wire
    }

    /// Once the link is down for good, cancels every open call.
    fn check_link(&mut self) {
        if !self.link.state().is_terminal() {
            return;
        }
        for (call_id, _) in core::mem::take(&mut self.calls) {
            self.events.push_back(ServerEvent::Cancelled { call_id });
        }
    }

    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    fn queue_response(&mut self, call_id: u32, result: Result<Vec<u8>, Status>) {
        if !matches!(self.calls.get(&call_id), Some(None)) {
            return; // cancelled meanwhile
        }
        self.calls.remove(&call_id);
        let (status, payload) = match result {
            Ok(p) => (wire_status(Ok(())), p),
            Err(s) => (wire_status(Err(s)), Vec::new()),
        };
        self.link
            .send(Header { call_id, status, ..Header::new(Kind::Response) }, payload);
        self.notify_io = true;
    }

    fn queue_item(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        let stream = self.stream(call_id)?;
        if stream.credit == 0 {
            return Err(StreamError::NoCredit);
        }
        stream.credit -= 1;
        self.link.send(Header { call_id, ..Header::new(Kind::Item) }, payload);
        self.notify_io = true;
        Ok(())
    }

    fn queue_latest(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        let stream = self.stream(call_id)?;
        if stream.credit == 0 {
            stream.latest = Some(payload);
            return Ok(());
        }
        self.queue_item(call_id, payload)
    }

    fn queue_end(&mut self, call_id: u32, result: Result<(), Status>) -> Result<(), StreamError> {
        self.stream(call_id)?;
        self.calls.remove(&call_id);
        let header = Header { call_id, status: wire_status(result), ..Header::new(Kind::End) };
        self.link.send(header, Vec::new());
        self.notify_io = true;
        Ok(())
    }

    /// Whether the call is still waiting for a response or open for items.
    pub fn is_open(&self, call_id: u32) -> bool {
        self.calls.contains_key(&call_id)
    }

    pub fn credit(&self, call_id: u32) -> Option<u16> {
        self.calls.get(&call_id)?.as_ref().map(|s| s.credit)
    }

    fn stream(&mut self, call_id: u32) -> Result<&mut Stream, StreamError> {
        self.calls.get_mut(&call_id).and_then(Option::as_mut).ok_or(StreamError::Closed)
    }

    fn grant(&mut self, call_id: u32, credit: u16) {
        let Ok(stream) = self.stream(call_id) else {
            return; // ended or cancelled meanwhile
        };
        stream.credit = stream.credit.saturating_add(credit);
        if let Some(value) = stream.latest.take() {
            let _ = self.queue_item(call_id, value);
        }
    }

    fn dispatch(&mut self, frame: Frame) {
        let h = frame.header;
        match h.kind {
            Kind::Request => {
                self.calls.insert(h.call_id, None);
                self.events.push_back(ServerEvent::Call {
                    call_id: h.call_id,
                    method: MethodId::new(h.service, h.method),
                    payload: frame.payload,
                });
            }
            Kind::Open => {
                let streams = self.calls.values().filter(|c| c.is_some()).count();
                if streams >= self.max_streams {
                    let header = Header {
                        call_id: h.call_id,
                        status: wire_status(Err(Status::ResourceExhausted)),
                        ..Header::new(Kind::End)
                    };
                    self.link.send(header, Vec::new());
                    return;
                }
                self.calls.insert(h.call_id, Some(Stream { credit: h.credit, latest: None }));
                self.events.push_back(ServerEvent::Open {
                    call_id: h.call_id,
                    method: MethodId::new(h.service, h.method),
                    payload: frame.payload,
                });
            }
            Kind::Credit => {
                let Ok(credits) = flatbuffers::root::<Credits>(&frame.payload) else {
                    return;
                };
                for grant in credits.grants().iter().flatten() {
                    self.grant(grant.call_id(), grant.credit());
                }
            }
            Kind::Cancel if self.calls.remove(&h.call_id).is_some() => {
                self.events.push_back(ServerEvent::Cancelled { call_id: h.call_id });
            }
            _ => {}
        }
    }
}

/// The server as the I/O task, the router, replies and sinks share it.
/// Clones are cheap and share one server.
///
/// Each method borrows the server only while it runs and none of them is
/// async, so no borrow can be held across an `.await`. `io` is notified after
/// the borrow ends.
///
/// It isn't thread-safe: everything that touches it must run on one executor.
#[derive(Clone)]
pub struct SharedServer(Rc<RefCell<Server>>);

impl SharedServer {
    /// Runs `f` on the server, then notifies `io` if it queued something.
    fn with<R>(&self, f: impl FnOnce(&mut Server) -> R) -> R {
        let (result, io) = {
            let mut server = self.0.borrow_mut();
            let result = f(&mut server);
            let io = core::mem::take(&mut server.notify_io).then(|| server.io.clone());
            (result, io)
        };
        if let Some(io) = io {
            io.notify();
        }
        result
    }

    pub fn receive(&self, bytes: &[u8]) {
        self.with(|s| s.feed(bytes));
    }

    pub fn poll_transmit(&self, now: u64) -> Option<Vec<u8>> {
        self.with(|s| s.poll_transmit(now))
    }

    pub fn poll_event(&self) -> Option<ServerEvent> {
        self.with(Server::poll_event)
    }

    pub fn respond(&self, call_id: u32, result: Result<Vec<u8>, Status>) {
        self.with(|s| s.queue_response(call_id, result));
    }

    pub fn send(&self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        self.with(|s| s.queue_item(call_id, payload))
    }

    /// See [`Server::set_latest`].
    pub fn set_latest(&self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        self.with(|s| s.queue_latest(call_id, payload))
    }

    pub fn end(&self, call_id: u32, result: Result<(), Status>) -> Result<(), StreamError> {
        self.with(|s| s.queue_end(call_id, result))
    }

    /// See [`Server::is_open`].
    pub fn is_open(&self, call_id: u32) -> bool {
        self.0.borrow().is_open(call_id)
    }

    pub fn credit(&self, call_id: u32) -> Option<u16> {
        self.0.borrow().credit(call_id)
    }

    /// See [`Server::next_deadline`].
    pub fn next_deadline(&self) -> Option<u64> {
        self.0.borrow().next_deadline()
    }

    /// See [`Server::io`].
    pub fn io(&self) -> SharedNotify {
        self.0.borrow().io()
    }

    pub fn link_state(&self) -> LinkState {
        self.0.borrow().link_state()
    }

    pub fn link_stats(&self) -> LinkStats {
        self.0.borrow().link_stats()
    }

    pub fn open_calls(&self) -> usize {
        self.0.borrow().open_calls()
    }
}

impl ServerTypes for Server {
    type Reply = ServerReply;
    type Sink = ServerSink;
}

/// Answers one unary call on a shared server.
pub struct ServerReply {
    pub(crate) call: u32,
    pub(crate) server: SharedServer,
}

impl RawReply for ServerReply {
    fn call_id(&self) -> CallId {
        CallId(self.call)
    }

    fn send(self, result: Result<&[u8], Status>) {
        self.server.respond(self.call, result.map(<[u8]>::to_vec));
    }
}

/// The server's end of one channel on a shared server.
#[derive(Clone)]
pub struct ServerSink {
    pub(crate) call: u32,
    pub(crate) server: SharedServer,
}

impl RawSink for ServerSink {
    fn call_id(&self) -> CallId {
        CallId(self.call)
    }

    fn send(&self, item: &[u8]) -> Result<(), StreamError> {
        self.server.send(self.call, item.to_vec())
    }

    fn set_latest(&self, item: &[u8]) -> Result<(), StreamError> {
        self.server.set_latest(self.call, item.to_vec())
    }

    fn credit(&self) -> u16 {
        self.server.credit(self.call).unwrap_or(0)
    }

    fn end(self, result: Result<(), Status>) {
        let _ = self.server.end(self.call, result);
    }
}
