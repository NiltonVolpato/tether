//! Generates typed Rust clients and handler traits from the `rpc_service`s of
//! a flatbuffers binary schema, and a table for each `rpc_server` enum:
//!
//!   flatc -b --schema --bfbs-builtins --bfbs-comments app.fbs
//!   rpcgen app.bfbs --types crate::app_generated [--rpc rpc_experiment] > app_rpc.rs
//!
//! `--bfbs-builtins` keeps the `streaming` and `deprecated` attributes.
//! `--types` is where `flatc --rust --gen-object-api` output lives.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::ExitCode;

use flatbuffers_reflection::reflection;
use rpc_experiment::flatbuffers;

const DEFAULT_TIMEOUT_MS: u64 = 5000;

struct Args {
    input: String,
    types: String,
    rpc: String,
}

fn parse_args() -> Result<Args, String> {
    let mut input = None;
    let mut types = None;
    let mut rpc = "rpc_experiment".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--types" => types = args.next(),
            "--rpc" => rpc = args.next().ok_or("--rpc needs a value")?,
            _ if input.is_none() => input = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    Ok(Args {
        input: input.ok_or("missing input .bfbs")?,
        types: types.ok_or("missing --types")?,
        rpc,
    })
}

// --- The schema as declared ---

struct SchemaDef {
    services: Vec<ServiceDef>,
    servers: Vec<ServerDef>,
}

struct ServiceDef {
    /// Fully qualified, e.g. "CoprocessorProto.Wifi".
    name: String,
    doc: Vec<String>,
    calls: Vec<CallDef>,
}

struct CallDef {
    name: String,
    request: String,
    response: String,
    attributes: BTreeMap<String, String>,
    doc: Vec<String>,
}

/// An enum marked `rpc_server`.
struct ServerDef {
    name: String,
    doc: Vec<String>,
    ubyte: bool,
    entries: Vec<EntryDef>,
}

struct EntryDef {
    name: String,
    value: i64,
    deprecated: bool,
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
fn read(schema: reflection::Schema<'_>) -> SchemaDef {
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

struct Method {
    name: String,
    full_name: String,
    /// Position in the service: the method's wire id.
    number: u8,
    request: String,
    response: String,
    streaming: bool,
    timeout_ms: u64,
    deprecated: bool,
    doc: Vec<String>,
}

struct Service {
    namespace: Vec<String>,
    name: String,
    full_name: String,
    /// Value in its server's enum: the service's wire id.
    id: u8,
    methods: Vec<Method>,
    doc: Vec<String>,
}

struct Server {
    namespace: Vec<String>,
    name: String,
    /// Indexed by service id: the service's index in `Resolved::services`,
    /// `None` for deprecated entries and gaps.
    slots: Vec<Option<usize>>,
    doc: Vec<String>,
}

struct Resolved {
    services: Vec<Service>,
    servers: Vec<Server>,
}

fn split(full: &str) -> (Vec<String>, String) {
    let (namespace, name) = full.rsplit_once('.').unwrap_or(("", full));
    let namespace = namespace.split('.').filter(|p| !p.is_empty()).map(str::to_string).collect();
    (namespace, name.to_string())
}

/// Assigns wire ids: services from the `rpc_server` enums, methods from their
/// position.
fn resolve(schema: &SchemaDef) -> Result<Resolved, String> {
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

// --- Rust output ---

/// flatc's UpperCamel -> snake_case conversion.
fn snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn ident(s: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else",
        "enum", "extern", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "static", "struct", "trait", "type", "unsafe",
        "use", "where", "while", "yield",
    ];
    if KEYWORDS.contains(&s) { format!("r#{s}") } else { s.to_string() }
}

/// Rust path of a flatc-generated table, e.g. `fb::coprocessor_proto::WifiStatus`.
fn type_path(full: &str) -> String {
    let mut parts: Vec<String> = full.split('.').map(str::to_string).collect();
    let name = parts.pop().unwrap();
    let mut path = String::from("fb");
    for p in parts {
        path += "::";
        path += &snake(&p);
    }
    path + "::" + &name
}

fn doc(out: &mut String, indent: &str, lines: &[String]) {
    for l in lines {
        writeln!(out, "{indent}///{l}").unwrap();
    }
}

fn live(methods: &[Method]) -> impl Iterator<Item = &Method> {
    methods.iter().filter(|m| !m.deprecated)
}

fn generate(r: &Resolved, args: &Args, source: &str) -> String {
    let rpc = &args.rpc;
    let mut out = String::new();
    writeln!(out, "// @generated by rpcgen from {source}. Do not edit.").unwrap();
    writeln!(out, "#![allow(clippy::all, unused_imports)]\n").unwrap();
    writeln!(out, "use {rpc}::flatbuffers;").unwrap();
    writeln!(out, "use {rpc}::typed::{{Pack, Table}};").unwrap();
    writeln!(out, "use {} as fb;\n", args.types).unwrap();

    let tables: BTreeSet<&str> = (r.services.iter())
        .flat_map(|s| live(&s.methods))
        .flat_map(|m| [m.request.as_str(), m.response.as_str()])
        .collect();
    for t in &tables {
        let p = type_path(t);
        writeln!(
            out,
            "impl Table for {p}<'static> {{
    type View<'a> = {p}<'a>;
    fn verify(buf: &[u8]) -> Result<{p}<'_>, flatbuffers::InvalidFlatbuffer> {{
        flatbuffers::root::<{p}>(buf)
    }}
}}

impl Pack for {p}T {{
    fn to_bytes(&self) -> Vec<u8> {{
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let root = self.pack(&mut fbb);
        fbb.finish(root, None);
        fbb.finished_data().to_vec()
    }}
}}
"
        )
        .unwrap();
    }

    type Items<'a> = (Vec<&'a Server>, Vec<&'a Service>);
    let mut by_namespace: BTreeMap<&[String], Items<'_>> = BTreeMap::new();
    for srv in &r.servers {
        by_namespace.entry(&srv.namespace).or_default().0.push(srv);
    }
    for s in &r.services {
        by_namespace.entry(&s.namespace).or_default().1.push(s);
    }
    for (namespace, (servers, services)) in by_namespace {
        for n in namespace {
            writeln!(out, "pub mod {} {{", snake(n)).unwrap();
        }
        for srv in servers {
            server_table(&mut out, srv, &r.services, rpc);
        }
        for s in services {
            service(&mut out, s, rpc, &args.types);
        }
        for _ in namespace {
            writeln!(out, "}}").unwrap();
        }
    }
    out
}

