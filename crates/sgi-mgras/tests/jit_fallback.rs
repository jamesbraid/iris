//! A shader that fails to compile leaves the interpreter drawing, the same
//! words, with one log line for the failures rather than the compiler's dump.
#![cfg(feature = "gr4-jit")]

mod support;

use std::sync::atomic::{AtomicU32, Ordering};

use sgi_mgras::dev::mgras::engine::Mode;
use sgi_mgras::dev::mgras::rss_jit::{MODE_OFF, MODE_SYNC};
use support::*;

static WARNS: AtomicU32 = AtomicU32::new(0);
static DEBUGS: AtomicU32 = AtomicU32::new(0);

struct Counter;

impl log::Log for Counter {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        if r.args().to_string().starts_with("GR4 JIT") {
            match r.level() {
                log::Level::Warn => WARNS.fetch_add(1, Ordering::SeqCst),
                _ => DEBUGS.fetch_add(1, Ordering::SeqCst),
            };
        }
    }
    fn flush(&self) {}
}

#[test]
fn a_failed_compile_falls_back_to_the_interpreter() {
    std::env::set_var("GR4_JIT_INJECT_FAIL", "1");
    log::set_logger(&Counter).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-swap-tmesh-sphere").unwrap().1);
    let interpreter = play(&rep, Mode::Inline, Some(MODE_OFF), Drive::Each);

    let mut p = Player::new(config(&rep.head, Mode::Inline, Some(MODE_SYNC)), Drive::Each);
    rep.cmds.iter().for_each(|c| p.apply(c));
    let jit = p.finish();
    let (hits, misses, _) = p.e.jit_counts();

    assert_eq!(jit.hash, interpreter.hash, "the interpreter drew different words");
    assert_eq!(hits, 0, "a shader ran that should have failed to compile");
    assert!(misses > 0, "no primitive asked for a shader");
    assert_eq!(WARNS.load(Ordering::SeqCst), 1, "one warning for all the failures");
    assert!(DEBUGS.load(Ordering::SeqCst) > 0, "the others are at debug");
}
