//! The services and servers a schema declares, read from its binary schema,
//! and resolved: wire ids assigned from the `rpc_server` enums and the order of
//! methods.

use std::collections::BTreeMap;

use flatbuffers_reflection::reflection;
use tether::flatbuffers;

const DEFAULT_TIMEOUT_MS: u32 = 5000;

// --- The schema as declared ---

pub struct SchemaDef {
    pub services: Vec<ServiceDef>,
    pub servers: Vec<ServerDef>,
}

pub struct ServiceDef {
    /// Fully qualified, e.g. "CoprocessorProto.Wifi".
    pub name: String,
    pub doc: Vec<String>,
    pub calls: Vec<CallDef>,
}

pub struct CallDef {
    pub name: String,
    pub request: String,
    pub response: String,
    pub attributes: BTreeMap<String, String>,
    pub doc: Vec<String>,
}

/// An enum marked `rpc_server`.
pub struct ServerDef {
    pub name: String,
    pub doc: Vec<String>,
    pub ubyte: bool,
    pub entries: Vec<EntryDef>,
}

pub struct EntryDef {
    pub name: String,
    pub value: i64,
    pub deprecated: bool,
}

type Strings<'a> = flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<&'a str>>;
type KeyValues<'a> =
    flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<reflection::KeyValue<'a>>>;

fn strings(v: Option<Strings<'_>>) -> Vec<String> {
    v.iter().flatten().map(str::to_string).collect()
}

fn attributes(v: Option<KeyValues<'_>>) -> BTreeMap<String, String> {
    (v.iter().flatten())
        .map(|kv| (kv.key().to_string(), kv.value().unwrap_or("").to_string()))
        .collect()
}

/// Extracts the services and server tables of a binary schema.
pub fn read(schema: reflection::Schema<'_>) -> SchemaDef {
    let services = (schema.services().into_iter().flatten())
        .map(|s| ServiceDef {
            name: s.name().to_string(),
            doc: strings(s.documentation()),
            calls: (s.calls().into_iter().flatten())
                .map(|c| CallDef {
                    name: c.name().to_string(),
                    request: c.request().name().to_string(),
                    response: c.response().name().to_string(),
                    attributes: attributes(c.attributes()),
                    doc: strings(c.documentation()),
                })
                .collect(),
        })
        .collect();
    let servers = (schema.enums().iter())
        .filter(|e| attributes(e.attributes()).contains_key("rpc_server"))
        .map(|e| ServerDef {
            name: e.name().to_string(),
            doc: strings(e.documentation()),
            ubyte: e.underlying_type().base_type() == reflection::BaseType::UByte,
            entries: (e.values().iter())
                .map(|v| EntryDef {
                    name: v.name().to_string(),
                    value: v.value(),
                    deprecated: attributes(v.attributes()).contains_key("deprecated"),
                })
                .collect(),
        })
        .collect();
    SchemaDef { services, servers }
}

// --- Resolved ---

pub struct Method {
    pub name: String,
    pub full_name: String,
    /// Position in the service: the method's wire id.
    pub number: u8,
    pub request: String,
    pub response: String,
    pub streaming: bool,
    pub timeout_ms: u32,
    pub deprecated: bool,
    pub doc: Vec<String>,
}

pub struct Service {
    pub namespace: Vec<String>,
    pub name: String,
    pub full_name: String,
    /// Value in its server's enum: the service's wire id.
    pub id: u8,
    pub methods: Vec<Method>,
    pub doc: Vec<String>,
}

pub struct Server {
    pub namespace: Vec<String>,
    pub name: String,
    /// Indexed by service id: the service's index in `Resolved::services`,
    /// `None` for deprecated entries and gaps.
    pub slots: Vec<Option<usize>>,
    pub doc: Vec<String>,
}

pub struct Resolved {
    pub services: Vec<Service>,
    pub servers: Vec<Server>,
}

pub fn split(full: &str) -> (Vec<String>, String) {
    let (namespace, name) = full.rsplit_once('.').unwrap_or(("", full));
    let namespace = namespace.split('.').filter(|p| !p.is_empty()).map(str::to_string).collect();
    (namespace, name.to_string())
}

