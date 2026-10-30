//! The board's own thread, which runs the queued command-FIFO stores.
//!
//! The board decodes and draws behind the processor, so the model does its
//! decoding and drawing on a thread of its own rather than inside the
//! processor's store (front.rs explains the two locks). The thread sleeps
//! until a store joins an empty queue, then runs the queue in slices: each
//! slice holds the board lock for at most `SLICE` of host time, and the
//! thread leaves both locks free for `GAP` before the next, so that a
//! processor access waiting for the board gets it.
//!
//! A slice never lets go of the board lock part-way: whatever a processor
//! may touch during one lives under the front lock.

use super::*;

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::MutexGuard;

/// The board thread's name.
pub const ODYSSEY_WORK_THREAD: &str = "odyssey-work";

/// One slice of host time. The guest's scheduler clock ticks every 10 ms,
/// so a slice stays well below that.
pub const SLICE: Duration = Duration::from_millis(1);

/// The pause before each slice, with both locks free. A thread that lets go
/// of a lock and takes it straight back usually wins it again, so without
/// the pause a processor waiting for the board could wait out many slices.
pub const GAP: Duration = Duration::from_micros(100);

/// How much one slice may do: host time, and for tests a count of stores.
/// A slice checks the budget between stores, so it always runs at least one
/// and stops at the first store boundary past the deadline.
pub struct OdysseyBudget {
    deadline: Option<Instant>,
    words: Option<u32>,
}

impl OdysseyBudget {
    /// A slice on the board's thread: `SLICE` of host time, or `words`
    /// stores when not zero.
    pub fn slice(words: u32) -> Self {
        OdysseyBudget { deadline: Some(Instant::now() + SLICE), words: (words != 0).then_some(words) }
    }

    /// Exactly `words` stores, or fewer if the queue runs dry.
    pub fn words(words: u32) -> Self {
        OdysseyBudget { deadline: None, words: Some(words) }
    }

    /// Everything that waits.
    pub fn unlimited() -> Self {
        OdysseyBudget { deadline: None, words: None }
    }

    /// Count one store against the budget; true when the slice must stop.
    pub fn spent(&mut self) -> bool {
        if let Some(w) = &mut self.words {
            *w = w.saturating_sub(1);
            if *w == 0 {
                return true;
            }
        }
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

/// Start the board's thread.
pub fn odyssey_work_start(h: &OdysseyHandles) {
    let mut f = h.front.lock();
    if f.worker.is_some() {
        return;
    }
    f.worker_exit = false;
    let handles = h.clone();
    f.worker = Some(
        std::thread::Builder::new()
            .name(ODYSSEY_WORK_THREAD.into())
            .spawn(move || odyssey_work_run(&handles))
            .expect("odyssey work thread"),
    );
}

/// The machine stops: finish the waiting work, then stop the board's
/// thread, so no work is left behind a stopped machine.
pub fn odyssey_work_stop(h: &OdysseyHandles) {
    {
        let mut s = h.board.lock();
        let waiting = s.front.lock().work_queue.len() as u32;
        odyssey_work_step(&mut s, &mut OdysseyBudget::words(waiting));
    }
    let thread = {
        let mut f = h.front.lock();
        f.worker_exit = true;
        f.kick.notify_one();
        f.worker.take()
    };
    if let Some(t) = thread {
        let _ = t.join();
    }
}

fn odyssey_work_run(h: &OdysseyHandles) {
    let mut f = h.front.lock();
    let kick: Arc<parking_lot::Condvar> = f.kick.clone();
    loop {
        while f.work_queue.is_empty() && !f.worker_exit {
            f.worker_waiting = true;
            kick.wait(&mut f);
            f.worker_waiting = false;
        }
        if f.worker_exit {
            break;
        }
        MutexGuard::unlocked(&mut f, || std::thread::sleep(GAP));
        if f.worker_exit {
            break;
        }
        #[cfg(test)]
        let words = f.test_words;
        #[cfg(not(test))]
        let words = 0;
        f.slice = true;
        MutexGuard::unlocked(&mut f, || {
            let mut s = h.board.lock();
            odyssey_work_step(&mut s, &mut OdysseyBudget::slice(words));
        });
        f.slice = false;
    }
}
