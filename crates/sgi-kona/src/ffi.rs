//! The C ABI of the KONA engine: its constructor and the entries that name its own types.
//! Everything else is `sgi_engine::ffi`. `include/sgi_kona.h` is the header, written by hand;
//! `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_ERROR, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::kona::engine::{FlashDrive, KonaEngine, Options, Region};
use crate::dev::kona::flash::{fresh_image, FlashSink, FLASH_SIZE};
use crate::dev::kona::ident::{KonaConfig, Product};
use crate::wgpu::{Gpu, GpuOptions};
use crate::SharedGpu;

/// The most tile workers a shell may ask for; each is a host thread.
const MAX_POOL_THREADS: u32 = 256;

/// The KONA configuration: the shared part, then the pipe's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisKonaConfig {
    pub common: IrisEngineConfig,
    /// The product: `ir2`, `ir2e` or `ir3`, NUL-terminated. NULL is refused.
    pub product: *const c_char,
    /// Raster managers, 1 or 2, or 4 behind a host board.
    pub rm_count: u32,
    /// The pipe slot the machine puts this on, 0 to 3.
    pub identity_index: u32,
    /// The widget the pipe's DMA goes to out of reset, 0 to 15.
    pub host_widget: u32,
    /// The crossbar widget the pipe sits at, which its packets name as their sender, 0 to 15.
    pub widget: u32,
    /// The value the probe register reads.
    pub probe: u32,
    /// The managed area in pixels; 0 for the default 1280 by 1024.
    pub managed_width: u32,
    pub managed_height: u32,
    /// The tile workers (0: draw on the caller's thread; at most 256) and tile size in pixels, as `[graphics]`.
    pub pool_threads: u32,
    pub pool_tile: u32,
    /// The boards. NULL or empty takes the product's own (the display generator's own is the
    /// DG5-8, or the DG5-2 behind a host board).
    pub geometry: *const c_char,
    pub rm_type: *const c_char,
    pub display: *const c_char,
    /// `small` or `medium`; NULL is small.
    pub pixel_depth: *const c_char,
    /// 0 or the product's own texture memory per raster manager, in MB.
    pub texture_mb: u32,
    /// 0 or the display generator's channel count.
    pub channels: u32,
    /// The Onyx 3400's Ktown2 is fitted: it allows four raster managers.
    pub ktown2: u32,
    /// The one-wire parts fitted besides the geometry board's, by part number (the Ktown's,
    /// `030-1067-001`, for a rack), and how many. A part the chain cannot hold is refused.
    pub parts: *const *const c_char,
    pub num_parts: u32,
    /// The pipe slot whose Ktown serial the record follows, plus one: 1 to 4, or 0 for the pipe's own.
    pub shared_identity: u32,
    /// The geometry board part's ROM serial, 48 bits; 0 is the synthetic one.
    pub geometry_serial: u64,
    /// The argument of the callbacks below.
    pub user: *mut c_void,
    /// The flash's contents, `flash_len` bytes (0, or exactly 0x80000); NULL is a new flash.
    pub flash_image: *const u8,
    pub flash_len: u32,
    /// Called with every range of the flash the pipe programs, and with the whole image after a
    /// snapshot loads. NULL keeps the flash in memory only.
    pub flash_write: Option<unsafe extern "C" fn(user: *mut c_void, offset: u32, data: *const u8, len: usize)>,
    /// Called with the widget the pipe's DMA goes to (0 to 15) before the engine returns from
    /// the call that created it or changed it, so the shell's bus callbacks can name it.
    pub dma_widget: Option<unsafe extern "C" fn(user: *mut c_void, widget: u32)>,
    /// Bit 0: the shell models the display option board and will give `sgi_kona_set_display_bus` its
    /// device, which lets `display` name `dg5-2-gvo`. Zero for a caller that predates the field.
    pub flags: u32,
}

/// The shell's `user` pointer, for the callbacks the engine makes on its own threads' behalf.
#[derive(Clone, Copy)]
struct User(*mut c_void);

// SAFETY: the contract of `IrisKonaConfig`: the callbacks may be called from any thread that calls
// the engine, and `user` stays valid until `iris_engine_free`.
unsafe impl Send for User {}

impl User {
    fn ptr(self) -> *mut c_void {
        self.0
    }
}

