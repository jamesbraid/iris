//! What the UI, CI socket and GUI front-end need from whichever graphics board
//! drives the screen.
//!
//! A board owns its display thread and its composition; the thread is
//! [`run_display`] with the board's closures. What the front-end sees is one
//! [`Screen`] per head: the renderer slot it installs into, the newest
//! composed frame (screenshots, VNC, the monitor), the requests it makes for
//! a fresh one, and the counts of what the thread did. [`GfxDisplay`] only
//! hands the screens out, with the hardware cursor and the CPU cycle counter.

use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::rex3::Renderer;

pub trait GfxDisplay: Send + Sync {
    /// The board's screens in head order. Most boards have one.
    fn screens(&self) -> &[Arc<Screen>];

    /// The board's [`DisplayStop`], which its stop and its display thread use.
    fn display_stop(&self) -> &DisplayStop;

    /// The hardware cursor as the guest has set it, read live from the
    /// board's registers (not the last composed frame).
    fn hw_cursor(&self) -> Option<HwCursor> { None }

    /// CPU cycle counter the board was wired to (GUI's live MIPS estimate).
    fn cycles(&self) -> CyclesPtr;
}

/// Set when anything the scan-out reads may have changed; taken by the
/// display before it reads anything. One store per processed command, job or
/// display-register write, never one per pixel or per memory store.
///
/// The writer marks after its effect is visible and the display takes the flag
/// before it reads, so a frame composed for a mark sees what the mark follows.
/// Every mark stores, even with the flag already up: a frame that takes it
/// between a writer's effect and its mark would otherwise lose the effect. The
/// flag has a cache line to itself, away from the state the writers touch.
///
/// A display thread that has nothing to do until something changes can park on
/// it ([`FrameWait::park_until_marked`]); a mark then wakes it.
#[repr(align(64))]
#[derive(Default)]
pub struct ScreenChanged {
    flag: AtomicBool,
    /// The display thread is parked, or on its way to be.
    parked: AtomicBool,
    /// Someone asked the thread to stop waiting ([`wake`](Self::wake)).
    wake: AtomicBool,
    /// The display thread, set by [`FrameWait::new`].
    thread: Mutex<Option<std::thread::Thread>>,
}

impl ScreenChanged {
    /// A flag that starts up, for a display whose first frame must be composed.
    pub fn marked() -> Self {
        let c = Self::default();
        c.flag.store(true, Ordering::Relaxed);
        c
    }

    /// One release store, and a relaxed load of whether the display thread
    /// sleeps, which a thread that is not parked leaves false: no call, lock or
    /// read-modify-write on the writer's path.
    #[inline(always)]
    pub fn mark(&self) {
        self.flag.store(true, Ordering::Release);
        if self.parked.load(Ordering::Relaxed) {
            self.unpark();
        }
    }

    /// Whether a mark is waiting, leaving it up.
    #[inline]
    pub fn is_marked(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Whether a mark came since the last call, clearing it. The display calls
    /// it before it reads anything the mark covers.
    #[inline]
    pub fn take(&self) -> bool {
        self.flag.swap(false, Ordering::AcqRel)
    }

    /// End the display thread's current or next wait at once: a screenshot
    /// request, or a stop.
    pub fn wake(&self) {
        self.wake.store(true, Ordering::Release);
        self.parked.store(false, Ordering::Release);
        if let Some(t) = self.thread.lock().as_ref() {
            t.unpark();
        }
    }

    fn unpark(&self) {
        // The swap makes one writer the waker.
        if self.parked.swap(false, Ordering::AcqRel) {
            if let Some(t) = self.thread.lock().as_ref() {
                t.unpark();
            }
        }
    }
}

/// What the display thread did, counted by that thread alone with relaxed
/// stores and read by the monitor's `screen frames`.
#[derive(Default)]
pub struct FrameCounts {
    frames: AtomicU64,
    composed: AtomicU64,
    skipped: AtomicU64,
}

impl FrameCounts {
    /// One frame, composed or skipped.
    pub fn count(&self, composed: bool) {
        // One writer, so a load and a store do.
        let add = |a: &AtomicU64| a.store(a.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        add(&self.frames);
        add(if composed { &self.composed } else { &self.skipped });
    }

    /// Frames, composed, skipped.
    pub fn get(&self) -> (u64, u64, u64) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        (g(&self.frames), g(&self.composed), g(&self.skipped))
    }

