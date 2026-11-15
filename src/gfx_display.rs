//! What the UI, CI socket and GUI front-end need from whichever graphics board
//! drives the screen.
//!
//! A board owns its display thread and its composition; the thread is
//! [`run_display`] with the board's closures. What the front-end sees is one
//! [`Screen`] per head: the renderer slot it installs into, the newest
//! composed frame (screenshots, VNC, the monitor), the requests it makes for
//! a fresh one, and the counts of what the thread did. [`GfxDisplay`] only
//! hands the screens out, with the hardware cursor and the CPU cycle counter.

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::screen::Ng1Screen;
use crate::disp::BarStats;

/// What the front-end draws a screen's frames with: the GL window, the wgpu
/// window, iris-gui's capture. It owns the status bar and draws it under the
/// frame, and it never reads the picture back: the consumers that want words
/// ask the screen for them ([`Need::words`]).
///
/// [`Screen::publish`] calls [`present`](Self::present) on the board's display
/// thread, once for each new frame and again at the idle rate for an unchanged
/// one (same [`Frame::seq`]), so the status bar moves. A renderer skips the
/// upload of a frame it has already seen.
pub trait Renderer: Send {
    fn present(&mut self, frame: &Frame, stats: &BarStats);

    /// The device this renderer presents on, if it takes frames composed
    /// there ([`Frame::gpu`]).
    #[cfg(feature = "wgpu")]
    fn gpu(&self) -> Option<&Arc<crate::wgpu::Gpu>> { None }

    /// Whether it composes Newport's planes itself ([`Frame::planes`]). Only
    /// the GL window does, in a fragment shader: the CPU compose it would
    /// otherwise be handed costs 7.4 ms for each changed Newport frame.
    fn takes_planes(&self) -> bool { false }

    fn resize(&mut self, _width: usize, _height: usize) {}

    /// The display thread is ending (or the screen is no longer shown here):
    /// release what only that thread may.
    fn stop(&mut self) {}

    /// The test-only resident path presents a frame already on the device,
    /// and reports its failure rather than logging it.
    #[cfg(all(test, feature = "wgpu"))]
    fn present_resident_gpu_frame(&mut self, _frame: &Frame, _stats: &BarStats) -> Result<(), String> {
        Err("renderer does not support resident GPU frames".into())
    }
}

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
    /// The same pixels in the same order on the screen's device, `0xAABBGGRR`.
    #[cfg(feature = "wgpu")]
    pub gpu: Option<::wgpu::Buffer>,
    /// Newport's planes and display state, for the one presenter that composes
    /// them itself; `words` is then empty. Only [`Need::planes`] asks for it.
    pub planes: Option<Ng1Screen>,
    /// Newport's debug overlay, `width` to a row in the same order as `words`
    /// (transparent where nothing is shown), only while it is on.
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
    /// The shown renderer composes Newport's planes itself, and no pull
    /// consumer wants words.
    pub planes: bool,
    /// A renderer is shown: the board redraws what changed, and at its idle
    /// rate so the status bar moves.
    pub redraw: bool,
}

impl Need {
    pub fn any(&self) -> bool {
        self.words || self.gpu || self.planes || self.redraw
    }
}

/// The front-end's half of a screen: the renderer it installs, and which of
/// the machine's screens that renderer is showing.
///
/// A window that shows several screens in turn keeps one renderer, in the
/// slot of the screen it shows now. A GL renderer is bound to the thread that
/// presents with it, so a move between screens is done by the thread that is
/// giving the renderer up ([`pass_to`](Self::pass_to), [`hand_over`](Self::hand_over)).
#[derive(Default)]
pub struct Window {
    pub renderer: Mutex<Option<Box<dyn Renderer>>>,
    /// The screen this slot belongs to, as its place in the machine's list.
    pub shown: AtomicUsize,
    /// How many screens the machine has, when the status bar should name the
    /// one shown (`screen 2/2`); 0 for no name.
    pub of: AtomicUsize,
    /// Where the renderer goes when this screen's thread next ticks.
    next: Mutex<Option<Arc<Window>>>,
    /// Another screen drew into the picture this slot's renderer shows: show
    /// the newest frame again at the next tick (see `side_by_side`).
    poked: AtomicBool,
}

