//! What a front end calls: the VPro raster back end over its own board
//! memory, with no board, bus or display behind it.
//!
//! The front end decodes the command stream into the three word records
//! of a draw (`FragmentWords`), assembles vertices into `Prim`s under a
//! transform (`XfState`), and says where drawing lands (`DestViews`). The
//! back end sets up, walks, shades and stores them, on the CPU or through
//! the GPU unit, into board memory it owns.

use std::sync::Arc;

use super::clip::ClipRect;
use super::consts::{write, ODSY_DEPTH24_MAX, ODSY_ID_COMPARE_ENABLE, ODSY_ID_COMPARE_MASK_SHIFT};
use super::gpu_unit::{GpuFault, Observation, Slot};
use super::pipe::backend::{pack_rgba8, quantise8, ClearDraw, DestMem};
use super::pipe::setup::{self, LiveDraw, Transform};
use super::pipe::state::{FragmentWords, RasterWords};
use super::pipe::texture::{Latch, TexRegs, Unit};
use super::pipe::PipeDraw;
use super::assembly::{Prim, XfState};
use super::view::DestViews;
use super::vram::{Vram, VramPtr};
use super::wgpu::GpuRaster;
use crate::wgpu::Gpu;


/// The plane the GPU's batch grid covers, per axis (the board's own).
pub const PLANE_SPAN: u32 = super::wgpu::PLANE_SPAN;

/// What the transform side hands a draw besides the matrices.
#[derive(Clone, Copy, Debug)]
pub struct DepthMap {
    /// The depth row and divisor of the combined matrix, and the depth
    /// rounding slack.
    pub row: [f64; 4],
    pub divisor: Option<[f64; 4]>,
    pub slack: f64,
}

/// A rectangle clear: the colour, depth and stencil stored over a box.
#[derive(Clone, Copy, Debug)]
pub struct Clear {
    /// Inclusive plane pixels.
    pub rect: (i32, i32, i32, i32),
    /// The colour in [0, 1], quantised as a draw's store takes it.
    pub rgba: Option<[f32; 4]>,
    pub depth24: Option<u32>,
    /// The stencil value and its write mask.
    pub stencil: Option<(u8, u8)>,
}

/// Pixels read back from the planes.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct Planes {
    pub colour: Vec<u32>,
    pub depth: Vec<u32>,
    pub stencil: Vec<u32>,
}

impl Planes {
    /// blake3 over the three planes as big-endian words, the form iris's
    /// golden scenes keep.
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        for plane in [&self.colour, &self.depth, &self.stencil] {
            for w in plane {
                h.update(&w.to_be_bytes());
            }
        }
        h.finalize().to_hex().to_string()
    }
}

/// The raster back end and the board memory it draws into.
pub struct VproBack {
    vram: Vram,
    gpu: Option<Box<GpuRaster>>,
    fault: GpuFault,
}

impl VproBack {
    /// A back end over `vram_bytes` (a power of two) of board memory, on
    /// the GPU when one is given and on the CPU otherwise.
    pub fn new(vram_bytes: u64, gpu: Option<&Arc<Gpu>>) -> Result<VproBack, String> {
        let mut vram = Vram::new(vram_bytes);
        let fault: GpuFault = Arc::new(parking_lot::Mutex::new(None));
        let gpu = match gpu {
            Some(g) => {
                let mut unit = GpuRaster::new(g, vram_bytes, PLANE_SPAN, PLANE_SPAN)?;
                unit.attach_fault(fault.clone());
                vram.track_dirty();
                unit.attach_mirror(vram.ptr());
                Some(Box::new(unit))
            }
            None => None,
        };
        Ok(VproBack { vram, gpu, fault })
    }

    /// Board memory as the CPU sees it. Current only after `sync` when
    /// the GPU draws.
    pub fn memory(&mut self) -> VramPtr {
        self.vram.ptr()
    }

    /// Batches flushed and records drawn on the GPU so far; None on the
    /// CPU.
    pub fn gpu_counts(&self) -> Option<(u64, u64)> {
        self.gpu.as_deref().map(|g| g.counts())
    }

