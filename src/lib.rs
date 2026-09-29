//! Sans-IO implementation of the `api` traits for the S3 <-> co-processor
//! link: no UART, no executor, no clock. Callers feed received bytes in,
//! pull bytes to transmit out, and pass the current time in milliseconds.
//!
//! Shared state uses `RefCell` for now: the client and the server each
//! assume everything that touches them runs on one executor.

#![no_std]

extern crate alloc;

pub mod client;
pub mod frame;
pub mod link;
pub mod router;
pub mod server;

pub use api;

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
mod rpc_generated;

pub mod proto {
    pub use crate::rpc_generated::common::Status;
    pub use crate::rpc_generated::rpc::*;
}

/// The header's status for a result.
fn wire_status(result: Result<(), api::Status>) -> proto::Status {
    proto::Status(match result {
        Ok(()) => 0,
        Err(status) => status.code() as i8,
    })
}

fn api_status(status: proto::Status) -> Result<(), api::Status> {
    api::Status::from_code(status.0 as u8)
}
