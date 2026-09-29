//! Routes server events to the services that own them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::proto::Status;
use crate::server::{Server, ServerEvent};

pub struct IncomingCall<'a> {
    pub call_id: u32,
    pub method: u32,
    pub payload: &'a [u8],
}

/// Implemented by generated code for each `rpc_service`.
pub trait Service {
    /// `Some(streaming)` if `method` belongs to this service.
    fn method(&self, method: u32) -> Option<bool>;
    /// Handles a call to one of this service's methods.
    fn call(&mut self, server: &mut Server, call: IncomingCall<'_>);
    fn cancelled(&mut self, server: &mut Server, call_id: u32);
}

#[derive(Default)]
pub struct Router {
    services: Vec<Box<dyn Service>>,
    /// Which service each open call belongs to, to route its cancellation.
    owners: BTreeMap<u32, usize>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, service: impl Service + 'static) -> &mut Self {
        self.services.push(Box::new(service));
        self
    }

    /// Handles every pending server event.
    pub fn run(&mut self, server: &mut Server) {
        while let Some(event) = server.poll_event() {
            self.handle(server, event);
        }
    }

    pub fn handle(&mut self, server: &mut Server, event: ServerEvent) {
        match event {
            ServerEvent::Call { call_id, method, payload } => {
                self.dispatch(server, call_id, method, &payload, false)
            }
            ServerEvent::Open { call_id, method, payload } => {
                self.dispatch(server, call_id, method, &payload, true)
            }
            ServerEvent::Cancelled { call_id } => {
                if let Some(i) = self.owners.remove(&call_id) {
                    self.services[i].cancelled(server, call_id);
                }
            }
        }
        // Calls that were answered or ended since the last event.
        self.owners.retain(|&id, _| server.is_open(id));
    }

    fn dispatch(
        &mut self,
        server: &mut Server,
        call_id: u32,
        method: u32,
        payload: &[u8],
        streaming: bool,
    ) {
        let Some(i) = self.services.iter().position(|s| s.method(method) == Some(streaming)) else {
            if streaming {
                let _ = server.end(call_id, Status::UNIMPLEMENTED);
            } else {
                server.respond(call_id, Err(Status::UNIMPLEMENTED));
            }
            return;
        };
        self.owners.insert(call_id, i);
        self.services[i].call(server, IncomingCall { call_id, method, payload });
    }
}
