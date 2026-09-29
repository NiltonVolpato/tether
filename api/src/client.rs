use core::future::{Future, poll_fn};
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::{Message, MethodId, Status, Table};

/// Starts calls: the framework's link to the server, or a fake.
pub trait Transport {
    type Call: RawCall;
    type Channel: RawChannel;

    /// Starts a unary call that fails with `DeadlineExceeded` after
    /// `timeout_ms`. Dropping the returned call cancels it.
    fn call(&self, method: MethodId, request: &[u8], timeout_ms: u32) -> Self::Call;

    /// Opens a server-streaming call that buffers up to `capacity` items;
    /// the server waits while the buffer is full. Dropping the returned
    /// channel cancels the call.
    fn open(&self, method: MethodId, request: &[u8], capacity: u16) -> Self::Channel;
}

impl<X: Transport + ?Sized> Transport for &X {
    type Call = X::Call;
    type Channel = X::Channel;

    fn call(&self, method: MethodId, request: &[u8], timeout_ms: u32) -> X::Call {
        (**self).call(method, request, timeout_ms)
    }

    fn open(&self, method: MethodId, request: &[u8], capacity: u16) -> X::Channel {
        (**self).open(method, request, capacity)
    }
}

/// A unary call in progress, before its response is verified.
pub trait RawCall: Unpin {
    type Buf: AsRef<[u8]>;
    /// Ready once the server responded, the deadline passed or the link failed.
    fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<Result<Self::Buf, Status>>;
}

/// The receiving end of a channel, before its items are verified.
pub trait RawChannel: Unpin {
    type Buf: AsRef<[u8]>;
    /// The next item; `Ready(None)` once the channel ended and every item
    /// was received.
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Self::Buf>>;
    /// `None` while open or while items are waiting; `Some(Ok(()))` when the
    /// server finished the channel.
    fn end(&self) -> Option<Result<(), Status>>;
}

fn poll_now<R>(poll: impl FnOnce(&mut Context<'_>) -> Poll<R>) -> Option<R> {
    match poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(r) => Some(r),
        Poll::Pending => None,
    }
}

/// A unary call returning a `T`. Await it, or poll it with `try_result`.
pub struct Call<C: RawCall, T: Table> {
    raw: C,
    _t: PhantomData<fn() -> T>,
}

impl<C: RawCall, T: Table> Call<C, T> {
    pub fn new(raw: C) -> Self {
        Self { raw, _t: PhantomData }
    }

    /// `None` while the call is in progress.
    pub fn try_result(&mut self) -> Option<Result<Message<T, C::Buf>, Status>> {
        poll_now(|cx| Pin::new(&mut *self).poll(cx))
    }
}

impl<C: RawCall, T: Table> Future for Call<C, T> {
    /// A response that doesn't verify is `DataLoss`.
    type Output = Result<Message<T, C::Buf>, Status>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = core::task::ready!(self.get_mut().raw.poll_result(cx));
        Poll::Ready(result.and_then(|buf| Message::new(buf).map_err(|_| Status::DataLoss)))
    }
}

/// A server-streaming call delivering `T`s.
pub struct Channel<C: RawChannel, T: Table> {
    /// Dropped when an item fails to verify, which cancels the call.
    raw: Option<C>,
    end: Option<Result<(), Status>>,
    _t: PhantomData<fn() -> T>,
}

impl<C: RawChannel, T: Table> Channel<C, T> {
    pub fn new(raw: C) -> Self {
        Self { raw: Some(raw), end: None, _t: PhantomData }
    }

    /// The next item; `Ready(None)` once the channel ended (see `end`). An
    /// item that doesn't verify cancels the call and ends it with `DataLoss`.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Message<T, C::Buf>>> {
        let Some(raw) = &mut self.raw else { return Poll::Ready(None) };
        let Some(buf) = core::task::ready!(raw.poll_recv(cx)) else {
            return Poll::Ready(None);
        };
        match Message::new(buf) {
            Ok(message) => Poll::Ready(Some(message)),
            Err(_) => {
                self.raw = None;
                self.end = Some(Err(Status::DataLoss));
                Poll::Ready(None)
            }
        }
    }

    pub async fn recv(&mut self) -> Option<Message<T, C::Buf>> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// An item if one is waiting.
    pub fn try_recv(&mut self) -> Option<Message<T, C::Buf>> {
        poll_now(|cx| self.poll_recv(cx)).flatten()
    }

    /// `None` while open; `Some(Ok(()))` when the server finished the channel.
    pub fn end(&self) -> Option<Result<(), Status>> {
        self.end.or_else(|| self.raw.as_ref()?.end())
    }
}