    /// The first failure the GPU unit reported.
    pub fn fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }

    /// Store a clear over `clip` (plane-pixel boxes the window owns).
    pub fn clear(&mut self, dest: &DestViews, clip: &[ClipRect], c: &Clear) {
        let planes = DestMem::of(self.vram.ptr(), dest);
        let clear = ClearDraw {
            planes,
            targets: [true, false],
            colour: c.rgba.map(|v| pack_rgba8(v.map(|x| quantise8(x as f64)))),
            colour_mask: u32::MAX,
            depth: c.depth24.map(|d| d & ODSY_DEPTH24_MAX),
            stencil: c.stencil,
            id_value: 0,
            id_mask: 0,
        };
        let (x0, y0, x1, y1) = c.rect;
        let rects = clip.iter().map(|r| (x0.max(r.x0), y0.max(r.y0), x1.min(r.x1), y1.min(r.y1))).filter(|r| r.0 <= r.2 && r.1 <= r.3);
        match self.gpu.as_deref_mut() {
            Some(g) => {
                let words = RasterWords { clip_rect: pad_clip(clip), clip_kept: clip.len().min(4) as u32, ..Default::default() };
                match Slot::clear(&clear.planes, &words) {
                    Ok(slot) => {
                        for (a, b, c, d) in rects {
                            g.push_rect(a, b, c, d, &clear, &slot);
                        }
                    }
                    Err(e) => g.reject("GPU clear slot", e.0),
                }
            }
            None => {
                for (a, y0, c, y1) in rects {
                    for y in y0..=y1 {
                        clear.span(y, a, c);
                    }
                }
            }
        }
    }

    /// Draw one assembled primitive under `words` and the transform in
    /// `prim.xf`. Untextured; a textured draw also needs the texture
    /// registers' words, which the front end sets through `TexRegs`.
    pub fn draw(&mut self, words: &FragmentWords, depth: &DepthMap, prim: &mut Prim) -> Result<(), String> {
        let (regs, latch) = (TexRegs::default(), Latch::default());
        let ptr = self.vram.ptr();
        let xf = Transform { st: prim.xf, depth_row: depth.row, divisor: depth.divisor, back_colours: prim.back_colours };
        let stipple = words.raster.polygon_stipple();
        let tex = Unit { regs: &regs, latch: &latch, mem: ptr };
        // SAFETY of 'static: the pipe borrows the texture only for this call.
        let tex: Unit<'static> = unsafe { std::mem::transmute(tex) };
        let planes = DestMem::of(ptr, &words.backend.dest);
        let live = LiveDraw::default();
        let pipe = PipeDraw::new(words, tex, planes, false, false, xf.depth_range(), depth.slack, None, stipple.as_ref(), None, self.gpu.as_deref_mut())?;

        setup::run(&pipe, prim, xf, stipple.as_ref(), None, &live);
        Ok(())
    }

    /// The colour (buffer A), depth and stencil words of the `width` x
    /// `height` pixels at the window's origin, row by row: the planes as a
    /// reader of board memory sees them. Call `sync` first.
    pub fn planes(&mut self, dest: &DestViews, width: i32, height: i32) -> Planes {
        let m = DestMem::of(self.vram.ptr(), dest);
        let mut p = Planes::default();
        for y in 0..height {
            for x in 0..width {
                p.colour.push(m.colour[0].load_word(x, y).unwrap_or(0));
                p.depth.push(m.depth.load_word(x, y).unwrap_or(0));
                p.stencil.push(m.stencil.load1(x, y).unwrap_or(0) as u32);
            }
        }
        p
    }

    /// Wait for the GPU to store everything submitted and bring the CPU's
    /// memory current. A no-op on the CPU.
    pub fn sync(&mut self) -> Result<(), String> {
        match self.gpu.as_deref_mut() {
            Some(g) => {
                g.wait()?;
                g.read_back_now(Observation::Frontend)
            }
            None => Ok(()),
        }
    }
}

fn pad_clip(clip: &[ClipRect]) -> [ClipRect; 4] {
    let mut out = [ClipRect { x0: 0, y0: 0, x1: 0, y1: 0 }; 4];
    for (o, c) in out.iter_mut().zip(clip) {
        *o = *c;
    }
    out
}

#[allow(unused_imports)]
use {write as _, ODSY_ID_COMPARE_ENABLE as _, ODSY_ID_COMPARE_MASK_SHIFT as _, XfState as _};
