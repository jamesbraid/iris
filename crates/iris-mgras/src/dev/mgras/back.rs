//! What a front end calls: one IMPACT raster engine and the pixel memory it
//! draws into, with no host interface, microcode or display behind it.
//!
//! The front end turns drawing into what the raster engine receives, writes
//! to its registers, `exec` set on the one that runs the primitive. The
//! engine walks the primitive on the CPU, or hands it to the GPU unit, and
//! stores into the planes this value owns.

use std::mem::MaybeUninit;
use std::sync::Arc;

use super::pixmem::Buffer;
use super::plain::boxed_zeroed;
use super::rss::{Entry, Planes, Rss};
use super::wgpu::{GpuLink, GpuRaster, Observation};
use crate::locks::SpinLock;
use crate::wgpu::Gpu;

/// The raster engines of one board over their pixel planes: one, or the
/// Maximum Impact's two, which one thread steps entry by entry.
pub struct ImpactBack {
    // The engines point into the planes and the fault cell, so they go
    // first and drop first.
    pub(super) rss: Vec<Box<Rss>>,
    gpu: Option<Box<GpuRaster>>,
    fault: Box<SpinLock<Option<String>>>,
    planes: Box<Planes>,
}

impl ImpactBack {
    /// An engine drawing every scanline, on the GPU when one is given.
    pub fn new(gpu: Option<&Arc<Gpu>>) -> Result<ImpactBack, String> {
        Self::with_engines(gpu, 1)
    }

    /// `engines` raster engines, each drawing the scanlines `y` with
    /// `y % engines == its number`, as a Maximum Impact's do. The GPU unit
    /// draws every scanline, so it takes one.
    pub fn with_engines(gpu: Option<&Arc<Gpu>>, engines: usize) -> Result<ImpactBack, String> {
        if !(engines == 1 || (engines == 2 && gpu.is_none())) {
            return Err(format!("{engines} raster engines{}", if gpu.is_some() { " on the GPU unit" } else { "" }));
        }
        let mut planes = boxed_zeroed::<Planes>();
        let mut fault = Box::new(SpinLock::new(None));
        let mut rss = Vec::with_capacity(engines);
        for id in 0..engines {
            let mut e = Box::<Rss>::new_zeroed();
            // SAFETY: zeroed and exclusively owned; `init` writes the fields
            // that are not valid zeroed, as the board does for each engine.
            let mut e = unsafe {
                Rss::init(e.as_mut_ptr(), &mut *planes);
                e.assume_init()
            };
            e.set_engine(id as u32, engines as u32 - 1);
            e.step_te(engines as u32);
            rss.push(e);
        }
        let gpu = match gpu {
            Some(g) => {
                let mut unit = Box::new(GpuRaster::new(g)?);
                unit.attach(&planes.mem, &planes.cid, &mut planes.te);
                rss[0].gpu = GpuLink::new(&mut unit);
                rss[0].gpu_fault = &*fault;
                Some(unit)
            }
            None => None,
        };
        Ok(ImpactBack { rss, gpu, fault, planes })
    }

    /// How many raster engines draw.
    pub fn engines(&self) -> usize {
        self.rss.len()
    }

    /// Run one entry on every engine, in turn, and say whether any stored
    /// pixels. An engine reads the pixels of the other's rows only at a PIO
    /// advance, after the other has drawn everything before it.
    pub fn run(&mut self, e: Entry) -> bool {
        let mut stored = false;
        for rss in &mut self.rss {
            stored |= rss.run_entry(e);
        }
        stored
    }

    /// Write raster register `reg` on the engines `CONFIG.DestRE` names;
    /// `exec` runs the primitive after it.
    pub fn write(&mut self, reg: u32, value: u32, exec: bool) -> bool {
        self.run(Entry::Write { reg, val: value, exec })
    }

    /// The geometry engine's entry to and exit from GL drawing, which
    /// switches the engine's line-stipple and texture state.
    pub fn gl_enter(&mut self) {
        self.run(Entry::GlEnter);
    }

    pub fn gl_leave(&mut self) {
        self.run(Entry::GlLeave);
    }

    /// Wait for the GPU and bring the CPU's planes current. A no-op on the
    /// CPU.
    pub fn sync(&mut self) -> Result<(), String> {
        self.rss[0].gpu_flush();
        self.rss[0].gpu_sync(Observation::Validation).map(|_| ())
    }

    /// The first failure the GPU unit reported.
    pub fn fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }

    /// Records drawn on the GPU so far; None on the CPU.
    pub fn gpu_counts(&self) -> Option<(u64, u64)> {
        self.gpu.as_deref().map(|g| g.counts())
    }

    /// The pixel at (x, y) of `buffer`, as stored: 36 bits for a wide
    /// buffer. Call `sync` first.
    pub fn pixel(&self, buffer: &Buffer, x: u32, y: u32) -> u64 {
        self.planes.mem.get(buffer, x, y)
    }

    /// The register `r` as the engine holds it.
    pub fn reg(&self, r: u32) -> u32 {
        self.rss[0].reg(r)
    }
}
