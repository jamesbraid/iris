//! Odyssey's command worker. Register producers never wait for drawing.
use super::render::Render;
use crate::traits::{BUS_BUSY, BUS_OK};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CAPACITY: usize = 4096;
#[derive(Clone, Copy)]
pub struct Submission {
    pub counted: bool,
    pub port: u32,
    pub width: u8,
    pub value: u64,
}
struct Shared {
    queue: Mutex<VecDeque<Submission>>,
    wake: Condvar,
    running: AtomicBool,
    pending: AtomicUsize,
    render: Mutex<Render>,
    drained: Box<dyn Fn(Submission) + Send + Sync>,
    frame: OnceLock<Box<dyn Fn(&Render, usize) + Send + Sync>>,
    finish: OnceLock<Box<dyn Fn() + Send + Sync>>,
}
pub struct Engine {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Engine {
    pub fn new(drained: Box<dyn Fn(Submission) + Send + Sync>) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
                running: AtomicBool::new(false),
                pending: AtomicUsize::new(0),
                render: Mutex::new(Render::default()),
                drained,
                frame: OnceLock::new(),
                finish: OnceLock::new(),
            }),
            worker: Mutex::new(None),
        }
    }
    pub fn presentation(
        &self,
        frame: Box<dyn Fn(&Render, usize) + Send + Sync>,
        finish: Box<dyn Fn() + Send + Sync>,
    ) {
        assert!(self.shared.frame.set(frame).is_ok());
        assert!(self.shared.finish.set(finish).is_ok());
    }
    pub fn push(&self, s: Submission) -> u32 {
        let mut queue = self.shared.queue.lock();
        if queue.len() == CAPACITY {
            return BUS_BUSY;
        }
        self.shared.pending.fetch_add(1, Ordering::AcqRel);
        queue.push_back(s);
        self.shared.wake.notify_one();
        BUS_OK
    }
    pub fn busy(&self) -> bool {
        self.shared.pending.load(Ordering::Acquire) != 0
    }
    pub fn running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }
    pub fn start(&self) {
        let mut worker = self.worker.lock();
        if worker.is_some() {
            return;
        }
        let s = self.shared.clone();
        s.running.store(true, Ordering::Release);
        *worker = Some(
            std::thread::Builder::new()
                .name("Odyssey".into())
                .spawn(move || {
                    // Host presentation is paced separately from guest retrace.
                    let period = Duration::from_micros(16667);
                    let mut next = Instant::now() + period;
                    loop {
                        let submission = {
                            let mut q = s.queue.lock();
                            while q.is_empty()
                                && s.running.load(Ordering::Acquire)
                                && Instant::now() < next
                            {
                                s.wake.wait_for(
                                    &mut q,
                                    next.saturating_duration_since(Instant::now()),
                                );
                            }
                            if !s.running.load(Ordering::Acquire) {
                                break;
                            }
                            q.pop_front()
                        };
                        if let Some(submission) = submission {
                            {
                                let mut render = s.render.lock();
                                if submission.width == 8 {
                                    render.word((submission.value >> 32) as u32);
                                }
                                render.word(submission.value as u32);
                            }
                            (s.drained)(submission);
                            s.pending.fetch_sub(1, Ordering::Release);
                        }
                        if Instant::now() >= next {
                            if let Some(frame) = s.frame.get() {
                                frame(&s.render.lock(), s.pending.load(Ordering::Acquire));
                            }
                            next = Instant::now() + period;
                        }
                    }
                    if let Some(finish) = s.finish.get() {
                        finish();
                    }
                })
                .expect("start Odyssey worker"),
        );
    }
    pub fn stop(&self) {
        let mut worker = self.worker.lock();
        // Pair the stop transition with the consumer's wait predicate.
        {
            let _q = self.shared.queue.lock();
            self.shared.running.store(false, Ordering::Release);
            self.shared.wake.notify_all();
        }
        if let Some(thread) = worker.take() {
            thread.join().expect("Odyssey worker panicked");
        }
    }
    pub fn reset(&self) {
        self.stop();
        self.shared.queue.lock().clear();
        self.shared.pending.store(0, Ordering::Release);
        *self.shared.render.lock() = Render::default();
    }
    /// An idle readback. A busy producer retries rather than waiting on the
    /// render lock held by a long operation.
    pub fn with_idle<R>(&self, f: impl FnOnce(&Render) -> R) -> Option<R> {
        if self.busy() {
            return None;
        }
        let render = self.shared.render.try_lock()?;
        if self.busy() {
            return None;
        }
        Some(f(&render))
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_queue_refuses_entire_doubleword() {
        let e = Engine::new(Box::new(|_| {}));
        for _ in 0..CAPACITY {
            assert_eq!(
                e.push(Submission {
                    counted: false,
                    port: 0x110000,
                    width: 8,
                    value: 0
                }),
                BUS_OK
            );
        }
        assert_eq!(
            e.push(Submission {
                counted: false,
                port: 0x110000,
                width: 8,
                value: 0x1234
            }),
            BUS_BUSY
        );
        assert_eq!(e.shared.pending.load(Ordering::Acquire), CAPACITY);
        e.reset();
        assert!(!e.busy());
    }

    #[test]
    fn stop_joins_and_restart_retains_pending_work() {
        let done = Arc::new(AtomicUsize::new(0));
        let d = done.clone();
        let e = Engine::new(Box::new(move |_| {
            d.fetch_add(1, Ordering::Release);
        }));
        e.push(Submission {
            counted: false,
            port: 0x110000,
            width: 4,
            value: 0x00010046,
        });
        assert!(e.busy());
        assert!(!e.running());
        e.start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while e.busy() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        e.stop();
        assert!(!e.running());
        assert_eq!(done.load(Ordering::Acquire), 1);
        e.start();
        e.stop();
    }
}
