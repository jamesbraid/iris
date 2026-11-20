#[path = "../../../src/dev/bayer.rs"]
pub mod bayer;
pub mod gl;
pub mod mgras;

/// The three timing constants `disp_timing.rs` takes from NG1's VC2, whose timing-word layout the
/// VC3 shares.
pub mod ng1 {
    pub mod vc2 {
        pub const VC2_REG_VIDEO_ENTRY_PTR: u8 = 0x00;
        pub const VT_CBLANK_XMAP_N: u8 = 1 << 6;
        pub const VT_HPOS_VC_N: u8 = 1 << 1;
    }
}
