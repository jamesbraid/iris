//! Panic injection for the tests: arm a point and the next call that passes
//! it panics. Without the `fault-injection` feature every point is a no-op.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Point {
    New = 1,
    Free,
    Write,
    PushWords,
    Read,
    SetSource,
    AccessWaits,
    Run,
    Frame,
    Advance,
    WaitIdle,
    IrqLevels,
    DrainEvents,
    DrainDirty,
    Trapped,
    ObserveLent,
    CpuWrote,
    Observe,
    ObserveAll,
    Quiesce,
    Resume,
    Save,
    Load,
    StateVersion,
    Reset,
    Fault,
    LastError,
    /// Board entries (`call` from a board crate): a pixel peek, a debug accessor.
    Pixel,
    Flags,
    /// Inside the engine, while it executes an operation (a worker panic).
    Exec,
}

#[cfg(feature = "fault-injection")]
static ARMED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The next call that reaches `p` panics, once.
#[cfg(feature = "fault-injection")]
pub fn arm(p: Point) {
    ARMED.store(p as u32, std::sync::atomic::Ordering::SeqCst);
}

#[inline(always)]
pub fn hit(p: Point) {
    #[cfg(feature = "fault-injection")]
    if ARMED.compare_exchange(p as u32, 0, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok() {
        panic!("injected panic at {p:?}");
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = p;
}
