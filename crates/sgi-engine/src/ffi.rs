//! The C ABI every board shares: `include/iris_engine.h` is the header,
//! written by hand; `tools/check-header` compares it with cbindgen's.
//!
//! A board crate builds an engine, hands it to `build`, and adds the few
//! entries that name its own types (`call` runs one on the concrete engine).
//! Everything else here is the board's too: the shell writes its glue once.
//!
//! Every entry runs under `guard`: one call at a time (a second, from a
//! callback or another thread, answers as a fault without touching the
//! engine), a panic becomes a device fault, and a faulted engine answers
//! as the trait says. The crate does not choose the panic profile; see
//! `iris_engine_contains_panics`.

use std::any::Any;
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::mem::{size_of, MaybeUninit};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::engine::{BoardEngine, Event, Fault, HeadTiming, Idle, Pump, Read, Refused, Source, StallOn, Write};
use crate::inject::{hit, Point};
use crate::mem::{DirtyRun, HostMem, Lent, MemFault, Swap};
use crate::state::{Chunk, StateSink, StateSource};

pub const IRIS_ENGINE_ABI_VERSION: u32 = 1;

pub const IRIS_ENGINE_OK: c_int = 0;
pub const IRIS_ENGINE_BUSY: c_int = 1;
/// The device faulted, or a call re-entered the engine (which leaves it alone).
pub const IRIS_ENGINE_FAULT: c_int = 2;
/// A bad argument, or a state callback failed.
pub const IRIS_ENGINE_ERROR: c_int = 3;
pub const IRIS_ENGINE_NOT_QUIESCED: c_int = 4;
pub const IRIS_ENGINE_MIDWAY: c_int = 5;
pub const IRIS_ENGINE_MISMATCH: c_int = 6;

pub const IRIS_ENGINE_IDLE: u32 = 0;
pub const IRIS_ENGINE_WORKING: u32 = 1;
pub const IRIS_ENGINE_STALLED_FRAME: u32 = 2;
pub const IRIS_ENGINE_STALLED_TIME: u32 = 3;
pub const IRIS_ENGINE_FAULTED: u32 = 4;
/// The command stream stopped for good (a fetch fault) and the engine is not faulted.
pub const IRIS_ENGINE_STOPPED: u32 = 5;

pub const IRIS_ENGINE_EVENT_IRQ: u32 = 1;
pub const IRIS_ENGINE_EVENT_DAMAGE: u32 = 2;
pub const IRIS_ENGINE_EVENT_NOTE: u32 = 3;
pub const IRIS_ENGINE_EVENT_BUS_FAULT: u32 = 4;
pub const IRIS_ENGINE_EVENT_DEVICE_FAULT: u32 = 5;
pub const IRIS_ENGINE_EVENT_GPU_LOST: u32 = 6;
pub const IRIS_ENGINE_EVENT_INTERRUPT: u32 = 7;
pub const IRIS_ENGINE_EVENT_CREDITS: u32 = 8;

pub const IRIS_ENGINE_GPU_NONE: u32 = 0;
pub const IRIS_ENGINE_GPU_AUTO: u32 = 1;
pub const IRIS_ENGINE_GPU_SOFTWARE: u32 = 2;

/// The shell's side: host memory, the notify hook and the one `user`
/// pointer all callbacks get. `ram_*` return 0 on success and are called
/// from engine threads; `bus_*` may be NULL.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineHost {
    /// `IRIS_ENGINE_ABI_VERSION`.
    pub abi_version: u32,
    /// `sizeof` this structure; a field added later reads as zero for a caller
    /// that is older.
    pub struct_size: u32,
    pub user: *mut c_void,
    pub ram_read: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, swap: u32) -> c_int>,
    pub ram_write: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *const u8, len: usize, swap: u32) -> c_int>,
    pub bus_read: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, swap: u32) -> c_int>,
    pub bus_write: Option<unsafe extern "C" fn(user: *mut c_void, addr: u64, buf: *const u8, len: usize, swap: u32) -> c_int>,
    pub notify: Option<unsafe extern "C" fn(user: *mut c_void)>,
}

/// The configuration every board shares. A board's own configuration
/// starts with this and adds its fields after it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineConfig {
    /// `IRIS_ENGINE_ABI_VERSION`.
    pub abi_version: u32,
    /// `sizeof` the board's whole configuration; fields past it read as zero.
    pub struct_size: u32,
    /// 0 inline, 1 threaded.
    pub mode: u32,
    /// Operations queued before a write answers busy; 0 for the default.
    pub queue_ops: u32,
    /// `IRIS_ENGINE_GPU_*`: whether the engine opens a GPU unit.
    pub gpu: u32,
    /// The GPU backend (`auto`, `vulkan`, `metal`, `dx12`, `gl`); NULL is `auto`.
    pub gpu_backend: *const c_char,
    /// Board memory the shell lends: 8-aligned, a whole number of words, valid
    /// until `iris_engine_free`. NULL and 0 for a board that has its own.
    pub lent_ptr: *mut u8,
    pub lent_len: u64,
    /// Room for fields the shared part grows, so the board's start where they were.
    pub reserved: [u64; 4],
}

impl IrisEngineConfig {
    /// The lent memory, if the shell gave any and it is well formed.
    ///
    /// # Safety
    /// The pointer is valid as `Lent::new` asks.
    pub unsafe fn lent(&self) -> Option<Lent> {
        unsafe { Lent::new(self.lent_ptr, self.lent_len as usize) }
    }

    pub fn threaded(&self) -> bool {
        self.mode == 1
    }

