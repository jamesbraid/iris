//! The fragment pipeline's screen tiles and their worker pool
//! (`clanker/odyssey/screen-tiles.md`).
//!
//! The raster thread sets primitives up and writes each as one command in
//! the next slot of a ring: a triangle (its record and bounding box), a
//! quad it shaded itself (lines, points, smoothed polygons), or a clear's
//! rectangle. It then appends the slot's index to the queue of every screen
//! tile (`TILE` pixels square, in plane coordinates) the command touches.
//! A tile with commands waiting goes on the ready queue; a worker takes it
//! and runs its commands in order: walks the triangle within the tile and
//! shades and stores each quad, stores the shaded quad, or clears its part
//! of the rectangle. One worker owns a tile at a time, so every pixel sees
//! its fragments in command order with no other ordering, and a large
//! primitive is drawn by as many workers as it covers tiles.
//!
//! A slot counts the tiles still to run it; the raster thread retires
//! slots whose count is zero in order (`tail`), behind which it writes
//! slots again, and a draw keeps its job and context until the tail is past
//! its last slot (`hold`, `reclaim`). The display composes each frame from
//! board memory as it stands, so nobody tracks the rows the stores wrote.

use super::backend::{DestMem, BackendState, ClearDraw};
use super::raster::{triangle_quads, RasterState};
use super::super::gl::{DrawFrame, GlDrawJob};
use super::setup::{LiveLine, Tri};
use super::super::raster_unit::Completion;
use crate::tile_pool::{copies, Rect, Tile, TilePool, GRID_SPAN};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;

/// The pool threads' name, numbered.
pub(crate) const ODYSSEY_POOL_THREAD: &str = "odyssey-pool";

/// Slots in the ring.
const RING: usize = 1 << 14;
/// A slot's command.
/// A triangle: its record in ptr, its bounding box in plane pixels in x, y
/// and zq[0], zq[1] (the far corner).
pub(crate) const KIND_TRI: u8 = 0;
/// A quad shaded by the raster thread: colours in data, back-end state in ptr.
pub(crate) const KIND_SHADED: u8 = 1;
/// A clear's rectangle: x, y to zq[0], zq[1] in plane pixels, the clear's
/// record (`backend::ClearDraw`) in ptr.
pub(crate) const KIND_RECT: u8 = 3;
/// An aliased line: its record (`setup::LiveLine`) in ptr, its box as a
/// triangle's.
pub(crate) const KIND_LINE: u8 = 4;

/// A draw's back-end state, as the ring's slots name it.
#[repr(C, align(64))]
pub(crate) struct BackendDraw {
    pub state: BackendState,
    pub planes: DestMem,
}

/// Up to four shaded fragments of one quad, as the raster thread shades
/// them itself: pixel i is plane pixel (x + (i & 1), y + (i >> 1)) where
/// `cov` bit i is set, with its depth word (polygon offset applied) and
/// 12-bit colour components.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub(crate) struct BackQuad {
    pub draw: *const BackendDraw,
    pub x: i32,
    pub y: i32,
    pub zq: [u32; 4],
    pub rgba: [[u16; 4]; 4],
    pub cov: u8,
}

impl Default for BackQuad {
    fn default() -> Self {
        BackQuad { draw: std::ptr::null(), x: 0, y: 0, zq: [0; 4], rgba: [[0; 4]; 4], cov: 0 }
    }
}

impl BackQuad {
}

/// One slot: a quad on its way through the stages, one cache line.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub(crate) struct RingQuad {
    /// Plane pixel of the quad's first pixel.
    pub x: i16,
    pub y: i16,
    pub cov: u8,
    pub kind: u8,
    _pad: u16,
    /// The depth words, polygon offset applied.
    pub zq: [u32; 4],
    /// Before shading, the four pixels' barycentrics l1 then l2 as f32
    /// bits; after, each pixel's colour as four 12-bit components in u16s
    /// (`colour`, `set_colour`).
    pub data: [u32; 8],
    /// The triangle's record (KIND_SHADE, which leads to the draw's context
    /// and back-end state), the draw's back-end state (KIND_SHADED) or the
    /// clear's record (KIND_SPAN).
    pub ptr: *const (),
}

