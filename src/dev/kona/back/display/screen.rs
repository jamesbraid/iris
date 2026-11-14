//! The window the board shows the guest: what `GfxDisplay` hands out.
//!
//! The board owns its screen. Its display thread composes a frame at each
//! retrace and presents it here: the pixels go into the screen's frame buffer
//! (stride 2048, opaque `0xAABBGGRR`) and on to whatever renderer the front end
//! installed in the renderer slot. Screenshot and frame requests ask for the
//! next composed frame. Presenting every frame it is given is this module's
//! whole job; the compositor decides which frames to compose. The machine
//! keeps a clone of the `Arc` and returns it from `get_display`.

use crate::cpu::mips_core::CyclesPtr;
use crate::disp::{Rex3Screen, StatusSource, FB_STRIDE};
use crate::gfx_display::{DisplayStop, GfxDisplay, Screen};
use parking_lot::Mutex;
use std::sync::Arc;


struct Local {
    overlay: crate::debug_overlay::DebugOverlay,
    status: crate::disp::StatusBar,
    texture: crate::disp::StatusBarTexture,
    size: (usize, usize),
}

pub struct Display {
    screens: [Arc<Screen>; 1],
    /// What the installed renderer presents from.
    screen: Mutex<Rex3Screen>,
    display_stop: DisplayStop,
    local: Mutex<Option<Local>>,
    /// The machine's status-bar inputs.
    status: Mutex<Arc<StatusSource>>,
    /// The tables the cursor registers live in, for `hw_cursor`.
    tables: std::sync::OnceLock<Arc<Mutex<super::tables::Tables>>>,
}

impl Display {
    pub fn new(status: Arc<StatusSource>) -> Display {
        Display {
            screens: [Arc::new(Screen::new(0, Arc::default()))],
            screen: Mutex::new(Rex3Screen::new()),
            display_stop: DisplayStop::default(),
            local: Mutex::new(None),
            status: Mutex::new(status),
            tables: std::sync::OnceLock::new(),
        }
    }

    /// Wire in the machine's status-bar inputs.
    pub fn set_status(&self, status: Arc<StatusSource>) {
        *self.status.lock() = status;
    }

    /// Wire in the tables whose cursor registers `hw_cursor` reads.
    pub fn set_tables(&self, tables: Arc<Mutex<super::tables::Tables>>) {
        let _ = self.tables.set(tables);
    }

    /// A display with a status source of its own, for a board no machine has
    /// wired a status bar to.
    pub fn detached() -> Display {
        Display::new(StatusSource::new(Arc::default(), Arc::default()))
    }

    /// The window and the screen's consumers: this board's only screen.
    pub fn out(&self) -> &Arc<Screen> {
        &self.screens[0]
    }

    /// Hand a composed frame to the installed renderer: `rows` are opaque
    /// `0xAABBGGRR` pixels, `width` to a row, the first being the frame's top
    /// line.
    pub fn present(&self, rows: &[u32], width: usize, height: usize) {
        let mut renderer = self.out().window.renderer.lock();
        let Some(renderer) = renderer.as_mut() else { return };
        let mut screen = self.screen.lock();
        let w = width.min(FB_STRIDE);
        let h = height.min(screen.fb_rgb.len() / FB_STRIDE).min(rows.len() / width.max(1));
        screen.width = w;
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        for (y, src) in rows.chunks_exact(width.max(1)).take(h).enumerate() {
            let at = y * FB_STRIDE;
            screen.fb_rgb[at..at + w].copy_from_slice(&src[..w]);
        }
        let mut local = self.local.lock();
        let local = local.get_or_insert_with(|| Local {
            overlay: crate::debug_overlay::DebugOverlay::new(),
            status: crate::disp::StatusBar::new(),
            texture: crate::disp::StatusBarTexture::new(),
            size: (0, 0),
        });
        if local.size != (w, h) {
            renderer.resize(w, h);
            local.size = (w, h);
        }
        let stats = self.status.lock().snapshot(std::time::Instant::now(), 0);
        renderer.present(&mut screen, &mut local.overlay, &mut local.status, &mut local.texture, &stats, false, None, None);
    }

    /// Show a frame composed on the GPU (0xAABBGGRR, rows of `width`) through
    /// the installed renderer, from where it is: nothing comes back, and the
    /// screen's words keep their last frame.
    #[cfg(feature = "wgpu")]
    pub fn present_gpu(&self, frame: &wgpu::Buffer, width: u32, height: u32) {
        let mut renderer = self.out().window.renderer.lock();
        let Some(renderer) = renderer.as_mut() else { return };
        let mut screen = self.screen.lock();
        let (w, h) = (width as usize, height as usize);
        screen.width = w.min(FB_STRIDE);
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        self.out().set_size(width, height);
        let mut local = self.local.lock();
        let local = local.get_or_insert_with(|| Local {
            overlay: crate::debug_overlay::DebugOverlay::new(),
            status: crate::disp::StatusBar::new(),
            texture: crate::disp::StatusBarTexture::new(),
            size: (0, 0),
        });
        if local.size != (w, h) {
            renderer.resize(w, h);
            local.size = (w, h);
        }
        let stats = self.status.lock().snapshot(std::time::Instant::now(), 0);
        renderer.present_gpu_frame(frame, width, height, &mut screen, &mut local.overlay, &mut local.status, &mut local.texture, &stats);
    }

    /// Say which device the board composes on, so a window on it presents the
    /// GPU's frames directly.
    #[cfg(feature = "wgpu")]
    pub fn set_gpu(&self, gpu: crate::SharedGpu) {
        let _ = self.out().gpu.set(gpu);
    }

    /// The display thread's last act: release the renderer, unless the stop came
    /// from the main thread, and drop the per-frame overlay state.
    pub fn finish(&self) {
        self.display_stop.finish(&self.out().window.renderer);
        self.local.lock().take();
    }
}

impl GfxDisplay for Display {
    fn screens(&self) -> &[Arc<Screen>] {
        &self.screens
    }

    fn display_stop(&self) -> &DisplayStop {
        &self.display_stop
    }

    fn cycles(&self) -> CyclesPtr {
        self.status.lock().cycles()
    }

    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        // Read live from the registers, not from the last composed frame.
        self.tables.get()?.lock().snapshot().hw_cursor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame handed to a renderer reaches it at the size given, and a short
    /// or wide one is cut to what the screen holds.
    #[test]
    fn a_wide_frame_is_cut_to_the_screen() {
        struct Sizes(Arc<Mutex<Vec<(usize, usize)>>>);
        impl crate::dev::ng1::rex3::Renderer for Sizes {
            fn present(&mut self, s: &mut Rex3Screen, _: &mut crate::debug_overlay::DebugOverlay, _: &mut crate::disp::StatusBar,
                _: &mut crate::disp::StatusBarTexture, _: &crate::disp::BarStats, _: bool, _: Option<&[u32]>, _: Option<&[u32]>) {
                self.0.lock().push((s.width, s.height));
            }
        }
        let d = Display::detached();
        let seen = Arc::new(Mutex::new(Vec::new()));
        *d.out().window.renderer.lock() = Some(Box::new(Sizes(seen.clone())));
        d.present(&vec![0xff00_0002; 7], 5, 2);
        d.present(&vec![0xff00_0003; 3000], 3000, 1);
        assert_eq!(*seen.lock(), [(5, 1), (FB_STRIDE, 1)]);
    }
}
