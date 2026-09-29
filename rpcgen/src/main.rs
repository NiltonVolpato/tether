//! Generates typed Rust clients and handler traits from the `rpc_service`s of
//! a flatbuffers schema, read as JSON:
//!
//!   flatc -b --schema --bfbs-builtins --bfbs-comments app.fbs
//!   flatc --json --strict-json --raw-binary reflection.fbs -- app.bfbs
//!   rpcgen app.json --types crate::app_generated [--rpc rpc_experiment] > app_rpc.rs
//!
//! `--types` is where `flatc --rust --gen-object-api` output lives.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::ExitCode;

use rpc_experiment::method_id;
use serde_json::Value;

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
        input: input.ok_or("missing input JSON")?,
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

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn attributes(v: &Value) -> BTreeMap<String, String> {
    v["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["key"].as_str()?.to_string(), a["value"].as_str()?.to_string())))
        .collect()
}

fn parse(schema: &Value) -> Result<Vec<Service>, String> {
    let mut services = Vec::new();
    let mut ids: BTreeMap<u32, String> = BTreeMap::new();
    for s in schema["services"].as_array().into_iter().flatten() {
        let full = s["name"].as_str().ok_or("service without a name")?;
        let (namespace, name) = full.rsplit_once('.').unwrap_or(("", full));
        let mut methods = Vec::new();
        for c in s["calls"].as_array().into_iter().flatten() {
            let name = c["name"].as_str().ok_or("call without a name")?.to_string();
            let attrs = attributes(c);
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
                request: c["request"]["name"].as_str().ok_or("call without request")?.into(),
                response: c["response"]["name"].as_str().ok_or("call without response")?.into(),
                streaming,
                timeout_ms,
                doc: strings(&c["documentation"]),
            });
        }
        services.push(Service {
            namespace: namespace.split('.').filter(|p| !p.is_empty()).map(str::to_string).collect(),
            name: name.to_string(),
            doc: strings(&s["documentation"]),
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

    let mut by_namespace: BTreeMap<&[String], Vec<&Service>> = BTreeMap::new();
    for s in services {
        by_namespace.entry(&s.namespace).or_default().push(s);
    }
    for (namespace, services) in by_namespace {
        for n in namespace {
            writeln!(out, "pub mod {} {{", snake(n)).unwrap();
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

fn service(out: &mut String, s: &Service, rpc: &str, types: &str) {
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
        fn method(&self, method: u32) -> Option<bool> {{
            match method {{"
    )
    .unwrap();
    for m in &s.methods {
        writeln!(
            out,
            "                {} => Some({}),",
            snake(&m.name).to_uppercase(),
            m.streaming
        )
        .unwrap();
    }
    writeln!(
        out,
        "                _ => None,
            }}
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
        let text =
            std::fs::read_to_string(&args.input).map_err(|e| format!("{}: {e}", args.input))?;
        let schema: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let services = parse(&schema)?;
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
