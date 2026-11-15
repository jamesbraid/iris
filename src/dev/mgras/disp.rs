//! Display side: the refresh thread (vertical retrace, then a frame
//! snapshot and composition, see `frame`), host GL frame compositing, and
//! PNG output.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::dcb::Dcb;
use super::rss::{self, Rss};
use super::frame::{self, Frame};
use super::{microcode, plain, Line, Mgras};

/// The window ID a frame for the window at (`x`, `y`), `w` x `h` (screen
/// coordinates, top-down) should be painted through: the commonest ID
/// over a grid of samples inside it that has an RGB display mode. None
/// if no sampled pixel is in an RGB window.
pub fn window_did(rss: &Rss, dcb: &Dcb, x: i32, y: i32, w: usize, h: usize) -> Option<u8> {
    let mut counts = [0u32; 32];
    let mut runs = Vec::new();
    let (screen_w, screen_h) = frame::display_size(dcb);
    for sy in 0..16 {
        let py = y + (h as i32 * (2 * sy + 1)) / 32;
        if !(0..screen_h as i32).contains(&py) {
            continue;
        }
        dcb.vc3.main_did_runs(py as usize, &mut runs);
        for sx in 0..16 {
            let px = x + (w as i32 * (2 * sx + 1)) / 32;
            if !(0..screen_w as i32).contains(&px) {
                continue;
            }
            let did = runs.iter().rev().find(|r| r.0 as i32 <= px).map_or(0, |r| r.1);
            if dcb.xmap.main_mode(did as u32) & 0x1F >= 4 {
                counts[did as usize & 31] += 1;
            }
        }
    }
    let (did, n) = counts.iter().enumerate().max_by_key(|&(_, n)| *n)?;
    (*n > 0).then_some(did as u8)
}

/// Paint a host GL frame (`bgra`: `h` rows, top first, `stride` bytes
/// each) into the framebuffer at screen position (`x`, `y`), wherever the
/// pixel belongs to the frame's window ID, so windows over it stay over it.
/// The pixels become part of the framebuffer, as GL's would on the board,
/// for anything that reads them back. False when no window ID fits.
pub fn composite(rss: &mut Rss, dcb: &Dcb, x: i32, y: i32, bgra: &[u8], stride: usize, w: usize, h: usize) -> bool {
    let Some(target) = window_did(rss, dcb, x, y, w, h) else { return false };
    let mut runs = Vec::new();
    let (screen_w, screen_h) = frame::display_size(dcb);
    let (main, _) = frame::scanout_buffers(rss, dcb);
    // A 12-bit pair window shows the frame in both buffers.
    let pair = frame::pair_format(dcb.xmap.main_mode(target as u32));
    for row in 0..h {
        let sy = y + row as i32;
        if !(0..screen_h as i32).contains(&sy) {
            continue;
        }
        dcb.vc3.main_did_runs(sy as usize, &mut runs);
        if runs.is_empty() || runs[0].0 != 0 {
            runs.insert(0, (0, 0));
        }
        let fb_y = (screen_h - 1 - sy as usize) as u32;
        for (k, &(x0, did)) in runs.iter().enumerate() {
            if did != target {
                continue;
            }
            let x1 = runs.get(k + 1).map_or(screen_w as i32, |r| r.0 as i32);
            let lo = (x0 as i32).max(x).max(0);
            let hi = x1.min(x + w as i32).min(screen_w as i32);
            for sx in lo..hi {
                let i = row * stride + (sx - x) as usize * 4;
                let Some(p) = bgra.get(i..i + 4) else { break };
                let v = p[2] as u32 | (p[1] as u32) << 8 | (p[0] as u32) << 16;
                let v = match pair {
                    Some(p) => p.pack_with(v | 0xFF << 24, None),
                    None => v,
                };
                rss.mem.put(&main, sx as u32, fb_y, v as u64);
            }
        }
    }
    true
}

/// The window ID a colour-index frame's window has, in the main planes (a
/// colour-index display mode) or the overlay planes (one that is on): the
/// commonest such ID over a grid of samples inside the window, as
/// `window_did` -- and only when it covers three quarters of the samples on
/// the screen. A window just mapped may not have its ID in the tables yet,
/// and the commonest ID around it is then someone else's: the frame went
/// there, the program was told it was shown, and gltest's overlay read back
/// 0s now and then. None, and the program puts the frame up itself.
fn index_window_did(dcb: &Dcb, x: i32, y: i32, w: usize, h: usize, overlay: bool) -> Option<u8> {
    let mut total = 0u32;
    let mut counts = [0u32; 32];
    let mut runs = Vec::new();
    let (screen_w, screen_h) = frame::display_size(dcb);
    for sy in 0..16 {
        let py = y + (h as i32 * (2 * sy + 1)) / 32;
        if !(0..screen_h as i32).contains(&py) {
            continue;
        }
        if overlay {
            dcb.vc3.overlay_did_runs(py as usize, &mut runs);
        } else {
            dcb.vc3.main_did_runs(py as usize, &mut runs);
        }
        for sx in 0..16 {
            let px = x + (w as i32 * (2 * sx + 1)) / 32;
            if !(0..screen_w as i32).contains(&px) {
                continue;
            }
            let did = runs.iter().rev().find(|r| r.0 as i32 <= px).map_or(0, |r| r.1) as u32;
            let index = if overlay { dcb.xmap.overlay_mode(did) != 0 } else { dcb.xmap.main_mode(did) & 0x1F < 4 };
            total += 1;
            if index {
                counts[did as usize & 31] += 1;
            }
        }
    }
    let (did, n) = counts.iter().enumerate().max_by_key(|&(_, n)| *n)?;
    (*n > 0 && *n * 4 >= total * 3).then_some(did as u8)
}

