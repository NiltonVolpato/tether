//! Sans-IO RPC core for the S3 <-> co-processor link: no UART, no executor,
//! no clock. Callers feed received bytes in, pull bytes to transmit out, and
//! pass the current time in milliseconds.

#![no_std]

extern crate alloc;

pub mod client;
pub mod frame;
pub mod link;
pub mod router;
pub mod server;
pub mod typed;

pub use flatbuffers;
pub use phf;

/// FNV-1a (32-bit) of a method's full name, e.g. "CoprocessorProto.Wifi/Connect".
pub const fn method_id(full_name: &str) -> u32 {
    let bytes = full_name.as_bytes();
    let mut hash = 0x811c_9dc5u32;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
mod rpc_generated;

pub mod proto {
    pub use crate::rpc_generated::common::Status;
    pub use crate::rpc_generated::rpc::*;
}
