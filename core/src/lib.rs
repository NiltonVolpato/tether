//! Sans-IO implementation of the `tether` traits for the S3 <-> co-processor
//! link: no UART, no executor, no clock. Callers feed received bytes in,
//! pull bytes to transmit out, and pass the current time in milliseconds.
//!
//! Shared state uses `RefCell` and `Cell`: the client and the server each
//! assume everything that touches them, their I/O task and the apps, runs on
//! one executor. The I/O task sleeps until `next_deadline` or until it's
//! notified (`notify`), so it never polls.

#![no_std]

extern crate alloc;

pub mod client;
pub mod frame;
pub mod link;
pub mod notify;
pub mod router;
pub mod server;

pub use tether;

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
mod wire_generated;

/// The wire format's types, generated from `schema/wire.fbs`.
pub mod wire {
    pub use crate::wire_generated::tether::wire::*;
}

/// The header's status for a result.
fn wire_status(result: Result<(), tether::Status>) -> wire::Status {
    wire::Status(match result {
        Ok(()) => 0,
        Err(status) => status.code() as i8,
    })
}

fn tether_status(status: wire::Status) -> Result<(), tether::Status> {
    tether::Status::from_code(status.0 as u8)
}
