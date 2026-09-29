//! Routes server events to the services that own them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::proto::Status;
use crate::server::{Server, ServerEvent};

/// An entry of the generated method table (`METHODS`), keyed by method id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method {
    /// Full name, e.g. "CoprocessorProto.Wifi/Connect".
    pub name: &'static str,
    /// Index of the method's service in the schema.
    pub service: usize,
    pub streaming: bool,
}

pub type MethodTable = phf::Map<u32, Method>;

pub struct IncomingCall<'a> {
    pub call_id: u32,
    pub method: u32,
    pub payload: &'a [u8],
}

/// Implemented by generated code for each `rpc_service`.
pub trait Service {
    /// This service's index in the schema, as used by `Method::service`.
    fn index(&self) -> usize;
    /// Handles a call to one of this service's methods.
    fn call(&mut self, server: &mut Server, call: IncomingCall<'_>);
    fn cancelled(&mut self, server: &mut Server, call_id: u32);
}

pub struct Router {
    methods: &'static MethodTable,
    services: Vec<Option<Box<dyn Service>>>,
    /// Which service each open call belongs to, to route its cancellation.
    owners: BTreeMap<u32, usize>,
}

impl Router {
    pub fn new(methods: &'static MethodTable) -> Self {
        Self { methods, services: Vec::new(), owners: BTreeMap::new() }
    }

    /// Serves `service`; methods of services never added are UNIMPLEMENTED.
    pub fn add(&mut self, service: impl Service + 'static) -> &mut Self {
        let i = service.index();
        if self.services.len() <= i {
            self.services.resize_with(i + 1, || None);
        }
        assert!(self.services[i].is_none(), "service {i} added twice");
        self.services[i] = Some(Box::new(service));
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
                if let Some(i) = self.owners.remove(&call_id)
                    && let Some(service) = &mut self.services[i]
                {
                    service.cancelled(server, call_id);
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
        let service = self
            .methods
            .get(&method)
            .filter(|m| m.streaming == streaming)
            .and_then(|m| Some((m.service, self.services.get_mut(m.service)?.as_mut()?)));
        let Some((i, service)) = service else {
            if streaming {
                let _ = server.end(call_id, Status::UNIMPLEMENTED);
            } else {
                server.respond(call_id, Err(Status::UNIMPLEMENTED));
            }
            return;
        };
        self.owners.insert(call_id, i);
        service.call(server, IncomingCall { call_id, method, payload });
    }
}
