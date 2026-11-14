//! The GPU arm of KONA's pixel store.
//!
//! The planes (one array of words per word position, as `PixelStore` keeps them) live in GPU
//! buffers, allocated the first time a record names them. A writer hands the store an op; the ops
//! the shared batch kernels can run become records (a triangle, a clear, a box fill, a bitmap, an
//! X fill, or a copy as a barrier of two compute passes) and the CPU never touches the planes
//! for them. The rest still run on the CPU: the store makes the words they read current in the
//! CPU mirror (`observe`), runs the CPU code on the mirror, and puts what the op wrote back
//! (`Bracket`). Which ops stay on the CPU, by what the GPU kernels do not do yet:
//!
//! - triangles with fog, smoothing, multisampling or a line stipple, and clears of a multisampled
//!   window; an edge step that does not fit 32 bits; textures when the store holds more pages than
//!   the GPU's pool can (one storage binding);
//! - X pixel writes with a host payload, GL pixel writes, accumulation and the stencil write
//!   of glDrawPixels;
//! - the overlay and popup planes (word positions past the display-ID plane, which never go to the
//!   GPU), X fills with more than four clip rectangles, bitmaps of more than 1792 bits.
//!
//! # The mirror
//!
//! The mirror is an ordinary `PixelStore`, so every reader of the words, the display, the
//! checksum, the X and GL readers, reads it as it always did. It is current only where `observe`
//! has made it so: every record marks the 32 x 32 tiles of each plane it may write with its
//! generation, and `observe` reads back only the tiles written since the mirror last matched,
//! through `Gpu::read`, and only after submitting the open batch if a record it needs is still in
//! it. A read of tiles no record has touched costs no GPU work.
//!
//! # Planes
//!
//! Planes are packed into chunks, each at most 128 MiB (llvmpipe's limit for one storage
//! binding) and at most four of them bound, because indexing a runtime array of buffers is not
//! portable to the GL backend: the shader picks a chunk with a `switch`. A plane is named in a
//! slot by a reference, chunk in bits 31:28 and word offset in the chunk below. A plane that does
//! not fit the four chunks stops the machine through the shared device; there is no CPU
//! fallback for a GPU that cannot hold the screen.
//!
//! # Widths
//!
//! The X server refuses a request when a destination word it would read is wider than the target
//! (index 16 or RGB 24 bits). Reading those words back to decide would stall on every fill that
//! reads its destination, so each tile of each plane keeps an upper bound on the width class
//! (8, 16, 24 or any bits) of the words records may have stored there. When the bound is within
//! the target's, no word can be too wide and the check needs no read. A copy checks on the GPU:
//! its first pass compares every source word, and the destination words its write reads, against
//! the width mask and records the verdict, and its second pass writes unless the verdict is a
//! refusal. Nothing is read back; `copies_refused` reads the verdicts when a monitor asks.
//!
//! Locking: the state is behind one mutex, taken by a writer for one op and by the display thread
//! for one `observe`; the GPU upload ring has its own, always taken inside it.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use parking_lot::Mutex;

use super::raster::fog::curve_of;
use super::raster::setup::TriJob;
use super::raster::smooth::Cover;
use super::raster::{lit, ClearReq, DrawState, Rect};
use super::store::seam::StoreOp;
use super::store::texture::TextureStore;
use super::store::{PixelStore, DID_PLANE};
use super::x::fill::LoweredFill;
use super::x::pixel::LoweredCopy;
use super::gpu_lib::{pack_desc, FRAGMENT, LOD_MATH, TEXTURE, TEXTURE_MEM};
use super::raster::texel::TexDesc;
use super::store::texture::PLANES as TEX_PLANES;
use crate::wgpu::{library, ArchShaders, Batch, BatchPipelines, Fragment, Full, Gpu, Source, TileGrid};
use texmem::TexMem;

mod texmem;

const KONA: Fragment = Fragment::new("kona.wgsl", include_str!("kona.wgsl"));
const COPY: Fragment = Fragment::new("copy.wgsl", include_str!("copy.wgsl"));

/// Words in a record, past its slot index and the constant `one`.
const REC_W: usize = 50;
/// Record kinds, word 37 (`kona.wgsl`).
const K_TRI: u32 = 0;
const K_XFILL: u32 = 1;
const K_BOX: u32 = 2;
const K_BITMAP: u32 = 3;
const K_CLEAR: u32 = 4;
/// Where a record's box lives, for every kind but the X fill.
const R_RECT: usize = 33;
const R_KIND: usize = 37;
/// A triangle's texture coordinate planes: s/w, t/w, r/w, q/w, each at, dx, dy.
const R_TEX: usize = 38;

/// Bits a bitmap slot holds: 56 words after its five header words.
const BITMAP_BITS: u64 = 56 * 32;
/// Clip rectangles an X fill or copy takes; more run on the CPU.
const CLIPS: usize = 4;

/// Word positions that live on the GPU: the record's words and the display-ID plane. The overlay
/// and popup planes stay in the mirror.
const RESIDENT: usize = DID_PLANE + 1;
/// Chunks bound at once (`kona.wgsl` declares this many), and the bytes of one.
const MAX_CHUNKS: usize = 4;
const CHUNK_BYTES: u64 = 128 << 20;
/// Storage buffers the batch kernels bind besides the planes.
const BATCH_STORAGE: u32 = 6;
/// Storage buffers of the texture memory (descriptors, page table, page pool).
const TEX_BINDINGS: u32 = 3;
/// Dirty-tracking granularity, in pixels (the readback unit).
const TILE: u32 = 32;
/// Verdict words for copies; used up, they are summed and zeroed.
const STATUS_SLOTS: u32 = 4096;

/// The widest word each class of tile can hold: 8, 16, 24 bits, or any.
const BOUND: [u32; 4] = [0xff, 0xffff, 0xff_ffff, u32::MAX];

fn class_of(max: u32) -> u8 {
    BOUND.iter().position(|&b| max <= b).unwrap_or(3) as u8
}

