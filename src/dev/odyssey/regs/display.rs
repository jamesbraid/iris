// generated from odsy 865e0c278a6f DISPLAY_DECODE.h - do not edit, regenerate in odsy
//!
//! Display-half registers of the Odyssey board: colour map, line and frame
//! tables, cursor, DAC, timing generator, genlock and the display-mode table.
//!
//! Grades on each item say what is known about it: `established` is what the
//! shipped driver software does, read in full; `inferred` is a reasoned
//! reading; `unknown` is not derived. None of them is a measurement of the
//! silicon.

#![allow(dead_code)]

/// Display-space register `cmap`: write, 32-bit, 8192 consecutive registers; reset unknown; established.
pub const REG_CMAP: u32 = 0x0000;
pub const REG_CMAP_WIDTH: u32 = 32;
pub const REG_CMAP_COUNT: u32 = 8192;
/// Display-space register `line_table`: write, 32-bit, 1024 consecutive registers; reset unknown; established.
pub const REG_LINE_TABLE: u32 = 0x2000;
pub const REG_LINE_TABLE_WIDTH: u32 = 32;
pub const REG_LINE_TABLE_COUNT: u32 = 1024;
/// Display-space register `frame_table`: write, 32-bit, 128 consecutive registers; reset unknown; established.
pub const REG_FRAME_TABLE: u32 = 0x2400;
pub const REG_FRAME_TABLE_WIDTH: u32 = 32;
pub const REG_FRAME_TABLE_COUNT: u32 = 128;
/// Display-space register `vtg_ctl`: write, 32-bit; reset unknown; established.
pub const REG_VTG_CTL: u32 = 0x2481;
pub const REG_VTG_CTL_WIDTH: u32 = 32;
/// Display-space register `vtg_init_state`: read/write, 32-bit; reset unknown; established.
pub const REG_VTG_INIT_STATE: u32 = 0x2482;
pub const REG_VTG_INIT_STATE_WIDTH: u32 = 32;
/// Display-space register `vtg_chan_en`: read/write, 32-bit; reset unknown; established.
pub const REG_VTG_CHAN_EN: u32 = 0x2483;
pub const REG_VTG_CHAN_EN_WIDTH: u32 = 32;
/// Display-space register `cursor_image_a`: write, 32-bit, 128 consecutive registers; reset unknown; established.
pub const REG_CURSOR_IMAGE_A: u32 = 0x2500;
pub const REG_CURSOR_IMAGE_A_WIDTH: u32 = 32;
pub const REG_CURSOR_IMAGE_A_COUNT: u32 = 128;
/// Display-space register `cursor_pos`: write, 32-bit; reset unknown; established.
pub const REG_CURSOR_POS: u32 = 0x2580;
pub const REG_CURSOR_POS_WIDTH: u32 = 32;
/// Display-space register `cursor_ctl`: write, 32-bit; reset unknown; established.
pub const REG_CURSOR_CTL: u32 = 0x2581;
pub const REG_CURSOR_CTL_WIDTH: u32 = 32;
/// Display-space register `cursor_colors`: write, 32-bit, 15 consecutive registers; reset unknown; established.
pub const REG_CURSOR_COLORS: u32 = 0x2590;
pub const REG_CURSOR_COLORS_WIDTH: u32 = 32;
pub const REG_CURSOR_COLORS_COUNT: u32 = 15;
/// Display-space register `cursor_image_b`: write, 32-bit, 64 consecutive registers; reset unknown; established.
pub const REG_CURSOR_IMAGE_B: u32 = 0x25c0;
pub const REG_CURSOR_IMAGE_B_WIDTH: u32 = 32;
pub const REG_CURSOR_IMAGE_B_COUNT: u32 = 64;
/// Display-space register `datapath_ctl`: read/write, 32-bit; reset unknown; inferred.
pub const REG_DATAPATH_CTL: u32 = 0x2600;
pub const REG_DATAPATH_CTL_WIDTH: u32 = 32;
/// Display-space register `aux_ctl`: write, 32-bit; reset unknown; established.
pub const REG_AUX_CTL: u32 = 0x2680;
pub const REG_AUX_CTL_WIDTH: u32 = 32;
/// Display-space register `dac_ctl`: read/write, 32-bit; reset unknown; established.
pub const REG_DAC_CTL: u32 = 0x26c0;
pub const REG_DAC_CTL_WIDTH: u32 = 32;
/// Display-space register `vtg_misc`: write, 32-bit, 12 consecutive registers; reset unknown; established.
pub const REG_VTG_MISC: u32 = 0x27c0;
pub const REG_VTG_MISC_WIDTH: u32 = 32;
pub const REG_VTG_MISC_COUNT: u32 = 12;
/// Display-space register `genlock_ctl`: read/write, 32-bit; reset unknown; established.
pub const REG_GENLOCK_CTL: u32 = 0x27cc;
pub const REG_GENLOCK_CTL_WIDTH: u32 = 32;
/// Display-space register `genlock_status`: read, 32-bit; reset unknown; inferred.
pub const REG_GENLOCK_STATUS: u32 = 0x27cd;
pub const REG_GENLOCK_STATUS_WIDTH: u32 = 32;
/// Display-space register `mode_table`: write, 32-bit, 37 consecutive registers; reset unknown; established.
pub const REG_MODE_TABLE: u32 = 0x2900;
pub const REG_MODE_TABLE_WIDTH: u32 = 32;
pub const REG_MODE_TABLE_COUNT: u32 = 37;
/// Display-space register `cmap_unit_ctl`: write, 32-bit; reset unknown; inferred.
pub const REG_CMAP_UNIT_CTL: u32 = 0x2929;
pub const REG_CMAP_UNIT_CTL_WIDTH: u32 = 32;
/// Display-space register `cmap_override`: write, 32-bit, 3 consecutive registers; reset unknown; inferred.
pub const REG_CMAP_OVERRIDE: u32 = 0x292a;
pub const REG_CMAP_OVERRIDE_WIDTH: u32 = 32;
pub const REG_CMAP_OVERRIDE_COUNT: u32 = 3;

