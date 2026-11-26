//! GR2 on the display refresh thread: vertical retrace and composition.
//!
//! The refresh thread (`crate::display`) runs the frame. The retrace line
//! goes up for the blanking interval; the IOC latches the retrace until the
//! host acknowledges: PORT_CONFIG on Indigo2, HPC3_GEN_CONTROL on Indy.
//! The vertical status level (EXTIO SG_STAT_0) is
//! held for the blank: the kernel's retrace handler polls it before it
//! writes a pending swap's display mode, and a polling PROM sees both
//! edges. After the blank, when something visible changed, the frame is
//! composed from VRAM into `screen.fb_rgb` (`prebuilt`): the handler has
//! flipped, and the client draws its next frame in the other buffer.

use crate::disp::Rex3Screen;
use crate::display::{Composed, Head, Want};

use super::gr2comp;
use super::re3::{FB_H, FB_W};
use super::Gr2;

impl Head for Gr2 {
    fn vblank_start(&self, _field: u32, _t_ns: u64) {
        // The refresh rate is the guest's: the VC1's table totals at the
        // clock the PLL was programmed for. Anything that does not decode
        // (the early boards' clock generator, tables half loaded) leaves
        // the last timing.
        let timing = {
            let r = self.regs();
            r.vc1.totals().zip(super::debug::pll_hz(&r.pll_last)).map(|(t, pixel_clock_hz)| {
                let fields = t.fields.clamp(1, 2);
                let blank = t.v_total.saturating_sub(t.visible_lines) / fields;
                crate::display::VideoTiming { h_total: t.h_total, v_total: t.v_total, pixel_clock_hz, vblank_lines: if blank != 0 { blank } else { t.v_total / 25 }, fields }
            })
        };
        if let Some(timing) = timing {
            self.head.set_timing(timing);
        }
        self.set_time(super::host_clock_ns());
        if let Some(cb) = self.retrace_cb.lock().clone() {
            cb(true);
        }
    }

    fn vblank_end(&self, _field: u32, _t_ns: u64) {
        if let Some(cb) = self.retrace_cb.lock().clone() {
            cb(false);
        }
        self.trace_flush(false);
    }

    /// There is nothing smaller to take than the composed frame.
    fn capture(&self, screen: &mut Rex3Screen, _want: Want) -> bool {
        // With a scan-out on the GPU the frame is composed there, in `compose`.
        #[cfg(feature = "wgpu")]
        // SAFETY: the scan-out is the refresh thread's alone, and this runs there.
        if unsafe { &*self.scanout.get() }.is_some() {
            screen.prebuilt = true;
            screen.width = FB_W;
            screen.height = FB_H;
            return true;
        }
        let r = self.regs();
        gr2comp::compose(self.vram(), &r.vc1, &r.xmap, &r.dac, &mut screen.fb_rgb);
        self.presentation_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.presentation_host_read_pixels
            .fetch_add((FB_W * FB_H) as u64, std::sync::atomic::Ordering::Relaxed);
        screen.prebuilt = true;
        screen.width = FB_W;
        screen.height = FB_H;
        true
    }

    fn compose(&self, screen: &mut Rex3Screen, want: Want) -> Composed {
        #[cfg(feature = "wgpu")]
        {
            // SAFETY: as in `capture`.
            if let Some(sc) = unsafe { &mut *self.scanout.get() }.as_mut() {
                let r = self.regs();
                // A GPU that fails has stopped the machine: no frame comes from the CPU in its place.
                if sc.compose(self.vram(), &r.vc1, &r.xmap, &r.dac).is_err() {
                    return Composed::NotReady;
                }
                self.presentation_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if want.gpu {
                    return Composed::Gpu { frame: sc.output().clone(), width: FB_W as u32, height: FB_H as u32 };
                }
                let mut words = vec![0u32; FB_W * FB_H];
                if sc.read_into(&mut words).is_err() {
                    return Composed::NotReady;
                }
                for (y, row) in words.chunks_exact(FB_W).enumerate() {
                    screen.fb_rgb[y * crate::disp::FB_STRIDE..y * crate::disp::FB_STRIDE + FB_W].copy_from_slice(row);
                }
                return Composed::Frame;
            }
        }
        let _ = (screen, want);
        Composed::Frame
    }

    fn gfifo_pending(&self) -> usize {
        self.hq_fifo.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::gr2::Gr2Variant;

    #[test]
    fn ordinary_presentation_scans_the_complete_host_framebuffer() {
        // Not started: a started board's refresh thread also presents its first frame.
        let g = crate::test_board::TestBoard::new(Gr2::new(Gr2Variant::Xz, crate::status::MachineStatus::detached()));
        let mut screen = Rex3Screen::new();
        let want = Want { readback: false, gpu: true };

        assert!(Head::capture(&*g, &mut screen, want));
        assert!(matches!(Head::compose(&*g, &mut screen, want), Composed::Frame));

        let telemetry = g.telemetry_snapshot();
        assert_eq!(telemetry.presentation_frames, 1);
        assert_eq!(telemetry.presentation_host_read_pixels, (FB_W * FB_H) as u64);
    }
}
