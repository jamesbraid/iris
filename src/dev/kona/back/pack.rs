//! Packers and unpackers for the pixel formats a transfer crosses.
//!
//! The host sees only a packed image: the pack mode and the host-format word
//! pick one of the host formats below, and the bus carries pixels and texels
//! in the 64-bit words of the bus formats. The model keeps a colour as four
//! 16-bit components, red, green, blue and alpha (`Rgba16`), and a scalar
//! (stencil or depth) as an unsigned integer of the format's width; a pack or
//! unpack converts a row between the plane's value and the packed bytes, with
//! the format chosen once per transfer.
//!
//! Layouts. Components pack most significant first and multi-byte fields are
//! big-endian, the guest's memory order. That is the model's reading of the
//! format names, not a measurement: the hardware documents name the formats and give the
//! bus word of one of them (RGBA12, code 0) and nothing else about bit
//! positions, so every other layout here is approximated. Conversions between
//! widths the hardware documents do not give round to nearest: narrowing takes
//! `(v * max_n + 32767) / 65535`, widening scales `v` by 65535 / max_n with
//! rounding, so a value that fits in `n` bits survives a round trip exactly.
//!
//! Not modelled, and refused (None): the host RGBA12 format, whose field
//! placement the hardware documents do not give, and the numeric pack-mode encodings,
//! which belong to the token layer.

/// A colour as red, green, blue and alpha, each 0 to 65535.
pub type Rgba16 = [u16; 4];

/// The formats of the host-side packing: set by the pack-mode register and the
/// host-format word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFormat {
    Rgba4,
    /// One bit a pixel, most significant bit first, rows padded to a byte.
    Bitmap,
    Rgb5A1,
    R5G6B5,
    La8,
    L16,
    /// Intensity: one component that fills all four.
    I16,
    S16,
    Z16,
    Rgba8,
    Rgb10A2,
    La16,
    Z32,
    Rgba16,
    Rgba12,
}

/// `v` of `from` bits as `to` bits, rounded to nearest (both at most 32).
pub fn rescale(v: u32, from: u32, to: u32) -> u32 {
    debug_assert!(from > 0 && from <= 32 && to > 0 && to <= 32, "rescale {from} to {to} bits");
    if from == to {
        return v;
    }
    let (fmax, tmax) = (((1u64 << from) - 1), ((1u64 << to) - 1));
    ((v as u64 * tmax + fmax / 2) / fmax) as u32
}

fn widen(v: u32, bits: u32) -> u16 {
    rescale(v, bits, 16) as u16
}

fn narrow(v: u16, bits: u32) -> u32 {
    rescale(v as u32, 16, bits)
}

impl HostFormat {
    /// Bytes a pixel takes, or None for the bitmap (a row is `ceil(w / 8)`
    /// bytes) and the unmodelled RGBA12.
    pub fn bytes_per_pixel(self) -> Option<usize> {
        use HostFormat::*;
        match self {
            Rgba4 | Rgb5A1 | R5G6B5 | La8 | L16 | I16 | S16 | Z16 => Some(2),
            Rgba8 | Rgb10A2 | La16 | Z32 => Some(4),
            Rgba16 => Some(8),
            Bitmap | Rgba12 => None,
        }
    }

    /// Whether the format carries a colour (as opposed to a scalar).
    pub fn is_colour(self) -> bool {
        !matches!(self, HostFormat::Bitmap | HostFormat::S16 | HostFormat::Z16 | HostFormat::Z32 | HostFormat::Rgba12)
    }

    /// The bits of a scalar format (stencil or depth).
    pub fn scalar_bits(self) -> Option<u32> {
        match self {
            HostFormat::S16 | HostFormat::Z16 => Some(16),
            HostFormat::Z32 => Some(32),
            _ => None,
        }
    }

