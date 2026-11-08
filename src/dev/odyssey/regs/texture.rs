// generated from odsy 7e00547eb687 TEXTURE_DECODE.h - do not edit, regenerate in odsy
//!
//! Texture state words of the Odyssey command stream: environment, mode, LOD,
//! offset and target blocks, and the opcodes that carry them.
//!
//! Grades on each item say what is known about it: `established` is what the
//! shipped driver software does, read in full; `inferred` is a reasoned
//! reading; `unknown` is not derived. None of them is a measurement of the
//! silicon.

#![allow(dead_code)]

/// established.
pub const OP_TEXENV: u32 = 0x141b;
pub const ENV_WORDS: u32 = 5;
/// established; same as BUZZ_OP_TEXMODE.
pub const OP_TEXMODE: u32 = 0x1c16;
pub const MODE_WORDS: u32 = 7;
/// established.
pub const OP_TEXLOD: u32 = 0x1013;
pub const LOD_WORDS: u32 = 4;
/// established.
pub const OP_TEXOFFSET: u32 = 0xc3e;
pub const OFFSET_WORDS: u32 = 3;
/// established.
pub const OP_FIRSTLEVEL: u32 = 0x406;
/// established.
pub const OP_TARGET: u32 = 0x405;
/// established.
pub const COLOUR12_BITS: u32 = 12;
/// established.
pub const COLOUR12_MAX: u32 = 0xfff;
pub const LOD_BITS: u32 = 14;
/// established, min/max clamp.
pub const LOD_LIMIT: u32 = 15;
/// established.
pub const LOD_MIN_DEFAULT: u32 = 0x3100;
/// established.
pub const LOD_MAX_DEFAULT: u32 = 0xf00;
/// established.
pub const LEVEL_TABLE_ENTRIES: u32 = 64;
/// established.
pub const PRIMARY_LEVELS_MAX: u32 = 16;
/// established.
pub const PRIMARY_LAST: u32 = 15;
/// established.
pub const UNUSED_FIRST: u32 = 16;
/// established.
pub const UNUSED_LAST: u32 = 48;
/// established.
pub const DETAIL_FIRST: u32 = 49;
/// established.
pub const DETAIL_COUNT_MAX: u32 = 15;
/// established.
pub const TABLE_LAST: u32 = 63;
/// established.
pub const SCRATCH_LEVEL: u32 = 60;
/// established.
pub const TILE_TEXELS: u32 = 16;
/// established arithmetic.
pub const SLICE_STRIDE_BASE: u32 = 32;
/// established: 0..49 span the tables.
pub const FMT_INDEX_MAX_USED: u32 = 49;
pub const PIXCLASS_COUNT: u32 = 18;
/// established: red and green lanes.
pub const COLTAB_A_BASE: u32 = 0x3e00;
/// established: blue and alpha lanes.
pub const COLTAB_B_BASE: u32 = 0x3e10;
/// i = 0..15.
pub const COLTAB_UPLOAD_LAST: u32 = 15;
pub const COLTAB_UPLOAD_WORDS: u32 = 16;
/// established.
pub const COLTAB_ENTRIES: u32 = 0x100;
/// established.
pub const COLTAB_MAX_WIDTH: u32 = 0x100;
/// established.
pub const COLOUR12_ROUND_SCALE: u32 = 0xfff;
/// inferred never sent.
pub const COLTAB_C_BASE: u32 = 0x3e20;
/// inferred never sent.
pub const COLTAB_D_BASE: u32 = 0x3e30;
/// established.
pub const SIZE_MAX: u32 = 0x8000;
/// established.
pub const BORDER_MAX: u32 = 1;

/// Values of `mode` in `EnvW0` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvW0Mode {
    /// established.
    Replace,
    /// established.
    Modulate,
    /// established.
    Decal,
    /// established.
    Blend,
    /// established.
    Add,
    /// A value the notes do not name.
    Other(u8),
}