fn server_table(out: &mut String, srv: &Server, services: &[Service], rpc: &str) {
    doc(out, "", &srv.doc);
    let name = snake(&srv.name).to_uppercase();
    writeln!(out, "pub static {name}: &{rpc}::router::ServerTable = &[").unwrap();
    for slot in &srv.slots {
        let Some(i) = slot else {
            writeln!(out, "    None,").unwrap();
            continue;
        };
        let s = &services[*i];
        writeln!(
            out,
            "    Some({rpc}::router::ServiceInfo {{ name: {:?}, methods: &[",
            s.full_name
        )
        .unwrap();
        for m in &s.methods {
            if m.deprecated {
                writeln!(out, "        None,").unwrap();
            } else {
                writeln!(
                    out,
                    "        Some({rpc}::router::MethodInfo {{ name: {:?}, streaming: {} }}),",
                    m.name, m.streaming
                )
                .unwrap();
            }
        }
        writeln!(out, "    ] }}),").unwrap();
    }
    writeln!(out, "];\n").unwrap();
}

fn service(out: &mut String, s: &Service, rpc: &str, types: &str) {
    doc(out, "", &s.doc);
    writeln!(
        out,
        "pub mod {} {{
    use {rpc}::MethodId;
    use {rpc}::flatbuffers;
    use {rpc}::proto::Status;
    use {rpc}::router::IncomingCall;
    use {rpc}::server::Server;
    use {rpc}::typed::{{Call, Channel, Pack, Reply, Sink}};

    use {types} as fb;

    /// This service's id in its server's table.
    pub const ID: u8 = {};
",
        snake(&s.name),
        s.id
    )
    .unwrap();
    for m in live(&s.methods) {
        writeln!(out, "    /// `{}`", m.full_name).unwrap();
        let konst = snake(&m.name).to_uppercase();
        writeln!(out, "    pub const {konst}: MethodId = MethodId::new(ID, {});", m.number)
            .unwrap();
    }

    writeln!(out, "\n    pub struct Client<'c>(pub &'c mut {rpc}::client::Client);\n").unwrap();
    writeln!(out, "    impl Client<'_> {{").unwrap();
    for m in live(&s.methods) {
        let (fn_name, konst) = (ident(&snake(&m.name)), snake(&m.name).to_uppercase());
        let (req, resp) = (type_path(&m.request), type_path(&m.response));
        doc(out, "        ", &m.doc);
        if m.streaming {
            writeln!(
                out,
                "        pub fn {fn_name}(&mut self, req: &{req}T, capacity: u16) -> Channel<{resp}<'static>> {{
            Channel::new(self.0.open({konst}, req.to_bytes(), capacity))
        }}"
            )
            .unwrap();
        } else {
            writeln!(
                out,
                "        pub fn {fn_name}(&mut self, req: &{req}T) -> Call<{resp}<'static>> {{
            Call::new(self.0.call({konst}, req.to_bytes(), {}))
        }}",
                m.timeout_ms
            )
            .unwrap();
        }
    }
    writeln!(out, "    }}\n").unwrap();

    writeln!(out, "    pub trait Handler {{").unwrap();
    for m in live(&s.methods) {
        let fn_name = ident(&snake(&m.name));
        let (req, resp) = (type_path(&m.request), type_path(&m.response));
        let (arg, ty) = if m.streaming { ("sink", "Sink") } else { ("reply", "Reply") };
        doc(out, "        ", &m.doc);
        writeln!(
            out,
            "        fn {fn_name}(&mut self, server: &mut Server, {arg}: {ty}<{resp}T>, req: {req}<'_>);"
        )
        .unwrap();
    }
    writeln!(
        out,
        "        /// The client dropped `call_id` before it finished.
        fn cancelled(&mut self, _server: &mut Server, _call_id: u32) {{}}
    }}

    pub struct Service<H>(pub H);

    impl<H: Handler> {rpc}::router::Service for Service<H> {{
        fn id(&self) -> u8 {{
            ID
        }}

        fn call(&mut self, server: &mut Server, call: IncomingCall<'_>) {{
            match call.method {{"
    )
    .unwrap();
    for m in live(&s.methods) {
        let (fn_name, req) = (ident(&snake(&m.name)), type_path(&m.request));
        let (handle, reject) = if m.streaming {
            (
                format!("self.0.{fn_name}(server, Sink::new(call.call_id), req)"),
                "{ let _ = server.end(call.call_id, Status::INVALID_ARGUMENT); }",
            )
        } else {
            (
                format!("self.0.{fn_name}(server, Reply::new(call.call_id), req)"),
                "server.respond(call.call_id, Err(Status::INVALID_ARGUMENT))",
            )
        };
        writeln!(
            out,
            "                {} => match flatbuffers::root::<{req}>(call.payload) {{
                    Ok(req) => {handle},
                    Err(_) => {reject},
                }},",
            m.number
        )
        .unwrap();
    }
    writeln!(
        out,
        "                _ => server.respond(call.call_id, Err(Status::UNIMPLEMENTED)),
            }}
        }}

        fn cancelled(&mut self, server: &mut Server, call_id: u32) {{
            self.0.cancelled(server, call_id)
        }}
    }}
}}
"
    )
    .unwrap();
}