    /// Pack a colour into `out`, which must hold `bytes_per_pixel`. The
    /// luminance formats take the red component as luminance (approximated:
    /// the planes' luminance is not defined further). None for a scalar,
    /// bitmap or unmodelled format, or a short `out`.
    pub fn pack_colour(self, c: Rgba16, out: &mut [u8]) -> Option<()> {
        use HostFormat::*;
        let n = self.bytes_per_pixel().filter(|_| self.is_colour())?;
        let out = out.get_mut(..n)?;
        let [r, g, b, a] = c;
        match self {
            Rgba4 => out.copy_from_slice(&((narrow(r, 4) << 12 | narrow(g, 4) << 8 | narrow(b, 4) << 4 | narrow(a, 4)) as u16).to_be_bytes()),
            Rgb5A1 => out.copy_from_slice(&((narrow(r, 5) << 11 | narrow(g, 5) << 6 | narrow(b, 5) << 1 | narrow(a, 1)) as u16).to_be_bytes()),
            R5G6B5 => out.copy_from_slice(&((narrow(r, 5) << 11 | narrow(g, 6) << 5 | narrow(b, 5)) as u16).to_be_bytes()),
            La8 => out.copy_from_slice(&[narrow(r, 8) as u8, narrow(a, 8) as u8]),
            L16 | I16 => out.copy_from_slice(&r.to_be_bytes()),
            Rgba8 => out.copy_from_slice(&[narrow(r, 8) as u8, narrow(g, 8) as u8, narrow(b, 8) as u8, narrow(a, 8) as u8]),
            Rgb10A2 => out.copy_from_slice(&(narrow(r, 10) << 22 | narrow(g, 10) << 12 | narrow(b, 10) << 2 | narrow(a, 2)).to_be_bytes()),
            La16 => {
                out[..2].copy_from_slice(&r.to_be_bytes());
                out[2..].copy_from_slice(&a.to_be_bytes());
            }
            Rgba16 => {
                for (i, v) in c.iter().enumerate() {
                    out[2 * i..2 * i + 2].copy_from_slice(&v.to_be_bytes());
                }
            }
            Bitmap | S16 | Z16 | Z32 | Rgba12 => unreachable!("filtered by is_colour"),
        }
        Some(())
    }

    /// Unpack one colour from `src`. Luminance fills red, green and blue and
    /// leaves alpha full; intensity fills all four.
    pub fn unpack_colour(self, src: &[u8]) -> Option<Rgba16> {
        use HostFormat::*;
        let n = self.bytes_per_pixel().filter(|_| self.is_colour())?;
        let s = src.get(..n)?;
        let be16 = |i: usize| u16::from_be_bytes([s[i], s[i + 1]]) as u32;
        Some(match self {
            Rgba4 => {
                let v = be16(0);
                [widen(v >> 12, 4), widen(v >> 8 & 15, 4), widen(v >> 4 & 15, 4), widen(v & 15, 4)]
            }
            Rgb5A1 => {
                let v = be16(0);
                [widen(v >> 11, 5), widen(v >> 6 & 31, 5), widen(v >> 1 & 31, 5), widen(v & 1, 1)]
            }
            R5G6B5 => {
                let v = be16(0);
                [widen(v >> 11, 5), widen(v >> 5 & 63, 6), widen(v & 31, 5), u16::MAX]
            }
            La8 => {
                let l = widen(s[0] as u32, 8);
                [l, l, l, widen(s[1] as u32, 8)]
            }
            L16 => [be16(0) as u16, be16(0) as u16, be16(0) as u16, u16::MAX],
            I16 => [be16(0) as u16; 4],
            Rgba8 => [widen(s[0] as u32, 8), widen(s[1] as u32, 8), widen(s[2] as u32, 8), widen(s[3] as u32, 8)],
            Rgb10A2 => {
                let v = u32::from_be_bytes([s[0], s[1], s[2], s[3]]);
                [widen(v >> 22, 10), widen(v >> 12 & 1023, 10), widen(v >> 2 & 1023, 10), widen(v & 3, 2)]
            }
            La16 => [be16(0) as u16, be16(0) as u16, be16(0) as u16, be16(2) as u16],
            Rgba16 => [be16(0) as u16, be16(2) as u16, be16(4) as u16, be16(6) as u16],
            Bitmap | S16 | Z16 | Z32 | Rgba12 => unreachable!("filtered by is_colour"),
        })
    }

