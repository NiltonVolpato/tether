//! flatc's schema parser, linked in (`flatc.cpp`, built by `build.rs`).

use std::ffi::{CStr, CString, c_char};

#[repr(C)]
struct TetherSchema {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn tether_schema_parse(
        source: *const c_char,
        filename: *const c_char,
        include_paths: *const *const c_char,
    ) -> *mut TetherSchema;
    fn tether_schema_ok(schema: *const TetherSchema) -> bool;
    fn tether_schema_messages(schema: *const TetherSchema) -> *const c_char;
    fn tether_schema_bfbs(schema: *const TetherSchema, size: *mut usize) -> *const u8;
    fn tether_schema_free(schema: *mut TetherSchema);
}

/// A binary schema, and the parser's warnings.
pub struct Compiled {
    pub bfbs: Vec<u8>,
    pub warnings: String,
}

fn c_string(s: &str, what: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("{what} contains a NUL byte"))
}

/// Parses the schema at `path` into a binary schema, as
/// `flatc -b --schema --bfbs-builtins --bfbs-comments -I <include>...` does.
pub fn binary_schema(path: &str, include: &[String]) -> Result<Compiled, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let source = c_string(&source, path)?;
    let filename = c_string(path, "the schema's path")?;
    let include: Vec<CString> = (include.iter())
        .map(|dir| c_string(dir, "an include path"))
        .collect::<Result<_, _>>()?;
    let mut include_ptrs: Vec<*const c_char> = include.iter().map(|s| s.as_ptr()).collect();
    include_ptrs.push(std::ptr::null());

    // SAFETY: every pointer is a NUL-terminated string, or the list's null
    // terminator, that outlives the call; the result is freed once, below.
    unsafe {
        let schema = tether_schema_parse(source.as_ptr(), filename.as_ptr(), include_ptrs.as_ptr());
        if schema.is_null() {
            return Err(format!("{path}: out of memory"));
        }
        let messages = CStr::from_ptr(tether_schema_messages(schema)).to_string_lossy();
        let result = if tether_schema_ok(schema) {
            let mut size = 0;
            let bfbs = tether_schema_bfbs(schema, &mut size);
            let bfbs = std::slice::from_raw_parts(bfbs, size).to_vec();
            Ok(Compiled { bfbs, warnings: messages.into_owned() })
        } else {
            Err(messages.trim_end().to_string())
        };
        tether_schema_free(schema);
        result
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
}