struct CFlash(User, unsafe extern "C" fn(*mut c_void, u32, *const u8, usize));

impl FlashSink for CFlash {
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        // SAFETY: the callback's contract; `data` is valid for its length.
        unsafe { (self.1)(self.0.ptr(), offset as u32, data.as_ptr(), data.len()) };
        Ok(())
    }
}

/// A box of pixels in one word position of the store.
#[repr(C)]
pub struct IrisKonaRegion {
    pub word: u32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

fn pipe_config(c: &IrisKonaConfig) -> Result<KonaConfig, String> {
    if c.pool_threads > MAX_POOL_THREADS {
        return Err(format!("{} tile workers: at most {MAX_POOL_THREADS}", c.pool_threads));
    }
    if c.product.is_null() {
        return Err("no product".into());
    }
    // SAFETY: the caller's contract: NUL-terminated.
    let name = unsafe { CStr::from_ptr(c.product) }.to_str().map_err(|e| e.to_string())?;
    let byte = |v: u32, what: &str| u8::try_from(v).map_err(|_| format!("{what} {v} does not fit"));
    let text = |p: *const c_char| -> Result<&str, String> {
        if p.is_null() {
            return Ok("");
        }
        // SAFETY: the caller's contract: NUL-terminated.
        unsafe { CStr::from_ptr(p) }.to_str().map_err(|e| e.to_string())
    };
    // The chain holds one part besides the geometry board's; a longer list, or a count with no
    // list, is refused before anything is built from it.
    if c.num_parts > 1 || (c.num_parts != 0 && c.parts.is_null()) {
        return Err(format!("{} one-wire parts: the chain can hold one besides the geometry board's", c.num_parts));
    }
    let parts: Vec<&str> = (0..c.num_parts as usize).map(|i| if c.parts.is_null() { Ok("") } else { text(unsafe { *c.parts.add(i) }) }).collect::<Result<_, _>>()?;
    let host_board = KonaConfig::host_board_of(&parts, c.ktown2 != 0)?;
    let mut cfg = KonaConfig::new(Product::from_name(name)?, 1)?
        .with_host_board(host_board)
        .with_rm_count(c.rm_count)?
        .with_geometry(text(c.geometry)?)?
        .with_rm_type(text(c.rm_type)?)?
        .with_texture_mb(c.texture_mb)?;
    if c.flags & 1 != 0 {
        cfg = cfg.with_option_slave();
    }
    let mut cfg = cfg
        .with_display(text(c.display)?, c.channels)?
        .with_pixel_depth(text(c.pixel_depth)?)?
        .in_slot(byte(c.identity_index, "identity index")?)?
        .with_host_widget(byte(c.host_widget, "host widget")?)?
        .with_widget(byte(c.widget, "widget")?)?
        .with_pool(c.pool_threads as usize, c.pool_tile);
    if c.shared_identity != 0 {
        cfg = cfg.with_shared_identity(byte(c.shared_identity - 1, "shared identity")?)?;
    }
    if c.geometry_serial != 0 {
        cfg = cfg.with_geometry_serial(c.geometry_serial)?;
    }
    cfg.probe = c.probe;
    if c.managed_width != 0 && c.managed_height != 0 {
        cfg.managed = (c.managed_width, c.managed_height);
    }
    Ok(cfg)
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `host` point to valid structures; the callbacks and `host->user` stay valid
/// until `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_new(cfg: *const IrisKonaConfig, host: *const IrisEngineHost) -> *mut IrisEngine {
    unsafe { new_with(cfg, host, None) }
}

/// As `sgi_kona_new` for a Rust shell that already owns the process's GPU device: the pipe draws and
/// composes on `gpu`, so a buffer it lends or composes into is on the device the shell's own display
/// uses. `loss` is the target of that device's loss callback; the configuration's `gpu` request is not
/// read.
///
/// # Safety
/// As `sgi_kona_new`.
pub unsafe fn new_on(cfg: *const IrisKonaConfig, host: *const IrisEngineHost, gpu: SharedGpu, loss: Arc<GpuLoss>) -> *mut IrisEngine {
    unsafe { new_with(cfg, host, Some((gpu, loss))) }
}

unsafe fn new_with(cfg: *const IrisKonaConfig, host: *const IrisEngineHost, given: Option<(SharedGpu, Arc<GpuLoss>)>) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        let pipe = match pipe_config(&c) {
            Ok(p) => p,
            Err(e) => {
                log::error!("sgi_kona_new: {e}");
                return None;
            }
        };
        if c.common.lent_len != 0 {
            log::error!("sgi_kona_new: KONA has its own memory and takes none lent");
            return None;
        }
        let (gpu, loss) = match given {
            Some((gpu, loss)) => (Some(gpu), loss),
            None => {
                let loss = GpuLoss::new();
                let gpu = match unsafe { c.common.gpu_request() } {
                    None => None,
                    Some((backend, software)) => match Gpu::new(&GpuOptions { backend, software, ..Default::default() }, loss.callback()) {
                        Ok(g) => Some(g),
                        Err(e) => {
                            log::error!("sgi_kona_new: GPU: {e}");
                            return None;
                        }
                    },
                };
                (gpu, loss)
            }
        };
        let mut opts = Options { gpu, ..Options::default() };
        let user = User(c.user);
        if !c.flash_image.is_null() && c.flash_len as usize != FLASH_SIZE {
            log::error!("sgi_kona_new: the flash image is {} bytes, not {FLASH_SIZE}", c.flash_len);
            return None;
        }
        if let Some(write) = c.flash_write {
            // SAFETY: `flash_image` holds `flash_len` bytes, which is the whole flash when not NULL.
            let image = if c.flash_image.is_null() { fresh_image() } else { unsafe { std::slice::from_raw_parts(c.flash_image, FLASH_SIZE) }.to_vec() };
            opts.flash = Some(FlashDrive { image, sink: Box::new(CFlash(user, write)) });
        } else if !c.flash_image.is_null() {
            log::error!("sgi_kona_new: a flash image with no write callback would be read-only; give the callback or no image");
            return None;
        }
        if let Some(f) = c.dma_widget {
            // SAFETY: the callback's contract.
            opts.dma_widget = Some(Box::new(move |w| unsafe { f(user.ptr(), w as u32) }));
        }
        if c.common.queue_ops != 0 {
            opts.queue = c.common.queue_ops as usize;
        }
        match KonaEngine::new(pipe, mem, opts, notify) {
            Ok(e) => {
                loss.attach(e.gpu_lost_handler());
                Some(e)
            }
            Err(e) => {
                log::error!("sgi_kona_new: {e}");
                None
            }
        }
    })
}

