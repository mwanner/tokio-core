//! A "tiny" example of HTTP request/response handling using just tokio-core
//!
//! This example is intended for *learning purposes* to see how various pieces
//! hook up together and how HTTP can get up and running. Note that this example
//! is written with the restriction that it *can't* use any "big" library other
//! than tokio-core, if you'd like a "real world" HTTP library you likely want a
//! crate like Hyper.
//!
//! Code here is based on the `echo-threads` example and implements two paths,
//! the `/plaintext` and `/json` routes to respond with some text and json,
//! respectively. By default this will run I/O on all the cores your system has
//! available, and it doesn't support HTTP request bodies.

extern crate bytes;
extern crate env_logger;
extern crate futures;
extern crate http;
extern crate httparse;
#[macro_use]
extern crate log;
extern crate num_cpus;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate time;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_timer;

use std::cell::RefCell;
use std::env;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;

use bytes::BytesMut;
use futures::future;
use futures::sync::mpsc;
use futures::{Stream, Future, Sink};
use futures::thread::{RunningThread, Spawner, EventLoop};
use tokio::reactor::Handle;
use http::header::HeaderValue;
use http::{Request, Response, StatusCode};
use tokio::{global, local};
use tokio::net::{TcpStream, TcpListener};
use tokio_io::codec::{Encoder, Decoder};
use tokio_io::AsyncRead;
use tokio_timer::{Timer, wheel, Worker, LocalWorker};

static TICK_DURATION_MS: u64 = 100;

fn main() {
    env_logger::init().unwrap();

    // Parse the arguments, bind the TCP socket we'll be listening to, spin up
    // our worker threads, and start shipping sockets to those worker threads.
    let addr = env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let addr = addr.parse::<SocketAddr>().unwrap();
    let num_worker_threads = env::args().nth(2).and_then(|s| s.parse().ok())
        .unwrap_or(num_cpus::get());

    let flags = (2..5).map(|i| {
        let flag_str = env::args().nth(i).unwrap_or("true".to_string());
        FromStr::from_str(&flag_str).expect("not a boolean")
    }).collect::<Vec<bool>>();

    // config
    let use_dedicated_acceptor_thread = flags[0];
    let use_dedicated_timer_thread = flags[1];
    let use_dedicated_reactor_thread = flags[2];
    let tick_duration = Duration::from_millis(TICK_DURATION_MS);

    let global_cnt: Arc<Mutex<(u64, u64)>> = Arc::new(Mutex::new((0, 0)));

    // initialization of global stuff (in the non-thread-local sense)
    let global_timer = if use_dedicated_timer_thread {
        info!("launching timer thread");
        Some(wheel().tick_duration(tick_duration).build())
    } else {
        None
    };

    // Maybe start a dedicated reactor thread for i/o.
    let global_reactor_handle = if use_dedicated_reactor_thread {
        info!("launching reactor thread");
        Some(global::start_reactor_thread())
    } else {
        None
    };

    // Main thread initialization, similar to what a worker thread needs
    // as well.
    let mut event_loop = EventLoop::new();

    // Spawn worker threads
    let mut channels = Vec::new();
    let mut threads = Vec::new();
    for i in 0..num_worker_threads {
        let rx = if use_dedicated_acceptor_thread {
            let (tx, rx) = mpsc::unbounded();
            channels.push(tx);
            Some(rx)
        } else { None };

        info!("launching worker thread {}", i + 1);
        let ctx = (
            global_reactor_handle.clone(),
            addr.clone(),
            tick_duration.clone(),
            global_cnt.clone(),
            global_timer.clone(),
            use_dedicated_reactor_thread.clone(),
            use_dedicated_acceptor_thread.clone());
        let join_handle = thread::Builder::new()
            .name("tinyhttp-worker".to_owned())
            .spawn(move || worker(rx, ctx))
            .expect("thread::spawn");
        threads.push(join_handle);
    }

    // The main thread takes the role of the acceptor thread, if any.
    // Otherwise, we just wait for all spawned children.
    if use_dedicated_acceptor_thread {
        match global_reactor_handle {
            Some(handle) => local::configure_remote_reactor(handle),
            None => { local::install_local_reactor(); }
        };
        let done = run_dedicated_listener(addr, channels);

        event_loop.spawner().spawn(done);

        let guard = RunningThread::new();
        loop {
            local::turn_reactor(Some(tick_duration.clone()));
            event_loop.iterate(&guard);
            if use_dedicated_reactor_thread {
                guard.park_thread();
            }
        }
    }

    for thread in threads {
        thread.join().unwrap();
    }
}

