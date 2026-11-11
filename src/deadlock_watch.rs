//! Lock-order deadlock detection for the lib tests.
//!
//! Devices drive interrupt lines while holding their own state lock, which is
//! safe only while no parent calls back into a child under a lock. A
//! regression there shows up as a test that hangs. With
//! `--features deadlock-detect`, `parking_lot` records who waits for which
//! lock, and `start()` runs a thread that asks for cycles once a second.
//! Without the feature `start()` does nothing. On a
//! cycle it prints every thread's backtrace and aborts the test process: a panic on this thread would end only the watchdog and leave the
//! deadlocked test hanging.
//!
//! This sees `parking_lot` locks only. `std::sync::Mutex` and the
//! `locks::SpinLock` are invisible to it.
//!
//! Call `start()` from a fixture; it is cheap and runs its thread once per
//! process. It is called from several test fixtures for now and is meant to
//! move to one shared start point.

/// Start the watchdog thread, once per process. A no-op without the
/// `deadlock-detect` feature.
#[cfg(not(feature = "deadlock-detect"))]
pub(crate) fn start() {}

#[cfg(feature = "deadlock-detect")]
pub(crate) use watch::start;

#[cfg(feature = "deadlock-detect")]
mod watch {
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::sync::Once;
    use std::time::Duration;

    /// Start the watchdog thread, once per process.
    pub(crate) fn start() {
        static START: Once = Once::new();
        START.call_once(|| {
            std::thread::Builder::new()
                .name("deadlock-watch".into())
                .spawn(|| loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if let Some(report) = cycles() {
                        // Not eprintln!: this thread inherits the output
                        // capture of the test that started it, and the abort
                        // would throw the captured report away.
                        let mut err = std::io::stderr().lock();
                        let _ = err.write_all(report.as_bytes());
                        let _ = err.flush();
                        std::process::abort();
                    }
                })
                .expect("spawn deadlock watchdog");
        });
    }

    /// The report for the deadlock cycles that exist now, if any.
    fn cycles() -> Option<String> {
        let found = parking_lot::deadlock::check_deadlock();
        if found.is_empty() {
            return None;
        }
        let mut out = format!("{} lock-order deadlock(s) detected\n", found.len());
        for (i, cycle) in found.iter().enumerate() {
            let _ = writeln!(out, "deadlock #{i}");
            for t in cycle {
                // Captured unresolved, so it prints no frames until resolved.
                let mut bt = t.backtrace().clone();
                bt.resolve();
                let _ = writeln!(out, "thread id {:?}\n{:?}", t.thread_id(), bt);
            }
        }
        Some(out)
    }
}

#[cfg(all(test, feature = "deadlock-detect"))]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::{Arc, Barrier};

    /// Two threads take two mutexes in opposite order. The watchdog must
    /// report the cycle and abort the process, so this test ends in SIGABRT
    /// with both backtraces on stderr; it is the expected result, which is why
    /// the test is ignored. Run it with
    ///
    /// ```text
    /// cargo test --release --lib --features deadlock-detect deadlock_watch -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "deadlocks on purpose: passes by the watchdog aborting the process"]
    fn the_watchdog_reports_a_lock_order_cycle() {
        start();
        let a = Arc::new(Mutex::new(()));
        let b = Arc::new(Mutex::new(()));
        let both_held_one = Arc::new(Barrier::new(2));
        let spawn = |first: Arc<Mutex<()>>, second: Arc<Mutex<()>>, gate: Arc<Barrier>| {
            std::thread::spawn(move || {
                let _g = first.lock();
                gate.wait();
                let _h = second.lock();
            })
        };
        let t1 = spawn(a.clone(), b.clone(), both_held_one.clone());
        let t2 = spawn(b, a, both_held_one);
        // Never returns: the watchdog aborts within a second or two.
        t1.join().unwrap();
        t2.join().unwrap();
        panic!("the deadlock was not detected");
    }
}
