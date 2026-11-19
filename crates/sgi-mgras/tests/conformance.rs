//! The shared `sgi_engine` clauses (`sgi_engine::conformance`) run on the IMPACT engine, inline and
//! with its raster thread.

use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode, Region};
use sgi_mgras::dev::mgras::hq3::host;
use sgi_mgras::dev::mgras::pixmem::{Buffer, Kind};
use sgi_mgras::display::Rect;
use sgi_engine::conformance::Subject;
use sgi_engine::{HostMem, MemFault, Swap};

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

struct Impact(Mode);

/// A raster register write that changes nothing the status shows.
const REG: u32 = host::RASTER + 4 * 0x10;

impl Subject for Impact {
    type Engine = MgrasEngine<Ram>;

    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> MgrasEngine<Ram> {
        MgrasEngine::new(Config::new(Board::High, self.0), Ram, None, notify).unwrap()
    }

    fn probe(&self) -> u32 {
        host::STATUS
    }

    fn store(&self) -> (u32, u64) {
        (REG, 0x1234)
    }

    fn port(&self) -> u32 {
        0
    }

    fn region(&self) -> Region {
        Region { buffer: Buffer::new(0x240, Kind::Wide, 0x31e), rect: Rect::new(0, 0, 3, 3) }
    }

    /// 8 and 16 bit accesses are taken as 32 bit (recorded streams hold them).
    fn missing_width(&self) -> Option<(u32, u32)> {
        None
    }

    fn holds_stores(&self) -> bool {
        true
    }

    /// The IMPACT board has no flow control of its own to the shell.
    fn credits(&self) -> bool {
        false
    }
}

mod inline {
    sgi_engine::conformance_tests!(super::Impact(sgi_mgras::dev::mgras::engine::Mode::Inline));
}

mod threaded {
    sgi_engine::conformance_tests!(super::Impact(sgi_mgras::dev::mgras::engine::Mode::Threaded));
}

/// A GPU unit's loss, reported on any thread, posts `GpuLost` with the adapter's text, faults the
/// engine and wakes the shell.
#[test]
fn a_gpu_loss_posts_gpu_lost_faults_the_engine_and_wakes_the_shell() {
    use sgi_engine::{BoardEngine, Event, GpuLoss};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    let woken = Arc::new(AtomicU32::new(0));
    let w = woken.clone();
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Ram, None, Some(Box::new(move || {
        w.fetch_add(1, Ordering::SeqCst);
    })))
    .unwrap();
    let loss = GpuLoss::new();
    loss.attach(e.gpu_lost_handler());
    let lost = loss.callback();
    std::thread::spawn(move || lost("NVIDIA GeForce RTX 4090: wgpu device lost (Unknown): gone")).join().unwrap();
    assert!(woken.load(Ordering::SeqCst) >= 1, "the shell was told without a call into the engine");
    let mut seen = Vec::new();
    e.drain_events(&mut seen, usize::MAX);
    assert!(seen.contains(&Event::GpuLost("NVIDIA GeForce RTX 4090: wgpu device lost (Unknown): gone".into())), "{seen:?}");
    assert_eq!(seen.iter().filter(|x| matches!(x, Event::DeviceFault(_))).count(), 1, "{seen:?}");
    assert!(e.fault().unwrap().contains("RTX 4090"));
}
