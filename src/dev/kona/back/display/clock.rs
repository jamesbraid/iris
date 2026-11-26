//! The display's clock: one frame at a time, at the video format's rate.
//!
//! At each frame boundary the display raises the video interrupt, once a frame
//! whether or not anything was drawn. The retrace sequence register is not
//! the clock's: the pipe sets it when it performs the token.
//!
//! The frames come from the machine's display refresh thread
//! (`crate::display`), which runs whether or not the pipe works, so they never
//! mark activity for the driver's watchdog. The head holds only the handle the
//! front end gave it, nothing of the pipe's state.
//!
//! Grades. The frame rate is 60 Hz until a video format's refresh rate can be
//! parsed from its load (approximated).

use super::super::super::handle::DisplayHandle;
use crate::display::{Composed, Head, HeadHandle, VideoTiming, Want};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;
use std::time::Duration;

/// The default frame period: 60 Hz.
pub const FRAME_PERIOD: Duration = Duration::from_micros(16_667);

/// What the board and its display thread share.
#[derive(Default)]
pub struct Clock {
    /// Frames the display has run.
    frames: AtomicU64,
    /// The display is to hold still (a snapshot), and the lock every frame
    /// callback runs under, so holding still can wait one out.
    paused: AtomicBool,
    busy: Mutex<()>,
}

impl Clock {
    pub fn frames(&self) -> u64 {
        self.frames.load(Relaxed)
    }

    /// One frame boundary: raise the video interrupt.
    pub fn frame(&self, handle: &DisplayHandle) {
        handle.video_frame(handle.epoch());
        self.frames.fetch_add(1, Release);
    }

    /// Hold the display still, and wait out a frame callback in flight. A
    /// board with no display running has nothing to wait for.
    pub fn quiesce(&self, running: bool) {
        self.paused.store(true, Release);
        if running {
            drop(self.busy.lock());
        }
    }

    pub fn resume(&self) {
        self.paused.store(false, Release);
    }
}

/// The board on the refresh thread: the clock, the handle the retraces go
/// through, and the compositor.
struct Scan {
    clock: Arc<Clock>,
    handle: DisplayHandle,
    sink: Mutex<super::Compositor>,
}

impl Head for Scan {
    fn vblank_start(&self, _field: u32, _t_ns: u64) {
        let _busy = self.clock.busy.lock();
        if !self.clock.paused.load(Acquire) {
            self.clock.frame(&self.handle);
        }
    }

    fn changed(&self) -> bool {
        let _busy = self.clock.busy.lock();
        !self.clock.paused.load(Acquire) && self.sink.lock().drawn()
    }

    fn compose(&self, screen: &mut crate::disp::Rex3Screen, want: Want) -> Composed {
        let _busy = self.clock.busy.lock();
        self.sink.lock().compose(screen, want)
    }
}

/// The running display: its head on the refresh thread, and the board's
/// side of it.
pub struct DisplayThread {
    head: Option<Arc<HeadHandle>>,
    /// The head holds this weakly.
    _scan: Arc<Scan>,
}

impl DisplayThread {
    /// Start frames every `period` on the refresh thread of `sink`'s display.
    pub fn spawn(clock: Arc<Clock>, handle: DisplayHandle, period: Duration, sink: super::Compositor) -> DisplayThread {
        let head = sink.display().head().clone();
        let scan = Arc::new(Scan { clock, handle, sink: Mutex::new(sink) });
        let weak: std::sync::Weak<dyn Head> = Arc::downgrade(&(scan.clone() as Arc<dyn Head>));
        head.attach(weak);
        if period != FRAME_PERIOD {
            // A frame of a million clocks: one clock is a millionth of it.
            let clock_hz = 1_000_000u64 * 1_000_000_000 / (period.as_nanos() as u64).max(1);
            head.set_timing(VideoTiming { h_total: 1000, v_total: 1000, pixel_clock_hz: clock_hz, vblank_lines: 40, fields: 1 });
        } else {
            head.set_timing(VideoTiming::HZ60);
        }
        head.start();
        DisplayThread { head: Some(head), _scan: scan }
    }

