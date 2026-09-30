//! flatc's schema parser, linked in (`flatc.cpp`, built by `build.rs`).

use std::ffi::{CStr, CString, c_char, c_void};

unsafe extern "C" {
    fn tether_schema_parse(
        filename: *const c_char,
        include_paths: *const *const c_char,
        schema: *mut *mut u8,
        size: *mut usize,
        messages: *mut *mut c_char,
    ) -> bool;
    fn free(ptr: *mut c_void);
}

/// A binary schema, and the parser's warnings.
pub struct Compiled {
    pub bfbs: Vec<u8>,
    pub warnings: String,
}

fn c_string(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("{s:?} contains a NUL byte"))
}

/// Parses the schema at `path` into a binary schema, as
/// `flatc -b --schema --bfbs-builtins --bfbs-comments -I <include>...` does.
pub fn binary_schema(path: &str, include: &[String]) -> Result<Compiled, String> {
    let filename = c_string(path)?;
    let include: Vec<CString> =
        include.iter().map(|dir| c_string(dir)).collect::<Result<_, _>>()?;
    let mut include_ptrs: Vec<*const c_char> = include.iter().map(|s| s.as_ptr()).collect();
    include_ptrs.push(std::ptr::null());

    let (mut schema, mut size, mut messages) = (std::ptr::null_mut(), 0, std::ptr::null_mut());
    // SAFETY: the strings and the null-terminated list outlive the call. What
    // it returns is malloc'd, copied, then freed once.
    let (ok, bfbs, messages) = unsafe {
        let ok = tether_schema_parse(
            filename.as_ptr(),
            include_ptrs.as_ptr(),
            &mut schema,
            &mut size,
            &mut messages,
        );
        let bfbs = (!schema.is_null()).then(|| std::slice::from_raw_parts(schema, size).to_vec());
        let text =
            (!messages.is_null()).then(|| CStr::from_ptr(messages).to_string_lossy().into_owned());
        free(schema.cast());
        free(messages.cast());
        (ok, bfbs, text.unwrap_or_default())
    };
    match bfbs {
        Some(bfbs) if ok => Ok(Compiled { bfbs, warnings: messages }),
        _ => Err(messages.trim_end().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use flatbuffers_reflection::reflection;

    use super::*;

    fn repo(path: &str) -> String {
        format!("{}/../{path}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Writes `source` to a schema file of its own, for the test `name`.
    fn schema_file(name: &str, source: &str) -> String {
        let dir = std::env::temp_dir().join(format!("tether-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.fbs"));
        std::fs::write(&path, source).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn compiles_a_schema_with_its_includes() {
        let compiled =
            binary_schema(&repo("core/tests/coprocessor.fbs"), &[repo("schema")]).unwrap();
        let schema = reflection::root_as_schema(&compiled.bfbs).unwrap();
        let wifi = schema.services().unwrap().iter().find(|s| s.name() == "CoprocessorProto.Wifi");
        let watch = wifi.unwrap().calls().unwrap().iter().find(|c| c.name() == "Watch").unwrap();
        assert!(watch.documentation().is_some(), "--bfbs-comments");
        let streaming = watch.attributes().unwrap().iter().find(|kv| kv.key() == "streaming");
        assert_eq!(streaming.unwrap().value(), Some("server"), "--bfbs-builtins");
    }

    #[test]
    fn reports_a_missing_include() {
        let err = binary_schema(&repo("core/tests/coprocessor.fbs"), &[]).err().unwrap();
        assert!(err.contains("unable to load include file: tether.fbs"), "{err}");
    }

    #[test]
    fn reports_where_a_schema_is_wrong() {
        let path = schema_file("wrong", "table T {\n  a: int\n}\n");
        let err = binary_schema(&path, &[]).err().unwrap();
        assert!(err.contains("wrong.fbs:3:"), "{err}");
        assert!(err.contains("expecting: ;"), "{err}");
    }

    #[test]
    fn reports_a_missing_file() {
        let err = binary_schema("no/such.fbs", &[]).err().unwrap();
        assert!(err.starts_with("no/such.fbs: "), "{err}");
    }

    #[test]
    fn declares_the_framework_attributes() {
        let source = "enum S: ubyte (rpc_server) { A }\n\
                      table T {}\n\
                      rpc_service A { Get(T): T (timeout_ms: \"1\"); }\n";
        let compiled = binary_schema(&schema_file("undeclared", source), &[]).unwrap();
        let schema = reflection::root_as_schema(&compiled.bfbs).unwrap();
        let server = schema.enums().iter().find(|e| e.name() == "S").unwrap();
        assert!(server.attributes().unwrap().iter().any(|kv| kv.key() == "rpc_server"));
    }

    #[test]
    fn a_schema_may_declare_them_too() {
        binary_schema(&repo("core/tests/coprocessor.fbs"), &[repo("schema")]).unwrap();
        let source = "attribute \"rpc_server\";\nenum S: ubyte (rpc_server) { A }\n";
        binary_schema(&schema_file("declared", source), &[]).unwrap();
    }

    #[test]
    fn returns_warnings() {
        let compiled =
            binary_schema(&schema_file("warns", "table T { Bad: int; }\n"), &[]).unwrap();
        assert!(compiled.warnings.contains("lowercase snake_case"), "{}", compiled.warnings);
    }
}
