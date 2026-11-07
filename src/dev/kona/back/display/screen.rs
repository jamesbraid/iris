//! The window the board shows the guest: what `GfxDisplay` hands out.
//!
//! The board owns its screen. Its display thread composes a frame at each
//! retrace and presents it here: the pixels go into the screen's frame buffer
//! (stride 2048, opaque `0xAABBGGRR`) and on to whatever renderer the front end
//! installed in the renderer slot. Screenshot and frame requests ask for the
//! next composed frame; a frame is presented when the store changed since the
//! last, when one is asked for, or every sixth idle frame so a window or VNC
//! client that missed one catches up. The machine keeps a clone of the `Arc`
//! and returns it from `get_display`.

use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::{Rex3Screen, StatusSource, FB_STRIDE};
use crate::gfx_display::{DisplayStop, GfxDisplay};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// Idle frames between forced presents.
const IDLE_REFRESH: u32 = 6;

struct Local {
    overlay: crate::debug_overlay::DebugOverlay,
    status: crate::disp::StatusBar,
    texture: crate::disp::StatusBarTexture,
    size: (usize, usize),
}

pub struct Display {
    renderer: Mutex<Option<Box<dyn Renderer>>>,
    screen: Mutex<Rex3Screen>,
    /// Requests and what has been presented of them, as counts: a request is
    /// pending until a frame composed after it has been presented, and one made
    /// while a frame is composed is still pending after that frame.
    screenshot_asked: AtomicU64,
    screenshot_done: AtomicU64,
    frame_asked: AtomicU64,
    frame_done: AtomicU64,
    screenshots: AtomicU32,
    display_stop: DisplayStop,
    local: Mutex<Option<Local>>,
    /// Frames since the last present.
    idle: AtomicU32,
    /// The machine's status-bar inputs.
    status: Mutex<Arc<StatusSource>>,
}

/// A frame the display has decided to present: the requests it answers.
pub struct Due {
    screenshot: u64,
    frame: u64,
}

impl Display {
    pub fn new(status: Arc<StatusSource>) -> Display {
        Display {
            renderer: Mutex::new(None),
            screen: Mutex::new(Rex3Screen::new()),
            screenshot_asked: AtomicU64::new(0),
            screenshot_done: AtomicU64::new(0),
            frame_asked: AtomicU64::new(0),
            frame_done: AtomicU64::new(0),
            screenshots: AtomicU32::new(0),
            display_stop: DisplayStop::default(),
            local: Mutex::new(None),
            idle: AtomicU32::new(0),
            status: Mutex::new(status),
        }
    }

    /// Wire in the machine's status-bar inputs.
    pub fn set_status(&self, status: Arc<StatusSource>) {
        *self.status.lock() = status;
    }

    /// A display with a status source of its own, for a board no machine has
    /// wired a status bar to.
    pub fn detached() -> Display {
        Display::new(StatusSource::new(Arc::default(), Arc::default()))
    }

    /// Whether this frame is presented: when the store `changed`, when a
    /// screenshot or frame is wanted, or on every sixth idle frame. The requests
    /// it will answer are noted before the frame is composed.
    pub fn due(&self, changed: bool) -> Option<Due> {
        let (screenshot, frame) = (self.screenshot_asked.load(Ordering::Acquire), self.frame_asked.load(Ordering::Acquire));
        let wanted = screenshot != self.screenshot_done.load(Ordering::Acquire) || frame != self.frame_done.load(Ordering::Acquire);
        let idle = self.idle.fetch_add(1, Ordering::Relaxed) + 1;
        if changed || wanted || idle >= IDLE_REFRESH {
            self.idle.store(0, Ordering::Relaxed);
            return Some(Due { screenshot, frame });
        }
        None
    }

    /// Present a composed frame: `rows` are opaque `0xAABBGGRR` pixels, `width`
    /// to a row, the first being the frame's top line.
    pub fn present(&self, rows: &[u32], width: usize, height: usize, due: Due) {
        let mut screen = self.screen.lock();
        let w = width.min(FB_STRIDE);
        let h = height.min(screen.rgba.len() / FB_STRIDE).min(rows.len() / width.max(1));
        screen.width = w;
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        let planes = &mut *screen;
        for (y, src) in rows.chunks_exact(width.max(1)).take(h).enumerate() {
            let at = y * FB_STRIDE;
            planes.fb_rgb[at..at + w].copy_from_slice(&src[..w]);
            planes.rgba[at..at + w].copy_from_slice(&src[..w]);
        }
        screen.frame_seq = screen.frame_seq.wrapping_add(1);
        // A requested screenshot is saved as presented.
        if due.screenshot != self.screenshot_done.load(Ordering::Acquire) {
            let pixels = screen.rgba.clone();
            let n = self.screenshots.fetch_add(1, Ordering::Relaxed);
            let path = format!("screenshot_{:04}.png", n);
            std::thread::spawn(move || match crate::disp::save_screenshot(&path, &pixels, w, h) {
                Ok(()) => println!("iris: screenshot saved to {}", path),
                Err(e) => println!("iris: screenshot failed: {}", e),
            });
        }
        let mut renderer = self.renderer.lock();
        if let Some(renderer) = renderer.as_mut() {
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
        // Mark only the requests this frame answers: one made while it was
        // composed stays pending for the next.
        self.screenshot_done.store(due.screenshot, Ordering::Release);
        self.frame_done.store(due.frame, Ordering::Release);
    }

    /// The display thread's last act: release the renderer, unless the stop came
    /// from the main thread, and drop the per-frame overlay state.
    pub fn finish(&self) {
        self.display_stop.finish(&self.renderer);
        self.local.lock().take();
    }
}

impl GfxDisplay for Display {
    fn display_stop(&self) -> &DisplayStop {
        &self.display_stop
    }

    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        &self.renderer
    }