/// Whether the word position `w` lives on the GPU.
pub fn resident(w: usize) -> bool {
    w < RESIDENT
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GRec {
    slot: u32,
    /// 1.0 as bits: the multiply that keeps a product apart from a sum.
    one: u32,
    w: [u32; REC_W],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GSlot {
    w: [u32; SLOT_W],
}

/// Words in a draw-state slot: the first 64 are the X fill's and the plain draw state's, the
/// rest are the features past them (`kona.wgsl`).
const SLOT_W: usize = 128;
/// The texture descriptor's offset in the descriptor table, and the flag that says there is one.
const S_TEXD: usize = 64;
const F_TEX: u32 = 1024;

/// What the store did, for tests and the monitor.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Records handed in (the generation counter).
    pub records: u64,
    /// Batch submissions.
    pub flushes: u64,
    /// `observe` calls, and those answered from the mirror with no GPU work.
    pub observes: u64,
    pub hits: u64,
    /// Fenced readbacks (one GPU wait each), the words and row ranges they moved.
    pub readbacks: u64,
    pub readback_words: u64,
    pub readback_ranges: u64,
    /// Width checks answered from the class bound, and not; copies run (each checks on the GPU).
    pub proven: u64,
    pub unproven: u64,
    pub copies: u64,
    /// Ops that ran on the CPU for want of a GPU kernel, and the words they wrote back.
    pub cpu_ops: u64,
    pub uploaded_words: u64,
    /// Texture memory syncs, and the 64 KiB pages they wrote to the GPU.
    pub tex_syncs: u64,
    pub tex_pages: u64,
}

/// A plane on the GPU: where it is, which tiles records have written, and how wide their words can be.
struct Plane {
    /// Chunk and word offset, as the slots name it.
    r: u32,
    chunk: usize,
    /// Byte offset in the chunk.
    base: u64,
    /// Per tile: the generation of the last record that may have written it, and the generation
    /// the mirror last matched it at.
    dirty: Vec<u64>,
    clean: Vec<u64>,
    /// Per tile: an upper bound on the width class of the words stored there.
    width: Vec<u8>,
}

/// X copy area (`copy.wgsl`): two pipelines, a scratch copy of one plane and a verdict word per copy.
struct CopyKernel {
    layout: wgpu::BindGroupLayout,
    gather: wgpu::ComputePipeline,
    scatter: wgpu::ComputePipeline,
    scratch: wgpu::Buffer,
    status: wgpu::Buffer,
    params: wgpu::Buffer,
    /// A bind group for each chunk, made when a copy first needs it.
    groups: Vec<Option<wgpu::BindGroup>>,
    used: u32,
    /// Verdicts already summed.
    refused: u64,
}

struct State {
    gpu: Arc<Gpu>,
    pipelines: BatchPipelines,
    batch: Batch<GRec, GSlot>,
    w: u32,
    h: u32,
    /// Words in one plane, and planes in a chunk.
    plane_words: u32,
    per_chunk: u32,
    chunks: Vec<wgpu::Buffer>,
    /// Stand-ins for the chunks not made yet: a binding wants a buffer, and one buffer cannot be
    /// bound twice for writing in a dispatch.
    spare: Vec<wgpu::Buffer>,
    layout: wgpu::BindGroupLayout,
    group: Option<wgpu::BindGroup>,
    planes: Vec<Option<Plane>>,
    allocated: u32,
    gen: u64,
    submitted: u64,
    tiles_x: u32,
    tiles_y: u32,
    slots: HashMap<[u32; SLOT_W], u32>,
    tex: TexMem,
    /// The slot the last draw state got, while the batch it is in is open.
    last: Option<(DrawState, u8, u32)>,
    copy: CopyKernel,
    stats: Stats,
}

/// The GPU arm of `Store`: the mirror, and the planes behind it.
pub struct GpuStore {
    mirror: PixelStore,
    state: Mutex<State>,
}

/// What `GpuStore::route` leaves to the caller.
pub enum Route {
    /// The GPU took the op.
    Done,
    /// The CPU runs the op on the mirror: the planes it reads or writes (those on the GPU are
    /// brought up to date before and put back after), and where.
    Cpu(Vec<usize>, Rect),
    /// Zero everything: the caller zeroes the mirror.
    Reset,
    /// The caller fills the mirror's array, then `upload`s it.
    Restore(usize),
}

/// The draw-state slot variants: a polygon triangle, a line's triangle (the stipple differs), a clear.
const MODE_POLY: u8 = 0;
const MODE_LINE: u8 = 1;
const MODE_CLEAR: u8 = 2;

impl GpuStore {
    /// A store of `width` x `height` pixels with `tex` as its texture memory (which stays in the
    /// mirror), drawing on `gpu`.
    pub fn new(gpu: Arc<Gpu>, width: u32, height: u32, tex: TextureStore) -> Result<GpuStore, String> {
        let shape = (tex.plane_pages() as u32, tex.cap_pages());
        let mirror = PixelStore::with_texture(width, height, tex);
        let state = State::new(gpu, width, height, u32::MAX, shape)?;
        Ok(GpuStore { mirror, state: Mutex::new(state) })
    }

    /// A store whose chunks hold at most `planes` planes, to exercise the chunk table on a device
    /// whose bindings would hold them all.
    #[cfg(test)]
    pub fn chunked(gpu: Arc<Gpu>, width: u32, height: u32, tex: TextureStore, planes: u32) -> Result<GpuStore, String> {
        let shape = (tex.plane_pages() as u32, tex.cap_pages());
        let mirror = PixelStore::with_texture(width, height, tex);
        let state = State::new(gpu, width, height, planes, shape)?;
        Ok(GpuStore { mirror, state: Mutex::new(state) })
    }

    /// The CPU mirror. Only the parts `observe` has refreshed are current.
    pub fn mirror(&self) -> &PixelStore {
        &self.mirror
    }

    pub fn stats(&self) -> Stats {
        let s = self.state.lock();
        Stats { tex_syncs: s.tex.syncs, tex_pages: s.tex.pages_written, ..s.stats.clone() }
    }

    /// Make the words of `word` inside `r` (store pixels, far corner exclusive) current in the
    /// mirror. Tiles no record has written since the mirror last matched are not read.
    pub fn observe(&self, word: usize, r: Rect) {
        if resident(word) {
            self.state.lock().observe(&self.mirror, word, r);
        }
    }

    /// Whether no word of `word` inside `r` can be wider than `mask`, from the records so far.
    pub fn proven(&self, word: usize, r: Rect, mask: u32) -> bool {
        // The overlay and popup planes are in the mirror alone: nothing to prove, and a check reads them.
        resident(word) && self.state.lock().proven(word, r, mask)
    }

    /// How many copies the GPU refused for a word too wide: one fenced read of the verdicts.
    pub fn copies_refused(&self) -> u64 {
        self.state.lock().copies_refused()
    }

    /// Put the mirror's `word` inside `r` on the GPU, allocating the plane. For what the CPU wrote.
    pub fn upload(&self, word: usize, r: Rect) {
        if resident(word) {
            self.state.lock().upload(&self.mirror, word, r);
        }
    }

    /// Copy the plane of `word` into `dst` at `at` bytes on the device, after the records so far,
    /// for a reader that works on the GPU (the scan-out). False for a word that lives in the
    /// mirror alone; a plane nothing has named is zeros.
    pub fn copy_plane(&self, word: usize, dst: &wgpu::Buffer, at: u64) -> bool {
        if !resident(word) {
            return false;
        }
        self.state.lock().copy_plane(word, dst, at);
        true
    }

    /// Zero the planes on the GPU and forget what is known of them.
    pub fn reset(&self) {
        self.state.lock().reset();
    }

    /// Handle `op` on the GPU if it can, else say what the CPU has to do.
    pub fn route(&self, op: &StoreOp<'_>) -> Route {
        let m = &self.mirror;
        let (w, h) = (m.width() as i64, m.height() as i64);
        let screen = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
        let mut s = self.state.lock();
        let done = match op {
            StoreOp::Plot { word, x, y, value } => {
                m.ensure(*word);
                if *x >= 0 && *y >= 0 && *x < w && *y < h {
                    let r = Rect { x0: *x as i32, y0: *y as i32, x1: *x as i32 + 1, y1: *y as i32 + 1 };
                    if !resident(*word) {
                        return Route::Cpu(vec![*word], r);
                    }
                    s.boxed(m, *word, r, *value)
                } else {
                    true
                }
            }
            StoreOp::FillBox { word, x1, y1, x2, y2, value } => {
                m.ensure(*word);
                let (xs, ys) = (lit(*x1, *x2), lit(*y1, *y2));
                let (xs, ys) = (xs.start.max(0)..xs.end.min(w), ys.start.max(0)..ys.end.min(h));
                if xs.start < xs.end && ys.start < ys.end {
                    let r = Rect { x0: xs.start as i32, y0: ys.start as i32, x1: xs.end as i32, y1: ys.end as i32 };
                    if !resident(*word) {
                        return Route::Cpu(vec![*word], r);
                    }
                    s.boxed(m, *word, r, *value)
                } else {
                    true
                }
            }
            StoreOp::Bitmap { word, x0, y0, width, height, bits, value } => {
                m.ensure(*word);
                let r = Rect { x0: (*x0).clamp(0, w) as i32, y0: (*y0).clamp(0, h) as i32, x1: (*x0 + *width as i64).clamp(0, w) as i32, y1: (*y0 + *height as i64).clamp(0, h) as i32 };
                if r.is_empty() {
                    true
                } else if *width as u64 * *height as u64 <= BITMAP_BITS && resident(*word) {
                    s.bitmap(m, *word, r, (*x0 as i32, *y0 as i32, *width), bits, *value)
                } else {
                    return Route::Cpu(vec![*word], r);
                }
            }
            StoreOp::Tri { ds, job } => {
                let r = job.rect.intersect(&screen);
                if r.is_empty() {
                    true
                } else if tri_on_gpu(ds, job) && (ds.texture.is_none() || s.tex.fits(m.texture())) {
                    s.tri(m, ds, job)
                } else {
                    return Route::Cpu(ds_planes(ds), r);
                }
            }
            StoreOp::Clear { ds, req } => {
                let r = req.rect.intersect(&screen);
                if r.is_empty() {
                    true
                } else if ds.samples.is_none() {
                    s.clear(m, ds, req, r)
                } else {
                    return Route::Cpu(ds_planes(ds), r);
                }
            }
            StoreOp::XFill { plan, b, .. } => {
                let r = plan.store_rect(*b);
                if resident(plan.word()) && plan.clip_list().len() <= CLIPS {
                    s.xfill(m, &plan.lower(*b))
                } else {
                    return Route::Cpu(vec![plan.word()], r);
                }
            }
            StoreOp::XImage { plan, .. } => return Route::Cpu(vec![plan.word()], plan.bounds_rect()),
            StoreOp::XCopy { plan, setup, rect, .. } => match super::x::pixel::lower_copy(plan, setup, *rect) {
                None => true,
                Some(c) if takes_copy(c.word, c.clips.len()) => s.copy(m, &c),
                // A copy the kernel cannot take reads and writes the plan's bounds.
                Some(c) => return Route::Cpu(vec![c.word], plan.bounds_rect()),
            },
            StoreOp::StencilWrite { word, index, .. } => {
                let (x, y) = ((*index as i64 % w) as i32, (*index as i64 / w) as i32);
                return Route::Cpu(vec![*word], Rect { x0: x, y0: y, x1: x + 1, y1: y + 1 });
            }
            StoreOp::Accumulate { ds, accumulation, colour, .. } => {
                let mut planes: Vec<usize> = accumulation.clone().map(|w| w as usize).collect();
                planes.extend(*colour);
                if ds.ownership.is_some() {
                    planes.push(DID_PLANE);
                }
                return Route::Cpu(planes, ds.clip);
            }
            StoreOp::ClearAccumulation { ds, first, values } => {
                let mut planes: Vec<usize> = (*first as usize..*first as usize + values.len()).collect();
                if ds.ownership.is_some() {
                    planes.push(DID_PLANE);
                }
                return Route::Cpu(planes, ds.clip);
            }
            StoreOp::Reset(_) => return Route::Reset,
            StoreOp::Restore { word, .. } => return Route::Restore(*word),
        };
        // False: a plane did not fit the GPU, and the device has failed, which stops the machine.
        let _ = done;
        m.touch();
        Route::Done
    }

    /// Hold the words of `planes` inside `r` for CPU code that works on the mirror: they are made
    /// current now, and when `wrote` is called the region goes back to the GPU as the bracket drops.
    pub fn bracket(&self, planes: &[usize], r: Rect) -> Bracket<'_> {
        let planes: Vec<usize> = planes.iter().copied().filter(|&w| resident(w)).collect();
        {
            let mut s = self.state.lock();
            for &w in &planes {
                s.observe(&self.mirror, w, r);
            }
        }
        Bracket { store: self, planes, rect: r, wrote: Cell::new(false) }
    }

    /// Every word of a resident plane, straight from the GPU (not through the mirror), or zeros
    /// while the plane is unallocated.
    #[cfg(test)]
    pub fn download(&self, word: usize) -> Vec<u32> {
        self.state.lock().download(word)
    }

    /// Copy `init`'s words into the mirror and the GPU, for tests that start from known planes.
    #[cfg(test)]
    pub fn load(&self, init: &PixelStore) {
        for word in init.allocated() {
            self.mirror.ensure(word);
            let (a, b) = (init.array(word).expect("allocated"), self.mirror.array(word).expect("ensured"));
            // SAFETY: both arrays cover the same managed area.
            (0..a.len()).for_each(|i| unsafe { b.store(i, a.load(i)) });
            self.upload(word, Rect { x0: 0, y0: 0, x1: init.width() as i32, y1: init.height() as i32 });
        }
    }
}