    /// The GPU the shell asked for: the backend name (`auto` when NULL) and whether to
    /// prefer a software adapter. None for `IRIS_ENGINE_GPU_NONE`.
    ///
    /// # Safety
    /// `gpu_backend` is NULL or a NUL-terminated string.
    pub unsafe fn gpu_request(&self) -> Option<(String, bool)> {
        if self.gpu == IRIS_ENGINE_GPU_NONE {
            return None;
        }
        let backend = if self.gpu_backend.is_null() { "auto".to_string() } else { unsafe { CStr::from_ptr(self.gpu_backend) }.to_string_lossy().into_owned() };
        Some((backend, self.gpu == IRIS_ENGINE_GPU_SOFTWARE))
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisEngineIdle {
    /// `IRIS_ENGINE_IDLE` .. `IRIS_ENGINE_STOPPED`.
    pub state: u32,
    /// `STALLED_TIME`: advance to this, in ns.
    pub stall_time: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisEnginePump {
    pub idle: IrisEngineIdle,
    /// Events wait.
    pub events: u32,
    /// `next_deadline` is valid.
    pub has_deadline: u32,
    /// When to call `iris_engine_advance` next, in ns.
    pub next_deadline: u64,
    pub irq_levels: u64,
}

/// A run of lent memory, in bytes from its start.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisEngineRun {
    pub offset: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineDamage {
    pub store: u64,
    pub planes: u64,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineBusFault {
    pub addr: u64,
    /// Non-zero for a write.
    pub write: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEnginePacket {
    pub addr: u64,
    pub sender: u32,
    pub widget: u32,
    pub vector: u32,
    pub level: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineCredits {
    pub source: u32,
    pub retired: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union IrisEngineEventData {
    pub damage: IrisEngineDamage,
    pub bus_fault: IrisEngineBusFault,
    pub packet: IrisEnginePacket,
    pub credits: IrisEngineCredits,
    /// NOTE, DEVICE_FAULT, GPU_LOST: NUL-terminated, cut to fit.
    pub text: [c_char; 96],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineEvent {
    /// `IRIS_ENGINE_EVENT_*`; says which member of `data` is valid.
    pub kind: u32,
    pub data: IrisEngineEventData,
}

/// A piece of saved state, as `save` gives it to the shell's callback and
/// the shell's callback gives it to `load`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrisEngineChunk {
    /// NUL-terminated.
    pub name: [c_char; 32],
    pub index: u64,
    /// Valid until the next callback.
    pub bytes: *const u8,
    pub len: usize,
}

struct State {
    eng: Box<dyn DynEngine>,
    fault: Option<CString>,
    error: Option<CString>,
    /// Dirty runs the engine reported that a short `cap` left for the next call.
    dirty: Vec<DirtyRun>,
}

/// The opaque handle.
pub struct IrisEngine {
    busy: AtomicBool,
    state: UnsafeCell<State>,
}

/// `BoardEngine` without its region, so a handle can hold any board's.
trait DynEngine: Send {
    fn any(&mut self) -> &mut dyn Any;
    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write;
    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write;
    fn heads(&self) -> u32;
    fn head_timing(&self, head: u32) -> Option<HeadTiming>;
    fn head_bits(&self, head: u32) -> u32;
    fn busy_ns(&self) -> u64;
    fn scanout10(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault>;
    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault>;
    fn head_deadline(&self, head: u32) -> Option<u64>;
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize;
    fn read(&mut self, off: u32, bits: u32) -> Read;
    fn set_source(&mut self, source: Source);
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool;
    fn run(&mut self, max_ops: usize) -> usize;
    fn frame(&mut self, now_ns: u64);
    fn advance(&mut self, now_ns: u64) -> Pump;
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle;
    fn irq_levels(&self) -> u64;
    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize);
    fn drain_dirty(&mut self, out: &mut Vec<DirtyRun>);
    fn trapped(&self, out: &mut Vec<DirtyRun>);
    fn observe_lent(&mut self, off: u64, len: u64) -> Result<(), Fault>;
    fn cpu_wrote(&mut self, off: u64, len: u64);
    fn observe_all(&mut self) -> Result<(), Refused>;
    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused>;
    fn resume(&mut self);
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused>;
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused>;
    fn state_version(&self) -> u32;
    fn reset(&mut self) -> Result<(), Refused>;
    fn fault(&self) -> Option<String>;
    fn fail(&self, why: &str);
}

impl<E: BoardEngine + Send + 'static> DynEngine for E {
    fn any(&mut self) -> &mut dyn Any {
        self
    }
    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        BoardEngine::write(self, off, bits, val)
    }
    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        BoardEngine::write_at(self, now_ns, off, bits, val)
    }
    fn heads(&self) -> u32 {
        BoardEngine::heads(self)
    }
    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        BoardEngine::head_timing(self, head)
    }
    fn head_bits(&self, head: u32) -> u32 {
        BoardEngine::head_bits(self, head)
    }
    fn busy_ns(&self) -> u64 {
        BoardEngine::busy_ns(self)
    }
    fn scanout10(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        BoardEngine::scanout10(self, head, out)
    }
    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        BoardEngine::scanout(self, head, out)
    }
    fn head_deadline(&self, head: u32) -> Option<u64> {
        BoardEngine::head_deadline(self, head)
    }
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        BoardEngine::push_words(self, port, words)
    }
    fn read(&mut self, off: u32, bits: u32) -> Read {
        BoardEngine::read(self, off, bits)
    }
    fn set_source(&mut self, source: Source) {
        BoardEngine::set_source(self, source)
    }
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        BoardEngine::access_waits(self, off, bits, write)
    }
    fn run(&mut self, max_ops: usize) -> usize {
        BoardEngine::run(self, max_ops)
    }
    fn frame(&mut self, now_ns: u64) {
        BoardEngine::frame(self, now_ns)
    }
    fn advance(&mut self, now_ns: u64) -> Pump {
        BoardEngine::advance(self, now_ns)
    }
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle {
        BoardEngine::wait_idle(self, now_ns, budget_ns)
    }
    fn irq_levels(&self) -> u64 {
        BoardEngine::irq_levels(self)
    }
    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        BoardEngine::drain_events(self, out, max)
    }
    fn drain_dirty(&mut self, out: &mut Vec<DirtyRun>) {
        BoardEngine::drain_dirty(self, out)
    }
    fn trapped(&self, out: &mut Vec<DirtyRun>) {
        BoardEngine::trapped(self, out)
    }
    fn observe_lent(&mut self, off: u64, len: u64) -> Result<(), Fault> {
        BoardEngine::observe_lent(self, off, len)
    }
    fn cpu_wrote(&mut self, off: u64, len: u64) {
        BoardEngine::cpu_wrote(self, off, len)
    }
    fn observe_all(&mut self) -> Result<(), Refused> {
        BoardEngine::observe_all(self)
    }
    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        BoardEngine::quiesce(self, budget_ns)
    }
    fn resume(&mut self) {
        BoardEngine::resume(self)
    }
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        BoardEngine::save(self, sink)
    }
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        BoardEngine::load(self, src)
    }
    fn state_version(&self) -> u32 {
        BoardEngine::state_version(self)
    }
    fn reset(&mut self) -> Result<(), Refused> {
        BoardEngine::reset(self)
    }
    fn fault(&self) -> Option<String> {
        BoardEngine::fault(self)
    }
    fn fail(&self, why: &str) {
        BoardEngine::fail(self, why)
    }
}

