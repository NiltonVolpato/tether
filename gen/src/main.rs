//! Generates typed clients and handlers from the `rpc_service`s of a
//! flatbuffers schema, and a table for each `rpc_server` enum:
//!
//!   tether-gen app.fbs [-I dir]... --types crate::app_generated [--tether tether] > app_rpc.rs
//!   tether-gen app.fbs [-I dir]... --lang cpp --include app_generated.h > app_rpc.h
//!
//! flatc's parser is linked in, so this needs no flatc. `--types` is where
//! `flatc --rust --gen-object-api` output lives, `--include` the header of
//! `flatc --cpp` (the C++ output is for servers only). The generated Rust
//! depends only on the `tether` crate.

mod backend;
mod cpp;
mod flatc;
mod rust;
mod schema;

use std::process::ExitCode;

use flatbuffers_reflection::reflection;

enum Lang {
    Rust {
        /// Where the flatc-generated types are, as a path in the generated code.
        types: String,
        /// Path of the `tether` crate in the generated code.
        tether: String,
    },
    Cpp {
        /// The flatc-generated header, as it's `#include`d.
        include: String,
    },
}

struct Args {
    input: String,
    /// Where to look for included schemas, as flatc's `-I`.
    include: Vec<String>,
    lang: Lang,
}

fn parse_args() -> Result<Args, String> {
    let mut input = None;
    let mut include = Vec::new();
    let mut lang = "rust".to_string();
    let mut types = None;
    let mut tether = "tether".to_string();
    let mut header = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-I" => include.push(args.next().ok_or("-I needs a directory")?),
            "--lang" => lang = args.next().ok_or("--lang needs rust or cpp")?,
            "--types" => types = args.next(),
            "--tether" => tether = args.next().ok_or("--tether needs a value")?,
            "--include" => header = args.next(),
            _ if input.is_none() => input = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let lang = match lang.as_str() {
        "rust" => Lang::Rust { types: types.ok_or("missing --types")?, tether },
        "cpp" => Lang::Cpp { include: header.ok_or("missing --include")? },
        other => return Err(format!("unknown language {other}: rust or cpp")),
    };
    Ok(Args { input: input.ok_or("missing input .fbs")?, include, lang })
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
        let backend: Box<dyn backend::Backend> = match args.lang {
            Lang::Rust { types, tether } => Box::new(rust::Rust { types, tether }),
            Lang::Cpp { include } => Box::new(cpp::Cpp { include }),
        };
        Ok(backend::generate(&*backend, &resolved, &source))
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
