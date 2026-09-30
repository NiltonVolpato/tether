//! A proxy written against the generic layer only: it serves whatever service
//! id it's given by forwarding bytes to another server, without knowing any
//! message type.

mod support;

use std::task::{Context, Poll, Waker};

use support::app::{Polite, greeting, now};
use support::fakes::{Loop, Loopback};
use support::greeter;
use support::text::TextT;
use tether::{
    CallId, MethodId, RawCall, RawChannel, RawReply, RawSink, ServerTypes, Service, Status,
    Transport,
};

/// Serves service `id` by forwarding its calls to `upstream`.
struct Proxy<S: ServerTypes, X: Transport> {
    id: u8,
    upstream: X,
    calls: Vec<(X::Call, S::Reply)>,
    channels: Vec<(X::Channel, S::Sink)>,
}

impl<S: ServerTypes, X: Transport> Proxy<S, X> {
    fn new(id: u8, upstream: X) -> Self {
        Self { id, upstream, calls: Vec::new(), channels: Vec::new() }
    }

    /// Moves whatever is ready from upstream to downstream. Channel items
    /// move only while the downstream client has room, so each hop keeps its
    /// own credit and the proxy buffers nothing.
    fn pump(&mut self) {
        let cx = &mut Context::from_waker(Waker::noop());
        let mut pending = Vec::new();
        for (mut call, reply) in self.calls.drain(..) {
            match call.poll_result(cx) {
                Poll::Ready(Ok(response)) => reply.send(Ok(response.as_ref())),
                Poll::Ready(Err(status)) => reply.send(Err(status)),
                Poll::Pending => pending.push((call, reply)),
            }
        }
        self.calls = pending;

        let mut open = Vec::new();
        'channels: for (mut channel, sink) in self.channels.drain(..) {
            while sink.credit() > 0 {
                match channel.poll_recv(cx) {
                    Poll::Ready(Some(item)) => {
                        // There's credit, so this fails only if the client
                        // cancelled; `cancelled` is on its way.
                        let _ = sink.send(item.as_ref());
                    }
                    Poll::Ready(None) => {
                        sink.end(channel.end().unwrap_or(Err(Status::Unavailable)));
                        continue 'channels;
                    }
                    Poll::Pending => break,
                }
            }
            open.push((channel, sink));
        }
        self.channels = open;
    }
}

impl<S: ServerTypes, X: Transport> Service<S> for Proxy<S, X> {
    fn id(&self) -> u8 {
        self.id
    }

    fn call(&mut self, method: u8, request: &[u8], reply: S::Reply) {
        let call = self.upstream.call(MethodId::new(self.id, method), request, 5000);
        self.calls.push((call, reply));
        self.pump();
    }

    fn open(&mut self, method: u8, request: &[u8], sink: S::Sink) {
        let capacity = sink.credit().max(1);
        let channel = self.upstream.open(MethodId::new(self.id, method), request, capacity);
        self.channels.push((channel, sink));
        self.pump();
    }

    /// Dropping the upstream call or channel cancels it upstream too.
    fn cancelled(&mut self, call: CallId) {
        self.calls.retain(|(_, reply)| reply.call_id() != call);
        self.channels.retain(|(_, sink)| sink.call_id() != call);
    }
}

type Upstream = Loopback<greeter::Service<Polite<Loop>>>;

/// client -> proxy -> handler, each hop in-process.
fn proxied() -> Loopback<Proxy<Loop, Upstream>> {
    let upstream = Loopback::new(greeter::Service(Polite::<Loop>::default()));
    Loopback::new(Proxy::new(greeter::ID, upstream))
}

/// Lets the handler and then the proxy do pending work.
fn pump(server: &Loopback<Proxy<Loop, Upstream>>) {
    server.with_service(|proxy| {
        proxy.upstream.flush_cancellations();
        proxy.upstream.with_service(|s| s.0.pump());
        proxy.pump();
    });
}

fn handler<R>(server: &Loopback<Proxy<Loop, Upstream>>, f: impl FnOnce(&Polite<Loop>) -> R) -> R {
    server.with_service(|proxy| proxy.upstream.with_service(|s| f(&s.0)))
}

#[test]
fn unary_calls_and_statuses_pass_through() {
    let server = proxied();
    let greeter = greeter::Client(&server);
    assert_eq!(now(greeting(&greeter, "Ada")), "Hello, Ada!");
    assert_eq!(now(greeting(&greeter, "")), "Error: InvalidArgument");
}

#[test]
fn channels_pass_through_with_credit_on_each_hop() {
    let server = proxied();
    let mut countdown = greeter::Client(&server).countdown(&TextT::new("100"), 1);
    for _ in 0..10 {
        pump(&server);
    }
    // The client read nothing: one item waits at the client, one upstream
    // at the proxy's channel, and the handler holds the rest back.
    handler(&server, |h| assert_eq!(h.countdowns[0].1, 98));

    let mut got = Vec::new();
    while countdown.end().is_none() {
        got.extend(countdown.try_recv().map(|m| m.get().parse::<u32>().unwrap()));
        pump(&server);
    }
    assert_eq!(got, (1..=100).rev().collect::<Vec<_>>());
    assert_eq!(countdown.end(), Some(Ok(())));
}

#[test]
fn dropping_a_channel_cancels_through_the_proxy() {
    let server = proxied();
    let mut countdown = greeter::Client(&server).countdown(&TextT::new("100"), 1);
    assert_eq!(countdown.try_recv().unwrap().get(), "100");

    drop(countdown);
    server.flush_cancellations();
    pump(&server);
    handler(&server, |h| {
        assert_eq!(h.cancelled.len(), 1);
        assert!(h.countdowns.is_empty());
    });
}

#[test]
fn methods_the_upstream_lacks_are_unimplemented() {
    let server = proxied();
    let mut call = tether::Call::<_, support::text::Text>::new(server.call(
        MethodId::new(greeter::ID, 9),
        &[],
        5000,
    ));
    assert_eq!(call.try_result().unwrap().err(), Some(Status::Unimplemented));
}
