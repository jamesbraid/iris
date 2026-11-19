//! The C ABI of the VPro engine: its constructor and the board's memory size.
//! Everything else is `sgi_engine::ffi`. `include/sgi_odyssey.h` is the header,
//! written by hand; `tools/check-header` compares it with cbindgen's.

use std::ffi::{c_char, c_int, CStr};

use sgi_engine::ffi::{build, call, host_parts, read_config, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_ERROR, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss, Rect};

use crate::dev::odyssey::board::Params;
use crate::dev::odyssey::engine::{memory_bytes_for, Config, Mode, VproEngine};
use crate::wgpu::{Gpu, GpuOptions};

/// The VPro configuration: the shared part, then the board's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisVproConfig {
    pub common: IrisEngineConfig,
    /// The product: `v12`, `v10`, `v8` or `v6`, NUL-terminated. It overrides the revision, memory
    /// size, banks and back-end version; NULL or empty uses the properties below as given.
    pub product: *const c_char,
    /// The NIC's record text; NULL or empty for the product's.
    pub nic_record: *const c_char,
    /// The NIC's serial (48 bits); 0 for the synthetic one.
    pub nic_serial: u64,
    /// The widget number the board answers as.
    pub widget: u32,
    pub revision: u32,
    /// megabytes: 32, 64 or 128
    pub memory_size: u32,
    /// 2 or 4
    pub memory_banks: u32,
    /// 2 or 3
    pub cas_latency: u32,
    pub backend_version: u32,
    /// The fragment pool's workers and tile size in pixels, as `[graphics]`; 0 for a default.
    pub pool_threads: u32,
    pub pool_tile: u32,
}

fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    // SAFETY: the caller's contract: NULL or NUL-terminated.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().ok()).flatten()
}

/// The board properties `c` gives and the product it names (empty for none), or why not.
fn board(c: &IrisVproConfig) -> Result<(String, Params), String> {
    let text = |p: *const c_char| if p.is_null() { Ok("") } else { c_str(p).ok_or("a string that is not UTF-8") };
    let params = Params {
        revision: c.revision,
        memory_size: c.memory_size,
        memory_banks: c.memory_banks,
        cas_latency: c.cas_latency,
        backend_version: c.backend_version,
        nic_serial: if c.nic_serial == 0 { Params::default().nic_serial } else { c.nic_serial },
        nic_record: text(c.nic_record)?.to_string(),
    };
    Ok((text(c.product)?.to_string(), params))
}

/// Bytes of board memory the configuration's product (or its memory size, with no product)
/// carries: the length to lend in `common`. 0 for a configuration the board cannot have.
///
/// # Safety
/// `cfg` is NULL or points to an `IrisVproConfig` whose strings are NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_memory_bytes(cfg: *const IrisVproConfig) -> u64 {
    // SAFETY: the caller's contract.
    let Some(c) = (unsafe { cfg.as_ref() }) else { return 0 };
    board(c).and_then(|(p, params)| memory_bytes_for(&p, &params)).unwrap_or(0)
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `host` point to valid structures; the callbacks, `host->user` and
/// the lent memory stay valid until `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_new(cfg: *const IrisVproConfig, host: *const IrisEngineHost) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        let (name, params) = match board(&c) {
            Ok(b) => b,
            Err(e) => {
                log::error!("sgi_odyssey_new: {e}");
                return None;
            }
        };
        let config = Config { product: name, params, widget: c.widget, mode: if c.common.threaded() { Mode::Threaded } else { Mode::Inline }, pool: (c.pool_threads as usize, c.pool_tile), gpu: None };
        let loss = GpuLoss::new();
        let gpu = match unsafe { c.common.gpu_request() } {
            None => None,
            Some((backend, software)) => match Gpu::new(&GpuOptions { backend, software, ..Default::default() }, loss.callback()) {
                Ok(g) => Some(g),
                Err(e) => {
                    log::error!("sgi_odyssey_new: GPU: {e}");
                    return None;
                }
            },
        };
        let lent = unsafe { c.common.lent() };
        if (c.common.lent_len != 0) != lent.is_some() {
            log::error!("sgi_odyssey_new: the lent memory is not 8-aligned whole words");
            return None;
        }
        match VproEngine::with_gpu(config, lent, mem, notify, gpu, loss) {
            Ok(e) => Some(e),
            Err(e) => {
                log::error!("sgi_odyssey_new: {e}");
                None
            }
        }
    })
}

