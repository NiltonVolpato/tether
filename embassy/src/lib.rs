//! tether's I/O tasks over `embedded-io-async` and `embassy-time`.
//!
//! The core is sans-IO: something has to feed it the bytes that arrive, write
//! out the bytes it has for the peer, and call it again at its next deadline.
//! [`run_client`] and [`run_server`] do that for one UART (or anything that
//! reads and writes bytes). Each is one future to spawn on the executor that
//! also runs the apps, since the core's shared state isn't thread-safe.
//!
//! A future's two halves, receiving and transmitting, run concurrently: a write
//! that waits on a slow line doesn't stop the reading, and a read is never
//! cancelled halfway. The transmitting half sleeps until the core's next
//! deadline, or until an app notifies it (the core's `Notify`), so an idle link
//! costs a ping a quarter of a second and nothing else.
//!
//! Both end when the link does, for good: they return its state, and a new
//! link needs a new client or server (with a new boot id).

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use tether_core::client::SharedClient;
use tether_core::link::LinkState;
use tether_core::notify::SharedNotify;
use tether_core::router::Router;
use tether_core::server::{ServerEvent, SharedServer};

/// Why a task stopped without the link ending.
#[derive(Debug)]
pub enum Error<R, W> {
    Read(R),
    Write(W),
    /// The reader reached the end of its stream: there's no peer to talk to.
    Closed,
}

/// What an I/O task drives: the client or the server, behind their shared
/// handles.
pub trait Endpoint {
    fn receive(&self, bytes: &[u8]);
    fn poll_transmit(&self, now: u64) -> Option<Vec<u8>>;
    fn next_deadline(&self) -> Option<u64>;
    fn link_state(&self) -> LinkState;
    fn io(&self) -> SharedNotify;
}

impl Endpoint for SharedClient {
    fn receive(&self, bytes: &[u8]) {
        self.borrow_mut().receive(bytes);
    }

    fn poll_transmit(&self, now: u64) -> Option<Vec<u8>> {
        self.borrow_mut().poll_transmit(now)
    }

    fn next_deadline(&self) -> Option<u64> {
        self.borrow().next_deadline()
    }

    fn link_state(&self) -> LinkState {
        self.borrow().link_state()
    }

    fn io(&self) -> SharedNotify {
        self.borrow().io()
    }
}

impl Endpoint for SharedServer {
    fn receive(&self, bytes: &[u8]) {
        self.borrow_mut().receive(bytes);
    }

    fn poll_transmit(&self, now: u64) -> Option<Vec<u8>> {
        self.borrow_mut().poll_transmit(now)
    }

    fn next_deadline(&self) -> Option<u64> {
        self.borrow().next_deadline()
    }

    fn link_state(&self) -> LinkState {
        self.borrow().link_state()
    }

    fn io(&self) -> SharedNotify {
        self.borrow().io()
    }
}

/// What runs on the server: it handles the events the server raises, and gets a
/// tick after each read, which is when credit arrives for the channels it
/// feeds. The [`Router`] is one.
pub trait ServerApp {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent);
    fn tick(&mut self, _server: &SharedServer) {}
}

impl ServerApp for Router {
    fn handle(&mut self, server: &SharedServer, event: ServerEvent) {
        Router::handle(self, server, event);
    }
}

/// Reads bytes of at most this many at a time.
const READ_SIZE: usize = 256;

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Runs the client's I/O until its link ends.
pub async fn run_client<R: Read, W: Write>(
    client: &SharedClient,
    rx: R,
    tx: W,
) -> Result<LinkState, Error<R::Error, W::Error>> {
    run(client, rx, tx, || {}).await
}

/// Runs the server's I/O until its link ends. `app` handles what the server
/// receives.
pub async fn run_server<A: ServerApp, R: Read, W: Write>(
    server: &SharedServer,
    app: &RefCell<A>,
    rx: R,
    tx: W,
) -> Result<LinkState, Error<R::Error, W::Error>> {
    run(server, rx, tx, || {
        // The app runs without the server borrowed, so its replies and sinks
        // can use it.
        loop {
            let event = server.borrow_mut().poll_event();
            let Some(event) = event else { break };
            app.borrow_mut().handle(server, event);
        }
        app.borrow_mut().tick(server);
    })
    .await
}

async fn run<E: Endpoint, R: Read, W: Write>(
    endpoint: &E,
    rx: R,
    tx: W,
    on_receive: impl FnMut(),
) -> Result<LinkState, Error<R::Error, W::Error>> {
    match select(receive(endpoint, rx, on_receive), transmit(endpoint, tx)).await {
        Either::First(result) => result,
        Either::Second(result) => result,
    }
}

/// Feeds what arrives to the endpoint, for as long as the link lasts.
async fn receive<E: Endpoint, R: Read, W>(
    endpoint: &E,
    mut rx: R,
    mut on_receive: impl FnMut(),
) -> Result<LinkState, Error<R::Error, W>> {
    let mut buf = [0; READ_SIZE];
    loop {
        let n = rx.read(&mut buf).await.map_err(Error::Read)?;
        if n == 0 {
            return Err(Error::Closed);
        }
        endpoint.receive(&buf[..n]);
        on_receive();
        // What came in may need an answer, an ack at least.
        endpoint.io().notify();
        let state = endpoint.link_state();
        if state.is_terminal() {
            return Ok(state);
        }
    }
}

/// Writes what the endpoint has to send, and sleeps until it has more.
async fn transmit<E: Endpoint, R, W: Write>(
    endpoint: &E,
    mut tx: W,
) -> Result<LinkState, Error<R, W::Error>> {
    let io = endpoint.io();
    loop {
        while let Some(wire) = endpoint.poll_transmit(now_ms()) {
            tx.write_all(&wire).await.map_err(Error::Write)?;
        }
        let state = endpoint.link_state();
        if state.is_terminal() {
            return Ok(state);
        }
        match endpoint.next_deadline() {
            Some(at) => {
                select(Timer::at(Instant::from_millis(at)), io.notified()).await;
            }
            None => io.notified().await,
        }
    }
}
