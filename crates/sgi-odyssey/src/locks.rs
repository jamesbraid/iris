//! The lock the mounted files use from iris's `locks`: the processors'
//! submit lock. iris's is a spin lock that reports a suspected deadlock;
//! this crate's callers bring their own discipline.

pub struct SpinLock<T>(parking_lot::Mutex<T>);

pub type SpinGuard<'a, T> = parking_lot::MutexGuard<'a, T>;

impl<T> SpinLock<T> {
    pub fn new(v: T) -> SpinLock<T> {
        SpinLock(parking_lot::Mutex::new(v))
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        self.0.lock()
    }
}
