//! Generates typed Rust clients and handler traits from the `rpc_service`s of
//! a flatbuffers binary schema:
//!
//!   flatc -b --schema --bfbs-builtins --bfbs-comments app.fbs
//!   rpcgen app.bfbs --types crate::app_generated [--rpc rpc_experiment] > app_rpc.rs
//!
//! `--bfbs-builtins` keeps the `streaming` attribute. `--types` is where
//! `flatc --rust --gen-object-api` output lives.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::ExitCode;

use flatbuffers_reflection::reflection;
use rpc_experiment::{flatbuffers, method_id};

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

struct Method {
    name: String,
    full_name: String,
    id: u32,
    request: String,
    response: String,
    streaming: bool,
    timeout_ms: u64,
    doc: Vec<String>,
}

struct Service {
    namespace: Vec<String>,
    name: String,
    doc: Vec<String>,
    methods: Vec<Method>,
}

/// An `rpc_service` as declared, before validation.
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

fn strings<'a>(
    v: Option<flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<&'a str>>>,
) -> Vec<String> {
    v.iter().flatten().map(str::to_string).collect()
}

/// Extracts the services of a binary schema.
fn read(schema: reflection::Schema<'_>) -> Vec<ServiceDef> {
    let services = schema.services().into_iter().flatten();
    services
        .map(|s| ServiceDef {
            name: s.name().to_string(),
            doc: strings(s.documentation()),
            calls: (s.calls().into_iter().flatten())
                .map(|c| CallDef {
                    name: c.name().to_string(),
                    request: c.request().name().to_string(),
                    response: c.response().name().to_string(),
                    attributes: (c.attributes().into_iter().flatten())
                        .map(|kv| (kv.key().to_string(), kv.value().unwrap_or("").to_string()))
                        .collect(),
                    doc: strings(c.documentation()),
                })
                .collect(),
        })
        .collect()
}

/// Validates attributes and assigns method ids.
fn resolve(defs: &[ServiceDef]) -> Result<Vec<Service>, String> {
    let mut services = Vec::new();
    let mut ids: BTreeMap<u32, String> = BTreeMap::new();
    for s in defs {
        let full = s.name.as_str();
        let (namespace, name) = full.rsplit_once('.').unwrap_or(("", full));
        let mut methods = Vec::new();
        for c in &s.calls {
            let name = c.name.clone();
            let attrs = &c.attributes;
            let streaming = match attrs.get("streaming").map(String::as_str) {
                None | Some("none") => false,
                Some("server") => true,
                Some(other) => {
                    return Err(format!("{full}.{name}: streaming \"{other}\" unsupported"));
                }
            };
            let timeout_ms = match attrs.get("timeout_ms") {
                None => DEFAULT_TIMEOUT_MS,
                Some(t) => t.parse().map_err(|_| format!("{full}.{name}: bad timeout_ms {t}"))?,
            };
            let full_name = format!("{full}/{name}");
            let id = method_id(&full_name);
            if let Some(other) = ids.insert(id, full_name.clone()) {
                return Err(format!("method id collision: {other} and {full_name}"));
            }
            methods.push(Method {
                name,
                full_name,
                id,
                request: c.request.clone(),
                response: c.response.clone(),
                streaming,
                timeout_ms,
                doc: c.doc.clone(),
            });
        }
        services.push(Service {
            namespace: namespace.split('.').filter(|p| !p.is_empty()).map(str::to_string).collect(),
            name: name.to_string(),
            doc: s.doc.clone(),
            methods,
        });
    }
    Ok(services)
}

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

fn generate(services: &[Service], args: &Args, source: &str) -> String {
    let rpc = &args.rpc;
    let mut out = String::new();
    writeln!(out, "// @generated by rpcgen from {source}. Do not edit.").unwrap();
    writeln!(out, "#![allow(clippy::all, unused_imports)]\n").unwrap();
    writeln!(out, "use {rpc}::flatbuffers;").unwrap();
    writeln!(out, "use {rpc}::typed::{{Pack, Table}};").unwrap();
    writeln!(out, "use {} as fb;\n", args.types).unwrap();

    let tables: BTreeSet<&str> = services
        .iter()
        .flat_map(|s| &s.methods)
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

    let mut table = phf_codegen::Map::new();
    table.phf_path(format!("{rpc}::phf"));
    for (i, s) in services.iter().enumerate() {
        for m in &s.methods {
            let value = format!(
                "{rpc}::router::Method {{ name: {:?}, service: {i}, streaming: {} }}",
                m.full_name, m.streaming
            );
            table.entry(m.id, value);
        }
    }
    writeln!(out, "/// Every method in the schema, by method id.").unwrap();
    writeln!(out, "pub static METHODS: {rpc}::router::MethodTable = {};\n", table.build()).unwrap();

    let mut by_namespace: BTreeMap<&[String], Vec<(usize, &Service)>> = BTreeMap::new();
    for (i, s) in services.iter().enumerate() {
        by_namespace.entry(&s.namespace).or_default().push((i, s));
    }
    for (namespace, services) in by_namespace {
        for n in namespace {
            writeln!(out, "pub mod {} {{", snake(n)).unwrap();
        }
        for (i, s) in services {
            service(&mut out, i, s, rpc, &args.types);
        }
        for _ in namespace {
            writeln!(out, "}}").unwrap();
        }
    }
    out
}

