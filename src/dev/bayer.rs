//! The 4x4 Bayer dither matrix the dithering boards share.

// Bayer 4x4 dither matrix packed as 16 nibbles in a u64.
// Indexed by (y&3)<<2|(x&3): threshold = (BAYER_PACKED >> (idx*4)) & 0xF.
// Table: [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5]
pub(crate) const BAYER_PACKED: u64 = 0x5D7F91B36E4CA280;

#[inline(always)]
pub(crate) fn bayer_threshold(idx: u32) -> u32 {
    ((BAYER_PACKED >> (idx * 4)) & 0xF) as u32
}
