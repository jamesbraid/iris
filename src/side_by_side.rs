//! One window showing all screens in a row (`windows = "side-by-side"`): screen 0's
//! slot holds the real renderer and joins the newest frames on the CPU; the others poke it.

use std::sync::Arc;

use crate::disp::BarStats;
use crate::gfx_display::{Frame, Renderer, Screen};

/// Where each screen's picture goes in the one the window shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Top left of each screen's picture, in screen order.
    pub origins: Vec<(u32, u32)>,
}

impl Layout {
    /// Pictures of `sizes` (width, height) left to right, tops level; a screen
    /// with no picture yet takes no room.
    pub fn row(sizes: &[(u32, u32)]) -> Layout {
        let mut x = 0;
        let mut origins = Vec::with_capacity(sizes.len());
        for &(w, _) in sizes {
            origins.push((x, 0));
            x += w;
        }
        Layout { width: x, height: sizes.iter().map(|s| s.1).max().unwrap_or(0), origins }
    }
}

/// A screen's renderer in the side-by-side window; see the module comment.
pub struct SideBySide {
    /// Every screen of the window, in order.
    screens: Arc<[Arc<Screen>]>,
    /// This renderer's screen. Screen 0's draws; the rest poke it.
    me: usize,
    /// The window's renderer: only screen 0's holds it.
    inner: Option<Box<dyn Renderer>>,
    /// The sequence number of each screen's frame in `out`.
    seen: Vec<u64>,
    out: Frame,
    /// The size last told to `inner`.
    sized: (u32, u32),
}

impl SideBySide {
    /// The renderers for `screens`, to install in the slot of the screen at the
    /// same position. `inner` is the window's renderer.
    pub fn for_screens(screens: &[Arc<Screen>], inner: Box<dyn Renderer>) -> Vec<Box<dyn Renderer>> {
        let all: Arc<[Arc<Screen>]> = screens.into();
        let mut inner = Some(inner);
        (0..screens.len())
            .map(|me| {
                Box::new(SideBySide {
                    screens: all.clone(),
                    me,
                    inner: inner.take().filter(|_| me == 0),
                    seen: vec![0; screens.len()],
                    out: Frame::default(),
                    sized: (0, 0),
                }) as Box<dyn Renderer>
            })
            .collect()
    }

    /// Copy the newest frame of every screen into `out`, when any is new.
    /// Returns whether there is a picture to show.
    fn compose(&mut self) -> bool {
        let frames: Vec<_> = self.screens.iter().map(|s| s.latest_words()).collect();
        let sizes: Vec<_> = frames.iter().map(|f| f.as_ref().map_or((0, 0), |f| (f.width, f.height))).collect();
        let seqs: Vec<_> = frames.iter().map(|f| f.as_ref().map_or(0, |f| f.seq)).collect();
        let layout = Layout::row(&sizes);
        if layout.width == 0 || layout.height == 0 {
            return false;
        }
        let same_shape = (self.out.width, self.out.height) == (layout.width, layout.height);
        if same_shape && seqs == self.seen {
            return true;
        }
        let (w, h) = (layout.width as usize, layout.height as usize);
        let mut words = std::mem::take(&mut self.out.words);
        // Black where no picture reaches (a shorter screen, one with no frame).
        words.clear();
        words.resize(w * h, 0xff00_0000);
        for (frame, &(x, y)) in frames.iter().zip(&layout.origins) {
            let Some(f) = frame else { continue };
            for row in 0..f.height as usize {
                let src = &f.words[row * f.width as usize..][..f.width as usize];
                words[(y as usize + row) * w + x as usize..][..f.width as usize].copy_from_slice(src);
            }
        }
        self.out = Frame { seq: self.out.seq.wrapping_add(1), width: layout.width, height: layout.height, words, ..Frame::default() };
        self.seen = seqs;
        true
    }
}

impl Renderer for SideBySide {
    fn present(&mut self, _frame: &Frame, stats: &BarStats) {
        if self.me != 0 {
            // Screen 0's thread draws at its next tick; a wake would cut its
            // frame short and add a retrace.
            self.screens[0].window.poke();
            return;
        }
        if !self.compose() {
            return;
        }
        let size = (self.out.width, self.out.height);
        let Some(inner) = self.inner.as_mut() else { return };
        // The window follows the whole picture, not the first screen's.
        if self.sized != size {
            self.sized = size;
            inner.resize(size.0 as usize, size.1 as usize);
        }
        inner.present(&self.out, stats);
    }

