//! Display composition in two steps, the way Newport's compositor works:
//!
//! 1. `Frame::snapshot`, taken by the display thread just after the vertical
//!    blank (so the guest's retrace-time updates are in): copies the planes
//!    and decodes every piece of board state into flat buffers and small
//!    tables, in display order. All board-specific decisions are made here:
//!    the VC3 window-ID runs become a window ID per pixel (main and overlay),
//!    the XMAP mode words become a descriptor per window ID, the DAC gamma
//!    becomes an 8-bit table, the cursor glyph a 2-bit-per-pixel image.
//! 2. `Frame::compose`: one pass over the pixels, cursor first, then the
//!    overlay, then the main planes through their window ID's descriptor,
//!    then gamma. Each pixel depends only on the buffers at its own position
//!    and on the small tables, so the same pass can run as a fragment shader
//!    with the buffers uploaded as textures.
//!
//! The displayed size comes from the VC3 (see `display_size`): IRIX runs
//! the same board from 1024x768 to 1600x1200, with the screen's top row at
//! framebuffer row height - 1 (the window origin X uses). W x H is the
//! whole framebuffer, of which the screen is the bottom-left corner.
//!
//! Output: stride `OUT_STRIDE`, `0xFFBBGGRR` (red in the low byte), what the
//! display thread cuts the screen's frame from.

use super::dcb::Dcb;
use super::pixmem::{Buffer, Kind};
use super::rss::{self, Rss};

pub const W: usize = rss::WIDTH;
pub const H: usize = rss::HEIGHT;
pub const OUT_STRIDE: usize = crate::disp::FB_STRIDE;
/// What the board displays before IRIX loads its VC3 tables.
pub const DEFAULT_SIZE: (usize, usize) = (1280, 1024);
/// Largest cursor glyph (64x64).
pub const CURSOR_MAX: usize = 64;

/// How a window ID's main planes display. `rgb`: the pixel is `0x00BBGGRR`;
/// otherwise its low `index_mask` bits index the colormap from `cmap_base`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MainMode {
    pub rgb: bool,
    pub cmap_base: u16,
    /// Colour index planes shown: 8 for format 1 (the X server's CI8
    /// windows draw through mask 0xFF and leave the planes above alone,
    /// so they hold whatever an earlier window wrote), 12 otherwise.
    pub index_mask: u16,
    /// Pixel pairs (see `rss::Pair`) in this layout: buffer A, or B when
    /// the window ID's BUF_SELECT bit is set.
    pub pair: Option<rss::Pair>,
}

impl MainMode {
    /// Decode an XMAP main mode word: format in bits 4:0 (4 and up RGB),
    /// colormap block in 9:5.
    pub fn of(m: u32) -> Self {
        MainMode {
            rgb: m & 0x1F >= 4,
            cmap_base: (((m >> 5) & 0x1F) * 256) as u16,
            index_mask: if m & 0x1F == 1 { 0xFF } else { 0xFFF },
            pair: pair_format(m),
        }
    }
}

/// XMAP main mode formats (bits 4:0) that display pixel pairs, traced
/// with GL's 12-bit double-buffered visuals, the kernel's swaps flipping
/// BUF_SELECT: 5 the 4:4:4 layout (RGBA 4,4,4,4: electropaint, PP1 pixel
/// type 0) and 7 the 5:5:5 one (RGBA 5,5,5,1: octahedra, pixel type 1).
/// 0x15 is GL's 24-bit double-buffered visual (two pages).
pub(super) fn pair_format(mode: u32) -> Option<rss::Pair> {
    match mode & 0x1F {
        5 => Some(rss::Pair::P444),
        7 => Some(rss::Pair::P555),
        _ => None,
    }
}

/// How a window ID's overlay planes display: off, or a non-zero 8-bit value
/// indexing the colormap from `cmap_base` (0 shows the main planes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayMode {
    pub on: bool,
    pub cmap_base: u16,
}

