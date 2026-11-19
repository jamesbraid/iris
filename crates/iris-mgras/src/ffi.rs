//! The C ABI of the MGRAS engine. `include/iris_mgras.h` is the header,
//! written by hand; `tools/check-header` compares it with cbindgen's.
//!
//! Every entry catches panics: the engine stops with a device fault, the call
//! answers `IRIS_MGRAS_FAULT` (or NULL, or zero), and later calls on the
//! handle answer the same until `iris_mgras_reset`. One thread calls at a time.

use std::ffi::{c_char, c_int, c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::dev::mgras::engine::{Board, Config, MgrasEngine, Mode, Region};
use crate::dev::mgras::pixmem::{Buffer, Kind};
use crate::display::Rect;
use crate::inject::{hit, Point};
use crate::interface::{decode_chunks, encode_chunks, BoardEngine, Chunk, Event, HostMem, MemFault, Read, Swap, VecSource, Write};

pub const IRIS_MGRAS_ABI_VERSION: u32 = 1;

pub const IRIS_MGRAS_OK: c_int = 0;
pub const IRIS_MGRAS_BUSY: c_int = 1;
pub const IRIS_MGRAS_FAULT: c_int = 2;
pub const IRIS_MGRAS_ERROR: c_int = 3;

pub const IRIS_MGRAS_EVENT_FLAGS: u32 = 1;
pub const IRIS_MGRAS_EVENT_DAMAGE: u32 = 2;
pub const IRIS_MGRAS_EVENT_NOTE: u32 = 3;
pub const IRIS_MGRAS_EVENT_BUS_FAULT: u32 = 4;
pub const IRIS_MGRAS_EVENT_DEVICE_FAULT: u32 = 5;

/// Host memory callbacks. `ram_*` return 0 on success and are called from the
/// engine's worker thread: lock-free, RAM only. `bus_*` may be NULL (the RAM
/// path serves), may reach MMIO and are called only inside an engine call.
#[repr(C)]
pub struct IrisMgrasMemOps {
    pub ram_read: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, swap: u32) -> c_int>,
    pub ram_write: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *const u8, len: usize, swap: u32) -> c_int>,
    pub bus_read: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, swap: u32) -> c_int>,
    pub bus_write: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *const u8, len: usize, swap: u32) -> c_int>,
}