fn create_listener_for_worker<W>(addr: std::net::SocketAddr, spawner: Spawner,
        global_cnt: Arc<Mutex<(u64, u64)>>,
        global_timer: Option<Timer<W>>,
        local_timer: Option<Timer<Rc<RefCell<LocalWorker>>>>)
    -> Box<Future<Item=(), Error=()>>
    where W: Worker + 'static
{
    let listener = TcpListener::bind(&addr).expect("failed to bind");
    info!("listening on: {}", addr);

    Box::new(listener.incoming().for_each(move |(socket, _addr)| {
        // process on the main thread
        let spawner = spawner.clone();
        match global_timer {
            Some(ref timer) => {
                let tbox = Box::new(timer.clone());
                socket_handler(socket, tbox, spawner, global_cnt.clone())
            },
            None => {
                let tbox = Box::new(local_timer.clone().unwrap());
                socket_handler(socket, tbox, spawner, global_cnt.clone())
            }
        }.expect("connection failed");
        Ok(())
    }).map_err(|e| { panic!("i/o error: {}", e); }))
}

fn run_dedicated_listener(addr: std::net::SocketAddr,
        channels: Vec<mpsc::UnboundedSender<TcpStream>>)
    -> Box<Future<Item=(), Error=()>>
{
    let listener = TcpListener::bind(&addr).expect("failed to bind");
    info!("listening on: {}", addr);

    // Use a very simple round-robin work assigment algorithm.
    let next_cell = RefCell::new(0);
    Box::new(listener.incoming().for_each(move |(socket, _addr)| {
        let mut next = next_cell.borrow_mut();
        // send to a worker thread
        channels[*next].unbounded_send(socket).expect("worker thread died");
        *next = (*next + 1) % channels.len();
        Ok(())
    }).map_err(|e| { panic!("i/o error: {}", e); }))
}

fn worker<X>(rx: Option<mpsc::UnboundedReceiver<TcpStream>>,
             ctx: (Option<Handle>, SocketAddr, Duration, Arc<Mutex<(u64, u64)>>,
                   Option<Timer<X>>, bool, bool))
    where X: Worker + 'static
{
    let (global_handle, addr, tick_duration, global_cnt, global_timer,
         use_dedicated_reactor_thread, use_dedicated_acceptor_thread) = ctx;

    // setup the reactor
    match global_handle {
        Some(global_handle) => {
            // Install a thread-local handle to the global reactor running on
            // another thread.
            local::configure_remote_reactor(global_handle);
        },
        None => {
            // Create and install a thread-local reactor.
            local::install_local_reactor();
        }
    };

    // setup the event loop
    let mut event_loop = EventLoop::new();
    let spawner = event_loop.spawner();

    // setup the timer
    let mut local_timer = create_local_timer(tick_duration, global_timer.clone());

    let done = if use_dedicated_acceptor_thread {
        // In this case, the thread gets the socket via an mpsc channel.
        assert!(rx.is_some());
        let spawner_clone = spawner.clone();
        let global_timer_clone = global_timer.clone();
        let local_timer_clone = local_timer.clone();
        Box::new(rx.unwrap().for_each(move |socket| {
            match global_timer_clone {
                Some(ref timer) => {
                    let tbox = Box::new(timer.clone());
                    socket_handler(socket, tbox, spawner_clone.clone(), global_cnt.clone())
                },
                None => {
                    let tbox = Box::new(local_timer_clone.clone().unwrap());
                    socket_handler(socket, tbox, spawner_clone.clone(), global_cnt.clone())
                }
            }
        }))
    } else {
        // With local acceptors, we start a local listener and don't need
        // any communication between threads.
        create_listener_for_worker(addr, spawner.clone(), global_cnt,
                                   global_timer.clone(), local_timer.clone())
    };

    spawner.spawn(done);
    let timeout = match local_timer {
        Some(_) => Some(tick_duration),
        None => None
    };
    let guard = RunningThread::new();
    loop {
        local::turn_reactor(Some(tick_duration.clone()));
        if local_timer.is_some() {
            local_timer.as_mut().unwrap().iterate();
        }
        event_loop.iterate(&guard);
        if use_dedicated_reactor_thread {
            match timeout {
                None => guard.park_thread(),
                Some(dur) => guard.park_thread_timeout(dur)
            }
        }
    }
}