    pub fn reset(&self) {
        for a in [&self.frames, &self.composed, &self.skipped] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

/// Requests for a fresh frame, counted so that one made while a frame is being
/// composed is still pending after it: asked and answered are counts, not a
/// flag. A screenshot, `request_frame` and a capturing client (VNC) are each
/// one of these on a board.
///
/// The display thread takes [`note`](Self::note) before it composes and calls
/// [`answer`](Self::answer) with it once the frame is in the screen; a caller
/// waits on [`pending`](Self::pending). A board that parks its display thread
/// wakes it from the request ([`ScreenChanged::wake`]).
#[derive(Default)]
pub struct FrameRequests {
    asked: AtomicU64,
    done: AtomicU64,
    capture: std::sync::atomic::AtomicU32,
}

impl FrameRequests {
    pub fn request(&self) {
        self.asked.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether a request is waiting for its frame.
    pub fn pending(&self) -> bool {
        self.asked.load(Ordering::Acquire) != self.done.load(Ordering::Acquire)
    }

    /// The requests a frame composed from now on will answer. Take it before
    /// composing.
    pub fn note(&self) -> u64 {
        self.asked.load(Ordering::Acquire)
    }

    /// Whether the frame that `noted` was taken for answers a request.
    pub fn answers(&self, noted: u64) -> bool {
        noted != self.done.load(Ordering::Acquire)
    }

    /// The frame is in the screen: everything up to `noted` is answered.
    pub fn answer(&self, noted: u64) {
        self.done.store(noted, Ordering::Release);
    }

    /// One more consumer wants every frame (`on`), or one fewer. Calls nest.
    pub fn capture(&self, on: bool) {
        if on {
            self.capture.fetch_add(1, Ordering::AcqRel);
        } else {
            let _ = self.capture.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
    }

    pub fn capturing(&self) -> bool {
        self.capture.load(Ordering::Acquire) != 0
    }
}

/// The frame period of a 60 Hz mode.
pub const FRAME_PERIOD: Duration = Duration::from_micros(16_667);

/// Frames between presents of an unchanged screen, about ten a second at
/// 60 Hz: the status bar moves, and a window that missed a frame catches up.
pub const IDLE_REFRESH: u32 = 6;

/// The longest a parked display thread goes without looking for a mark. A
/// writer that marks while the thread is on its way to sleep may not see that
/// it sleeps, since the mark is a store with no barrier behind it. The thread's
/// own look finds the flag, so the race costs this much latency, not a frame.
const PARK_RECHECK: Duration = Duration::from_millis(100);

/// Why [`FrameWait::park_until_marked`] returned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Woke {
    /// The display was marked; the mark is still up for the frame to take.
    Marked,
    /// [`ScreenChanged::wake`] asked for the thread's attention.
    Requested,
    /// The board's timer came due.
    Timer,
}

/// The display thread's wait between frames, for a loop that already has its
/// frames. Frames are paced from a fixed start, so a slow one does not stretch
/// the rate, and a stall resyncs to now rather than running the missed frames
/// back to back. Call it last in the loop, after the frame.
pub struct FrameWait<'a> {
    changed: &'a ScreenChanged,
    period: Duration,
    /// Where the frame now running started.
    next: Instant,
}

impl<'a> FrameWait<'a> {
    /// For the calling thread, which `changed` records to unpark.
    pub fn new(changed: &'a ScreenChanged, period: Duration) -> Self {
        *changed.thread.lock() = Some(std::thread::current());
        Self { changed, period, next: Instant::now() }
    }

    /// Wait out the frame's period; ends early if [`ScreenChanged::wake`] asks.
    pub fn wait(&mut self) {
        let mut now = Instant::now();
        let end = self.next + self.period;
        if now >= end {
            self.next = now;
            return;
        }
        loop {
            if self.changed.wake.swap(false, Ordering::AcqRel) {
                self.next = now;
                return;
            }
            std::thread::park_timeout(end - now);
            now = Instant::now();
            if now >= end {
                self.next = end;
                return;
            }
        }
    }

    /// With nothing marked, park until the display is marked,
    /// [`ScreenChanged::wake`] is called, or `timer` comes due, instead of
    /// waking every period to find nothing. A board whose retrace or other
    /// hardware duty has a time of its own passes it as `timer`.
    ///
    /// A mark ends the wait at the next vsync, not at once, so marks between
    /// two vsyncs make one frame; after a long park that vsync is long past and
    /// the frame starts at once.
    pub fn park_until_marked(&mut self, timer: Option<Instant>) -> Woke {
        let c = self.changed;
        let end = self.next + self.period;
        let woke = loop {
            if c.is_marked() {
                break Woke::Marked;
            }
            if c.wake.swap(false, Ordering::AcqRel) {
                break Woke::Requested;
            }
            let now = Instant::now();
            let wait = match timer {
                Some(t) if t <= now => break Woke::Timer,
                Some(t) => (t - now).min(PARK_RECHECK),
                None => PARK_RECHECK,
            };
            // Say so, then look again: a writer that marked before it could
            // see this is found by the look.
            c.parked.store(true, Ordering::Relaxed);
            fence(Ordering::SeqCst);
            if !c.is_marked() && !c.wake.load(Ordering::Acquire) {
                std::thread::park_timeout(wait);
            }
            c.parked.store(false, Ordering::Relaxed);
        };
        let now = Instant::now();
        if woke == Woke::Timer {
            // The board's own time: leave the slot as it is unless it ran out.
            if now >= end {
                self.next = now;
            }
        } else if end > now {
            std::thread::sleep(end - now);
            self.next = end;
        } else {
            self.next = now;
        }
        woke
    }
}

/// Whether the display thread tears its renderer down when it ends.
///
/// Releasing a renderer's GL context has to happen on the thread that made
/// it current, so the display thread does it as its last act. On macOS
/// glutin runs that release on the main thread, and the main thread is the
/// one stopping the machine when the window closes, waiting in the join.
/// Neither goes on, and the disks are never flushed. So a board calls
/// [`request`](Self::request) first thing in its stop, and its display
/// thread ends with [`finish`](Self::finish), which leaves the renderer
/// alone when the stop came from the main thread. That stop is final: the
/// process is ending, and the renderer cannot be set up again.
#[derive(Default)]
pub struct DisplayStop(AtomicBool);

impl DisplayStop {
    pub fn request(&self) {
        // std has no is-main-thread test. The platform ones exist where the
        // hang does (pthread_main_np on macOS), but Rust names the main
        // thread, and the name lets a test on Linux stand in for it.
        self.0.store(std::thread::current().name() == Some("main"), Ordering::Release);
    }

    pub fn finish(&self, renderer: &Mutex<Option<Box<dyn Renderer>>>) {
        if !self.0.load(Ordering::Acquire) {
            if let Some(r) = renderer.lock().as_mut() {
                r.stop();
            }
        }
    }
}

/// One composed picture of a screen, immutable once published in an `Arc`.
#[derive(Default)]
pub struct Frame {
    /// Counts up by one for each frame the screen publishes.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// `0xAABBGGRR`, `width` to a row, no padding. Empty when only `gpu` was
    /// needed.
    pub words: Vec<u32>,
    /// The same pixels in the same order on the screen's device.
    #[cfg(feature = "wgpu")]
    pub gpu: Option<::wgpu::Buffer>,
    /// Newport's debug overlay, only while it is on.
    pub overlay: Option<Vec<u32>>,
}

/// What the screen needs composed this tick. All false: nobody looks, so the
/// board composes nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Need {
    /// A pull consumer (a screenshot, VNC, the monitor) wants the frame's words.
    pub words: bool,
    /// The shown renderer takes frames on the board's device, and no pull
    /// consumer wants words.
    pub gpu: bool,
    /// A renderer is shown: the board redraws what changed, and at its idle
    /// rate so the status bar moves.
    pub redraw: bool,
}

impl Need {
    pub fn any(&self) -> bool {
        self.words || self.gpu || self.redraw
    }
}

/// The front-end's half of a screen: the renderer it installs, and which of
/// the machine's screens that renderer is showing.
#[derive(Default)]
pub struct Window {
    pub renderer: Mutex<Option<Box<dyn Renderer>>>,
    pub shown: AtomicUsize,
}

/// One head's output, shared by the board's display thread and every consumer.
/// The board side takes no lock but its own: it marks changes, reads requests
/// and publishes frames, and the consumers read.
pub struct Screen {
    /// Set by whatever the scan-out reads (see [`ScreenChanged`]).
    pub changed: Arc<ScreenChanged>,
    pub requests: FrameRequests,
    /// What the display thread did, for the monitor's `screen frames`.
    pub counts: FrameCounts,
    pub window: Arc<Window>,
    pub index: usize,
    /// Park the display thread while nobody looks, instead of ticking at the
    /// frame period. Only a board with no retrace duty may: its marks wake it.
    pub parks: bool,
    /// Compose at the idle rate with nobody looking, for a board whose
    /// compose also learns state others read (Newport's mode and cursor offset).
    pub always: bool,
    /// The host GPU the board draws or composes on, when it uses one: a wgpu
    /// window presents on the same device. Set once, when the board knows it.
    #[cfg(feature = "wgpu")]
    pub gpu: std::sync::OnceLock<Arc<crate::wgpu::Gpu>>,
    /// The frame size last published or set, packed height << 32 | width.
    size: AtomicU64,
    /// The requests the frame being composed will answer.
    noted: AtomicU64,
    /// Held only to clone or swap an `Arc`.
    latest: Mutex<Option<Arc<Frame>>>,
    /// A published frame's buffer, once nobody holds the frame.
    spare: Mutex<Vec<u32>>,
}

impl Screen {
    pub fn new(index: usize, window: Arc<Window>) -> Screen {
        Screen {
            changed: Arc::new(ScreenChanged::marked()),
            requests: FrameRequests::default(),
            counts: FrameCounts::default(),
            window,
            index,
            parks: false,
            always: false,
            #[cfg(feature = "wgpu")]
            gpu: std::sync::OnceLock::new(),
            size: AtomicU64::new(0),
            noted: AtomicU64::new(0),
            latest: Mutex::new(None),
            spare: Mutex::new(Vec::new()),
        }
    }