impl Window {
    /// Ask this screen's display thread to give the renderer to `next`.
    pub fn pass_to(&self, next: Arc<Window>) {
        *self.next.lock() = Some(next);
    }

    /// A [`pass_to`](Self::pass_to) is waiting for the display thread.
    pub fn passing(&self) -> bool {
        self.next.lock().is_some()
    }

    /// Done on the display thread of the screen that holds the renderer, at
    /// the top of its tick: release what only this thread may and move the
    /// renderer to the slot it was passed to. A request that arrives while
    /// the renderer is elsewhere stays until it is here.
    pub fn hand_over(&self) {
        let mut mine = self.renderer.lock();
        if mine.is_none() {
            return;
        }
        let Some(next) = self.next.lock().take() else { return };
        let mut r = mine.take().expect("checked above");
        r.stop();
        *next.renderer.lock() = Some(r);
    }

    /// Show the newest frame again at the next tick.
    pub fn poke(&self) {
        self.poked.store(true, Ordering::Release);
    }

    /// The status bar's "screen n of m", when this slot names its screen.
    pub fn label(&self) -> Option<(usize, usize)> {
        let of = self.of.load(Ordering::Relaxed);
        (of > 1).then(|| (self.shown.load(Ordering::Relaxed) + 1, of))
    }
}

/// The hotkey's order through a machine's screens when one window shows one
/// screen at a time. Lives on the event thread.
pub struct Cycle {
    screens: Vec<Arc<Screen>>,
    cur: usize,
}

impl Cycle {
    /// `None` for a machine with one screen: nothing to cycle.
    pub fn new(screens: &[Arc<Screen>]) -> Option<Cycle> {
        (screens.len() > 1).then(|| Cycle { screens: screens.to_vec(), cur: 0 })
    }

    /// How many screens it steps through.
    pub fn screen_count(&self) -> usize {
        self.screens.len()
    }

    /// The screen after `cur` of `n`, wrapping.
    pub fn after(cur: usize, n: usize) -> usize {
        (cur + 1) % n
    }

    /// The screen the window shows (or is about to).
    pub fn current(&self) -> &Arc<Screen> {
        &self.screens[self.cur]
    }

