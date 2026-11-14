//! GR2 display thread: vertical retrace, composition, presentation.
//!
//! Every ~16.7 ms the thread composes VRAM into `screen.fb_rgb` (`prebuilt`)
//! when something visible changed (on the GPU, with `[graphics] raster =
//! "wgpu"`: see `scanout.rs`), or on a periodic heartbeat for the status
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
        // The canvas lags the last frame shown from the GPU.
        let mut canvas_stale = false;

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

                let recompose = dirty || !sized;
                // On the GPU the frame is composed there. A window on the
                // same device shows it from there; anything else reads it
                // back into the canvas. A GPU that fails has stopped the
                // machine: no frame comes from the CPU in its place.
                let Ok(on_gpu) = self.gpu_compose(recompose) else {
                    screen.counts.count(false);
                    return false;
                };
                #[cfg(feature = "wgpu")]
                if on_gpu && need.gpu && !need.words {
                    self.present_on_gpu(screen, &mut sized, &mut overlay, &mut status_bar, &mut sbtex, &stats);
                    canvas_stale = true;
                    screen.counts.count(true);
                    return false;
                }

                let mut canvas = self.screen.lock();
                if on_gpu {
                    if (recompose || need.words || canvas_stale) && !self.gpu_read(&mut canvas.fb_rgb) {
                        screen.counts.count(false);
                        return false;
                    }
                } else if recompose {
                    let r = self.regs();
                    gr2comp::compose(self.vram(), &r.vc1, &r.xmap, &r.dac, &mut canvas.fb_rgb);
                }
                canvas_stale = false;
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

    /// With the GPU scan-out: compose the frame there if `recompose`.
    /// Ok(false) when the board has none (the CPU compose runs), Err when
    /// the one it has failed: the device has been told, and stops the
    /// machine.
    pub(super) fn gpu_compose(&self, recompose: bool) -> Result<bool, ()> {
        #[cfg(feature = "wgpu")]
        {
            // SAFETY: the scan-out is the display thread's, and this runs there.
            let Some(sc) = unsafe { &mut *self.scanout.get() }.as_mut() else { return Ok(false) };
            if !recompose && sc.composed() {
                return Ok(true);
            }
            let r = self.regs();
            return match sc.compose(self.vram(), &r.vc1, &r.xmap, &r.dac) {
                Ok(()) => Ok(true),
                Err(e) => Err(sc.fail(&e)),
            };
        }
        #[cfg(not(feature = "wgpu"))]
        {
            let _ = recompose;
            Ok(false)
        }
    }

    /// Read the GPU's last frame into the canvas. False if that failed
    /// (reported to the device, as in `gpu_compose`).
    fn gpu_read(&self, fb_rgb: &mut [u32]) -> bool {
        #[cfg(feature = "wgpu")]
        {
            // SAFETY: as `gpu_compose`.
            let Some(sc) = unsafe { &mut *self.scanout.get() }.as_mut() else { return false };
            return match sc.read(fb_rgb, gr2comp::OUT_STRIDE) {
                Ok(()) => true,
                Err(e) => {
                    sc.fail(&e);
                    false
                }
            };
        }
        #[cfg(not(feature = "wgpu"))]
        {
            let _ = fb_rgb;
            false
        }
    }

    /// Present the frame composed on the GPU from there, on the renderer's
    /// device: nothing is read back and the canvas keeps its last frame.
    #[cfg(feature = "wgpu")]
    fn present_on_gpu(
        &self,
        screen: &crate::gfx_display::Screen,
        sized: &mut bool,
        overlay: &mut crate::debug_overlay::DebugOverlay,
        status_bar: &mut crate::disp::StatusBar,
        sbtex: &mut crate::disp::StatusBarTexture,
        stats: &crate::disp::BarStats,
    ) {
        // SAFETY: as `gpu_compose`.
        let Some(sc) = (unsafe { &*self.scanout.get() }).as_ref() else { return };
        let mut canvas = self.screen.lock();
        canvas.prebuilt = true;
        canvas.width = FB_W;
        canvas.height = FB_H;
        canvas.status_bar_only = false;
        if let Some(r) = screen.window.renderer.lock().as_mut() {
            if !*sized {
                r.resize(FB_W, FB_H);
            }
            r.present_gpu_frame(sc.output(), FB_W as u32, FB_H as u32, &mut canvas, overlay, status_bar, sbtex, stats);
        }
        *sized = true;
    }
}