    fn screen(&self) -> &Mutex<Rex3Screen> {
        &self.screen
    }

    fn request_screenshot(&self) {
        self.screenshot_asked.fetch_add(1, Ordering::AcqRel);
    }

    fn screenshot_pending(&self) -> bool {
        self.screenshot_asked.load(Ordering::Acquire) != self.screenshot_done.load(Ordering::Acquire)
    }

    fn cycles(&self) -> CyclesPtr {
        self.status.lock().cycles()
    }

    fn request_frame(&self) {
        self.frame_asked.fetch_add(1, Ordering::AcqRel);
    }

    fn frame_pending(&self) -> bool {
        self.frame_asked.load(Ordering::Acquire) != self.frame_done.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, px: u32) -> Vec<u32> {
        vec![px; w * h]
    }

    /// A presented frame lands in the screen's buffers, row by row at the
    /// screen's stride, and a requested frame clears when it is presented.
    #[test]
    fn a_presented_frame_lands_in_the_screen() {
        let d = Display::detached();
        let mut f = frame(4, 3, 0xff00_0001);
        f[4 + 2] = 0xff12_3456;
        d.request_frame();
        assert!(d.frame_pending());
        let due = d.due(false).expect("a requested frame is due");
        d.present(&f, 4, 3, due);
        assert!(!d.frame_pending());
        let s = d.screen().lock();
        assert_eq!((s.width, s.height, s.frame_seq), (4, 3, 1));
        assert_eq!(s.rgba[FB_STRIDE + 2], 0xff12_3456);
        assert_eq!(s.fb_rgb[FB_STRIDE + 2], 0xff12_3456);
        assert_eq!(s.rgba[3], 0xff00_0001);
        assert!(s.prebuilt);
    }

    /// A short frame presents what it has, and a wide one is cut to the screen.
    #[test]
    fn a_short_or_wide_frame_is_cut_to_what_it_has() {
        let d = Display::detached();
        d.present(&frame(5, 2, 0xff00_0002)[..7], 5, 2, d.due(true).unwrap());
        assert_eq!(d.screen().lock().height, 1);
        d.present(&frame(3000, 1, 0xff00_0003), 3000, 1, d.due(true).unwrap());
        assert_eq!(d.screen().lock().width, FB_STRIDE);
    }

    /// A frame is presented on a change, when asked for, and every sixth idle
    /// frame.
    #[test]
    fn a_frame_is_due_on_change_on_request_and_when_idle() {
        let d = Display::detached();
        assert!(d.due(true).is_some());
        let idle: Vec<bool> = (0..7).map(|_| d.due(false).is_some()).collect();
        assert_eq!(idle, [false, false, false, false, false, true, false]);
        d.request_frame();
        assert!(d.due(false).is_some());
        d.request_screenshot();
        assert!(d.screenshot_pending());
        assert!(d.due(false).is_some());
    }

    /// A request made while a frame is composed is not lost when that frame is
    /// presented: it is still pending, and the next frame answers it.
    #[test]
    fn a_request_made_during_composition_is_not_lost() {
        let d = Display::detached();
        d.request_frame();
        let due = d.due(false).unwrap();
        d.request_frame(); // arrives while the frame is composed
        d.present(&frame(2, 2, 0xff00_0004), 2, 2, due);
        assert!(d.frame_pending(), "the second request is still pending");
        let due = d.due(false).expect("and makes the next frame due");
        d.present(&frame(2, 2, 0xff00_0004), 2, 2, due);
        assert!(!d.frame_pending());
        // Two requests answered by one frame are both answered.
        d.request_frame();
        d.request_frame();
        d.present(&frame(2, 2, 0xff00_0004), 2, 2, d.due(false).unwrap());
        assert!(!d.frame_pending());
    }
}
