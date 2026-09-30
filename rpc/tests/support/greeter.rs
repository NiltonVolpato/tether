//! What rpcgen is to generate for this schema, written by hand:
//!
//! ```fbs
//! namespace Test;
//!
//! rpc_service Greeter {
//!     SayHello(Text): Text;
//!     /// Counts down from the number in the request.
//!     Countdown(Text): Text (streaming: "server");
//! }
//!
//! enum Server : ubyte (rpc_server) { Greeter }
//! ```
//!
//! It depends on the `rpc` crate only.

use rpc::{
    Call, CallId, Channel, MethodId, MethodInfo, Pack, RawReply, RawSink, Reply, ServerTable,
    ServerTypes, ServiceInfo, Sink, Status, Table, Transport,
};

use super::text::{Text, TextT};

/// `enum Server`, the table of services one server offers.
pub static SERVER: &ServerTable = &[Some(ServiceInfo {
    name: "Test.Greeter",
    methods: &[
        Some(MethodInfo { name: "SayHello", streaming: false }),
        Some(MethodInfo { name: "Countdown", streaming: true }),
    ],
})];

/// This service's id in `SERVER`.
pub const ID: u8 = 0;
/// `Test.Greeter/SayHello`
pub const SAY_HELLO: MethodId = MethodId::new(ID, 0);
/// `Test.Greeter/Countdown`
pub const COUNTDOWN: MethodId = MethodId::new(ID, 1);

pub struct Client<X>(pub X);

impl<X: Transport> Client<X> {
    pub fn say_hello(&self, req: &TextT) -> Call<X::Call, Text> {
        Call::new(self.0.call(SAY_HELLO, &req.to_bytes(), 5000))
    }

    /// Counts down from the number in the request.
    pub fn countdown(&self, req: &TextT, capacity: u16) -> Channel<X::Channel, Text> {
        Channel::new(self.0.open(COUNTDOWN, &req.to_bytes(), capacity))
    }
}

pub trait Handler<S: ServerTypes> {
    fn say_hello(&mut self, reply: Reply<S::Reply, TextT>, req: &str);
    /// Counts down from the number in the request.
    fn countdown(&mut self, sink: Sink<S::Sink, TextT>, req: &str);
    /// The client dropped `call` before it finished.
    fn cancelled(&mut self, _call: CallId) {}
}

pub struct Service<H>(pub H);

impl<S: ServerTypes, H: Handler<S>> rpc::Service<S> for Service<H> {
    fn id(&self) -> u8 {
        ID
    }

    fn call(&mut self, method: u8, request: &[u8], reply: S::Reply) {
        match method {
            0 => match Text::verify(request) {
                Ok(req) => self.0.say_hello(Reply::new(reply), req),
                Err(_) => reply.send(Err(Status::InvalidArgument)),
            },
            _ => reply.send(Err(Status::Unimplemented)),
        }
    }

    fn open(&mut self, method: u8, request: &[u8], sink: S::Sink) {
        match method {
            1 => match Text::verify(request) {
                Ok(req) => self.0.countdown(Sink::new(sink), req),
                Err(_) => sink.end(Err(Status::InvalidArgument)),
            },
            _ => sink.end(Err(Status::Unimplemented)),
        }
    }

    fn cancelled(&mut self, call: CallId) {
        self.0.cancelled(call)
    }
}