fn main() -> ExitCode {
    let run = || -> Result<String, String> {
        let args = parse_args()?;
        let bfbs = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input))?;
        if !reflection::schema_buffer_has_identifier(&bfbs) {
            return Err(format!("{}: not a binary schema (flatc -b --schema)", args.input));
        }
        let schema =
            reflection::root_as_schema(&bfbs).map_err(|e| format!("{}: {e}", args.input))?;
        let resolved = resolve(&read(schema))?;
        let source = std::path::Path::new(&args.input)
            .file_name()
            .map_or(args.input.clone(), |f| f.to_string_lossy().into_owned());
        Ok(generate(&resolved, &args, &source))
    };
    match run() {
        Ok(code) => {
            print!("{code}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rpcgen: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, deprecated: bool) -> CallDef {
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

    fn service(name: &str, calls: Vec<CallDef>) -> ServiceDef {
        ServiceDef { name: format!("T.{name}"), doc: Vec::new(), calls }
    }

    fn server(name: &str, entries: &[(&str, i64, bool)]) -> ServerDef {
        ServerDef {
            name: format!("T.{name}"),
            doc: Vec::new(),
            ubyte: true,
            entries: (entries.iter())
                .map(|&(name, value, deprecated)| EntryDef { name: name.into(), value, deprecated })
                .collect(),
        }
    }

    fn wifi_and_sonos() -> Vec<ServiceDef> {
        vec![
            service("Wifi", vec![call("Connect", false)]),
            service("Sonos", vec![call("Play", false)]),
        ]
    }

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
    fn deprecated_method_keeps_its_number() {
        let calls = vec![call("Connect", false), call("Scan", true), call("Watch", false)];
        let schema = SchemaDef {
            services: vec![service("Wifi", calls)],
            servers: vec![server("Server", &[("Wifi", 0, false)])],
        };
        let r = resolve(&schema).unwrap();
        let numbers: Vec<_> =
            r.services[0].methods.iter().map(|m| (m.number, m.deprecated)).collect();
        assert_eq!(numbers, [(0, false), (1, true), (2, false)]);

        let args = Args { input: String::new(), types: "t".into(), rpc: "rpc".into() };
        let code = generate(&r, &args, "test");
        assert!(code.contains("pub const WATCH: MethodId = MethodId::new(ID, 2);"), "{code}");
        assert!(!code.contains("SCAN") && !code.contains("fn scan"), "{code}");
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
