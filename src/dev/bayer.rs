//! The 4x4 Bayer dither matrix the dithering boards share.

// Bayer 4x4 dither matrix packed as 16 nibbles in a u64.
// Indexed by (y&3)<<2|(x&3): threshold = (BAYER_PACKED >> (idx*4)) & 0xF.
// Table: [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5]
pub(crate) const BAYER_PACKED: u64 = 0x5D7F91B36E4CA280;

#[inline(always)]
pub(crate) fn bayer_threshold(idx: u32) -> u32 {
    ((BAYER_PACKED >> (idx * 4)) & 0xF) as u32
}

pub(crate) fn rgb24_to_rgb12(val: u32) -> u32 {
    let r = (val >> 4) & 0xF;
    let g = (val >> 12) & 0xF;
    let b = (val >> 20) & 0xF;
    (b << 8) | (g << 4) | r
}

/// Pack bayer index into bits 27:24 of color value (top byte unused by 24-bit BGR).
/// Encoding: bits[3:2] = y&3, bits[1:0] = x&3 → index = (y&3)<<2|(x&3).
/// Non-dither compress variants ignore these bits.
#[inline(always)]
pub(crate) fn bayer_pack(color: u32, x: i32, y: i32) -> u32 {
    (color & 0x00FFFFFF) | (((y as u32 & 3) << 2 | (x as u32 & 3)) << 24)
}

pub(crate) fn rgb24_to_rgb12_dither(val: u32) -> u32 {
    let bayer = bayer_threshold(val >> 24);
    let r = (val & 0xFF) as u32;
    let g = ((val >> 8) & 0xFF) as u32;
    let b = ((val >> 16) & 0xFF) as u32;
    // 12bpp: 4-4-4 BGR.
    let sr = r - (r >> 4);
    let sg = g - (g >> 4);
    let sb = b - (b >> 4);
    let mut dr = (sr >> 4) & 15;
    let mut dg = (sg >> 4) & 15;
    let mut db = (sb >> 4) & 15;
    if (sr & 0xf) > bayer { dr = (dr + 1).min(15); }
    if (sg & 0xf) > bayer { dg = (dg + 1).min(15); }
    if (sb & 0xf) > bayer { db = (db + 1).min(15); }
    (db << 8) | (dg << 4) | dr
}
