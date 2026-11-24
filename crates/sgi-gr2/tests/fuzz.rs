//! Random command words and register accesses, seeded: no input may panic the engine, hang it, or take
//! unbounded memory. Words go to the HQ2 FIFO window by token index, the data ports and the RE3 window,
//! beside stores and reads anywhere in the slot.

use sgi_engine::fuzz::{progress, LimitAlloc, Rng};
use sgi_engine::shell::drain_stalls;
use sgi_engine::{BoardEngine, HostMem, Idle, MemFault, Swap, Write};
use sgi_gr2::dev::gr2::engine::{Config, Gr2Engine};
use sgi_gr2::dev::gr2::Gr2Variant;

#[global_allocator]
static MEMORY: LimitAlloc = LimitAlloc::new(3 << 30);

struct Ram;

impl HostMem for Ram {
    fn ram_read(&self, _: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        out.fill(0);
        Ok(())
    }

    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

/// Run one seed; what went wrong, if anything.
fn run(n: u64, variant: Gr2Variant, ops: usize) -> Option<String> {
    let seed = n * 0x9e37_79b9;
    let mut r = Rng::new(seed);
    let mut config = Config::new(variant.probe());
    // The RE3 span JIT compiles on first use for one seed in four (ignored in a build without it).
    config.jit = if n % 4 == 0 { 1 } else { 0 };
    let mut e = Gr2Engine::new(config, Ram, None).unwrap();
    let mut now = 0u64;
    for i in 0..ops {
        progress(120, || format!("seed {seed:#x} op {i} rng {:#x}", r.0));
        match r.below(8) {
            // A token in the FIFO window, or its data port, and a few words after it.
            0..=3 => {
                let index = if r.below(4) == 0 { 479 } else { r.below(0x400) as u32 };
                let _ = e.write(0x40000 + index * 4, 32, r.edge() as u64);
                for _ in 0..r.below(12) {
                    let _ = e.write(0x40000 + 479 * 4, 32, r.edge() as u64);
                }
            }
            4 => {
                let words: Vec<u32> = (0..1 + r.below(32)).map(|_| r.edge()).collect();
                let port = if r.below(8) == 0 { r.next() as u32 } else { 0x6a068 };
                let _ = e.push_words(port, &words);
            }
            // The RE3 window and the other registers.
            5 | 6 => {
                let off = if r.below(2) == 0 { 0x6c200 + (r.below(0x100) as u32 & !3) } else { (r.below(0x70000) as u32) & !3 };
                let (bits, v) = ([8, 16, 32, 64][r.below(4) as usize], r.edge() as u64);
                let _ = e.write(off, bits, v);
            }
            _ => {
                let off = (r.below(0x70000) as u32) & !3;
                let _ = e.read(off, [8, 16, 32, 64][r.below(4) as usize]);
                let mut out = [0u32; 8];
                let _ = e.pull_words(0x6a068, &mut out);
            }
        }
        if i % 32 == 0 {
            now += 1_000_000;
            let _ = e.advance(now);
            drain_stalls(&mut e, &mut now, 1_000_000, 50);
            let mut ev = Vec::new();
            e.drain_events(&mut ev, usize::MAX);
        }
    }
    // A reader empties the read port while the board runs down; then it must go idle by itself.
    let mut out = [0u32; 256];
    let mut end = Idle::Working;
    for _ in 0..5000 {
        while e.pull_words(0x6a068, &mut out) != 0 {}
        end = drain_stalls(&mut e, &mut now, 1_000_000, 1);
        if end != Idle::Working {
            break;
        }
    }
    if end != Idle::Yes && e.fault().is_none() {
        return Some(format!("seed {seed:#x}: the board did not go idle ({end:?}); stall {:?}, port waits {}", e.board().stall_deadline(), e.access_waits(0x6a068, 32, false)));
    }
    let mut px = Vec::new();
    let _ = e.scanout(0, &mut px);
    assert_eq!(e.write(0x6a070, 32, 0) == Write::Fault, e.fault().is_some());
    e.fault().map(|f| format!("seed {seed:#x}: {f}"))
}

/// Seeds `first` to `first + count - 1`, from `GR2_FUZZ_FIRST` and `GR2_FUZZ_SEEDS` (default 1 and 64), on
/// eight threads. A release build is far faster: `cargo test --release` with `GR2_FUZZ_SEEDS=500`.
#[test]
fn random_words_and_stores_never_panic() {
    let var = |n: &str, d: u64| std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (first, count) = (var("GR2_FUZZ_FIRST", 1), var("GR2_FUZZ_SEEDS", 64));
    let next = std::sync::atomic::AtomicU64::new(first);
    let failed = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| loop {
                let seed = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if seed >= first + count {
                    break;
                }
                let variant = if seed % 3 == 2 { Gr2Variant::Extreme } else { Gr2Variant::Xz };
                failed.lock().unwrap().extend(run(seed, variant, 4000));
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    assert!(failed.is_empty(), "{failed:#?}");
}
