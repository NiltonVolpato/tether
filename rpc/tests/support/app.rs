//! Application code, as a user writes it, and helpers to test it.

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rpc::{CallId, Message, ServerTypes, Sink, Status, StreamError, Transport};

use super::greeter;
use super::text::{Text, TextT};

/// App code on the S3: the greeting to show.
pub async fn greeting<X: Transport>(greeter: &greeter::Client<X>, name: &str) -> String {
    match greeter.say_hello(&TextT::new(name)).await {
        Ok(reply) => reply.get().to_string(),
        Err(Status::Unavailable) => "Offline".into(),
        Err(status) => format!("Error: {status:?}"),
    }
}

/// A handler on the co-processor: greets, and counts down as fast as each
/// client reads.
pub struct Polite<S: ServerTypes> {
    pub countdowns: Vec<(Sink<S::Sink, TextT>, u32)>,
    pub cancelled: Vec<CallId>,
}

impl<S: ServerTypes> Default for Polite<S> {
    fn default() -> Self {
        Self { countdowns: Vec::new(), cancelled: Vec::new() }
    }
}

impl<S: ServerTypes> Polite<S> {
    /// Sends what credit allows; runs whenever the handler gets a chance.
    pub fn pump(&mut self) {
        let mut waiting = Vec::new();
        for (sink, mut next) in self.countdowns.drain(..) {
            loop {
                if next == 0 {
                    sink.end(Ok(()));
                    break;
                }
                match sink.send(&TextT::new(next.to_string())) {
                    Ok(()) => next -= 1,
                    Err(StreamError::NoCredit) => {
                        waiting.push((sink, next));
                        break;
                    }
                    Err(StreamError::Closed) => break,
                }
            }
        }
        self.countdowns = waiting;
    }
}

impl<S: ServerTypes> greeter::Handler<S> for Polite<S> {
    fn say_hello(&mut self, reply: rpc::Reply<S::Reply, TextT>, name: &str) {
        if name.is_empty() {
            reply.send(Err(Status::InvalidArgument));
        } else {
            reply.send(Ok(&TextT(format!("Hello, {name}!"))));
        }
    }

    fn countdown(&mut self, sink: Sink<S::Sink, TextT>, from: &str) {
        match from.parse() {
            Ok(from) => {
                self.countdowns.push((sink, from));
                self.pump();
            }
            Err(_) => sink.end(Err(Status::InvalidArgument)),
        }
    }

    fn cancelled(&mut self, call: CallId) {
        self.cancelled.push(call);
        self.countdowns.retain(|(sink, _)| sink.call_id() != call);
    }
}

/// Fakes answer at once, so one poll is enough.
pub fn now<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("still pending"),
    }
}

pub fn text(bytes: &[u8]) -> String {
    Message::<Text, _>::new(bytes).unwrap().get().to_string()
}
