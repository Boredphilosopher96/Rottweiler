#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
use std::{
    ffi::OsStr,
    sync::{Arc, Barrier},
};

#[test]
fn ordinary_multithreaded_callers_are_rejected_before_authority_changes() {
    let rendezvous = Arc::new(Barrier::new(2));
    let waiter = Arc::clone(&rendezvous);
    let thread = std::thread::spawn(move || {
        waiter.wait();
    });
    let error = rw_macos_bootstrap::exec_worker(OsStr::new("/usr/bin/false"), &[]);
    rendezvous.wait();
    thread.join().expect("waiting thread");
    assert!(error.to_string().contains("requires one worker thread"));
}
