//! Generates typed clients and handler traits from the `rpc_service`s of a
//! flatbuffers schema, and a table for each `rpc_server` enum:
//!
//!   tether-gen app.fbs [-I dir]... --types crate::app_generated [--tether tether] > app_rpc.rs
//!
//! flatc's parser is linked in, so this needs no flatc. `--types` is where
//! `flatc --rust --gen-object-api` output lives. The generated code depends
//! only on the `tether` crate.

mod backend;
mod flatc;
mod rust;
mod schema;

use std::process::ExitCode;

use flatbuffers_reflection::reflection;

struct Args {
    input: String,
    /// Where to look for included schemas, as flatc's `-I`.
    include: Vec<String>,
    types: String,
    /// Path of the `tether` crate in the generated code.
    tether: String,
}

fn parse_args() -> Result<Args, String> {
    let mut input = None;
    let mut include = Vec::new();
    let mut types = None;
    let mut tether = "tether".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-I" => include.push(args.next().ok_or("-I needs a directory")?),
            "--types" => types = args.next(),
            "--tether" => tether = args.next().ok_or("--tether needs a value")?,
            _ if input.is_none() => input = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    Ok(Args {
        input: input.ok_or("missing input .fbs")?,
        include,
        types: types.ok_or("missing --types")?,
        tether,
    })
}

fn main() -> ExitCode {
    let run = || -> Result<String, String> {
        let args = parse_args()?;
        let compiled = flatc::binary_schema(&args.input, &args.include)?;
        eprint!("{}", compiled.warnings);
        let schema = reflection::root_as_schema(&compiled.bfbs)
            .map_err(|e| format!("{}: {e}", args.input))?;
        let resolved = schema::resolve(&schema::read(schema))?;
        let source = std::path::Path::new(&args.input)
            .file_name()
            .map_or(args.input.clone(), |f| f.to_string_lossy().into_owned());
        let backend = rust::Rust { types: args.types, tether: args.tether };
        Ok(backend::generate(&backend, &resolved, &source))
    };
    match run() {
        Ok(code) => {
            print!("{code}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tether-gen: {e}");
            ExitCode::FAILURE
        }
    }
}
