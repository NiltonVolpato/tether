//! Routes server events to the `tether::Service`s that own them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tether::{CallId, MethodId, RawReply, RawSink, ServerTable, Service, Status, lookup};

use crate::server::{Server, ServerEvent, ServerReply, ServerSink, SharedServer};

pub struct Router {
    table: &'static ServerTable,
    services: Vec<Option<Box<dyn Service<Server>>>>,
    /// Which service each open call belongs to, to route its cancellation.
    owners: BTreeMap<u32, u8>,
}

impl Router {
    pub fn new(table: &'static ServerTable) -> Self {
        let services = table.iter().map(|_| None).collect();
        Self { table, services, owners: BTreeMap::new() }
    }

    /// Serves `service`; methods of services never added are `Unimplemented`.
    pub fn add(&mut self, service: impl Service<Server> + 'static) -> &mut Self {
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
    pub fn run(&mut self, server: &SharedServer) {
        loop {
            let Some(event) = server.borrow_mut().poll_event() else { return };
            self.handle(server, event);
        }
    }

    /// Services run without the server borrowed, so their replies and sinks
    /// can use it.
    pub fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
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
                    service.cancelled(CallId(call_id));
                }
            }
        }
        // Calls that were answered or ended since the last event.
        self.owners.retain(|&id, _| server.borrow().is_open(id));
    }

    fn dispatch(
        &mut self,
        server: &SharedServer,
        call: u32,
        id: MethodId,
        payload: &[u8],
        streaming: bool,
    ) {
        let known = lookup(self.table, id).is_some_and(|(_, m)| m.streaming == streaming);
        let service = known.then(|| self.services[usize::from(id.service)].as_mut()).flatten();
        let server = server.clone();
        let Some(service) = service else {
            if streaming {
                ServerSink { call, server }.end(Err(Status::Unimplemented));
            } else {
                ServerReply { call, server }.send(Err(Status::Unimplemented));
            }
            return;
        };
        self.owners.insert(call, id.service);
        if streaming {
            service.open(id.method, payload, ServerSink { call, server });
        } else {
            service.call(id.method, payload, ServerReply { call, server });
        }
    }
}
