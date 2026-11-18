//! The HQ4 microcode's differences from the Indigo2's, as the engine's
//! command handling needs them.

use super::gl::Gl;
use super::hq3::Hq3Board;

/// The HQ4 microcode's differences the engine has to know about.
pub struct Hq4Quirks;

impl Hq3Board for Hq4Quirks {
    /// The HQ4 microcode's GE state sits 0x8C words above the Indigo2
    /// microcode's from (at the latest) the polygon-stipple enables on:
    /// every address known in both layouts from there up (enables, stencil,
    /// depth, raster position, clear values, viewport, scissor, fog, blend,
    /// lighting, the matrix-stack indices and their bases) differs by exactly
    /// that, while the matrix-mode index, the matrices and the modelview
    /// stack index, all below 0x84, do not move. Where between 0x84 and
    /// 0x1FA the shift starts is not known; this shifts every Indigo2
    /// address from 0x84 up. An approximation, inferred from the two
    /// layouts' known addresses. Pixel pipeline words answer where they
    /// were loaded. The colour matrix (`0x1B9-0x1C8`) does not move
    /// either: the 6.5.22 Octane libGLcore reads it where the Indigo2's
    /// does (glstate `pixel/COLOR_MATRIX_SGI`), and no Indigo2 state sits
    /// 0x8C below it.
    fn ge_state(&self, gl: &mut Gl, addr: u32) -> Option<u32> {
        const SHIFT: u32 = 0x8C;
        const COLOR_MATRIX: std::ops::RangeInclusive<u32> = 0x1B9..=0x1C8;
        let hq3 = if addr >= 0x84 + SHIFT && !COLOR_MATRIX.contains(&addr) { addr - SHIFT } else { addr };
        // Pixel pipeline words are kept in the Indigo2 layout (see
        // `pixel_state_address`).
        let v = gl.state_word_in(hq3, self.pixel_state_address(addr))?;
        // The matrix-stack depths answer with an address in the GE's stack,
        // which moved with the rest.
        Some(if matches!(hq3, 0x83 | 0x896 | 0x897) { v + SHIFT } else { v })
    }

    /// That the register field is nine bits is an inference, not a
    /// published width: every raster register name known runs to 0x1FF, and
    /// the direct packets' decode used for the Octane's port takes the bit
    /// above as part of the packet class. The IP30 PROM and the IP30 kernel
    /// both send one such word (0x001A6804, decoded as a write of 0 to
    /// register 0x268, which no datasheet names); the kernel's copies can
    /// arrive after a context switch has started. What the board does with
    /// it is unknown, so it is dropped and reported.
    fn drop_raster_write(&self, cmd: u32) -> Option<String> {
        (cmd & 0x200 != 0).then(|| format!("raster command {cmd:#x} past register 0x1FF dropped"))
    }

    /// The pixel pipeline's ERAM blocks moved up by the same 0x8C: libGLcore
    /// on the 6.5.22 Octane writes the expand state, the transfer size and
    /// the texture-load blocks at 0xD91, 0xE34 and 0xE43 where the Indigo2's
    /// uses 0xD05, 0xDA8 and 0xDB7 (traced, pixtest). GE memory below the
    /// ERAM (the program at 0x13B, the module states at 0x187-0x1EF, the
    /// zoom at 0x191) did not move. Without this the HLE missed the host
    /// pixel shape, and glDrawPixels' rows reached the raster engine as
    /// RGBA when the host sent RGB.
    fn pixel_state_address(&self, addr: u32) -> u32 {
        const SHIFT: u32 = 0x8C;
        if addr >= super::pixpipe::ERAM_BASE + SHIFT { addr - SHIFT } else { addr }
    }
}
