//! The X server's graphics context, as the thirteen words it loads.
//!
//! The words say what the next fills draw: a colour, a background colour, a
//! write mask, a logic operation, and a target (which planes). Only some forms
//! are understood. A form that is not is reported with why, and a draw using
//! it is refused and counted, never guessed.
//!
//! Ported from hw/display/sgi/x-gc.c in jamesbraid/qemu-sgi-origami. Its
//! grades carry over: the field layouts are software expectations read from
//! the senders. What QEMU marks inferred stays inferred here: that the second
//! buffer selector, when it equals the first, is the same selection; that the
//! overlay-both form (config 0x28, selector 3) is the geometry GC loader's
//! form for an 8-bit overlay window drawn into both overlay buffers; and that
//! the 30-bit TrueColor form is told apart by format 9. The canonical colour
//! subsets (8-bit index, 8-bit-per-component RGB) are model subsets, not
//! visual identities.

/// How many words a graphics context load carries.
pub const GC_WORDS: usize = 13;

/// Which planes a context draws into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// An 8-bit colour index in the main planes.
    MainCi8,
    /// The native 16-bit main-plane index.
    MainIndex16,
    /// 24-bit RGB in the main planes.
    MainRgb8,
    Overlay0,
    Overlay1,
    Popup,
    /// The display-ID plane.
    Did,
    /// Both overlay buffers at once, the same 8-bit index in each.
    OverlayBoth,
}

/// Why a context was not decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    BadFields,
    BadColour,
    BadMask,
    MixedDid,
    /// A form whose meaning is not known.
    Deferred,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gc {
    pub raw: [u32; GC_WORDS],
    pub target: Target,
    pub foreground: u32,
    pub background: u32,
    pub mask: u32,
    pub width_mask: u32,
    /// The logic operation, 0 to 15 in the GL numbering (clear, and, and
    /// reverse, copy, ...).
    pub gx: u8,
    pub first_selector: u8,
    pub second_selector: u8,
    /// Word 4's bit 31: an opaque stippled or tiled fill, which carries both
    /// colours and needs a stipple loaded.
    pub opaque_or_tiled: bool,
}

/// A logic operation on a source and destination; an operation above 15 is a
/// copy.
pub fn logic_op(gx: u32, s: u32, d: u32) -> u32 {
    match gx {
        0 => 0,
        1 => s & d,
        2 => s & !d,
        3 => s,
        4 => !s & d,
        5 => d,
        6 => s ^ d,
        7 => s | d,
        8 => !(s | d),
        9 => !(s ^ d),
        10 => !d,
        11 => s | !d,
        12 => !s,
        13 => !s | d,
        14 => !(s & d),
        15 => !0,
        _ => s,
    }
}

fn index_value(high: u32) -> u32 {
    (high >> 4) & 0xfff0 | (high >> 3) & 0xf
}

fn index_mask(word: u32) -> u32 {
    (word & 0xfff) << 4 | (word >> 20) & 0xf
}

/// Whether a 12-bit colour field is a widened component: an 8-bit one
/// `b << 4 | b >> 4` for 24-bit TrueColor, a 10-bit one `c << 2 | c >> 8` for
/// 30-bit TrueColor, as the X server's context builder widens them.
fn canonical_component(component: u32, rgb30: bool) -> bool {
    if rgb30 {
        let c = component >> 2;
        component == (c << 2 | c >> 8)
    } else {
        let byte = component >> 4;
        component == (byte << 4 | byte >> 4)
    }
}

/// The model keeps eight bits a component: a 30-bit component's low two bits
/// are dropped.
fn canonical_rgb(colour: u64, rgb30: bool) -> Option<u32> {
    const FIELDS: u64 = 0xfff << 40 | 0xfff << 27 | 0xfff << 14 | 0x1ffe;
    let alpha = (colour >> 1) & 0xfff;
    if colour & !FIELDS != 0 || (alpha != 0 && alpha != 0xfff) {
        return None;
    }
    let mut out = 0;
    for k in 1..=3u32 {
        let component = ((colour >> (13 * k + 1)) & 0xfff) as u32;
        if !canonical_component(component, rgb30) {
            return None;
        }
        out |= (component >> 4) << (8 * (k - 1));
    }
    Some(out)
}