    /// Move the window to the next screen. A press while the last move is
    /// still waiting for its display thread is ignored, so the renderer never
    /// has two pending moves. Returns the new screen's index when it moved.
    pub fn step(&mut self) -> Option<usize> {
        if self.screens.iter().any(|s| s.window.passing()) {
            return None;
        }
        let to = Self::after(self.cur, self.screens.len());
        self.screens[self.cur].window.pass_to(self.screens[to].window.clone());
        // The thread gives it up at its next tick; do not wait for a parked one.
        self.screens[self.cur].changed.wake();
        self.cur = to;
        Some(to)
    }
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
    /// The size last given to the renderer, packed the same way.
    sized: AtomicU64,
    /// Which renderer was last shown a frame (its address), so one installed
    /// since gets the newest frame at once.
    shown: AtomicUsize,
    /// Ticks since a renderer was last shown a frame.
    idle: AtomicU32,
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
            sized: AtomicU64::new(0),
            shown: AtomicUsize::new(0),
            idle: AtomicU32::new(0),
            noted: AtomicU64::new(0),
            latest: Mutex::new(None),
            spare: Mutex::new(Vec::new()),
        }
    }

    /// What this tick has to compose: a request or a capturing client wants
    /// words; a renderer is shown (and takes device frames or Newport's
    /// planes, when it does and nobody wants words).
    pub fn need(&self) -> Need {
        let words = self.requests.pending() || self.requests.capturing();
        let renderer = self.window.renderer.lock();
        let renderer = renderer.as_ref();
        // A frame on a device is valid only there: the board's own, when it
        // has one, else whichever one the window presents on.
        #[cfg(feature = "wgpu")]
        let gpu = !words
            && renderer.is_some_and(|r| r.gpu().is_some_and(|d| self.gpu.get().is_none_or(|g| Arc::ptr_eq(g, d))));
        #[cfg(not(feature = "wgpu"))]
        let gpu = false;
        let planes = !words && renderer.is_some_and(|r| r.takes_planes());
        Need { words, gpu, planes, redraw: self.always || renderer.is_some() }
    }

    /// The device the shown renderer presents on, for a board that composes
    /// there and has no device of its own: it keeps that one from now on.
    #[cfg(feature = "wgpu")]
    pub fn adopt_window_gpu(&self) -> Option<&Arc<crate::wgpu::Gpu>> {
        if self.gpu.get().is_none() {
            if let Some(d) = self.window.renderer.lock().as_ref().and_then(|r| r.gpu().cloned()) {
                let _ = self.gpu.set(d);
            }
        }
        self.gpu.get()
    }

    /// A frame to compose into, `w` by `h` words, in a buffer no consumer
    /// holds. It notes the requests the frame will answer: take it before
    /// composing, and a request made meanwhile stays pending after
    /// [`publish`](Self::publish).
    pub fn frame(&self, w: u32, h: u32) -> Frame {
        self.noted.store(self.requests.note(), Ordering::Release);
        self.spare_frame(w, h)
    }

    fn spare_frame(&self, w: u32, h: u32) -> Frame {
        let mut words = std::mem::take(&mut *self.spare.lock());
        words.resize(w as usize * h as usize, 0);
        Frame { width: w, height: h, words, ..Frame::default() }
    }

    /// One tick of the display: compose if anything looks, and publish what
    /// `compose` filled (see [`run_display`]). Returns what was needed. `stats`
    /// is the status bar's input, taken only for a frame a renderer is shown.
    pub fn tick(&self, mut stats: impl FnMut() -> BarStats, compose: impl FnOnce(&mut Frame, Need) -> bool) -> Need {
        self.window.hand_over();
        // Note the requests before reading what is needed: one that arrives
        // after the note is not answered by this frame, so it cannot be
        // answered by one composed without words.
        self.noted.store(self.requests.note(), Ordering::Release);
        let need = self.need();
        if need.any() {
            let mut frame = self.spare_frame(0, 0);
            if compose(&mut frame, need) {
                // A tick with nobody shown has no use for the bar's figures.
                let stats = if need.redraw { self.bar(&mut stats) } else { self.idle_stats() };
                self.publish(frame, &stats);
            } else {
                // The picture stands; what was asked for is answered by it,
                // and a renderer sees it again at the idle rate.
                *self.spare.lock() = frame.words;
                self.requests.answer(self.noted.load(Ordering::Acquire));
                if need.redraw {
                    self.repeat(&mut stats);
                }
            }
        } else {
            // Nothing looked, so a mark has no frame to make. Taken, so that
            // a parked thread sleeps on the next one.
            self.changed.take();
            self.counts.count(false);
        }
        need
    }

    /// The board's figures for the status bar, with this screen's name when
    /// the window shows several.
    fn bar(&self, stats: &mut impl FnMut() -> BarStats) -> BarStats {
        let mut s = stats();
        s.screen = self.window.label();
        s
    }

    fn idle_stats(&self) -> BarStats {
        crate::disp::StatusSource::detached().snapshot(Instant::now(), 0)
    }

    /// Show the newest frame to the renderer again, so its status bar moves:
    /// every [`IDLE_REFRESH`] ticks, and at once to a renderer that has not
    /// seen it.
    fn repeat(&self, stats: &mut impl FnMut() -> BarStats) {
        let Some(frame) = self.latest() else { return };
        let mut renderer = self.window.renderer.lock();
        let Some(renderer) = renderer.as_mut() else { return };
        let idle = self.idle.fetch_add(1, Ordering::Relaxed) + 1;
        let poked = self.window.poked.swap(false, Ordering::AcqRel);
        if !poked && idle < IDLE_REFRESH && self.shown.load(Ordering::Relaxed) == renderer_id(&**renderer) {
            return;
        }
        let bar = self.bar(stats);
        self.show(renderer, &frame, &bar);
    }

    /// Hand `frame` to `renderer`, telling it first when the size changed.
    fn show(&self, renderer: &mut Box<dyn Renderer>, frame: &Frame, stats: &BarStats) {
        self.idle.store(0, Ordering::Relaxed);
        let arrived = self.shown.swap(renderer_id(&**renderer), Ordering::Relaxed) != renderer_id(&**renderer);
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let size = u64::from(frame.height) << 32 | u64::from(frame.width);
        if self.sized.swap(size, Ordering::Relaxed) != size || arrived {
            renderer.resize(frame.width as usize, frame.height as usize);
        }
        renderer.present(frame, stats);
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

    /// Store `f` as the newest frame, show it to the renderer if one is
    /// installed, and answer the requests it was noted for.
    pub fn publish(&self, mut f: Frame, stats: &BarStats) {
        self.set_size(f.width, f.height);
        let (new, old) = {
            let mut latest = self.latest.lock();
            f.seq = latest.as_ref().map_or(0, |o| o.seq).wrapping_add(1);
            let new = Arc::new(f);
            (new.clone(), latest.replace(new))
        };
        if let Some(renderer) = self.window.renderer.lock().as_mut() {
            self.show(renderer, &new, stats);
        }
        drop(new);
        if let Some(Ok(old)) = old.map(Arc::try_unwrap) {
            *self.spare.lock() = old.words;
        }
        self.requests.answer(self.noted.load(Ordering::Acquire));
    }

    /// The newest frame, if any has been published.
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.lock().clone()
    }

    /// The newest frame that has a picture in `words`: a frame composed only
    /// on a device (for a wgpu window) does not.
    pub fn latest_words(&self) -> Option<Arc<Frame>> {
        self.latest().filter(|f| f.width > 0 && f.height > 0 && f.words.len() == f.width as usize * f.height as usize)
    }

    /// Ask for a frame composed from now on, wait at most `timeout` for it and
    /// return the newest with its words. When the wait ran out, that is an
    /// older frame (or none) and [`FrameRequests::pending`] still says so.
    pub fn capture(&self, timeout: Duration) -> Option<Arc<Frame>> {
        // Held until the frame is read, so no frame without words replaces the
        // answer in between.
        self.requests.capture(true);
        self.requests.request();
        self.changed.wake();
        let deadline = Instant::now() + timeout;
        while self.requests.pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let frame = self.latest_words();
        self.requests.capture(false);
        frame
    }
}

