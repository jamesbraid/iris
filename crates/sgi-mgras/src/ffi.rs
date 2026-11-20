//! The C ABI of the MGRAS engine: its constructor and the entries that name
//! its own types. Everything else is `sgi_engine::ffi`. `include/sgi_mgras.h`
//! is the header, written by hand; `tools/check-header` compares it with
//! cbindgen's.

use std::ffi::{c_int, c_void};
use std::sync::Arc;

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_ERROR, IRIS_ENGINE_FAULT, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;
use sgi_engine::{BoardEngine, GpuLoss};

use crate::dev::mgras::engine::{Board, Config, MgrasEngine, Mode, Region};
use crate::dev::mgras::pixmem::{Buffer, Kind};
use crate::dev::mgras::rss;
use crate::display::Rect;
use crate::wgpu::{Gpu, GpuOptions};

/// The MGRAS configuration: the shared part, then the board's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisMgrasConfig {
    pub common: IrisEngineConfig,
    /// 0 Solid IMPACT, 1 High, 2 Maximum.
    pub board: u32,
    /// Bit 0: the Octane's HQ4 front. Bit 1: 8-byte DMA page-table entries (Origin 2000, Onyx2).
    /// Bit 2: `bdvers`, `cmap_rev` and `ge_revision` are given; otherwise the board's own bus bytes and no GE revision.
    pub flags: u32,
    /// -1 the default, 0 interpreter only, 1 compile on first use, 2 compile in the background.
    pub jit: i32,
    /// The display bus's two board-version bytes and the two colour maps' revision registers.
    pub bdvers: [u32; 2],
    pub cmap_rev: [u32; 2],
    /// The GE revision the kernel reads at 0x028004 before its first GE query.
    pub ge_revision: u32,
}

#[repr(C)]
pub struct IrisMgrasRegion {
    pub page: u32,
    /// Nonzero: the 9-bit overlay buffer.
    pub overlay: u32,
    /// The raster drb_size register the buffer was laid out with.
    pub drbsize: u32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

fn region(r: &IrisMgrasRegion) -> Region {
    Region { buffer: Buffer::new(r.page, if r.overlay != 0 { Kind::Overlay } else { Kind::Wide }, r.drbsize), rect: Rect::new(r.x0, r.y0, r.x1, r.y1) }
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `host` point to valid structures; the callbacks and `host->user`
/// stay valid until `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn sgi_mgras_new(cfg: *const IrisMgrasConfig, host: *const IrisEngineHost) -> *mut IrisEngine {
    unsafe { new_with(cfg, host, None) }
}

/// As `sgi_mgras_new` for a Rust shell that already owns the process's GPU device: the engine draws
/// and composes on `gpu`, so a buffer it lends or composes into is on the device the shell's own
/// display uses (a wgpu buffer cannot cross devices). `loss` is the target of that device's loss
/// callback; the configuration's `gpu` request is not read.
///
/// # Safety
/// As `sgi_mgras_new`.
pub unsafe fn new_on(cfg: *const IrisMgrasConfig, host: *const IrisEngineHost, gpu: Arc<Gpu>, loss: Arc<GpuLoss>) -> *mut IrisEngine {
    unsafe { new_with(cfg, host, Some((gpu, loss))) }
}

unsafe fn new_with(cfg: *const IrisMgrasConfig, host: *const IrisEngineHost, given: Option<(Arc<Gpu>, Arc<GpuLoss>)>) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        let board = match c.board {
            0 => Board::Solid,
            1 => Board::High,
            2 => Board::Max,
            b => {
                log::error!("sgi_mgras_new: board {b}");
                return None;
            }
        };
        if c.common.lent_len != 0 {
            log::error!("sgi_mgras_new: MGRAS has its own memory and takes none lent");
            return None;
        }
        let mut config = Config::new(board, if c.common.threaded() { Mode::Threaded } else { Mode::Inline });
        config.octane = c.flags & 1 != 0;
        if c.flags & 2 != 0 {
            config.entry_bytes = 8;
        }
        if c.flags & 4 != 0 {
            config.bdvers = Some(c.bdvers);
            config.cmap_rev = c.cmap_rev;
            config.ge_revision = c.ge_revision;
        }
        if c.common.queue_ops != 0 {
            config.queue_ops = c.common.queue_ops as usize;
        }
        #[cfg(feature = "gr4-jit")]
        {
            use crate::dev::mgras::rss_jit::{MODE_ASYNC, MODE_OFF, MODE_SYNC};
            config.jit = match c.jit {
                0 => Some(MODE_OFF),
                1 => Some(MODE_SYNC),
                2 => Some(MODE_ASYNC),
                _ => None,
            };
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
                            log::error!("sgi_mgras_new: GPU: {e}");
                            return None;
                        }
                    },
                };
                (gpu, loss)
            }
        };
        match MgrasEngine::new(config, mem, gpu.as_ref(), notify) {
            Ok(engine) => {
                loss.attach(engine.gpu_lost_handler());
                Some(engine)
            }
            Err(e) => {
                log::error!("sgi_mgras_new: {e}");
                None
            }
        }
    })
}