fn create_local_timer<X>(tick_duration: Duration,
                         global_timer: Option<Timer<X>>)
    -> Option<Timer<Rc<RefCell<LocalWorker>>>>
{
    // If the dedicated timer thread is disabled, we instantiate a timer
    // instance per worker thread.
    if global_timer.is_none() {
        Some(wheel().tick_duration(tick_duration).build_local())
    } else {
        None
    }
}

fn socket_handler<W>(socket: TcpStream, timer: Box<Timer<W>>, spawn: Spawner,
                     global_cnt: Arc<Mutex<(u64, u64)>>)
       -> Result<(), ()>
    where W: Worker + 'static
{
    // Increment request counter.
    {
        let mut counters = global_cnt.lock().unwrap();
        counters.0 += 1;
    }

    let f = timer.sleep(Duration::from_millis(100 * TICK_DURATION_MS))
        .or_else(|_| { future::ok(()) })
        .and_then(move |_| {
            {
                // Increment timeout counter.
                let mut counters = global_cnt.lock().unwrap();
                counters.1 += 1;

                if counters.1 % 1000000 == 0 {
                    println!("Timeouts in flight: {}", counters.0 - counters.1);
                }
            }
            future::ok(())
        });
    spawn.spawn(f);

    // Associate each socket we get with our local event loop, and then use
    // the codec support in the tokio-io crate to deal with discrete
    // request/response types instead of bytes. Here we'll just use our
    // framing defined below and then use the `send_all` helper to send the
    // responses back on the socket after we've processed them.
    let (tx, rx) = socket.framed(Http).split();
    let plan = rx.and_then(respond);
    let req = tx.send_all(plan);
    spawn.spawn(req.then(move |result| {
        drop(result);
        Ok(())
    }));
    Ok(())
}

/// "Server logic" is implemented in this function.
///
/// This function is a map from and HTTP request to a future of a response and
/// represents the various handling a server might do. Currently the contents
/// here are pretty uninteresting.
fn respond(req: Request<()>)
    -> Box<Future<Item = Response<String>, Error = io::Error>>
{
    let mut ret = Response::builder();
    let body = match req.uri().path() {
        "/plaintext" => {
            ret.header("Content-Type", "text/plain");
            "Hello, World!".to_string()
        }
        "/json" => {
            ret.header("Content-Type", "application/json");

            #[derive(Serialize)]
            struct Message {
                message: &'static str,
            }
            serde_json::to_string(&Message { message: "Hello, World!" })
                .unwrap()
        }
        _ => {
            ret.status(StatusCode::NOT_FOUND);
            String::new()
        }
    };
    Box::new(future::ok(ret.body(body).unwrap()))
}

struct Http;

/// Implementation of encoding an HTTP response into a `BytesMut`, basically
/// just writing out an HTTP/1.1 response.
impl Encoder for Http {
    type Item = Response<String>;
    type Error = io::Error;

    fn encode(&mut self, item: Response<String>, dst: &mut BytesMut) -> io::Result<()> {
        use std::fmt::Write;

        write!(BytesWrite(dst), "\
            HTTP/1.1 {}\r\n\
            Server: Example\r\n\
            Content-Length: {}\r\n\
            Date: {}\r\n\
        ", item.status(), item.body().len(), date::now()).unwrap();

        for (k, v) in item.headers() {
            dst.extend_from_slice(k.as_str().as_bytes());
            dst.extend_from_slice(b": ");
            dst.extend_from_slice(v.as_bytes());
            dst.extend_from_slice(b"\r\n");
        }

        dst.extend_from_slice(b"\r\n");
        dst.extend_from_slice(item.body().as_bytes());

        return Ok(());

        // Right now `write!` on `Vec<u8>` goes through io::Write and is not
        // super speedy, so inline a less-crufty implementation here which
        // doesn't go through io::Error.
        struct BytesWrite<'a>(&'a mut BytesMut);

        impl<'a> fmt::Write for BytesWrite<'a> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.0.extend_from_slice(s.as_bytes());
                Ok(())
            }

            fn write_fmt(&mut self, args: fmt::Arguments) -> fmt::Result {
                fmt::write(self, args)
            }
        }
    }
}