/// Paint a colour-index window's host GL frame into the framebuffer, as
/// `composite` does an RGB one, but as indices: each pixel's index is its
/// red byte and the low nibble of its green (`bgra`: B, G, R, A), and goes
/// where the board keeps that window's indices -- the low 8 or 12 main
/// planes (by the window ID's display mode), the high 12 when the ID shows
/// buffer B (`frame::Frame::snapshot`), or the overlay planes for an overlay
/// window -- for the colour map to show, as GL's drawing would on the board.
/// Before this, the program read every frame back, turned it into indices
/// and put it to the X server itself: SGI's IRIS GL on host OpenGL ran
/// flight's full-screen colour-index window at a frame every few seconds.
/// False when no window ID fits, or there are no overlay planes.
#[allow(clippy::too_many_arguments)]
pub fn composite_index(rss: &mut Rss, dcb: &Dcb, x: i32, y: i32, bgra: &[u8], stride: usize, w: usize, h: usize, overlay: bool) -> bool {
    let (main, over) = frame::scanout_buffers(rss, dcb);
    let buf = match (overlay, over) {
        (false, _) => main,
        (true, Some(o)) => o,
        (true, None) => return false,
    };
    let Some(target) = index_window_did(dcb, x, y, w, h, overlay) else { return false };
    let b_half = !overlay && dcb.xmap.buf_select() >> target & 1 != 0;
    let (mask, shift): (u64, u32) = if overlay {
        (0xFF, 0)
    } else if b_half {
        (0xFFF << 12, 12)
    } else if dcb.xmap.main_mode(target as u32) & 0x1F == 1 {
        (0xFF, 0)
    } else {
        (0xFFF, 0)
    };
    let mut runs = Vec::new();
    let (screen_w, screen_h) = frame::display_size(dcb);
    for row in 0..h {
        let sy = y + row as i32;
        if !(0..screen_h as i32).contains(&sy) {
            continue;
        }
        if overlay {
            dcb.vc3.overlay_did_runs(sy as usize, &mut runs);
        } else {
            dcb.vc3.main_did_runs(sy as usize, &mut runs);
        }
        if runs.is_empty() || runs[0].0 != 0 {
            runs.insert(0, (0, 0));
        }
        let fb_y = (screen_h - 1 - sy as usize) as u32;
        for (k, &(x0, did)) in runs.iter().enumerate() {
            if did != target {
                continue;
            }
            let x1 = runs.get(k + 1).map_or(screen_w as i32, |r| r.0 as i32);
            let lo = (x0 as i32).max(x).max(0);
            let hi = x1.min(x + w as i32).min(screen_w as i32);
            for sx in lo..hi {
                let i = row * stride + (sx - x) as usize * 4;
                let Some(p) = bgra.get(i..i + 4) else { break };
                let index = p[2] as u64 | (p[1] as u64 & 0xF) << 8;
                let old = rss.mem.get(&buf, sx as u32, fb_y);
                rss.mem.put(&buf, sx as u32, fb_y, (old & !mask) | (index << shift & mask));
            }
        }
    }
    true
}

