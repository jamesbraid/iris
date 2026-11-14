//! What the board shows: the scanout's colour lookup and gamma, the retrace
//! with its swap latching, and the window iris presents the frame in.

use super::pipe::backend::DestMem;
use super::board::Board;
use super::consts::{
    dispmode, lut, overlay, ODSY_CMAP_UNVERIFIED_GROUP, ODSY_DID_MAIN_SHIFT,
    ODSY_GAMMA_GROUP, ODSY_INDEX_MASK,
};
use super::fifo::display_fifo_pending;
use super::reader::{DisplayRegs, DisplayShared, ScanoutReader};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::cpu::mips_core::CyclesPtr;
use crate::gfx_display::{Frame, Screen};
use crate::disp::{Rex3Screen, FB_STRIDE};

/// Idle frames between forced presents.
const IDLE_REFRESH: u32 = 6;

/// The window the board shows the guest: what `GfxDisplay` hands out.
pub(super) struct Display {
    pub screens: [Arc<Screen>; 1],
    /// What the installed renderer presents from.
    pub screen: Mutex<Rex3Screen>,
    /// Whether the display thread releases the renderer when it ends.
    pub(super) display_stop: crate::gfx_display::DisplayStop,
    local: Mutex<Option<Local>>,
    /// What the display keeps of the scanout: the presented frame, the
    /// retrace clock and the update in progress. Held for each frame.
    pub reader: Mutex<ScanoutReader>,
    /// Frames since the last present.
    idle: AtomicU32,
    /// The clock ran at the start of this tick's frame.
    pub(super) polled: AtomicBool,
    /// The machine's status-bar inputs.
    status: Arc<crate::disp::StatusSource>,
    /// The registers the display reads, for the host's cursor query; set
    /// when the board hands the display out.
    shared: OnceLock<Arc<DisplayShared>>,
}

struct Local {
    overlay: crate::debug_overlay::DebugOverlay,
    status: crate::disp::StatusBar,
    texture: crate::disp::StatusBarTexture,
    size: (usize, usize),
}

impl Display {
    pub(super) fn new(status: Arc<crate::disp::StatusSource>) -> Self {
        Self {
            screens: [Arc::new(Screen::new(0, Arc::default()))],
            screen: Mutex::new(Rex3Screen::new()),
            display_stop: Default::default(),
            local: Mutex::new(None),
            reader: Mutex::new(ScanoutReader::default()),
            idle: AtomicU32::new(0),
            polled: AtomicBool::new(false),
            status,
            shared: OnceLock::new(),
        }
    }

    /// The window and the consumers' side of the board's screen.
    pub(super) fn out(&self) -> &Arc<Screen> {
        &self.screens[0]
    }

