//! The C ABI of the VPro engine: its constructor and the board's memory size.
//! Everything else is `sgi_engine::ffi`. `include/iris_vpro.h` is the header,
//! written by hand; `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, CStr};

use sgi_engine::ffi::{build, call, host_parts, read_config, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::odyssey::engine::{memory_bytes, Config, Mode, VproEngine};
use crate::wgpu::{Gpu, GpuOptions};

/// The VPro configuration: the shared part, then the board's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisVproConfig {
    pub common: IrisEngineConfig,
    /// The product: `v12`, `v10`, `v8` or `v6`, NUL-terminated. NULL is refused.
    pub product: *const c_char,
    /// The widget number the board answers as.
    pub widget: u32,
    /// The fragment pool's workers and tile size in pixels, as `[graphics]`; 0 for a default.
    pub pool_threads: u32,
    pub pool_tile: u32,
}

fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    // SAFETY: the caller's contract: NULL or NUL-terminated.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().ok()).flatten()
}

/// Bytes of board memory the product carries: the length to lend in the shared
/// configuration. 0 for a product that does not exist.
///
/// # Safety
/// `product` is NULL or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn iris_vpro_memory_bytes(product: *const c_char) -> u64 {
    c_str(product).and_then(|p| memory_bytes(p).ok()).unwrap_or(0)
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `host` point to valid structures; the callbacks, `host->user` and
/// the lent memory stay valid until `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn iris_vpro_new(cfg: *const IrisVproConfig, host: *const IrisEngineHost) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        let Some(name) = c_str(c.product) else {
            log::error!("iris_vpro_new: no product");
            return None;
        };
        let config = Config { product: name.to_string(), widget: c.widget, mode: if c.common.threaded() { Mode::Threaded } else { Mode::Inline }, pool: (c.pool_threads as usize, c.pool_tile), gpu: None };
        let loss = GpuLoss::new();
        let gpu = match unsafe { c.common.gpu_request() } {
            None => None,
            Some((backend, software)) => match Gpu::new(&GpuOptions { backend, software, ..Default::default() }, loss.callback()) {
                Ok(g) => Some(g),
                Err(e) => {
                    log::error!("iris_vpro_new: GPU: {e}");
                    return None;
                }
            },
        };
        let lent = unsafe { c.common.lent() };
        if (c.common.lent_len != 0) != lent.is_some() {
            log::error!("iris_vpro_new: the lent memory is not 8-aligned whole words");
            return None;
        }
        match VproEngine::with_gpu(config, lent, mem, notify, gpu, loss) {
            Ok(e) => Some(e),
            Err(e) => {
                log::error!("iris_vpro_new: {e}");
                None
            }
        }
    })
}

/// Bring the pixels of a box (corners included, every plane under it) current for a CPU reader.
///
/// # Safety
/// `h` is a live VPro handle.
#[no_mangle]
pub unsafe extern "C" fn iris_vpro_observe(h: *mut IrisEngine, x0: i32, y0: i32, x1: i32, y1: i32) -> c_int {
    unsafe { call::<VproEngine, _>(h, Point::Observe, IRIS_ENGINE_FAULT, |e| e.observe(&Rect::new(x0, y0, x1, y1)).map_or(IRIS_ENGINE_FAULT, |_| IRIS_ENGINE_OK)) }
}
