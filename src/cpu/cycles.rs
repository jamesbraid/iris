//! The cycle counter as devices on other threads see it, and the CP0 Count
//! rate. Kept apart from `MipsCore` so a device can hold a cycle pointer
//! without naming the core.

/// Virtual CP0 Count frequency. Fixed for the life of the core — override
/// with `[clock] fixed_mhz` or the CLI. Guests infer CPU speed from it
/// (IRIX's `hinv` reports 66 MHz for a 33 MHz Count); since these systems are
/// interrupt-driven, the emulator running faster or slower than the reported
/// speed has no ill effects.
pub const DEFAULT_COUNT_HZ: u64 = 33_000_000;

/// A raw pointer into `Hot::cycles`, handed out by `MipsExecutor::cycles_ptr`/
/// `MipsCpu::cycles_ptr` to devices on other threads that need to read the
/// live cycle count (status displays, `Wd33c93a`'s deferred-interrupt
/// spin-wait). Wraps the bare `*const u64` in a `Send`/`Sync` newtype
/// instead of blanket-asserting `unsafe impl Send`/`Sync` on every struct
/// that stores one — narrows the safety claim to exactly this one pointer
/// (valid for the process lifetime once obtained: it points into the
/// `MipsCore` owned by the executor's own top-level `Arc<Mutex<...>>`, which
/// outlives every device) rather than silencing the compiler's check for a
/// struct's other fields too, which would then need re-verifying by
/// inspection every time a new field is added.
///
/// `Copy`: cheap to pass/store by value everywhere a bare pointer would be,
/// no lifetime to track. Read with `.get()`, which does the
/// `ptr::read_volatile` itself — see `Hot::cycles`'s doc comment for why a
/// volatile read, not a plain one.
#[derive(Clone, Copy)]
pub struct CyclesPtr(*const u64);

unsafe impl Send for CyclesPtr {}
unsafe impl Sync for CyclesPtr {}

impl CyclesPtr {
    /// Construct from a raw pointer obtained via `MipsExecutor::cycles_ptr`/
    /// `MipsCpu::cycles_ptr` — never call this with anything else.
    pub fn new(ptr: *const u64) -> Self {
        Self(ptr)
    }

    /// A `CyclesPtr` that reads as `0` forever — for fields not yet wired up
    /// (`Rex3`/`Wd33c93a` are constructed before the CPU exists; see
    /// `set_cpu_cycles` on each).
    pub const fn dangling() -> Self {
        Self(std::ptr::null())
    }

    /// Read the current cycle count. Volatile: see `Hot::cycles`'s doc
    /// comment for why a plain read isn't enough. Returns 0 if this
    /// `CyclesPtr` hasn't been wired up yet (`dangling()`/not yet set).
    pub fn get(self) -> u64 {
        if self.0.is_null() { 0 } else { unsafe { std::ptr::read_volatile(self.0) } }
    }

    /// The counter's address (null when dangling), for readers that keep it
    /// somewhere a `CyclesPtr` cannot go, such as an `AtomicPtr`.
    pub fn raw(self) -> *const u64 {
        self.0
    }
}