/// A Crossbow link reset reached the widget: the widget control, interrupt destination and vector
/// registers go back to their reset values and the rest of the pipe, the map RAM included, stays.
///
/// # Safety
/// `h` is a live KONA handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_link_reset(h: *mut IrisEngine) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            e.link_reset();
            IRIS_ENGINE_OK
        })
    }
}

/// Bring a box of pixels current for a CPU reader.
///
/// # Safety
/// `h` is a live KONA handle and `r` points to a region.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_observe(h: *mut IrisEngine, r: *const IrisKonaRegion) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Observe, IRIS_ENGINE_FAULT, |e| {
            if r.is_null() {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            let r = &*r;
            e.observe(&Region { word: r.word as usize, rect: Rect::new(r.x0, r.y0, r.x1, r.y1) }).map_or(IRIS_ENGINE_FAULT, |_| IRIS_ENGINE_OK)
        })
    }
}

/// Copy the composed scan-out into `buf` (`cap` words): 0x00RRGGBB a pixel, row by row from the
/// top, as `sgi_odyssey_scanout` does. `*w` and `*hgt` are set to the frame's size, also when
/// `cap` is too small (ERROR, nothing copied). FAULT on a faulted engine.
///
/// # Safety
/// `h` is a live KONA handle, `buf` points to `cap` words and `w` and `hgt` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_scanout(h: *mut IrisEngine, buf: *mut u32, cap: usize, w: *mut u32, hgt: *mut u32) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Pixel, IRIS_ENGINE_FAULT, |e| {
            if w.is_null() || hgt.is_null() || (buf.is_null() && cap != 0) {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            let (fw, fh, pixels) = e.compose();
            *w = fw;
            *hgt = fh;
            if pixels.len() > cap {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            // The compose is 0xAABBGGRR; the scan-out the shells share is 0x00RRGGBB.
            for (i, p) in pixels.iter().enumerate() {
                *buf.add(i) = (p & 0xff) << 16 | (p & 0xff00) | (p >> 16 & 0xff);
            }
            IRIS_ENGINE_OK
        })
    }
}