fn panic_text(p: &(dyn Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

/// Run `f` on the engine: refuse a re-entering call, catch a panic into a
/// device fault. `on_fault` is what the entry answers then.
fn guard<R: Copy>(h: *mut IrisEngine, point: Point, on_fault: R, f: impl FnOnce(&mut State) -> R) -> R {
    if h.is_null() {
        return on_fault;
    }
    // SAFETY: the caller passes a live handle. Only the shared part is
    // touched until `busy` says this is the one call on the engine.
    let h = unsafe { &*h };
    if h.busy.swap(true, Ordering::Acquire) {
        log::error!("{point:?}: a call came in while another was running on the engine; refused");
        return on_fault;
    }
    // SAFETY: `busy` makes this the only access to the state.
    let st = unsafe { &mut *h.state.get() };
    let r = catch_unwind(AssertUnwindSafe(|| {
        hit(point);
        f(&mut *st)
    }));
    let out = match r {
        Ok(r) => r,
        Err(p) => {
            st.eng.fail(&format!("panic in {point:?}: {}", panic_text(&*p)));
            on_fault
        }
    };
    h.busy.store(false, Ordering::Release);
    out
}

/// Run `f` on the board's concrete engine: for the entries a board adds.
///
/// # Safety
/// `h` is a live handle or NULL.
pub unsafe fn call<E: 'static, R: Copy>(h: *mut IrisEngine, point: Point, on_fault: R, f: impl FnOnce(&mut E) -> R) -> R {
    guard(h, point, on_fault, |s| match s.eng.any().downcast_mut::<E>() {
        Some(e) => f(e),
        None => {
            log::error!("{point:?}: the handle holds another board's engine");
            on_fault
        }
    })
}

/// Make a handle from what a board's `new` builds. NULL if `f` says no
/// or panics.
pub fn build<E: BoardEngine + Send + 'static>(f: impl FnOnce() -> Option<E>) -> *mut IrisEngine {
    match catch_unwind(AssertUnwindSafe(|| {
        hit(Point::New);
        f()
    })) {
        Ok(Some(e)) => Box::into_raw(Box::new(IrisEngine { busy: AtomicBool::new(false), state: UnsafeCell::new(State { eng: Box::new(e), fault: None, error: None, dirty: Vec::new() }) })),
        Ok(None) => std::ptr::null_mut(),
        Err(p) => {
            log::error!("engine construction panicked: {}", panic_text(&*p));
            std::ptr::null_mut()
        }
    }
}

/// Copy a caller's structure, whose second word is its size, into a zeroed
/// one of ours: a field the caller predates reads as zero.
unsafe fn read_sized<T>(p: *const T) -> T {
    // SAFETY: the caller's contract: `p` points to a structure that starts
    // with the abi version and its size.
    let size = unsafe { std::ptr::read_unaligned(p.cast::<u32>().add(1)) } as usize;
    let mut out = MaybeUninit::<T>::zeroed();
    unsafe { std::ptr::copy_nonoverlapping(p.cast::<u8>(), out.as_mut_ptr().cast::<u8>(), size.min(size_of::<T>())) };
    unsafe { out.assume_init() }
}

/// A board's configuration as the caller built it; `None` for another ABI version.
///
/// # Safety
/// `p` points to a structure of type `T` that starts with an `IrisEngineConfig`.
pub unsafe fn read_config<T>(p: *const T) -> Option<T> {
    if p.is_null() {
        return None;
    }
    let version = unsafe { std::ptr::read_unaligned(p.cast::<u32>()) };
    if version != IRIS_ENGINE_ABI_VERSION {
        log::error!("ABI version {version} (this is {IRIS_ENGINE_ABI_VERSION})");
        return None;
    }
    Some(unsafe { read_sized(p) })
}

