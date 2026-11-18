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
use super::rss::{Planes, Rss};
use super::wgpu::{GpuLink, GpuRaster, Observation};
use crate::locks::SpinLock;
use crate::wgpu::Gpu;

/// One raster engine over its pixel planes.
pub struct ImpactBack {
    // The engine points into the planes and the fault cell, so it goes
    // first and drops first.
    rss: Box<Rss>,
    gpu: Option<Box<GpuRaster>>,
    fault: Box<SpinLock<Option<String>>>,
    planes: Box<Planes>,
}

impl ImpactBack {
    /// An engine drawing every scanline, on the GPU when one is given.
    pub fn new(gpu: Option<&Arc<Gpu>>) -> Result<ImpactBack, String> {
        let mut planes = boxed_zeroed::<Planes>();
        let mut fault = Box::new(SpinLock::new(None));
        let mut rss = Box::<Rss>::new_zeroed();
        // SAFETY: zeroed and exclusively owned; `init` writes the fields
        // that are not valid zeroed, as the board does for each engine.
        let mut rss = unsafe {
            Rss::init(rss.as_mut_ptr(), &mut *planes);
            rss.assume_init()
        };
        rss.set_engine(0, 0);
        let gpu = match gpu {
            Some(g) => {
                let mut unit = Box::new(GpuRaster::new(g)?);
                unit.attach(&planes.mem, &planes.cid, &mut planes.te);
                rss.gpu = GpuLink::new(&mut unit);
                rss.gpu_fault = &*fault;
                Some(unit)
            }
            None => None,
        };
        Ok(ImpactBack { rss, gpu, fault, planes })
    }

    /// Write raster register `reg`; `exec` runs the primitive after it.
    pub fn write(&mut self, reg: u32, value: u32, exec: bool) -> bool {
        self.rss.write(reg, value, exec)
    }

    /// The geometry engine's entry to and exit from GL drawing, which
    /// switches the engine's line-stipple and texture state.
    pub fn gl_enter(&mut self) {
        self.rss.gl_enter();
    }

    pub fn gl_leave(&mut self) {
        self.rss.gl_leave();
    }

    /// Wait for the GPU and bring the CPU's planes current. A no-op on the
    /// CPU.
    pub fn sync(&mut self) -> Result<(), String> {
        self.rss.gpu_flush();
        self.rss.gpu_sync(Observation::Validation).map(|_| ())
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
        self.rss.reg(r)
    }
}
