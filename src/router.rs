//! Routes server events to the services that own them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::MethodId;
use crate::proto::Status;
use crate::server::{Server, ServerEvent};

/// A server's services, generated from its `rpc_server` enum and indexed by
/// service id. Deprecated services are `None`.
pub type ServerTable = [Option<ServiceInfo>];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceInfo {
    /// Full name, e.g. "CoprocessorProto.Wifi".
    pub name: &'static str,
    /// Indexed by method number. Deprecated methods are `None`.
    pub methods: &'static [Option<MethodInfo>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodInfo {
    pub name: &'static str,
    pub streaming: bool,
}

/// Looks up a method, `None` if it's unknown or deprecated.
pub fn lookup(table: &ServerTable, id: MethodId) -> Option<(&ServiceInfo, &MethodInfo)> {
    let service = table.get(usize::from(id.service))?.as_ref()?;
    Some((service, service.methods.get(usize::from(id.method))?.as_ref()?))
}

pub struct IncomingCall<'a> {
    pub call_id: u32,
    /// The method's number within the service.
    pub method: u8,
    pub payload: &'a [u8],
}

/// Implemented by generated code for each `rpc_service`.
pub trait Service {
    /// This service's id in the server's table.
    fn id(&self) -> u8;
    /// Handles a call to one of this service's methods.
    fn call(&mut self, server: &mut Server, call: IncomingCall<'_>);
    fn cancelled(&mut self, server: &mut Server, call_id: u32);
}

pub struct Router {
    table: &'static ServerTable,
    services: Vec<Option<Box<dyn Service>>>,
    /// Which service each open call belongs to, to route its cancellation.
    owners: BTreeMap<u32, u8>,
}

impl Router {
    pub fn new(table: &'static ServerTable) -> Self {
        let services = table.iter().map(|_| None).collect();
        Self { table, services, owners: BTreeMap::new() }
    }

    /// Serves `service`; methods of services never added are UNIMPLEMENTED.
    pub fn add(&mut self, service: impl Service + 'static) -> &mut Self {
        let id = usize::from(service.id());
        assert!(
            self.table.get(id).is_some_and(Option::is_some),
            "service {id} isn't in this server's table"
        );
        assert!(self.services[id].is_none(), "service {id} added twice");
        self.services[id] = Some(Box::new(service));
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
                if let Some(id) = self.owners.remove(&call_id)
                    && let Some(service) = &mut self.services[usize::from(id)]
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
        id: MethodId,
        payload: &[u8],
        streaming: bool,
    ) {
        let known = lookup(self.table, id).is_some_and(|(_, m)| m.streaming == streaming);
        let service = known.then(|| self.services[usize::from(id.service)].as_mut()).flatten();
        let Some(service) = service else {
            if streaming {
                let _ = server.end(call_id, Status::UNIMPLEMENTED);
            } else {
                server.respond(call_id, Err(Status::UNIMPLEMENTED));
            }
            return;
        };
        self.owners.insert(call_id, id.service);
        service.call(server, IncomingCall { call_id, method: id.method, payload });
    }
}
