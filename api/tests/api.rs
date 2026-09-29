//! Application code written against the api, tested with fakes.

mod support;

use std::task::{Context, Poll, Waker};

use api::{CallId, Pack, RawChannel, Service, Status, Transport, lookup};
use support::app::{Polite, greeting, now, text};
use support::fakes::{FakeTransport, Loop, Loopback, Recorder, Recording, Sent};
use support::greeter;
use support::text::TextT;

// --- App code against a fake transport ---

#[test]
fn app_against_a_fake_transport() {
    let transport = FakeTransport::default();
    transport.respond(greeter::SAY_HELLO, Ok(&TextT::new("Hi, Ada")));

    assert_eq!(now(greeting(&greeter::Client(&transport), "Ada")), "Hi, Ada");
    let requests = transport.requests.borrow();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, greeter::SAY_HELLO);
    assert_eq!(text(&requests[0].1), "Ada");
}

#[test]
fn app_handles_statuses() {
    let transport = FakeTransport::default();
    let greeter = greeter::Client(&transport);
    assert_eq!(now(greeting(&greeter, "Ada")), "Error: Unimplemented");
    transport.respond_raw(greeter::SAY_HELLO, Err(Status::Unavailable));
    assert_eq!(now(greeting(&greeter, "Ada")), "Offline");
}

#[test]
fn malformed_response_is_data_loss() {
    let transport = FakeTransport::default();
    transport.respond_raw(greeter::SAY_HELLO, Ok(vec![0xFF; 3]));
    let call = greeter::Client(&transport).say_hello(&TextT::new("Ada"));
    assert_eq!(now(call).err(), Some(Status::DataLoss));
}

#[test]
fn malformed_item_ends_the_channel_with_data_loss() {
    let transport = FakeTransport::default();
    let items = vec![TextT::new("3").to_bytes(), vec![0xFF; 3], TextT::new("1").to_bytes()];
    transport.stream_raw(greeter::COUNTDOWN, items, Ok(()));
    let mut channel = greeter::Client(&transport).countdown(&TextT::new("3"), 1);

    assert_eq!(channel.try_recv().unwrap().get(), "3");
    assert_eq!(channel.end(), None);
    assert!(channel.try_recv().is_none());
    assert_eq!(channel.end(), Some(Err(Status::DataLoss)));
    assert!(channel.try_recv().is_none(), "nothing after the bad item");
}

// --- Handler code against recording replies and sinks ---

#[test]
fn handler_replies() {
    let recorder = Recorder::default();
    let mut service = greeter::Service(Polite::<Recording>::default());
    let reply = recorder.reply();
    let call = api::RawReply::call_id(&reply);

    service.call(0, &TextT::new("Ada").to_bytes(), reply);
    let sent = recorder.take();
    let [Sent::Reply(id, Ok(bytes))] = &sent[..] else { panic!("{sent:?}") };
    assert_eq!((*id, text(bytes).as_str()), (call, "Hello, Ada!"));

    service.call(0, &TextT::new("").to_bytes(), recorder.reply());
    assert!(matches!(recorder.take()[..], [Sent::Reply(_, Err(Status::InvalidArgument))]));
}

#[test]
fn generated_service_rejects_bad_calls() {
    let recorder = Recorder::default();
    let mut service = greeter::Service(Polite::<Recording>::default());
    service.call(0, &[0xFF; 3], recorder.reply());
    service.call(7, &TextT::new("Ada").to_bytes(), recorder.reply());
    // SayHello is unary, so opening it as a channel is not a method.
    service.open(0, &TextT::new("Ada").to_bytes(), recorder.sink(1));
    let statuses: Vec<_> = (recorder.take().into_iter())
        .map(|sent| match sent {
            Sent::Reply(_, Err(s)) | Sent::End(_, Err(s)) => s,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        statuses,
        [Status::InvalidArgument, Status::Unimplemented, Status::Unimplemented]
    );
}

#[test]
fn countdown_waits_for_credit() {
    let recorder = Recorder::default();
    let mut service = greeter::Service(Polite::<Recording>::default());
    let sink = recorder.sink(2);
    service.open(1, &TextT::new("3").to_bytes(), sink.clone());

    let items = |sent: Vec<Sent>| -> Vec<String> {
        (sent.iter())
            .map(|s| match s {
                Sent::Item(_, bytes) => text(bytes),
                Sent::End(_, result) => format!("end {result:?}"),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(items(recorder.take()), ["3", "2"]);
    service.0.pump();
    assert!(recorder.take().is_empty(), "no credit");
    sink.grant(1);
    service.0.pump();
    assert_eq!(items(recorder.take()), ["1", "end Ok(())"]);
}

// --- App and handler together, in-process ---

#[test]
fn app_and_handler_in_process() {
    let server = Loopback::new(greeter::Service(Polite::<Loop>::default()));
    let greeter = greeter::Client(&server);
    assert_eq!(now(greeting(&greeter, "Ada")), "Hello, Ada!");
    assert_eq!(now(greeting(&greeter, "")), "Error: InvalidArgument");

    let mut countdown = greeter.countdown(&TextT::new("3"), 1);
    let mut got = Vec::new();
    while countdown.end().is_none() {
        got.extend(countdown.try_recv().map(|m| m.get().to_string()));
        server.with_service(|s| s.0.pump());
    }
    assert_eq!(got, ["3", "2", "1"]);
    assert_eq!(countdown.end(), Some(Ok(())));
}

#[test]
fn dropping_a_channel_cancels_the_handlers_work() {
    let server = Loopback::new(greeter::Service(Polite::<Loop>::default()));
    let mut countdown = greeter::Client(&server).countdown(&TextT::new("100"), 1);
    assert_eq!(countdown.try_recv().unwrap().get(), "100");
    server.with_service(|s| assert_eq!(s.0.countdowns.len(), 1));

    drop(countdown);
    server.flush_cancellations();
    server.with_service(|s| {
        assert_eq!(s.0.cancelled, [CallId(1)]);
        assert!(s.0.countdowns.is_empty());
    });
}

// --- Data objects ---

#[test]
fn server_table_lookup() {
    let (service, method) = lookup(greeter::SERVER, greeter::COUNTDOWN).unwrap();
    assert_eq!(
        (service.name, method.name, method.streaming),
        ("Test.Greeter", "Countdown", true)
    );
    assert!(lookup(greeter::SERVER, api::MethodId::new(0, 2)).is_none());
    assert!(lookup(greeter::SERVER, api::MethodId::new(1, 0)).is_none());
}

#[test]
fn status_codes_round_trip() {
    assert_eq!(Status::from_code(0), Ok(()));
    for code in 1..=16 {
        assert_eq!(Status::from_code(code).unwrap_err().code(), code);
    }
    assert_eq!(Status::from_code(200), Err(Status::Unknown));
}

/// `RawChannel` is implementable without the typed layer, e.g. by a
/// framework that hands out pooled buffers.
#[test]
fn raw_channel_is_usable_directly() {
    let transport = FakeTransport::default();
    transport.stream_raw(greeter::COUNTDOWN, vec![vec![1, 2]], Ok(()));
    let mut raw = transport.open(greeter::COUNTDOWN, &[], 1);
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(raw.poll_recv(&mut cx), Poll::Ready(Some(vec![1, 2])));
    assert_eq!(raw.end(), Some(Ok(())));
}
