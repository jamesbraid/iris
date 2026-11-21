//! What a front end calls: one IMPACT raster engine and the pixel memory it
//! draws into, with no host interface, microcode or display behind it.
//!
//! The front end turns drawing into what the raster engine receives, writes
//! to its registers, `exec` set on the one that runs the primitive. The
//! engine walks the primitive on the CPU, or hands it to the GPU unit, and
//! stores into the planes this value owns.

use std::mem::MaybeUninit;
use std::sync::Arc;

use super::dcb::Dcb;
use super::pixmem::Buffer;
use super::plain::boxed_zeroed;
use super::rss::{Entry, Planes, Rss};
#[cfg(feature = "scanout")]
use super::wgpu::scanout::MgrasScanout;
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
    /// The display kernel over the unit's pixel pages.
    #[cfg(feature = "scanout")]
    scanout: Option<Box<MgrasScanout>>,
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
        #[cfg(feature = "scanout")]
        let mut scanout = None;
        let gpu = match gpu {
            Some(g) => {
                let mut unit = Box::new(GpuRaster::new(g)?);
                #[cfg(feature = "scanout")]
                {
                    scanout = Some(Box::new(MgrasScanout::new(g, &unit.pix_buffer())?));
                }
                unit.attach(&planes.mem, &planes.cid, &mut planes.te);
                rss[0].gpu = GpuLink::new(&mut unit);
                rss[0].gpu_fault = &*fault;
                Some(unit)
            }
            None => None,
        };
        Ok(ImpactBack {
            rss,
            gpu,
            #[cfg(feature = "scanout")]
            scanout,
            fault,
            planes,
        })
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

    /// Bring a box of pixels of `bufs` current from the GPU; batches that
    /// do not touch it are not waited for. A no-op on the CPU.
    pub fn observe(&mut self, bufs: &[Buffer], b: (i32, i32, i32, i32)) -> Result<(), String> {
        self.rss[0].gpu_flush();
        self.rss[0].gpu_sync_box(bufs, b, Observation::HostComposite).map(|_| ())
    }

    pub fn planes(&self) -> &Planes {
        &self.planes
    }

    pub fn planes_mut(&mut self) -> &mut Planes {
        &mut self.planes
    }

    /// Point the GPU unit at the planes again after they were rewritten.
    pub fn reattach_gpu(&mut self) {
        if let Some(unit) = self.gpu.as_deref_mut() {
            unit.attach(&self.planes.mem, &self.planes.cid, &mut self.planes.te);
            self.rss[0].gpu = GpuLink::new(unit);
            self.rss[0].gpu_fault = &*self.fault;
        }
    }

    /// Whether the frame can be composed on the GPU.
    pub fn has_gpu_scanout(&self) -> bool {
        #[cfg(feature = "scanout")]
        return self.scanout.is_some();
        #[cfg(not(feature = "scanout"))]
        false
    }

    /// The frame composed on the GPU behind the work submitted so far and
    /// read back as `0xFFBBGGRR`, rows of `stride` (`MgrasScanout::read`).
    /// None without a GPU unit.
    pub fn gpu_frame(&mut self, dcb: &Dcb, out: &mut [u32], stride: usize) -> Option<Result<(usize, usize), String>> {
        #[cfg(feature = "scanout")]
        {
            let sc = self.scanout.as_mut()?;
            self.rss[0].gpu_flush();
            return Some(sc.read(&self.rss[0], dcb, out, stride));
        }
        #[cfg(not(feature = "scanout"))]
        {
            let _ = (dcb, out, stride);
            None
        }
    }

    /// The frame composed on the GPU and left there (`MgrasScanout::lend`).
    pub fn gpu_lend(&mut self, dcb: &Dcb) -> Option<Result<(wgpu::Buffer, u32, u32), String>> {
        #[cfg(feature = "scanout")]
        {
            let sc = self.scanout.as_mut()?;
            self.rss[0].gpu_flush();
            return Some(sc.lend(&self.rss[0], dcb));
        }
        #[cfg(not(feature = "scanout"))]
        {
            let _ = dcb;
            None
        }
    }

    /// The frame composed into `dst` (`MgrasScanout::compose_into`).
    pub fn gpu_compose_into(&mut self, dcb: &Dcb, dst: &wgpu::Buffer, stride: u32) -> Option<Result<(u32, u32), String>> {
        #[cfg(feature = "scanout")]
        {
            let sc = self.scanout.as_mut()?;
            self.rss[0].gpu_flush();
            return Some(sc.compose_into(&self.rss[0], dcb, dst, stride));
        }
        #[cfg(not(feature = "scanout"))]
        {
            let _ = (dcb, dst, stride);
            None
        }
    }

    /// The frame composed into `dst` with ten bits a channel (`MgrasScanout::compose_into10`).
    pub fn gpu_compose_into10(&mut self, dcb: &Dcb, dst: &wgpu::Buffer, stride: u32) -> Option<Result<(u32, u32), String>> {
        #[cfg(feature = "scanout")]
        {
            let sc = self.scanout.as_mut()?;
            self.rss[0].gpu_flush();
            return Some(sc.compose_into10(&self.rss[0], dcb, dst, stride));
        }
        #[cfg(not(feature = "scanout"))]
        {
            let _ = (dcb, dst, stride);
            None
        }
    }

    /// The frame composed on the GPU with ten bits a channel and read back (`MgrasScanout::read_wide`), as
    /// x2r10g10b10 words. None without a GPU unit.
    pub fn gpu_frame_wide(&mut self, dcb: &Dcb, out: &mut Vec<u32>) -> Option<Result<(u32, u32), String>> {
        #[cfg(feature = "scanout")]
        {
            let sc = self.scanout.as_mut()?;
            self.rss[0].gpu_flush();
            return Some(sc.read_wide(&self.rss[0], dcb, out));
        }
        #[cfg(not(feature = "scanout"))]
        {
            let _ = (dcb, out);
            None
        }
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