    // The screens tell each renderer their own size; the window's is the row's.
    fn resize(&mut self, _width: usize, _height: usize) {}

    fn stop(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.stop();
            // A restarted window asks for its size again.
            self.sized = (0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn two_screens_sit_left_to_right_with_tops_level() {
        let l = Layout::row(&[(1280, 1024), (1280, 1024)]);
        assert_eq!((l.width, l.height), (2560, 1024));
        assert_eq!(l.origins, [(0, 0), (1280, 0)]);
    }

    #[test]
    fn the_row_is_as_tall_as_the_tallest_screen() {
        let l = Layout::row(&[(1024, 768), (1600, 1200), (640, 480)]);
        assert_eq!((l.width, l.height), (3264, 1200));
        assert_eq!(l.origins, [(0, 0), (1024, 0), (2624, 0)]);
    }

    #[test]
    fn a_screen_with_no_picture_takes_no_room() {
        assert_eq!(Layout::row(&[(0, 0), (800, 600)]).origins, [(0, 0), (0, 0)]);
        assert_eq!(Layout::row(&[(0, 0), (800, 600)]).width, 800);
        assert_eq!(Layout::row(&[]), Layout::default());
        assert_eq!(Layout::row(&[(0, 0)]).width, 0);
    }

    /// What the window's renderer was shown: (width, height, words) and sizes.
    #[derive(Clone, Default)]
    struct Window {
        shown: Arc<Mutex<Vec<(u32, u32, Vec<u32>)>>>,
        sizes: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    impl Renderer for Window {
        fn present(&mut self, f: &Frame, _: &BarStats) {
            self.shown.lock().push((f.width, f.height, f.words.clone()));
        }
        fn resize(&mut self, w: usize, h: usize) {
            self.sizes.lock().push((w, h));
        }
    }

    fn stats() -> BarStats {
        crate::disp::StatusSource::detached().snapshot(std::time::Instant::now(), 0)
    }

    fn publish(s: &Screen, w: u32, h: u32, fill: u32) {
        let mut f = s.frame(w, h);
        f.words.fill(fill);
        s.publish(f, &stats());
    }

    /// Two screens of different sizes land side by side in the one picture the
    /// window's renderer gets, from the first screen's thread; the second
    /// screen's frames only ask for a redraw.
    #[test]
    fn the_first_screen_draws_every_screens_newest_frame_into_one_picture() {
        let screens: Vec<_> = (0..2).map(|_| Arc::new(Screen::default())).collect();
        let window = Window::default();
        for (s, r) in screens.iter().zip(SideBySide::for_screens(&screens, Box::new(window.clone()))) {
            *s.window.renderer.lock() = Some(r);
        }
        publish(&screens[0], 2, 2, 1);
        // The second screen has no frame yet: its room is empty.
        assert_eq!(window.shown.lock().last().unwrap(), &(2, 2, vec![1; 4]));
        let mut wait = crate::gfx_display::FrameWait::new(&screens[0].changed, std::time::Duration::from_millis(300));
        let start = std::time::Instant::now();
        publish(&screens[1], 3, 1, 2);
        wait.wait();
        assert!(start.elapsed() >= std::time::Duration::from_millis(250), "the second screen's frame does not cut the first one's period short");
        assert_eq!(window.shown.lock().len(), 1, "the second screen's thread does not draw");
        assert!(screens[0].window.label().is_none());
        // The first screen's next tick shows the newest frames of both.
        screens[0].tick(stats, |_, _| false);
        let (w, h, words) = window.shown.lock().last().unwrap().clone();
        assert_eq!((w, h), (5, 2));
        let black = 0xff00_0000;
        assert_eq!(words, [1, 1, 2, 2, 2, 1, 1, black, black, black]);
        assert_eq!(*window.sizes.lock(), [(2, 2), (5, 2)], "the window is told the row's size");
        // Nothing new: the same picture is shown again, built once.
        let before = window.shown.lock().len();
        for _ in 0..crate::gfx_display::IDLE_REFRESH {
            screens[0].tick(stats, |_, _| false);
        }
        assert!(window.shown.lock().len() > before);
        assert!(window.shown.lock().iter().skip(1).all(|s| s.2 == words));
    }
}
