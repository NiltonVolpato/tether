//! What each output language provides, and the walk over the schema they share.

use std::collections::{BTreeMap, BTreeSet};

use crate::schema::{Method, Resolved, Server, Service};

/// One output language. The generated file's order: the preamble, then for each
/// namespace its server tables and services, inside the namespace.
pub trait Backend {
    /// The start of the file: its header, includes, and what each message
    /// table in `tables` (fully qualified names) needs.
    fn preamble(&self, out: &mut String, source: &str, tables: &BTreeSet<&str>);
    fn open_namespace(&self, out: &mut String, namespace: &[String]);
    fn close_namespace(&self, out: &mut String, namespace: &[String]);
    /// The table of the services of an `rpc_server` enum.
    fn server_table(&self, out: &mut String, server: &Server, services: &[Service]);
    fn service(&self, out: &mut String, service: &Service);
}

/// The methods that exist: not tombstones.
pub fn live(methods: &[Method]) -> impl Iterator<Item = &Method> {
    methods.iter().filter(|m| !m.deprecated)
}

/// flatc's UpperCamel -> snake_case conversion.
pub fn snake(s: &str) -> String {
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

pub fn generate(backend: &dyn Backend, r: &Resolved, source: &str) -> String {
    let mut out = String::new();
    let tables: BTreeSet<&str> = (r.services.iter())
        .flat_map(|s| live(&s.methods))
        .flat_map(|m| [m.request.as_str(), m.response.as_str()])
        .collect();
    backend.preamble(&mut out, source, &tables);

    type Items<'a> = (Vec<&'a Server>, Vec<&'a Service>);
    let mut by_namespace: BTreeMap<&[String], Items<'_>> = BTreeMap::new();
    for srv in &r.servers {
        by_namespace.entry(&srv.namespace).or_default().0.push(srv);
    }
    for s in &r.services {
        by_namespace.entry(&s.namespace).or_default().1.push(s);
    }
    for (namespace, (servers, services)) in by_namespace {
        backend.open_namespace(&mut out, namespace);
        for srv in servers {
            backend.server_table(&mut out, srv, &r.services);
        }
        for s in services {
            backend.service(&mut out, s);
        }
        backend.close_namespace(&mut out, namespace);
    }
    out
}
