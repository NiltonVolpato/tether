//! Builds flatc's schema parser (from the `flatbuffers` submodule) and the
//! shim tether-gen calls it through, `src/flatc.cpp`.

use std::path::Path;

const SOURCES: &[&str] = &["idl_parser.cpp", "util.cpp"];

fn main() {
    let root = Path::new("flatbuffers");
    if !root.join("include").exists() {
        panic!("gen/flatbuffers is empty: run `git submodule update --init`");
    }
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").include(root.join("include")).warnings(false);
    build.file("src/flatc.cpp");
    for source in SOURCES {
        build.file(root.join("src").join(source));
    }
    build.compile("flatc");

    println!("cargo::rerun-if-changed=src/flatc.cpp");
    println!("cargo::rerun-if-changed=flatbuffers/include");
    println!("cargo::rerun-if-changed=flatbuffers/src");
}
