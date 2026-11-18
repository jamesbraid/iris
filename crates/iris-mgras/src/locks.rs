//! The one lock name the mounted files use from iris's `locks`: a mutex
//! the raster engine's fault cell sits in. iris's own is a spin lock that
//! reports a suspected deadlock; this crate's callers bring their own
//! discipline.

pub struct SpinLock<T>(parking_lot::Mutex<T>);

impl<T> SpinLock<T> {
    pub fn new(v: T) -> SpinLock<T> {
        SpinLock(parking_lot::Mutex::new(v))
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, T> {
        self.0.lock()
    }
}
