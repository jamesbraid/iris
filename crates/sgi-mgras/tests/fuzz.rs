//! Random command words and bus writes, seeded: no input a guest can make may panic the engine,
//! hang it, or take unbounded memory. Words go to the command FIFO ports and the raster window,
//! as the shell's bus would, beside register stores and reads anywhere in the aperture.

use sgi_engine::fuzz::{progress, LimitAlloc, Rng};
use sgi_engine::{BoardEngine, HostMem, MemFault, Swap, Write};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;

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

const PORTS: [u32; 3] = [host::CFIFO, host::CFIFO_GL, host::CFIFO_PRIVILEGED];

fn run(seed: u64, board: Board, mode: Mode, ops: usize) {
    let mut r = Rng::new(seed);
    let mut e = MgrasEngine::new(Config::new(board, mode), Noise(seed), None, None).unwrap();
    let mut now = 0u64;
    for i in 0..ops {
        progress(120, || format!("seed {seed:#x} {mode:?} op {i} rng {:#x}", r.0));
        match r.below(8) {
            0..=3 => {
                let port = PORTS[r.below(3) as usize];
                // A command: header (command number, conversion, byte count) then its words.
                let n = r.below(17) as u32;
                let header = if r.below(8) == 0 { r.next() as u32 } else { (r.below(0x400) as u32) << 8 | (r.below(0x80) as u32) << 23 | n * 4 };
                let _ = e.write(port, 32, header as u64);
                for _ in 0..n {
                    let v = r.edge();
                    let _ = e.write(port, 32, v as u64);
                }
            }
            4 => {
                let words: Vec<u32> = (0..1 + r.below(32)).map(|_| r.edge()).collect();
                let port = if r.below(8) == 0 { r.next() as u32 } else { PORTS[r.below(3) as usize] };
                let _ = e.push_words(port, &words);
            }
            // A raster register or any other offset of the window.
            5 | 6 => {
                let off = if r.below(2) == 0 { host::RASTER + (r.below(0x4000) as u32 & !3) } else { (r.below(0x8_0000) as u32) & !3 };
                let (bits, v) = ([8, 16, 32, 64][r.below(4) as usize], r.edge() as u64);
                let _ = e.write(off, bits, v);
            }
            _ => {
                let off = (r.below(0x8_0000) as u32) & !3;
                let _ = e.read(off, [8, 16, 32, 64][r.below(4) as usize]);
            }
        }
        if i % 32 == 0 {
            now += 1_000_000;
            let _ = e.run(1 << 16);
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
    assert_eq!(e.write(host::STATUS, 32, 0) == Write::Fault, e.fault().is_some());
}

#[test]
fn random_words_and_stores_never_panic_inline() {
    for (seed, board) in [(1u64, Board::High), (2, Board::Max), (3, Board::Solid), (4, Board::High)] {
        run(seed * 0x9e37_79b9, board, Mode::Inline, 4000);
    }
}

#[test]
fn random_words_and_stores_never_panic_threaded() {
    for (seed, board) in [(1u64, Board::High), (2, Board::Max)] {
        run(seed * 0x1234_5679, board, Mode::Threaded, 4000);
    }
}