/// A CPU op in progress on the mirror. When `wrote` has been called, dropping it puts the region
/// back on the GPU.
pub struct Bracket<'a> {
    store: &'a GpuStore,
    planes: Vec<usize>,
    rect: Rect,
    wrote: Cell<bool>,
}

impl Bracket<'_> {
    /// The mirror's words in the region changed.
    pub fn wrote(&self) {
        self.wrote.set(true);
    }
}

impl Drop for Bracket<'_> {
    fn drop(&mut self) {
        if self.wrote.get() {
            let mut s = self.store.state.lock();
            s.stats.cpu_ops += 1;
            for &w in &self.planes {
                s.upload(&self.store.mirror, w, self.rect);
            }
            self.store.mirror.touch();
        }
    }
}

/// The planes a draw state reads or writes, on the GPU or not.
pub fn ds_planes(ds: &DrawState) -> Vec<usize> {
    let mut v: Vec<usize> = ds.colour[..ds.colours as usize].iter().map(|&w| w as usize).collect();
    if ds.depth_bits != 0 {
        v.push(ds.depth_word as usize);
    }
    if ds.stencil_bits != 0 {
        v.push(ds.stencil_word as usize);
    }
    if ds.ownership.is_some() {
        v.push(DID_PLANE);
    }
    if let Some(ms) = &ds.samples {
        for (first, n) in [ms.colour, ms.depth, ms.stencil] {
            v.extend((first..first + n).map(|w| w as usize));
        }
    }
    v
}