/// The rows of head `head`'s picture that may have changed since the last call for it: `*first` up to `*end`
/// (both 0 for none). OK; ERROR for a NULL output; FAULT on a faulted engine.
///
/// # Safety
/// `h` is a live KONA handle; `first` and `end` point to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_take_damage(h: *mut IrisEngine, head: u32, first: *mut u32, end: *mut u32) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if first.is_null() || end.is_null() {
                return IRIS_ENGINE_ERROR;
            }
            (*first, *end) = e.take_damage(head).unwrap_or((0, 0));
            IRIS_ENGINE_OK
        })
    }
}

/// Channel 0's DAC cursor as the display bus's registers hold it, without waiting for queued stores: the origin of
/// its 64 by 64 image in screen pixels from the top left (signed) in `*x` and `*y`, and in `*on` whether it is enabled
/// in X11 mode, for a shell that publishes the pointer. OK; ERROR for a NULL output.
///
/// # Safety
/// `h` is a live KONA handle; `x` and `y` point to `int32_t` and `on` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_dac_cursor(h: *mut IrisEngine, x: *mut i32, y: *mut i32, on: *mut u32) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if x.is_null() || y.is_null() || on.is_null() {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            let (cx, cy, shown) = e.dac_cursor();
            (*x, *y, *on) = (cx, cy, shown as u32);
            IRIS_ENGINE_OK
        })
    }
}

/// What the GPU store has done, for a shell that asserts a GPU run executed GPU records and fell back to the CPU for none.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisKonaGpuCounters {
    pub records: u64,
    pub batches: u64,
    pub copies: u64,
    pub readbacks: u64,
    pub rejections: u64,
    /// Ops that ran on the CPU for want of a GPU kernel.
    pub cpu_ops: u64,
    /// 1 once the engine has faulted.
    pub faults: u64,
}

/// Fill `*out` with the GPU store's counters. OK; ERROR when the engine has no GPU store (or `out` is NULL); FAULT on a
/// faulted engine.
///
/// # Safety
/// `h` is a live KONA handle and `out` points to an `IrisKonaGpuCounters`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_gpu_counters(h: *mut IrisEngine, out: *mut IrisKonaGpuCounters) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if out.is_null() {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            match e.gpu_counters() {
                Some([records, batches, copies, readbacks, rejections, cpu_ops]) => {
                    *out = IrisKonaGpuCounters { records, batches, copies, readbacks, rejections, cpu_ops, faults: 0 };
                    IRIS_ENGINE_OK
                }
                None => sgi_engine::ffi::IRIS_ENGINE_ERROR,
            }
        })
    }
}

/// A video-output-chip interrupt: `HH_INTR_STATUS` bit 9 rises, and the packet goes out when the driver enabled it.
///
/// # Safety
/// `h` is a live KONA handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_raise_voc_interrupt(h: *mut IrisEngine) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            e.raise_voc_interrupt();
            IRIS_ENGINE_OK
        })
    }
}

/// The shared swap-ready line is held low by another pipe (non-zero) or released (zero): the swaps wait while it is held.
///
/// # Safety
/// `h` is a live KONA handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_set_swap_ready_held(h: *mut IrisEngine, held: c_int) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            e.set_swap_ready_held(held != 0);
            IRIS_ENGINE_OK
        })
    }
}

/// The channels the pipe's display generator has: 8 on a DG5-8, 2 on a DG5-2, with or without its option board
/// (whose channel is one more once the firmware's probe finds it). 0 for a NULL handle.
///
/// # Safety
/// `h` is a live KONA handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_display_channels(h: *mut IrisEngine) -> u32 {
    unsafe { call::<KonaEngine<CMem>, _>(h, Point::Flags, 0, |e| e.display_channels()) }
}