fn service(out: &mut String, index: usize, s: &Service, rpc: &str, types: &str) {
    doc(out, "", &s.doc);
    writeln!(
        out,
        "pub mod {} {{
    use {rpc}::flatbuffers;
    use {rpc}::proto::Status;
    use {rpc}::router::IncomingCall;
    use {rpc}::server::Server;
    use {rpc}::typed::{{Call, Channel, Pack, Reply, Sink}};

    use {types} as fb;

    /// This service's index in `METHODS`.
    pub const INDEX: usize = {index};
",
        snake(&s.name)
    )
    .unwrap();
    for m in &s.methods {
        writeln!(out, "    /// `{}`", m.full_name).unwrap();
        writeln!(out, "    pub const {}: u32 = {:#010x};", snake(&m.name).to_uppercase(), m.id)
            .unwrap();
    }

    writeln!(out, "\n    pub struct Client<'c>(pub &'c mut {rpc}::client::Client);\n").unwrap();
    writeln!(out, "    impl Client<'_> {{").unwrap();
    for m in &s.methods {
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
    for m in &s.methods {
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
        fn index(&self) -> usize {{
            INDEX
        }}

        fn call(&mut self, server: &mut Server, call: IncomingCall<'_>) {{
            match call.method {{"
    )
    .unwrap();
    for m in &s.methods {
        let (fn_name, konst, req) =
            (ident(&snake(&m.name)), snake(&m.name).to_uppercase(), type_path(&m.request));
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
            "                {konst} => match flatbuffers::root::<{req}>(call.payload) {{
                    Ok(req) => {handle},
                    Err(_) => {reject},
                }},"
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
        let services = resolve(&read(schema))?;
        let source = std::path::Path::new(&args.input)
            .file_name()
            .map_or(args.input.clone(), |f| f.to_string_lossy().into_owned());
        Ok(generate(&services, &args, &source))
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
    use std::collections::HashMap;

    use super::*;

    /// Two methods of service `T.S` with the same id. By the birthday bound,
    /// 32-bit ids collide after ~82k names.
    fn colliding_names() -> (String, String) {
        let mut seen = HashMap::new();
        for i in 0.. {
            let name = format!("M{i}");
            if let Some(other) = seen.insert(method_id(&format!("T.S/{name}")), name.clone()) {
                return (other, name);
            }
        }
        unreachable!()
    }

    fn service(calls: &[&str]) -> ServiceDef {
        let call = |name: &&str| CallDef {
            name: name.to_string(),
            request: "T.Req".into(),
            response: "T.Resp".into(),
            attributes: BTreeMap::new(),
            doc: Vec::new(),
        };
        ServiceDef { name: "T.S".into(), doc: Vec::new(), calls: calls.iter().map(call).collect() }
    }

    #[test]
    fn method_id_collision_names_both_methods() {
        let (a, b) = colliding_names();
        let err = resolve(&[service(&[&a, &b])]).err().unwrap();
        assert!(err.contains(&format!("T.S/{a}")) && err.contains(&format!("T.S/{b}")), "{err}");
    }

    #[test]
    #[should_panic(expected = "duplicate key")]
    fn method_table_rejects_colliding_ids() {
        // Bypasses resolve()'s check to reach the phf table's own guard.
        let (a, b) = colliding_names();
        let args = Args { input: String::new(), types: "t".into(), rpc: "rpc".into() };
        let mut services = resolve(&[service(&[&a])]).unwrap();
        services.extend(resolve(&[service(&[&b])]).unwrap());
        generate(&services, &args, "test");
    }
}