    /// What this tick has to compose: a request or a capturing client wants
    /// words, a renderer is shown (and takes device frames, when it does).
    pub fn need(&self) -> Need {
        let words = self.requests.pending() || self.requests.capturing();
        let renderer = self.window.renderer.lock();
        #[cfg(feature = "wgpu")]
        let gpu = !words && renderer.as_ref().is_some_and(|r| r.takes_gpu_frames());
        #[cfg(not(feature = "wgpu"))]
        let gpu = false;
        Need { words, gpu, redraw: self.always || renderer.is_some() }
    }

    /// A frame to compose into, `w` by `h` words, in a buffer no consumer
    /// holds. It notes the requests the frame will answer: take it before
    /// composing, and a request made meanwhile stays pending after
    /// [`publish`](Self::publish).
    pub fn frame(&self, w: u32, h: u32) -> Frame {
        self.noted.store(self.requests.note(), Ordering::Release);
        let mut words = std::mem::take(&mut *self.spare.lock());
        words.resize(w as usize * h as usize, 0);
        Frame { width: w, height: h, words, ..Frame::default() }
    }

    /// One tick of the display: compose if anything looks, and publish what
    /// `compose` filled (see [`run_display`]). Returns what was needed.
    pub fn tick(&self, compose: impl FnOnce(&mut Frame, Need) -> bool) -> Need {
        let need = self.need();
        if need.any() {
            let mut frame = self.frame(0, 0);
            if compose(&mut frame, need) {
                self.publish(frame);
            } else {
                // The picture stands; what was asked for is answered by it.
                self.requests.answer(self.noted.load(Ordering::Acquire));
            }
        } else {
            // Nothing looked, so a mark has no frame to make. Taken, so that
            // a parked thread sleeps on the next one.
            self.changed.take();
            self.counts.count(false);
        }
        need
    }