impl EnvW0Mode {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x1 => Self::Replace,
            0x2 => Self::Modulate,
            0x3 => Self::Decal,
            0x4 => Self::Blend,
            0x5 => Self::Add,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::Replace => 0x1,
            Self::Modulate => 0x2,
            Self::Decal => 0x3,
            Self::Blend => 0x4,
            Self::Add => 0x5,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `value` in `Wrapaux` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapauxValue {
    /// established.
    RepeatClamp,
    /// established.
    Edge,
    /// established.
    Border,
    /// A value the notes do not name.
    Other(u8),
}

impl WrapauxValue {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x0 => Self::RepeatClamp,
            0x1 => Self::Edge,
            0x3f => Self::Border,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::RepeatClamp => 0x0,
            Self::Edge => 0x1,
            Self::Border => 0x3f,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `index` in `Fmt` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FmtIndex {
    /// established.
    A8,
    /// established.
    A8Alt,
    /// established.
    I8,
    /// established.
    I8Alt,
    /// established.
    L8,
    /// established.
    L8Alt,
    /// established.
    R3g3b2,
    /// established.
    R3g3b2Alt,
    /// established.
    A12,
    /// established.
    I12,
    /// established.
    L12,
    /// established.
    L8a8,
    /// established.
    Rgba4,
    /// established.
    Rgb5a1,
    /// established.
    L12a12,
    /// established.
    Rgba8,
    /// established.
    Rgb10a2,
    /// inferred.
    Rgba12Move,
    /// established.
    Rgba12,
    /// established.
    IccRgb,
    /// established.
    IccRgba,
    /// A value the notes do not name.
    Other(u8),
}

impl FmtIndex {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x0 => Self::A8,
            0x1 => Self::A8Alt,
            0x2 => Self::I8,
            0x3 => Self::I8Alt,
            0x4 => Self::L8,
            0x5 => Self::L8Alt,
            0x6 => Self::R3g3b2,
            0x7 => Self::R3g3b2Alt,
            0x8 => Self::A12,
            0x9 => Self::I12,
            0xa => Self::L12,
            0xb => Self::L8a8,
            0xc => Self::Rgba4,
            0xd => Self::Rgb5a1,
            0x10 => Self::L12a12,
            0x11 => Self::Rgba8,
            0x12 => Self::Rgb10a2,
            0x20 => Self::Rgba12Move,
            0x21 => Self::Rgba12,
            0x30 => Self::IccRgb,
            0x31 => Self::IccRgba,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::A8 => 0x0,
            Self::A8Alt => 0x1,
            Self::I8 => 0x2,
            Self::I8Alt => 0x3,
            Self::L8 => 0x4,
            Self::L8Alt => 0x5,
            Self::R3g3b2 => 0x6,
            Self::R3g3b2Alt => 0x7,
            Self::A12 => 0x8,
            Self::I12 => 0x9,
            Self::L12 => 0xa,
            Self::L8a8 => 0xb,
            Self::Rgba4 => 0xc,
            Self::Rgb5a1 => 0xd,
            Self::L12a12 => 0x10,
            Self::Rgba8 => 0x11,
            Self::Rgb10a2 => 0x12,
            Self::Rgba12Move => 0x20,
            Self::Rgba12 => 0x21,
            Self::IccRgb => 0x30,
            Self::IccRgba => 0x31,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `value` in `Class` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassValue {
    /// established.
    None,
    /// inferred.
    Alpha,
    /// inferred.
    Luminance,
    /// inferred.
    LuminanceAlpha,
    /// inferred.
    Intensity,
    /// inferred.
    Rgb,
    /// inferred.
    Rgba,
    /// A value the notes do not name.
    Other(u8),
}

impl ClassValue {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x0 => Self::None,
            0x1 => Self::Alpha,
            0x2 => Self::Luminance,
            0x3 => Self::LuminanceAlpha,
            0x4 => Self::Intensity,
            0x5 => Self::Rgb,
            0x6 => Self::Rgba,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::None => 0x0,
            Self::Alpha => 0x1,
            Self::Luminance => 0x2,
            Self::LuminanceAlpha => 0x3,
            Self::Intensity => 0x4,
            Self::Rgb => 0x5,
            Self::Rgba => 0x6,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `code` in `Size` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeCode {
    /// established.
    B2,
    /// established.
    B4,
    /// established.
    B8,
    /// A value the notes do not name.
    Other(u8),
}

impl SizeCode {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x0 => Self::B2,
            0x1 => Self::B4,
            0x2 => Self::B8,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::B2 => 0x0,
            Self::B4 => 0x1,
            Self::B8 => 0x2,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `value` in `Offcode` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffcodeValue {
    /// established.
    None,
    /// established.
    Code2,
    /// A value the notes do not name.
    Other(u8),
}

impl OffcodeValue {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x0 => Self::None,
            0x2 => Self::Code2,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::None => 0x0,
            Self::Code2 => 0x2,
            Self::Other(v) => v as u32,
        }
    }
}

/// The `ENV_W0` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvW0(u32);

impl EnvW0 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, established.
    pub const fn mode(self) -> EnvW0Mode {
        EnvW0Mode::from_bits(self.0 & 0x7)
    }