/// Test hook (feature `test-hold`): the board's thread holds its next slice open for up to `ms`
/// milliseconds or until `sgi_odyssey_release_hold`, so a read that waits answers BUSY meanwhile.
/// OK in threaded mode; ERROR in inline mode, where there is no thread to hold.
///
/// # Safety
/// `h` is a live VPro handle.
#[cfg(feature = "test-hold")]
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_hold_next_slice(h: *mut IrisEngine, ms: u32) -> c_int {
    unsafe { call::<VproEngine, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| if e.hold_next_slice(ms) { IRIS_ENGINE_OK } else { IRIS_ENGINE_ERROR }) }
}

/// Test hook (feature `test-hold`): end a held slice.
///
/// # Safety
/// `h` is a live VPro handle.
#[cfg(feature = "test-hold")]
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_release_hold(h: *mut IrisEngine) -> c_int {
    unsafe {
        call::<VproEngine, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            e.release_hold();
            IRIS_ENGINE_OK
        })
    }
}

/// Test hook (feature `test-hold`): 1 while the board's thread is inside a slice, else 0.
///
/// # Safety
/// `h` is a live VPro handle.
#[cfg(feature = "test-hold")]
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_slice_open(h: *mut IrisEngine) -> c_int {
    unsafe { call::<VproEngine, _>(h, Point::Flags, 0, |e| e.slice_open() as c_int) }
}

/// The cursor's position (signed, in pixels) and whether it is drawn, as the display registers
/// hold them, for a shell that publishes the pointer.
///
/// # Safety
/// `h` is a live VPro handle; `x` and `y` point to `int32_t` and `shown` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_cursor(h: *mut IrisEngine, x: *mut i32, y: *mut i32, shown: *mut u32) -> c_int {
    unsafe {
        call::<VproEngine, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if x.is_null() || y.is_null() || shown.is_null() {
                return IRIS_ENGINE_ERROR;
            }
            let (cx, cy, on) = e.cursor();
            (*x, *y, *shown) = (cx, cy, on as u32);
            IRIS_ENGINE_OK
        })
    }
}

/// What the GPU unit has done, for a shell that asserts a GPU run executed GPU records.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisVproGpuCounters {
    pub records: u64,
    pub batches: u64,
    pub copies: u64,
    pub readbacks: u64,
    pub rejections: u64,
    pub faults: u64,
}

/// Fill `*out` with the GPU unit's counters. OK; ERROR when the engine has no GPU unit (or `out`
/// is NULL); FAULT on a faulted engine.
///
/// # Safety
/// `h` is a live VPro handle and `out` points to an `IrisVproGpuCounters`.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_gpu_counters(h: *mut IrisEngine, out: *mut IrisVproGpuCounters) -> c_int {
    unsafe {
        call::<VproEngine, _>(h, Point::Flags, IRIS_ENGINE_FAULT, |e| {
            if out.is_null() {
                return IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            match e.gpu_counters() {
                Some([records, batches, copies, readbacks, rejections, faults]) => {
                    *out = IrisVproGpuCounters { records, batches, copies, readbacks, rejections, faults };
                    IRIS_ENGINE_OK
                }
                None => IRIS_ENGINE_ERROR,
            }
        })
    }
}

/// Bring the pixels of a box (corners included, every plane under it) current for a CPU reader.
///
/// # Safety
/// `h` is a live VPro handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_observe(h: *mut IrisEngine, x0: i32, y0: i32, x1: i32, y1: i32) -> c_int {
    unsafe { call::<VproEngine, _>(h, Point::Observe, IRIS_ENGINE_FAULT, |e| e.observe(&Rect::new(x0, y0, x1, y1)).map_or(IRIS_ENGINE_FAULT, |_| IRIS_ENGINE_OK)) }
}

/// Copy the composed scan-out, one `0x00RRGGBB` word a pixel, row by row from the top, into `buf`
/// (`cap` words). `*w` and `*hgt` are set to the frame's size: 0 by 0 while no video mode is set
/// (OK, nothing copied), and the size needed when `cap` is too small (ERROR, nothing copied).
/// FAULT on a faulted engine.
///
/// # Safety
/// `h` is a live VPro handle, `buf` points to `cap` words and `w` and `hgt` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_odyssey_scanout(h: *mut IrisEngine, buf: *mut u32, cap: usize, w: *mut u32, hgt: *mut u32) -> c_int {
    unsafe {
        call::<VproEngine, _>(h, Point::Pixel, IRIS_ENGINE_FAULT, |e| {
            if w.is_null() || hgt.is_null() || (buf.is_null() && cap != 0) {
                return IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            let (fw, fh, pixels) = e.scanout().unwrap_or((0, 0, Vec::new()));
            *w = fw;
            *hgt = fh;
            if pixels.len() > cap {
                return IRIS_ENGINE_ERROR;
            }
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), buf, pixels.len());
            IRIS_ENGINE_OK
        })
    }
}