const _: () = assert!(std::mem::size_of::<RingQuad>() == 64);

// SAFETY: `ptr` names a record the draw keeps alive until the tail is past the
// slot (`QuadRing::hold`), and the workers only read through it.
unsafe impl Send for RingQuad {}

impl RingQuad {
    /// Pixel i's 12-bit colour components.
    #[inline(always)]
    pub fn colour(&self, i: usize) -> [u32; 4] {
        let (a, b) = (self.data[2 * i], self.data[2 * i + 1]);

        [a & 0xffff, a >> 16, b & 0xffff, b >> 16]
    }

    #[inline(always)]
    pub fn set_colour(&mut self, i: usize, c: [u16; 4]) {
        self.data[2 * i] = c[0] as u32 | (c[1] as u32) << 16;
        self.data[2 * i + 1] = c[2] as u32 | (c[3] as u32) << 16;
    }
}

impl Default for RingQuad {
    fn default() -> Self {
        RingQuad {
            x: 0,
            y: 0,
            cov: 0,
            kind: KIND_SHADED,
            _pad: 0,
            zq: [0; 4],
            data: [0; 8],
            ptr: std::ptr::null(),
        }
    }
}

/// A draw kept until the tail passes `end`: its job and context.
type Held = (u64, Box<GlDrawJob>, Box<DrawFrame>);

/// The pool's closure: a plain function, so the ring's type names it.
type RunSlot = fn(&RingQuad, Tile);

pub(crate) struct QuadRing {
    /// The slots, tile queues and workers.
    tiles: Arc<TilePool<RingQuad, RunSlot>>,
    /// Workers shade filled triangles; without it the raster thread shades
    /// every quad itself.
    shader: bool,
    held: Mutex<VecDeque<Held>>,
    /// Clears' records, kept as the draws are.
    held_clears: Mutex<VecDeque<(u64, Box<ClearDraw>)>>,
    /// Completions waiting for the tail to pass every slot written before
    /// them, in order, and how many.
    completions: Mutex<VecDeque<(u64, Completion)>>,
    completing: AtomicU32,
}

impl QuadRing {
    /// Start the pool of `workers` threads on tiles `tile` pixels square (a
    /// power of two, 8 to 256); with `shader` the workers shade filled
    /// triangles.
    pub fn start(shader: bool, workers: usize, tile: u32) -> Arc<QuadRing> {
        Arc::new(QuadRing {
            tiles: TilePool::new(ODYSSEY_POOL_THREAD, RING, tile, workers, run_slot as RunSlot),
            shader,
            held: Mutex::new(VecDeque::new()),
            held_clears: Mutex::new(VecDeque::new()),
            completions: Mutex::new(VecDeque::new()),
            completing: AtomicU32::new(0),
        })
    }

    /// Whether workers shade filled triangles.
    pub fn shader(&self) -> bool {
        self.shader
    }

    // The raster thread's side.

    /// Retire the slots every tile has run, in order, and run the
    /// completions they free.
    fn retire(&self) {
        let t = self.tiles.retire();

        if self.completing.load(Relaxed) != 0 {
            self.run_completions(t);
        }
    }

    /// Run `job` once every slot written so far is stored: now, when the
    /// ring is idle.
    pub fn complete(&self, job: Completion) {
        self.completions.lock().push_back((self.tiles.staged(), job));
        self.completing.fetch_add(1, Relaxed);
        self.retire();
    }

    /// Whether completions wait for the tail.
    pub fn completing(&self) -> bool {
        self.completing.load(Relaxed) != 0
    }

    /// Run the completions the tail is past, in order, with no lock held.
    fn run_completions(&self, tail: u64) {
        loop {
            let mut c = self.completions.lock();
            if !c.front().is_some_and(|(end, _)| *end <= tail) {
                return;
            }
            let (_, job) = c.pop_front().expect("a completion");
            drop(c);
            self.completing.fetch_sub(1, Relaxed);
            job();
        }
    }

    /// Write a triangle for the tiles to walk: its record as its draw keeps
    /// it, and its bounding box in plane pixels.
    #[inline]
    pub fn push_tri(&self, tri: *const Tri, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.tiles.push(box_job(KIND_TRI, tri.cast(), x0, y0, x1, y1), Rect { x0, y0, x1, y1 });
    }

