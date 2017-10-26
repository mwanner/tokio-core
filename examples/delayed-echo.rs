//! A "hello world" deferred echo server with tokio-timer
//!
//! This server will create a TCP listener, accept connections in a loop, and
//! simply write back everything that's read off of each TCP connection with a
//! random delay.
//!
//! To see this server in action, you can run this in one terminal:
//!
//!     cargo run --example echo
//!
//! and in another terminal you can run:
//!
//!     cargo run --example connect 127.0.0.1:8080
//!
//! Each line you type in to the `connect` terminal should be echo'd back to
//! you! If you open up multiple terminals running the `connect` example you
//! should be able to see them all make progress simultaneously.

extern crate futures;
extern crate tokio;
#[macro_use]
extern crate tokio_io;
extern crate tokio_timer;

use std::env;
use std::io;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use futures::{Async, Future, Poll};
use futures::stream::Stream;
use futures::thread::Spawner;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio::{global, local};
use tokio::net::TcpListener;
use tokio_timer::{Timer, Worker};

struct RequestHandler<R, W, X> {
    source: R,
    sink: Rc<RefCell<W>>,
    spawner: Spawner,
    timer: Timer<X>
}

impl<R, W, X> RequestHandler<R, W, X> {
    fn new(source: R, sink: W, timer: Timer<X>, spawner: Spawner) -> RequestHandler<R, W, X> {
        RequestHandler {
            source: source,
            sink: Rc::new(RefCell::new(sink)),
            spawner: spawner,
            timer: timer
        }
    }
}

impl<R, W, X> Future for RequestHandler<R, W, X>
    where R: AsyncRead,
          W: AsyncWrite + 'static,
          X: Worker + 'static
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut buf = vec![0; 8192];
        let n = try_nb!(self.source.read(&mut buf));
        if n == 0 {
            println!("Yup, null bytes remaining...");
            Ok(Async::Ready(()))
        } else {
            // Set a timeout that expires in 500 milliseconds
            let sleep = self.timer.sleep(Duration::from_millis(5000));
            let sink = self.sink.clone();
            let msg = sleep.then(move |_| ResponseHandler::new(sink, buf))
                .or_else(move |_| Ok(()));

            self.spawner.spawn(msg);

            // Continue with the RequestHandler
            Ok(Async::NotReady)
        }
    }
}

struct ResponseHandler<S> {
    stream: Rc<RefCell<S>>,
    data: Vec<u8>,
    pos: usize
}

impl<S> ResponseHandler<S> {
    fn new(stream: Rc<RefCell<S>>, data: Vec<u8>) -> ResponseHandler<S> {
        ResponseHandler {
            stream: stream,
            data: data,
            pos: 0
        }
    }
}

impl<S> Future for ResponseHandler<S>
    where S: AsyncWrite
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut stream = self.stream.borrow_mut();

        // If our buffer has some data, let's write it out!
        while self.pos < self.data.len() {
            let i = try_nb!(stream.write(&self.data[self.pos..]));
            self.pos += i;
        }
        try_nb!(stream.flush());

        Ok(Async::Ready(()))
    }
}

fn main() {
    // Allow passing an address to listen on as the first argument of this
    // program, but otherwise we'll just set up our TCP listener on
    // 127.0.0.1:8080 for connections.
    let addr = env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let addr = addr.parse::<SocketAddr>().unwrap();

    // Create the timer
    let timer = Timer::default();

    // TODO: update dox
    // First up we'll create the event loop that's going to drive this server.
    // This is done by creating an instance of the `Core` type, tokio-core's
    // event loop. Most functions in tokio-core return an `io::Result`, and
    // `Core::new` is no exception. For this example, though, we're mostly just
    // ignoring errors, so we unwrap the return value.
    //
    // After the event loop is created we acquire a handle to it through the
    // `handle` method. With this handle we'll then later be able to create I/O
    // objects and spawn futures.
    // let mut core = Core::new().unwrap();
    // let handle = core.handle();

    // Next up we create a TCP listener which will listen for incoming
    // connections. This TCP listener is bound to the address we determined
    // above and must be associated with an event loop, so we pass in a handle
    // to our event loop. After the socket's created we inform that we're ready
    // to go and start accepting connections.

    let global_reactor = global::start_reactor_thread();
    local::configure_remote_reactor(global_reactor.clone());

    let socket = TcpListener::bind(&addr).unwrap();
    println!("Listening on: {}", addr);

    let mut run = futures::thread::EventLoop::new();
    let spawner = run.spawner();

    // Here we convert the `TcpListener` to a stream of incoming connections
    // with the `incoming` method. We then define how to process each element in
    // the stream with the `for_each` method.
    //
    // This combinator, defined on the `Stream` trait, will allow us to define a
    // computation to happen for all items on the stream (in this case TCP
    // connections made to the server).  The return value of the `for_each`
    // method is itself a future representing processing the entire stream of
    // connections, and ends up being our server.
    let done = socket.incoming().for_each(move |(socket, addr)| {
        let (source, sink) = socket.split();
        let msg = RequestHandler::new(source, sink, timer.clone(), spawner.clone());

        let msg = msg.then(move |result| {
            match result {
                Ok(_) => println!("{}: connection closed", addr),
                Err(e) => println!("error on {}: {}", addr, e),
            }

            Ok(())
        });

        spawner.spawn(msg);

        Ok(())
    });

    // And finally now that we've define what our server is, we run it! We
    // didn't actually do much I/O up to this point and this `Core::run` method
    // is responsible for driving the entire server to completion.
    //
    // The `run` method will return the result of the future that it's running,
    // but in our case the `done` future won't ever finish because a TCP
    // listener is never done accepting clients. That basically just means that
    // we're going to be running the server until it's killed (e.g. ctrl-c).
    run.block_until(done).unwrap();
}