/// The shell's host memory callbacks as a `HostMem`.
pub struct CMem {
    host: IrisEngineHost,
}

// SAFETY: the contract of `IrisEngineHost`: the `ram_*` callbacks may be
// called from any thread, and `user` stays valid until `iris_engine_free`.
unsafe impl Send for CMem {}
unsafe impl Sync for CMem {}

impl CMem {
    fn access(&self, f: Option<unsafe extern "C" fn(*mut c_void, u64, *mut u8, usize, u32) -> c_int>, addr: u64, buf: *mut u8, len: usize, swap: Swap) -> Result<(), MemFault> {
        let f = f.ok_or(MemFault)?;
        // SAFETY: the callback's contract; `buf` is valid for `len` bytes.
        (unsafe { f(self.host.user, addr, buf, len, swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }

    fn access_w(&self, f: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, usize, u32) -> c_int>, addr: u64, buf: &[u8], swap: Swap) -> Result<(), MemFault> {
        let f = f.ok_or(MemFault)?;
        // SAFETY: as `access`.
        (unsafe { f(self.host.user, addr, buf.as_ptr(), buf.len(), swap as u32) } == 0).then_some(()).ok_or(MemFault)
    }
}

impl HostMem for CMem {
    fn ram_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.access(self.host.ram_read, addr, out.as_mut_ptr(), out.len(), swap)
    }
    fn ram_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.access_w(self.host.ram_write, addr, data, swap)
    }
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        match self.host.bus_read {
            Some(f) => self.access(Some(f), addr, out.as_mut_ptr(), out.len(), swap),
            None => self.ram_read(addr, out, swap),
        }
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        match self.host.bus_write {
            Some(f) => self.access_w(Some(f), addr, data, swap),
            None => self.ram_write(addr, data, swap),
        }
    }
}

/// A shell's host callbacks as a `HostMem` and the notify hook. `None` for a
/// bad or foreign structure.
///
/// # Safety
/// `p` is NULL or points to an `IrisEngineHost`; the callbacks and `user` stay
/// valid until the engine is freed.
pub unsafe fn host_parts(p: *const IrisEngineHost) -> Option<(CMem, Option<Box<dyn Fn() + Send + Sync>>)> {
    let host = unsafe { read_config(p) }?;
    let notify = host.notify.map(|f| {
        let user = host.user as usize;
        Box::new(move || {
            // SAFETY: `notify`'s contract: any thread, only schedules.
            unsafe { f(user as *mut c_void) }
        }) as Box<dyn Fn() + Send + Sync>
    });
    Some((CMem { host }, notify))
}

/// Stop the engine and free the handle. The notify hook is not called after it
/// returns.
///
/// # Safety
/// `h` is a handle from a board's `new`, or NULL, and is not used again.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_free(h: *mut IrisEngine) {
    if h.is_null() {
        return;
    }
    let b = unsafe { Box::from_raw(h) };
    let _ = catch_unwind(AssertUnwindSafe(move || {
        hit(Point::Free);
        drop(b);
    }));
}

/// 1 if a panic in the engine is caught and becomes a device fault; 0 if this
/// build aborts on a panic (`panic = "abort"`), which is the build's choice.
#[no_mangle]
pub extern "C" fn iris_engine_contains_panics() -> c_int {
    cfg!(panic = "unwind") as c_int
}

fn code_write(w: Write) -> c_int {
    match w {
        Write::Done => IRIS_ENGINE_OK,
        Write::Busy => IRIS_ENGINE_BUSY,
        Write::Fault => IRIS_ENGINE_FAULT,
    }
}

fn set_error(s: &mut State, text: &str) {
    s.error = CString::new(text.replace('\0', " ")).ok();
}

fn refused(s: &mut State, r: Refused) -> c_int {
    set_error(s, &r.to_string());
    match r {
        Refused::NotQuiesced => IRIS_ENGINE_NOT_QUIESCED,
        Refused::Midway(_) => IRIS_ENGINE_MIDWAY,
        Refused::Faulted(_) => IRIS_ENGINE_FAULT,
        Refused::Mismatch(_) => IRIS_ENGINE_MISMATCH,
    }
}

fn idle_state(i: Idle) -> IrisEngineIdle {
    let (state, stall_time) = match i {
        Idle::Yes => (IRIS_ENGINE_IDLE, 0),
        Idle::Working => (IRIS_ENGINE_WORKING, 0),
        Idle::Stalled(StallOn::Frame) => (IRIS_ENGINE_STALLED_FRAME, 0),
        Idle::Stalled(StallOn::Time(t)) => (IRIS_ENGINE_STALLED_TIME, t),
        Idle::Faulted => (IRIS_ENGINE_FAULTED, 0),
        Idle::Stopped => (IRIS_ENGINE_STOPPED, 0),
    };
    IrisEngineIdle { state, stall_time }
}

fn idle_code(i: Idle) -> c_int {
    if i == Idle::Faulted { IRIS_ENGINE_FAULT } else { IRIS_ENGINE_OK }
}

/// A register or FIFO write at a host-window offset. OK, BUSY (the queue is
/// full and nothing was taken) or FAULT.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_write(h: *mut IrisEngine, off: u32, bits: u32, val: u64) -> c_int {
    guard(h, Point::Write, IRIS_ENGINE_FAULT, |s| code_write(s.eng.write(off, bits, val)))
}

/// `iris_engine_write` with the shell's time at the store, in its `advance` time base.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_write_at(h: *mut IrisEngine, now_ns: u64, off: u32, bits: u32, val: u64) -> c_int {
    guard(h, Point::Write, IRIS_ENGINE_FAULT, |s| code_write(s.eng.write_at(now_ns, off, bits, val)))
}

