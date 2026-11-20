//! What the whole-board tests (`mgras_tests.rs`, mounted here as iris has it) drive in place of iris's
//! board: the engine behind a bus-shaped interface, with the engine's worker thread running. The tests
//! that need the rest of iris's board (its snapshots, its busy timing, its host memory bus, its display
//! thread) are gated off in this crate by the `engine-crate` feature.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use sgi_engine::{BoardEngine, Event, HostMem, Idle, MemFault, Swap};

use super::engine::{Board, Config, MgrasEngine, Mode};
use super::rss::Rss;
use crate::config::{GraphicsBoard, RasterUnit};
use crate::traits::BusDevice;

/// Guest memory: what a test's bus device holds, or zeros before it sets one. An aligned word read
/// takes the device's word when it has one (the tests keep page tables as words and data as bytes).
#[derive(Default, Clone)]
pub struct Mem(Arc<Mutex<Option<Arc<dyn BusDevice>>>>);

impl HostMem for Mem {
    fn ram_read(&self, addr: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let Some(dev) = self.0.lock().clone() else {
            out.fill(0);
            return Ok(());
        };
        if out.len() == 4 && addr & 3 == 0 {
            let w = dev.read32(addr).data;
            if w != 0 {
                out.copy_from_slice(&w.to_be_bytes());
                return Ok(());
            }
        }
        for (i, b) in out.iter_mut().enumerate() {
            *b = dev.read8(addr + i as u64).data;
        }
        Ok(())
    }
    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        if let Some(dev) = self.0.lock().clone() {
            for (i, b) in data.iter().enumerate() {
                dev.write8(addr + i as u64, *b);
            }
        }
        Ok(())
    }
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_read(addr, out, swap)
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_write(addr, data, swap)
    }
}

/// iris's board takes this lock to keep the engine idle; here the tests wait for idle themselves.
pub struct Submit;

impl Submit {
    pub fn lock(&self) {}
}

/// The first raster engine, as the board lends it for reading its planes.
pub struct RssCell(*mut Rss);

impl RssCell {
    pub fn get(&self) -> *mut Rss {
        self.0
    }
}

// SAFETY: the pointer is into the engine this value sits beside; the tests read it with the engine idle.
unsafe impl Send for RssCell {}
unsafe impl Sync for RssCell {}

/// An engine with the interface the board tests use.
pub struct Mgras {
    e: Mutex<MgrasEngine<Mem>>,
    mem: Mem,
    notes: Mutex<Vec<String>>,
    /// What the tests take from iris's board: its submit lock and its first raster engine.
    pub submit: Submit,
    pub rss: RssCell,
    /// Raster engines drawing.
    pub nrss: usize,
}

fn board_of(kind: GraphicsBoard) -> Board {
    match kind {
        GraphicsBoard::SolidImpact => Board::Solid,
        GraphicsBoard::HighImpact => Board::High,
        GraphicsBoard::MaxImpact => Board::Max,
    }
}

impl Mgras {
    fn new(kind: GraphicsBoard, raster: RasterUnit) -> Mgras {
        let gpu = (raster == RasterUnit::Wgpu).then(|| crate::wgpu::Gpu::new(&Default::default(), Box::new(|m| panic!("GPU failed: {m}"))).unwrap_or_else(|e| panic!("{e}")));
        let cfg = Config::new(board_of(kind), Mode::Threaded);
        let mem = Mem::default();
        let mut e = MgrasEngine::new(cfg, mem.clone(), gpu.as_ref(), None).unwrap_or_else(|e| panic!("{e}"));
        let rss = RssCell(e.first_raster_engine());
        let nrss = if kind == GraphicsBoard::MaxImpact && gpu.is_none() { 2 } else { 1 };
        Mgras { e: Mutex::new(e), mem, notes: Mutex::new(Vec::new()), submit: Submit, rss, nrss }
    }

    /// Wait for everything queued.
    pub fn wait_idle(&self) {
        let mut e = self.e.lock();
        while e.wait_idle(0, 1_000_000_000) != Idle::Yes {
            assert!(e.fault().is_none(), "the engine stopped: {:?}", e.fault());
        }
    }