/// Whether the copy kernel takes a copy in `word` under `clips` clip rectangles.
pub fn takes_copy(word: usize, clips: usize) -> bool {
    resident(word) && clips <= CLIPS
}

/// Whether the kernels draw this triangle, texture memory aside (the pool must hold the store's
/// pages: `TexMem::fits`): one with no fog, smoothing or multisampling, and no line stipple.
fn tri_on_gpu(ds: &DrawState, job: &TriJob) -> bool {
    let t = &job.tri;
    ds.samples.is_none()
        && curve_of(ds) == 0
        && t.cover == Cover::None
        && !(job.line && ds.line_stipple.is_some())
        && (0..3).all(|k| i32::try_from(t.a[k]).is_ok() && i32::try_from(t.b[k]).is_ok())
        && ds_planes(ds).iter().all(|&w| resident(w))
}

impl State {
    fn new(gpu: Arc<Gpu>, w: u32, h: u32, chunk_planes: u32, (tex_per_plane, tex_cap_pages): (u32, usize)) -> Result<State, String> {
        let plane_bytes = w as u64 * h as u64 * 4;
        let cap = gpu.caps.max_storage_binding.min(gpu.caps.max_buffer).min(CHUNK_BYTES);
        if plane_bytes == 0 || plane_bytes > cap {
            return Err(format!("kona: a {w} x {h} plane of {plane_bytes} bytes does not fit one storage binding of {cap} bytes"));
        }
        let bound = gpu.device.limits().max_storage_buffers_per_shader_stage;
        let needed = BATCH_STORAGE + MAX_CHUNKS as u32 + TEX_BINDINGS;
        if bound < needed {
            return Err(format!("kona: the GPU binds {bound} storage buffers to a shader; {needed} are needed"));
        }
        let tex_pages = (cap / texmem::PAGE_BYTES).min(tex_cap_pages as u64) as u32;
        let tex = TexMem::new(gpu.clone(), TEX_PLANES as u32, tex_per_plane, tex_pages);
        let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let rw = wgpu::BufferBindingType::Storage { read_only: false };
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kona planes"),
            entries: &(0..MAX_CHUNKS as u32).map(|b| entry(b, rw)).collect::<Vec<_>>(),
        });
        let pipelines = BatchPipelines::new(
            &gpu,
            &ArchShaders {
                label: "kona",
                raster: &[KONA],
                bin_accept: None,
                library: &[library::I64, library::WIDE, library::F64, library::FLOAT, LOD_MATH, TEXTURE_MEM, TEXTURE, FRAGMENT],
                defines: &[("WIDTH", w), ("HEIGHT", h), ("PLANE", w * h), ("TEX_PLANES", TEX_PLANES as u32), ("TEX_PAGES_PER_PLANE", tex_per_plane)],
                layouts: &[&layout, &tex.layout],
            },
        )?;
        let batch = Batch::new(&gpu, &pipelines, TileGrid::new(w, h), 256);
        let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
        let spare = (0..MAX_CHUNKS).map(|_| gpu.buffer("kona spare chunk", 256, usage)).collect();
        let copy_layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kona copy"),
            entries: &[entry(0, rw), entry(1, rw), entry(2, rw), entry(3, wgpu::BufferBindingType::Uniform)],
        });
        let src = Source::new("kona copy").body(COPY);
        let gather = src.compute_pipeline(&gpu.device, "gather", &[&copy_layout])?;
        let scatter = src.compute_pipeline(&gpu.device, "scatter", &[&copy_layout])?;
        let copy = CopyKernel {
            layout: copy_layout,
            gather,
            scatter,
            scratch: gpu.buffer("kona copy scratch", plane_bytes, usage),
            status: gpu.buffer("kona copy status", STATUS_SLOTS as u64 * 4, usage),
            params: gpu.buffer("kona copy params", 144, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
            groups: Vec::new(),
            used: 0,
            refused: 0,
        };
        Ok(State {
            gpu,
            pipelines,
            batch,
            w,
            h,
            plane_words: w * h,
            per_chunk: ((cap / plane_bytes) as u32).min(chunk_planes),
            chunks: Vec::new(),
            spare,
            layout,
            group: None,
            planes: (0..RESIDENT).map(|_| None).collect(),
            allocated: 0,
            gen: 0,
            submitted: 0,
            tiles_x: w.div_ceil(TILE),
            tiles_y: h.div_ceil(TILE),
            slots: HashMap::new(),
            tex,
            last: None,
            copy,
            stats: Stats::default(),
        })
    }

    fn tiles(&self) -> usize {
        (self.tiles_x * self.tiles_y) as usize
    }

    /// The plane of `word`, allocated on first use, as its reference. None, with the device failed,
    /// when the planes outgrow the chunks.
    fn plane(&mut self, mirror: &PixelStore, word: usize) -> Option<u32> {
        if let Some(p) = &self.planes[word] {
            return Some(p.r);
        }
        mirror.ensure(word);
        let k = self.allocated;
        let chunk = (k / self.per_chunk) as usize;
        if chunk >= MAX_CHUNKS {
            self.gpu.fail(&format!("kona: the pixel planes need more than the {MAX_CHUNKS} chunks of {} planes the GPU binds", self.per_chunk));
            return None;
        }
        while self.chunks.len() <= chunk {
            let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
            self.chunks.push(self.gpu.buffer("kona planes", self.per_chunk as u64 * self.plane_words as u64 * 4, usage));
            self.group = None;
        }
        let off = (k % self.per_chunk) * self.plane_words;
        let n = self.tiles();
        self.planes[word] = Some(Plane { r: (chunk as u32) << 28 | off, chunk, base: off as u64 * 4, dirty: vec![0; n], clean: vec![0; n], width: vec![0; n] });
        self.allocated += 1;
        Some((chunk as u32) << 28 | off)
    }

    fn ensure_group(&mut self) {
        if self.group.is_some() {
            return;
        }
        let entries: Vec<_> = (0..MAX_CHUNKS)
            .map(|i| wgpu::BindGroupEntry { binding: i as u32, resource: self.chunks.get(i).unwrap_or(&self.spare[i]).as_entire_binding() })
            .collect();
        self.group = Some(self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("kona planes"), layout: &self.layout, entries: &entries }));
    }

    /// Submit the open batch, whatever the generation. Whether the batch is empty after it.
    fn flush_batch(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        self.ensure_group();
        let group = self.group.as_ref().expect("made above");
        match self.batch.try_flush(&self.pipelines, &[group, self.tex.group()]) {
            Ok(_) => {
                self.slots.clear();
                self.last = None;
                self.stats.flushes += 1;
                true
            }
            Err(e) => {
                self.gpu.fail(&format!("kona batch: {e}"));
                false
            }
        }
    }

    /// Submit what the records so far need submitted. Whether anything was.
    fn flush(&mut self) -> bool {
        self.submitted = self.gen;
        if self.batch.is_empty() {
            return false;
        }
        self.flush_batch();
        true
    }

    fn slot_index(&mut self, s: &GSlot) -> Option<u32> {
        if let Some(&i) = self.slots.get(&s.w) {
            return Some(i);
        }
        let i = self.batch.slot(*s).ok()?;
        self.slots.insert(s.w, i);
        Some(i)
    }

    /// Add one record to the batch, submitting it first when it is full. False when the device
    /// failed and the batch cannot be emptied.
    fn push(&mut self, slot: &GSlot, rec: impl Fn(u32) -> GRec, b: (i32, i32, i32, i32)) -> bool {
        loop {
            if let Some(i) = self.slot_index(slot) {
                match self.batch.prim(rec(i), b.0, b.1, b.2, b.3) {
                    Ok(_) => return true,
                    Err(Full) => {}
                }
            }
            if !self.flush_batch() {
                return false;
            }
        }
    }

    /// As `push`, for a draw state whose slot is cached while the batch stays open.
    fn push_ds(&mut self, key: (DrawState, u8), build: impl FnOnce(&mut Self) -> Option<GSlot>, rec: impl Fn(u32) -> GRec, b: (i32, i32, i32, i32)) -> bool {
        if let Some((d, m, i)) = &self.last {
            if *d == key.0 && *m == key.1 {
                let i = *i;
                loop {
                    match self.batch.prim(rec(i), b.0, b.1, b.2, b.3) {
                        Ok(_) => return true,
                        Err(Full) => break,
                    }
                }
                // Full: submit, which forgets the slot, and take the slow way.
                if !self.flush_batch() {
                    return false;
                }
            }
        }
        let Some(slot) = build(self) else { return false };
        if !self.push(&slot, &rec, b) {
            return false;
        }
        if let Some(&i) = self.slots.get(&slot.w) {
            self.last = Some((key.0, key.1, i));
        }
        true
    }

    /// Mark the tiles of `r` (store pixels) of a plane as written by the record being made, and
    /// widen their word class.
    fn mark(&mut self, word: usize, r: Rect, class: u8) {
        let (x0, y0, x1, y1) = (r.x0.max(0) as u32, r.y0.max(0) as u32, (r.x1.max(0) as u32).min(self.w), (r.y1.max(0) as u32).min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (gen, tx) = (self.gen, self.tiles_x);
        let Some(p) = &mut self.planes[word] else { return };
        for ty in y0 / TILE..=(y1 - 1) / TILE {
            for x in x0 / TILE..=(x1 - 1) / TILE {
                let t = (ty * tx + x) as usize;
                p.dirty[t] = gen;
                p.width[t] = p.width[t].max(class);
            }
        }
    }

    fn class_in(&self, word: usize, r: Rect) -> u8 {
        let (x0, y0, x1, y1) = (r.x0.max(0) as u32, r.y0.max(0) as u32, (r.x1.max(0) as u32).min(self.w), (r.y1.max(0) as u32).min(self.h));
        let Some(p) = &self.planes[word] else { return 0 };
        let mut m = 0;
        if x0 < x1 && y0 < y1 {
            for ty in y0 / TILE..=(y1 - 1) / TILE {
                for tx in x0 / TILE..=(x1 - 1) / TILE {
                    m = m.max(p.width[(ty * self.tiles_x + tx) as usize]);
                }
            }
        }
        m
    }

    fn proven(&mut self, word: usize, r: Rect, mask: u32) -> bool {
        let ok = BOUND[self.class_in(word, r) as usize] <= mask;
        if ok {
            self.stats.proven += 1;
        } else {
            self.stats.unproven += 1;
        }
        ok
    }

    // ---- records ------------------------------------------------------------

    /// A box of one value, in store pixels.
    fn boxed(&mut self, m: &PixelStore, word: usize, r: Rect, value: u32) -> bool {
        let Some(plane) = self.plane(m, word) else { return false };
        self.gen += 1;
        self.stats.records += 1;
        let mut slot = GSlot { w: [0; SLOT_W] };
        slot.w[0] = plane;
        let mut rec = GRec { slot: 0, one: 1.0f32.to_bits(), w: [0; REC_W] };
        rec.w[0] = value;
        rec.w[R_RECT] = r.x0 as u32;
        rec.w[R_RECT + 1] = r.y0 as u32;
        rec.w[R_RECT + 2] = r.x1 as u32;
        rec.w[R_RECT + 3] = r.y1 as u32;
        rec.w[R_KIND] = K_BOX;
        self.mark(word, r, class_of(value));
        self.push(&slot, move |i| GRec { slot: i, ..rec }, (r.x0, r.y0, r.x1 - 1, r.y1 - 1))
    }

    /// A bitmap whose visible part is `r`.
    fn bitmap(&mut self, m: &PixelStore, word: usize, r: Rect, (x0, y0, width): (i32, i32, u32), bits: &[u32], value: u32) -> bool {
        let Some(plane) = self.plane(m, word) else { return false };
        self.gen += 1;
        self.stats.records += 1;
        let mut slot = GSlot { w: [0; SLOT_W] };
        slot.w[0] = plane;
        slot.w[1] = x0 as u32;
        slot.w[2] = y0 as u32;
        slot.w[3] = width;
        slot.w[4] = value;
        // Missing words read as clear; the slot is zero past them.
        for (k, b) in bits.iter().take(56).enumerate() {
            slot.w[5 + k] = *b;
        }
        let mut rec = GRec { slot: 0, one: 1.0f32.to_bits(), w: [0; REC_W] };
        rec.w[R_RECT] = r.x0 as u32;
        rec.w[R_RECT + 1] = r.y0 as u32;
        rec.w[R_RECT + 2] = r.x1 as u32;
        rec.w[R_RECT + 3] = r.y1 as u32;
        rec.w[R_KIND] = K_BITMAP;
        self.mark(word, r, class_of(value));
        self.push(&slot, move |i| GRec { slot: i, ..rec }, (r.x0, r.y0, r.x1 - 1, r.y1 - 1))
    }

    /// The slot of a draw state. None with the device failed.
    fn ds_slot(&mut self, m: &PixelStore, ds: &DrawState, mode: u8) -> Option<GSlot> {
        let mut s = GSlot { w: [0; SLOT_W] };
        let mut fl = 0u32;
        let stencil_on = ds.stencil_bits != 0 && ds.stencil.enabled;
        if ds.depth_bits != 0 {
            fl |= 1;
        }
        if ds.depth_test {
            fl |= 2;
        }
        if ds.depth_mask {
            fl |= 4;
        }
        if ds.alpha.enabled {
            fl |= 8;
        }
        if stencil_on {
            fl |= 16;
        }
        if ds.blend.enabled {
            fl |= 32;
        }
        if ds.logic.is_some() {
            fl |= 64;
        }
        if ds.ownership.is_some() {
            fl |= 128;
        }
        if mode == MODE_POLY && ds.polygon_stipple.is_some() {
            fl |= 256;
        }
        if ds.colour_mask != [true; 4] {
            fl |= 512;
        }
        s.w[0] = fl;
        s.w[1] = ds.colours as u32;
        for k in 0..ds.colours as usize {
            s.w[2 + k] = self.plane(m, ds.colour[k] as usize)?;
        }
        if ds.depth_bits != 0 {
            s.w[4] = self.plane(m, ds.depth_word as usize)?;
        }
        if stencil_on || (mode == MODE_CLEAR && ds.stencil_bits != 0) {
            s.w[5] = self.plane(m, ds.stencil_word as usize)?;
        }
        s.w[6] = ds.depth_func as u32;
        s.w[7] = ds.alpha.func as u32;
        s.w[8] = ds.alpha.reference;
        s.w[9] = ds.stencil.func as u32;
        s.w[10] = ds.stencil.reference as u32;
        s.w[11] = ds.stencil.read_mask as u32;
        s.w[12] = ds.stencil.write_mask as u32;
        s.w[13] = ds.stencil.fail as u32;
        s.w[14] = ds.stencil.zfail as u32;
        s.w[15] = ds.stencil.zpass as u32;
        s.w[16] = ds.blend.src as u32;
        s.w[17] = ds.blend.dst as u32;
        s.w[18] = ds.blend.eq as u32;
        s.w[19..23].copy_from_slice(&ds.blend.constant);
        s.w[23] = ds.logic.map_or(0, |l| l as u32);
        let m_ = ds.colour_mask;
        s.w[24] = (!m_[0] as u32 * 0xff) << 16 | (!m_[1] as u32 * 0xff) << 8 | (!m_[2] as u32 * 0xff) | (!m_[3] as u32 * 0xff) << 24;
        if let Some(o) = &ds.ownership {
            s.w[25] = o.reference;
            s.w[26] = o.mask;
            s.w[27] = self.plane(m, DID_PLANE)?;
        }
        s.w[28] = ds.origin[0] as u32;
        s.w[29] = ds.origin[1] as u32;
        if let Some(d) = &ds.texture {
            s.w[0] |= F_TEX;
            s.w[S_TEXD] = self.desc_offset(d)?;
        }
        if mode == MODE_POLY {
            if let Some(rows) = &ds.polygon_stipple {
                s.w[32..64].copy_from_slice(rows);
            }
        }
        Some(s)
    }

    /// Where the descriptor of `d` is in the table the kernel reads. None with the device failed.
    fn desc_offset(&mut self, d: &TexDesc) -> Option<u32> {
        let words = pack_desc(d);
        loop {
            if let Some(o) = self.tex.desc(&words) {
                return Some(o);
            }
            // The table is full: the records that name its entries go out first.
            if !self.flush_batch() {
                return None;
            }
            self.tex.reset_descs();
        }
    }

    /// Bring the GPU's texture memory up to the store's, for a draw that samples. The records still
    /// open read the old words, so they are submitted first.
    fn tex_sync(&mut self, m: &PixelStore) -> bool {
        if !self.tex.stale(m.texture()) {
            return true;
        }
        if !self.flush_batch() {
            return false;
        }
        self.tex.sync(m.texture());
        true
    }

    fn tri(&mut self, m: &PixelStore, ds: &DrawState, job: &TriJob) -> bool {
        if ds.texture.is_some() && !self.tex_sync(m) {
            return false;
        }
        self.gen += 1;
        self.stats.records += 1;
        let r = job.rect;
        let written = ds_written(ds);
        for &(w, class) in &written {
            self.plane(m, w);
            self.mark(w, r, class);
        }
        let rec = tri_rec(job);
        let mode = if job.line { MODE_LINE } else { MODE_POLY };
        let ds_ = *ds;
        self.push_ds((*ds, mode), move |s| s.ds_slot(m, &ds_, mode), move |i| GRec { slot: i, ..rec }, (r.x0, r.y0, r.x1 - 1, r.y1 - 1))
    }

    fn clear(&mut self, m: &PixelStore, ds: &DrawState, q: &ClearReq, r: Rect) -> bool {
        self.gen += 1;
        self.stats.records += 1;
        // What the CPU path writes: each only if the window has the plane (`pixel::run`).
        let colour = q.colour.filter(|_| ds.colours != 0);
        let depth = q.depth.filter(|_| ds.depth_bits != 0 && q.depth_mask);
        let stencil = q.stencil.filter(|_| ds.stencil_bits != 0);
        let m_ = q.colour_mask;
        let keep = (!m_[0] as u32 * 0xff) << 16 | (!m_[1] as u32 * 0xff) << 8 | (!m_[2] as u32 * 0xff) | (!m_[3] as u32 * 0xff) << 24;
        let mut rec = GRec { slot: 0, one: 1.0f32.to_bits(), w: [0; REC_W] };
        rec.w[0] = colour.unwrap_or(0);
        rec.w[1] = depth.unwrap_or(0);
        rec.w[2] = stencil.unwrap_or(0) as u32;
        rec.w[3] = q.stencil_mask as u32;
        rec.w[4] = keep;
        rec.w[5] = colour.is_some() as u32 | (depth.is_some() as u32) << 1 | (stencil.is_some() as u32) << 2;
        rec.w[R_RECT] = r.x0 as u32;
        rec.w[R_RECT + 1] = r.y0 as u32;
        rec.w[R_RECT + 2] = r.x1 as u32;
        rec.w[R_RECT + 3] = r.y1 as u32;
        rec.w[R_KIND] = K_CLEAR;
        if colour.is_some() {
            for &c in &ds.colour[..ds.colours as usize] {
                self.plane(m, c as usize);
                self.mark(c as usize, r, 3);
            }
        }
        if depth.is_some() {
            self.plane(m, ds.depth_word as usize);
            self.mark(ds.depth_word as usize, r, 3);
        }
        if stencil.is_some() {
            self.plane(m, ds.stencil_word as usize);
            self.mark(ds.stencil_word as usize, r, 0);
        }
        let ds_ = *ds;
        self.push_ds((*ds, MODE_CLEAR), move |s| s.ds_slot(m, &ds_, MODE_CLEAR), move |i| GRec { slot: i, ..rec }, (r.x0, r.y0, r.x1 - 1, r.y1 - 1))
    }

    /// An X fill: one clipped box of a request.
    fn xfill(&mut self, m: &PixelStore, f: &LoweredFill<'_>) -> bool {
        debug_assert_eq!(f.height, self.h as i64);
        let Some(plane) = self.plane(m, f.word) else { return false };
        self.gen += 1;
        self.stats.records += 1;
        let (w, h) = (self.w as i64, self.h as i64);
        let mut s = GSlot { w: [0; SLOT_W] };
        s.w[0] = plane;
        s.w[1] = f.fg;
        s.w[2] = f.bg;
        s.w[3] = f.mask;
        s.w[4] = f.width_mask;
        s.w[5] = f.gx;
        s.w[6] = f.needs as u32 | (f.opaque as u32) << 1 | (f.stipple.is_some() as u32) << 2;
        if let Some(p) = &f.stipple {
            s.w[7] = p.width as u32;
            s.w[8] = p.height as u32;
            s.w[9] = p.origin.0 as i32 as u32;
            s.w[10] = p.origin.1 as i32 as u32;
            s.w[28..28 + p.rows.len()].copy_from_slice(&p.rows);
        }
        s.w[11] = f.clips.len() as u32;
        for (k, c) in f.clips.iter().enumerate() {
            for j in 0..4 {
                s.w[12 + 4 * k + j] = c[j] as u32;
            }
        }
        let (x1, y1, x2, y2) = f.b;
        let mut rec = GRec { slot: 0, one: 1.0f32.to_bits(), w: [0; REC_W] };
        rec.w[0] = x1 as i32 as u32;
        rec.w[1] = y1 as i32 as u32;
        rec.w[2] = x2 as i32 as u32;
        rec.w[3] = y2 as i32 as u32;
        rec.w[R_KIND] = K_XFILL;
        // Store rows run the other way: X row y is store row h - 1 - y.
        let r = Rect { x0: x1.clamp(0, w) as i32, y0: (h - y2).clamp(0, h) as i32, x1: x2.clamp(0, w) as i32, y1: (h - y1).clamp(0, h) as i32 };
        if r.is_empty() {
            return true;
        }
        self.mark(f.word, r, class_of(f.width_mask));
        self.push(&s, move |i| GRec { slot: i, ..rec }, (r.x0, r.y0, r.x1 - 1, r.y1 - 1))
    }

    /// An X copy area, as a barrier: the open batch is submitted first (the copy reads what earlier
    /// records wrote), then the gather and scatter dispatches run behind it, and later batches run
    /// behind them. The width check is made on the GPU; nothing is read back.
    fn copy(&mut self, m: &PixelStore, c: &LoweredCopy<'_>) -> bool {
        let Some(plane) = self.plane(m, c.word) else { return false };
        self.gen += 1;
        self.stats.records += 1;
        self.stats.copies += 1;
        if !self.flush_batch() {
            return false;
        }
        self.submitted = self.gen;
        if self.copy.used == STATUS_SLOTS {
            self.sum_status();
        }
        let slot = self.copy.used;
        self.copy.used += 1;
        let (chunk, off) = ((plane >> 28) as usize, plane & 0x0fff_ffff);
        let mut p = [0u32; 36];
        p[0] = off;
        p[1] = self.w;
        p[2] = self.h;
        p[3] = slot;
        p[4] = c.sx as i32 as u32;
        p[5] = c.sy as i32 as u32;
        p[6] = c.dx as i32 as u32;
        p[7] = c.dy as i32 as u32;
        p[8] = c.i0 as i32 as u32;
        p[9] = c.i1 as i32 as u32;
        p[10] = c.j0 as i32 as u32;
        p[11] = c.j1 as i32 as u32;
        p[12] = c.gx;
        p[13] = c.mask;
        p[14] = c.width_mask;
        p[15] = c.needs as u32;
        p[16] = c.clips.len() as u32;
        for (k, r) in c.clips.iter().enumerate() {
            for j in 0..4 {
                p[20 + 4 * k + j] = r[j] as u32;
            }
        }
        // The write lands before the work of the next submission and after the earlier ones, so
        // one buffer serves every copy.
        self.gpu.write(&self.copy.params, 0, bytemuck::cast_slice(&p));
        if self.copy.groups.len() <= chunk {
            self.copy.groups.resize_with(chunk + 1, || None);
        }
        if self.copy.groups[chunk].is_none() {
            self.copy.groups[chunk] = Some(self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kona copy"),
                layout: &self.copy.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.chunks[chunk].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: self.copy.scratch.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.copy.status.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: self.copy.params.as_entire_binding() },
                ],
            }));
        }
        let group = self.copy.groups[chunk].as_ref().expect("made above");
        let (ni, nj) = ((c.i1 - c.i0) as u32, (c.j1 - c.j0) as u32);
        let mut enc = self.gpu.encoder("kona copy");
        for pipe in [&self.copy.gather, &self.copy.scatter] {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("kona copy"), timestamp_writes: None });
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, group, &[]);
            pass.dispatch_workgroups(ni.div_ceil(8), nj.div_ceil(8), 1);
        }
        self.gpu.submit(enc);
        // Destination tiles: written, and as wide as anything the source could hold.
        let h = self.h as i64;
        let class = self.class_in(c.word, Rect { x0: (c.sx + c.i0) as i32, y0: (h - (c.sy + c.j1)) as i32, x1: (c.sx + c.i1) as i32, y1: (h - (c.sy + c.j0)) as i32 });
        let dst = Rect { x0: (c.dx + c.i0) as i32, y0: (h - (c.dy + c.j1)) as i32, x1: (c.dx + c.i1) as i32, y1: (h - (c.dy + c.j0)) as i32 };
        self.mark(c.word, dst, class);
        true
    }

    /// Sum the verdicts of the copies so far and zero them, for another run of slots.
    fn sum_status(&mut self) {
        self.copy.refused += self.read_status();
        let mut enc = self.gpu.encoder("kona copy status");
        enc.clear_buffer(&self.copy.status, 0, None);
        self.gpu.submit(enc);
        self.copy.used = 0;
    }

    fn read_status(&mut self) -> u64 {
        if self.copy.used == 0 {
            return 0;
        }
        let mut out = vec![0u8; self.copy.used as usize * 4];
        if let Err(e) = self.gpu.read(&self.copy.status, &[(0, out.len() as u64)], &mut out) {
            self.gpu.fail(&format!("kona copy verdicts: {e}"));
            return 0;
        }
        bytemuck::pod_collect_to_vec::<u8, u32>(&out).iter().map(|&v| v as u64).sum()
    }

    fn copies_refused(&mut self) -> u64 {
        self.flush();
        self.copy.refused + self.read_status()
    }

    // ---- reading back and putting back ----------------------------------------

    /// Make the pixels of `word` inside `r` (store pixels, far corner exclusive) current in the mirror.
    fn observe(&mut self, mirror: &PixelStore, word: usize, r: Rect) {
        self.stats.observes += 1;
        let (x0, y0, x1, y1) = (r.x0.max(0) as u32, r.y0.max(0) as u32, (r.x1.max(0) as u32).min(self.w), (r.y1.max(0) as u32).min(self.h));
        let Some(p) = &self.planes[word] else {
            self.stats.hits += 1;
            return;
        };
        if x0 >= x1 || y0 >= y1 {
            self.stats.hits += 1;
            return;
        }
        let (tx0, tx1, ty0, ty1) = (x0 / TILE, (x1 - 1) / TILE, y0 / TILE, (y1 - 1) / TILE);
        let tx = self.tiles_x;
        let stale = |p: &Plane, x: u32, y: u32| {
            let t = (y * tx + x) as usize;
            p.dirty[t] > p.clean[t]
        };
        let mut newest = 0;
        for ty in ty0..=ty1 {
            for x in tx0..=tx1 {
                if stale(p, x, ty) {
                    newest = newest.max(p.dirty[(ty * tx + x) as usize]);
                }
            }
        }
        if newest == 0 {
            self.stats.hits += 1;
            return;
        }
        if newest > self.submitted {
            self.flush();
        }
        let p = self.planes[word].as_ref().expect("checked above");
        // Whole stale tiles, one range per pixel row and run of adjacent stale tiles.
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        let mut spans: Vec<(u32, u32, u32)> = Vec::new(); // (y, x_start, x_end)
        for ty in ty0..=ty1 {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            let mut x = tx0;
            while x <= tx1 {
                if stale(p, x, ty) {
                    let s = x;
                    while x < tx1 && stale(p, x + 1, ty) {
                        x += 1;
                    }
                    runs.push((s * TILE, ((x + 1) * TILE).min(self.w)));
                }
                x += 1;
            }
            for y in ty * TILE..((ty + 1) * TILE).min(self.h) {
                for &(a, b) in &runs {
                    ranges.push((p.base + (y as u64 * self.w as u64 + a as u64) * 4, (b - a) as u64 * 4));
                    spans.push((y, a, b));
                }
            }
        }
        let mut bytes = vec![0u8; ranges.iter().map(|r| r.1).sum::<u64>() as usize];
        if let Err(e) = self.gpu.read(&self.chunks[p.chunk], &ranges, &mut bytes) {
            // The mirror stays as it was; the device has failed and the machine stops.
            self.gpu.fail(&format!("kona pixel read-back: {e}"));
            return;
        }
        let words: Vec<u32> = bytemuck::pod_collect_to_vec(&bytes);
        mirror.ensure(word);
        let a = mirror.array(word).expect("ensured");
        let mut at = 0;
        for &(y, xa, xb) in &spans {
            for x in xa..xb {
                // SAFETY: inside the managed area.
                unsafe { a.store((y * self.w + x) as usize, words[at]) };
                at += 1;
            }
        }
        let p = self.planes[word].as_mut().expect("checked above");
        for ty in ty0..=ty1 {
            for x in tx0..=tx1 {
                if p.dirty[(ty * tx + x) as usize] > p.clean[(ty * tx + x) as usize] {
                    let t = (ty * tx + x) as usize;
                    p.clean[t] = p.dirty[t];
                }
            }
        }
        self.stats.readbacks += 1;
        self.stats.readback_words += at as u64;
        self.stats.readback_ranges += ranges.len() as u64;
    }

    /// Write the mirror's `word` inside `r` to the GPU, allocating the plane. The tiles it touches
    /// were made current first (or all of the plane is being replaced), so the rest of each stays
    /// as it was on both sides.
    fn upload(&mut self, mirror: &PixelStore, word: usize, r: Rect) {
        let (x0, y0, x1, y1) = (r.x0.max(0) as u32, r.y0.max(0) as u32, (r.x1.max(0) as u32).min(self.w), (r.y1.max(0) as u32).min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // Records still open write this plane before the words do.
        self.flush();
        let Some(_) = self.plane(mirror, word) else { return };
        let Some(a) = mirror.array(word) else { return };
        let (tx, w) = (self.tiles_x, self.w);
        let p = self.planes[word].as_mut().expect("made above");
        let mut row = vec![0u32; (x1 - x0) as usize];
        for y in y0..y1 {
            let base = (y * w + x0) as usize;
            for (k, v) in row.iter_mut().enumerate() {
                // SAFETY: inside the managed area.
                *v = unsafe { a.load(base + k) };
            }
            for x in x0 / TILE..=(x1 - 1) / TILE {
                let (a0, a1) = ((x * TILE).max(x0), ((x + 1) * TILE).min(x1));
                let max = row[(a0 - x0) as usize..(a1 - x0) as usize].iter().copied().max().unwrap_or(0);
                let t = ((y / TILE) * tx + x) as usize;
                p.width[t] = p.width[t].max(class_of(max));
            }
            self.gpu.write(&self.chunks[p.chunk], p.base + (y as u64 * w as u64 + x0 as u64) * 4, bytemuck::cast_slice(&row));
        }
        self.stats.uploaded_words += (x1 - x0) as u64 * (y1 - y0) as u64;
    }

    fn copy_plane(&mut self, word: usize, dst: &wgpu::Buffer, at: u64) {
        self.flush();
        let bytes = self.plane_words as u64 * 4;
        let mut enc = self.gpu.encoder("kona plane copy");
        match &self.planes[word] {
            Some(p) => enc.copy_buffer_to_buffer(&self.chunks[p.chunk], p.base, dst, at, bytes),
            None => enc.clear_buffer(dst, at, Some(bytes)),
        }
        self.gpu.submit(enc);
    }

    /// Zero every plane on the GPU.
    fn reset(&mut self) {
        self.flush();
        let mut enc = self.gpu.encoder("kona reset");
        for c in &self.chunks {
            enc.clear_buffer(c, 0, None);
        }
        enc.clear_buffer(&self.copy.status, 0, None);
        self.gpu.submit(enc);
        self.copy.used = 0;
        self.copy.refused = 0;
        for p in self.planes.iter_mut().flatten() {
            p.dirty.fill(0);
            p.clean.fill(0);
            p.width.fill(0);
        }
    }

    #[cfg(test)]
    fn download(&mut self, word: usize) -> Vec<u32> {
        self.flush();
        let Some(p) = &self.planes[word] else { return vec![0; self.plane_words as usize] };
        let mut out = vec![0u8; self.plane_words as usize * 4];
        self.gpu.read(&self.chunks[p.chunk], &[(p.base, out.len() as u64)], &mut out).expect("readback");
        bytemuck::pod_collect_to_vec(&out)
    }
}

