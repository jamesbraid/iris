//! The C ABI of the KONA engine: its constructor and the entries that name its own types.
//! Everything else is `sgi_engine::ffi`. `include/sgi_kona.h` is the header, written by hand;
//! `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, CStr};

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::kona::engine::{KonaEngine, Options, Region};
use crate::dev::kona::ident::{HostBoard, KonaConfig, Product};
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
    /// The host board: 0 none (the Onyx2 deskside), 1 an Onyx2 rack's Ktown, 2 an Onyx 3400's Ktown2.
    pub host_board: u32,
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
    let host_board = match c.host_board {
        0 => HostBoard::Deskside,
        1 => HostBoard::Ktown,
        2 => HostBoard::Ktown2,
        n => return Err(format!("host board {n}: 0 none, 1 Ktown, 2 Ktown2")),
    };
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