    /// Pack a scalar of `scalar_bits` into `out`.
    pub fn pack_scalar(self, v: u32, out: &mut [u8]) -> Option<()> {
        match self.scalar_bits()? {
            16 => out.get_mut(..2)?.copy_from_slice(&(v as u16).to_be_bytes()),
            _ => out.get_mut(..4)?.copy_from_slice(&v.to_be_bytes()),
        }
        Some(())
    }

    pub fn unpack_scalar(self, src: &[u8]) -> Option<u32> {
        Some(match self.scalar_bits()? {
            16 => u16::from_be_bytes(src.get(..2)?.try_into().ok()?) as u32,
            _ => u32::from_be_bytes(src.get(..4)?.try_into().ok()?),
        })
    }
}

/// Pack a row of bitmap pixels (nonzero is set), most significant bit first,
/// into `out`, which holds `ceil(n / 8)` bytes; the padding bits are zero.
pub fn pack_bitmap_row(pixels: &[bool], out: &mut [u8]) -> Option<()> {
    let out = out.get_mut(..pixels.len().div_ceil(8))?;
    out.fill(0);
    for (i, &p) in pixels.iter().enumerate() {
        out[i / 8] |= (p as u8) << (7 - i % 8);
    }
    Some(())
}

pub fn unpack_bitmap_row(src: &[u8], n: usize) -> Option<Vec<bool>> {
    let src = src.get(..n.div_ceil(8))?;
    Some((0..n).map(|i| src[i / 8] >> (7 - i % 8) & 1 != 0).collect())
}

/// The formats of a 64-bit raster-bus word, by the code the output module is
/// told. Pixels and texels cross the bus in these. The code-to-contents table
/// is established from the pipe's firmware as software expectation; the hardware documents
/// give the field positions of code 0 only, so only it packs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusFormat {
    /// One RGBA12 pixel: R 63-52, G 51-40, B 39-28, A 27-16.
    Rgba12,
    TwoRgba8,
    TwoRgb10A2,
    TwoL16A16,
    TwoZ32,
    FourRgba4,
    FourRgb5A1,
    FourR5G6B5,
    FourLa8,
    /// Four L16 or Z16.
    FourL16,
    TwoComponent32,
    OneRgba16,
    OneRgb16,
    TwoRgb8,
    EightBytes,
    /// Texel-only formats in 16-bit fields.
    TexelL4,
    TexelA4,
    TexelLa4,
    TexelL8,
    TexelL12A4,
    TexelA16,
}

impl BusFormat {
    /// The format of `code`; None for the codes with no module (0xd, 0x10,
    /// 0x11, 0x18, 0x19) and anything beyond.
    pub fn from_code(code: u8) -> Option<BusFormat> {
        use BusFormat::*;
        Some(match code {
            0x0 => Rgba12,
            0x1 => TwoRgba8,
            0x2 => TwoRgb10A2,
            0x3 => TwoL16A16,
            0x4 => TwoZ32,
            0x5 => FourRgba4,
            0x6 => FourRgb5A1,
            0x7 => FourR5G6B5,
            0x8 => FourLa8,
            0x9 => FourL16,
            0xa => TwoComponent32,
            0xb => OneRgba16,
            0xc => OneRgb16,
            0xe => TwoRgb8,
            0xf => EightBytes,
            0x12 => TexelL4,
            0x13 => TexelA4,
            0x14 => TexelLa4,
            0x15 => TexelL8,
            0x16 => TexelL12A4,
            0x17 => TexelA16,
            _ => return None,
        })
    }

    /// Pixels or texels in one 64-bit word.
    pub fn per_word(self) -> u32 {
        use BusFormat::*;
        match self {
            Rgba12 | OneRgba16 | OneRgb16 => 1,
            TwoRgba8 | TwoRgb10A2 | TwoL16A16 | TwoZ32 | TwoComponent32 | TwoRgb8 => 2,
            FourRgba4 | FourRgb5A1 | FourR5G6B5 | FourLa8 | FourL16 => 4,
            EightBytes => 8,
            // Texels sit in 16-bit fields.
            TexelL4 | TexelA4 | TexelLa4 | TexelL8 | TexelL12A4 | TexelA16 => 4,
        }
    }

