// flatc's schema compiler, as a library: parses a .fbs into a binary schema,
// as `flatc -b --schema --bfbs-builtins --bfbs-comments` does, but reports
// errors instead of exiting. Called from flatc.rs.

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>

#include "flatbuffers/idl.h"
#include "flatbuffers/util.h"

namespace {

// A malloc'd copy, for the caller to free.
char* Copy(const std::string& s) {
  if (s.empty()) return nullptr;
  auto copy = static_cast<char*>(std::malloc(s.size() + 1));
  if (copy != nullptr) std::memcpy(copy, s.c_str(), s.size() + 1);
  return copy;
}

bool Parse(const char* filename, const char* const* include_paths,
           uint8_t** schema, size_t* size, std::string& messages) {
  std::string source;
  if (!flatbuffers::LoadFile(filename, false, &source)) {
    messages = std::string(filename) + ": unable to load file";
    return false;
  }
  flatbuffers::IDLOptions opts;
  opts.binary_schema_builtins = true;
  opts.binary_schema_comments = true;
  // The paths recorded in the schema are relative to it, as with flatc.
  opts.project_root = flatbuffers::StripFileName(filename);
  flatbuffers::Parser parser(opts);
  // What tether.fbs declares, so schemas needn't include it (they still may).
  parser.known_attributes_["rpc_server"] = false;
  parser.known_attributes_["timeout_ms"] = false;

  // Includes are looked up next to the including file, then in include_paths.
  // Parse takes a non-const list it doesn't modify.
  const bool ok = parser.Parse(
      source.c_str(), const_cast<const char**>(include_paths), filename);
  messages = parser.error_;
  if (!ok) return false;

  parser.Serialize();
  *size = parser.builder_.GetSize();
  *schema = static_cast<uint8_t*>(std::malloc(*size));
  if (*schema == nullptr) {
    messages = "out of memory";
    return false;
  }
  std::memcpy(*schema, parser.builder_.GetBufferPointer(), *size);
  return true;
}

}  // namespace

// Parses the schema in `filename`, looking for its includes in
// `include_paths` (null-terminated), as flatc's -I. On success, sets `schema`
// and `size` to the binary schema and returns true; otherwise returns false.
// Either way, sets `messages` to the error or warnings, or null if there are
// none. The caller frees `schema` and `messages`.
extern "C" bool tether_schema_parse(const char* filename,
                                    const char* const* include_paths,
                                    uint8_t** schema, size_t* size,
                                    char** messages) {
  *schema = nullptr;
  *size = 0;
  std::string text;
  bool ok = false;
  try {
    ok = Parse(filename, include_paths, schema, size, text);
  } catch (const std::exception& e) {
    ok = false;
    text = e.what();
  }
  *messages = Copy(text);
  return ok;
}
