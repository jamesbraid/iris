//! What the board shows: the scanout's colour lookup and gamma, the retrace
//! with its swap latching, and the window iris presents the frame in.

use super::board::Board;
use super::reader::{DisplayShared, ScanoutReader};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::{Rex3Screen, FB_STRIDE};
use crate::display::{Composed, Head, HeadHandle, Want};

/// The window the board shows the guest: what `GfxDisplay` hands out.
pub(super) struct Display {
    /// The display channel: screen, renderer slot and frame requests, run
    /// by the machine's refresh thread (`crate::display`).
    pub head: Arc<HeadHandle>,
    /// What the display keeps of the scanout: the presented frame and the
    /// update in progress. Held for each frame.
    pub reader: Mutex<ScanoutReader>,
    /// The machine's status-bar inputs.
    status: Arc<crate::status::MachineStatus>,
    /// The registers the display reads, for the host's cursor query; set
    /// when the board hands the display out.
    shared: OnceLock<Arc<DisplayShared>>,
    verify: Mutex<crate::display::verify::Check>,
}

impl Display {
    pub(super) fn new(status: Arc<crate::status::MachineStatus>) -> Self {
        Self {
            head: status.refresh().head(status.clone()),
            reader: Mutex::new(ScanoutReader::default()),
            status,
            shared: OnceLock::new(),
            verify: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn screen(&self) -> &Mutex<Rex3Screen> {
        self.head.screen()
    }

    #[cfg(test)]
    pub(super) fn renderer(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        self.head.renderer_slot()
    }

    /// Read the cursor from `shared` from now on.
    pub(super) fn attach_registers(&self, shared: Arc<DisplayShared>) {
        let _ = self.shared.set(shared);
    }

    /// If anything the scan-out reads has changed, the whole frame composed
    /// from board memory as the scan-out registers describe it (frame.rs).
    /// On the GPU raster unit with the wgpu renderer on the same device it
    /// is composed there and stays there; otherwise it is composed on the
    /// CPU straight into the screen, in its order (the GPU scan-out's
    /// readback comes back in the reader's `frame` and is converted).
    /// The display takes no board lock and waits for no drawing: it reads
    /// memory while the raster unit and the front end write it, as the
    /// board's own scan-out does.
    pub(super) fn compose(&self, shared: &Arc<DisplayShared>, screen: &mut Rex3Screen, want: Want) -> Composed {
        if shared.gpu_fault.lock().is_some() {
            return Composed::NotReady;
        }
        let mut reader = self.reader.lock();
        reader.attach(shared);
        crate::pf_begin!(crate::perfetto::events::ODSY_FRAME_PLANES);
        #[cfg(feature = "wgpu")]
        if want.gpu {
            if let Some((frame, width, height)) = reader.update_on_gpu() {
                crate::pf_end!(crate::perfetto::events::ODSY_FRAME_PLANES);
                screen.width = (width as usize).min(FB_STRIDE);
                screen.height = height as usize;
                screen.prebuilt = true;
                return Composed::Gpu { frame, width, height };
            }
        }
        if shared.gpu_fault.lock().is_some() {
            return Composed::NotReady;
        }
        let _ = want;
        reader.screen_order = true;
        // Checking the unchanged verdicts (`display::verify`): compose a
        // frame the reader would leave, and compare.
        let verify = crate::display::verify::on() && screen.prebuilt && screen.width != 0;
        if verify {
            self.verify.lock().before(screen);
        }
        reader.force = verify;
        let composed = reader.update_to(Some((&mut screen.fb_rgb, FB_STRIDE)));
        if shared.gpu_fault.lock().is_some() {
            return Composed::NotReady;
        }
        if verify && matches!(composed, super::reader::Composed::InTarget) {
            self.verify.lock().after("vpro", screen, !reader.skipped);
        }
        crate::pf_end!(crate::perfetto::events::ODSY_FRAME_PLANES);
        let (width, height) = (reader.frame_width as usize, reader.frame_height as usize);
        if width == 0 || height == 0 {
            return Composed::Unchanged;
        }
        let (w, h) = (width.min(FB_STRIDE), height.min(screen.fb_rgb.len() / FB_STRIDE));
        match composed {
            super::reader::Composed::No => return Composed::Unchanged,
            super::reader::Composed::InTarget => {}
            super::reader::Composed::InFrame => {
                // The GPU scan-out's readback is already the screen's order; a CPU frame
                // in 0x00RRGGBB is a byte swap with alpha set.
                let abgr = reader.frame_abgr;
                for (y, src) in reader.frame[..width * height].chunks_exact(width).enumerate().take(h) {
                    let at = y * FB_STRIDE;
                    for (d, &px) in screen.fb_rgb[at..at + w].iter_mut().zip(&src[..w]) {
                        *d = if abgr { px } else { super::frame::to_screen(px) };
                    }
                }
            }
        }
        screen.width = w;
        screen.height = h;
        screen.prebuilt = true;
        Composed::Frame
    }
}

/// The board on the refresh thread.
pub(super) struct Scan {
    pub state: super::BoardLocks,
    pub display: Arc<Display>,
}

impl Head for Scan {
    /// The retrace: the host interface latches the swaps armed and raises
    /// the retrace source. The frame composed after the blank reads the
    /// buffer this latch displays, so nothing is captured before it.
    fn vblank_start(&self, _field: u32, _t_ns: u64) {
        self.state.trace.flush(false);
        let mut reader = self.display.reader.lock();
        reader.attach(&self.state.shared);
        reader.retrace();
        // The retrace runs at the rate of the mode the guest loaded (its
        // frame and line tables at its clock part's frequency).
        if let Some(timing) = reader.mode.video_timing() {
            self.display.head.set_timing(timing);
        }
        // A mode load or reset restarts the phase at the guest's write, when the shell said when
        // that was (`write_at`).
        if let Some(at) = reader.restarted.take() {
            self.display.head.restart_phase((at >= 0).then_some(at as u64));
        }
    }

    /// What the scan-out reads changes without a mark the display could test for less than `compose`
    /// does, so every frame is looked at while a window, a request or a capturing client reads one.
    /// Headless, the GPU scan-out would otherwise compose and read back sixty times a second for nobody.
    fn changed(&self) -> bool {
        self.display.head.watched()
    }

    fn compose(&self, screen: &mut Rex3Screen, want: Want) -> Composed {
        crate::pf_begin!(crate::perfetto::events::ODSY_CAPTURE);
        let composed = self.display.compose(&self.state.shared, screen, want);
        crate::pf_end!(crate::perfetto::events::ODSY_CAPTURE);
        composed
    }

    fn gfifo_pending(&self) -> usize {
        self.state.fifo.level() as usize + self.state.raster.pending() as usize
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
    fn display_stop(&self) -> &crate::gfx_display::DisplayStop { self.head.display_stop() }
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> { self.head.renderer_slot() }
    fn screen(&self) -> &Mutex<Rex3Screen> { self.head.screen() }
    fn request_screenshot(&self) { self.head.request_screenshot(); }
    fn screenshot_pending(&self) -> bool { self.head.screenshot_pending() }
    fn cycles(&self) -> CyclesPtr { self.status.cycles() }
    fn request_frame(&self) { self.head.request_frame(); }
    fn frame_pending(&self) -> bool { self.head.frame_pending() }
    fn set_capture(&self, on: bool) { self.head.set_capture(on); }
    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        // A racy read of the registers, as the display thread's frame does.
        self.shared.get()?.regs.hw_cursor()
    }
    #[cfg(feature = "wgpu")]
    fn gpu(&self) -> Option<std::sync::Arc<crate::wgpu::Gpu>> {
        Some(self.shared.get()?.gpu_scanout.get()?.lock().gpu())
    }
}
