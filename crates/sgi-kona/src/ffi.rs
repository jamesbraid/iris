//! The C ABI of the KONA engine: its constructor and the entries that name its own types.
//! Everything else is `sgi_engine::ffi`. `include/sgi_kona.h` is the header, written by hand;
//! `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, c_void, CStr};

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::kona::engine::{FlashDrive, KonaEngine, Options, Region};
use crate::dev::kona::flash::{fresh_image, FlashSink, FLASH_SIZE};
use crate::dev::kona::ident::{KonaConfig, Product};
use crate::wgpu::{Gpu, GpuOptions};

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
    /// The tile workers (0: draw on the caller's thread) and tile size in pixels, as `[graphics]`.
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
    let parts: Vec<&str> = (0..c.num_parts as usize).map(|i| if c.parts.is_null() { Ok("") } else { text(unsafe { *c.parts.add(i) }) }).collect::<Result<_, _>>()?;
    let host_board = KonaConfig::host_board_of(&parts, c.ktown2 != 0)?;
    let mut cfg = KonaConfig::new(Product::from_name(name)?, 1)?
        .with_host_board(host_board)
        .with_rm_count(c.rm_count)?
        .with_geometry(text(c.geometry)?)?
        .with_rm_type(text(c.rm_type)?)?
        .with_texture_mb(c.texture_mb)?
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