fn decode_did(w: &[u32; GC_WORDS], mut gc: Gc) -> Result<Gc, Status> {
    let (high, low) = (w[0] != 0 || w[4] != 0, w[1] != 0 || w[5] != 0);
    gc.target = Target::Did;
    gc.width_mask = 0xffff;
    if w[6] != 8 || w[7] != 1 || w[8] != 0 || w[9] != 6 || w[10] != 0 || w[11] != 1 || w[12] != 1 || w[2] != 0 || w[3] != 0 {
        return Err(Status::BadFields);
    }
    if high && low {
        return Err(Status::MixedDid);
    }
    if w[0] & !0xfff78 != 0 || w[1] & !0x3ffde00 != 0 {
        return Err(Status::BadColour);
    }
    if w[4] & !0xf0_0fff != 0 || w[5] & !0xfff_00f != 0 {
        return Err(Status::BadMask);
    }
    if high {
        gc.foreground = index_value(w[0]);
        gc.mask = index_mask(w[4]);
    } else {
        gc.foreground = (w[1] >> 9) & 0xf | (w[1] >> 10) & 0xfff0;
        gc.mask = (w[5] >> 12) & 0xfff | (w[5] & 0xf) << 12;
    }
    Ok(gc)
}

impl Gc {
    /// Decode the words the way the X fills do: the native 16-bit index form
    /// for configurations 0xc and 0x180c, the 8-bit form otherwise.
    pub fn decode_for_fill(w: &[u32; GC_WORDS]) -> Result<Gc, Status> {
        if w[6] == 0xc || w[6] == 0x180c {
            Gc::decode_main_index(w)
        } else {
            Gc::decode(w)
        }
    }

    pub fn decode(w: &[u32; GC_WORDS]) -> Result<Gc, Status> {
        decode(w, false)
    }

    /// Keep the full native main-plane index and mask.
    pub fn decode_main_index(w: &[u32; GC_WORDS]) -> Result<Gc, Status> {
        let gc = decode(w, true)?;
        if gc.target == Target::MainIndex16 { Ok(gc) } else { Err(Status::BadFields) }
    }

    /// The write that changes nothing: no bits enabled, or the no-op operation.
    pub fn is_noop(&self) -> bool {
        self.mask == 0 || self.gx == 5
    }

    /// Whether a write depends on the pixel it replaces.
    pub fn needs_destination(&self) -> bool {
        if self.is_noop() {
            return false;
        }
        self.mask != self.width_mask || !matches!(self.gx, 0 | 3 | 12 | 15)
    }

    /// The word written for `source` over `destination`.
    pub fn apply(&self, source: u32, destination: u32) -> u32 {
        ((logic_op(self.gx as u32, source, destination) & self.mask) | (destination & !self.mask)) & self.width_mask
    }
}

