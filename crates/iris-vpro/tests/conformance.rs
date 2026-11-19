//! The shared `sgi_engine` clauses (`sgi_engine::conformance`) run on the VPro engine, inline and
//! with its own thread.

use iris_vpro::dev::odyssey::engine::{Config, Mode, VproEngine};
use sgi_engine::conformance::Subject;
use sgi_engine::{BoardEngine, HostMem, MemFault, Rect, Swap, Write};

const CMD_FIFO: u32 = 0x110000;
const INTR_CLEAR: u32 = 0x101c;
const INTR_ENABLE_SET: u32 = 0x1034;
const ROUTE_DEST_UPPER: u32 = 0x1044;
const ROUTE_DEST_LOWER: u32 = 0x104c;
const ROUTE_VECTORS: u32 = 0x1054;
const LOW_WATER: u64 = 0x200;
const HEART: u64 = 8;

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

struct Vpro(Mode);

fn put(e: &mut VproEngine, off: u32, val: u64) {
    assert_eq!(e.write(off, 32, val), Write::Done, "{off:#x}");
}

impl Subject for Vpro {
    type Engine = VproEngine;

    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> VproEngine {
        let config = Config { product: "v12".into(), widget: 11, mode: self.0, pool: (0, 0), gpu: None };
        VproEngine::new(config, None, Ram, notify).unwrap()
    }

    fn probe(&self) -> u32 {
        0x4
    }

    fn store(&self) -> (u32, u64) {
        (CMD_FIFO, 0)
    }

    fn port(&self) -> u32 {
        CMD_FIFO
    }

    fn region(&self) -> Rect {
        Rect::new(0, 0, 3, 3)
    }

    fn missing_width(&self) -> Option<(u32, u32)> {
        Some((24, 0x4))
    }

    fn holds_stores(&self) -> bool {
        self.0 == Mode::Threaded
    }

    fn credits(&self) -> bool {
        true
    }

    /// The empty FIFO leaves low water pending, so enabling it raises the flag line and clearing
    /// it drops the line: two states of one line, over and over.
    fn packet_storm(&self, e: &mut VproEngine) -> Option<usize> {
        put(e, ROUTE_DEST_UPPER, HEART << 25 | 7 << 16);
        put(e, ROUTE_DEST_LOWER, 0x80);
        put(e, ROUTE_VECTORS, 21 << 24 | 20 << 16 | 19 << 8 | 18);
        for _ in 0..1000 {
            put(e, INTR_ENABLE_SET, LOW_WATER);
            put(e, INTR_CLEAR, LOW_WATER);
        }
        Some(1)
    }
}

mod inline {
    sgi_engine::conformance_tests!(super::Vpro(iris_vpro::dev::odyssey::engine::Mode::Inline));
}

mod threaded {
    sgi_engine::conformance_tests!(super::Vpro(iris_vpro::dev::odyssey::engine::Mode::Threaded));
}
