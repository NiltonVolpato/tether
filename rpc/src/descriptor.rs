/// A method's wire id: its service's value in the server's `rpc_server` enum
/// and its position within that `rpc_service`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodId {
    pub service: u8,
    pub method: u8,
}

impl MethodId {
    pub const fn new(service: u8, method: u8) -> Self {
        Self { service, method }
    }
}

/// Identifies one call on a server, from start to finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId(pub u32);

/// A server's services, generated from its `rpc_server` enum and indexed by
/// service id. Deprecated services are `None`.
pub type ServerTable = [Option<ServiceInfo>];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceInfo {
    /// Full name, e.g. "CoprocessorProto.Wifi".
    pub name: &'static str,
    /// Indexed by method number. Deprecated methods are `None`.
    pub methods: &'static [Option<MethodInfo>],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodInfo {
    pub name: &'static str,
    pub streaming: bool,
}

/// Looks up a method; `None` if it's unknown or deprecated.
pub fn lookup(table: &ServerTable, id: MethodId) -> Option<(&ServiceInfo, &MethodInfo)> {
    let service = table.get(usize::from(id.service))?.as_ref()?;
    Some((service, service.methods.get(usize::from(id.method))?.as_ref()?))
}
