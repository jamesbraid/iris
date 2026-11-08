// generated from odsy 865e0c278a6f PBJ_DECODE.h - do not edit, regenerate in odsy
//!
//! Host-interface, I2C and scanout-buffer registers of the Odyssey board,
//! and the header word that frames a display FIFO write.
//!
//! Grades on each item say what is known about it: `established` is what the
//! shipped driver software does, read in full; `inferred` is a reasoned
//! reading; `unknown` is not derived. None of them is a measurement of the
//! silicon.

#![allow(dead_code)]

/// established.
pub const FIFO_DEPTH_DWORDS: u32 = 24;
/// established.
pub const READ_SLOT_BYTES: u32 = 8;
/// established.
pub const ADDR_BITS: u32 = 14;

/// Display-space register `hif_ctl`: read/write, 32-bit; reset unknown; established.
pub const REG_HIF_CTL: u32 = 0x2700;
pub const REG_HIF_CTL_WIDTH: u32 = 32;
/// Display-space register `id`: read, 32-bit; reset unknown; established.
pub const REG_ID: u32 = 0x2701;
pub const REG_ID_WIDTH: u32 = 32;
/// Display-space register `i2c_opt_ctl`: read/write, 32-bit; reset unknown; established.
pub const REG_I2C_OPT_CTL: u32 = 0x2740;
pub const REG_I2C_OPT_CTL_WIDTH: u32 = 32;
/// Display-space register `i2c_opt_data`: read, 32-bit; reset unknown; established.
pub const REG_I2C_OPT_DATA: u32 = 0x2741;
pub const REG_I2C_OPT_DATA_WIDTH: u32 = 32;
/// Display-space register `i2c_main_ctl`: read/write, 32-bit; reset unknown; established.
pub const REG_I2C_MAIN_CTL: u32 = 0x2780;
pub const REG_I2C_MAIN_CTL_WIDTH: u32 = 32;
/// Display-space register `i2c_main_data`: read, 32-bit; reset unknown; established.
pub const REG_I2C_MAIN_DATA: u32 = 0x2781;
pub const REG_I2C_MAIN_DATA_WIDTH: u32 = 32;
/// Display-space register `fb_base_b`: write, 32-bit; reset unknown; established.
pub const REG_FB_BASE_B: u32 = 0x2800;
pub const REG_FB_BASE_B_WIDTH: u32 = 32;
/// Display-space register `fb_base_a`: write, 32-bit; reset unknown; established.
pub const REG_FB_BASE_A: u32 = 0x2801;
pub const REG_FB_BASE_A_WIDTH: u32 = 32;
/// Display-space register `scan_width`: write, 32-bit; reset unknown; inferred.
pub const REG_SCAN_WIDTH: u32 = 0x2802;
pub const REG_SCAN_WIDTH_WIDTH: u32 = 32;
/// Display-space register `scan_height`: write, 32-bit; reset unknown; inferred.
pub const REG_SCAN_HEIGHT: u32 = 0x2803;
pub const REG_SCAN_HEIGHT_WIDTH: u32 = 32;
/// Display-space register `scan_cfg_a`: write, 32-bit; reset unknown; established.
pub const REG_SCAN_CFG_A: u32 = 0x2804;
pub const REG_SCAN_CFG_A_WIDTH: u32 = 32;
/// Display-space register `scan_cfg_b`: write, 32-bit; reset unknown; unknown.
pub const REG_SCAN_CFG_B: u32 = 0x2805;
pub const REG_SCAN_CFG_B_WIDTH: u32 = 32;
/// Display-space register `scan_cfg_c`: write, 32-bit; reset unknown; established.
pub const REG_SCAN_CFG_C: u32 = 0x2806;
pub const REG_SCAN_CFG_C_WIDTH: u32 = 32;
/// Display-space register `fb_pitch`: write, 32-bit; reset unknown; inferred.
pub const REG_FB_PITCH: u32 = 0x2807;
pub const REG_FB_PITCH_WIDTH: u32 = 32;
/// Display-space register `fb_depth`: write, 32-bit; reset unknown; inferred.
pub const REG_FB_DEPTH: u32 = 0x2809;
pub const REG_FB_DEPTH_WIDTH: u32 = 32;
/// Display-space register `dcb_ctl`: write, 32-bit; reset unknown; inferred.
pub const REG_DCB_CTL: u32 = 0x2b00;
pub const REG_DCB_CTL_WIDTH: u32 = 32;

/// Values of `type` in `FifoHdr` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FifoHdrType {
    /// inferred.
    Now,
    /// inferred.
    Synced,
    /// A value the notes do not name.
    Other(u8),
}

impl FifoHdrType {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x3 => Self::Now,
            0x7 => Self::Synced,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::Now => 0x3,
            Self::Synced => 0x7,
            Self::Other(v) => v as u32,
        }
    }
}

/// Values of `code` in `FbDepth` that the header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbDepthCode {
    /// established.
    Bpp8,
    /// established.
    Bpp16,
    /// established.
    Bpp32,
    /// A value the notes do not name.
    Other(u8),
}

impl FbDepthCode {
    pub const fn from_bits(v: u32) -> Self {
        match v {
            0x2 => Self::Bpp8,
            0x3 => Self::Bpp16,
            0x4 => Self::Bpp32,
            _ => Self::Other(v as u8),
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Self::Bpp8 => 0x2,
            Self::Bpp16 => 0x3,
            Self::Bpp32 => 0x4,
            Self::Other(v) => v as u32,
        }
    }
}

/// The `FIFO_HDR` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FifoHdr(u32);

impl FifoHdr {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 13:0, established.
    pub const fn count(self) -> u32 {
        self.0 & 0x3fff
    }

