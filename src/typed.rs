//! Typed wrappers over the byte-level client and server, used by generated code.

use alloc::vec::Vec;
use core::marker::PhantomData;

use flatbuffers::InvalidFlatbuffer;

use crate::client;
use crate::proto::Status;
use crate::server::{Server, StreamError};

/// A flatbuffer table used as a message. Implemented for the table type at
/// `'static` (e.g. `WifiStatus<'static>`); `View<'a>` is the same table
/// borrowing a buffer.
pub trait Table {
    type View<'a>;
    fn verify(buf: &[u8]) -> Result<Self::View<'_>, InvalidFlatbuffer>;
}

/// An object-API type (e.g. `WifiStatusT`) that serializes to a finished
/// flatbuffer.
pub trait Pack {
    fn to_bytes(&self) -> Vec<u8>;
}

/// An owned, verified message.
pub struct Message<T: Table> {
    buf: Vec<u8>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Table> Message<T> {
    pub fn new(buf: Vec<u8>) -> Result<Self, InvalidFlatbuffer> {
        T::verify(&buf)?;
        Ok(Self { buf, _t: PhantomData })
    }

    pub fn get(&self) -> T::View<'_> {
        T::verify(&self.buf).expect("verified in new()")
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// A pending unary call. Dropping it before the response cancels the call.
pub struct Call<T: Table> {
    inner: client::Call,
    _t: PhantomData<fn() -> T>,
}

impl<T: Table> Call<T> {
    pub fn new(inner: client::Call) -> Self {
        Self { inner, _t: PhantomData }
    }

    /// A response that doesn't verify is reported as DATA_LOSS.
    pub fn try_result(&self) -> Option<Result<Message<T>, Status>> {
        let result = self.inner.try_result()?;
        Some(result.and_then(|buf| Message::new(buf).map_err(|_| Status::DATA_LOSS)))
    }

    pub async fn result(self) -> Result<Message<T>, Status> {
        let buf = self.inner.result().await?;
        Message::new(buf).map_err(|_| Status::DATA_LOSS)
    }
}

/// Receiving end of a server-streaming call. Dropping it cancels the call.
pub struct Channel<T: Table> {
    inner: client::Channel,
    _t: PhantomData<fn() -> T>,
}

impl<T: Table> Channel<T> {
    pub fn new(inner: client::Channel) -> Self {
        Self { inner, _t: PhantomData }
    }

    /// Next item; `None` once the server ended the channel (see `end_status`).
    pub fn try_recv(&self) -> Option<Result<Message<T>, InvalidFlatbuffer>> {
        self.inner.try_recv().map(Message::new)
    }

    pub async fn recv(&self) -> Option<Result<Message<T>, InvalidFlatbuffer>> {
        self.inner.recv().await.map(Message::new)
    }

    pub fn end_status(&self) -> Option<Status> {
        self.inner.end_status()
    }
}

/// Responds once to a unary call.
pub struct Reply<T: Pack> {
    call_id: u32,
    _t: PhantomData<fn(&T)>,
}

impl<T: Pack> Reply<T> {
    pub fn new(call_id: u32) -> Self {
        Self { call_id, _t: PhantomData }
    }

    pub fn call_id(&self) -> u32 {
        self.call_id
    }

    /// Does nothing if the client cancelled meanwhile.
    pub fn send(self, server: &mut Server, result: Result<&T, Status>) {
        server.respond(self.call_id, result.map(Pack::to_bytes));
    }
}

/// The server's end of a channel. Handlers keep it for as long as the
/// stream lasts; once the client cancels, every operation fails with `Closed`.
pub struct Sink<T: Pack> {
    call_id: u32,
    _t: PhantomData<fn(&T)>,
}

impl<T: Pack> Clone for Sink<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Pack> Copy for Sink<T> {}

impl<T: Pack> Sink<T> {
    pub fn new(call_id: u32) -> Self {
        Self { call_id, _t: PhantomData }
    }

    pub fn call_id(&self) -> u32 {
        self.call_id
    }

    pub fn send(&self, server: &mut Server, item: &T) -> Result<(), StreamError> {
        server.send(self.call_id, item.to_bytes())
    }

    pub fn set_latest(&self, server: &mut Server, item: &T) -> Result<(), StreamError> {
        server.set_latest(self.call_id, item.to_bytes())
    }

    pub fn end(&self, server: &mut Server, status: Status) -> Result<(), StreamError> {
        server.end(self.call_id, status)
    }

    pub fn credit(&self, server: &Server) -> Option<u16> {
        server.credit(self.call_id)
    }
}
