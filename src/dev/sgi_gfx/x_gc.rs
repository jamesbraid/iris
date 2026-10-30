//! Ported from include/hw/display/sgi/x-gc.h in jamesbraid/qemu-sgi-origami.
use super::*;

pub const SGI_X_GC_WORDS: usize = 13;

pub type SGIXGCTarget = u32;
pub const SGI_X_GC_UNSUPPORTED: SGIXGCTarget = 0;
pub const SGI_X_GC_MAIN_CI8: SGIXGCTarget = 1;
pub const SGI_X_GC_MAIN_INDEX16: SGIXGCTarget = 2;
pub const SGI_X_GC_MAIN_RGB8: SGIXGCTarget = 3;
pub const SGI_X_GC_OVERLAY0: SGIXGCTarget = 4;
pub const SGI_X_GC_OVERLAY1: SGIXGCTarget = 5;
pub const SGI_X_GC_POPUP: SGIXGCTarget = 6;
pub const SGI_X_GC_DID: SGIXGCTarget = 7;
/// Both overlay buffers at once, the same 8-bit index in each.
pub const SGI_X_GC_OVERLAY_BOTH: SGIXGCTarget = 8;

pub type SGIXGCStatus = u32;
pub const SGI_X_GC_OK: SGIXGCStatus = 0;
pub const SGI_X_GC_BAD_FIELDS: SGIXGCStatus = 1;
pub const SGI_X_GC_BAD_COLOR: SGIXGCStatus = 2;
pub const SGI_X_GC_BAD_MASK: SGIXGCStatus = 3;
pub const SGI_X_GC_MIXED_DID: SGIXGCStatus = 4;
pub const SGI_X_GC_DEFERRED: SGIXGCStatus = 5;

#[derive(Clone, Copy, Default, Debug)]
pub struct SGIXGC {
    pub raw: [u32; SGI_X_GC_WORDS],
    pub target: SGIXGCTarget,
    pub foreground: u32,
    pub background: u32,
    pub mask: u32,
    pub width_mask: u32,
    pub gx: u8,
    pub first_selector: u8,
    pub second_selector: u8,
    pub opaque_or_tiled: bool,
}

/// GX numbers are shared with GL enums after subtracting GL_CLEAR.
pub fn sgi_x_logic_op(gx: u32, source: u32, destination: u32) -> u32 {
    // The board-memory helper masks op, while invalid GL enums use source.
    if gx <= 15 {
        sgi_logic_op(gx as u8, source, destination)
    } else {
        source
    }
}

/// RGB8 accepts zero or full alpha, with all field-gap bits zero.
pub fn sgi_x_gc_decode(words: &[u32; SGI_X_GC_WORDS], decoded: &mut SGIXGC) -> SGIXGCStatus {
    decode(words, decoded, false)
}
/// Preserve the full native main-plane index and mask.
pub fn sgi_x_gc_decode_main_index(words: &[u32; SGI_X_GC_WORDS], decoded: &mut SGIXGC) -> SGIXGCStatus {
    let status = decode(words, decoded, true);

    if status == SGI_X_GC_OK && decoded.target != SGI_X_GC_MAIN_INDEX16 {
        SGI_X_GC_BAD_FIELDS
    } else {
        status
    }
}
pub fn sgi_x_gc_apply(gc: &SGIXGC, source: u32, destination: u32) -> u32 {
    ((sgi_x_logic_op(gc.gx as u32, source, destination) & gc.mask) | (destination & !gc.mask))
        & gc.width_mask
}
pub fn sgi_x_gc_noop(gc: &SGIXGC) -> bool {
    gc.mask == 0 || gc.gx == 5
}
pub fn sgi_x_gc_needs_destination(gc: &SGIXGC) -> bool {
    if sgi_x_gc_noop(gc) {
        return false;
    }
    gc.mask != gc.width_mask || (gc.gx != 0 && gc.gx != 3 && gc.gx != 12 && gc.gx != 15)
}

