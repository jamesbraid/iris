//! The C ABI of the GR2 engine: its constructor and the entries that name its own types. Everything else
//! is `sgi_engine::ffi`. `include/sgi_gr2.h` is the header, written by hand; `tools/check-header` compares
//! it with cbindgen's.

use std::ffi::c_int;

use sgi_engine::ffi::{build, call, host_parts, read_config, CMem, IrisEngine, IrisEngineConfig, IrisEngineHost, IRIS_ENGINE_ERROR, IRIS_ENGINE_OK};
use sgi_engine::inject::Point;

use crate::dev::gr2::engine::{Config, Gr2Engine};
use crate::dev::gr2::{AuxWindow, Gr2Probe};

/// Bit 0 of the engine's `irq_levels`: the HQ2 FIFO level, GIO interrupt line 0. Not raised yet.
pub const IRIS_GR2_IRQ_FIFO: u64 = 1;

/// The GR2 configuration: the shared part, then the board's.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisGr2Config {
    pub common: IrisEngineConfig,
    /// Geometry engines fitted: 1, 2, 4 or 8.
    pub ges: u32,
    /// What the four board configuration ports (+0x6c000 to +0x6c00c) read as.
    pub ports: [u32; 4],
    /// The HQ2 revision in the status word and in `hq_gepc`.
    pub hq_rev: u32,
    /// The window at +0x6c1c0: 0 not decoded (an Indigo), 1 iris's own word, 2 sixteen byte cells.
    pub aux_window: u32,
    /// The RE3 span JIT: -1 the default, 0 off, 1 compile on first use, 2 compile in the background.
    pub jit: i32,
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `host` point to valid structures; the callbacks and `host->user` stay valid until
/// `iris_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn sgi_gr2_new(cfg: *const IrisGr2Config, host: *const IrisEngineHost) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        if !c.common.threaded() {
            log::error!("sgi_gr2_new: the HQ2 and RE3 run on threads of their own: mode must be threaded");
            return None;
        }
        if c.common.lent_len != 0 {
            log::error!("sgi_gr2_new: GR2 has its own memory and takes none lent");
            return None;
        }
        if unsafe { c.common.gpu_request() }.is_some() {
            log::error!("sgi_gr2_new: GR2 composes on the CPU and has no GPU unit");
            return None;
        }
        let aux_window = match c.aux_window {
            0 => AuxWindow::None,
            1 => AuxWindow::Legacy,
            2 => AuxWindow::Bytes,
            n => {
                log::error!("sgi_gr2_new: aux_window {n}");
                return None;
            }
        };
        let mut config = Config::new(Gr2Probe { ges: c.ges, ports: c.ports, hq_rev: c.hq_rev, aux_window });
        config.jit = c.jit;
        match Gr2Engine::new(config, mem, notify) {
            Ok(engine) => Some(engine),
            Err(e) => {
                log::error!("sgi_gr2_new: {e}");
                None
            }
        }
    })
}

/// The hardware cursor as the VC1's registers hold it: the glyph's top-left pixel in screen pixels from
/// the top left in `*x` and `*y`, set whether or not it is shown, and the return is 1 when it is shown, 0
/// when not or for a NULL output.
///
/// # Safety
/// `h` is a live GR2 handle; `x` and `y` point to `int32_t`.
#[no_mangle]
pub unsafe extern "C" fn sgi_gr2_cursor_position(h: *mut IrisEngine, x: *mut i32, y: *mut i32) -> c_int {
    unsafe {
        call::<Gr2Engine<CMem>, _>(h, Point::Flags, 0, |e| {
            if x.is_null() || y.is_null() {
                return 0;
            }
            let (cx, cy, shown) = e.cursor_position();
            (*x, *y) = (cx, cy);
            shown as c_int
        })
    }
}