/// A renderer's identity: its address.
fn renderer_id(r: &dyn Renderer) -> usize {
    r as *const dyn Renderer as *const () as usize
}

/// The hotkey's screenshot: the screen's next frame, written as
/// `screenshot_NNNN.png` in the working directory off the caller's thread.
pub fn save_screenshot(screen: &Arc<Screen>) {
    static SHOTS: AtomicU64 = AtomicU64::new(0);
    let screen = screen.clone();
    std::thread::spawn(move || {
        let Some(frame) = screen.capture(Duration::from_secs(2)) else {
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
/// `stats` is the status bar's input; it is called for each frame a renderer is
/// shown.
/// The thread ends by releasing the renderer ([`DisplayStop`]).
pub fn run_display(
    screen: &Screen,
    running: &AtomicBool,
    stop: &DisplayStop,
    mut stats: impl FnMut() -> BarStats,
    mut retrace: impl FnMut(),
    mut compose: impl FnMut(&mut Frame, Need) -> bool,
) {
    let mut wait = FrameWait::new(&screen.changed, FRAME_PERIOD);
    while running.load(Ordering::Relaxed) {
        let need = screen.tick(&mut stats, &mut compose);
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
            screens[0].publish(first, &crate::disp::StatusSource::detached().snapshot(Instant::now(), 0));
            let stop = Arc::new(DisplayStop::default());
            let running = Arc::new(AtomicBool::new(!stuck));
            let thread = (!stuck).then(|| {
                let (screen, stop, running) = (screens[0].clone(), stop.clone(), running.clone());
                std::thread::spawn(move || {
                    run_display(&screen, &running, &stop, || crate::disp::StatusSource::detached().snapshot(Instant::now(), 0), || {}, |out, _| {
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

    fn stats() -> BarStats {
        crate::disp::StatusSource::detached().snapshot(Instant::now(), 0)
    }

    /// A renderer that notes what it was shown: (seq, width, height) of each
    /// present, and each size it was told.
    #[derive(Clone, Default)]
    struct Shown {
        presents: Arc<Mutex<Vec<(u64, u32, u32)>>>,
        sizes: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    impl Renderer for Shown {
        fn present(&mut self, f: &Frame, _: &BarStats) {
            self.presents.lock().push((f.seq, f.width, f.height));
        }
        fn resize(&mut self, w: usize, h: usize) {
            self.sizes.lock().push((w, h));
        }
    }

    /// With nothing looking the board composes nothing, and a request, a
    /// capturing client or a renderer each bring the compose back.
    #[test]
    fn a_screen_nobody_looks_at_is_not_composed() {
        let s = screen();
        let composes = std::cell::Cell::new(0);
        let tick = |s: &Screen| s.tick(stats, |f, _| { composes.set(composes.get() + 1); f.width = 1; f.height = 1; f.words.push(7); true });
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
        *s.window.renderer.lock() = Some(Box::new(Shown::default()));
        assert_eq!(tick(&s), Need { words: false, gpu: false, planes: false, redraw: true });
        assert_eq!(composes.get(), 3);
    }

    /// A request made while a frame is composed is not in it: it stays pending
    /// after that frame is published, and the next frame answers it.
    #[test]
    fn a_request_made_during_a_compose_is_answered_by_the_next_frame() {
        let s = screen();
        s.requests.request();
        s.tick(stats, |f, _| {
            f.words.push(1);
            f.width = 1;
            f.height = 1;
            s.requests.request();
            true
        });
        assert!(s.requests.pending(), "the second request needs a frame composed after it");
        assert_eq!(s.latest().unwrap().seq, 1);
        s.tick(stats, |f, _| { f.words.push(2); f.width = 1; f.height = 1; true });
        assert!(!s.requests.pending());
        assert_eq!(s.latest().unwrap().words, [2]);
        assert_eq!(s.latest().unwrap().seq, 2);
        // An unchanged picture still answers the request.
        s.requests.request();
        s.tick(stats, |_, _| false);
        assert!(!s.requests.pending());
        assert_eq!(s.latest().unwrap().seq, 2, "and the newest frame stands");
    }

    /// Asks for words as the renderer is asked what it takes, as a consumer
    /// connecting at that moment would.
    struct AsksForWords(Arc<Screen>);

    impl Renderer for AsksForWords {
        fn present(&mut self, _: &Frame, _: &BarStats) {}
        fn takes_planes(&self) -> bool {
            self.0.requests.request();
            true
        }
    }

    /// A request that arrives while the tick decides what to compose is not
    /// answered by a frame composed without words.
    #[test]
    fn a_request_made_as_the_need_is_read_gets_a_frame_with_words() {
        let s = Arc::new(screen());
        *s.window.renderer.lock() = Some(Box::new(AsksForWords(s.clone())));
        let need = s.tick(stats, |f, need| { assert!(!need.words); f.width = 1; f.height = 1; true });
        assert!(need.planes);
        assert!(s.requests.pending(), "the planes-only frame does not answer it");
        let need = s.tick(stats, |f, need| { assert!(need.words); f.width = 1; f.height = 1; f.words.push(9); true });
        assert!(need.words);
        assert!(!s.requests.pending(), "the frame with words answers it");
        assert_eq!(s.latest().unwrap().words, [9]);
    }

    /// A frame composed only on a device has no picture for a consumer that
    /// reads words, and capture asks for one rather than return it.
    #[test]
    fn words_consumers_never_see_a_frame_without_words() {
        let s = Arc::new(screen());
        let mut f = s.frame(2, 1);
        f.words.clear();
        s.publish(f, &stats());
        assert!(s.latest().is_some() && s.latest_words().is_none());
        let running = Arc::new(AtomicBool::new(true));
        let t = {
            let (s, running) = (s.clone(), running.clone());
            std::thread::spawn(move || {
                run_display(&s, &running, &DisplayStop::default(), stats, || {}, |f, need| {
                    f.width = 2;
                    f.height = 1;
                    if need.words {
                        f.words.extend([3, 4]);
                    }
                    true
                })
            })
        };
        let f = s.capture(Duration::from_secs(5)).expect("a frame with words");
        assert_eq!(f.words, [3, 4]);
        assert!(!s.requests.capturing(), "the capture is released");
        running.store(false, Ordering::SeqCst);
        s.changed.wake();
        t.join().unwrap();
    }

    /// A frame a consumer holds is never overwritten; its buffer comes back
    /// once nobody holds it.
    #[test]
    fn a_held_frame_is_not_reused() {
        let s = screen();
        let mut f = s.frame(2, 1);
        f.words.copy_from_slice(&[1, 2]);
        s.publish(f, &stats());
        let held = s.latest().unwrap();
        let mut f = s.frame(2, 1);
        f.words.copy_from_slice(&[3, 4]);
        s.publish(f, &stats());
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
            std::thread::spawn(move || run_display(&s, &running, &stop, stats, || {}, |f, _| {
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

    /// A frame reaches the renderer once, with the size told first; an unchanged
    /// picture is shown again only at the idle rate, and a renderer installed
    /// later gets the newest frame at once.
    #[test]
    fn a_renderer_is_shown_each_new_frame_and_the_old_one_at_the_idle_rate() {
        let s = screen();
        let shown = Shown::default();
        *s.window.renderer.lock() = Some(Box::new(shown.clone()));
        s.tick(stats, |f, _| { f.width = 4; f.height = 2; f.words.resize(8, 1); true });
        assert_eq!(*shown.sizes.lock(), [(4, 2)]);
        assert_eq!(*shown.presents.lock(), [(1, 4, 2)]);
        for _ in 0..IDLE_REFRESH - 1 {
            s.tick(stats, |_, _| false);
        }
        assert_eq!(shown.presents.lock().len(), 1, "an unchanged picture waits for the idle rate");
        s.tick(stats, |_, _| false);
        assert_eq!(*shown.presents.lock(), [(1, 4, 2), (1, 4, 2)], "the same frame again, so the status bar moves");
        assert_eq!(shown.sizes.lock().len(), 1, "the size is told once");
        // A second renderer has not seen the frame.
        let second = Shown::default();
        *s.window.renderer.lock() = Some(Box::new(second.clone()));
        s.tick(stats, |_, _| false);
        assert_eq!(*second.presents.lock(), [(1, 4, 2)]);
        // A new size is told before its frame.
        s.tick(stats, |f, _| { f.width = 2; f.height = 2; f.words.resize(4, 2); true });
        assert_eq!(*second.sizes.lock(), [(4, 2), (2, 2)]);
    }

    /// A renderer that says where it was shown and where it was stopped.
    #[derive(Clone, Default)]
    struct Where {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Renderer for Where {
        fn present(&mut self, f: &Frame, stats: &BarStats) {
            self.log.lock().push(format!("present {}x{} {:?}", f.width, f.height, stats.screen));
        }
        fn stop(&mut self) {
            self.log.lock().push("stop".into());
        }
    }

    fn three() -> Vec<Arc<Screen>> {
        (0..3)
            .map(|i| {
                let s = Arc::new(Screen::new(i, Arc::default()));
                s.window.shown.store(i, Ordering::Relaxed);
                s.window.of.store(3, Ordering::Relaxed);
                s
            })
            .collect()
    }

    #[test]
    fn the_hotkey_walks_the_screens_in_order_and_wraps() {
        assert_eq!((0..5).scan(0, |c, _| { *c = Cycle::after(*c, 3); Some(*c) }).collect::<Vec<_>>(), [1, 2, 0, 1, 2]);
        assert!(Cycle::new(&three()[..1]).is_none(), "one screen has nothing to cycle");
    }

    /// The renderer leaves a screen from that screen's own thread, is stopped
    /// there, and the status bar names the screen it arrives at.
    #[test]
    fn the_window_follows_the_hotkey_one_hand_over_at_a_time() {
        let screens = three();
        let r = Where::default();
        *screens[0].window.renderer.lock() = Some(Box::new(r.clone()));
        let mut cycle = Cycle::new(&screens).unwrap();
        let show = |s: &Screen, w: u32| s.tick(stats, |f, _| { f.width = w; f.height = 1; f.words.resize(w as usize, 0); true });
        show(&screens[0], 4);
        assert_eq!(r.log.lock().as_slice(), ["present 4x1 Some((1, 3))"]);
        // A press asks; nothing moves until the holder's thread ticks.
        assert_eq!(cycle.step(), Some(1));
        assert!(screens[0].window.renderer.lock().is_some());
        assert_eq!(cycle.step(), None, "a press during a pending move is ignored");
        // A screen that does not hold the renderer shows nothing.
        show(&screens[1], 8);
        assert_eq!(r.log.lock().len(), 1);
        show(&screens[0], 4);
        assert!(screens[0].window.renderer.lock().is_none());
        assert_eq!(r.log.lock().as_slice(), ["present 4x1 Some((1, 3))", "stop"]);
        show(&screens[1], 8);
        assert_eq!(r.log.lock().last().unwrap(), "present 8x1 Some((2, 3))");
        assert!(Arc::ptr_eq(cycle.current(), &screens[1]));
        // Around the ring: 1 -> 2 -> 0.
        for (from, to) in [(1, 2), (2, 0)] {
            assert_eq!(cycle.step(), Some(to));
            show(&screens[from], 2);
        }
        assert!(screens[0].window.renderer.lock().is_some());
        assert!(screens[1].window.renderer.lock().is_none() && screens[2].window.renderer.lock().is_none());
    }

    /// A move asked of a screen that does not hold the renderer yet waits for
    /// it, so two quick presses end where they were headed.
    #[test]
    fn a_move_queued_before_the_renderer_arrives_waits_for_it() {
        let screens = three();
        *screens[0].window.renderer.lock() = Some(Box::new(Where::default()));
        screens[0].window.pass_to(screens[1].window.clone());
        screens[1].window.pass_to(screens[2].window.clone());
        screens[1].window.hand_over();
        assert!(screens[1].window.passing(), "nothing to move yet");
        screens[0].window.hand_over();
        screens[1].window.hand_over();
        assert!(screens[2].window.renderer.lock().is_some());
        assert!(!screens.iter().any(|s| s.window.passing()));
    }

    #[test]
    fn a_window_names_its_screen_only_when_there_are_several() {
        let w = Window::default();
        assert_eq!(w.label(), None);
        w.shown.store(1, Ordering::Relaxed);
        w.of.store(2, Ordering::Relaxed);
        assert_eq!(w.label(), Some((2, 2)));
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

    /// Asserts on the deadline the wait computed, not on how long the next
    /// wait takes: a thread descheduled after the resync would otherwise find
    /// the next slot nearer than a period and fail.
    #[test]
    fn a_late_frame_resyncs_instead_of_running_the_missed_ones_back_to_back() {
        let c = ScreenChanged::default();
        let period = Duration::from_millis(20);
        let mut w = FrameWait::new(&c, period);
        let first_slot = w.next + period;
        std::thread::sleep(Duration::from_millis(70));
        let before = Instant::now();
        w.wait();
        let after = Instant::now();
        assert!(w.next >= before && w.next <= after, "the slot restarts from the moment of the resync");
        assert!(w.next > first_slot + period, "not from one of the slots it missed");
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
