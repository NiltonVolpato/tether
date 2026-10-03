//! Generates typed clients and handlers from the `rpc_service`s of a
//! flatbuffers schema, and a table for each `rpc_server` enum. `--help` has
//! the usage and examples.
//!
//! flatc's parser is linked in, so this needs no flatc. The generated Rust
//! depends only on the `tether` crate.

mod backend;
mod cpp;
mod flatc;
mod rust;
mod schema;

use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, ValueEnum};
use flatbuffers_reflection::reflection;

const EXAMPLES: &str = "\
Examples:
  # Rust: a typed client, and handler traits and services, over the types of
  # `flatc --rust --gen-object-api`.
  tether-gen app.fbs -I tether/schema --types crate::generated::app_generated > app_rpc.rs

  # C++: a server's table, handler interfaces and services, over the header of
  # `flatc --cpp --scoped-enums`.
  tether-gen app.fbs -I tether/schema --lang cpp --include app_generated.h > app_rpc.h";

/// Generates typed RPC code from the `rpc_service`s and `rpc_server` enums of a
/// flatbuffers schema, and writes it to stdout.
#[derive(Parser)]
#[command(version, after_help = EXAMPLES)]
struct Cli {
    /// The schema to generate from.
    #[arg(value_name = "SCHEMA.fbs")]
    input: String,

    /// A directory to search for included schemas, as flatc's -I. Repeatable.
    #[arg(short = 'I', value_name = "DIR")]
    include_dirs: Vec<String>,

    /// The language to generate.
    #[arg(long, value_enum, default_value_t = Lang::Rust)]
    lang: Lang,

    /// Module path of the flatc-generated types, as the generated code names
    /// it (e.g. crate::generated::app_generated).
    #[arg(long, value_name = "PATH", help_heading = "Rust")]
    types: Option<String>,

    /// Path of the tether crate, as the generated code names it [default: tether].
    #[arg(long, value_name = "PATH", help_heading = "Rust")]
    tether: Option<String>,

    /// The flatc-generated header, as the generated code #includes it.
    #[arg(long, value_name = "HEADER", help_heading = "C++")]
    include: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Lang {
    /// A client, and handler traits and services.
    Rust,
    /// A server: a table per rpc_server, and handler interfaces and services.
    Cpp,
}

impl Cli {
    /// The backend for `--lang`, which needs its own options and refuses the
    /// other language's. (clap's `required_if_eq` misses a defaulted `--lang`.)
    fn backend(&self) -> Result<Box<dyn backend::Backend>, clap::Error> {
        let lang = self.lang.to_possible_value().expect("no skipped values");
        let error = |kind, message| Cli::command().error(kind, message);
        let missing = |arg| {
            error(
                ErrorKind::MissingRequiredArgument,
                format!("--lang {} needs {arg}", lang.get_name()),
            )
        };
        let stray = |arg| {
            error(
                ErrorKind::ArgumentConflict,
                format!("{arg} doesn't apply to --lang {}", lang.get_name()),
            )
        };
        match self.lang {
            Lang::Rust => {
                if self.include.is_some() {
                    return Err(stray("--include"));
                }
                Ok(Box::new(rust::Rust {
                    types: self.types.clone().ok_or_else(|| missing("--types <PATH>"))?,
                    tether: self.tether.clone().unwrap_or_else(|| "tether".into()),
                }))
            }
            Lang::Cpp => {
                if self.types.is_some() {
                    return Err(stray("--types"));
                }
                if self.tether.is_some() {
                    return Err(stray("--tether"));
                }
                let include = self.include.clone().ok_or_else(|| missing("--include <HEADER>"))?;
                Ok(Box::new(cpp::Cpp { include }))
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let backend = cli.backend().unwrap_or_else(|e| e.exit());
    let run = || -> Result<String, String> {
        let compiled = flatc::binary_schema(&cli.input, &cli.include_dirs)?;
        eprint!("{}", compiled.warnings);
        let schema = reflection::root_as_schema(&compiled.bfbs)
            .map_err(|e| format!("{}: {e}", cli.input))?;
        let resolved = schema::resolve(&schema::read(schema))?;
        let source = std::path::Path::new(&cli.input)
            .file_name()
            .map_or(cli.input.clone(), |f| f.to_string_lossy().into_owned());
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