    /// Present a frame composed on the GPU (0xAABBGGRR, rows of `width`)
    /// through the installed renderer; no words are filled.
    #[cfg(feature = "wgpu")]
    pub(super) fn present_gpu(&self, frame: &wgpu::Buffer, width: u32, height: u32) {
        let (w, h) = (width as usize, height as usize);
        let mut screen = self.screen.lock();
        screen.width = w.min(FB_STRIDE);
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        self.out().set_size(width, height);
        let mut renderer = self.out().window.renderer.lock();
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
            let stats = self.status.snapshot(std::time::Instant::now(), display_fifo_pending());
            renderer.present_gpu_frame(frame, width, height, &mut screen, &mut local.overlay, &mut local.status, &mut local.texture, &stats);
        }
    }

    /// Read the cursor from `shared` from now on.
    pub(super) fn attach_registers(&self, shared: Arc<DisplayShared>) {
        // A window presents on the device the scan-out composes on.
        #[cfg(feature = "wgpu")]
        if let Some(scanout) = shared.gpu_scanout.get() {
            let _ = self.out().gpu.set(scanout.lock().gpu());
        }
        let _ = self.shared.set(shared);
    }

    /// Whether this frame is presented: when a line changed, when its words
    /// are wanted, or on every sixth idle frame, as IMPACT does, so a window or
    /// VNC client that missed a frame catches up.
    pub(super) fn due(&self, changed: bool, wanted: bool) -> bool {
        let idle = self.idle.fetch_add(1, Ordering::Relaxed) + 1;
        if changed || wanted || idle >= IDLE_REFRESH {
            self.idle.store(0, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Copy `rows` (0xAABBGGRR lines of `width` pixels, the first being
    /// line `first` of a frame `height` lines tall) into the screen and
    /// present it. No rows: present the screen as it is.
    pub(super) fn present(&self, rows: &[u32], width: usize, height: usize, first: usize, out: &mut Frame, words: bool) {
        self.present_lines(rows.chunks_exact(width.max(1)).enumerate().map(|(i, row)| (first + i, row)), width, height, out, words);
    }

    /// As present(), for lines given with their numbers.
    pub(super) fn present_lines<'a>(&self, lines: impl Iterator<Item = (usize, &'a [u32])>, width: usize, height: usize, out: &mut Frame, words: bool) {
        let mut screen = self.screen.lock();
        let (w, h) = (width.min(FB_STRIDE), height.min(screen.fb_rgb.len() / FB_STRIDE));
        for (y, src) in lines {
            if y >= h {
                continue;
            }
            let at = y * FB_STRIDE;
            screen.fb_rgb[at..at + w].copy_from_slice(&src[..w]);
        }
        self.present_in_place(screen, width, height, out, words);
    }

    /// Present the screen whose `fb_rgb` holds the frame, `width` by
    /// `height` (the display thread composes into it under this lock). `out`
    /// gets the frame's words only when a consumer reads them (`words`): a
    /// screenshot, a frame request, a capturing VNC client. The renderers
    /// take `fb_rgb`.
    pub(super) fn present_in_place(&self, mut screen: parking_lot::MutexGuard<'_, Rex3Screen>, width: usize, height: usize, out: &mut Frame, words: bool) {
        let w = width.min(FB_STRIDE);
        let h = height.min(screen.fb_rgb.len() / FB_STRIDE);
        screen.width = w;
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        if words {
            out.width = w as u32;
            out.height = h as u32;
            out.words.clear();
            for y in 0..h {
                let at = y * FB_STRIDE;
                out.words.extend_from_slice(&screen.fb_rgb[at..at + w]);
            }
        }
        self.out().set_size(width as u32, height as u32);
        let mut renderer = self.out().window.renderer.lock();
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
            let stats = self.status.snapshot(std::time::Instant::now(), display_fifo_pending());
            renderer.present(&mut screen, &mut local.overlay, &mut local.status, &mut local.texture, &stats, false, None, None);
        }
    }

    /// The display thread's last act, after the renderer is released: drop
    /// the per-frame overlay state.
    pub(super) fn local_done(&self) {
        self.local.lock().take();
    }
}

impl Board {
    /// Pixel (x, y)'s main window id: bits 7:3 of its window-id byte in the
    /// buffer the scan-out names (PB&J 0x2800 base, 0x2807 stride).
    pub fn main_id(&mut self, x: u32, y: u32) -> u32 {
        // The id bytes are the front end's own: no wait (Board::id_compare_passes).
        let did = DestMem::of(self.vram_ptr(), &self.dest_views()).did.load1(x as i32, y as i32).unwrap_or(0);
        ((did as u32) >> ODSY_DID_MAIN_SHIFT) & (dispmode::COUNT - 1)
    }

    /// The colormap group main drawable main_id shows through: bits 8:4 of its
    /// display mode. Below ODSY_GAMMA_GROUP the word is an index.
    pub fn display_group(&mut self, main_id: u32) -> u32 {
        self.display.regs.display_group(main_id)
    }

    /// A vertical retrace: the host interface latches the swaps armed and
    /// raises the retrace source (HostInterface::retrace()).
    #[cfg(test)]
    pub fn retrace(&mut self) {
        self.display.retrace();
    }
}

/// The scan-out as the tests and the benchmark run it on a board they hold,
/// without a display thread: the same reader the display uses.
#[cfg(test)]
impl Board {
    fn test_reader(&mut self) -> &mut ScanoutReader {
        self.raster_wait();
        self.reader.attach(&self.display);
        &mut self.reader
    }

    /// Compose the whole frame into `reader.frame`.
    pub fn scanout_update(&mut self) -> bool {
        self.test_reader().update()
    }
}

