//! The seam between what draws into the pixel store and the store itself.
//!
//! A writer describes what it draws as a `StoreOp` and hands it to `Store::apply`. A reader that is
//! about to look at the words of a region calls `Store::observe` first, then reads. The store is the
//! only owner of the pixel words, so a store that keeps them somewhere else (device memory, with a
//! host copy made current by `observe`) can take the same calls.
//!
//! Today there is one store, `PixelStore`: `apply` runs the op at once on the calling thread and
//! `observe` returns at once. Three kinds of user stay on the CPU store through `Store::cpu`: the
//! tile workers that rasterise triangles, the display that composes the planes, and the checks that
//! read the words before a write to decide whether to draw it at all.

use super::PixelStore;
use crate::dev::kona::back::gl::pixels as gl_pixels;
use crate::dev::kona::back::raster::{DrawState, Rect};
use crate::dev::kona::back::x::fill::{self as x_fill, Box4, Plan};
use crate::dev::kona::back::x::pixel::{self as x_pixel, Conv, Geo, Setup};
use std::ops::Range;

/// Proof that no tile worker is running, which resetting or refilling a whole array needs. Whoever
/// has just drained the back end's tiles makes it.
#[derive(Clone, Copy)]
pub struct Drained(());

impl Drained {
    /// # Safety
    ///
    /// No tile worker is running and none starts until the op made with this token has been applied.
    pub unsafe fn assume() -> Drained {
        Drained(())
    }
}