    /// Bits 27:14, established.
    pub const fn address(self) -> u32 {
        (self.0 >> 14) & 0x3fff
    }

    /// Bits 31:28, established.
    pub const fn type_(self) -> FifoHdrType {
        FifoHdrType::from_bits(self.0 >> 28)
    }
}

/// The `hif_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HifCtl(u32);

impl HifCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 18, established.
    pub const fn clock_run(self) -> bool {
        (self.0 >> 18) & 1 != 0
    }

    /// Bits 17:15, established.
    pub const fn clock_sel_b(self) -> u32 {
        (self.0 >> 15) & 0x7
    }

    /// Bits 14:12, established.
    pub const fn clock_sel_a(self) -> u32 {
        (self.0 >> 12) & 0x7
    }

    /// Bits 11:0, unknown.
    pub const fn low_bits(self) -> u32 {
        self.0 & 0xfff
    }
}

/// The `id` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Id(u32);

impl Id {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, established.
    pub const fn version(self) -> u32 {
        self.0 & 0x7
    }

    /// Bit 10, inferred.
    pub const fn no_option(self) -> bool {
        (self.0 >> 10) & 1 != 0
    }
}

/// The `i2c_opt_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cOptCtl(u32);

impl I2cOptCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 8, unknown.
    pub const fn bit8(self) -> bool {
        (self.0 >> 8) & 1 != 0
    }

    /// Bit 10, unknown.
    pub const fn bit10(self) -> bool {
        (self.0 >> 10) & 1 != 0
    }

    /// Bit 12, unknown.
    pub const fn bit12(self) -> bool {
        (self.0 >> 12) & 1 != 0
    }
}

/// The `i2c_opt_data` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cOptData(u32);

impl I2cOptData {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 9, unknown.
    pub const fn bit9(self) -> bool {
        (self.0 >> 9) & 1 != 0
    }
}

/// The `i2c_main_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cMainCtl(u32);

impl I2cMainCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 8, unknown.
    pub const fn bit8(self) -> bool {
        (self.0 >> 8) & 1 != 0
    }

    /// Bit 10, unknown.
    pub const fn bit10(self) -> bool {
        (self.0 >> 10) & 1 != 0
    }

    /// Bit 12, unknown.
    pub const fn bit12(self) -> bool {
        (self.0 >> 12) & 1 != 0
    }
}

/// The `i2c_main_data` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cMainData(u32);

impl I2cMainData {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bit 9, unknown.
    pub const fn bit9(self) -> bool {
        (self.0 >> 9) & 1 != 0
    }
}

/// The `fb_depth` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbDepth(u32);

impl FbDepth {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, inferred.
    pub const fn code(self) -> FbDepthCode {
        FbDepthCode::from_bits(self.0 & 0x7)
    }
}

/// The `dcb_ctl` register word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DcbCtl(u32);

impl DcbCtl {
    pub const fn new(word: u32) -> Self {
        Self(word)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Bits 2:0, inferred.
    pub const fn dcb_clock(self) -> u32 {
        self.0 & 0x7
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testvec_fifo_hdr_38000400() {
        let w = FifoHdr::new(0x38000400);
        assert_eq!(w.count(), 1024, "count");
        assert_eq!(w.address(), 0x2000, "address=@DISPLAY.line_table");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_39000080() {
        let w = FifoHdr::new(0x39000080);
        assert_eq!(w.count(), 128, "count");
        assert_eq!(w.address(), 0x2400, "address=@DISPLAY.frame_table");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_39400080() {
        let w = FifoHdr::new(0x39400080);
        assert_eq!(w.count(), 128, "count");
        assert_eq!(w.address(), 0x2500, "address=@DISPLAY.cursor_image_a");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_39600001() {
        let w = FifoHdr::new(0x39600001);
        assert_eq!(w.count(), 1, "count");
        assert_eq!(w.address(), 0x2580, "address=@DISPLAY.cursor_pos");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_79600001() {
        let w = FifoHdr::new(0x79600001);
        assert_eq!(w.count(), 1, "count");
        assert_eq!(w.address(), 0x2580, "address=@DISPLAY.cursor_pos");
        assert_eq!(w.type_().bits(), 7, "type");
    }

    #[test]
    fn testvec_fifo_hdr_79604001() {
        let w = FifoHdr::new(0x79604001);
        assert_eq!(w.count(), 1, "count");
        assert_eq!(w.address(), 0x2581, "address=@DISPLAY.cursor_ctl");
        assert_eq!(w.type_().bits(), 7, "type");
    }

    #[test]
    fn testvec_fifo_hdr_3964000f() {
        let w = FifoHdr::new(0x3964000f);
        assert_eq!(w.count(), 15, "count");
        assert_eq!(w.address(), 0x2590, "address=@DISPLAY.cursor_colors");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_39700040() {
        let w = FifoHdr::new(0x39700040);
        assert_eq!(w.count(), 64, "count");
        assert_eq!(w.address(), 0x25c0, "address=@DISPLAY.cursor_image_b");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_fifo_hdr_39b00001() {
        let w = FifoHdr::new(0x39b00001);
        assert_eq!(w.count(), 1, "count");
        assert_eq!(w.address(), 0x26c0, "address=@DISPLAY.dac_ctl");
        assert_eq!(w.type_().bits(), 3, "type");
    }

    #[test]
    fn testvec_pbj_hif_ctl_6d3e0() {
        let w = HifCtl::new(0x6d3e0);
        assert_eq!(w.clock_run() as u32, 1, "clock_run");
        assert_eq!(w.clock_sel_b(), 5, "clock_sel_b");
        assert_eq!(w.clock_sel_a(), 5, "clock_sel_a");
        assert_eq!(w.low_bits(), 992, "low_bits");
    }
}