    /// The size of the picture the board shows, known before any frame is
    /// composed: what pointer steering needs. (0, 0) before the board has a mode.
    pub fn size(&self) -> (u32, u32) {
        let s = self.size.load(Ordering::Relaxed);
        (s as u32, (s >> 32) as u32)
    }

    /// Say what size the board's picture is, for a board that knows it
    /// without composing.
    pub fn set_size(&self, w: u32, h: u32) {
        self.size.store(u64::from(h) << 32 | u64::from(w), Ordering::Relaxed);
    }

    /// Store `f` as the newest frame and answer the requests it was noted for.
    pub fn publish(&self, mut f: Frame) {
        self.set_size(f.width, f.height);
        let old = {
            let mut latest = self.latest.lock();
            f.seq = latest.as_ref().map_or(0, |o| o.seq).wrapping_add(1);
            latest.replace(Arc::new(f))
        };
        if let Some(Ok(old)) = old.map(Arc::try_unwrap) {
            *self.spare.lock() = old.words;
        }
        self.requests.answer(self.noted.load(Ordering::Acquire));
    }

    /// The newest frame, if any has been published.
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.lock().clone()
    }

    /// Ask for a frame composed from now on, wait at most `timeout` for it and
    /// return the newest. When the wait ran out, that is an older frame and
    /// [`FrameRequests::pending`] still says so.
    pub fn capture(&self, timeout: Duration) -> Option<Arc<Frame>> {
        self.requests.request();
        self.changed.wake();
        let deadline = Instant::now() + timeout;
        while self.requests.pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        self.latest()
    }
}

/// The hotkey's screenshot: the screen's next frame, written as
/// `screenshot_NNNN.png` in the working directory off the caller's thread.
pub fn save_screenshot(screen: &Arc<Screen>) {
    static SHOTS: AtomicU64 = AtomicU64::new(0);
    let screen = screen.clone();
    std::thread::spawn(move || {
        let Some(frame) = screen.capture(Duration::from_secs(2)).filter(|f| f.width > 0 && f.height > 0) else {
            println!("iris: screenshot failed: the display has no frame");
            return;
        };
        let path = format!("screenshot_{:04}.png", SHOTS.fetch_add(1, Ordering::Relaxed));
        let (w, h) = (frame.width as usize, frame.height as usize);
        match crate::disp::write_png_words(&path, &frame.words, w, w, h) {
            Ok(()) => println!("iris: screenshot saved to {path}"),
            Err(e) => println!("iris: screenshot failed: {e}"),
        }
    });
}

/// The display thread of a board: until `running` clears, each frame period
/// it composes if anyone looks, runs the board's per-frame duty and waits out
/// the period. `compose` fills the frame it is given from the board's state
/// and returns whether it did; false means the picture is unchanged, and the
/// newest frame stands. A request is answered either way. The board counts
/// the frames it runs in [`Screen::counts`]; a tick nobody looks at is counted
/// here as skipped. `retrace` is the
/// board's hardware duty (the retrace pulse and its interrupt, trace flushes,
/// the status counters). It runs every tick, watched or not, after the frame
/// is taken, so a flip the guest makes at the retrace shows from the next one.
/// The thread ends by releasing the renderer ([`DisplayStop`]).
pub fn run_display(
    screen: &Screen,
    running: &AtomicBool,
    stop: &DisplayStop,
    mut retrace: impl FnMut(),
    mut compose: impl FnMut(&mut Frame, Need) -> bool,
) {
    let mut wait = FrameWait::new(&screen.changed, FRAME_PERIOD);
    while running.load(Ordering::Relaxed) {
        let need = screen.tick(&mut compose);
        retrace();
        if need.any() || !screen.parks {
            wait.wait();
        } else {
            wait.park_until_marked(Some(Instant::now() + Duration::from_millis(250)));
        }
    }
    stop.finish(&screen.window.renderer);
}

/// The hardware cursor: where the board draws its glyph and which of the
/// glyph's pixels are opaque. The glyph's top-left is the pointer position
/// minus the hotspot, which the hardware does not know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwCursor {
    /// Glyph top-left in display coordinates; negative near the top/left edge.
    pub x: i32,
    pub y: i32,
    /// Glyph side, 32 or 64.
    pub size: usize,
    /// One word per glyph row, bit `c` set where column `c` is opaque.
    pub mask: Vec<u64>,
}

