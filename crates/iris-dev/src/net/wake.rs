//! A wake-up signal between the NIC thread and its network backend.
//!
//! Waiters can block on it two ways: [`Wake::wait`] (a condvar, for threads
//! that only wait for the other side) or, on Unix, by putting [`Wake::fd`] in
//! a `poll(2)` set next to their sockets (the NAT engine sleeps until either
//! the guest sends or a host socket is ready). `notify` serves both.
//!
//! A notify is never lost: it sets a flag that the next wait consumes, so a
//! signal sent while the other side was busy still ends its next wait at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

pub struct Wake {
    lock: Mutex<()>,
    cvar: Condvar,
    pending: AtomicBool,
    #[cfg(unix)]
    pipe: [libc::c_int; 2], // [read, write], both non-blocking
}

impl Wake {
    pub fn new() -> std::sync::Arc<Self> {
        #[cfg(unix)]
        let pipe = {
            let mut fds = [-1 as libc::c_int; 2];
            // SAFETY: fds is a valid 2-element array for pipe() to fill.
            let ok = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
            if ok {
                for fd in fds {
                    // SAFETY: fd was just returned by pipe().
                    unsafe {
                        let fl = libc::fcntl(fd, libc::F_GETFL);
                        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                }
                fds
            } else {
                [-1, -1]
            }
        };
        std::sync::Arc::new(Self {
            lock: Mutex::new(()),
            cvar: Condvar::new(),
            pending: AtomicBool::new(false),
            #[cfg(unix)]
            pipe,
        })
    }

    /// Wake whoever waits (now or at its next wait).
    pub fn notify(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return; // already signalled and not yet consumed
        }
        #[cfg(unix)]
        if self.pipe[1] >= 0 {
            // SAFETY: writing one byte from a valid buffer to our own pipe fd.
            // A full pipe (EAGAIN) still has a byte to wake the reader.
            unsafe { libc::write(self.pipe[1], [1u8].as_ptr().cast(), 1) };
        }
        // Taking the lock orders this against a waiter between its check of
        // `pending` and its sleep, so the notify can't slip through the gap.
        drop(self.lock.lock());
        self.cvar.notify_all();
    }

    /// Block until notified or `timeout` passes. Returns true if notified.
    pub fn wait(&self, timeout: Duration) -> bool {
        let mut g = self.lock.lock();
        if !self.pending.load(Ordering::Acquire) {
            let _ = self.cvar.wait_for(&mut g, timeout);
        }
        drop(g);
        self.consume()
    }

    /// Clear the signal (after `fd` polled readable, or to start afresh).
    /// Returns true if it was set.
    pub fn consume(&self) -> bool {
        // Drain first, then clear. The other order loses wakes: a notify
        // between the two would set the flag and write a byte the drain then
        // eats, leaving the flag set with an empty pipe, so every later notify
        // skips the write and a poll(2) sleeper never wakes. In this order a
        // notify that lands in between only merges with the signal being
        // consumed, and the caller handles its work after this returns.
        #[cfg(unix)]
        if self.pipe[0] >= 0 {
            let mut buf = [0u8; 64];
            // SAFETY: reading into a valid local buffer from our own pipe fd.
            while unsafe { libc::read(self.pipe[0], buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
        self.pending.swap(false, Ordering::AcqRel)
    }

    /// The read end of the self-pipe, readable while a notify is pending.
    #[cfg(unix)]
    pub fn fd(&self) -> Option<libc::c_int> {
        (self.pipe[0] >= 0).then_some(self.pipe[0])
    }

    /// For the lock-holder diagnostics (`locks` monitor command).
    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        #[cfg(unix)]
        for fd in self.pipe {
            if fd >= 0 {
                // SAFETY: closing fds we own exactly once.
                unsafe { libc::close(fd) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_notify_before_the_wait_is_not_lost() {
        let w = Wake::new();
        w.notify();
        let t0 = Instant::now();
        assert!(w.wait(Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_millis(100));
        assert!(!w.wait(Duration::from_millis(10)), "consumed");
    }

    #[test]
    fn notify_from_another_thread_ends_the_wait() {
        let w = Wake::new();
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w2.notify();
        });
        let t0 = Instant::now();
        assert!(w.wait(Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_secs(1));
        h.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_fd_is_readable_while_pending() {
        let w = Wake::new();
        let fd = w.fd().unwrap();
        let readable = |fd| {
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd, zero timeout.
            unsafe { libc::poll(&mut p, 1, 0) > 0 }
        };
        assert!(!readable(fd));
        w.notify();
        assert!(readable(fd));
        assert!(w.consume());
        assert!(!readable(fd));
    }
}
