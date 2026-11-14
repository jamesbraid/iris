//! The window the board shows the guest: what `GfxDisplay` hands out.
//!
//! The board owns its screen. Its display thread composes a frame at each
//! retrace (opaque `0xAABBGGRR`) and the screen publishes it: to whatever
//! renderer the front end installed in the renderer slot, and to the consumers
//! that ask for the next composed frame. The compositor decides which frames to
//! compose. The machine keeps a clone of the `Arc` and returns it from
//! `get_display`.

use crate::cpu::mips_core::CyclesPtr;
use crate::disp::{BarStats, StatusSource};
use crate::gfx_display::{DisplayStop, GfxDisplay, Screen};
use parking_lot::Mutex;
use std::sync::Arc;


pub struct Display {
    screens: [Arc<Screen>; 1],
    display_stop: DisplayStop,
    /// The machine's status-bar inputs.
    status: Mutex<Arc<StatusSource>>,
    /// The tables the cursor registers live in, for `hw_cursor`.
    tables: std::sync::OnceLock<Arc<Mutex<super::tables::Tables>>>,
}

impl Display {
    pub fn new(status: Arc<StatusSource>) -> Display {
        Display {
            screens: [Arc::new(Screen::new(0, Arc::default()))],
            display_stop: DisplayStop::default(),
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

    /// The status bar's inputs for a frame shown now.
    pub fn stats(&self) -> BarStats {
        self.status.lock().snapshot(std::time::Instant::now(), 0)
    }

    /// Say which device the board composes on, so a window on it presents the
    /// GPU's frames directly.
    #[cfg(feature = "wgpu")]
    pub fn set_gpu(&self, gpu: crate::SharedGpu) {
        let _ = self.out().gpu.set(gpu);
    }

    /// The display thread's last act: release the renderer, unless the stop came
    /// from the main thread.
    pub fn finish(&self) {
        self.display_stop.finish(&self.out().window.renderer);
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