/// Values of `cmap_group` in `ModeTable` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeTableCmapGroup {
    /// inferred.
    Gamma,
    /// A value the notes do not name.
    Other(u8),
}

impl ModeTableCmapGroup {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x1a => Self::Gamma,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::Gamma => 0x1a,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `pixel_format` in `ModeTable` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeTablePixelFormat {
    /// inferred.
    Rgb10,
    /// inferred.
    Rgb8,
    /// inferred.
    Rgb5,
    /// inferred.
    Rgb4,
    /// inferred.
    Rgb10Alt,
    /// A value the notes do not name.
    Other(u8),
}

impl ModeTablePixelFormat {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x1 => Self::Rgb10,
            0x2 => Self::Rgb8,
            0x3 => Self::Rgb5,
            0x4 => Self::Rgb4,
            0x6 => Self::Rgb10Alt,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::Rgb10 => 0x1,
            Self::Rgb8 => 0x2,
            Self::Rgb5 => 0x3,
            Self::Rgb4 => 0x4,
            Self::Rgb10Alt => 0x6,
            Self::Other(v) => v as u32,
        }
    }
}

/// The `cmap` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cmap(u32);

impl Cmap {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 29:20, established.
    pub const fn r(self) -> u32 {
        (self.0 >> 20) & 0x3ff
    }

    /// Bits 19:10, established.
    pub const fn g(self) -> u32 {
        (self.0 >> 10) & 0x3ff
    }

    /// Bits 9:0, established.
    pub const fn b(self) -> u32 {
        self.0 & 0x3ff
    }

    /// Bits 31:30, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 30
    }
}

/// The `line_table` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineTable(u32);

impl LineTable {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 15, established.
    pub const fn e0_end(self) -> bool {
        (self.0 >> 15) & 1 != 0
    }

    /// Bits 14:8, inferred.
    pub const fn e0_duration(self) -> u32 {
        (self.0 >> 8) & 0x7f
    }

    /// Bits 7:0, unknown.
    pub const fn e0_levels(self) -> u32 {
        self.0 & 0xff
    }

    /// Bit 31, established.
    pub const fn e1_end(self) -> bool {
        (self.0 >> 31) & 1 != 0
    }

    /// Bits 30:24, inferred.
    pub const fn e1_duration(self) -> u32 {
        (self.0 >> 24) & 0x7f
    }

    /// Bits 23:16, unknown.
    pub const fn e1_levels(self) -> u32 {
        (self.0 >> 16) & 0xff
    }
}

/// The `frame_table` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTable(u32);

impl FrameTable {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 15:0, inferred.
    pub const fn position(self) -> u32 {
        self.0 & 0xffff
    }

    /// Bits 31:16, inferred.
    pub const fn repeat(self) -> u32 {
        self.0 >> 16
    }
}

/// The `vtg_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtgCtl(u32);

impl VtgCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 1, inferred.
    pub const fn locked(self) -> bool {
        (self.0 >> 1) & 1 != 0
    }
}

/// The `vtg_init_state` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtgInitState(u32);

impl VtgInitState {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 5, established.
    pub const fn blank_gate(self) -> bool {
        (self.0 >> 5) & 1 != 0
    }
}

/// The `vtg_chan_en` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtgChanEn(u32);

impl VtgChanEn {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 5, established.
    pub const fn blank_gate_5(self) -> bool {
        (self.0 >> 5) & 1 != 0
    }

    /// Bit 1, established.
    pub const fn blank_gate_1(self) -> bool {
        (self.0 >> 1) & 1 != 0
    }

    /// Bit 0, established.
    pub const fn blank_gate_0(self) -> bool {
        self.0 & 1 != 0
    }
}

/// The `cursor_pos` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPos(u32);

impl CursorPos {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 31:16, established.
    pub const fn x(self) -> u32 {
        self.0 >> 16
    }

    /// Bits 15:0, established.
    pub const fn y(self) -> u32 {
        self.0 & 0xffff
    }
}

/// The `cursor_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorCtl(u32);

