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

/// A method's wire id: its service's value in the server's `rpc_server` enum
/// and its position within that `rpc_service`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodId {
    pub service: u8,
    pub method: u8,
}

impl MethodId {
    pub const fn new(service: u8, method: u8) -> Self {
        Self { service, method }
    }
}

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
mod rpc_generated;

pub mod proto {
    pub use crate::rpc_generated::common::Status;
    pub use crate::rpc_generated::rpc::*;
}