/// Bring a box of pixels current for a CPU reader.
///
/// # Safety
/// `h` is a live MGRAS handle and `r` points to a region.
#[no_mangle]
pub unsafe extern "C" fn sgi_mgras_observe(h: *mut IrisEngine, r: *const IrisMgrasRegion) -> c_int {
    unsafe {
        call::<MgrasEngine<CMem>, _>(h, Point::Observe, IRIS_ENGINE_FAULT, |e| {
            if r.is_null() {
                return IRIS_ENGINE_ERROR;
            }
            e.observe(&region(&*r)).map_or(IRIS_ENGINE_FAULT, |_| IRIS_ENGINE_OK)
        })
    }
}

/// The pixel at (x, y) of the buffer `r` names, as stored (36 bits for a wide buffer).
///
/// # Safety
/// `h` is a live MGRAS handle, `r` a region and `val` a `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_mgras_pixel(h: *mut IrisEngine, r: *const IrisMgrasRegion, x: u32, y: u32, val: *mut u64) -> c_int {
    unsafe {
        call::<MgrasEngine<CMem>, _>(h, Point::Pixel, IRIS_ENGINE_FAULT, |e| {
            if r.is_null() || val.is_null() {
                return IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            *val = e.pixel(&region(&*r).buffer, x, y);
            IRIS_ENGINE_OK
        })
    }
}

/// Pixels `x0` up to `x1` (exclusive) of row `y` of the buffer `r` names, current after one observe:
/// the low 32 bits of each wide pixel, or its 9 bits for an overlay buffer. `out` has room for
/// `x1 - x0` words. A range outside the planes answers `IRIS_ENGINE_ERROR`.
///
/// # Safety
/// `h` is a live MGRAS handle, `r` a region (only its buffer is read) and `out` points to `x1 - x0` words.
#[no_mangle]
pub unsafe extern "C" fn sgi_mgras_read_row(h: *mut IrisEngine, r: *const IrisMgrasRegion, y: u32, x0: u32, x1: u32, out: *mut u32) -> c_int {
    unsafe {
        call::<MgrasEngine<CMem>, _>(h, Point::Pixel, IRIS_ENGINE_FAULT, |e| {
            if r.is_null() || out.is_null() || x0 > x1 || x1 as usize > rss::WIDTH || y as usize >= rss::HEIGHT {
                return IRIS_ENGINE_ERROR;
            }
            if e.fault().is_some() {
                return IRIS_ENGINE_FAULT;
            }
            let row = std::slice::from_raw_parts_mut(out, (x1 - x0) as usize);
            e.read_row(&region(&*r).buffer, y, x0, x1, row).map_or(IRIS_ENGINE_FAULT, |_| IRIS_ENGINE_OK)
        })
    }
}

/// The HQ flags as the board holds them.
///
/// # Safety
/// `h` is a live MGRAS handle.
#[no_mangle]
pub unsafe extern "C" fn sgi_mgras_flags(h: *mut IrisEngine) -> u32 {
    unsafe { call::<MgrasEngine<CMem>, _>(h, Point::Flags, 0, |e| e.flags()) }
}