    /// Host memory is `mem` from now on.
    pub fn set_phys(&self, mem: Arc<dyn BusDevice>) {
        *self.mem.0.lock() = Some(mem);
    }

    /// Whether the engine has said something containing `what`.
    pub fn noted(&self, what: &str) -> bool {
        self.wait_idle();
        let mut events = Vec::new();
        self.e.lock().drain_events(&mut events, usize::MAX);
        let mut notes = self.notes.lock();
        notes.extend(events.into_iter().filter_map(|e| if let Event::Note(t) = e { Some(t) } else { None }));
        notes.iter().any(|n| n.contains(what))
    }

    pub fn idle(&self) -> bool {
        self.e.lock().wait_idle(0, 0) == Idle::Yes
    }

    pub fn stop_engines(&self) {
        self.wait_idle();
        let _ = self.e.lock().quiesce(u64::MAX);
    }

    pub fn start_engines(&self) {
        self.e.lock().resume();
    }

    /// A hash of what the board stores, equal for equal stores.
    pub fn state_hash(&self) -> [u8; 32] {
        self.wait_idle();
        let h = self.e.lock().state_hash().expect("hash").to_le_bytes();
        std::array::from_fn(|i| h[i % 8])
    }

    /// The colour planes of the displayed buffer at display (x, y), y = 0 the top row.
    pub fn fb_pixel(&self, x: usize, y: usize) -> u32 {
        self.wait_idle();
        self.e.lock().fb_pixel(x, y)
    }
}

/// The tests' names for what iris's `record::tests` has.
pub mod record {
    pub use super::{load, load_text, Rec};

    pub mod tests {
        use std::sync::Arc;

        use super::super::Mgras;
        use crate::config::{GraphicsBoard, RasterUnit};
use crate::traits::BusDevice;
        use sgi_engine::{BoardEngine, Read, Write};

        pub fn live_board() -> Arc<Mgras> {
            let max = std::env::var("MGRAS_TEST_BOARD").is_ok_and(|v| v == "max");
            live_board_of(if max { GraphicsBoard::MaxImpact } else { GraphicsBoard::HighImpact })
        }

        pub fn live_board_of(kind: GraphicsBoard) -> Arc<Mgras> {
            live_board_raster(kind, RasterUnit::Cpu)
        }

        pub fn live_board_raster(kind: GraphicsBoard, raster: RasterUnit) -> Arc<Mgras> {
            Arc::new(Mgras::new(kind, raster))
        }

        pub fn live_board_with(kind: GraphicsBoard, raster: RasterUnit, _octane: bool) -> Arc<Mgras> {
            live_board_raster(kind, raster)
        }

        /// Text recordings are iris's board's own format; the tests that replay them are gated off here.
        pub fn replay_recs(_: &Mgras, _: &[super::Rec], _: bool) -> Result<(), String> {
            Err("text recordings are replayed by iris's board".into())
        }

        /// Poll `f` until it returns `Some`, for up to a minute.
        pub fn poll_until<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(v) = f() {
                    return Some(v);
                }
                if std::time::Instant::now() >= end {
                    return None;
                }
                std::thread::yield_now();
            }
        }

        /// Store, retrying while the engine's queue is full.
        pub fn write(m: &Mgras, bits: u8, off: u32, val: u64) {
            loop {
                match m.e.lock().write(off, bits as u32, val) {
                    Write::Done => return,
                    Write::Busy => std::thread::yield_now(),
                    Write::Fault => panic!("engine fault at write {off:#x}"),
                }
            }
        }

        /// Load, retrying while the engine answers busy.
        pub fn read(m: &Mgras, bits: u8, off: u32) -> u64 {
            loop {
                let r = m.e.lock().read(off, bits as u32);
                match r {
                    Read::Value(v) => return v,
                    Read::Busy => std::thread::yield_now(),
                    Read::Fault => panic!("engine fault at read {off:#x}"),
                }
            }
        }
    }
}

/// Text recordings are iris's board's own format; the tests that replay them are gated off here.
pub struct Rec;

pub fn load_text(_: &str) -> Result<Vec<Rec>, String> {
    Err("text recordings are replayed by iris's board".into())
}

pub fn load(_: &str) -> Result<Vec<Rec>, String> {
    load_text("")
}
