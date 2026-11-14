//! GR2 display thread: vertical retrace, composition, presentation.
//!
//! Every ~16.7 ms the thread composes VRAM into the frame when something
//! visible changed (on the GPU, with `[graphics] raster = "wgpu"`: see
//! `scanout.rs`), publishes it to the screen (the screen shows it to the
//! installed renderer and repeats it for the status bar), and then pulses the
//! retrace line, so a flip the guest makes in the blank shows from the next frame.
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
impl Gr2 {
    pub(super) fn display_loop(&self) {
        let screen = &self.screens[0];
        // The compositor's stride is 2048; the frame is cut from it row by row.
        let mut scan = vec![0u32; gr2comp::OUT_STRIDE * FB_H];
        let mut sized = false;
        // `scan` lags the last frame shown from the GPU.
        let mut scan_stale = false;

        crate::gfx_display::run_display(
            screen,
            &self.running,
            &self.display_stop,
            || self.status.snapshot(Instant::now(), self.hq_fifo.len()),
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
                let dirty = self.dirty.swap(false, Ordering::Acquire);
                // Unchanged and nobody asked: the screen shows the last frame
                // again for the status bar.
                if !(dirty || !sized || need.words) {
                    return false;
                }
                let recompose = dirty || !sized;
                // On the GPU the frame is composed there. A window on the
                // same device shows it from there; anything else reads it
                // back. A GPU that fails has stopped the machine: no frame
                // comes from the CPU in its place.
                let Ok(on_gpu) = self.gpu_compose(recompose) else {
                    screen.counts.count(false);
                    return false;
                };
                #[cfg(feature = "wgpu")]
                if on_gpu && need.gpu && !need.words {
                    // SAFETY: as `gpu_compose`.
                    if let Some(sc) = (unsafe { &*self.scanout.get() }).as_ref() {
                        out.width = FB_W as u32;
                        out.height = FB_H as u32;
                        out.gpu = Some(sc.output().clone());
                        scan_stale = true;
                        sized = true;
                        screen.counts.count(true);
                        return true;
                    }
                }
                if on_gpu {
                    if (recompose || need.words || scan_stale) && !self.gpu_read(&mut scan) {
                        screen.counts.count(false);
                        return false;
                    }
                } else if recompose {
                    let r = self.regs();
                    gr2comp::compose(self.vram(), &r.vc1, &r.xmap, &r.dac, &mut scan);
                }
                scan_stale = false;
                out.width = FB_W as u32;
                out.height = FB_H as u32;
                out.words.clear();
                for y in 0..FB_H {
                    let row = y * gr2comp::OUT_STRIDE;
                    out.words.extend_from_slice(&scan[row..row + FB_W]);
                }
                sized = true;
                screen.counts.count(true);
                true
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

    /// Read the GPU's last frame into `fb_rgb`. False if that failed
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
}