/// The number of scan-out heads `iris_engine_scanout` takes; 0 for a board with none.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_heads(h: *mut IrisEngine) -> u32 {
    guard(h, Point::Flags, 0, |s| s.eng.heads())
}

/// Copy head `head`'s composed picture, one `0x00RRGGBB` word a pixel, rows from the top, into
/// `buf` (`cap` words). `*w` and `*hgt` are the frame's size: 0 by 0 while the head has no mode
/// (OK, nothing copied), and the size needed when `cap` is too small (ERROR, nothing copied).
/// FAULT on a faulted engine.
///
/// # Safety
/// `h` is a live handle, `buf` points to `cap` words and `w` and `hgt` to `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_scanout(h: *mut IrisEngine, head: u32, buf: *mut u32, cap: usize, w: *mut u32, hgt: *mut u32) -> c_int {
    guard(h, Point::Pixel, IRIS_ENGINE_FAULT, |s| {
        if w.is_null() || hgt.is_null() || (buf.is_null() && cap != 0) {
            return IRIS_ENGINE_ERROR;
        }
        let mut pixels = Vec::new();
        match s.eng.scanout(head, &mut pixels) {
            Err(f) => {
                set_error(s, &f.0);
                IRIS_ENGINE_FAULT
            }
            Ok(size) => {
                let (fw, fh) = size.unwrap_or((0, 0));
                unsafe {
                    *w = fw;
                    *hgt = fh;
                }
                if pixels.len() > cap {
                    return IRIS_ENGINE_ERROR;
                }
                unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), buf, pixels.len()) };
                IRIS_ENGINE_OK
            }
        }
    })
}

/// A head's scan-out timing, as `HeadTiming`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IrisEngineHeadTiming {
    pub frame_clocks: u64,
    /// When retrace 0 fell in the `iris_engine_advance` time base, ns; -1 where the shell's clock starts the phase.
    pub epoch_ns: i64,
    /// Changes whenever the timing or the phase restarts.
    pub generation: u64,
    pub clock_hz: u32,
    pub fields: u32,
    /// Lines in a frame.
    pub lines: u32,
    pub reserved: u32,
}

/// The timing of head `head`'s scan-out: 1 with `*t` set, 0 for none (the shell keeps its default).
///
/// # Safety
/// `h` is a live handle and `t` points to an `IrisEngineHeadTiming`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_head_timing(h: *mut IrisEngine, head: u32, t: *mut IrisEngineHeadTiming) -> c_int {
    guard(h, Point::Flags, 0, |s| match s.eng.head_timing(head) {
        Some(x) if !t.is_null() => {
            unsafe { *t = IrisEngineHeadTiming { frame_clocks: x.frame_clocks, epoch_ns: x.epoch_ns, generation: x.generation, clock_hz: x.clock_hz, fields: x.fields, lines: x.lines, reserved: 0 } };
            1
        }
        _ => 0,
    })
}

/// Wall time in ns the engine's own worker threads have spent running work since it was built; 0 for an engine
/// with none.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_busy_ns(h: *mut IrisEngine) -> u64 {
    guard(h, Point::Flags, 0, |s| s.eng.busy_ns())
}

/// The bits a colour that head `head`'s hardware output carries (8 unless the board keeps more).
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_head_bits(h: *mut IrisEngine, head: u32) -> c_int {
    guard(h, Point::Flags, 8, |s| s.eng.head_bits(head) as c_int)
}

/// As `iris_engine_scanout`, `x2b10g10r10` a pixel, red in the low bits (the top 8 bits of each colour are the 8-bit scan-out's).
///
/// # Safety
/// As `iris_engine_scanout`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_scanout10(h: *mut IrisEngine, head: u32, buf: *mut u32, cap: usize, w: *mut u32, hgt: *mut u32) -> c_int {
    guard(h, Point::Pixel, IRIS_ENGINE_FAULT, |s| {
        if w.is_null() || hgt.is_null() || (buf.is_null() && cap != 0) {
            return IRIS_ENGINE_ERROR;
        }
        let mut pixels = Vec::new();
        match s.eng.scanout10(head, &mut pixels) {
            Err(f) => {
                set_error(s, &f.0);
                IRIS_ENGINE_FAULT
            }
            Ok(size) => {
                let (fw, fh) = size.unwrap_or((0, 0));
                unsafe {
                    *w = fw;
                    *hgt = fh;
                }
                if pixels.len() > cap {
                    return IRIS_ENGINE_ERROR;
                }
                unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), buf, pixels.len()) };
                IRIS_ENGINE_OK
            }
        }
    })
}

/// When head `head` next needs `iris_engine_advance`, in ns: 1 with `*when` set, 0 for none.
///
/// # Safety
/// `h` is a live handle and `when` points to a `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_head_deadline(h: *mut IrisEngine, head: u32, when: *mut u64) -> c_int {
    guard(h, Point::Flags, 0, |s| match s.eng.head_deadline(head) {
        Some(t) if !when.is_null() => {
            unsafe { *when = t };
            1
        }
        _ => 0,
    })
}

/// A batch of FIFO words into `port`; returns how many were taken.
///
/// # Safety
/// `h` is a live handle and `words` points to `n` words.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_push_words(h: *mut IrisEngine, port: u32, words: *const u32, n: usize) -> usize {
    guard(h, Point::PushWords, 0, |s| {
        if words.is_null() {
            return 0;
        }
        s.eng.push_words(port, unsafe { std::slice::from_raw_parts(words, n) })
    })
}

