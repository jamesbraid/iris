//! The C ABI of the KONA engine: its constructor and the entries that name its own types.
//! Everything else is `sgi_engine::ffi`. `include/sgi_kona.h` is the header, written by hand;
//! `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, CStr};

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::kona::engine::{KonaEngine, Options, Region};
use crate::dev::kona::ident::{KonaConfig, Product};
use crate::wgpu::{Gpu, GpuOptions};

/// The KONA configuration: the shared part, then the pipe's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisKonaConfig {
    pub common: IrisEngineConfig,
    /// The product: `ir2`, `ir2e` or `ir3`, NUL-terminated. NULL is refused.
    pub product: *const c_char,
    /// Raster managers, 1 or 2.
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
    let mut cfg = KonaConfig::new(Product::from_name(name)?, c.rm_count)?
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