impl HwCursor {
    /// The hotspot when nothing better is known: the first opaque pixel of
    /// the topmost opaque row, the tip of an arrow. (0, 0) for an empty glyph.
    pub fn tip(&self) -> (i32, i32) {
        self.mask.iter().enumerate()
            .find(|(_, &m)| m != 0)
            .map(|(y, &m)| (m.trailing_zeros() as i32, y as i32))
            .unwrap_or((0, 0))
    }

    /// A key that tells glyphs apart, to remember a learned hotspot per shape.
    pub fn shape_key(&self) -> u64 {
        // FNV-1a over the mask words.
        self.mask.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &m| (h ^ m).wrapping_mul(0x100_0000_01b3))
            ^ self.size as u64
    }
}

/// A display for tests: a screen of a given size whose requested frames are
/// composed at once, or never.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub(crate) struct FakeDisplay {
        stop: Arc<DisplayStop>,
        screens: [Arc<Screen>; 1],
        running: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeDisplay {
        /// A `w` by `h` screen; `stuck` never composes a requested frame.
        pub(crate) fn new(w: u32, h: u32, stuck: bool) -> Self {
            let screens = [Arc::new(Screen::new(0, Arc::default()))];
            // The picture a stuck board still has from before.
            let mut first = screens[0].frame(w, h);
            first.words.fill(0xff00_0000);
            screens[0].publish(first);
            let stop = Arc::new(DisplayStop::default());
            let running = Arc::new(AtomicBool::new(!stuck));
            let thread = (!stuck).then(|| {
                let (screen, stop, running) = (screens[0].clone(), stop.clone(), running.clone());
                std::thread::spawn(move || {
                    run_display(&screen, &running, &stop, || {}, |out, _| {
                        out.width = w;
                        out.height = h;
                        out.words.resize(w as usize * h as usize, 0);
                        true
                    })
                })
            });
            Self { stop, screens, running, thread }
        }
    }

    impl Drop for FakeDisplay {
        fn drop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            self.screens[0].changed.wake();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl GfxDisplay for FakeDisplay {
        fn screens(&self) -> &[Arc<Screen>] { &self.screens }
        fn display_stop(&self) -> &DisplayStop { &self.stop }
        fn cycles(&self) -> CyclesPtr { CyclesPtr::dangling() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen {
        Screen::new(0, Arc::default())
    }

    /// With nothing looking the board composes nothing, and a request, a
    /// capturing client or a renderer each bring the compose back.
    #[test]
    fn a_screen_nobody_looks_at_is_not_composed() {
        let s = screen();
        let composes = std::cell::Cell::new(0);
        let tick = |s: &Screen| s.tick(|f, _| { composes.set(composes.get() + 1); f.width = 1; f.height = 1; f.words.push(7); true });
        assert_eq!(tick(&s), Need::default());
        assert_eq!(composes.get(), 0);
        assert_eq!(s.counts.get(), (1, 0, 1), "a tick nobody looked at is counted as skipped");
        s.requests.request();
        assert!(tick(&s).words);
        assert_eq!(composes.get(), 1);
        assert!(!s.requests.pending());
        assert_eq!(s.latest().unwrap().words, [7]);
        s.requests.capture(true);
        assert!(tick(&s).words);
        s.requests.capture(false);
        assert!(!tick(&s).any());
        assert_eq!(composes.get(), 2);
        struct Quiet;
        impl Renderer for Quiet {
            fn present(&mut self, _: &mut crate::disp::Rex3Screen, _: &mut crate::debug_overlay::DebugOverlay, _: &mut crate::disp::StatusBar,
                _: &mut crate::disp::StatusBarTexture, _: &crate::disp::BarStats, _: bool, _: Option<&[u32]>, _: Option<&[u32]>) {}
        }
        *s.window.renderer.lock() = Some(Box::new(Quiet));
        assert_eq!(tick(&s), Need { words: false, gpu: false, redraw: true });
        assert_eq!(composes.get(), 3);
    }

    /// A request made while a frame is composed is not in it: it stays pending
    /// after that frame is published, and the next frame answers it.
    #[test]
    fn a_request_made_during_a_compose_is_answered_by_the_next_frame() {
        let s = screen();
        s.requests.request();
        s.tick(|f, _| {
            f.words.push(1);
            f.width = 1;
            f.height = 1;
            s.requests.request();
            true
        });
        assert!(s.requests.pending(), "the second request needs a frame composed after it");
        assert_eq!(s.latest().unwrap().seq, 1);
        s.tick(|f, _| { f.words.push(2); f.width = 1; f.height = 1; true });
        assert!(!s.requests.pending());
        assert_eq!(s.latest().unwrap().words, [2]);
        assert_eq!(s.latest().unwrap().seq, 2);
        // An unchanged picture still answers the request.
        s.requests.request();
        s.tick(|_, _| false);
        assert!(!s.requests.pending());
        assert_eq!(s.latest().unwrap().seq, 2, "and the newest frame stands");
    }

    /// A frame a consumer holds is never overwritten; its buffer comes back
    /// once nobody holds it.
    #[test]
    fn a_held_frame_is_not_reused() {
        let s = screen();
        let mut f = s.frame(2, 1);
        f.words.copy_from_slice(&[1, 2]);
        s.publish(f);
        let held = s.latest().unwrap();
        let mut f = s.frame(2, 1);
        f.words.copy_from_slice(&[3, 4]);
        s.publish(f);
        assert_eq!(held.words, [1, 2]);
        assert_eq!(s.latest().unwrap().words, [3, 4]);
        assert_eq!(s.size(), (2, 1));
    }

    /// The thread composes while a consumer waits, answers a capture, and ends
    /// by releasing the renderer unless the stop came from the main thread.
    #[test]
    fn run_display_serves_a_capture_and_stops() {
        let s = Arc::new(screen());
        let running = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(DisplayStop::default());
        let t = {
            let (s, running, stop) = (s.clone(), running.clone(), stop.clone());
            std::thread::spawn(move || run_display(&s, &running, &stop, || {}, |f, _| {
                f.width = 2;
                f.height = 1;
                f.words.extend([5, 6]);
                true
            }))
        };
        let f = s.capture(Duration::from_secs(5)).expect("a frame");
        assert!(!s.requests.pending());
        assert_eq!(f.words, [5, 6]);
        running.store(false, Ordering::SeqCst);
        s.changed.wake();
        t.join().unwrap();
    }

    #[test]
    fn a_mark_is_taken_once_and_every_mark_stores() {
        let c = ScreenChanged::default();
        assert!(!c.take());
        c.mark();
        c.mark();
        assert!(c.is_marked());
        assert!(c.take());
        assert!(!c.take());
        assert!(ScreenChanged::marked().take());
    }

    #[test]
    fn a_request_made_while_a_frame_is_composed_is_still_pending_after_it() {
        let r = FrameRequests::default();
        assert!(!r.pending());
        r.request();
        let noted = r.note();
        assert!(r.pending() && r.answers(noted));
        // Asked again while that frame is composed.
        r.request();
        r.answer(noted);
        assert!(r.pending(), "the second request needs a frame composed after it");
        assert!(!r.answers(noted));
        r.answer(r.note());
        assert!(!r.pending());
    }

    #[test]
    fn capture_calls_nest() {
        let r = FrameRequests::default();
        r.capture(true);
        r.capture(true);
        r.capture(false);
        assert!(r.capturing());
        r.capture(false);
        r.capture(false);
        assert!(!r.capturing(), "an extra off does not go below none");
    }

    #[test]
    fn the_counts_say_what_the_frames_did_and_reset() {
        let n = FrameCounts::default();
        for composed in [true, false, false, false] {
            n.count(composed);
        }
        assert_eq!(n.get(), (4, 1, 3));
        n.reset();
        assert_eq!(n.get(), (0, 0, 0));
    }

    #[test]
    fn waiting_out_the_frame_takes_the_period_and_does_not_drift() {
        let c = ScreenChanged::default();
        let start = Instant::now();
        let mut w = FrameWait::new(&c, Duration::from_millis(20));
        for _ in 0..4 {
            // A frame that takes a quarter of the period.
            std::thread::sleep(Duration::from_millis(5));
            w.wait();
        }
        assert!(start.elapsed() >= Duration::from_millis(80), "four periods, not four frames' work");
    }

    #[test]
    fn a_late_frame_resyncs_instead_of_running_the_missed_ones_back_to_back() {
        let c = ScreenChanged::default();
        let mut w = FrameWait::new(&c, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(70));
        w.wait();
        let t = Instant::now();
        w.wait();
        assert!(t.elapsed() >= Duration::from_millis(19), "the next slot is a period away, not the missed ones");
    }

    /// Run `f` on a thread with a wait, and return once it is parked (or done).
    fn parked_thread<T: Send + 'static>(
        c: &Arc<ScreenChanged>,
        period: Duration,
        f: impl FnOnce(&mut FrameWait<'_>) -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        let q = c.clone();
        let t = std::thread::spawn(move || f(&mut FrameWait::new(&q, period)));
        while !c.parked.load(Ordering::Relaxed) && !t.is_finished() {
            std::thread::yield_now();
        }
        t
    }

    #[test]
    fn a_wake_ends_a_wait_at_once() {
        let c = Arc::new(ScreenChanged::default());
        let q = c.clone();
        let t = std::thread::spawn(move || {
            let mut w = FrameWait::new(&q, Duration::from_secs(30));
            let t = Instant::now();
            w.wait();
            t.elapsed()
        });
        while !t.is_finished() {
            c.wake();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(t.join().unwrap() < Duration::from_secs(10));
    }

    #[test]
    fn a_parked_display_wakes_on_a_mark_and_not_before() {
        let c = Arc::new(ScreenChanged::default());
        let t = parked_thread(&c, Duration::from_millis(1), |w| w.park_until_marked(None));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished(), "nothing marked, so it stays parked");
        c.mark();
        assert_eq!(t.join().unwrap(), Woke::Marked);
        assert!(c.take(), "the mark is still up for the frame to take");
    }

    #[test]
    fn a_wake_ends_a_park() {
        let c = Arc::new(ScreenChanged::default());
        let t = parked_thread(&c, Duration::from_millis(1), |w| w.park_until_marked(None));
        c.wake();
        assert_eq!(t.join().unwrap(), Woke::Requested);
    }

    #[test]
    fn marks_between_two_vsyncs_make_one_frame() {
        let c = ScreenChanged::default();
        let start = Instant::now();
        let mut w = FrameWait::new(&c, Duration::from_millis(20));
        // Frame one runs at the start; four marks follow within its period.
        for _ in 0..4 {
            c.mark();
        }
        assert_eq!(w.park_until_marked(None), Woke::Marked);
        assert!(start.elapsed() >= Duration::from_millis(20), "the frame starts at the vsync, not at the mark");
        assert!(c.take());
        c.mark();
        assert_eq!(w.park_until_marked(None), Woke::Marked);
        assert!(start.elapsed() >= Duration::from_millis(40), "and the next one a period after that");
    }

    /// After a long park the vsync is long past: the frame starts at once and
    /// the next slot is a period from now, not from where the clock had got to.
    #[test]
    fn a_mark_after_a_long_park_starts_the_frame_at_once() {
        let c = ScreenChanged::default();
        let mut w = FrameWait::new(&c, Duration::from_millis(20));
        w.next -= Duration::from_secs(1);
        c.mark();
        let t = Instant::now();
        assert_eq!(w.park_until_marked(None), Woke::Marked);
        assert!(w.next >= t, "the slot starts from now");
    }

    #[test]
    fn a_board_timer_wakes_a_parked_display_when_it_is_due() {
        let c = ScreenChanged::default();
        let mut w = FrameWait::new(&c, Duration::from_millis(20));
        let due = Instant::now() + Duration::from_millis(40);
        assert_eq!(w.park_until_marked(Some(due)), Woke::Timer);
        assert!(Instant::now() >= due);
        assert!(!c.take());
    }
}