/// One thing a writer does to the pixel words.
pub enum StoreOp<'a> {
    /// Set the pixel (`x`, `y`) of `word` to `value`; nothing if it is off the managed area.
    Plot { word: usize, x: i64, y: i64, value: u32 },
    /// Fill the box with corners (`x1`, `y1`) and (`x2`, `y2`) by the fill rule (`raster::lit`),
    /// clipped to the managed area.
    FillBox { word: usize, x1: i64, y1: i64, x2: i64, y2: i64, value: u32 },
    /// Set the pixels of a bitmap whose bits are set, most significant bit first, row-major,
    /// `width` bits a row, its row 0 at `y0` and its column 0 at `x0`.
    Bitmap { word: usize, x0: i64, y0: i64, width: u32, height: u32, bits: &'a [u32], value: u32 },
    /// An X server fill of the clipped box `b` under the request's plan.
    XFill { plan: &'a Plan<'a>, b: Box4, spans: &'a mut Vec<(i64, i64)> },
    /// An X server image (pixel write) under the plan; `words` are the rows as sent.
    XImage { plan: &'a Plan<'a>, kind: u8, conv: &'a Conv, geo: &'a Geo, words: &'a [u32], spans: &'a mut Vec<(i64, i64)>, pad: &'a mut Vec<u32> },
    /// The writing half of an X server copy area (the plan's word to itself), after the caller has
    /// checked that the words fit. `rect` is source and destination corners (sx, sy, dx, dy).
    XCopy { plan: &'a Plan<'a>, setup: &'a Setup, rect: [i64; 4], spans: &'a mut Vec<(i64, i64)>, row: &'a mut Vec<u32> },
    /// glDrawPixels of stencil: the bits `mask` of the stencil word at plane index `index` take
    /// `value`'s.
    StencilWrite { word: usize, index: usize, value: u8, mask: u8 },
    /// One accumulation operation over the window the draw state names, as `passes` (an operation
    /// and its value each). `colour` is the word the operation reads its source from.
    Accumulate { ds: &'a DrawState, accumulation: Range<u32>, colour: Option<usize>, passes: &'a [(u32, f32)] },
    /// glClear of the accumulation words: word `first + k` goes to `values[k]` (float bits), under
    /// ownership and the clip rectangle.
    ClearAccumulation { ds: &'a DrawState, first: u32, values: &'a [u32] },
    /// Zero every array.
    Reset(Drained),
    /// Fill the whole array of `word` from `words` (a saved store).
    Restore { word: usize, words: &'a [u32], drained: Drained },
}

/// What an applied op did, for the counters of the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Pixels an accumulation operation or clear visited.
    pub fragments: u64,
}

/// The pixel store, whatever holds the words.
pub enum Store {
    Cpu(PixelStore),
}

impl Store {
    /// A store with `tex` as its texture memory, on the CPU.
    pub fn with_texture(width: u32, height: u32, tex: super::texture::TextureStore) -> Store {
        Store::Cpu(PixelStore::with_texture(width, height, tex))
    }

    /// A store for tests.
    #[cfg(test)]
    pub fn new(width: u32, height: u32) -> Store {
        Store::Cpu(PixelStore::new(width, height))
    }

    /// The CPU store. For the tile workers, the display and the checks that read before they write,
    /// and for the allocation, flags and texture memory the seam does not carry. A store with no
    /// CPU words would not have it.
    #[inline]
    pub fn cpu(&self) -> &PixelStore {
        match self {
            Store::Cpu(s) => s,
        }
    }

    /// Make a region of the words of `word` ready to read through `cpu`. Nothing to do while the
    /// words live in host memory.
    #[inline]
    pub fn observe(&self, word: usize, r: Rect) {
        let _ = (word, r);
    }

    /// `observe` of the whole managed area of every allocated word position.
    pub fn observe_all(&self) {
        let s = self.cpu();
        let r = Rect { x0: 0, y0: 0, x1: s.width() as i32, y1: s.height() as i32 };
        s.allocated().into_iter().for_each(|w| self.observe(w, r));
    }

    /// Do `op`. Whoever hands in an op has made every queued raster job finish first (`raster::fence`).
    pub fn apply(&self, op: StoreOp<'_>) -> Applied {
        let s = self.cpu();
        let mut done = Applied::default();
        match op {
            StoreOp::Plot { word, x, y, value } => plot(s, word, x, y, value),
            StoreOp::FillBox { word, x1, y1, x2, y2, value } => fill_box(s, word, x1, y1, x2, y2, value),
            StoreOp::Bitmap { word, x0, y0, width, height, bits, value } => bitmap(s, word, x0, y0, width, height, bits, value),
            StoreOp::XFill { plan, b, spans } => x_fill::draw(plan, b, spans),
            StoreOp::XImage { plan, kind, conv, geo, words, spans, pad } => x_pixel::draw_by_op(plan, kind, conv, geo, words, spans, pad),
            StoreOp::XCopy { plan, setup, rect, spans, row } => x_pixel::copy_write(plan, setup, rect, spans, row),
            StoreOp::StencilWrite { word, index, value, mask } => {
                if let Some(a) = s.array(word) {
                    // SAFETY: the caller found `index` inside the clip rectangle, inside the screen.
                    unsafe { a.store(index, ((a.load(index) as u8 & !mask) | (value & mask)) as u32) };
                }
            }
            StoreOp::Accumulate { ds, accumulation, colour, passes } => done.fragments = gl_pixels::accumulate_cpu(self, ds, accumulation, colour, passes),
            StoreOp::ClearAccumulation { ds, first, values } => done.fragments = gl_pixels::clear_accumulation_cpu(self, ds, first, values),
            StoreOp::Reset(_) => {
                // SAFETY: the op's token says no tile worker runs.
                unsafe { s.reset() }
            }
            StoreOp::Restore { word, words, .. } => {
                s.ensure(word);
                if let Some(a) = s.array(word) {
                    // SAFETY: the array holds `entries` words, which `words` is at most.
                    words.iter().take(a.len()).enumerate().for_each(|(i, &v)| unsafe { a.store(i, v) });
                }
            }
        }
        done
    }
}

#[cfg(test)]
impl std::ops::Deref for Store {
    type Target = PixelStore;

    /// Tests read the words they drew without an `observe`; production code goes through `cpu`.
    fn deref(&self) -> &PixelStore {
        self.cpu()
    }
}

fn plot(s: &PixelStore, word: usize, x: i64, y: i64, value: u32) {
    // Allocated whether or not the point lands: a draw's set-up allocates first.
    s.ensure(word);
    let a = s.array(word).expect("allocated by the set-up");
    let (w, h) = (s.width() as i64, s.height() as i64);
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    // SAFETY: the pixel is inside the managed area, which the array covers.
    unsafe { a.store(y as usize * a.pitch() + x as usize, value) };
}

fn fill_box(s: &PixelStore, word: usize, x1: i64, y1: i64, x2: i64, y2: i64, value: u32) {
    s.ensure(word);
    let a = s.array(word).expect("allocated by the set-up");
    let (xs, ys) = (crate::dev::kona::back::raster::lit(x1, x2), crate::dev::kona::back::raster::lit(y1, y2));
    let xs = xs.start.max(0)..xs.end.min(s.width() as i64);
    let ys = ys.start.max(0)..ys.end.min(s.height() as i64);
    let pitch = a.pitch();
    for y in ys {
        let row = y as usize * pitch;
        for x in xs.clone() {
            // SAFETY: both ranges are clipped to the managed area.
            unsafe { a.store(row + x as usize, value) };
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "a bitmap is its place, its size, its bits and its colour")]
fn bitmap(s: &PixelStore, plane: usize, x0: i64, y0: i64, width: u32, height: u32, bits: &[u32], value: u32) {
    s.ensure(plane);
    let a = s.array(plane).expect("allocated by the set-up");
    let (w, h) = (s.width() as i64, s.height() as i64);
    let pitch = a.pitch();
    // The columns of the bitmap that land on the store.
    let (lo, hi) = ((-x0).max(0), (w - x0).min(width as i64));
    for r in 0..height as u64 {
        let y = y0 + r as i64;
        if y < 0 || y >= h || lo >= hi {
            continue;
        }
        let (row_bit, row_store) = (r * width as u64, y as usize * pitch);
        let (mut bit, end) = (row_bit + lo as u64, row_bit + hi as u64);
        while bit < end {
            let word = bits.get((bit / 32) as usize).copied().unwrap_or(0);
            if word == 0 {
                bit = (bit / 32 + 1) * 32;
                continue;
            }
            if word >> (31 - bit % 32) & 1 != 0 {
                // SAFETY: the column is inside `lo..hi`, clipped to the store.
                unsafe { a.store(row_store + (x0 + (bit - row_bit) as i64) as usize, value) };
            }
            bit += 1;
        }
    }
}