/// One displayed frame's worth of decoded board state. Plain data, valid
/// zeroed; build with `plain::boxed_zeroed` and fill with `snapshot`.
#[repr(C)]
pub struct Frame {
    /// Displayed size (at most W x H).
    pub width: usize,
    pub height: usize,
    /// Main planes, display order (row 0 at the top).
    pub main: [u32; W * H],
    /// Overlay planes (8 bits used), display order.
    pub overlay: [u8; W * H],
    /// Window ID of each pixel, main and overlay planes, display order.
    pub did_main: [u8; W * H],
    pub did_overlay: [u8; W * H],
    pub main_mode: [MainMode; 32],
    pub overlay_mode: [OverlayMode; 32],
    /// Colormap 0, `0x00RRGGBB`.
    pub cmap: [u32; 8192],
    /// DAC gamma reduced to 8 bits, per input level: [red, green, blue].
    pub gamma: [[u8; 3]; 256],
    /// The same table at the DAC's own ten bits (`gamma` is each value >> 2).
    pub gamma10: [[u16; 3]; 256],
    /// DAC pixel mask is zero: the screen is blanked.
    pub blank: bool,
    pub cursor_on: bool,
    /// Top-left corner on screen, and the glyph's size (32 or 64).
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_size: usize,
    /// Glyph, 2 bits a pixel (0 transparent), CURSOR_MAX x CURSOR_MAX.
    pub cursor: [u8; CURSOR_MAX * CURSOR_MAX],
    /// Colours of cursor values 1..3 (index 0 unused), `0x00RRGGBB`.
    pub cursor_rgb: [u32; 4],
}

impl Frame {
    /// Take the frame: copy the planes and decode the display state.
    pub fn snapshot(&mut self, rss: &Rss, dcb: &Dcb) {
        self.snapshot_rows(rss, dcb, 0..usize::MAX);
    }

    /// `snapshot` for the display rows `rows` alone (clamped to the screen); every table is decoded, the
    /// planes and window IDs of the other rows are left as they were.
    pub fn snapshot_rows(&mut self, rss: &Rss, dcb: &Dcb, rows: std::ops::Range<usize>) {
        (self.width, self.height) = display_size(dcb);
        let (w, h) = (self.width, self.height);
        let rows = rows.start.min(h)..rows.end.min(h);
        let (main, overlay) = scanout_buffers(rss, dcb);
        // Double buffering: window IDs with their BUF_SELECT bit show
        // buffer B. The window-ID runs are decoded below; this pass needs
        // them first, so decode them before the planes.
        let select = dcb.xmap.buf_select();
        let back = Buffer::new(dcb.xmap.dib_pointers() >> 10, Kind::Wide, rss.reg(rss::reg::MGRAS_PP1_DRB_SIZE));
        let mut runs = Vec::with_capacity(64);
        for y in rows.clone() {
            dcb.vc3.main_did_runs(y, &mut runs);
            fill_did_row(&mut self.did_main[y * W..(y + 1) * W], &mut runs);
        }
        for did in 0..32u32 {
            let m = dcb.xmap.main_mode(did);
            self.main_mode[did as usize] = MainMode::of(m);
            let o = dcb.xmap.overlay_mode(did);
            self.overlay_mode[did as usize] = OverlayMode { on: o != 0, cmap_base: (((o >> 3) & 0x1F) * 256) as u16 };
        }
        let mut row = [0u32; W];
        let mut row_b = [0u32; W];
        let select_pass = self.buffer_select_matters(select);
        for y in rows.clone() {
            let dst = y * W;
            let fb_y = (h - 1 - y) as u32;
            rss.mem.read_row(&main, fb_y, &mut self.main[dst..dst + w]);
            if select != 0 {
                rss.mem.read_row(&back, fb_y, &mut row_b[..w]);
            }
            if select_pass {
                self.select_buffers(dst, w, select, &row_b);
            }
            match overlay {
                Some(o) => {
                    rss.mem.read_row(&o, fb_y, &mut row[..w]);
                    for (d, s) in self.overlay[dst..dst + w].iter_mut().zip(&row[..w]) {
                        *d = *s as u8;
                    }
                }
                None => self.overlay[dst..dst + w].fill(0),
            }
        }
        for y in rows {
            dcb.vc3.overlay_did_runs(y, &mut runs);
            fill_did_row(&mut self.did_overlay[y * W..(y + 1) * W], &mut runs);
        }
        self.cmap.copy_from_slice(&dcb.cmap[0].pal);
        for (g, d) in self.gamma.iter_mut().zip(dcb.dac.gamma.iter()) {
            *g = [(d[0] >> 2) as u8, (d[1] >> 2) as u8, (d[2] >> 2) as u8];
        }
        self.gamma10 = dcb.dac.gamma;
        self.blank = dcb.dac.pixmask() == 0;
        self.cursor_on = false;
        if let Some((cx, cy, size, glyph)) = dcb.vc3.cursor() {
            self.cursor_on = true;
            (self.cursor_x, self.cursor_y, self.cursor_size) = (cx, cy, size);
            let sram = &dcb.vc3.sram;
            let words_per_row = size / 16;
            let plane_words = size * words_per_row;
            let bit = |plane: usize, row: usize, col: usize| -> u8 {
                let w = sram[(glyph + plane * plane_words + row * words_per_row + col / 16) & 0x7FFF];
                (w >> (15 - col % 16)) as u8 & 1
            };
            for row in 0..size {
                for col in 0..size {
                    self.cursor[row * CURSOR_MAX + col] = bit(0, row, col) | bit(1, row, col) << 1;
                }
            }
            // Cursor colours sit in the colormap at the XMAP's cursor base;
            // past its end they read as white.
            let base = dcb.xmap.cursor_cmap_base();
            for c in 1..4 {
                self.cursor_rgb[c] = self.cmap.get(base + c).copied().unwrap_or(0xFF_FFFF);
            }
        }
    }