fn decode(w: &[u32; GC_WORDS], native_index: bool) -> Result<Gc, Status> {
    let plane = w[6] & 0x3c;
    let mask_word = w[4] & !0x8000_0000;
    // The X server's context builder copies the buffer selector in op bits
    // 1:0 into bits 3:2 for a format with bit 0x80000. What the second copy
    // selects is unknown: a copy equal to the first reads as the same
    // selection (inferred), any other value is deferred.
    let op = if (w[7] >> 2) & 3 == w[7] & 3 { w[7] & !0xc } else { w[7] };
    let residual = op & !0xfc_0003;
    let geometry = w[10] == 0;
    let rgb30 = w[6] == 0 && (w[10] == 0 || w[10] == 9);

    if w[9] > 30 || w[9] & 1 != 0 {
        return Err(Status::BadFields);
    }
    let mut gc = Gc {
        raw: *w,
        target: Target::MainCi8,
        foreground: 0,
        background: 0,
        mask: 0,
        width_mask: 0,
        gx: (w[9] / 2) as u8,
        first_selector: (w[7] & 3) as u8,
        second_selector: ((w[7] >> 18) & 0x3f) as u8,
        opaque_or_tiled: w[4] & 0x8000_0000 != 0,
    };
    if w[6] == 8 {
        return decode_did(w, gc);
    }
    // Config 0x28 with buffer selector 3 is the geometry GC loader's form for
    // an 8-bit overlay window drawn into both overlay buffers (inferred); a
    // 15-bit overlay format also gets 0x28 whatever its selector, and those
    // forms stay deferred.
    if (plane == 0x28 && !(w[6] == 0x28 && op == 3)) || (w[6] == 0x1008 && w[7] == 0x10001 && w[10] == 5) {
        return Err(Status::Deferred);
    }
    match w[6] {
        0 => {
            // 30-bit TrueColor: config 0 from every builder, told apart by
            // format 9, or 0 from the geometry builder (inferred).
            if !rgb30 {
                return Err(Status::BadFields);
            }
            gc.target = Target::MainRgb8;
            gc.width_mask = 0xff_ffff;
        }
        0xc | 0x180c => {
            gc.target = if native_index { Target::MainIndex16 } else { Target::MainCi8 };
            gc.width_mask = if native_index { 0xffff } else { 0xff };
        }
        4 | 0x2804 | 0x3804 => {
            gc.target = Target::MainRgb8;
            gc.width_mask = 0xff_ffff;
        }
        0x2c | 0x182c | 0x102c => {
            gc.target = Target::Overlay0;
            gc.width_mask = 0xffff;
        }
        0x30 | 0x1030 => return Err(Status::Deferred),
        0x1830 => {
            gc.target = Target::Overlay1;
            gc.width_mask = 0xffff;
        }
        0x28 => {
            gc.target = Target::OverlayBoth;
            gc.width_mask = 0xff;
        }
        0x20 | 0x1820 | 0x1020 => {
            gc.target = Target::Popup;
            gc.width_mask = 3;
        }
        _ => return Err(Status::BadFields),
    }
    if op & 4 != 0 {
        return Err(Status::Deferred);
    }
    if !rgb30 && geometry != (w[6] < 0x1000) {
        return Err(Status::BadFields);
    }
    if matches!(gc.target, Target::MainCi8 | Target::MainIndex16 | Target::MainRgb8) {
        let rgb = gc.target == Target::MainRgb8;
        let tail = if rgb { 0 } else { 1 };
        if w[8] != 1 || w[11] != tail || w[12] != tail {
            return Err(Status::BadFields);
        }
        if geometry {
            if op != 1 && !(rgb && w[6] == 4 && op == 3) {
                return Err(Status::BadFields);
            }
        } else if rgb30 {
            // Selector 2 has no config form to show it in: deferred.
            if op != 0x64001 {
                return Err(Status::Deferred);
            }
        } else if w[10] != if rgb { 8 } else { 5 }
            || !(1..=2).contains(&gc.first_selector)
            || !(1..=2).contains(&gc.second_selector)
            || residual != if rgb { 0x2_0000 } else { 0 }
            || (rgb && ((w[6] == 0x3804) != (gc.second_selector == 2)))
        {
            return Err(Status::BadFields);
        }
    } else if w[8] != 0
        || w[11] != 1
        || w[12] != 1
        || (!geometry && w[10] != 5)
        || op != match gc.target {
            Target::Overlay1 => 2,
            Target::OverlayBoth => 3,
            _ => 1,
        }
    {
        return Err(Status::BadFields);
    }

    if gc.target == Target::MainRgb8 {
        let (red, green, blue) = (mask_word & 0xfff, (mask_word >> 12) & 0xfff, w[5] & 0xfff);
        let (Some(fg), Some(bg)) = (canonical_rgb((w[0] as u64) << 32 | w[1] as u64, rgb30), canonical_rgb((w[2] as u64) << 32 | w[3] as u64, rgb30)) else {
            return Err(Status::BadColour);
        };
        gc.foreground = fg;
        gc.background = bg;
        if mask_word & !0xff_ffff != 0
            || w[5] & !0xff_ffff != 0
            || w[5] & 0xfff000 != 0xfff000
            || !canonical_component(red, rgb30)
            || !canonical_component(green, rgb30)
            || !canonical_component(blue, rgb30)
        {
            return Err(Status::BadMask);
        }
        gc.mask = (red >> 4) << 16 | (green >> 4) << 8 | (blue >> 4);
    } else {
        if w[0] & !0xfff78 != 0 || w[2] & !0xfff78 != 0 || (w[1] != 0 && w[1] != 0x1ffe) || (w[3] != 0 && w[3] != 0x1ffe) {
            return Err(Status::BadColour);
        }
        if mask_word & !0xf0_0fff != 0 || w[5] != 0 {
            return Err(Status::BadMask);
        }
        gc.foreground = index_value(w[0]);
        gc.background = index_value(w[2]);
        gc.mask = index_mask(mask_word);
        match gc.target {
            Target::MainCi8 => {
                if (gc.foreground | gc.background) & 0xff != 0 {
                    return Err(Status::BadColour);
                }
                if gc.mask & 0xff != 0xff {
                    return Err(Status::BadMask);
                }
                gc.foreground >>= 8;
                gc.background >>= 8;
                gc.mask >>= 8;
            }
            Target::OverlayBoth => {
                // The two bytes are one index repeated; keep one of them.
                if gc.foreground >> 8 != gc.foreground & 0xff || gc.background >> 8 != gc.background & 0xff {
                    return Err(Status::BadColour);
                }
                if gc.mask >> 8 != gc.mask & 0xff {
                    return Err(Status::BadMask);
                }
                gc.foreground &= 0xff;
                gc.background &= 0xff;
                gc.mask &= 0xff;
            }
            Target::Popup => {
                if mask_word & !0x30_0000 != 0 {
                    return Err(Status::BadMask);
                }
                gc.foreground &= 3;
                gc.background &= 3;
                gc.mask = (mask_word >> 20) & 3;
            }
            _ => {}
        }
    }
    Ok(gc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
    const RGB: [u32; 13] = [0, 0, 0, 0, 0xffffff, 0xffffff, 0x2804, 0x60001, 1, 6, 8, 0, 0];
    const DID: [u32; 13] = [0, 0, 0, 0, 0, 0xfc000, 8, 1, 0, 6, 0, 1, 1];

    fn ok(w: &[u32; 13]) -> Gc {
        Gc::decode(w).unwrap()
    }

    /// The 16 operations by their truth tables over (source, destination).
    #[test]
    fn logic_ops_follow_their_truth_tables() {
        const TRUTH: [u32; 16] = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];
        let (s, d) = (0x963c_a569u32, 0x5a81_c30fu32);
        for (gx, t) in TRUTH.iter().enumerate() {
            let want = (0..32).fold(0u32, |acc, bit| {
                let index = ((s >> bit) & 1) << 1 | (d >> bit) & 1;
                acc | ((t >> index) & 1) << bit
            });
            assert_eq!(logic_op(gx as u32, s, d), want, "gx {gx}");
        }
        assert_eq!(logic_op(16, s, d), s);
        assert_eq!(logic_op(u32::MAX, s, d), s);
    }

    #[test]
    fn an_index_context_decodes() {
        let gc = ok(&CI);
        assert_eq!((gc.target, gc.foreground, gc.background, gc.mask), (Target::MainCi8, 0x5a, 0xa5, 0xff));
        assert_eq!(gc.raw, CI);
        let mut w = CI;
        w[4] = 0xf0053f; // logical mask 0x53, alignment-fill bits 0xff
        let gc = ok(&w);
        assert_eq!(gc.mask, 0x53);
        assert_eq!(gc.apply(0x5a, 0xa5), 0xf6);
        w[4] = 0xf0000f;
        assert!(ok(&w).is_noop());
        w[4] = CI[4] ^ 1;
        assert_eq!(Gc::decode(&w), Err(Status::BadMask));
        w[4] = CI[4];
        w[0] |= 0x100;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
    }

    #[test]
    fn an_rgb_context_decodes_every_widened_byte() {
        let mut w = RGB;
        for b in 0..256u64 {
            let component = b << 4 | b >> 4;
            let colour = component << 40 | component << 27 | component << 14 | 0x1ffe;
            w[0] = (colour >> 32) as u32;
            w[1] = colour as u32;
            w[4] = (component | component << 12) as u32;
            w[5] = 0xfff000 | component as u32;
            let gc = ok(&w);
            assert_eq!((gc.foreground, gc.mask), (b as u32 * 0x010101, b as u32 * 0x010101));
        }
        let mut w = RGB;
        for m in 0..4096u32 {
            let byte = m >> 4;
            let canonical = m == (byte << 4 | byte >> 4);
            w[4] = 0xfff000 | m;
            assert_eq!(Gc::decode(&w).map(|_| ()), if canonical { Ok(()) } else { Err(Status::BadMask) }, "{m:#x}");
        }
        let mut w = RGB;
        w[0] = 0xff000;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
        w[0] = 0;
        w[2] = 0xff000;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
    }

    #[test]
    fn a_display_id_context_decodes_both_halves() {
        for v in (0..=0xffffu32).step_by(7).chain([0xffff]) {
            let mut w = DID;
            w[0] = (v & 15) << 3 | (v & 0xfff0) << 4;
            w[4] = (v >> 4) & 0xfff | (v & 15) << 20;
            w[5] = 0;
            let gc = ok(&w);
            assert_eq!((gc.target, gc.foreground, gc.mask), (Target::Did, v, v));
            w[0] = 0;
            w[4] = 0;
            w[1] = (v & 15) << 9 | (v & 0xfff0) << 10;
            w[5] = (v >> 12) & 15 | (v & 0xfff) << 12;
            let gc = ok(&w);
            assert_eq!((gc.foreground, gc.mask), (v, v));
        }
        let mut w = DID;
        assert_eq!(ok(&w).apply(0, 0xa5c3), 0xa503);
        w[4] = 0x300a5c;
        assert_eq!(Gc::decode(&w), Err(Status::MixedDid));
        w[5] = 0;
        w[0] = 0x80;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
        let mut w = DID;
        w[9] = 10;
        assert_eq!(Gc::decode(&w), Err(Status::BadFields));
    }

    #[test]
    fn the_ancillary_plane_sets_decode_to_their_targets() {
        for (cfg, target) in [
            (0x182c, Target::Overlay0),
            (0x2c, Target::Overlay0),
            (0x102c, Target::Overlay0),
            (0x1830, Target::Overlay1),
            (0x1820, Target::Popup),
            (0x20, Target::Popup),
            (0x1020, Target::Popup),
        ] {
            let mut w = CI;
            w[0] = 0x78;
            w[2] = 0x38;
            w[4] = if target == Target::Popup { 0x300000 } else { 0xf00000 };
            w[6] = cfg;
            w[7] = if cfg & 0x3c == 0x30 { 2 } else { 1 };
            w[8] = 0;
            if cfg < 0x1000 {
                w[10] = 0;
            }
            let gc = ok(&w);
            let (fg, m) = if target == Target::Popup { (3, 3) } else { (15, 15) };
            assert_eq!((gc.target, gc.foreground, gc.mask), (target, fg, m), "{cfg:#x}");
        }
        let mut w = CI;
        w[6] = 0xc;
        w[7] = 1;
        w[10] = 0;
        assert_eq!(ok(&w).target, Target::MainCi8);
        w[0] = 0x93000;
        assert_eq!(ok(&w).target, Target::MainCi8);
        w[4] |= 0x8000_0000;
        assert!(ok(&w).opaque_or_tiled);
        let mut w = RGB;
        w[7] = 0x7c001;
        assert_eq!(Gc::decode(&w), Err(Status::BadFields));
        w[6] = 0;
        w[7] = 0x64001;
        w[10] = 9;
        assert_eq!(ok(&w).target, Target::MainRgb8);
        w[7] = 0xa4001;
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
        assert_eq!(Gc::decode(&[0; 13]), Err(Status::BadFields));
    }

    /// The 30-bit packing: each 10-bit component to `c << 2 | c >> 8`; the
    /// model keeps the top eight bits.
    #[test]
    fn a_thirty_bit_context_keeps_eight_bits_a_component() {
        fn words(pixel: u32) -> (u32, u32) {
            let mut colour = 0x1ffeu64;
            for k in 1..=3u32 {
                let c = (pixel >> (10 * (k - 1))) & 0x3ff;
                colour |= ((c << 2 | c >> 8) as u64) << (13 * k + 1);
            }
            ((colour >> 32) as u32, colour as u32)
        }
        let mut w = [0, 0, 0, 0, 0xffffff, 0xffffff, 0, 0x64001, 1, 6, 9, 0, 0];
        (w[0], w[1]) = words(0x123 << 20 | 0x2c7 << 10 | 0x0a5);
        (w[2], w[3]) = words(0x3ff << 20 | 0x001 << 10 | 0x200);
        let gc = ok(&w);
        assert_eq!((gc.target, gc.foreground, gc.background, gc.mask), (Target::MainRgb8, 0x48b129, 0xff0080, 0xffffff));
        w[7] = 1;
        w[10] = 0;
        assert_eq!(ok(&w).foreground, 0x48b129);
        w[1] ^= 1 << 14;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
        w[1] ^= 1 << 14;
        w[10] = 5;
        assert_eq!(Gc::decode(&w), Err(Status::BadFields));
    }

    #[test]
    fn a_repeated_buffer_selector_reads_as_the_same_selection() {
        let mut w = RGB;
        w[7] = 0x60005;
        let gc = ok(&w);
        assert_eq!((gc.target, gc.first_selector), (Target::MainRgb8, 1));
        w[6] = 4;
        w[7] = 5;
        w[10] = 0;
        assert!(Gc::decode(&w).is_ok());
        w[7] = 6;
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
        let mut w = CI;
        w[7] = 0x40005;
        assert!(Gc::decode(&w).is_ok());
        w[7] = 0x40006;
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
    }

    #[test]
    fn masks_and_operations_apply_as_the_gc_says() {
        for gx in 0..16u32 {
            let mut w = CI;
            w[9] = 2 * gx;
            w[4] = 0xf0053f;
            let gc = ok(&w);
            assert_eq!(gc.gx as u32, gx);
            assert_eq!(gc.apply(0x195, 0x26a), (logic_op(gx, 0x95, 0x6a) & 0x53) | (0x6a & 0xac));
            assert_eq!(gc.is_noop(), gx == 5);
            assert_eq!(gc.needs_destination(), gx != 5);
        }
        let mut w = CI;
        for gx in 0..16u32 {
            w[9] = 2 * gx;
            assert_eq!(ok(&w).needs_destination(), !matches!(gx, 0 | 3 | 5 | 12 | 15));
        }
        for bl in (1..32).step_by(2) {
            w[9] = bl;
            assert_eq!(Gc::decode(&w), Err(Status::BadFields));
        }
        w[9] = 32;
        assert_eq!(Gc::decode(&w), Err(Status::BadFields));
    }

    #[test]
    fn reserved_bits_refuse() {
        let mut w = CI;
        w[6] = 0x1820;
        w[7] = 1;
        w[8] = 0;
        for bit in 22..=23 {
            w[4] = 0x300000 | 1 << bit;
            assert_eq!(Gc::decode(&w), Err(Status::BadMask));
        }
        for pair in [0, 2] {
            for gap in [0, 13, 26, 39, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63] {
                let mut w = RGB;
                let v = 1u64 << gap;
                w[pair] = (v >> 32) as u32;
                w[pair + 1] = v as u32;
                assert_eq!(Gc::decode(&w), Err(Status::BadColour), "pair {pair} gap {gap}");
            }
            let mut w = RGB;
            w[pair + 1] = 2;
            assert_eq!(Gc::decode(&w), Err(Status::BadColour));
            w[pair + 1] = 0x1ffe;
            assert!(Gc::decode(&w).is_ok());
        }
        let mut w = CI;
        (w[6], w[7], w[8], w[10]) = (0x30, 2, 0, 0);
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
        (w[6], w[10]) = (0x1030, 5);
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
    }

    #[test]
    fn the_overlay_layer_clear_decodes_and_its_siblings_defer() {
        let mut w = [0, 0x1ffe, 0, 0x1ffe, 0xf00fff, 0, 0x28, 3, 0, 6, 0, 1, 1];
        let gc = ok(&w);
        assert_eq!((gc.target, gc.foreground, gc.mask, gc.width_mask), (Target::OverlayBoth, 0, 0xff, 0xff));
        w[0] = 0x5a5 << 8 | 0xa << 3;
        assert_eq!(ok(&w).foreground, 0x5a);
        w[0] = 0x5a000;
        assert_eq!(Gc::decode(&w), Err(Status::BadColour));
        w[0] = 0;
        w[4] = 0xfff;
        assert_eq!(Gc::decode(&w), Err(Status::BadMask));
        w[4] = 0xf00fff;
        w[7] = 1;
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
        (w[6], w[10], w[7]) = (0x1828, 5, 3);
        assert_eq!(Gc::decode(&w), Err(Status::Deferred));
    }

    /// The fills read configurations 0xc and 0x180c as the native 16-bit index.
    #[test]
    fn a_fill_reads_the_native_index_form_for_its_two_configurations() {
        let mut w = CI;
        w[6] = 0xc;
        w[7] = 1;
        w[10] = 0;
        assert_eq!(Gc::decode_for_fill(&w).unwrap().target, Target::MainIndex16);
        assert_eq!(Gc::decode_main_index(&CI).unwrap().target, Target::MainIndex16);
        assert_eq!(Gc::decode_main_index(&RGB), Err(Status::BadFields));
        assert_eq!(Gc::decode_for_fill(&RGB).unwrap().target, Target::MainRgb8);
    }
}
