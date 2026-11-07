//! Drawing into the pixel store.
//!
//! The integer primitives the textport and the X server use: points, boxes
//! and bitmaps written into one word position of the store, clipped to the
//! managed area. The store is bottom-origin, y up, and the textport's
//! coordinates are the board's own, written as given. The X server's run down
//! from the top of the screen, so its fills mirror the row (`xfill`); both were
//! settled by comparing the store rendered from the recordings with their
//! screen captures. The display then flips the rows once.
//!
//! # The fill rule
//!
//! For a box with corners `x1 < x2`, `y1 < y2` the board lights columns
//! `x1 + 1` to `x2` and rows `y1 + 1` to `y2`: the left column and bottom row of
//! a whole-pixel box are left out (established per the hardware documents for boxes sent
//! as four unchanged corners; the grade of that rests on a ruling still to
//! come). `FillRule` makes the rule a parameter so the ruling changes a value
//! and not the code: only its grade depends on it.

use super::store::{PixelStore, WordArray};

/// Which edges of a whole-pixel box light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    /// The left column and bottom row are left out; the right column and top
    /// row are lit. The rule the hardware documents give for the board.
    OmitLeftBottom,
    /// Every edge lights: columns `x1` to `x2`, rows `y1` to `y2`.
    AllEdges,
}

impl FillRule {
    /// The coordinates a box from `lo` to `hi` lights along one axis, as a
    /// half-open range. Empty when `hi < lo`.
    pub fn lit(self, lo: i64, hi: i64) -> std::ops::Range<i64> {
        match self {
            FillRule::OmitLeftBottom => lo + 1..hi + 1,
            FillRule::AllEdges => lo..hi + 1,
        }
    }
}

/// A word position of the store to draw into, and where drawing stops.
pub struct Surface<'a> {
    store: &'a PixelStore,
    array: WordArray<'a>,
}

impl<'a> Surface<'a> {
    /// Draw into word `word` of the store's pixels. Allocates the array: this
    /// is the draw's set-up, and resolves it once for every call after.
    pub fn new(store: &'a PixelStore, word: usize) -> Surface<'a> {
        store.ensure(word);
        Surface { store, array: store.array(word).expect("allocated by the set-up") }
    }

    /// The managed area's size.
    pub fn size(&self) -> (u32, u32) {
        (self.store.width(), self.store.height())
    }

    /// The word array drawn into.
    pub fn array(&self) -> WordArray<'a> {
        self.array
    }

