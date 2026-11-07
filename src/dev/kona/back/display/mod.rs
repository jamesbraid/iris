//! What the board shows: the tables the scan-out reads and the composition of a
//! frame from the pixel store.
//!
//! The display reads the store as it is, without waiting for drawing (a frame
//! can show a pixel being drawn; the next frame reads it again). It composes
//! the whole managed area per row from the store and the tables, choosing what
//! to do with a run of pixels once and not per pixel.
//!
//! # The default descriptor
//!
//! Every display ID uses one default descriptor until a table write replaces
//! it: the front colour buffer at level 0, one word a pixel, read as a colour
//! index into the textport's colour map, no overlay. That is what the PROM's
//! console needs. Which descriptor a display ID gets is a mode byte the model
//! does not decode yet: overlays, other colour-index windows and direct-colour
//! windows wait for that decode, and the X server's own colour map and the gamma
//! RAM are held (`tables`) but not applied to anything until it exists. That a
//! textport pixel is a colour index, resolved at scan-out and not when drawn, is
//! inferred from the textport's colour-select command taking an index.
//!
//! The design's default descriptor reads RGB; this one reads a colour index,
//! because the PROM's pixels are indices (inferred, as above). Where the design
//! says RGB for the PROM's console, this is the departure.
//!
//! Grades. The textport colour map's entries are 10-bit components the sender
//! scales up by four, narrowed to eight bits as they are stored. How a real pipe
//! orders colour map, gamma and cursor is unknown; the model does not apply
//! gamma or draw a cursor yet.

pub mod clock;

use super::store::PixelStore;

/// Entries in the textport's colour map.
pub const COLOUR_MAP_ENTRIES: usize = 256;

/// The textport's colour map: an index to a colour, held as the bytes red,
/// green, blue and a full alpha, the order the screens in the emulator use
/// (`0xAABBGGRR` read as a little-endian word).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColourMap {
    entries: [u32; COLOUR_MAP_ENTRIES],
}

impl Default for ColourMap {
    fn default() -> Self {
        ColourMap { entries: [0xff00_0000; COLOUR_MAP_ENTRIES] }
    }
}

impl ColourMap {
    /// A colour-map load: an index and three 10-bit components, each the
    /// 8-bit value scaled up by four (so the top eight bits are kept).
    pub fn load(&mut self, args: &[u32]) {
        let [index, r, g, b] = *args else { return };
        let c = |v: u32| (v >> 2) & 0xff;
        self.entries[(index & 0xff) as usize] = 0xff00_0000 | c(b) << 16 | c(g) << 8 | c(r);
    }

    pub fn get(&self, index: u32) -> u32 {
        self.entries[(index & 0xff) as usize]
    }

    pub fn reset(&mut self) {
        *self = ColourMap::default();
    }
}

/// Compose the frame the default descriptor gives into `out`, `stride` pixels
/// a row, top row first. The store's rows run up from the bottom, so row `y` of
/// the frame is the store's row `height - 1 - y`. A word position with no array
/// yet is a store of zeros: index 0.
pub fn compose_default(store: &PixelStore, word: usize, map: &ColourMap, out: &mut [u32], stride: usize) {
    let (w, h) = (store.width() as usize, store.height() as usize);
    assert!(out.len() >= stride * h.saturating_sub(1) + w, "the frame buffer is too small");
    let Some(array) = store.array(word) else {
        let black = map.get(0);
        for y in 0..h {
            out[y * stride..y * stride + w].fill(black);
        }
        return;
    };
    let pitch = array.pitch();
    for y in 0..h {
        let src = (h - 1 - y) * pitch;
        let row = &mut out[y * stride..y * stride + w];
        for (x, px) in row.iter_mut().enumerate() {
            // SAFETY: `src + x` is inside the array: `x < w <= pitch` and the row is in range.
            *px = map.entries[(unsafe { array.load(src + x) } & 0xff) as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_load_keeps_the_top_eight_bits_of_each_ten() {
        let mut m = ColourMap::default();
        m.load(&[5, 1020, 4, 0x3ff]);
        assert_eq!(m.get(5), 0xffff_01ff);
        assert_eq!(m.get(5) >> 24, 0xff, "alpha is full");
        assert_eq!(m.get(5) & 0xff, 0xff, "red 1020 is 255");
        assert_eq!((m.get(5) >> 8) & 0xff, 1, "green 4 is 1");
        assert_eq!((m.get(5) >> 16) & 0xff, 0xff, "blue 1023 is 255");
        m.load(&[0x105, 0, 0, 0]);
        assert_eq!(m.get(5) & 0xff_ffff, 0, "the index is a byte");
        m.load(&[1, 2, 3]);
        assert_eq!(m.get(1), 0xff00_0000, "a short load is ignored");
        m.reset();
        assert_eq!(m, ColourMap::default());
    }

    /// The frame is top row first: the store's bottom row is the frame's last.
    #[test]
    fn the_frame_is_composed_top_row_first_through_the_colour_map() {
        let s = PixelStore::new(4, 3);
        let mut m = ColourMap::default();
        m.load(&[1, 1020, 0, 0]); // red
        m.load(&[2, 0, 1020, 0]); // green
        s.write(0, 0, 1, 1); // bottom-left
        s.write(3, 2, 1, 2); // top-right
        let mut out = vec![0u32; 4 * 3];
        compose_default(&s, 1, &m, &mut out, 4);
        assert_eq!(out[2 * 4], m.get(1), "the store's row 0 is the frame's last row");
        assert_eq!(out[3], m.get(2), "the store's top row is the frame's first");
        assert_eq!(out[5], m.get(0));
        // A stride past the width leaves the gap alone.
        let mut wide = vec![0xdead_beefu32; 8 * 3];
        compose_default(&s, 1, &m, &mut wide, 8);
        assert_eq!(wide[4], 0xdead_beef);
        assert_eq!(wide[8 * 2], m.get(1));
        // Only the low byte of a word is an index.
        s.write(1, 1, 1, 0x1_0001);
        compose_default(&s, 1, &m, &mut out, 4);
        assert_eq!(out[4 + 1], m.get(1));
    }

    /// With no array the whole frame is index 0.
    #[test]
    fn an_unallocated_word_position_is_index_zero() {
        let s = PixelStore::new(3, 2);
        let mut m = ColourMap::default();
        m.load(&[0, 400, 800, 100]);
        let mut out = vec![0u32; 6];
        compose_default(&s, 1, &m, &mut out, 3);
        assert!(out.iter().all(|&p| p == m.get(0)));
    }
}