/// The shell's device for the display option board (a GVO), for a pipe configured with `dg5-2-gvo`: the
/// engine calls `read` for a read of display-bus word `addr` and `write` for a store, as the display
/// generator's firmware does for the board when the X server probes it, loads its logic and sets its data
/// format, and for the diagnostic requests that read and write a display-bus word. `read` returns 1 with
/// `*data` set when the board has the register, 0 when it does not; `write` returns 1 when the board took
/// the store. Both are called from the engine's calls, on the thread making them, and must not call the
/// engine. NULL for both detaches. It survives a reset and a load.
///
/// # Safety
/// `h` is a live KONA handle; the callbacks and `user` stay valid until detached or `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_set_display_bus(
    h: *mut IrisEngine,
    user: *mut c_void,
    read: Option<unsafe extern "C" fn(user: *mut c_void, addr: u32, data: *mut u32) -> c_int>,
    write: Option<unsafe extern "C" fn(user: *mut c_void, addr: u32, data: u32) -> c_int>,
) -> c_int {
    struct Callbacks {
        user: *mut c_void,
        read: Option<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> c_int>,
        write: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> c_int>,
    }
    // SAFETY: the caller's contract: the callbacks may be called from any thread the engine's calls come from.
    unsafe impl Send for Callbacks {}
    impl crate::dev::kona::back::display::option::DisplayBusSlave for Callbacks {
        fn read(&mut self, addr: u32) -> Option<u32> {
            let f = self.read?;
            let mut data = 0u32;
            (unsafe { f(self.user, addr, &mut data) } != 0).then_some(data)
        }
        fn write(&mut self, addr: u32, data: u32) -> bool {
            self.write.is_some_and(|f| unsafe { f(self.user, addr, data) } != 0)
        }
    }
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            e.set_display_bus((read.is_some() || write.is_some()).then(|| Box::new(Callbacks { user, read, write }) as Box<dyn crate::dev::kona::back::display::option::DisplayBusSlave>));
            IRIS_ENGINE_OK
        })
    }
}

/// Channel `channel`'s output window: the rectangle of the managed area its output shows, set to
/// (`*x`, `*y` from the top, `*w`, `*hgt`). OK, or ERROR for a channel that does not exist or does not run
/// (nothing set). The display channels are 0 and 1; the option board's, once the firmware has found it
/// (`sgi_kona_set_display_bus`), is the next, and the option board's own VOC id (0xc6) names it too.
///
/// # Safety
/// `h` is a live KONA handle; `x`, `y`, `w` and `hgt` point to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_channel_window(h: *mut IrisEngine, channel: u32, x: *mut u32, y: *mut u32, w: *mut u32, hgt: *mut u32) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if x.is_null() || y.is_null() || w.is_null() || hgt.is_null() {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            match e.channel_window_of(channel) {
                Some((cx, cy, cw, ch)) => {
                    (*x, *y, *w, *hgt) = (cx, cy, cw, ch);
                    IRIS_ENGINE_OK
                }
                None => sgi_engine::ffi::IRIS_ENGINE_ERROR,
            }
        })
    }
}

/// The pixels of `sgi_kona_channel_window`'s rectangle into `buf` (`cap` words): 0x00RRGGBB a pixel,
/// row by row from the top. `*w` and `*hgt` are set to its size, also when `cap` is too small (ERROR,
/// nothing copied); ERROR with them untouched for a channel that does not exist or does not run. FAULT on a
/// faulted engine. The call waits for the drawing queued behind the frame.
///
/// # Safety
/// `h` is a live KONA handle, `buf` points to `cap` words and `w` and `hgt` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_kona_channel_scanout(h: *mut IrisEngine, channel: u32, buf: *mut u32, cap: usize, w: *mut u32, hgt: *mut u32) -> c_int {
    unsafe {
        call::<KonaEngine<CMem>, _>(h, Point::Pixel, IRIS_ENGINE_FAULT, |e| {
            if w.is_null() || hgt.is_null() || (buf.is_null() && cap != 0) {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            let Some((cw, ch, pixels)) = e.channel_scanout_of(channel) else { return sgi_engine::ffi::IRIS_ENGINE_ERROR };
            *w = cw;
            *hgt = ch;
            if pixels.len() > cap {
                return sgi_engine::ffi::IRIS_ENGINE_ERROR;
            }
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), buf, pixels.len());
            IRIS_ENGINE_OK
        })
    }
}