/// Assigns wire ids: services from the `rpc_server` enums, methods from their
/// position.
pub fn resolve(schema: &SchemaDef) -> Result<Resolved, String> {
    let mut ids: BTreeMap<&str, u8> = BTreeMap::new();
    let mut servers = Vec::new();
    for srv in &schema.servers {
        if !srv.ubyte {
            return Err(format!("{}: an rpc_server enum must be a ubyte", srv.name));
        }
        let (namespace, name) = split(&srv.name);
        let mut slots = Vec::new();
        for e in &srv.entries {
            let id =
                u8::try_from(e.value).map_err(|_| format!("{}.{}: bad id", srv.name, e.name))?;
            if slots.len() <= usize::from(id) {
                slots.resize(usize::from(id) + 1, None);
            }
            if e.deprecated {
                continue;
            }
            let full = namespace.iter().chain([&e.name]).cloned().collect::<Vec<_>>().join(".");
            let index = (schema.services.iter().position(|s| s.name == full))
                .ok_or_else(|| format!("{}.{}: there's no rpc_service {full}", srv.name, e.name))?;
            if ids.insert(&schema.services[index].name, id).is_some() {
                return Err(format!("{full} is in more than one rpc_server enum"));
            }
            slots[usize::from(id)] = Some(index);
        }
        servers.push(Server { namespace, name, slots, doc: srv.doc.clone() });
    }

    let mut services = Vec::new();
    for s in &schema.services {
        let full = s.name.as_str();
        let id = *ids.get(full).ok_or_else(|| format!("{full} isn't in any rpc_server enum"))?;
        if s.calls.len() > 256 {
            return Err(format!("{full} has more than 256 methods"));
        }
        let mut methods = Vec::new();
        for (number, c) in s.calls.iter().enumerate() {
            let name = &c.name;
            let streaming = match c.attributes.get("streaming").map(String::as_str) {
                None | Some("none") => false,
                Some("server") => true,
                Some(other) => {
                    return Err(format!("{full}/{name}: streaming \"{other}\" unsupported"));
                }
            };
            let timeout_ms = match c.attributes.get("timeout_ms") {
                None => DEFAULT_TIMEOUT_MS,
                Some(t) => t.parse().map_err(|_| format!("{full}/{name}: bad timeout_ms {t}"))?,
            };
            methods.push(Method {
                name: name.clone(),
                full_name: format!("{full}/{name}"),
                number: number as u8,
                request: c.request.clone(),
                response: c.response.clone(),
                streaming,
                timeout_ms,
                deprecated: c.attributes.contains_key("deprecated"),
                doc: c.doc.clone(),
            });
        }
        let (namespace, name) = split(full);
        services.push(Service {
            namespace,
            name,
            full_name: full.to_string(),
            id,
            methods,
            doc: s.doc.clone(),
        });
    }
    Ok(Resolved { services, servers })
}

#[cfg(test)]
pub mod fixtures {
    use super::*;

    pub fn call(name: &str, deprecated: bool) -> CallDef {
        let mut attributes = BTreeMap::new();
        if deprecated {
            attributes.insert("deprecated".to_string(), "0".to_string());
        }
        CallDef {
            name: name.into(),
            request: "T.Req".into(),
            response: "T.Resp".into(),
            attributes,
            doc: Vec::new(),
        }
    }

    pub fn service(name: &str, calls: Vec<CallDef>) -> ServiceDef {
        ServiceDef { name: format!("T.{name}"), doc: Vec::new(), calls }
    }

    pub fn server(name: &str, entries: &[(&str, i64, bool)]) -> ServerDef {
        ServerDef {
            name: format!("T.{name}"),
            doc: Vec::new(),
            ubyte: true,
            entries: (entries.iter())
                .map(|&(name, value, deprecated)| EntryDef { name: name.into(), value, deprecated })
                .collect(),
        }
    }

    pub fn wifi_and_sonos() -> Vec<ServiceDef> {
        vec![
            service("Wifi", vec![call("Connect", false)]),
            service("Sonos", vec![call("Play", false)]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn service_ids_are_enum_values_and_tombstones_keep_theirs() {
        let schema = SchemaDef {
            services: wifi_and_sonos(),
            servers: vec![server(
                "Server",
                &[("Wifi", 0, false), ("Clock", 1, true), ("Sonos", 2, false)],
            )],
        };
        let r = resolve(&schema).unwrap();
        assert_eq!((r.services[0].id, r.services[1].id), (0, 2));
        assert_eq!(r.servers[0].slots, [Some(0), None, Some(1)]);
    }

    #[test]
    fn server_entry_must_name_a_service() {
        let schema = SchemaDef {
            services: wifi_and_sonos(),
            servers: vec![server(
                "Server",
                &[("Wifi", 0, false), ("Sonos", 1, false), ("Clock", 2, false)],
            )],
        };
        let err = resolve(&schema).err().unwrap();
        assert!(err.contains("no rpc_service T.Clock"), "{err}");
    }

    #[test]
    fn service_must_be_in_exactly_one_server() {
        let one = server("A", &[("Wifi", 0, false), ("Sonos", 1, false)]);
        let schema = SchemaDef { services: wifi_and_sonos(), servers: vec![one] };
        assert!(resolve(&schema).is_ok());

        let schema = SchemaDef {
            services: wifi_and_sonos(),
            servers: vec![server("A", &[("Wifi", 0, false)])],
        };
        let err = resolve(&schema).err().unwrap();
        assert!(err.contains("T.Sonos isn't in any rpc_server enum"), "{err}");

        let schema = SchemaDef {
            services: wifi_and_sonos(),
            servers: vec![
                server("A", &[("Wifi", 0, false), ("Sonos", 1, false)]),
                server("B", &[("Sonos", 0, false)]),
            ],
        };
        let err = resolve(&schema).err().unwrap();
        assert!(err.contains("T.Sonos is in more than one"), "{err}");
    }

    #[test]
    fn server_enum_must_be_ubyte() {
        let mut wide = server("Server", &[("Wifi", 0, false), ("Sonos", 1, false)]);
        wide.ubyte = false;
        let schema = SchemaDef { services: wifi_and_sonos(), servers: vec![wide] };
        assert!(resolve(&schema).err().unwrap().contains("must be a ubyte"));
    }
}
