use std::cell::RefCell;
use std::time::Duration;
use std::thread::LocalKeyState;

use reactor::{Handle, Reactor, Turn};

enum ReactorConfig {
    NotConfigured,
    OwnReactor(Reactor),
    RemoteReactor(Handle)
}

thread_local! {
    static REACTOR: RefCell<ReactorConfig> = RefCell::new(
        ReactorConfig::NotConfigured);
}

pub fn install_local_reactor() -> Handle {
    REACTOR.with(|rc| -> Handle {
        let mut cell = rc.borrow_mut();
        match *cell {
            ReactorConfig::NotConfigured => (),
            _ => panic!("reactor already configured"),
        }

        let reactor = Reactor::new().unwrap();
        let handle = reactor.handle().clone();
        *cell = ReactorConfig::OwnReactor(reactor);
        handle
    })
}

pub fn configure_remote_reactor(handle: Handle) {
    assert!(!handle.is_default());
    REACTOR.with(|rc| {
        let mut cell = rc.borrow_mut();
        match *cell {
            ReactorConfig::NotConfigured => (),
            _ => panic!("reactor already configured"),
        }
        *cell = ReactorConfig::RemoteReactor(handle);
    });
}

pub fn reactor() -> Option<Handle> {
    match REACTOR.state() {
        LocalKeyState::Destroyed => panic!("local reactor destroyed?"),
        _ => ()
    }

    REACTOR.with(|rc| match *rc.borrow() {
        ReactorConfig::NotConfigured => None,
        ReactorConfig::OwnReactor(ref reactor)
            => Some(reactor.handle().clone()),
        ReactorConfig::RemoteReactor(ref handle)
            => Some(handle.clone())
    })
}

pub fn turn_reactor(max_wait: Option<Duration>) -> Option<Turn> {
    REACTOR.with(|rc| match *rc.borrow_mut() {
        ReactorConfig::NotConfigured
            => panic!("no reactor configured for this thread"),
        ReactorConfig::OwnReactor(ref mut reactor)
            => Some(reactor.turn(max_wait)),
        ReactorConfig::RemoteReactor(_) => None
    })
}
