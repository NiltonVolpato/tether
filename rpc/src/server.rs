use core::marker::PhantomData;

use crate::{CallId, Pack, Status};

/// The implementations a server provides for replies and sinks. Generated
/// `Handler` traits are generic over it, so handlers run unchanged on the
/// framework and on fakes.
pub trait ServerTypes {
    type Reply: RawReply;
    type Sink: RawSink;
}

/// What the framework dispatches to: one per `rpc_service`, implemented by
/// generated code on top of a `Handler`. `method` is the method's number in
/// the service; requests are not verified yet.
pub trait Service<S: ServerTypes> {
    /// The service's id in its server's table.
    fn id(&self) -> u8;
    fn call(&mut self, method: u8, request: &[u8], reply: S::Reply);
    fn open(&mut self, method: u8, request: &[u8], sink: S::Sink);
    /// The client dropped `call` before it finished.
    fn cancelled(&mut self, call: CallId);
}

/// Answers one unary call, at most once.
pub trait RawReply {
    fn call_id(&self) -> CallId;
    /// Does nothing if the client cancelled meanwhile.
    fn send(self, result: Result<&[u8], Status>);
}

/// The server's end of one channel.
pub trait RawSink {
    fn call_id(&self) -> CallId;
    /// Uses one credit; `NoCredit` until the client consumes earlier items.
    fn send(&self, item: &[u8]) -> Result<(), StreamError>;
    /// Sends now if there's credit, otherwise replaces the value that is
    /// waiting for credit.
    fn set_latest(&self, item: &[u8]) -> Result<(), StreamError>;
    /// Items that can be sent right now; 0 once closed.
    fn credit(&self) -> u16;
    /// Does nothing if the client cancelled meanwhile.
    fn end(self, result: Result<(), Status>);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The client hasn't consumed earlier items; try again later.
    NoCredit,
    /// The client cancelled, or the channel ended.
    Closed,
}

/// Answers one unary call with a `T`.
pub struct Reply<R: RawReply, T: Pack> {
    raw: R,
    _t: PhantomData<fn(&T)>,
}

impl<R: RawReply, T: Pack> Reply<R, T> {
    pub fn new(raw: R) -> Self {
        Self { raw, _t: PhantomData }
    }

    pub fn call_id(&self) -> CallId {
        self.raw.call_id()
    }

    /// Does nothing if the client cancelled meanwhile.
    pub fn send(self, result: Result<&T, Status>) {
        match result {
            Ok(response) => self.raw.send(Ok(&response.to_bytes())),
            Err(status) => self.raw.send(Err(status)),
        }
    }
}

/// The server's end of a channel of `T`s. Handlers keep it for as long as
/// the channel lasts.
pub struct Sink<S: RawSink, T: Pack> {
    raw: S,
    _t: PhantomData<fn(&T)>,
}

impl<S: RawSink + Clone, T: Pack> Clone for Sink<S, T> {
    fn clone(&self) -> Self {
        Self::new(self.raw.clone())
    }
}

impl<S: RawSink, T: Pack> Sink<S, T> {
    pub fn new(raw: S) -> Self {
        Self { raw, _t: PhantomData }
    }

    pub fn call_id(&self) -> CallId {
        self.raw.call_id()
    }

    pub fn send(&self, item: &T) -> Result<(), StreamError> {
        self.raw.send(&item.to_bytes())
    }

    pub fn set_latest(&self, item: &T) -> Result<(), StreamError> {
        self.raw.set_latest(&item.to_bytes())
    }

    pub fn credit(&self) -> u16 {
        self.raw.credit()
    }

    pub fn end(self, result: Result<(), Status>) {
        self.raw.end(result);
    }
}