impl CursorCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 4, inferred.
    pub const fn dual_head(self) -> bool {
        (self.0 >> 4) & 1 != 0
    }

    /// Bit 3, inferred.
    pub const fn field_seq(self) -> bool {
        (self.0 >> 3) & 1 != 0
    }

    /// Bit 2, inferred.
    pub const fn crosshair(self) -> bool {
        (self.0 >> 2) & 1 != 0
    }

    /// Bit 1, inferred.
    pub const fn enable(self) -> bool {
        (self.0 >> 1) & 1 != 0
    }

    /// Bit 0, inferred.
    pub const fn delay_enable(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 5, unknown.
    pub const fn toggle(self) -> bool {
        (self.0 >> 5) & 1 != 0
    }
}

/// The `cursor_colors` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorColors(u32);

impl CursorColors {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 29:20, established.
    pub const fn r(self) -> u32 {
        (self.0 >> 20) & 0x3ff
    }

    /// Bits 19:10, established.
    pub const fn g(self) -> u32 {
        (self.0 >> 10) & 0x3ff
    }

    /// Bits 9:0, established.
    pub const fn b(self) -> u32 {
        self.0 & 0x3ff
    }

    /// Bits 31:30, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 30
    }
}

/// The `datapath_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatapathCtl(u32);

impl DatapathCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 6, unknown.
    pub const fn bit6(self) -> bool {
        (self.0 >> 6) & 1 != 0
    }

    /// Bit 7, unknown.
    pub const fn bit7(self) -> bool {
        (self.0 >> 7) & 1 != 0
    }
}

/// The `dac_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DacCtl(u32);

impl DacCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 0, established.
    pub const fn blank(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, inferred.
    pub const fn gain(self) -> bool {
        (self.0 >> 1) & 1 != 0
    }
}

/// The `genlock_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenlockCtl(u32);

impl GenlockCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 6, inferred.
    pub const fn enabled(self) -> bool {
        (self.0 >> 6) & 1 != 0
    }
}

/// The `genlock_status` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenlockStatus(u32);

impl GenlockStatus {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 0, inferred.
    pub const fn valid(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, inferred.
    pub const fn lost(self) -> bool {
        (self.0 >> 1) & 1 != 0
    }
}

/// The `mode_table` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeTable(u32);

impl ModeTable {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 3:0, unknown.
    pub const fn low_code(self) -> u32 {
        self.0 & 0xf
    }

    /// Bits 8:4, established.
    pub const fn cmap_group(self) -> ModeTableCmapGroup {
        ModeTableCmapGroup::from_bits((self.0 >> 4) & 0x1f)
    }

    /// Bits 11:9, inferred.
    pub const fn pixel_format(self) -> ModeTablePixelFormat {
        ModeTablePixelFormat::from_bits((self.0 >> 9) & 0x7)
    }

    /// Bits 23:12, unknown.
    pub const fn layout(self) -> u32 {
        (self.0 >> 12) & 0xfff
    }

    /// Bit 24, unknown.
    pub const fn bit24(self) -> bool {
        (self.0 >> 24) & 1 != 0
    }

    /// Bits 31:25, unknown.
    pub const fn top(self) -> u32 {
        self.0 >> 25
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testvec_display_mode_table_4b5f00() {
        let w = ModeTable::new(0x4b5f00);
        assert_eq!(w.low_code(), 0, "low_code");
        assert_eq!(w.cmap_group().bits(), 16, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 7, "pixel_format");
        assert_eq!(w.layout(), 1205, "layout");
    }

    #[test]
    fn testvec_display_mode_table_4b59a6() {
        let w = ModeTable::new(0x4b59a6);
        assert_eq!(w.low_code(), 6, "low_code");
        assert_eq!(w.cmap_group().bits(), 26, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 4, "pixel_format");
        assert_eq!(w.layout(), 1205, "layout");
    }

    #[test]
    fn testvec_display_mode_table_5213a6() {
        let w = ModeTable::new(0x5213a6);
        assert_eq!(w.low_code(), 6, "low_code");
        assert_eq!(w.cmap_group().bits(), 26, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 1, "pixel_format");
        assert_eq!(w.layout(), 1313, "layout");
    }

    #[test]
    fn testvec_display_mode_table_521590() {
        let w = ModeTable::new(0x521590);
        assert_eq!(w.low_code(), 0, "low_code");
        assert_eq!(w.cmap_group().bits(), 25, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 2, "pixel_format");
        assert_eq!(w.layout(), 1313, "layout");
    }

    #[test]
    fn testvec_display_mode_table_600da6() {
        let w = ModeTable::new(0x600da6);
        assert_eq!(w.low_code(), 6, "low_code");
        assert_eq!(w.cmap_group().bits(), 26, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 6, "pixel_format");
        assert_eq!(w.layout(), 1536, "layout");
    }

    #[test]
    fn testvec_display_mode_table_9659a7() {
        let w = ModeTable::new(0x9659a7);
        assert_eq!(w.low_code(), 7, "low_code");
        assert_eq!(w.cmap_group().bits(), 26, "cmap_group");
        assert_eq!(w.pixel_format().bits(), 4, "pixel_format");
        assert_eq!(w.layout(), 2405, "layout");
    }
}