pub fn index_value(high: u32) -> u32 {
    ((high >> 4) & 0xfff0) | ((high >> 3) & 0xf)
}

pub fn index_mask(word: u32) -> u32 {
    ((word & 0xfff) << 4) | ((word >> 20) & 0xf)
}

/// Whether a 12-bit colour field is a widened component: an 8-bit one
/// b << 4 | b >> 4 for 24-bit TrueColor, a 10-bit one c << 2 | c >> 8 for
/// 30-bit TrueColor, as the X server's GC builder widens them.
pub fn canonical_component(component: u32, rgb30: bool) -> bool {
    if rgb30 {
        let c = component >> 2;

        component == ((c << 2) | (c >> 8))
    } else {
        let byte = component >> 4;

        component == ((byte << 4) | (byte >> 4))
    }
}

/// The model keeps eight bits a component: a 30-bit TrueColor component's
/// low two bits are dropped.
pub fn canonical_rgb(color: u64, rgb30: bool, out: &mut u32) -> bool {
    let fields: u64 = (0xfffu64 << 40) | (0xfffu64 << 27) | (0xfffu64 << 14) | 0x1ffe;
    let alpha = ((color >> 1) & 0xfff) as u32;

    if (color & !fields) != 0 || (alpha != 0 && alpha != 0xfff) {
        return false;
    }
    *out = 0;
    for k in 1..=3u32 {
        let component = ((color >> (13 * k + 1)) & 0xfff) as u32;

        if !canonical_component(component, rgb30) {
            return false;
        }
        *out |= (component >> 4) << (8 * (k - 1));
    }
    true
}

pub fn decode_did(w: &[u32; SGI_X_GC_WORDS], gc: &mut SGIXGC) -> SGIXGCStatus {
    let high = w[0] != 0 || w[4] != 0;
    let low = w[1] != 0 || w[5] != 0;

    gc.target = SGI_X_GC_DID;
    gc.width_mask = 0xffff;
    if w[6] != 8 || w[7] != 1 || w[8] != 0 || w[9] != 6 || w[10] != 0 || w[11] != 1 || w[12] != 1
        || w[2] != 0 || w[3] != 0
    {
        return SGI_X_GC_BAD_FIELDS;
    }
    if high && low {
        return SGI_X_GC_MIXED_DID;
    }
    if (w[0] & !0xfff78u32) != 0 || (w[1] & !0x3ffde00u32) != 0 {
        return SGI_X_GC_BAD_COLOR;
    }
    if (w[4] & !0xf00fffu32) != 0 || (w[5] & !0xfff00fu32) != 0 {
        return SGI_X_GC_BAD_MASK;
    }
    if high {
        gc.foreground = index_value(w[0]);
        gc.mask = index_mask(w[4]);
    } else {
        gc.foreground = ((w[1] >> 9) & 0xf) | ((w[1] >> 10) & 0xfff0);
        gc.mask = ((w[5] >> 12) & 0xfff) | ((w[5] & 0xf) << 12);
    }
    SGI_X_GC_OK
}

