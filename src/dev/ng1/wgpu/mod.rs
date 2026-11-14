//! Newport's display composed on the host GPU: the GL compositor's work
//! (`gl_compositor.rs`) and the software one's (`SwCompositor`) as a kernel
//! on the common scan-out driver (`crate::wgpu::scanout`), so the wgpu
//! window renderer composes REX3 frames on the device it presents with.
//!
//! One scan-out serves every raster. It reads two plane buffers
//! ([`Ng1Planes`]): the CPU raster uploads its planes into them (whole
//! planes, or the row bands that changed under `rex-dirty-upload`), and the
//! test-only resident raster draws into its own.

use bytemuck::{Pod, Zeroable};

use crate::dev::ng1::screen::Ng1DisplaySource;
use crate::dev::ng1::vc2::{
    VC2_CTRL_CURSOR_EN, VC2_CTRL_CURSOR_SIZE, VC2_REG_CURRENT_CURSOR_X, VC2_REG_CURSOR_ENTRY_PTR, VC2_REG_DISPLAY_CONTROL,
    VC2_REG_WORKING_CURSOR_Y,
};
use crate::wgpu::shader::Fragment;

pub(crate) mod direct;
pub use direct::{Ng1Planes, Ng1Scanout};

#[cfg(test)]
pub(crate) mod resident;

#[cfg(test)]
pub(crate) mod resident_scanout;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod direct_tests;

const SCANOUT: Fragment = Fragment::new("ng1_scanout.wgsl", include_str!("shaders/scanout.wgsl"));

/// The VC2's RAM in 16-bit words.
const VC2_WORDS: usize = 32768;

/// A frame's registers and tables (`shaders/scanout.wgsl`, `Ng1Slot`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Ng1Slot {
    head: [u32; 16],
    xmap: [u32; 32],
    ramdac: [u32; 256],
    cmap: [u32; 8192],
}

/// The slot's first four words: where the cursor's hot spot is and what
/// shape it has.
fn cursor_header(r: &[u16; 32], cursor_x_adjust: i32) -> [u32; 4] {
    let ctrl = r[VC2_REG_DISPLAY_CONTROL as usize];
    [
        ((r[VC2_REG_CURRENT_CURSOR_X as usize] as i32) - 31 + cursor_x_adjust) as u32,
        ((r[VC2_REG_WORKING_CURSOR_Y as usize] as i32) - 31) as u32,
        r[VC2_REG_CURSOR_ENTRY_PTR as usize] as u32,
        ((ctrl & VC2_CTRL_CURSOR_EN != 0) as u32) | ((ctrl & VC2_CTRL_CURSOR_SIZE != 0) as u32) << 1,
    ]
}

impl Ng1Slot {
    fn of_display(src: &Ng1DisplaySource<'_>) -> Ng1Slot {
        let mut head = [0u32; 16];
        head[..4].copy_from_slice(&cursor_header(src.vc2_regs, src.cursor_x_adjust));
        head[4] = src.xmap_cursor_cmap as u32;
        head[5] = src.xmap_popup_cmap as u32;
        head[6] = (src.topscan + 1) as u32;
        head[7] = src.vc2_ram.len().min(VC2_WORDS) as u32;
        Ng1Slot { head, xmap: *src.xmap_mode, ramdac: *src.ramdac, cmap: *src.cmap }
    }
}
