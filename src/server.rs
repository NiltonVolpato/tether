//! The server (co-processor) side. Calls are addressed by call id; `Router`
//! and the generated services wrap them in typed replies and sinks.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use crate::MethodId;
use crate::frame::{Frame, Header};
use crate::link::{Link, LinkConfig, LinkState, LinkStats};
use crate::proto::{Kind, Status};

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
    /// The client dropped the call; stop working on it (e.g. UNSUBSCRIBE).
    Cancelled {
        call_id: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    /// The client hasn't consumed earlier items yet; try again later.
    NoCredit,
    /// Cancelled by the client, already ended, or never opened.
    Closed,
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
}

impl Server {
    pub fn new(boot_id: u32, cfg: LinkConfig, max_streams: usize) -> Self {
        Self {
            link: Link::new(boot_id, cfg),
            max_streams,
            calls: BTreeMap::new(),
            events: VecDeque::new(),
        }
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
        self.link.receive(bytes);
        while let Some(frame) = self.link.poll_receive() {
            self.dispatch(frame);
        }
    }

    pub fn poll_transmit(&mut self, now: u64) -> Option<Vec<u8>> {
        self.link.poll_transmit(now)
    }

    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    pub fn respond(&mut self, call_id: u32, result: Result<Vec<u8>, Status>) {
        if !matches!(self.calls.get(&call_id), Some(None)) {
            return; // cancelled meanwhile
        }
        self.calls.remove(&call_id);
        let (status, payload) = match result {
            Ok(p) => (Status::OK, p),
            Err(s) => (s, Vec::new()),
        };
        self.link
            .send(Header { call_id, status, ..Header::new(Kind::Response) }, payload);
    }

    pub fn send(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        let stream = self.stream(call_id)?;
        if stream.credit == 0 {
            return Err(StreamError::NoCredit);
        }
        stream.credit -= 1;
        self.link.send(Header { call_id, ..Header::new(Kind::Item) }, payload);
        Ok(())
    }

    /// Latest-value mode: sends now if there's credit, otherwise replaces
    /// whatever value is still waiting.
    pub fn set_latest(&mut self, call_id: u32, payload: Vec<u8>) -> Result<(), StreamError> {
        let stream = self.stream(call_id)?;
        if stream.credit == 0 {
            stream.latest = Some(payload);
            return Ok(());
        }
        self.send(call_id, payload)
    }

    pub fn end(&mut self, call_id: u32, status: Status) -> Result<(), StreamError> {
        self.stream(call_id)?;
        self.calls.remove(&call_id);
        self.link.send(Header { call_id, status, ..Header::new(Kind::End) }, Vec::new());
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
                        status: Status::RESOURCE_EXHAUSTED,
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
                let Ok(stream) = self.stream(h.call_id) else {
                    return;
                };
                stream.credit = stream.credit.saturating_add(h.credit);
                if let Some(value) = stream.latest.take() {
                    let _ = self.send(h.call_id, value);
                }
            }
            Kind::Cancel if self.calls.remove(&h.call_id).is_some() => {
                self.events.push_back(ServerEvent::Cancelled { call_id: h.call_id });
            }
            _ => {}
        }
    }
}