/// Implementation of decoding an HTTP request from the bytes we've read so far.
/// This leverages the `httparse` crate to do the actual parsing and then we use
/// that information to construct an instance of a `http::Request` object,
/// trying to avoid allocations where possible.
impl Decoder for Http {
    type Item = Request<()>;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Request<()>>> {
        // TODO: we should grow this headers array if parsing fails and asks
        //       for more headers
        let mut headers = [None; 16];
        let (method, path, version, amt) = {
            let mut parsed_headers = [httparse::EMPTY_HEADER; 16];
            let mut r = httparse::Request::new(&mut parsed_headers);
            let status = r.parse(src).map_err(|e| {
                let msg = format!("failed to parse http request: {:?}", e);
                io::Error::new(io::ErrorKind::Other, msg)
            })?;

            let amt = match status {
                httparse::Status::Complete(amt) => amt,
                httparse::Status::Partial => return Ok(None),
            };

            let toslice = |a: &[u8]| {
                let start = a.as_ptr() as usize - src.as_ptr() as usize;
                assert!(start < src.len());
                (start, start + a.len())
            };

            for (i, header) in r.headers.iter().enumerate() {
                let k = toslice(header.name.as_bytes());
                let v = toslice(header.value);
                headers[i] = Some((k, v));
            }

            (toslice(r.method.unwrap().as_bytes()),
             toslice(r.path.unwrap().as_bytes()),
             r.version.unwrap(),
             amt)
        };
        if version != 1 {
            return Err(io::Error::new(io::ErrorKind::Other, "only HTTP/1.1 accepted"))
        }
        let data = src.split_to(amt).freeze();
        let mut ret = Request::builder();
        ret.method(&data[method.0..method.1]);
        ret.uri(data.slice(path.0, path.1));
        ret.version(http::Version::HTTP_11);
        for header in headers.iter() {
            let (k, v) = match *header {
                Some((ref k, ref v)) => (k, v),
                None => break,
            };
            let value = unsafe {
                HeaderValue::from_shared_unchecked(data.slice(v.0, v.1))
            };
            ret.header(&data[k.0..k.1], value);
        }

        let req = ret.body(()).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, e)
        })?;
        Ok(Some(req))
    }
}

mod date {
    use std::cell::RefCell;
    use std::fmt::{self, Write};
    use std::str;

    use time::{self, Duration};

    pub struct Now(());

    /// Returns a struct, which when formatted, renders an appropriate `Date`
    /// header value.
    pub fn now() -> Now {
        Now(())
    }

    // Gee Alex, doesn't this seem like premature optimization. Well you see
    // there Billy, you're absolutely correct! If your server is *bottlenecked*
    // on rendering the `Date` header, well then boy do I have news for you, you
    // don't need this optimization.
    //
    // In all seriousness, though, a simple "hello world" benchmark which just
    // sends back literally "hello world" with standard headers actually is
    // bottlenecked on rendering a date into a byte buffer. Since it was at the
    // top of a profile, and this was done for some competitive benchmarks, this
    // module was written.
    //
    // Just to be clear, though, I was not intending on doing this because it
    // really does seem kinda absurd, but it was done by someone else [1], so I
    // blame them!  :)
    //
    // [1]: https://github.com/rapidoid/rapidoid/blob/f1c55c0555007e986b5d069fe1086e6d09933f7b/rapidoid-commons/src/main/java/org/rapidoid/commons/Dates.java#L48-L66

    struct LastRenderedNow {
        bytes: [u8; 128],
        amt: usize,
        next_update: time::Timespec,
    }

    thread_local!(static LAST: RefCell<LastRenderedNow> = RefCell::new(LastRenderedNow {
        bytes: [0; 128],
        amt: 0,
        next_update: time::Timespec::new(0, 0),
    }));

    impl fmt::Display for Now {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            LAST.with(|cache| {
                let mut cache = cache.borrow_mut();
                let now = time::get_time();
                if now > cache.next_update {
                    cache.update(now);
                }
                f.write_str(cache.buffer())
            })
        }
    }

    impl LastRenderedNow {
        fn buffer(&self) -> &str {
            str::from_utf8(&self.bytes[..self.amt]).unwrap()
        }

        fn update(&mut self, now: time::Timespec) {
            self.amt = 0;
            write!(LocalBuffer(self), "{}", time::at(now).rfc822()).unwrap();
            self.next_update = now + Duration::seconds(1);
            self.next_update.nsec = 0;
        }
    }

    struct LocalBuffer<'a>(&'a mut LastRenderedNow);

    impl<'a> fmt::Write for LocalBuffer<'a> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let start = self.0.amt;
            let end = start + s.len();
            self.0.bytes[start..end].copy_from_slice(s.as_bytes());
            self.0.amt += s.len();
            Ok(())
        }
    }
}