/// The planes a triangle writes, and the width class of what it stores in each: a triangle stores
/// full colour words and depths, and stencil is 8 bits.
fn ds_written(ds: &DrawState) -> Vec<(usize, u8)> {
    let mut v: Vec<(usize, u8)> = ds.colour[..ds.colours as usize].iter().map(|&c| (c as usize, 3)).collect();
    if ds.depth_bits != 0 {
        v.push((ds.depth_word as usize, 3));
    }
    if ds.stencil_bits != 0 && ds.stencil.enabled {
        v.push((ds.stencil_word as usize, 0));
    }
    v
}

fn tri_rec(job: &TriJob) -> GRec {
    let t = &job.tri;
    let mut w = [0u32; REC_W];
    for k in 0..3 {
        // `tri_on_gpu` checked that these fit.
        w[k] = t.a[k] as i32 as u32;
        w[3 + k] = t.b[k] as i32 as u32;
        w[6 + 2 * k] = t.c[k] as u64 as u32;
        w[7 + 2 * k] = (t.c[k] as u64 >> 32) as u32;
        w[12] |= (t.tie[k] as u32) << k;
    }
    w[13] = t.origin[0] as u32;
    w[14] = t.origin[1] as u32;
    for k in 0..4 {
        w[15 + k] = t.at_origin[k].to_bits();
        w[19 + k] = t.dx[k].to_bits();
        w[23 + k] = t.dy[k].to_bits();
    }
    for (n, v) in [t.z.at, t.z.dx, t.z.dy].into_iter().enumerate() {
        w[27 + 2 * n] = v as u64 as u32;
        w[28 + 2 * n] = (v as u64 >> 32) as u32;
    }
    for (k, plane) in t.tex.iter().enumerate() {
        for (n, v) in plane.iter().enumerate() {
            w[R_TEX + 3 * k + n] = v.to_bits();
        }
    }
    w[R_RECT] = job.rect.x0 as u32;
    w[R_RECT + 1] = job.rect.y0 as u32;
    w[R_RECT + 2] = job.rect.x1 as u32;
    w[R_RECT + 3] = job.rect.y1 as u32;
    w[R_KIND] = K_TRI;
    GRec { slot: 0, one: 1.0f32.to_bits(), w }
}

#[cfg(test)]
pub(super) mod tests;