/// A register read. OK with `*val` set, BUSY while queued work could change
/// the answer, or FAULT.
///
/// # Safety
/// `h` is a live handle and `val` points to a `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_read(h: *mut IrisEngine, off: u32, bits: u32, val: *mut u64) -> c_int {
    guard(h, Point::Read, IRIS_ENGINE_FAULT, |s| match s.eng.read(off, bits) {
        Read::Value(v) => {
            if !val.is_null() {
                unsafe { *val = v };
            }
            IRIS_ENGINE_OK
        }
        Read::Busy => IRIS_ENGINE_BUSY,
        Read::Fault => IRIS_ENGINE_FAULT,
    })
}

/// Name where the stores that follow come from, for flow control.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_set_source(h: *mut IrisEngine, id: u32, charged: u32) -> c_int {
    guard(h, Point::SetSource, IRIS_ENGINE_FAULT, |s| {
        s.eng.set_source(Source { id, charged: charged != 0 });
        IRIS_ENGINE_OK
    })
}

/// Whether a write (`write` non-zero) or read at `off` would answer BUSY now:
/// 1 if it would, 0 if not. A faulted engine answers 0: the call answers FAULT.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_access_waits(h: *mut IrisEngine, off: u32, bits: u32, write: u32) -> c_int {
    guard(h, Point::AccessWaits, 0, |s| s.eng.access_waits(off, bits, write != 0) as c_int)
}

/// Inline mode: run queued work, at most `max_ops`; returns how many ran.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_run(h: *mut IrisEngine, max_ops: usize) -> usize {
    guard(h, Point::Run, 0, |s| s.eng.run(max_ops))
}

/// A frame boundary of the shell's scan-out timing, at `now_ns`.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_frame(h: *mut IrisEngine, now_ns: u64) -> c_int {
    guard(h, Point::Frame, IRIS_ENGINE_FAULT, |s| {
        s.eng.frame(now_ns);
        IRIS_ENGINE_OK
    })
}

/// The pump, at the shell's time `now_ns`. Never waits. FAULT, with `*pump`
/// still filled, if the engine stopped.
///
/// # Safety
/// `h` is a live handle and `pump` is NULL or points to an `IrisEnginePump`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_advance(h: *mut IrisEngine, now_ns: u64, pump: *mut IrisEnginePump) -> c_int {
    guard(h, Point::Advance, IRIS_ENGINE_FAULT, |s| {
        let p = s.eng.advance(now_ns);
        if !pump.is_null() {
            unsafe {
                *pump = IrisEnginePump {
                    idle: idle_state(p.idle),
                    events: p.events as u32,
                    has_deadline: p.next_deadline.is_some() as u32,
                    next_deadline: p.next_deadline.unwrap_or(0),
                    irq_levels: p.irq_levels,
                }
            };
        }
        idle_code(p.idle)
    })
}

/// Wait for the engine's CPU work for at most `budget_ns` of host time. Never
/// waits for the shell's input or the GPU: a stalled engine answers at once
/// with what it needs. FAULT, with `*idle` set, if the engine stopped.
///
/// # Safety
/// `h` is a live handle and `idle` points to an `IrisEngineIdle`.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_wait_idle(h: *mut IrisEngine, now_ns: u64, budget_ns: u64, idle: *mut IrisEngineIdle) -> c_int {
    guard(h, Point::WaitIdle, IRIS_ENGINE_FAULT, |s| {
        let i = s.eng.wait_idle(now_ns, budget_ns);
        if !idle.is_null() {
            unsafe { *idle = idle_state(i) };
        }
        idle_code(i)
    })
}

/// The interrupt output lines, one bit per line.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_irq_levels(h: *mut IrisEngine) -> u64 {
    guard(h, Point::IrqLevels, 0, |s| s.eng.irq_levels())
}

fn text(c: &mut [c_char; 96], s: &str) {
    for (d, b) in c[..95].iter_mut().zip(s.bytes()) {
        *d = b as c_char;
    }
}

fn c_event(e: &Event) -> IrisEngineEvent {
    // SAFETY: all-zero is valid for every member of the union.
    let mut c: IrisEngineEvent = unsafe { std::mem::zeroed() };
    match e {
        Event::Irq => c.kind = IRIS_ENGINE_EVENT_IRQ,
        Event::Damage(m) => {
            c.kind = IRIS_ENGINE_EVENT_DAMAGE;
            c.data.damage = IrisEngineDamage { store: m.store, planes: m.planes, x0: m.rect.x0, y0: m.rect.y0, x1: m.rect.x1, y1: m.rect.y1 };
        }
        Event::Note(t) => {
            c.kind = IRIS_ENGINE_EVENT_NOTE;
            text(unsafe { &mut c.data.text }, t);
        }
        Event::BusFault { addr, write } => {
            c.kind = IRIS_ENGINE_EVENT_BUS_FAULT;
            c.data.bus_fault = IrisEngineBusFault { addr: *addr, write: *write as u32 };
        }
        Event::DeviceFault(t) => {
            c.kind = IRIS_ENGINE_EVENT_DEVICE_FAULT;
            text(unsafe { &mut c.data.text }, t);
        }
        Event::GpuLost(t) => {
            c.kind = IRIS_ENGINE_EVENT_GPU_LOST;
            text(unsafe { &mut c.data.text }, t);
        }
        Event::Interrupt(i) => {
            c.kind = IRIS_ENGINE_EVENT_INTERRUPT;
            c.data.packet = IrisEnginePacket { addr: i.addr, sender: i.sender as u32, widget: i.widget as u32, vector: i.vector as u32, level: i.level as u32 };
        }
        Event::Credits { source, retired } => {
            c.kind = IRIS_ENGINE_EVENT_CREDITS;
            c.data.credits = IrisEngineCredits { source: *source, retired: *retired };
        }
    }
    c
}