/// What one main drawable id shows, decided once per update instead of per
/// pixel: which colour buffer, and how its plane word becomes a colour.
#[derive(Clone, Copy, Default)]
pub(super) struct IdShow {
    /// The displayed colour buffer, 0 or 1.
    pub buf: usize,
    /// Whether the plane word is RGB through the gamma table.
    pub rgb: bool,
    /// An RGB mode's channel fold: (c & keep) | (c >> down).
    pub keep: u32,
    pub down: u32,
    /// An index mode: lut entry = base + (word & mask).
    pub base: u32,
    pub mask: u32,
    /// The mode is colour map group 25, reported once when shown.
    pub unverified: bool,
}

/// Everything a row's resolve reads outside the planes, gathered once from a
/// copy of the registers, so none of it can change meanwhile.
pub(super) struct ResolveCtx {
    pub ids: [IdShow; dispmode::COUNT as usize],
    /// Per overlay id: whether it is enabled, and its entry base.
    pub overlay: [(bool, u32); overlay::IDS as usize],
    /// Every lookup-RAM entry as 0x00RRGGBB (DisplayRegs::lut_rgb()).
    pub rgb: Box<[u32; lut::SIZE as usize]>,
    /// DisplayRegs::gamma() of each 8-bit channel value, placed in its byte of
    /// the pixel: red, green, blue.
    pub gamma: Box<[[u32; 256]; 3]>,
}

impl ResolveCtx {
    /// What the registers `regs` make of each drawable, overlay id, lookup
    /// entry and gamma step, with colour buffer (displayed >> id) & 1
    /// showing for each drawable.
    pub(super) fn new(regs: &DisplayRegs, displayed: u32) -> ResolveCtx {
        let mut ids = [IdShow::default(); dispmode::COUNT as usize];
        let mut overlay = [(false, 0u32); overlay::IDS as usize];
        let mut rgb = Box::new([0u32; lut::SIZE as usize]);
        let mut gamma = Box::new([[0u32; 256]; 3]);

        for (i, v) in rgb.iter_mut().enumerate() {
            *v = regs.lut_rgb(i as u32);
        }
        for c in 0..256u32 {
            gamma[0][c as usize] = regs.gamma(c, lut::RED_SHIFT) << 16;
            gamma[1][c as usize] = regs.gamma(c, lut::GREEN_SHIFT) << 8;
            gamma[2][c as usize] = regs.gamma(c, lut::BLUE_SHIFT);
        }
        for (id, show) in ids.iter_mut().enumerate() {
            let mode = regs.dispmode[id];
            let group = regs.display_group(id as u32);

            show.buf = ((displayed >> id) & 1) as usize;
            if group >= ODSY_GAMMA_GROUP {
                show.rgb = true;
                // display_channel() as a mask and a shift; a shift of 8 or
                // more leaves a byte's high part empty.
                (show.keep, show.down) = match (mode >> dispmode::WIDTH_SHIFT) & dispmode::WIDTH_MASK {
                    dispmode::WIDTH_4 => (0xf0, 4),
                    dispmode::WIDTH_5 => (0xf8, 5),
                    _ => (0xff, 8),
                };
            } else if group != 0 {
                show.base = 256 * group;
                show.mask = 0xff;
                show.unverified = group == ODSY_CMAP_UNVERIFIED_GROUP;
            } else {
                show.mask = ODSY_INDEX_MASK;
            }
        }
        for (id, o) in overlay.iter_mut().enumerate() {
            let mode = regs.overlay_mode[id];

            *o = (
                mode & overlay::MODE_ENABLE != 0,
                256 * ((mode >> overlay::MODE_GROUP_SHIFT) & overlay::MODE_GROUP_MASK),
            );
        }
        ResolveCtx { ids, overlay, rgb, gamma }
    }
}

impl crate::gfx_display::GfxDisplay for Display {
    fn screens(&self) -> &[Arc<Screen>] {
        &self.screens
    }
    fn display_stop(&self) -> &crate::gfx_display::DisplayStop {
        &self.display_stop
    }
    fn cycles(&self) -> CyclesPtr {
        self.status.cycles()
    }
    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        // A racy read of the registers, as the display thread's frame does.
        self.shared.get()?.regs.hw_cursor()
    }
}
