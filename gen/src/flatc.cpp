// flatc's schema compiler, as a library: parses a .fbs into a binary schema,
// as `flatc -b --schema --bfbs-builtins --bfbs-comments` does, but reports
// errors instead of exiting. Called from flatc.rs.

#include <exception>
#include <string>
#include <vector>

#include "flatbuffers/idl.h"
#include "flatbuffers/util.h"

struct TetherSchema {
  explicit TetherSchema(const flatbuffers::IDLOptions& opts) : parser(opts) {}

  flatbuffers::Parser parser;
  bool ok = false;
  // The parser's error, or its warnings when `ok`, or an exception's message.
  std::string messages;
};

namespace {

flatbuffers::IDLOptions Options(const std::string& filename) {
  flatbuffers::IDLOptions opts;
  opts.binary_schema_builtins = true;
  opts.binary_schema_comments = true;
  // The paths recorded in the schema are relative to it, as with flatc.
  opts.project_root = flatbuffers::StripFileName(filename);
  return opts;
}

void Parse(TetherSchema& schema, const char* source,
           const std::string& filename, const char* const* include_paths) {
  // flatc also searches the schema's own directory, last.
  const std::string local = flatbuffers::StripFileName(filename);
  std::vector<const char*> paths;
  for (auto p = include_paths; *p != nullptr; ++p) paths.push_back(*p);
  paths.push_back(local.c_str());
  paths.push_back(nullptr);

  schema.ok = schema.parser.Parse(source, paths.data(), filename.c_str());
  schema.messages = schema.parser.error_;
  if (schema.ok) schema.parser.Serialize();
}

}  // namespace

extern "C" {

// Parses `source`, the contents of the file `filename`, searching
// `include_paths` (null-terminated) for its includes. Returns null only if
// out of memory; free the result with tether_schema_free.
TetherSchema* tether_schema_parse(const char* source, const char* filename,
                                  const char* const* include_paths) {
  TetherSchema* schema = nullptr;
  try {
    schema = new TetherSchema(Options(filename));
    Parse(*schema, source, filename, include_paths);
  } catch (const std::exception& e) {
    if (schema == nullptr) return nullptr;
    schema->ok = false;
    schema->messages = e.what();
  }
  return schema;
}

bool tether_schema_ok(const TetherSchema* schema) { return schema->ok; }

// The parser's error if not ok, else its warnings, if any. NUL-terminated.
const char* tether_schema_messages(const TetherSchema* schema) {
  return schema->messages.c_str();
}

// The binary schema, if ok.
const uint8_t* tether_schema_bfbs(const TetherSchema* schema, size_t* size) {
  *size = schema->parser.builder_.GetSize();
  return schema->parser.builder_.GetBufferPointer();
}

void tether_schema_free(TetherSchema* schema) { delete schema; }

}  // extern "C"