    /// Whether `select_buffers` can change a pixel: some window ID shows buffer B, or a mode takes pixel pairs.
    fn buffer_select_matters(&self, select: u32) -> bool {
        !super::gl::fast_paths() || select != 0 || self.main_mode.iter().any(|m| m.pair.is_some())
    }

    /// The pixels of the row at `dst` as their window IDs show them: a pair's half, or buffer B (`row_b`).
    fn select_buffers(&mut self, dst: usize, w: usize, select: u32, row_b: &[u32]) {
        for x in 0..w {
            let did = self.did_main[dst + x] & 31;
            let b = select >> did & 1 != 0;
            let mode = self.main_mode[did as usize];
            if let Some(pair) = mode.pair {
                self.main[dst + x] = pair.unpack(self.main[dst + x], b) & 0xFF_FFFF;
            } else if b && !mode.rgb {
                // Colour index: B is the word's high 12 bits
                // (`rss::ci_pixtype`).
                self.main[dst + x] = (self.main[dst + x] >> 12) & 0xFFF;
            } else if b {
                self.main[dst + x] = row_b[x];
            }
        }
    }

    /// One pixel, `0xFFBBGGRR`. `x`, `y` in display coordinates.
    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> u32 {
        match self.levels(x, y) {
            None => 0xFF00_0000,
            Some([r, g, b]) => self.gamma_rgb(r, g, b),
        }
    }

    /// One pixel with the gamma's ten bits a channel, `x2b10g10r10` (red in the low ten bits, bits 31:30
    /// zero); its top 8 bits of each channel are `pixel`'s.
    #[inline]
    pub fn pixel10(&self, x: usize, y: usize) -> u32 {
        match self.levels(x, y) {
            None => 0,
            Some([r, g, b]) => {
                let lvl = |v: u32, comp: usize| self.gamma10[(v & 0xFF) as usize][comp] as u32;
                lvl(b, 2) << 20 | lvl(g, 1) << 10 | lvl(r, 0)
            }
        }
    }