#[repr(C)]
pub struct IrisMgrasConfig {
    pub abi_version: u32,
    /// 0 Solid IMPACT, 1 High, 2 Maximum.
    pub board: u32,
    /// Bit 0: the Octane's HQ4 front.
    pub flags: u32,
    /// 0 inline, 1 threaded.
    pub mode: u32,
    /// Operations queued before a write answers busy; 0 for the default.
    pub queue_ops: u32,
    /// -1 the default, 0 interpreter only, 1 compile on first use, 2 compile in the background.
    pub jit: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct IrisMgrasPump {
    pub idle: u32,
    pub events: u32,
    pub has_deadline: u32,
    pub next_deadline: u64,
}

#[repr(C)]
pub struct IrisMgrasEvent {
    pub kind: u32,
    /// BUS_FAULT: nonzero for a write.
    pub write: u32,
    /// DAMAGE: the store key. BUS_FAULT: the address.
    pub store: u64,
    pub planes: u64,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    /// NOTE and DEVICE_FAULT: NUL-terminated, cut to fit.
    pub text: [c_char; 96],
}

#[repr(C)]
pub struct IrisMgrasRegion {
    pub page: u32,
    /// Nonzero: the 9-bit overlay buffer.
    pub overlay: u32,
    /// The raster DRBSIZE register the buffer was laid out with.
    pub drbsize: u32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

struct CMem {
    ops: IrisMgrasMemOps,
    user: *mut c_void,
}

// SAFETY: the contract of `IrisMgrasMemOps`: the `ram_*` callbacks may be
// called from any thread, and the caller keeps `user` valid for the handle's life.
unsafe impl Send for CMem {}
unsafe impl Sync for CMem {}

impl HostMem for CMem {
    fn ram_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        let f = self.ops.ram_read.ok_or(MemFault)?;
        // SAFETY: the callback's contract; `out` is valid for `len` bytes.
        (unsafe { f(self.user, addr, out.as_mut_ptr(), out.len(), swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }
    fn ram_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        let f = self.ops.ram_write.ok_or(MemFault)?;
        // SAFETY: as `ram_read`.
        (unsafe { f(self.user, addr, data.as_ptr(), data.len(), swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        let Some(f) = self.ops.bus_read else { return self.ram_read(addr, out, swap) };
        // SAFETY: as `ram_read`.
        (unsafe { f(self.user, addr, out.as_mut_ptr(), out.len(), swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        let Some(f) = self.ops.bus_write else { return self.ram_write(addr, data, swap) };
        // SAFETY: as `ram_read`.
        (unsafe { f(self.user, addr, data.as_ptr(), data.len(), swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }
}

struct Notify(unsafe extern "C" fn(*mut c_void), *mut c_void);

// SAFETY: the notify callback's contract: any thread, only schedules.
unsafe impl Send for Notify {}
unsafe impl Sync for Notify {}

/// The opaque handle.
pub struct IrisMgras {
    engine: MgrasEngine<CMem>,
    state: Vec<u8>,
    fault: Option<CString>,
}

// A panic must become a device fault, which needs unwinding.
#[cfg(all(panic = "abort", not(test)))]
compile_error!("iris-mgras needs panic = \"unwind\" (iris's release profile aborts: set CARGO_PROFILE_RELEASE_PANIC=unwind)");

/// Run `f` on the handle, catching a panic into a device fault.
fn guarded<R>(h: *mut IrisMgras, point: Point, on_fault: R, f: impl FnOnce(&mut IrisMgras) -> R) -> R {
    if h.is_null() {
        return on_fault;
    }
    // SAFETY: the caller passes a live handle from `iris_mgras_new`.
    let hd = unsafe { &mut *h };
    match catch_unwind(AssertUnwindSafe(|| {
        hit(point);
        f(hd)
    })) {
        Ok(r) => r,
        Err(p) => {
            let text = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into());
            hd.engine.fail(&format!("panic in {point:?}: {text}"));
            on_fault
        }
    }
}

fn code_write(w: Write) -> c_int {
    match w {
        Write::Done => IRIS_MGRAS_OK,
        Write::Busy => IRIS_MGRAS_BUSY,
        Write::Fault => IRIS_MGRAS_FAULT,
    }
}

fn region(r: &IrisMgrasRegion) -> Region {
    Region { buffer: Buffer::new(r.page, if r.overlay != 0 { Kind::Overlay } else { Kind::Wide }, r.drbsize), rect: Rect::new(r.x0, r.y0, r.x1, r.y1) }
}

/// Start an engine. NULL if the arguments are wrong or the engine cannot be built.
///
/// # Safety
/// `cfg` and `mem` point to valid structures; the callbacks and `mem_user`
/// stay valid until `iris_mgras_free`.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_new(
    cfg: *const IrisMgrasConfig,
    mem: *const IrisMgrasMemOps,
    mem_user: *mut c_void,
    notify: Option<unsafe extern "C" fn(*mut c_void)>,
    notify_user: *mut c_void,
) -> *mut IrisMgras {
    let built = catch_unwind(AssertUnwindSafe(|| {
        hit(Point::New);
        if cfg.is_null() || mem.is_null() {
            return None;
        }
        let (c, m) = unsafe { (&*cfg, &*mem) };
        if c.abi_version != IRIS_MGRAS_ABI_VERSION {
            log::error!("iris_mgras_new: ABI version {} (this is {IRIS_MGRAS_ABI_VERSION})", c.abi_version);
            return None;
        }
        let board = match c.board {
            0 => Board::Solid,
            1 => Board::High,
            2 => Board::Max,
            b => {
                log::error!("iris_mgras_new: board {b}");
                return None;
            }
        };
        let mut config = Config::new(board, if c.mode == 1 { Mode::Threaded } else { Mode::Inline });
        config.octane = c.flags & 1 != 0;
        if c.queue_ops != 0 {
            config.queue_ops = c.queue_ops as usize;
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
        let mem = CMem {
            ops: IrisMgrasMemOps { ram_read: m.ram_read, ram_write: m.ram_write, bus_read: m.bus_read, bus_write: m.bus_write },
            user: mem_user,
        };
        let notify = notify.map(|f| {
            let n = Notify(f, notify_user);
            Box::new(move || {
                let n = &n;
                // SAFETY: `Notify`'s contract.
                unsafe { (n.0)(n.1) }
            }) as Box<dyn Fn() + Send + Sync>
        });
        match MgrasEngine::new(config, mem, None, notify) {
            Ok(engine) => Some(Box::new(IrisMgras { engine, state: Vec::new(), fault: None })),
            Err(e) => {
                log::error!("iris_mgras_new: {e}");
                None
            }
        }
    }));
    match built {
        Ok(Some(b)) => Box::into_raw(b),
        Ok(None) => std::ptr::null_mut(),
        Err(_) => {
            log::error!("iris_mgras_new: panic");
            std::ptr::null_mut()
        }
    }
}

/// Stop the engine and free the handle.
///
/// # Safety
/// `h` is a handle from `iris_mgras_new`, or NULL, and is not used again.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_free(h: *mut IrisMgras) {
    if h.is_null() {
        return;
    }
    let b = unsafe { Box::from_raw(h) };
    let _ = catch_unwind(AssertUnwindSafe(move || {
        hit(Point::Free);
        drop(b);
    }));
}

/// A register or FIFO write at a host-window offset, 32 or 64 bits wide.
/// Returns OK, BUSY (queue full, nothing taken) or FAULT.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_write(h: *mut IrisMgras, off: u32, bits: u32, val: u64) -> c_int {
    guarded(h, Point::Write, IRIS_MGRAS_FAULT, |h| code_write(h.engine.write(off, bits, val)))
}

/// A batch of FIFO words into `port`; returns how many were taken.
///
/// # Safety
/// `h` is a live handle and `words` points to `n` words.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_push_words(h: *mut IrisMgras, port: u32, words: *const u32, n: usize) -> usize {
    guarded(h, Point::PushWords, 0, |h| {
        if words.is_null() {
            return 0;
        }
        h.engine.push_words(port, unsafe { std::slice::from_raw_parts(words, n) })
    })
}

/// A register read. OK with `*val` set, BUSY while queued work could change
/// the answer, or FAULT.
///
/// # Safety
/// `h` is a live handle and `val` points to a `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_read(h: *mut IrisMgras, off: u32, bits: u32, val: *mut u64) -> c_int {
    guarded(h, Point::Read, IRIS_MGRAS_FAULT, |h| match h.engine.read(off, bits) {
        Read::Value(v) => {
            if !val.is_null() {
                unsafe { *val = v };
            }
            IRIS_MGRAS_OK
        }
        Read::Busy => IRIS_MGRAS_BUSY,
        Read::Fault => IRIS_MGRAS_FAULT,
    })
}

/// Whether a write (`write` non-zero) or read at `off` would answer BUSY now:
/// 1 if it would, 0 if not, FAULT on a device fault.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_access_waits(h: *mut IrisMgras, off: u32, bits: u32, write: u32) -> c_int {
    guarded(h, Point::AccessWaits, IRIS_MGRAS_FAULT, |h| h.engine.access_waits(off, bits, write != 0) as c_int)
}

/// Inline mode: run queued work, at most `max_ops`; returns how many ran.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_run(h: *mut IrisMgras, max_ops: usize) -> usize {
    guarded(h, Point::Run, 0, |h| h.engine.run(max_ops))
}

/// The pump: hand queued work over, at the shell's time `now_ns`. Never waits.
///
/// # Safety
/// `h` is a live handle and `pump` points to an `IrisMgrasPump`.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_advance(h: *mut IrisMgras, now_ns: u64, pump: *mut IrisMgrasPump) -> c_int {
    guarded(h, Point::Advance, IRIS_MGRAS_FAULT, |h| {
        let p = h.engine.advance(now_ns);
        if !pump.is_null() {
            unsafe {
                *pump = IrisMgrasPump {
                    idle: p.idle as u32,
                    events: p.events as u32,
                    has_deadline: p.next_deadline.is_some() as u32,
                    next_deadline: p.next_deadline.unwrap_or(0),
                }
            };
        }
        IRIS_MGRAS_OK
    })
}

/// Wait for the engine's CPU work for at most `budget_ns` of host time
/// (inline mode runs the queue). Returns 1 if idle, 0 if not, or FAULT (2).
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_wait_idle(h: *mut IrisMgras, now_ns: u64, budget_ns: u64) -> c_int {
    guarded(h, Point::WaitIdle, IRIS_MGRAS_FAULT, |h| {
        let idle = h.engine.wait_idle(now_ns, budget_ns);
        if h.engine.fault().is_some() {
            IRIS_MGRAS_FAULT
        } else {
            idle as c_int
        }
    })
}

/// Move up to `cap` events into `out`, oldest first; returns how many. Events
/// beyond `cap` stay for the next call.
///
/// # Safety
/// `h` is a live handle and `out` points to `cap` events.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_drain_events(h: *mut IrisMgras, out: *mut IrisMgrasEvent, cap: usize) -> usize {
    guarded(h, Point::DrainEvents, 0, |h| {
        if out.is_null() {
            return 0;
        }
        let mut evs = Vec::new();
        h.engine.drain_events_limit(&mut evs, cap);
        for (i, e) in evs.iter().enumerate() {
            unsafe { out.add(i).write(c_event(e)) };
        }
        evs.len()
    })
}

fn c_event(e: &Event) -> IrisMgrasEvent {
    let mut c = IrisMgrasEvent { kind: 0, write: 0, store: 0, planes: 0, x0: 0, y0: 0, x1: 0, y1: 0, text: [0; 96] };
    let text = |c: &mut IrisMgrasEvent, s: &str| {
        let room = c.text.len() - 1;
        for (d, b) in c.text[..room].iter_mut().zip(s.bytes()) {
            *d = b as c_char;
        }
    };
    match e {
        Event::Flags => c.kind = IRIS_MGRAS_EVENT_FLAGS,
        Event::Damage(m) => {
            c.kind = IRIS_MGRAS_EVENT_DAMAGE;
            (c.store, c.planes, c.x0, c.y0, c.x1, c.y1) = (m.store, m.planes, m.rect.x0, m.rect.y0, m.rect.x1, m.rect.y1);
        }
        Event::Note(t) => {
            c.kind = IRIS_MGRAS_EVENT_NOTE;
            text(&mut c, t);
        }
        Event::BusFault { addr, write } => {
            c.kind = IRIS_MGRAS_EVENT_BUS_FAULT;
            (c.store, c.write) = (*addr, *write as u32);
        }
        Event::DeviceFault(t) => {
            c.kind = IRIS_MGRAS_EVENT_DEVICE_FAULT;
            text(&mut c, t);
        }
        _ => {}
    }
    c
}

/// Bring a box of pixels current for a CPU reader.
///
/// # Safety
/// `h` is a live handle and `r` points to a region.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_observe(h: *mut IrisMgras, r: *const IrisMgrasRegion) -> c_int {
    guarded(h, Point::Observe, IRIS_MGRAS_FAULT, |h| {
        if r.is_null() {
            return IRIS_MGRAS_ERROR;
        }
        h.engine.observe(&region(unsafe { &*r })).map_or(IRIS_MGRAS_FAULT, |_| IRIS_MGRAS_OK)
    })
}

/// The pixel at (x, y) of the buffer `r` names, as stored (36 bits for a wide buffer).
///
/// # Safety
/// `h` is a live handle, `r` a region and `val` a `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_pixel(h: *mut IrisMgras, r: *const IrisMgrasRegion, x: u32, y: u32, val: *mut u64) -> c_int {
    guarded(h, Point::Pixel, IRIS_MGRAS_FAULT, |h| {
        if r.is_null() || val.is_null() {
            return IRIS_MGRAS_ERROR;
        }
        let g = region(unsafe { &*r });
        unsafe { *val = h.engine.pixel(&g.buffer, x, y) };
        IRIS_MGRAS_OK
    })
}

/// Bring all memory current; needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_observe_all(h: *mut IrisMgras) -> c_int {
    guarded(h, Point::ObserveAll, IRIS_MGRAS_FAULT, |h| h.engine.observe_all().map_or(IRIS_MGRAS_ERROR, |_| IRIS_MGRAS_OK))
}

