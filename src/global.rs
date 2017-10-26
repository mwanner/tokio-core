use std::cell::{RefCell};
use std::io;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use reactor::{Reactor, Handle};

struct HelperThread {
    thread: Option<thread::JoinHandle<()>>,
    done: Arc<AtomicBool>,
    reactor: Handle,
}

statik!(static REACTOR_THREAD: Mutex<RefCell<Option<HelperThread>>>
            = Mutex::new(RefCell::new(None)));

pub fn start_reactor_thread() -> Handle {
    let ht = HelperThread::new().unwrap();
    let handle = ht.reactor.clone();
    REACTOR_THREAD.with(|t| {
        let success = {
            let guard = t.lock().unwrap();
            let mut data = guard.borrow_mut();
            if data.is_some() {
                false
            } else {
                *data = Some(ht);
                true
            }
        };
        // Panic only outside of the scope of the lock, above.
        if !success {
            panic!("can only start a single global reactor thread");
        }
    });
    handle
}

#[allow(dead_code)]
pub fn shutdown_reactor_thread() {
    REACTOR_THREAD.drop();
}

impl HelperThread {
    fn new() -> io::Result<HelperThread> {
        let reactor = Reactor::new()?;
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let reactor_handle = reactor.handle().clone();
        let thread = thread::spawn(move || run(reactor, done2));

        Ok(HelperThread {
            thread: Some(thread),
            done: done,
            reactor: reactor_handle,
        })
    }
}

impl Drop for HelperThread {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        self.reactor.wakeup();
        drop(self.thread.take().unwrap().join());
    }
}

fn run(mut reactor: Reactor, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        reactor.turn(None);
    }
}