    /// The three input levels (red, green, blue; the colormap's entry or the RGB planes' channels) the DAC's gamma
    /// looks up for a pixel: None while the screen is blanked.
    #[inline(always)]
    fn levels(&self, x: usize, y: usize) -> Option<[u32; 3]> {
        if self.blank {
            return None;
        }
        let i = y * W + x;
        let rgb = self.cursor_value(x, y).map(|c| self.cursor_rgb[c as usize])
            .or_else(|| {
                let o = self.overlay_mode[self.did_overlay[i] as usize & 31];
                let v = self.overlay[i];
                (o.on && v != 0).then(|| self.cmap[(o.cmap_base as usize + v as usize) & 0x1FFF])
            });
        Some(match rgb {
            Some(c) => [c >> 16, c >> 8, c],
            None => {
                let m = self.main_mode[self.did_main[i] as usize & 31];
                let v = self.main[i];
                if m.rgb {
                    [v, v >> 8, v >> 16]
                } else {
                    let c = self.cmap[(m.cmap_base as usize + (v & m.index_mask as u32) as usize) & 0x1FFF];
                    [c >> 16, c >> 8, c]
                }
            }
        })
    }

    /// The cursor's 2-bit value at a screen position, if the cursor covers
    /// it with a non-transparent pixel.
    #[inline]
    fn cursor_value(&self, x: usize, y: usize) -> Option<u8> {
        if !self.cursor_on {
            return None;
        }
        let (cx, cy) = (x as i32 - self.cursor_x, y as i32 - self.cursor_y);
        let size = self.cursor_size as i32;
        if !(0..size).contains(&cx) || !(0..size).contains(&cy) {
            return None;
        }
        let c = self.cursor[cy as usize * CURSOR_MAX + cx as usize];
        (c != 0).then_some(c)
    }

    #[inline]
    fn gamma_rgb(&self, r: u32, g: u32, b: u32) -> u32 {
        let lvl = |v: u32, comp: usize| self.gamma[(v & 0xFF) as usize][comp] as u32;
        0xFF00_0000 | lvl(b, 2) << 16 | lvl(g, 1) << 8 | lvl(r, 0)
    }

    /// The whole frame into `out` (stride `OUT_STRIDE`, `height` rows of
    /// `width` pixels).
    pub fn compose(&self, out: &mut [u32]) {
        for y in 0..self.height {
            let row = &mut out[y * OUT_STRIDE..y * OUT_STRIDE + self.width];
            for (x, o) in row.iter_mut().enumerate() {
                *o = self.pixel(x, y);
            }
        }
    }
}

/// The buffers scanout reads: the XMAP DIB pointers' main buffer and, when
/// its pointer differs, the overlay.
pub fn scanout_buffers(rss: &Rss, dcb: &Dcb) -> (Buffer, Option<Buffer>) {
    let dib = dcb.xmap.dib_pointers();
    let drbsize = rss.reg(rss::reg::MGRAS_PP1_DRB_SIZE);
    let main = Buffer::new(dib, Kind::Wide, drbsize);
    let overlay = Buffer::new(dib >> 20, Kind::Overlay, drbsize);
    (main, (overlay.ptr != main.ptr).then_some(overlay))
}

/// The displayed size the VC3 is programmed for. Height: the scanlines in
/// the main window-ID frame table (one entry per visible line). Width: by
/// height, every IMPACT video format having its own (1280x492 stereo,
/// 1280x960 and 1920x1035 HDTV included). Only for a height not in the list,
/// the timing tables decoded with the VC2 layout, snapped to a standard
/// width; that decode is loose on the VC3 (978 for 1024, 1260 or 1680 for
/// 1280), its blanking bits not being pinned down. Before IRIX loads its
/// tables: `DEFAULT_SIZE`.
pub fn display_size(dcb: &Dcb) -> (usize, usize) {
    const WIDTHS: [usize; 7] = [640, 800, 1024, 1152, 1280, 1600, 1920];
    let lines = dcb.vc3.did_lines();
    let Some(h) = (1..=H.min(crate::disp::FB_MAX_H)).contains(&lines).then_some(lines) else {
        return DEFAULT_SIZE;
    };
    let w = match h {
        480 => 640,
        492 | 960 | 1024 => 1280,
        576 => 768,
        600 => 800,
        768 => 1024,
        864 => 1152,
        1035 => 1920,
        1200 => 1600,
        _ => match dcb.vc3.timing_size() {
            Some((tw, _)) => *WIDTHS.iter().min_by_key(|&&s| s.abs_diff(tw)).unwrap(),
            None => DEFAULT_SIZE.0,
        },
    };
    (w.min(W), h)
}

