//! The public interface of the RPC framework: traits and data objects only.
//!
//! Application code and generated code depend on this crate alone. The
//! framework implements its traits over the UART link; tests implement them
//! with fakes.
//!
//! - Client (S3): a [`Transport`] starts calls. Generated clients wrap it and
//!   return typed [`Call`]s and [`Channel`]s; dropping either cancels it.
//! - Server (co-processor): generated `Handler` traits get typed [`Reply`]s
//!   and [`Sink`]s, whose implementations come from [`ServerTypes`]. A
//!   generated `Service` adapts a handler to the byte-level [`Service`] trait
//!   the framework dispatches to.
//!
//! Traits take `&self` where the framework has shared state, so each
//! implementation chooses its own interior mutability.

#![no_std]

extern crate alloc;

mod client;
mod descriptor;
mod message;
mod server;
mod status;

pub use client::{Call, Channel, RawCall, RawChannel, Transport};
pub use descriptor::{CallId, MethodId, MethodInfo, ServerTable, ServiceInfo, lookup};
pub use flatbuffers;
pub use message::{Message, Pack, Table};
pub use server::{RawReply, RawSink, Reply, ServerTypes, Service, Sink, StreamError};
pub use status::Status;