    /// Write an aliased line for the tiles to walk: its record as its draw
    /// keeps it, and its box in plane pixels.
    #[inline]
    pub fn push_line(&self, line: *const LiveLine, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.tiles.push(box_job(KIND_LINE, line.cast(), x0, y0, x1, y1), Rect { x0, y0, x1, y1 });
    }

    /// Write a quad the raster thread shaded itself.
    #[inline]
    pub fn push_shaded(&self, q: &BackQuad) {
        let mut s = RingQuad::default();

        s.x = q.x as i16;
        s.y = q.y as i16;
        s.cov = q.cov;
        s.kind = KIND_SHADED;
        s.zq = q.zq;
        for i in 0..4 {
            s.set_colour(i, q.rgba[i]);
        }
        s.ptr = q.draw.cast();
        // The pixels it covers, which may straddle a tile edge.
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for i in 0..4 {
            if q.cov & (1 << i) != 0 {
                let (x, y) = (q.x + (i & 1) as i32, q.y + (i >> 1) as i32);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
        self.tiles.push(s, Rect { x0, y0, x1, y1 });
    }

    /// Write a clear's rectangle, (x0, y0)-(x1, y1) inclusive in plane
    /// pixels.
    #[inline]
    pub fn push_rect(&self, x0: i32, y0: i32, x1: i32, y1: i32, clear: *const ClearDraw) {
        self.tiles.push(box_job(KIND_RECT, clear.cast(), x0, y0, x1, y1), Rect { x0, y0, x1, y1 });
    }

    /// Keep a clear's record until the tail is past every slot written so
    /// far.
    pub fn hold_clear(&self, clear: Box<ClearDraw>) {
        self.held_clears.lock().push_back((self.tiles.staged(), clear));
    }

    /// Keep a draw's job and context until the tail is past every slot
    /// written so far.
    pub fn hold(&self, job: Box<GlDrawJob>, frame: Box<DrawFrame>) {
        self.held.lock().push_back((self.tiles.staged(), job, frame));
    }

    /// The jobs of the held draws the tail is past, to give back, and
    /// their contexts, for reuse.
    pub fn reclaim(&self) -> (Vec<Box<GlDrawJob>>, Vec<Box<DrawFrame>>) {
        self.retire();
        let tail = self.tiles.retire();
        let mut h = self.held.lock();
        let mut out = Vec::new();
        let mut frames = Vec::new();

        while h.front().is_some_and(|(end, _, _)| *end <= tail) {
            let (_, job, frame) = h.pop_front().expect("a held draw");

            out.push(job);
            frames.push(frame);
        }
        drop(h);
        let mut c = self.held_clears.lock();
        while c.front().is_some_and(|(end, _)| *end <= tail) {
            c.pop_front();
        }
        (out, frames)
    }

    /// Whether every slot written has run on all its tiles.
    pub fn idle(&self) -> bool {
        let idle = self.tiles.idle();

        self.retire();
        idle
    }

    /// Wait until every slot written has run, so board memory is the raster
    /// thread's again; it runs tiles meanwhile.
    pub fn drain(&self) {
        self.tiles.drain();
        self.retire();
    }

    /// Stop the pool once everything has run; the held draws go.
    pub fn stop(&self) {
        self.tiles.stop();
        self.retire();
        self.held.lock().clear();
        self.held_clears.lock().clear();
    }
}

/// A command's slot: its kind and record, its box in x, y and zq[0], zq[1].
#[inline(always)]
fn box_job(kind: u8, ptr: *const (), x0: i32, y0: i32, x1: i32, y1: i32) -> RingQuad {
    let mut s = RingQuad { x: x0 as i16, y: y0 as i16, kind, ptr, ..RingQuad::default() };

    s.zq[0] = x1 as u32;
    s.zq[1] = y1 as u32;
    s
}

/// Run slot `s` within `tile`. A rectangle's tiles wrap onto the grid, so the
/// command's own pixels decide which copy of the tile it means.
fn run_slot(s: &RingQuad, tile: Tile) {
    let (px, py, size) = (tile.x, tile.y, tile.size);
    match s.kind {
        KIND_SHADED => {
            // SAFETY: the draw outlives its slots (QuadRing::hold).
            let d = unsafe { &*s.ptr.cast::<BackendDraw>() };
            // The quad's pixels in this tile.
            let mut q = BackQuad { draw: d, x: s.x as i32, y: s.y as i32, zq: s.zq, cov: 0, ..BackQuad::default() };
            for i in 0..4usize {
                let (x, y) = (q.x + (i & 1) as i32, q.y + (i >> 1) as i32);
                if s.cov & (1 << i) != 0 && (x & !(size - 1) & (GRID_SPAN - 1)) == px && (y & !(size - 1) & (GRID_SPAN - 1)) == py {
                    q.cov |= 1 << i;
                    q.rgba[i] = s.colour(i).map(|c| c as u16);
                }
            }
            (d.state.store)(&d.state, &d.planes, &q);
        }
        KIND_RECT => {
            // SAFETY: the clear outlives its slots (QuadRing::hold_clear).
            let c = unsafe { &*s.ptr.cast::<ClearDraw>() };
            let (x0, y0, x1, y1) = (s.x as i32, s.y as i32, s.zq[0] as i32, s.zq[1] as i32);
            for (tx, ty) in copies(Rect { x0, y0, x1, y1 }, tile) {
                for y in y0.max(ty)..=y1.min(ty + size - 1) {
                    c.span(y, x0.max(tx), x1.min(tx + size - 1));
                }
            }
        }
        KIND_LINE => {
            // SAFETY: the draw, which keeps the line's record, outlives
            // its slots (QuadRing::hold).
            let l = unsafe { &*s.ptr.cast::<LiveLine>() };
            // SAFETY: as for a triangle below.
            let d = unsafe { &*l.tri.live }.shader_draw();
            let p = d.pipe;
            let (x0, y0, x1, y1) = (s.x as i32, s.y as i32, s.zq[0] as i32, s.zq[1] as i32);
            for (tx, ty) in copies(Rect { x0, y0, x1, y1 }, tile) {
                let (ox, oy) = (p.raster.ox, p.raster.oy);
                let win = (tx.wrapping_sub(ox), ty.wrapping_sub(oy), (tx + size - 1).wrapping_sub(ox), (ty + size - 1).wrapping_sub(oy));
                d.line_pixels(l, win, |q| (p.backend.store)(&p.backend, &p.planes, &q));
            }
        }
        _ => {
            // SAFETY: the draw, which keeps the triangle's record,
            // outlives its slots (QuadRing::hold).
            let t = unsafe { &*s.ptr.cast::<Tri>() };
            // SAFETY: as above. A fresh copy of the draw's context each
            // time: a cached one could be a freed draw's whose address
            // a later draw took.
            let d = unsafe { &*t.live }.shader_draw();
            let p = d.pipe;
            let rst = RasterState { offset_steps: p.fill_steps, offset_fill: true, ..p.raster };
            let Some((bx0, by0, bx1, by1)) = d.bounds else {
                return;
            };
            let (x0, y0, x1, y1) = (s.x as i32, s.y as i32, s.zq[0] as i32, s.zq[1] as i32);
            for (tx, ty) in copies(Rect { x0, y0, x1, y1 }, tile) {
                // The tile in window pixels, within the draw's bounds.
                let (wx0, wy0) = (tx.wrapping_sub(rst.ox).max(bx0), ty.wrapping_sub(rst.oy).max(by0));
                let (wx1, wy1) = ((tx + size - 1).wrapping_sub(rst.ox).min(bx1), (ty + size - 1).wrapping_sub(rst.oy).min(by1));
                triangle_quads(&rst, &t.tri, Some((wx0, wy0, wx1, wy1)), |q| {
                    let mut out = BackQuad { x: rst.ox.wrapping_add(q.x), y: rst.oy.wrapping_add(q.y), ..BackQuad::default() };
                    d.shade_quad(t, q, &mut out);
                    (p.backend.store)(&p.backend, &p.planes, &out);
                });
            }
        }
    }
}