    /// End the frames; returns once none is running. Called with the front
    /// end's pipe slot taken and no register-file lock held: a frame takes
    /// only that lock, in a handle call that sends an interrupt packet after
    /// dropping it, and never the slot, so this cannot wait on a lock its
    /// caller holds.
    pub fn stop(&mut self) {
        if let Some(head) = self.head.take() {
            head.stop();
        }
    }
}

impl Drop for DisplayThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::kona::tests::{test_cfg, TestHost};
    use crate::dev::kona::{Kona, Reply};
    use crate::traits::{Device, BUS_OK};

    const INT_ENABLE: u64 = 0x800230;
    const INT_CAUSE: u64 = 0x800238;
    const RETRACE_TAG: u64 = 0x800248;

    /// A compositor over a tiny store, for a thread that has nothing to show.
    fn quiet() -> crate::dev::kona::back::display::Compositor {
        use crate::dev::kona::back::display::{Compositor, screen::Display, tables::Tables};
        use crate::dev::kona::back::store::Store;
        Compositor::new(Arc::new(Store::new(4, 4)), 1, Arc::default(), Arc::new(Mutex::new(Tables::default())), Arc::new(Display::detached()))
    }

    /// A started board whose back end holds the handle; the board's host to read
    /// registers and see the packets.
    fn board() -> (Arc<crate::dev::kona::Kona>, Arc<TestHost>, DisplayHandle) {
        let kept = Arc::new(Mutex::new(None));
        let host = TestHost::new();
        let kona = Kona::new(test_cfg(), host.clone(), host.clone(), crate::dev::kona::tests::test_status(), None).unwrap();
        let keep = kept.clone();
        kona.shared.pipe.lock().backend.hook = Some(crate::dev::kona::back::TestHook { job: Box::new(|_, _| Some(Reply::Done)), events: Arc::default(), on_attach: Some(Box::new(move |h| *keep.lock() = Some(h))) });
        for (off, v) in [(0x34u64, 0x98000u32), (0x3c, 0x180_0090), (0x5c, 0x3e_2210), (INT_ENABLE, 0xf7c)] {
            assert_eq!(kona.test_write(off, 4, v as u64), BUS_OK);
        }
        kona.start();
        let handle = kept.lock().clone().expect("attach gave a handle");
        (kona, host, handle)
    }

    fn read(kona: &crate::dev::kona::Kona, off: u64) -> u32 {
        kona.bus_read(off, 4).1 as u32
    }

    /// A frame raises one video interrupt until the driver acknowledges it, and
    /// leaves the tag register alone.
    #[test]
    fn a_frame_raises_video_and_leaves_the_tag() {
        let (kona, host, h) = board();
        let clock = Clock::default();
        clock.frame(&h);
        assert_eq!(host.sent().len(), 1);
        assert_eq!(clock.frames(), 1);
        clock.frame(&h);
        assert_eq!(host.sent().len(), 1, "not again before the acknowledge");
        kona.bus_write(INT_CAUSE, 4, 8);
        clock.frame(&h);
        assert_eq!(host.sent().len(), 2);
        assert_eq!(read(&kona, RETRACE_TAG), 0, "a frame never changes the tag");
        kona.stop();
    }

    /// The thread runs frames by itself; stopping joins it.
    #[test]
    fn the_thread_runs_frames_and_joins() {
        let (kona, _host, h) = board();
        let clock = Arc::new(Clock::default());
        let mut t = DisplayThread::spawn(clock.clone(), h.clone(), Duration::from_millis(1), quiet());
        assert!(crate::test_board::until(|| clock.frames() >= 1));
        t.stop();
        let f = clock.frames();
        std::thread::yield_now();
        assert_eq!(clock.frames(), f, "no frames after the join");
        kona.stop();
    }

    /// A quiesced thread holds still between frames and goes on when resumed.
    #[test]
    fn a_quiesced_thread_holds_still() {
        let (kona, _host, h) = board();
        let clock = Arc::new(Clock::default());
        let mut t = DisplayThread::spawn(clock.clone(), h, Duration::from_millis(1), quiet());
        clock.quiesce(true);
        let f = clock.frames();
        for _ in 0..10_000 {
            std::thread::yield_now();
        }
        assert_eq!(clock.frames(), f);
        clock.resume();
        assert!(crate::test_board::until(|| clock.frames() > f));
        t.stop();
        kona.stop();
    }
}
