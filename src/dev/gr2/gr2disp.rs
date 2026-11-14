//! GR2 display thread: vertical retrace, composition, presentation.
//!
//! Every ~16.7 ms the thread composes VRAM into `screen.fb_rgb` (`prebuilt`)
//! when something visible changed, or on a periodic heartbeat for the status
//! bar, presents through the installed renderer, and then pulses the retrace
//! line, so a flip the guest makes in the blank shows from the next frame.
//! The IOC latches the retrace until the host acknowledges: PORT_CONFIG on
//! Indigo2, HPC3_GEN_CONTROL on Indy (GR2.h "INTERRUPTS").

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use super::gr2comp;
use super::re3::{FB_H, FB_W};
use super::Gr2;

/// Vertical blanking interval.
const VBLANK: Duration = Duration::from_micros(700);
/// Minimum refresh rate while idle, so the status bar keeps moving.
const IDLE_HEARTBEAT_FRAMES: u32 = 6;

impl Gr2 {
    pub(super) fn display_loop(&self) {
        let screen = &self.screens[0];
        let mut status_bar = crate::disp::StatusBar::new();
        let mut overlay = crate::debug_overlay::DebugOverlay::new();
        let mut sbtex = crate::disp::StatusBarTexture::new();
        let mut frames_since_render = u32::MAX;
        let mut sized = false;

        crate::gfx_display::run_display(
            screen,
            &self.running,
            &self.display_stop,
            // Vertical retrace, after the frame is composed: scanout reads
            // VRAM during the frame, and the guest flips buffers in the
            // blank. Composed after the pulse, the copy raced the flip and
            // the next frame's drawing into the buffer just shown. The
            // interrupt stays latched in the IOC until the guest clears it.
            // The vertical status level (EXTIO SG_STAT_0) is held for the
            // blanking interval (~40 of 1066 lines at 60 Hz), long enough
            // for a polling CPU to see both edges.
            || {
                if let Some(cb) = self.retrace_cb.lock().clone() {
                    cb(true);
                    thread::sleep(VBLANK);
                    cb(false);
                }
                self.trace_flush(false);
            },
            |out, need| {
                let start = Instant::now();
                let dirty = self.dirty.swap(false, Ordering::Acquire);
                if !(dirty || need.words || frames_since_render >= IDLE_HEARTBEAT_FRAMES) {
                    frames_since_render = frames_since_render.saturating_add(1);
                    return false;
                }
                frames_since_render = 0;
                let stats = self.status.snapshot(start, self.hq_fifo.len());

                let mut canvas = self.screen.lock();
                if dirty || !sized {
                    let r = self.regs();
                    gr2comp::compose(self.vram(), &r.vc1, &r.xmap, &r.dac, &mut canvas.fb_rgb);
                }
                canvas.prebuilt = true;
                canvas.width = FB_W;
                canvas.height = FB_H;
                canvas.status_bar_only = !dirty && sized && !need.words;
                // The frame is already final RGB, so a consumer's copy needs no
                // renderer readback. This also makes screenshots work headless.
                let fresh = dirty || !sized || need.words;
                if fresh {
                    out.width = FB_W as u32;
                    out.height = FB_H as u32;
                    out.words.clear();
                    for y in 0..FB_H {
                        let row = y * gr2comp::OUT_STRIDE;
                        out.words.extend_from_slice(&canvas.fb_rgb[row..row + FB_W]);
                    }
                }

                if let Some(r) = screen.window.renderer.lock().as_mut() {
                    if !sized {
                        r.resize(FB_W, FB_H);
                    }
                    r.present(&mut canvas, &mut overlay, &mut status_bar, &mut sbtex, &stats, false, None, None);
                }
                sized = true;
                screen.counts.count(fresh);
                fresh
            },
        );
    }
}
