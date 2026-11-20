//! What the scan-out makes of the display registers, decided once per frame:
//! the colour map and gamma, and which window ids show which buffer.

use super::board::Board;
use super::consts::{
    dispmode, lut, overlay, ODSY_CMAP_UNVERIFIED_GROUP, ODSY_DID_MAIN_SHIFT, ODSY_GAMMA_GROUP,
    ODSY_INDEX_MASK,
};
use super::pipe::backend::DestMem;
use super::reader::DisplayRegs;

impl Board {
    /// Pixel (x, y)'s main window id: bits 7:3 of its window-id byte in the
    /// buffer the scan-out names (PB&J 0x2800 base, 0x2807 stride).
    pub fn main_id(&mut self, x: u32, y: u32) -> u32 {
        // The id bytes are the front end's own: no wait (Board::id_compare_passes).
        let did = DestMem::of(self.vram_ptr(), &self.dest_views()).did.load1(x as i32, y as i32).unwrap_or(0);
        ((did as u32) >> ODSY_DID_MAIN_SHIFT) & (dispmode::COUNT - 1)
    }

    /// The colormap group main drawable main_id shows through: bits 8:4 of its
    /// display mode. Below ODSY_GAMMA_GROUP the word is an index.
    pub fn display_group(&mut self, main_id: u32) -> u32 {
        self.display.regs.display_group(main_id)
    }
}

/// What one main drawable id shows, decided once per update instead of per
/// pixel: which colour buffer, and how its plane word becomes a colour.
#[derive(Clone, Copy, Default)]
pub(super) struct IdShow {
    /// The displayed colour buffer, 0 or 1.
    pub buf: usize,
    /// Whether the plane word is RGB through the gamma table.
    pub rgb: bool,
    /// An RGB mode's channel fold: (c & keep) | (c >> down).
    pub keep: u32,
    pub down: u32,
    /// An index mode: lut entry = base + (word & mask).
    pub base: u32,
    pub mask: u32,
    /// The mode is colour map group 25, reported once when shown.
    pub unverified: bool,
}

/// Everything a row's resolve reads outside the planes, gathered once from a
/// copy of the registers, so none of it can change meanwhile.
pub(super) struct ResolveCtx {
    pub ids: [IdShow; dispmode::COUNT as usize],
    /// Per overlay id: whether it is enabled, and its entry base.
    pub overlay: [(bool, u32); overlay::IDS as usize],
    /// Every lookup-RAM entry as 0x00RRGGBB (DisplayRegs::lut_rgb()), or with
    /// ten bits a channel (`lut_wide()`) in a wide context.
    pub rgb: Box<[u32; lut::SIZE as usize]>,
    /// DisplayRegs::gamma() of each 8-bit channel value, placed in its field of
    /// the pixel (a byte, or ten bits in a wide context): red, green, blue.
    pub gamma: Box<[[u32; 256]; 3]>,
}

impl ResolveCtx {
    /// What the registers `regs` make of each drawable, overlay id, lookup
    /// entry and gamma step, with colour buffer (displayed >> id) & 1
    /// showing for each drawable. `wide` keeps the maps' ten bits a channel
    /// (x2r10g10b10) instead of their top eight.
    pub(super) fn new(regs: &DisplayRegs, displayed: u32, wide: bool) -> ResolveCtx {
        let mut ids = [IdShow::default(); dispmode::COUNT as usize];
        let mut overlay = [(false, 0u32); overlay::IDS as usize];
        let mut rgb = Box::new([0u32; lut::SIZE as usize]);
        let mut gamma = Box::new([[0u32; 256]; 3]);

        for (i, v) in rgb.iter_mut().enumerate() {
            *v = if wide { regs.lut_wide(i as u32) } else { regs.lut_rgb(i as u32) };
        }
        for c in 0..256u32 {
            let [r, g, b] = if wide {
                [regs.gamma_wide(c, lut::RED_SHIFT) << 20, regs.gamma_wide(c, lut::GREEN_SHIFT) << 10, regs.gamma_wide(c, lut::BLUE_SHIFT)]
            } else {
                [regs.gamma(c, lut::RED_SHIFT) << 16, regs.gamma(c, lut::GREEN_SHIFT) << 8, regs.gamma(c, lut::BLUE_SHIFT)]
            };
            (gamma[0][c as usize], gamma[1][c as usize], gamma[2][c as usize]) = (r, g, b);
        }
        for (id, show) in ids.iter_mut().enumerate() {
            let mode = regs.dispmode[id];
            let group = regs.display_group(id as u32);

            show.buf = ((displayed >> id) & 1) as usize;
            if group >= ODSY_GAMMA_GROUP {
                show.rgb = true;
                // display_channel() as a mask and a shift; a shift of 8 or
                // more leaves a byte's high part empty.
                (show.keep, show.down) = match (mode >> dispmode::WIDTH_SHIFT) & dispmode::WIDTH_MASK {
                    dispmode::WIDTH_4 => (0xf0, 4),
                    dispmode::WIDTH_5 => (0xf8, 5),
                    _ => (0xff, 8),
                };
            } else if group != 0 {
                show.base = 256 * group;
                show.mask = 0xff;
                show.unverified = group == ODSY_CMAP_UNVERIFIED_GROUP;
            } else {
                show.mask = ODSY_INDEX_MASK;
            }
        }
        for (id, o) in overlay.iter_mut().enumerate() {
            let mode = regs.overlay_mode[id];

            *o = (
                mode & overlay::MODE_ENABLE != 0,
                256 * ((mode >> overlay::MODE_GROUP_SHIFT) & overlay::MODE_GROUP_MASK),
            );
        }
        ResolveCtx { ids, overlay, rgb, gamma }
    }
}