impl Mgras {
    pub(super) fn refresh_loop(self: &Arc<Self>) {
        let screen = &self.screens[0];
        // The frame as the snapshot composes it, rows `OUT_STRIDE` apart; the
        // published frame is cut from it row by row.
        let mut scan = vec![0xFF00_0000u32; frame::OUT_STRIDE * crate::disp::FB_MAX_H];
        // A frame has been composed since the board started.
        let mut composed_once = false;
        // Microcode fingerprints: checked every two seconds, logged once
        // the same value is seen twice in a row and differs from the last
        // one logged.
        let mut microcode_seen = microcode::Fingerprints::default();
        let mut microcode_logged = microcode::Fingerprints::default();
        let mut frames = 0u32;
        let mut last_rec_flush = std::time::Instant::now();
        let mut snap = plain::boxed_zeroed::<Frame>();

        crate::gfx_display::run_display(
            screen,
            &self.running,
            &self.display_stop,
            // Both queues: the HQ3's usually drains at once, and the backlog
            // waits in front of the raster engines.
            || self.status.snapshot(std::time::Instant::now(), self.hq_fifo.len() + self.rss_fifo.len()),
            || {
                if last_rec_flush.elapsed() >= std::time::Duration::from_millis(500) {
                    if let Some(rec) = self.mem_rec.lock().clone() {
                        rec.lock().flush();
                    }
                    last_rec_flush = std::time::Instant::now();
                }
                self.trace_flush(false);
                frames = frames.wrapping_add(1);
                if frames % 120 == 0 {
                    let fp = self.microcode_fingerprints();
                    if fp.hq != 0 && fp == microcode_seen && fp != microcode_logged {
                        eprintln!("impact: microcode {fp}");
                        microcode_logged = fp;
                    }
                    microcode_seen = fp;
                }
                // Vertical retrace, after the frame is taken: scanout reads
                // the front buffer during the frame, and the kernel's retrace
                // handler flips buffers (XMAP buffer select) during the blank.
                // Taken after the pulse, the snapshot raced the flip: the
                // handler flips and the client starts its next frame in the
                // buffer just shown, while the snapshot was still copying it
                // (octahedra tore). Now a flip at this retrace shows from the
                // next frame, as on the board.
                self.begin_frame();
                // Video timing chip display control bit 0: retrace interrupts.
                let retrace = self.front_view().dcb.vc3.regs[0x1E] & 1 != 0;
                // A pulse per frame. The handler acknowledges nothing on the
                // board, so the line must drop again by itself.
                if retrace {
                    crate::pf_instant!(crate::perfetto::events::MGRAS_RETRACE);
                    self.set_line(Line::Retrace, true);
                    std::thread::sleep(std::time::Duration::from_micros(500));
                    self.set_line(Line::Retrace, false);
                }
            },
            |out, need| {
                let dirty = self.dirty.swap(false, Ordering::AcqRel);
                // On the GPU unit the frame is composed there every frame for
                // a window on its device; else only when something changed or
                // someone reads the words, and then it is read back.
                let on_gpu = self.presents_on_gpu();
                if !(on_gpu && need.gpu) && !(dirty || need.words || !composed_once) {
                    screen.counts.count(false);
                    return false;
                }
                composed_once = true;
                // The snapshot comes before this frame's retrace pulse:
                // the last retrace's updates (window-ID tables, swaps)
                // are in, and nothing draws into the front buffer now.
                // SAFETY: a read-only view of the framebuffer; tearing
                // tolerated.
                let rss = unsafe { &*self.rss.get() };
                crate::pf_begin!(crate::perfetto::events::MGRAS_CAPTURE, self.rss_fifo.len());
                // A GPU that fails has stopped the machine: no frame comes
                // from the CPU's stale mirror in its place.
                match self.gpu_frame(rss, need, out) {
                    Ok(true) => {
                        crate::pf_end!(crate::perfetto::events::MGRAS_CAPTURE);
                        screen.counts.count(true);
                        return true;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        crate::pf_end!(crate::perfetto::events::MGRAS_CAPTURE);
                        screen.counts.count(false);
                        return false;
                    }
                }
                snap.snapshot(rss, &self.front_view().dcb);
                snap.compose(&mut scan);
                let (w, h) = (snap.width, snap.height);
                crate::pf_end!(crate::perfetto::events::MGRAS_CAPTURE);
                out.width = w as u32;
                out.height = h as u32;
                out.words.clear();
                for y in 0..h {
                    let row = y * frame::OUT_STRIDE;
                    out.words.extend_from_slice(&scan[row..row + w]);
                }
                screen.counts.count(true);
                true
            },
        );
    }

    /// With the GPU unit: compose the displayed frame on the GPU for this
    /// tick's consumer. Ok(false) on the CPU raster path; Err when the
    /// scan-out failed, which has stopped the machine.
    fn gpu_frame(&self, rss: &Rss, need: crate::gfx_display::Need, out: &mut crate::gfx_display::Frame) -> Result<bool, String> {
        #[cfg(feature = "wgpu")]
        {
            // SAFETY: the scan-out is the display thread's; the registers and
            // the DCB are read racily, as the CPU snapshot reads them.
            let Some(sc) = (unsafe { &mut *self.gpu_scanout.get() }).as_mut() else { return Ok(false) };
            sc.show(rss, &self.front_view().dcb, need, out)?;
            self.gpu_frames[!need.gpu as usize].fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        #[cfg(not(feature = "wgpu"))]
        {
            let _ = (rss, need, out);
            Ok(false)
        }
    }

    /// Start the engines and the display refresh thread.
    pub fn start_display(self: &Arc<Self>) {
        self.start_engines();
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::clone(self);
        *self.refresh.lock() = Some(
            std::thread::Builder::new()
                .name("MGRAS-Refresh".into())
                .spawn(move || me.refresh_loop())
                .expect("spawn MGRAS refresh thread"),
        );
    }

    pub fn stop_display(&self) {
        self.display_stop.request();
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.refresh.lock().take() {
            let _ = h.join();
        }
        self.stop_engines();
    }
}