    /// Bits 31:3, unknown.
    pub const fn unwritten(self) -> u32 {
        self.0 >> 3
    }
}

/// The `ENV_W1` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvW1(u32);

impl EnvW1 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, inferred.
    pub const fn green(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, inferred.
    pub const fn red(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `ENV_W2` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvW2(u32);

impl EnvW2 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, inferred.
    pub const fn alpha(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, inferred.
    pub const fn blue(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `ENV_W3` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvW3(u32);

impl EnvW3 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 31:0, unknown.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// The `ENV_W4` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvW4(u32);

impl EnvW4 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 31:0, unknown.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// The `MODE_W0` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW0(u32);

impl ModeW0 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 4:0, inferred.
    pub const fn log2_r(self) -> u32 {
        self.0 & 0x1f
    }

    /// Bits 9:5, inferred.
    pub const fn log2_t(self) -> u32 {
        (self.0 >> 5) & 0x1f
    }

    /// Bits 14:10, inferred.
    pub const fn log2_s(self) -> u32 {
        (self.0 >> 10) & 0x1f
    }

    /// Bits 24:15, unknown.
    pub const fn unwritten(self) -> u32 {
        (self.0 >> 15) & 0x3ff
    }

    /// Bits 29:25, established.
    pub const fn detail_level(self) -> u32 {
        (self.0 >> 25) & 0x1f
    }

    /// Bit 30, unknown.
    pub const fn bit30(self) -> bool {
        (self.0 >> 30) & 1 != 0
    }

    /// Bit 31, established.
    pub const fn detail_modulate(self) -> bool {
        (self.0 >> 31) & 1 != 0
    }
}

/// The `MODE_W1` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW1(u32);

impl ModeW1 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 16:0, established.
    pub const fn max_clamp(self) -> u32 {
        self.0 & 0x1ffff
    }

    /// Bit 17, unknown.
    pub const fn bit17(self) -> bool {
        (self.0 >> 17) & 1 != 0
    }

    /// Bits 23:18, established.
    pub const fn wrap_aux(self) -> u32 {
        (self.0 >> 18) & 0x3f
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `MODE_W2` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW2(u32);

impl ModeW2 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 16:0, established.
    pub const fn max_clamp(self) -> u32 {
        self.0 & 0x1ffff
    }

    /// Bit 17, unknown.
    pub const fn bit17(self) -> bool {
        (self.0 >> 17) & 1 != 0
    }

    /// Bits 23:18, established.
    pub const fn wrap_aux(self) -> u32 {
        (self.0 >> 18) & 0x3f
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `MODE_W3` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW3(u32);

impl ModeW3 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 16:0, established.
    pub const fn max_clamp(self) -> u32 {
        self.0 & 0x1ffff
    }

    /// Bit 17, unknown.
    pub const fn bit17(self) -> bool {
        (self.0 >> 17) & 1 != 0
    }

    /// Bits 23:18, established.
    pub const fn wrap_aux(self) -> u32 {
        (self.0 >> 18) & 0x3f
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `WRAPAUX` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wrapaux(u32);

impl Wrapaux {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 5:0, established.
    pub const fn value(self) -> WrapauxValue {
        WrapauxValue::from_bits(self.0 & 0x3f)
    }
}

/// The `MODE_W5` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW5(u32);

impl ModeW5 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, established.
    pub const fn green(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, established.
    pub const fn red(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bits 31:24, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 24
    }
}

/// The `MODE_W6` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeW6(u32);

impl ModeW6 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, established.
    pub const fn alpha(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, established.
    pub const fn blue(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bit 24, established.
    pub const fn has_offset(self) -> bool {
        (self.0 >> 24) & 1 != 0
    }

    /// Bits 31:25, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 25
    }
}

/// The `LOD` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lod(u32);

impl Lod {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 13:0, established.
    pub const fn value(self) -> u32 {
        self.0 & 0x3fff
    }
}

/// The `FIRSTLEVEL` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firstlevel(u32);

impl Firstlevel {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 5:0, established.
    pub const fn index(self) -> u32 {
        self.0 & 0x3f
    }

    /// Bits 31:6, unknown.
    pub const fn unwritten(self) -> u32 {
        self.0 >> 6
    }
}

/// The `TARGET` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target(u32);

impl Target {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 3, established.
    pub const fn dim1(self) -> bool {
        (self.0 >> 3) & 1 != 0
    }

    /// Bit 4, established.
    pub const fn dim2(self) -> bool {
        (self.0 >> 4) & 1 != 0
    }

    /// Bit 5, established.
    pub const fn dim3(self) -> bool {
        (self.0 >> 5) & 1 != 0
    }

    /// Bit 10, established.
    pub const fn active(self) -> bool {
        (self.0 >> 10) & 1 != 0
    }

    /// Bit 11, unknown.
    pub const fn bit11(self) -> bool {
        (self.0 >> 11) & 1 != 0
    }
}

/// The `LEVEL3D_W2` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level3dW2(u32);

impl Level3dW2 {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, established.
    pub const fn slice_exp(self) -> u32 {
        self.0 & 0xfff
    }
}

/// The `FMT` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fmt(u32);

impl Fmt {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 5:0, established.
    pub const fn index(self) -> FmtIndex {
        FmtIndex::from_bits(self.0 & 0x3f)
    }
}

/// The `CLASS` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class(u32);

impl Class {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, established.
    pub const fn value(self) -> ClassValue {
        ClassValue::from_bits(self.0 & 0x7)
    }
}

/// The `SIZE` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size(u32);

impl Size {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 1:0, established.
    pub const fn code(self) -> SizeCode {
        SizeCode::from_bits(self.0 & 0x3)
    }
}

/// The `OFFCODE` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offcode(u32);

impl Offcode {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, established.
    pub const fn value(self) -> OffcodeValue {
        OffcodeValue::from_bits(self.0 & 0x7)
    }
}

/// The `COLTAB_A` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColtabA(u32);

impl ColtabA {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, established.
    pub const fn green(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, established.
    pub const fn red(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bits 31:24, unknown.
    pub const fn kept(self) -> u32 {
        self.0 >> 24
    }
}

/// The `COLTAB_B` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColtabB(u32);

impl ColtabB {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 11:0, established.
    pub const fn alpha(self) -> u32 {
        self.0 & 0xfff
    }

    /// Bits 23:12, established.
    pub const fn blue(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bits 31:24, unknown.
    pub const fn kept(self) -> u32 {
        self.0 >> 24
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testvec_env_w1_ffffff() {
        let w = EnvW1::new(0xffffff);
        assert_eq!(w.red(), 4095, "red");
        assert_eq!(w.green(), 4095, "green");
    }

    #[test]
    fn testvec_env_w2_ffffff() {
        let w = EnvW2::new(0xffffff);
        assert_eq!(w.blue(), 4095, "blue");
        assert_eq!(w.alpha(), 4095, "alpha");
    }

    #[test]
    fn testvec_lod_3100() {
        let w = Lod::new(0x3100);
        assert_eq!(w.value(), 12544, "value");
    }

    #[test]
    fn testvec_lod_f00() {
        let w = Lod::new(0xf00);
        assert_eq!(w.value(), 3840, "value");
    }
}
