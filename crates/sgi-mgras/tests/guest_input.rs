//! Inputs a guest or a shell can send that once panicked or never came back: each found by the
//! random-input test (fuzz.rs) and kept as a case of its own.

use sgi_engine::fuzz::progress;
use sgi_engine::{BoardEngine, HostMem, MemFault, Swap};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;

struct Noise(u64);

impl HostMem for Noise {
    fn ram_read(&self, _: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        out.fill(self.0 as u8);
        Ok(())
    }

    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

/// `push_words` names its port by index (0 the user's, 1 the privileged one) or by the window
/// offset `write` takes; a port that is neither is no FIFO and takes nothing. The offset's low
/// byte used to index the parser's two ports, which panicked for a shell that passed 0x70080.
#[test]
fn push_words_takes_a_port_by_index_or_offset_and_refuses_any_other() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Noise(1), None, None).unwrap();
    for port in [0, 1, host::CFIFO, host::CFIFO_GL, host::CFIFO_PRIVILEGED, 0x70084, 0x50084] {
        assert_eq!(e.push_words(port, &[0, 0]), 2, "port {port:#x}");
    }
    for port in [2, 0x80, 0x7_0000, 0x7_0100, u32::MAX] {
        assert_eq!(e.push_words(port, &[0, 0]), 0, "port {port:#x}");
    }
}

/// A triangle's reach is the guest's: eight 16-bit-biased coordinates and slopes name rows and
/// columns four billion wide, and only the framebuffer's 2048 by 2048 pixels can be stored. The
/// rasteriser walked every row and column the registers named (one found by random input never
/// came back), so it now walks only the framebuffer's.
#[test]
fn a_triangle_that_reaches_far_off_the_framebuffer_walks_only_the_framebuffer() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Noise(1), None, None).unwrap();
    let f = |v: f64| ((v + 49151.5) as f32).to_bits() as u64;
    let reg = |n: u32| host::RASTER + 4 * n;
    for (n, v) in [(0, f(-1.0e9)), (1, f(1.0e9)), (2, f(1.0e9)), (3, f(2.0e9)), (4, f(0.0)), (5, f(-2.0e9))] {
        e.write(reg(n), 32, v);
    }
    // IR = area, left to right, executed by the store (bit 12 of the offset).
    e.write(host::RASTER + 0x1000 + 4 * 0x13, 32, 0);
    progress(60, || "a triangle far off the framebuffer".into());
    let t = std::time::Instant::now();
    while e.run(1 << 16) != 0 {}
    assert!(t.elapsed().as_secs() < 30, "the triangle took {:?}", t.elapsed());
    assert!(e.fault().is_none(), "{:?}", e.fault());
}