pub fn decode(words: &[u32; SGI_X_GC_WORDS], gc: &mut SGIXGC, native_index: bool) -> SGIXGCStatus {
    let w = words;
    let plane = w[6] & 0x3c;
    let mask_word = w[4] & !0x80000000u32;
    // The X server's GC builder copies the buffer selector in op bits 1:0
    // into bits 3:2 for a format with bit 0x80000. What the second copy
    // selects is unknown; a copy equal to the first is read as the same
    // selection, and any other value is deferred.
    let op = if ((w[7] >> 2) & 3) == (w[7] & 3) { w[7] & !0xcu32 } else { w[7] };
    let residual = op & !0xfc0003u32;
    let geometry = w[10] == 0;
    let rgb30 = w[6] == 0 && (w[10] == 0 || w[10] == 9);

    *gc = SGIXGC::default();
    gc.raw = *w;
    if w[9] > 30 || (w[9] & 1) != 0 {
        return SGI_X_GC_BAD_FIELDS;
    }
    gc.gx = (w[9] / 2) as u8;
    gc.first_selector = (w[7] & 3) as u8;
    gc.second_selector = ((w[7] >> 18) & 0x3f) as u8;
    gc.opaque_or_tiled = (w[4] & 0x80000000u32) != 0;

    if w[6] == 8 {
        return decode_did(w, gc);
    }
    // cfg 0x28 with buffer selector 3 is the geometry GC loader's form for
    // an 8-bit overlay window drawn into both overlay buffers: it repeats
    // the index and the mask in the high byte and replaces the plane set
    // with 0x28 (selector 1 gives 0x2c and 2 gives 0x30). The layer clear
    // sends it to clear the overlay layer, index 0 under all ones, before
    // its boxes. A 15-bit overlay format (format bit 0x2000) also gets
    // 0x28, whatever its selector, with the value p << 1 | 1 and the mask
    // pm << 1 | 1; those forms stay deferred.
    // Limitation: a 15-bit GC sent with selector 3 whose value and mask
    // happen to repeat one byte would be read as this form. No caller is
    // known to send one; the layer clear passes the 8-bit format for the
    // overlay layer.
    if (plane == 0x28 && !(w[6] == 0x28 && op == 3))
        || (w[6] == 0x1008 && w[7] == 0x10001 && w[10] == 5)
    {
        return SGI_X_GC_DEFERRED;
    }
    match w[6] {
        0 => {
            // 30-bit TrueColor: cfg 0 from every builder, told apart by
            // ifmt 9, or 0 from the geometry builder.
            if !rgb30 {
                return SGI_X_GC_BAD_FIELDS;
            }
            gc.target = SGI_X_GC_MAIN_RGB8;
            gc.width_mask = 0xffffff;
        }
        0xc | 0x180c => {
            gc.target = if native_index { SGI_X_GC_MAIN_INDEX16 } else { SGI_X_GC_MAIN_CI8 };
            gc.width_mask = if native_index { 0xffff } else { 0xff };
        }
        4 | 0x2804 | 0x3804 => {
            gc.target = SGI_X_GC_MAIN_RGB8;
            gc.width_mask = 0xffffff;
        }
        0x2c | 0x182c | 0x102c => {
            gc.target = SGI_X_GC_OVERLAY0;
            gc.width_mask = 0xffff;
        }
        0x30 | 0x1030 => return SGI_X_GC_DEFERRED,
        0x1830 => {
            gc.target = SGI_X_GC_OVERLAY1;
            gc.width_mask = 0xffff;
        }
        0x28 => {
            gc.target = SGI_X_GC_OVERLAY_BOTH;
            gc.width_mask = 0xff;
        }
        0x20 | 0x1820 | 0x1020 => {
            gc.target = SGI_X_GC_POPUP;
            gc.width_mask = 3;
        }
        _ => return SGI_X_GC_BAD_FIELDS,
    }
    if (op & 4) != 0 {
        return SGI_X_GC_DEFERRED;
    }
    if !rgb30 && geometry != (w[6] < 0x1000) {
        return SGI_X_GC_BAD_FIELDS;
    }
    if gc.target == SGI_X_GC_MAIN_CI8
        || gc.target == SGI_X_GC_MAIN_INDEX16
        || gc.target == SGI_X_GC_MAIN_RGB8
    {
        let rgb = gc.target == SGI_X_GC_MAIN_RGB8;
        let rgb_v = if rgb { 0 } else { 1 };

        if w[8] != 1 || w[11] != rgb_v || w[12] != rgb_v {
            return SGI_X_GC_BAD_FIELDS;
        }
        if geometry {
            if op != 1 && !(rgb && w[6] == 4 && op == 3) {
                return SGI_X_GC_BAD_FIELDS;
            }
        } else if rgb30 {
            // Selector 2 has no cfg form to show it in; deferred.
            if op != 0x64001 {
                return SGI_X_GC_DEFERRED;
            }
        } else if w[10] != (if rgb { 8 } else { 5 })
            || gc.first_selector < 1
            || gc.first_selector > 2
            || gc.second_selector < 1
            || gc.second_selector > 2
            || residual != (if rgb { 0x20000 } else { 0 })
            || (rgb && ((w[6] == 0x3804) != (gc.second_selector == 2)))
        {
            return SGI_X_GC_BAD_FIELDS;
        }
    } else if w[8] != 0
        || w[11] != 1
        || w[12] != 1
        || (!geometry && w[10] != 5)
        || op
            != (if gc.target == SGI_X_GC_OVERLAY1 {
                2
            } else if gc.target == SGI_X_GC_OVERLAY_BOTH {
                3
            } else {
                1
            })
    {
        return SGI_X_GC_BAD_FIELDS;
    }

    if gc.target == SGI_X_GC_MAIN_RGB8 {
        let red = mask_word & 0xfff;
        let green = (mask_word >> 12) & 0xfff;
        let blue = w[5] & 0xfff;

        if !canonical_rgb(((w[0] as u64) << 32) | w[1] as u64, rgb30, &mut gc.foreground)
            || !canonical_rgb(((w[2] as u64) << 32) | w[3] as u64, rgb30, &mut gc.background)
        {
            return SGI_X_GC_BAD_COLOR;
        }
        if (mask_word & !0xffffffu32) != 0
            || (w[5] & !0xffffffu32) != 0
            || (w[5] & 0xfff000) != 0xfff000
            || !canonical_component(red, rgb30)
            || !canonical_component(green, rgb30)
            || !canonical_component(blue, rgb30)
        {
            return SGI_X_GC_BAD_MASK;
        }
        gc.mask = ((red >> 4) << 16) | ((green >> 4) << 8) | (blue >> 4);
    } else {
        if (w[0] & !0xfff78u32) != 0
            || (w[2] & !0xfff78u32) != 0
            || (w[1] != 0 && w[1] != 0x1ffe)
            || (w[3] != 0 && w[3] != 0x1ffe)
        {
            return SGI_X_GC_BAD_COLOR;
        }
        if (mask_word & !0xf00fffu32) != 0 || w[5] != 0 {
            return SGI_X_GC_BAD_MASK;
        }
        gc.foreground = index_value(w[0]);
        gc.background = index_value(w[2]);
        gc.mask = index_mask(mask_word);
        if gc.target == SGI_X_GC_MAIN_CI8 {
            if ((gc.foreground | gc.background) & 0xff) != 0 {
                return SGI_X_GC_BAD_COLOR;
            }
            if (gc.mask & 0xff) != 0xff {
                return SGI_X_GC_BAD_MASK;
            }
            gc.foreground >>= 8;
            gc.background >>= 8;
            gc.mask >>= 8;
        } else if gc.target == SGI_X_GC_OVERLAY_BOTH {
            // The two bytes are one index repeated; keep one of them.
            if (gc.foreground >> 8) != (gc.foreground & 0xff)
                || (gc.background >> 8) != (gc.background & 0xff)
            {
                return SGI_X_GC_BAD_COLOR;
            }
            if (gc.mask >> 8) != (gc.mask & 0xff) {
                return SGI_X_GC_BAD_MASK;
            }
            gc.foreground &= 0xff;
            gc.background &= 0xff;
            gc.mask &= 0xff;
        } else if gc.target == SGI_X_GC_POPUP {
            if (mask_word & !0x300000u32) != 0 {
                return SGI_X_GC_BAD_MASK;
            }
            gc.foreground &= 3;
            gc.background &= 3;
            gc.mask = (mask_word >> 20) & 3;
        }
    }
    SGI_X_GC_OK
}
