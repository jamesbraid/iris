//! Random command words and bus writes, seeded: no input a guest can make may panic the engine,
//! hang it, or take unbounded memory. The page takes plain words and a word whose low half is a
//! selector opens a command, so the generator mixes selectors (GL and kernel) with edge-valued
//! operands, after the context activation that opens the GL grammar.

use sgi_engine::fuzz::{progress, LimitAlloc, Rng};
use sgi_engine::{BoardEngine, HostMem, MemFault, Source, Swap, Write};
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

#[global_allocator]
static MEMORY: LimitAlloc = LimitAlloc::new(3 << 30);

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

fn word(r: &mut Rng) -> u32 {
    match r.below(10) {
        // A GL or kernel selector, with data in the top half as the short forms carry.
        0..=3 => r.below(0x200) as u32 | if r.below(4) == 0 { r.below(0x10000) as u32 } else { 0 } << 16,
        4 => 0x39,
        5 => 1,
        _ => r.edge(),
    }
}

fn run(seed: u64, threads: usize, ops: usize) {
    let mut r = Rng::new(seed);
    let config = KonaConfig::ir2(1).unwrap().with_pool(threads, 32);
    let mut e = KonaEngine::new(config, Noise(seed), Options::default(), None).unwrap();
    e.set_source(Source { id: 1, charged: true });
    // The context activation that opens the GL grammar: initialise, page, offset, length.
    for w in [CONTEXT_INITIALISE, 0x10, 0, 0x100] {
        let _ = e.write(COMMAND_PAGE, 32, w as u64);
    }
    let mut now = 0u64;
    for i in 0..ops {
        progress(120, || format!("seed {seed:#x} threads {threads} op {i} rng {:#x}", r.0));
        match r.below(8) {
            0..=4 => {
                for _ in 0..1 + r.below(16) {
                    let v = word(&mut r);
                    let _ = e.write(COMMAND_PAGE, 32, v as u64);
                }
            }
            5 => {
                let _ = e.write(COMMAND_PAGE, 64, r.next());
            }
            6 => {
                let off = (r.below(0x10_0000) as u32) & !3;
                let _ = e.write(off, [32, 64][r.below(2) as usize], r.next());
            }
            _ => {
                let off = (r.below(0x10_0000) as u32) & !3;
                let _ = e.read(off, [32, 64][r.below(2) as usize]);
            }
        }
        if i % 16 == 0 {
            now += 1_000_000;
            let _ = e.run(1 << 16);
            let _ = e.advance(now);
            e.frame(now);
            let _ = e.wait_idle(now, 1_000_000_000);
            let mut ev = Vec::new();
            e.drain_events(&mut ev, usize::MAX);
        }
    }
    let _ = e.wait_idle(now, 10_000_000_000);
    if let Some(f) = e.fault() {
        assert!(!f.contains("panicked"), "seed {seed:#x} threads {threads}: {f}");
    }
    assert_eq!(e.write(COMMAND_PAGE, 32, 0) == Write::Fault, e.fault().is_some());
}

#[test]
fn random_words_and_stores_never_panic_inline() {
    for seed in 1..=4u64 {
        run(seed * 0x9e37_79b9, 0, 500);
    }
}

#[test]
fn random_words_and_stores_never_panic_with_tile_workers() {
    for seed in 1..=2u64 {
        run(seed * 0x1234_5679, 2, 500);
    }
}
