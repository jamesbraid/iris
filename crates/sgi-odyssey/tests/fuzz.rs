//! Random command words and bus writes, seeded: no input a guest can make may panic the engine,
//! hang it, or take unbounded memory. Words are drawn from the shapes the board decodes (register
//! selects, block headers, the copy and image templates, DMA descriptors over random tables) as
//! well as plain noise, so the decoders past the first word are reached.

use sgi_engine::fuzz::{progress, LimitAlloc, Rng};
use sgi_engine::{BoardEngine, HostMem, MemFault, Swap, Write};
use sgi_odyssey::dev::odyssey::engine::{Config, Mode, VproEngine};

#[global_allocator]
static MEMORY: LimitAlloc = LimitAlloc::new(3 << 30);

/// Guest memory whose every byte is a function of its address and a seed, so tables and pixel
/// sources the board fetches are random but repeatable; writes are accepted and forgotten.
struct Noise(u64);

impl HostMem for Noise {
    fn ram_read(&self, addr: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        if addr >> 40 != 0 {
            return Err(MemFault);
        }
        for (i, b) in out.iter_mut().enumerate() {
            let x = ((addr + i as u64) ^ self.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            *b = (x >> 29) as u8;
        }
        Ok(())
    }

    fn ram_write(&self, addr: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        if addr >> 40 != 0 { Err(MemFault) } else { Ok(()) }
    }
}

const CMD_FIFO: u32 = 0x110000;
const USER_FIFO: u32 = 0x120000;

fn word(r: &mut Rng) -> u32 {
    match r.below(10) {
        // A register select: the register and its value follow.
        0 | 1 => 0x0001_0000 | (r.below(0x1000) as u32),
        // A block header with an operand count.
        2 | 3 => (r.below(0x4000) as u32) | (r.below(17) as u32) << 10 | (r.below(4) as u32) << 28,
        4 => 0x2031,
        5 => [0xcf80_a92f, 0x0000_4570, 0x0000_0071, 0x9000_8116, 0x2900_8000][r.below(5) as usize],
        6 => r.below(64) as u32,
        _ => r.edge(),
    }
}

fn run(seed: u64, mode: Mode, ops: usize) {
    let mut r = Rng::new(seed);
    let config = Config { product: "v10".into(), widget: 11, mode, pool: (0, 0), params: Default::default(), gpu: None };
    let mut e = VproEngine::new(config, None, Noise(seed), None).unwrap();
    let mut now = 0u64;
    for i in 0..ops {
        progress(120, || format!("seed {seed:#x} {mode:?} op {i} rng {:#x}", r.0));
        let w = word(&mut r);
        match r.below(8) {
            // A burst of words through a command port.
            0..=3 => {
                let port = if r.below(2) == 0 { CMD_FIFO } else { USER_FIFO };
                for _ in 0..1 + r.below(24) {
                    let v = word(&mut r);
                    let _ = e.write(port, 32, v as u64);
                }
            }
            // 8-byte pushes: the two-word DMA descriptors.
            4 => {
                let _ = e.write(CMD_FIFO, 64, r.next());
            }
            // A register store anywhere in the aperture, at any width.
            5 | 6 => {
                // Half the stores go to the board's register file, where the registers are dense.
                let off = if r.below(2) == 0 { r.below(0x1200) as u32 } else { r.below(0x24_0000) as u32 } & !3;
                let bits = [8, 16, 32, 64][r.below(4) as usize];
                let v = if r.below(2) == 0 { w as u64 } else { r.next() };
                let _ = e.write(off, bits, v);
            }
            _ => {
                let off = if r.below(2) == 0 { r.below(0x1200) as u32 } else { r.below(0x24_0000) as u32 } & !3;
                let _ = e.read(off, [8, 16, 32, 64][r.below(4) as usize]);
            }
        }
        if i % 64 == 0 {
            now += 1_000_000;
            let _ = e.advance(now);
            let _ = e.wait_idle(now, 1_000_000_000);
            let mut ev = Vec::new();
            e.drain_events(&mut ev, usize::MAX);
        }
    }
    let _ = e.wait_idle(now, 10_000_000_000);
    if let Some(f) = e.fault() {
        assert!(!f.contains("panicked"), "seed {seed:#x} {mode:?}: {f}");
    }
    assert_eq!(e.write(0x4, 32, 0) == Write::Fault, e.fault().is_some());
}

#[test]
fn random_words_and_stores_never_panic_inline() {
    for seed in 1..=3u64 {
        run(seed * 0x9e37_79b9, Mode::Inline, 300);
    }
}

#[test]
fn random_words_and_stores_never_panic_threaded() {
    for seed in 1..=2u64 {
        run(seed * 0x1234_5679, Mode::Threaded, 300);
    }
}