/// Window IDs of one display row from its VC3 runs `(first x, did)`. A row
/// with no run at x = 0 starts in window ID 0; each run lasts until the
/// next one's x (clamped to the screen); runs out of order overwrite the
/// pixels earlier ones set.
fn fill_did_row(row: &mut [u8], runs: &mut Vec<(u16, u8)>) {
    row.fill(0);
    if runs.is_empty() || runs[0].0 != 0 {
        runs.insert(0, (0, 0));
    }
    for (k, &(x0, did)) in runs.iter().enumerate() {
        let x0 = (x0 as usize).min(W);
        let x1 = runs.get(k + 1).map_or(W, |r| (r.0 as usize).min(W));
        if x1 > x0 {
            row[x0..x1].fill(did);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::plain::boxed_zeroed;
    use super::*;

    struct Rnd(u64);

    impl Rnd {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) % n
        }
    }

    /// Random state for a `w` x `h` picture: window-ID runs (some IDs past 31), colour index and RGB modes,
    /// overlay rows that are empty or not, a cursor that may cover the rows, a blanked screen now and then.
    fn random_frame(f: &mut Frame, r: &mut Rnd, w: usize, h: usize) {
        (f.width, f.height) = (w, h);
        f.gamma.iter_mut().for_each(|g| *g = [r.below(256) as u8, r.below(256) as u8, r.below(256) as u8]);
        f.cmap.iter_mut().for_each(|c| *c = r.below(1 << 24) as u32);
        for m in f.main_mode.iter_mut() {
            *m = MainMode { rgb: r.below(2) == 0, cmap_base: r.below(0x2000) as u16, index_mask: [0xFF, 0xFFF][r.below(2) as usize], pair: None };
        }
        for o in f.overlay_mode.iter_mut() {
            *o = OverlayMode { on: r.below(3) != 0, cmap_base: r.below(0x2000) as u16 };
        }
        for y in 0..h {
            let (mut x, row) = (0, y * W);
            while x < w {
                let (run, did) = (1 + r.below(40) as usize, r.below(48) as u8);
                (x..(x + run).min(w)).for_each(|i| f.did_main[row + i] = did);
                x += run;
            }
            let overlaid = r.below(3) == 0;
            for x in 0..w {
                f.main[row + x] = r.below(1 << 32) as u32;
                f.did_overlay[row + x] = r.below(40) as u8;
                f.overlay[row + x] = if overlaid && r.below(6) == 0 { 1 + r.below(255) as u8 } else { 0 };
            }
        }
        f.blank = r.below(8) == 0;
        f.cursor_on = r.below(2) == 0;
        (f.cursor_x, f.cursor_y, f.cursor_size) = (r.below(w as u64 + 20) as i32 - 10, r.below(h as u64 + 10) as i32 - 5, [32, 64][r.below(2) as usize]);
        f.cursor.iter_mut().for_each(|c| *c = r.below(4) as u8);
        f.cursor_rgb = [0, r.below(1 << 24) as u32, r.below(1 << 24) as u32, r.below(1 << 24) as u32];
    }

    /// The buffer-select pass is a no-op when it says it does not matter: with no window ID showing buffer B and
    /// no pixel-pair mode, it leaves every pixel as it is; with either, it changes some.
    #[test]
    fn buffer_select_pass_is_skipped_only_when_it_does_nothing() {
        let mut f = boxed_zeroed::<Frame>();
        let mut r = Rnd(0x0F0F_1234_ABCD_9876);
        let (mut skipped, mut changed) = (0, 0);
        for trial in 0..200 {
            let (w, h) = (1 + r.below(100) as usize, 1 + r.below(8) as usize);
            random_frame(&mut f, &mut r, w, h);
            let select = [0, 0, 0, r.below(1 << 32) as u32][r.below(4) as usize];
            if r.below(3) == 0 {
                let k = r.below(32) as usize;
                f.main_mode[k].pair = Some(rss::Pair::P444);
            }
            let row_b: Vec<u32> = (0..w).map(|_| r.below(1 << 32) as u32).collect();
            let before = f.main[..h * W].to_vec();
            for y in 0..h {
                f.select_buffers(y * W, w, select, &row_b);
            }
            let same = (0..h).all(|y| f.main[y * W..y * W + w] == before[y * W..y * W + w]);
            if f.buffer_select_matters(select) {
                changed += !same as usize;
            } else {
                assert!(same, "trial {trial}: skipped, yet it changes pixels");
                skipped += 1;
            }
        }
        assert!(skipped > 40 && changed > 40, "reach: {skipped} skipped, {changed} changed");
    }

    /// The snapshot as a window ID's BUF_SELECT bit or a pixel-pair mode shows the planes: buffer B for a selected
    /// window ID, a pair's half for a pair mode, and the plain pixel of buffer A for neither.
    #[test]
    fn snapshot_applies_the_buffer_select_pass() {
        use super::super::dcb::Dcb;
        use super::super::pixmem::{Buffer, Kind};
        let mut rss = Rss::new_boxed();
        let drb = rss.reg(rss::reg::MGRAS_PP1_DRB_SIZE);
        // Display row 3 is framebuffer row 1020 of the 1280x1024 screen the tables below name.
        let (a, b) = (Buffer::new(0x240, Kind::Wide, drb), Buffer::new(0x241, Kind::Wide, drb));
        rss.mem.put(&a, 5, 1020, 0x11_2233);
        rss.mem.put(&b, 5, 1020, 0x44_5566);
        let mut f = boxed_zeroed::<Frame>();
        let mut shown = |did: u16, mode: u32, select: u32| {
            let mut dcb = boxed_zeroed::<Dcb>();
            dcb.xmap.set_modes(did as u32, mode, 0);
            dcb.xmap.set_scanout(select, 0x240 | 0x241 << 10, 0);
            dcb.vc3.regs[4] = 0;
            (0..1024).for_each(|y| dcb.vc3.sram[y] = 0x4000);
            (dcb.vc3.sram[1024], dcb.vc3.sram[0x4000], dcb.vc3.sram[0x4001]) = (0xFFFF, did, 0x7FF << 5);
            f.snapshot(&rss, &dcb);
            assert_eq!((f.width, f.height, f.did_main[3 * W + 5]), (1280, 1024, did as u8));
            f.main[3 * W + 5]
        };
        assert_eq!(shown(1, 4, 0), 0x11_2233, "buffer A");
        assert_eq!(shown(1, 4, 1 << 1), 0x44_5566, "buffer B for window ID 1");
        assert_eq!(shown(1, 4, 1 << 2), 0x11_2233, "window ID 2 selected, not 1");
        assert_eq!(shown(0, 4, 1), 0x44_5566, "buffer B for window ID 0");
        assert_eq!(shown(17, 4, 1 << 17), 0x44_5566, "buffer B for window ID 17, past the low half of the bits");
        assert_eq!(shown(17, 4, 1 << 1), 0x11_2233, "window ID 1 selected, not 17");
        assert_eq!(shown(1, 5, 0), rss::Pair::P444.unpack(0x11_2233, false) & 0xFF_FFFF, "a 4:4:4 pair");
        assert_ne!(rss::Pair::P444.unpack(0x11_2233, false) & 0xFF_FFFF, 0x11_2233);
    }
}