    /// Pack a 12-bit RGBA pixel (each component 0 to 4095) into its bus word.
    pub fn pack_rgba12(c: [u16; 4]) -> u64 {
        (c[0] as u64 & 0xfff) << 52 | (c[1] as u64 & 0xfff) << 40 | (c[2] as u64 & 0xfff) << 28 | (c[3] as u64 & 0xfff) << 16
    }

    pub fn unpack_rgba12(w: u64) -> [u16; 4] {
        [(w >> 52 & 0xfff) as u16, (w >> 40 & 0xfff) as u16, (w >> 28 & 0xfff) as u16, (w >> 16 & 0xfff) as u16]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostFormat::*;

    /// Every value of every field of every colour format survives unpack and
    /// pack. The packed bytes are built by hand, shifting the field value into
    /// its place in a big-endian integer, so nothing here uses the code's own
    /// scaling. Fields are (shift, bits) in the integer of the format's size.
    #[test]
    fn every_packed_field_value_survives_unpack_and_pack() {
        type Fields = &'static [(u32, u32)];
        let cases: [(HostFormat, usize, Fields); 10] = [
            (Rgba4, 2, &[(12, 4), (8, 4), (4, 4), (0, 4)]),
            (Rgb5A1, 2, &[(11, 5), (6, 5), (1, 5), (0, 1)]),
            (R5G6B5, 2, &[(11, 5), (5, 6), (0, 5)]),
            (La8, 2, &[(8, 8), (0, 8)]),
            (L16, 2, &[(0, 16)]),
            (I16, 2, &[(0, 16)]),
            (Rgba8, 4, &[(24, 8), (16, 8), (8, 8), (0, 8)]),
            (Rgb10A2, 4, &[(22, 10), (12, 10), (2, 10), (0, 2)]),
            (La16, 4, &[(16, 16), (0, 16)]),
            (Rgba16, 8, &[(48, 16), (32, 16), (16, 16), (0, 16)]),
        ];
        for (f, n, fields) in cases {
            for &(shift, bits) in fields {
                for v in 0..(1u64 << bits) {
                    let packed = (v << shift).to_be_bytes();
                    let bytes = &packed[8 - n..];
                    let c = f.unpack_colour(bytes).unwrap();
                    let mut out = [0u8; 8];
                    f.pack_colour(c, &mut out).unwrap();
                    assert_eq!(&out[..n], bytes, "{f:?} field at {shift} value {v}");
                }
            }
        }
    }

    #[test]
    fn packed_bytes_are_big_endian_msb_first() {
        let mut b = [0u8; 8];
        Rgba8.pack_colour([0xffff, 0, 0x8080, 0x0101], &mut b).unwrap();
        assert_eq!(b[..4], [0xff, 0x00, 0x80, 0x01]);
        Rgb10A2.pack_colour([0xffff, 0, 0, 0xffff], &mut b).unwrap();
        assert_eq!(b[..4], [0xff, 0xc0, 0x00, 0x03]);
        R5G6B5.pack_colour([0xffff, 0, 0xffff, 0], &mut b).unwrap();
        assert_eq!(b[..2], [0xf8, 0x1f]);
        Rgb5A1.pack_colour([0, 0, 0, 0xffff], &mut b).unwrap();
        assert_eq!(b[..2], [0x00, 0x01]);
        Rgba4.pack_colour([0xffff, 0, 0, 0xffff], &mut b).unwrap();
        assert_eq!(b[..2], [0xf0, 0x0f]);
        Rgba16.pack_colour([1, 2, 3, 4], &mut b).unwrap();
        assert_eq!(b, [0, 1, 0, 2, 0, 3, 0, 4]);
        La8.pack_colour([0x1212, 0, 0, 0x3434], &mut b).unwrap();
        assert_eq!(b[..2], [0x12, 0x34]);
    }

    /// Narrowing rounds to nearest.
    #[test]
    fn narrowing_rounds_to_nearest() {
        // A step of 8 bits is 257 of 16; its half is 128.5.
        assert_eq!(rescale(0x0081, 16, 8), 1);
        assert_eq!(rescale(0x0080, 16, 8), 0);
        assert_eq!(rescale(65535, 16, 10), 1023);
        assert_eq!(rescale(1023, 10, 8), 255);
        assert_eq!(rescale(0, 8, 32), 0);
        assert_eq!(rescale(255, 8, 32), u32::MAX);
        for bits in 1..=16 {
            for v in 0..(1u32 << bits) {
                assert_eq!(narrow(widen(v, bits), bits), v, "{bits} bits, {v}");
            }
        }
    }

    #[test]
    fn luminance_and_intensity_unpack_to_their_components() {
        assert_eq!(L16.unpack_colour(&[0x12, 0x34]), Some([0x1234, 0x1234, 0x1234, 0xffff]));
        assert_eq!(I16.unpack_colour(&[0x12, 0x34]), Some([0x1234; 4]));
        assert_eq!(La8.unpack_colour(&[0xff, 0x00]), Some([0xffff, 0xffff, 0xffff, 0]));
    }

    #[test]
    fn scalars_round_trip_at_their_width() {
        let mut b = [0u8; 4];
        for (f, v) in [(S16, 0xbeef), (Z16, 1), (Z32, 0xdead_beef)] {
            f.pack_scalar(v, &mut b).unwrap();
            assert_eq!(f.unpack_scalar(&b), Some(v));
        }
        assert_eq!(Z32.scalar_bits(), Some(32));
        assert!(Rgba8.pack_scalar(1, &mut b).is_none());
        assert!(Z16.pack_colour([0; 4], &mut b).is_none());
    }

    #[test]
    fn bitmap_rows_are_most_significant_bit_first_and_padded() {
        let px = [true, false, true, true, false, false, false, true, true];
        let mut b = [0xffu8; 2];
        pack_bitmap_row(&px, &mut b).unwrap();
        assert_eq!(b, [0b1011_0001, 0b1000_0000]);
        assert_eq!(unpack_bitmap_row(&b, 9).unwrap(), px);
        assert!(pack_bitmap_row(&px, &mut [0u8; 1]).is_none());
        assert!(Bitmap.bytes_per_pixel().is_none());
    }

    /// RGBA12 (host) is refused: the hardware documents do not place its fields. Short
    /// buffers are refused rather than read past.
    #[test]
    fn what_is_not_modelled_or_too_short_is_refused() {
        let mut b = [0u8; 8];
        assert!(Rgba12.pack_colour([0; 4], &mut b).is_none());
        assert!(Rgba12.unpack_colour(&b).is_none());
        assert!(Rgba8.pack_colour([0; 4], &mut b[..3]).is_none());
        assert!(Rgba8.unpack_colour(&b[..3]).is_none());
    }

    #[test]
    fn bus_codes_are_the_published_table() {
        use BusFormat::*;
        for code in [0xd, 0x10, 0x11, 0x18, 0x19, 0x1a, 0xff] {
            assert_eq!(BusFormat::from_code(code), None, "{code:#x}");
        }
        assert_eq!(BusFormat::from_code(0x0), Some(Rgba12));
        assert_eq!(BusFormat::from_code(0x4), Some(TwoZ32));
        assert_eq!(BusFormat::from_code(0x9), Some(FourL16));
        assert_eq!(BusFormat::from_code(0xf), Some(EightBytes));
        assert_eq!(BusFormat::from_code(0x17), Some(TexelA16));
        let per_word: Vec<_> = [0x0, 0x1, 0x5, 0xb, 0xf, 0x12].iter().map(|c| BusFormat::from_code(*c).unwrap().per_word()).collect();
        assert_eq!(per_word, [1, 2, 4, 1, 8, 4]);
        let all = (0u8..=0x1f).filter(|c| BusFormat::from_code(*c).is_some()).count();
        assert_eq!(all, 21);
    }

    /// The one bus word the hardware documents place: R 63-52, G 51-40, B 39-28, A 27-16.
    #[test]
    fn the_rgba12_bus_word_has_the_published_fields() {
        let w = BusFormat::pack_rgba12([0xabc, 0xdef, 0x123, 0x456]);
        assert_eq!(w, 0xabcd_ef12_3456_0000);
        assert_eq!(BusFormat::unpack_rgba12(w), [0xabc, 0xdef, 0x123, 0x456]);
        for v in 0..4096u16 {
            assert_eq!(BusFormat::unpack_rgba12(BusFormat::pack_rgba12([v, 4095 - v, v, v]))[1], 4095 - v);
        }
    }
}