    /// Set the pixel (`x`, `y`) to `value`; false if it is off the managed area.
    pub fn plot(&self, x: i64, y: i64, value: u32) -> bool {
        let (w, h) = self.size();
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return false;
        }
        // SAFETY: the pixel is inside the managed area, which the array covers.
        unsafe { self.array.store(y as usize * self.array.pitch() + x as usize, value) };
        true
    }

    /// Fill the box with corners (`x1`, `y1`) and (`x2`, `y2`) by `rule`,
    /// clipped to the managed area. A box whose far corner is below or left of
    /// its near one draws nothing (what a reversed box does is unknown).
    pub fn fill_box(&self, rule: FillRule, x1: i64, y1: i64, x2: i64, y2: i64, value: u32) {
        let (xs, ys) = (rule.lit(x1, x2), rule.lit(y1, y2));
        let xs = xs.start.max(0)..xs.end.min(self.store.width() as i64);
        let ys = ys.start.max(0)..ys.end.min(self.store.height() as i64);
        let pitch = self.array.pitch();
        for y in ys {
            let row = y as usize * pitch;
            for x in xs.clone() {
                // SAFETY: both ranges are clipped to the managed area.
                unsafe { self.array.store(row + x as usize, value) };
            }
        }
    }

    /// Set the pixels of a bitmap whose bits are set, most significant bit
    /// first, row-major, `width` bits a row, with its row 0 at `y0` and its
    /// column 0 at `x0`. Rows and columns off the managed area are skipped
    /// without being looked at, and a clear word is skipped whole.
    pub fn bitmap(&self, x0: i64, y0: i64, width: u32, height: u32, bits: &[u32], value: u32) {
        let (w, h) = (self.store.width() as i64, self.store.height() as i64);
        let pitch = self.array.pitch();
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
                    unsafe { self.array.store(row_store + (x0 + (bit - row_bit) as i64) as usize, value) };
                }
                bit += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &PixelStore, w: usize) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        for y in 0..s.height() {
            for x in 0..s.width() {
                if s.read(x, y, w) != 0 {
                    v.push((x, y));
                }
            }
        }
        v
    }

    /// The four-corner packet of the hardware documents: x1 < x2, y1 < y2, no
    /// fraction, lights columns x1+1..=x2 and rows y1+1..=y2.
    #[test]
    fn a_box_lights_columns_and_rows_after_the_near_corner() {
        let s = PixelStore::new(16, 16);
        Surface::new(&s, 1).fill_box(FillRule::OmitLeftBottom, 2, 3, 5, 6, 7);
        let got = lit(&s, 1);
        assert_eq!(got.len(), 3 * 3);
        assert!(got.iter().all(|&(x, y)| (3..=5).contains(&x) && (4..=6).contains(&y)));
        assert_eq!(s.read(2, 4, 1), 0, "the left column is out");
        assert_eq!(s.read(3, 3, 1), 0, "the bottom row is out");
        assert_eq!(s.read(5, 6, 1), 7, "the right column and top row are in");
    }

    /// Adjacent boxes that share an edge meet with neither a gap nor an
    /// overlap, which is why the rule leaves the near edge out.
    #[test]
    fn boxes_sharing_an_edge_neither_gap_nor_overlap() {
        let s = PixelStore::new(8, 12);
        let d = Surface::new(&s, 0);
        d.fill_box(FillRule::OmitLeftBottom, 0, 0, 7, 5, 1);
        d.fill_box(FillRule::OmitLeftBottom, 0, 5, 7, 10, 2);
        for y in 1..=10u32 {
            for x in 1..=7u32 {
                assert_eq!(s.read(x, y, 0), if y <= 5 { 1 } else { 2 }, "({x},{y})");
            }
        }
    }

    /// The rule is a parameter: with every edge lit the box is one row and one
    /// column larger.
    #[test]
    fn the_rule_is_a_parameter() {
        let s = PixelStore::new(16, 16);
        Surface::new(&s, 0).fill_box(FillRule::AllEdges, 2, 3, 5, 6, 1);
        assert_eq!(lit(&s, 0).len(), 4 * 4);
        assert_eq!(FillRule::OmitLeftBottom.lit(2, 5), 3..6);
        assert_eq!(FillRule::AllEdges.lit(2, 5), 2..6);
        assert!(FillRule::OmitLeftBottom.lit(5, 2).is_empty());
    }

    /// A box at the left or bottom edge of the screen leaves column or row 0
    /// unlit; a box past the far edges is clipped; a reversed box draws nothing.
    #[test]
    fn boxes_are_clipped_and_reversed_ones_draw_nothing() {
        let s = PixelStore::new(8, 8);
        let d = Surface::new(&s, 0);
        d.fill_box(FillRule::OmitLeftBottom, 0, 0, 100, 100, 1);
        assert_eq!(s.read(0, 4, 0), 0);
        assert_eq!(s.read(4, 0, 0), 0);
        assert_eq!(lit(&s, 0).len(), 7 * 7);
        let t = PixelStore::new(8, 8);
        Surface::new(&t, 0).fill_box(FillRule::OmitLeftBottom, 5, 5, 2, 2, 1);
        Surface::new(&t, 0).fill_box(FillRule::OmitLeftBottom, -50, -50, -10, -10, 1);
        assert!(lit(&t, 0).is_empty());
    }

    #[test]
    fn plot_refuses_off_screen_points() {
        let s = PixelStore::new(4, 4);
        let d = Surface::new(&s, 0);
        assert!(d.plot(3, 3, 9));
        assert!(!d.plot(4, 0, 9) && !d.plot(0, 4, 9) && !d.plot(-1, 0, 9) && !d.plot(i64::MAX, 0, 9));
        assert_eq!(lit(&s, 0), [(3, 3)]);
    }

    /// Bits go most significant first, row-major, and a missing word reads as
    /// clear.
    #[test]
    fn a_bitmap_sets_its_set_bits() {
        let s = PixelStore::new(16, 16);
        // 5 x 3: row 0 = 10101, row 1 = 01010, row 2 = 11111.
        let bits = [0b10101_01010_11111u32 << 17];
        Surface::new(&s, 2).bitmap(4, 8, 5, 3, &bits, 3);
        assert_eq!(lit(&s, 2).len(), 3 + 2 + 5);
        assert_eq!(s.read(4, 8, 2), 3);
        assert_eq!(s.read(5, 8, 2), 0);
        assert_eq!(s.read(5, 9, 2), 3);
        assert_eq!(s.read(8, 10, 2), 3);
        // A short payload: the missing words read as clear.
        let wide = PixelStore::new(32, 4);
        Surface::new(&wide, 4).bitmap(0, 0, 32, 2, &[u32::MAX], 1);
        assert_eq!(lit(&wide, 4).len(), 32);
    }
}