/// Move up to `cap` events into `out`, oldest first; returns how many. The
/// rest stay for the next call. Works on a faulted engine.
///
/// # Safety
/// `h` is a live handle and `out` points to `cap` events.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_drain_events(h: *mut IrisEngine, out: *mut IrisEngineEvent, cap: usize) -> usize {
    guard(h, Point::DrainEvents, 0, |s| {
        if out.is_null() {
            return 0;
        }
        let mut evs = Vec::new();
        s.eng.drain_events(&mut evs, cap);
        for (i, e) in evs.iter().enumerate() {
            unsafe { out.add(i).write(c_event(e)) };
        }
        evs.len()
    })
}

fn runs(out: *mut IrisEngineRun, cap: usize, found: &[DirtyRun]) -> usize {
    if out.is_null() {
        return 0;
    }
    let n = found.len().min(cap);
    for (i, r) in found[..n].iter().enumerate() {
        unsafe { out.add(i).write(IrisEngineRun { offset: r.offset, len: r.len }) };
    }
    n
}

/// Move the lent-memory runs the engine wrote since the last call into `out`.
/// Call after `iris_engine_quiesce` too. Runs that do not fit in `cap` stay for
/// the next call: call until it returns less than `cap`.
///
/// # Safety
/// `h` is a live handle and `out` points to `cap` runs.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_drain_dirty(h: *mut IrisEngine, out: *mut IrisEngineRun, cap: usize) -> usize {
    guard(h, Point::DrainDirty, 0, |s| {
        let mut found = std::mem::take(&mut s.dirty);
        s.eng.drain_dirty(&mut found);
        let n = runs(out, cap, &found);
        found.drain(..n);
        s.dirty = found;
        n
    })
}

/// The ranges of lent memory the shell must not map straight into the guest.
///
/// # Safety
/// `h` is a live handle and `out` points to `cap` runs.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_trapped(h: *mut IrisEngine, out: *mut IrisEngineRun, cap: usize) -> usize {
    guard(h, Point::Trapped, 0, |s| {
        let mut found = Vec::new();
        s.eng.trapped(&mut found);
        runs(out, cap, &found)
    })
}

/// Bring a trapped range of lent memory current for a CPU read.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_observe_lent(h: *mut IrisEngine, off: u64, len: u64) -> c_int {
    guard(h, Point::ObserveLent, IRIS_ENGINE_FAULT, |s| match s.eng.observe_lent(off, len) {
        Ok(()) => IRIS_ENGINE_OK,
        Err(e) => {
            set_error(s, &e.0);
            IRIS_ENGINE_FAULT
        }
    })
}

/// The CPU wrote a trapped range of lent memory.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_cpu_wrote(h: *mut IrisEngine, off: u64, len: u64) -> c_int {
    guard(h, Point::CpuWrote, IRIS_ENGINE_FAULT, |s| {
        s.eng.cpu_wrote(off, len);
        IRIS_ENGINE_OK
    })
}

/// Bring all memory current; needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_observe_all(h: *mut IrisEngine) -> c_int {
    guard(h, Point::ObserveAll, IRIS_ENGINE_FAULT, |s| match s.eng.observe_all() {
        Ok(()) => IRIS_ENGINE_OK,
        Err(r) => refused(s, r),
    })
}

/// Run everything queued and park the workers, for at most `budget_ns` of host
/// time (`UINT64_MAX`: no bound). MIDWAY: the engine is stalled on the shell
/// or still busy, and keeps running; see `iris_engine_wait_idle`.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_quiesce(h: *mut IrisEngine, budget_ns: u64) -> c_int {
    guard(h, Point::Quiesce, IRIS_ENGINE_FAULT, |s| match s.eng.quiesce(budget_ns) {
        Ok(()) => IRIS_ENGINE_OK,
        Err(r) => refused(s, r),
    })
}

/// Start the workers again after `iris_engine_quiesce`.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_resume(h: *mut IrisEngine) -> c_int {
    guard(h, Point::Resume, IRIS_ENGINE_FAULT, |s| {
        s.eng.resume();
        IRIS_ENGINE_OK
    })
}

/// Power-on state; clears a fault. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_reset(h: *mut IrisEngine) -> c_int {
    guard(h, Point::Reset, IRIS_ENGINE_FAULT, |s| {
        s.fault = None;
        match s.eng.reset() {
            Ok(()) => IRIS_ENGINE_OK,
            Err(r) => refused(s, r),
        }
    })
}

struct CSink {
    put: unsafe extern "C" fn(user: *mut c_void, name: *const c_char, index: u64, bytes: *const u8, len: usize) -> c_int,
    user: *mut c_void,
    failed: bool,
}

impl StateSink for CSink {
    fn put(&mut self, c: Chunk) -> Result<(), Fault> {
        let name = CString::new(c.name).map_err(|_| Fault("chunk name has a NUL".into()))?;
        // SAFETY: the callback's contract.
        if unsafe { (self.put)(self.user, name.as_ptr(), c.index, c.bytes.as_ptr(), c.bytes.len()) } != 0 {
            self.failed = true;
            return Err(Fault("the state sink refused a chunk".into()));
        }
        Ok(())
    }
}

struct CSource {
    next: unsafe extern "C" fn(user: *mut c_void, chunk: *mut IrisEngineChunk) -> c_int,
    user: *mut c_void,
    failed: bool,
}

