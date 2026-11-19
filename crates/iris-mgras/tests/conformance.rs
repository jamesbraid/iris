//! The shared `sgi_engine` clauses (`sgi_engine::conformance`) run on the IMPACT engine, inline and
//! with its raster thread.

use iris_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode, Region};
use iris_mgras::dev::mgras::hq3::host;
use iris_mgras::dev::mgras::pixmem::{Buffer, Kind};
use iris_mgras::display::Rect;
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
    sgi_engine::conformance_tests!(super::Impact(iris_mgras::dev::mgras::engine::Mode::Inline));
}

mod threaded {
    sgi_engine::conformance_tests!(super::Impact(iris_mgras::dev::mgras::engine::Mode::Threaded));
}
