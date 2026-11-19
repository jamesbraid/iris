//! What the random-input tests of the board engines share: a seeded generator, a watchdog for a
//! case that stops advancing, and a global allocator that counts the bytes the test binary holds. It counts the bytes the test binary holds and
//! aborts, naming the size, when they pass a limit. A guest-sized allocation the engine does not
//! bound then fails the test at once, instead of taking the host's memory (a KONA read of
//! 0xffffffff by 0xffffffff took 128 GB of a shared host before it was bounded).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

pub struct LimitAlloc {
    limit: usize,
    live: AtomicUsize,
}

impl LimitAlloc {
    pub const fn new(limit: usize) -> LimitAlloc {
        LimitAlloc { limit, live: AtomicUsize::new(0) }
    }

    fn take(&self, n: usize) {
        let now = self.live.fetch_add(n, Ordering::Relaxed) + n;
        if now > self.limit {
            eprintln!("LimitAlloc: {n} bytes asked with {now} held, past the limit of {}", self.limit);
            std::process::abort();
        }
    }
}

// SAFETY: every call is the System allocator's; the counter only observes.
unsafe impl GlobalAlloc for LimitAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.take(l.size());
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        self.take(l.size());
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        self.live.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new > l.size() {
            self.take(new - l.size());
        } else {
            self.live.fetch_sub(l.size() - new, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
}

/// A xorshift generator: the same seed gives the same case on every host.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A word that tends to be a boundary: zero, small, a sign or size edge, or noise.
    pub fn edge(&mut self) -> u32 {
        match self.below(8) {
            0 => 0,
            1 => self.below(64) as u32,
            2 => u32::MAX - self.below(4) as u32,
            3 => 1 << self.below(32),
            4 => (1u32 << self.below(32)).wrapping_sub(1),
            5 => self.below(0x10000) as u32,
            _ => self.next() as u32,
        }
    }
}

static PROGRESS: AtomicU64 = AtomicU64::new(0);
static WHERE: Mutex<String> = Mutex::new(String::new());
static WATCHING: std::sync::Once = std::sync::Once::new();

/// Note that a case advanced, and where it is. A case that stops advancing for `secs` seconds is
/// a hang a guest could cause: the watchdog prints where and ends the process, as a panic would
/// fail the test.
pub fn progress(secs: u64, at: impl FnOnce() -> String) {
    WATCHING.call_once(|| {
        std::thread::spawn(move || {
            let (mut last, mut stuck) = (u64::MAX, 0);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let now = PROGRESS.load(Ordering::Relaxed);
                stuck = if now == last { stuck + 1 } else { 0 };
                last = now;
                if stuck >= secs {
                    eprintln!("HANG: no progress for {secs} s at {}", WHERE.lock().unwrap());
                    std::process::exit(2);
                }
            }
        });
    });
    if PROGRESS.fetch_add(1, Ordering::Relaxed) % 16 == 0 {
        *WHERE.lock().unwrap() = at();
    }
}