/// Run everything queued and park the worker. Before save, load, reset and
/// `iris_mgras_observe_all`.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_quiesce(h: *mut IrisMgras) -> c_int {
    guarded(h, Point::Quiesce, IRIS_MGRAS_FAULT, |h| h.engine.quiesce().map_or(IRIS_MGRAS_FAULT, |_| IRIS_MGRAS_OK))
}

/// Start the worker again after `iris_mgras_quiesce`.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_resume(h: *mut IrisMgras) -> c_int {
    guarded(h, Point::Resume, IRIS_MGRAS_FAULT, |h| {
        h.engine.resume();
        IRIS_MGRAS_OK
    })
}

/// Power-on state; clears a fault. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_reset(h: *mut IrisMgras) -> c_int {
    guarded(h, Point::Reset, IRIS_MGRAS_FAULT, |h| {
        h.fault = None;
        h.engine.reset().map_or(IRIS_MGRAS_ERROR, |_| IRIS_MGRAS_OK)
    })
}

/// Save the state into the handle; read it with `iris_mgras_state_len` and
/// `iris_mgras_state_copy`. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_save(h: *mut IrisMgras) -> c_int {
    guarded(h, Point::Save, IRIS_MGRAS_FAULT, |h| {
        let mut chunks: Vec<Chunk> = Vec::new();
        match h.engine.save(&mut chunks).map_err(|_| ()).and_then(|_| encode_chunks(&chunks).map_err(|_| ())) {
            Ok(b) => {
                h.state = b;
                IRIS_MGRAS_OK
            }
            Err(()) => IRIS_MGRAS_ERROR,
        }
    })
}