impl StateSource for CSource {
    fn next(&mut self) -> Result<Option<Chunk>, Fault> {
        let mut c = IrisEngineChunk { name: [0; 32], index: 0, bytes: std::ptr::null(), len: 0 };
        // SAFETY: the callback's contract.
        let r = unsafe { (self.next)(self.user, &mut c) };
        if r < 0 {
            self.failed = true;
            return Err(Fault("the state source failed".into()));
        }
        if r == 0 {
            return Ok(None);
        }
        if c.name[31] != 0 || (c.len != 0 && c.bytes.is_null()) {
            self.failed = true;
            return Err(Fault("the state source gave a malformed chunk".into()));
        }
        // SAFETY: NUL-terminated by the check; `bytes` valid for `len` bytes.
        let name = unsafe { CStr::from_ptr(c.name.as_ptr()) }.to_string_lossy().into_owned();
        let bytes = if c.len == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(c.bytes, c.len) }.to_vec() };
        Ok(Some(Chunk { name, index: c.index, bytes }))
    }
}

/// Stream the state to `put`, one chunk a call; `put` returns 0, or non-zero to
/// stop (then ERROR). `meta` comes first. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle; `put` and `user` are valid for the call.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_save(
    h: *mut IrisEngine,
    put: Option<unsafe extern "C" fn(user: *mut c_void, name: *const c_char, index: u64, bytes: *const u8, len: usize) -> c_int>,
    user: *mut c_void,
) -> c_int {
    guard(h, Point::Save, IRIS_ENGINE_FAULT, |s| {
        let Some(put) = put else { return IRIS_ENGINE_ERROR };
        let mut sink = CSink { put, user, failed: false };
        match s.eng.save(&mut sink) {
            Ok(()) => IRIS_ENGINE_OK,
            Err(_) if sink.failed => IRIS_ENGINE_ERROR,
            Err(r) => refused(s, r),
        }
    })
}

/// Take a state from `next`, which fills a chunk and returns 1, or 0 at the
/// end, or negative to stop (then ERROR). Chunks are applied as they arrive;
/// after any failure call `iris_engine_reset`. Needs a quiesced engine.
///
/// # Safety
/// `h` is a live handle; `next` and `user` are valid for the call.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_load(
    h: *mut IrisEngine,
    next: Option<unsafe extern "C" fn(user: *mut c_void, chunk: *mut IrisEngineChunk) -> c_int>,
    user: *mut c_void,
) -> c_int {
    guard(h, Point::Load, IRIS_ENGINE_FAULT, |s| {
        let Some(next) = next else { return IRIS_ENGINE_ERROR };
        let mut src = CSource { next, user, failed: false };
        match s.eng.load(&mut src) {
            Ok(()) => IRIS_ENGINE_OK,
            Err(_) if src.failed => IRIS_ENGINE_ERROR,
            Err(r) => refused(s, r),
        }
    })
}

/// The version of the state `iris_engine_save` writes.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_state_version(h: *mut IrisEngine) -> u32 {
    guard(h, Point::StateVersion, 0, |s| s.eng.state_version())
}

/// The device-fault message, or NULL. Valid until the next call on the handle.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_fault(h: *mut IrisEngine) -> *const c_char {
    guard(h, Point::Fault, std::ptr::null(), |s| {
        s.fault = s.eng.fault().and_then(|m| CString::new(m.replace('\0', " ")).ok());
        s.fault.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())
    })
}

/// The reason for the last refusal (NOT_QUIESCED, MIDWAY, MISMATCH, a refusing
/// FAULT or a failed observe), for the shell's error message, or NULL. Valid
/// until the next refusal.
///
/// # Safety
/// `h` is a live handle.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_last_error(h: *mut IrisEngine) -> *const c_char {
    guard(h, Point::LastError, std::ptr::null(), |s| s.error.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
}

static LOG_CB: Mutex<Option<(unsafe extern "C" fn(*mut c_void, c_int, *const c_char), usize)>> = Mutex::new(None);
static LOG_SET: AtomicBool = AtomicBool::new(false);

struct CLogger;

impl log::Log for CLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, r: &log::Record) {
        let Some((cb, user)) = *LOG_CB.lock() else { return };
        let level = match r.level() {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 5,
        };
        if let Ok(msg) = CString::new(r.args().to_string().replace('\0', " ")) {
            // SAFETY: the callback's contract: any thread.
            unsafe { cb(user as *mut c_void, level, msg.as_ptr()) };
        }
    }

    fn flush(&self) {}
}

/// Send the engine's log lines to `cb(user, level, text)`: level 1 error, 2
/// warn, 3 info, 4 debug, 5 trace. `max_level` is the most verbose to pass
/// (0: none). `cb` may be called from any thread and must not call into the
/// engine. ERROR if the process already has another logger.
///
/// # Safety
/// `cb` and `user` stay valid for the life of the process, or until another
/// call replaces them.
#[no_mangle]
pub unsafe extern "C" fn iris_engine_log_set(cb: Option<unsafe extern "C" fn(user: *mut c_void, level: c_int, text: *const c_char)>, user: *mut c_void, max_level: c_int) -> c_int {
    *LOG_CB.lock() = cb.map(|c| (c, user as usize));
    if !LOG_SET.swap(true, Ordering::AcqRel) && log::set_logger(&CLogger).is_err() {
        LOG_SET.store(false, Ordering::Release);
        return IRIS_ENGINE_ERROR;
    }
    log::set_max_level(match max_level {
        i32::MIN..=0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    });
    IRIS_ENGINE_OK
}
