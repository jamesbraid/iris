//! The shared `sgi_engine` clauses (`sgi_engine::conformance`) run on the GR2 engine.

use sgi_engine::conformance::Subject;
use sgi_engine::{HostMem, MemFault, Rect, Swap};
use sgi_gr2::dev::gr2::engine::{Config, Gr2Engine};
use sgi_gr2::dev::gr2::{hq2, Gr2Variant};

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

struct Xz;

impl Subject for Xz {
    type Engine = Gr2Engine<Ram>;

    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Gr2Engine<Ram> {
        Gr2Engine::new(Config::new(Gr2Variant::Xz.probe()), Ram, notify).unwrap()
    }

    /// The first board configuration port.
    fn probe(&self) -> u32 {
        0x6c000
    }

    /// A command word the interpreter takes as a colour: the HQ2 FIFO window is indexed by command.
    fn store(&self) -> (u32, u64) {
        (0x40000 + 4 * hq2::PUC_COLOR, 7)
    }

    /// The HQ2_GEDMA data port.
    fn port(&self) -> u32 {
        0x6a068
    }

    fn region(&self) -> Rect {
        Rect::new(0, 0, 31, 31)
    }

    /// The registers answer 8, 16, 32 and 64 bits; nothing else.
    fn missing_width(&self) -> Option<(u32, u32)> {
        Some((128, 0))
    }

    fn worker_panics(&self) -> bool {
        cfg!(feature = "fault-injection")
    }

    fn holds_stores(&self) -> bool {
        true
    }

    /// The board has no flow control to the shell.
    fn credits(&self) -> bool {
        false
    }
}

sgi_engine::conformance_tests!(Xz);