/// The size in bytes of the state `iris_mgras_save` made.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_state_len(h: *mut IrisMgras) -> usize {
    guarded(h, Point::StateCopy, 0, |h| h.state.len())
}

/// Copy up to `cap` bytes of the saved state; returns the bytes copied.
///
/// # Safety
/// `h` is a live handle and `buf` points to `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_state_copy(h: *mut IrisMgras, buf: *mut u8, cap: usize) -> usize {
    guarded(h, Point::StateCopy, 0, |h| {
        let n = h.state.len().min(cap);
        if !buf.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(h.state.as_ptr(), buf, n) };
        }
        n
    })
}

/// Load a state `iris_mgras_state_copy` gave. A state that fails leaves the
/// engine as it was. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle and `buf` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_load(h: *mut IrisMgras, buf: *const u8, len: usize) -> c_int {
    guarded(h, Point::Load, IRIS_MGRAS_FAULT, |h| {
        if buf.is_null() {
            return IRIS_MGRAS_ERROR;
        }
        let loaded = decode_chunks(unsafe { std::slice::from_raw_parts(buf, len) }).map_err(|_| ()).and_then(|c| h.engine.load(&mut VecSource(c.into_iter())).map_err(|_| ()));
        if loaded.is_ok() { IRIS_MGRAS_OK } else { IRIS_MGRAS_ERROR }
    })
}

/// The device-fault message, or NULL. Valid until the next call on the handle.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_fault(h: *mut IrisMgras) -> *const c_char {
    guarded(h, Point::Fault, std::ptr::null(), |h| {
        h.fault = h.engine.fault().and_then(|m| CString::new(m.replace('\0', " ")).ok());
        h.fault.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())
    })
}

/// The HQ flags as the board holds them.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_mgras_flags(h: *mut IrisMgras) -> u32 {
    guarded(h, Point::Flags, 0, |h| h.engine.flags())
}
