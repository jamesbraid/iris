//! What the board shows: the scanout's colour lookup and gamma, the retrace
//! with its swap latching, and the window iris presents the frame in.

use super::board::Board;
use super::fifo::display_fifo_pending;
use super::reader::{DisplayShared, ScanoutReader};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

/// The host's monotonic clock in nanoseconds since the first call: the
/// time base of the retrace here.
pub(super) fn monotonic_ns() -> i64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64
}

use crate::cpu::mips_core::CyclesPtr;
use crate::gfx_display::Screen;

/// The window the board shows the guest: what `GfxDisplay` hands out.
pub(super) struct Display {
    pub screens: [Arc<Screen>; 1],
    /// Whether the display thread releases the renderer when it ends.
    pub(super) display_stop: crate::gfx_display::DisplayStop,
    /// What the display keeps of the scanout: the presented frame, the
    /// retrace clock and the update in progress. Held for each frame.
    pub reader: Mutex<ScanoutReader>,
    /// The newest frame published holds words (not only a device frame).
    pub(super) words: AtomicBool,
    /// The clock ran at the start of this tick's frame.
    pub(super) polled: AtomicBool,
    /// The machine's status-bar inputs.
    status: Arc<crate::disp::StatusSource>,
    /// The registers the display reads, for the host's cursor query; set
    /// when the board hands the display out.
    shared: OnceLock<Arc<DisplayShared>>,
}

impl Display {
    pub(super) fn new(status: Arc<crate::disp::StatusSource>) -> Self {
        Self {
            screens: [Arc::new(Screen::default())],
            display_stop: Default::default(),
            reader: Mutex::new(ScanoutReader::default()),
            words: AtomicBool::new(false),
            polled: AtomicBool::new(false),
            status,
            shared: OnceLock::new(),
        }
    }

    /// The window and the consumers' side of the board's screen.
    pub(super) fn out(&self) -> &Arc<Screen> {
        &self.screens[0]
    }

    /// The status bar's inputs for a frame shown now.
    pub(super) fn stats(&self) -> crate::disp::BarStats {
        self.status.snapshot(std::time::Instant::now(), display_fifo_pending())
    }

    /// Read the cursor from `shared` from now on.
    pub(super) fn attach_registers(&self, shared: Arc<DisplayShared>) {
        // A window presents on the device the scan-out composes on.
        #[cfg(feature = "wgpu")]
        if let Some(scanout) = shared.gpu_scanout.get() {
            let _ = self.out().gpu.set(scanout.lock().gpu());
        }
        let _ = self.shared.set(shared);
    }
}

impl Board {
    /// A vertical retrace: the host interface latches the swaps armed and
    /// raises the retrace source (HostInterface::retrace()).
    #[cfg(test)]
    pub fn retrace(&mut self) {
        self.display.retrace();
    }
}

/// The scan-out as the tests and the benchmark run it on a board they hold,
/// without a display thread: the same reader the display uses.
#[cfg(test)]
impl Board {
    fn test_reader(&mut self) -> &mut ScanoutReader {
        self.raster_wait();
        self.reader.attach(&self.display);
        &mut self.reader
    }

    /// Compose the whole frame into `reader.frame`.
    pub fn scanout_update(&mut self) -> bool {
        self.test_reader().update()
    }
}

impl crate::gfx_display::GfxDisplay for Display {
    fn screens(&self) -> &[Arc<Screen>] {
        &self.screens
    }
    fn display_stop(&self) -> &crate::gfx_display::DisplayStop {
        &self.display_stop
    }
    fn cycles(&self) -> CyclesPtr {
        self.status.cycles()
    }
    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        // A racy read of the registers, as the display thread's frame does.
        self.shared.get()?.regs.hw_cursor()
    }
}
