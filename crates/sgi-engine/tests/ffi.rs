//! The C ABI, driven the way a C shell would, over the mock board: every
//! entry, the event union, streamed state, refusal reasons, faults, panics,
//! re-entry and a handle that changes threads.

mod support;

use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use parking_lot::Mutex;
use sgi_engine::ffi::*;
use sgi_engine::inject::{arm, Point};
use sgi_engine::mock::{reg, Mock, MockConfig, STATE_VERSION};
use sgi_engine::{HostMem, Swap};
use support::*;

/// `arm` is global: one test at a time that uses it.
static ONE: Mutex<()> = Mutex::new(());

/// What the callbacks' `user` points to.
struct User {
    ram: Ram,
    h: AtomicPtr<IrisEngine>,
    reenter: AtomicU32,
    answers: Mutex<Vec<c_int>>,
    notified: AtomicU32,
}

unsafe extern "C" fn ram_read(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    let u = unsafe { &*(user as *const User) };
    let mut tmp = vec![0u8; len];
    if u.ram.ram_read(addr, &mut tmp, Swap::None).is_err() {
        return 1;
    }
    unsafe { std::ptr::copy_nonoverlapping(tmp.as_ptr(), buf, len) };
    if u.reenter.load(Ordering::SeqCst) != 0 {
        // A DMA at the device's own window: what the shell must not let happen.
        let h = u.h.load(Ordering::SeqCst);
        let mut v = 0u64;
        let mut a = u.answers.lock();
        a.push(unsafe { iris_engine_write(h, reg::DATA, 32, 1) });
        a.push(unsafe { iris_engine_read(h, reg::IRQ, 32, &mut v) });
        a.push(unsafe { iris_engine_run(h, 1) } as c_int);
    }
    0
}

unsafe extern "C" fn ram_write(user: *mut c_void, addr: u64, buf: *const u8, len: usize, _: u32) -> c_int {
    let u = unsafe { &*(user as *const User) };
    u.ram.ram_write(addr, unsafe { std::slice::from_raw_parts(buf, len) }, Swap::None).map_or(1, |_| 0)
}

unsafe extern "C" fn notify(user: *mut c_void) {
    unsafe { &*(user as *const User) }.notified.fetch_add(1, Ordering::SeqCst);
}

/// A board's configuration: the shared part and a field of its own.
#[repr(C)]
#[derive(Clone, Copy)]
struct Cfg {
    common: IrisEngineConfig,
    extra: u32,
}

fn config(gpu: bool, lent: Option<(*mut u8, u64)>) -> Cfg {
    let (lent_ptr, lent_len) = lent.unwrap_or((null_mut(), 0));
    Cfg {
        common: IrisEngineConfig {
            abi_version: IRIS_ENGINE_ABI_VERSION,
            struct_size: std::mem::size_of::<Cfg>() as u32,
            mode: 0,
            queue_ops: 0,
            gpu: gpu as u32,
            gpu_backend: null(),
            lent_ptr,
            lent_len,
            reserved: [0; 4],
        },
        extra: 5,
    }
}

fn host(user: &User) -> IrisEngineHost {
    IrisEngineHost {
        abi_version: IRIS_ENGINE_ABI_VERSION,
        struct_size: std::mem::size_of::<IrisEngineHost>() as u32,
        user: user as *const User as *mut c_void,
        ram_read: Some(ram_read),
        ram_write: Some(ram_write),
        bus_read: None,
        bus_write: None,
        notify: Some(notify),
    }
}

/// What a board's `new` does with the shared pieces.
unsafe fn new(cfg: *const Cfg, host: *const IrisEngineHost) -> *mut IrisEngine {
    build(|| {
        let c = unsafe { read_config(cfg) }?;
        let (mem, notify) = unsafe { host_parts(host) }?;
        let queue_ops = if c.common.queue_ops == 0 { 64 } else { c.common.queue_ops as usize };
        Some(Mock::new(MockConfig { queue_ops, lent: unsafe { c.common.lent() }, gpu_unit: c.common.gpu != 0 }, mem, notify))
    })
}

struct Fixture {
    h: *mut IrisEngine,
    user: Box<User>,
    lent: Vec<u64>,
}

impl Fixture {
    fn new(gpu: bool) -> Fixture {
        let mut lent = vec![0u64; 8192];
        let user = Box::new(User { ram: Ram::new(), h: AtomicPtr::new(null_mut()), reenter: AtomicU32::new(0), answers: Mutex::new(Vec::new()), notified: AtomicU32::new(0) });
        let cfg = config(gpu, Some((lent.as_mut_ptr().cast(), 8192 * 8)));
        let h = unsafe { new(&cfg, &host(&user)) };
        assert!(!h.is_null());
        user.h.store(h, Ordering::SeqCst);
        Fixture { h, user, lent }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe { iris_engine_free(self.h) };
    }
}

fn events(h: *mut IrisEngine, cap: usize) -> Vec<IrisEngineEvent> {
    let mut buf: Vec<IrisEngineEvent> = Vec::with_capacity(cap);
    let n = unsafe { iris_engine_drain_events(h, buf.as_mut_ptr(), cap) };
    unsafe { buf.set_len(n) };
    buf
}

fn text(c: *const c_char) -> String {
    assert!(!c.is_null());
    unsafe { CStr::from_ptr(c) }.to_string_lossy().into_owned()
}

fn wr(h: *mut IrisEngine, off: u32, val: u64) -> c_int {
    unsafe { iris_engine_write(h, off, 32, val) }
}

fn rd(h: *mut IrisEngine, off: u32) -> (c_int, u64) {
    let mut v = 0;
    (unsafe { iris_engine_read(h, off, 32, &mut v) }, v)
}

fn idle(h: *mut IrisEngine, now: u64) -> (c_int, IrisEngineIdle) {
    let mut i = IrisEngineIdle::default();
    (unsafe { iris_engine_wait_idle(h, now, 0, &mut i) }, i)
}

#[test]
fn a_c_shell_drives_the_interface_end_to_end() {
    let f = Fixture::new(true);
    let h = f.h;
    assert_eq!(iris_engine_contains_panics(), 1);
    assert!(unsafe { iris_engine_fault(h) }.is_null() && unsafe { iris_engine_last_error(h) }.is_null());

    // Interrupt levels, from atomics, and the event.
    assert_eq!(wr(h, reg::IRQ_SET, 6), IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_irq_levels(h) }, 6);
    assert_eq!(events(h, 8).iter().map(|e| e.kind).collect::<Vec<_>>(), [IRIS_ENGINE_EVENT_IRQ]);
    assert!(f.user.notified.load(Ordering::SeqCst) >= 1, "the notify hook ran");

    // Credits and packets through the event union.
    assert_eq!(unsafe { iris_engine_set_source(h, 7, 1) }, IRIS_ENGINE_OK);
    let words = [1u32; 5];
    assert_eq!(unsafe { iris_engine_push_words(h, 0, words.as_ptr(), words.len()) }, 5);
    assert_eq!(wr(h, reg::PACKET, 9), IRIS_ENGINE_OK);
    assert_eq!(rd(h, reg::SUM).0, IRIS_ENGINE_BUSY);
    assert_eq!(unsafe { iris_engine_access_waits(h, reg::SUM, 32, 0) }, 1);
    assert_eq!(unsafe { iris_engine_run(h, usize::MAX) }, 6);
    assert_eq!(rd(h, reg::SUM), (IRIS_ENGINE_OK, 5));
    let ev = events(h, 1);
    assert_eq!(ev.len(), 1, "drained a piece at a time");
    let ev2 = events(h, 8);
    let credits = unsafe { ev[0].data.credits };
    assert_eq!((ev[0].kind, credits.source, credits.retired), (IRIS_ENGINE_EVENT_CREDITS, 7, 5));
    let p = unsafe { ev2[0].data.packet };
    assert_eq!((ev2[0].kind, p.widget, p.vector, p.level, p.addr), (IRIS_ENGINE_EVENT_INTERRUPT, 8, 9, 1, 0x1000));

    // A stall is a state, not a wait; a frame releases it.
    wr(h, reg::SWAP, 0);
    wr(h, reg::DATA, 100);
    let (code, i) = idle(h, 1);
    assert_eq!((code, i.state), (IRIS_ENGINE_OK, IRIS_ENGINE_STALLED_FRAME));
    assert_eq!(unsafe { iris_engine_quiesce(h, 1_000) }, IRIS_ENGINE_MIDWAY);
    assert!(text(unsafe { iris_engine_last_error(h) }).contains("frame"));
    assert_eq!(unsafe { iris_engine_save(h, Some(put_none), null_mut()) }, IRIS_ENGINE_NOT_QUIESCED);
    assert_eq!(text(unsafe { iris_engine_last_error(h) }), "the engine is not quiesced");
    assert_eq!(unsafe { iris_engine_frame(h, 2) }, IRIS_ENGINE_OK);
    wr(h, reg::WAIT, 900);
    wr(h, reg::DATA, 1000);
    let (_, i) = idle(h, 3);
    assert_eq!((i.state, i.stall_time), (IRIS_ENGINE_STALLED_TIME, 900));
    let mut pump = IrisEnginePump::default();
    assert_eq!(unsafe { iris_engine_advance(h, 900, &mut pump) }, IRIS_ENGINE_OK);
    assert_eq!((pump.idle.state, pump.irq_levels, pump.has_deadline), (IRIS_ENGINE_WORKING, 6, 0));
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    assert_eq!(rd(h, reg::SUM), (IRIS_ENGINE_OK, 1105));
    assert_eq!(unsafe { iris_engine_observe_all(h) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_state_version(h) }, STATE_VERSION);
    assert_eq!(unsafe { iris_engine_resume(h) }, IRIS_ENGINE_OK);
}

extern "C" fn put_none(_: *mut c_void, _: *const c_char, _: u64, _: *const u8, _: usize) -> c_int {
    0
}

#[test]
fn lent_memory_through_the_c_entries() {
    let mut f = Fixture::new(true);
    let h = f.h;
    let mut runs = [IrisEngineRun::default(); 4];
    let n = unsafe { iris_engine_trapped(h, runs.as_mut_ptr(), 4) };
    assert_eq!((n, runs[0].offset, runs[0].len), (1, 0, 32 * 1024));
    f.lent[5] = 100;
    assert_eq!(unsafe { iris_engine_cpu_wrote(h, 40, 8) }, IRIS_ENGINE_OK);
    wr(h, reg::LENT_ADD + 40, 23);
    wr(h, reg::LENT_STORE + 5000 * 8, 9);
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_observe_lent(h, 40, 8) }, IRIS_ENGINE_OK);
    assert_eq!((f.lent[5], f.lent[5000]), (123, 9));
    // Two words in two blocks, so two runs, the second at the untrapped store.
    let n = unsafe { iris_engine_drain_dirty(h, runs.as_mut_ptr(), 4) };
    assert_eq!(n, 2);
    assert_eq!((runs[0].offset, runs[1].offset), (0, 32 * 1024));
    assert_eq!(unsafe { iris_engine_drain_dirty(h, runs.as_mut_ptr(), 4) }, 0);
}

/// A cap smaller than the runs found must not lose the rest.
#[test]
fn drain_dirty_with_a_short_cap_keeps_the_rest_for_the_next_call() {
    let mut f = Fixture::new(true);
    let h = f.h;
    f.lent[5] = 100;
    assert_eq!(unsafe { iris_engine_cpu_wrote(h, 40, 8) }, IRIS_ENGINE_OK);
    wr(h, reg::LENT_ADD + 40, 23);
    wr(h, reg::LENT_STORE + 5000 * 8, 9);
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_observe_lent(h, 40, 8) }, IRIS_ENGINE_OK);
    let mut run = [IrisEngineRun::default(); 1];
    let got: Vec<(usize, u64)> = (0..3).map(|_| (unsafe { iris_engine_drain_dirty(h, run.as_mut_ptr(), 1) }, run[0].offset)).collect();
    assert_eq!(got, [(1, 0), (1, 32 * 1024), (0, 32 * 1024)]);
}

#[test]
fn state_streams_through_callbacks_and_refusals_carry_their_reason() {
    let f = Fixture::new(true);
    let h = f.h;
    for w in [3u32, 600, 1100] {
        wr(h, reg::LENT_STORE + w * 8, 1);
    }
    wr(h, reg::DATA, 42);
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);

    struct Sink(Vec<(String, u64, Vec<u8>)>);
    unsafe extern "C" fn put(user: *mut c_void, name: *const c_char, index: u64, bytes: *const u8, len: usize) -> c_int {
        let s = unsafe { &mut *(user as *mut Sink) };
        s.0.push((text(name), index, unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()));
        0
    }
    let mut sink = Sink(Vec::new());
    assert_eq!(unsafe { iris_engine_save(h, Some(put), &mut sink as *mut Sink as *mut c_void) }, IRIS_ENGINE_OK);
    let names: Vec<(&str, u64)> = sink.0.iter().map(|c| (c.0.as_str(), c.1)).collect();
    assert_eq!(names, [("meta", 0), ("shadow", 0), ("shadow", 1), ("shadow", 2)]);
    assert_eq!(sink.0[1].2.len(), 4096, "a piece, not the whole store");

    struct Src(std::vec::IntoIter<(String, u64, Vec<u8>)>, Vec<u8>);
    unsafe extern "C" fn next(user: *mut c_void, c: *mut IrisEngineChunk) -> c_int {
        let s = unsafe { &mut *(user as *mut Src) };
        let Some((name, index, bytes)) = s.0.next() else { return 0 };
        s.1 = bytes;
        let c = unsafe { &mut *c };
        for (d, b) in c.name.iter_mut().zip(name.bytes().chain([0])) {
            *d = b as c_char;
        }
        (c.index, c.bytes, c.len) = (index, s.1.as_ptr(), s.1.len());
        1
    }
    let load = |h: *mut IrisEngine, chunks: Vec<(String, u64, Vec<u8>)>| {
        let mut src = Src(chunks.into_iter(), Vec::new());
        unsafe { iris_engine_load(h, Some(next), &mut src as *mut Src as *mut c_void) }
    };
    assert_eq!(unsafe { iris_engine_reset(h) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    assert_eq!(load(h, sink.0.clone()), IRIS_ENGINE_OK);
    assert_eq!(rd(h, reg::SUM), (IRIS_ENGINE_OK, 42));

    // Each way a load can fail answers its own code and says why.
    let mut old = sink.0.clone();
    old[0].2[0] ^= 1;
    assert_eq!(load(h, old), IRIS_ENGINE_MISMATCH);
    assert!(text(unsafe { iris_engine_last_error(h) }).contains("version"));
    assert_eq!(load(h, vec![]), IRIS_ENGINE_MISMATCH);
    assert_eq!(unsafe { iris_engine_load(h, None, null_mut()) }, IRIS_ENGINE_ERROR);
    unsafe extern "C" fn broken(_: *mut c_void, _: *mut IrisEngineChunk) -> c_int {
        -1
    }
    assert_eq!(unsafe { iris_engine_load(h, Some(broken), null_mut()) }, IRIS_ENGINE_ERROR, "the shell's own failure is not a device fault");
    unsafe extern "C" fn unterminated(_: *mut c_void, c: *mut IrisEngineChunk) -> c_int {
        unsafe { (*c).name = [b'x' as c_char; 32] };
        1
    }
    assert_eq!(unsafe { iris_engine_load(h, Some(unterminated), null_mut()) }, IRIS_ENGINE_ERROR);
    unsafe extern "C" fn refuse(_: *mut c_void, _: *const c_char, _: u64, _: *const u8, _: usize) -> c_int {
        1
    }
    assert_eq!(unsafe { iris_engine_save(h, Some(refuse), null_mut()) }, IRIS_ENGINE_ERROR, "a sink that stops is not a device fault");
    assert!(unsafe { iris_engine_fault(h) }.is_null());
    // Midway: a swap is held.
    wr(h, reg::SWAP, 0);
    assert_eq!(unsafe { iris_engine_resume(h) }, IRIS_ENGINE_OK);
    unsafe { iris_engine_run(h, 10) };
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_save(h, Some(put_none), null_mut()) }, IRIS_ENGINE_MIDWAY);
}

#[test]
fn a_caller_older_than_the_library_reads_new_fields_as_zero() {
    let user = User { ram: Ram::new(), h: AtomicPtr::new(null_mut()), reenter: AtomicU32::new(0), answers: Mutex::new(Vec::new()), notified: AtomicU32::new(0) };
    let mut cfg = config(true, None);
    // A caller that predates the board's `extra` field and the host's notify hook.
    cfg.common.struct_size = std::mem::size_of::<IrisEngineConfig>() as u32;
    let got = unsafe { read_config(&cfg) }.unwrap();
    assert_eq!((got.extra, got.common.gpu), (0, 1));
    let mut hs = host(&user);
    hs.struct_size = (std::mem::size_of::<IrisEngineHost>() - 8) as u32;
    let h = unsafe { new(&cfg, &hs) };
    assert!(!h.is_null(), "a caller without the notify hook still gets an engine");
    unsafe { iris_engine_free(h) };
    // A caller with a newer config reads as far as it knows.
    cfg.common.struct_size = 4096;
    assert!(unsafe { read_config(&cfg) }.is_some());
    cfg.common.abi_version = 2;
    assert!(unsafe { read_config(&cfg) }.is_none(), "another ABI version");
    assert!(unsafe { new(null(), &host(&user)) }.is_null());
    assert!(unsafe { read_config::<Cfg>(null()) }.is_none());
}

/// Every entry, with what it answers when it panics.
#[test]
fn a_panic_at_each_entry_is_a_device_fault() {
    let _g = ONE.lock();
    let mut v = 0u64;
    let mut pump = IrisEnginePump::default();
    let mut i = IrisEngineIdle::default();
    let mut runs = [IrisEngineRun::default(); 2];
    let mut trapped = [IrisEngineRun::default(); 2];
    let mut evs: Vec<IrisEngineEvent> = Vec::with_capacity(2);
    let entries: Vec<(Point, Box<dyn FnMut(*mut IrisEngine) -> i64 + '_>, i64)> = vec![
        (Point::Write, Box::new(|h| wr(h, reg::DATA, 1) as i64), IRIS_ENGINE_FAULT as i64),
        (Point::PushWords, Box::new(|h| unsafe { iris_engine_push_words(h, 0, [0u32].as_ptr(), 1) } as i64), 0),
        (Point::Read, Box::new(|h| unsafe { iris_engine_read(h, reg::IRQ, 32, &mut v) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::SetSource, Box::new(|h| unsafe { iris_engine_set_source(h, 1, 1) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::AccessWaits, Box::new(|h| unsafe { iris_engine_access_waits(h, reg::SUM, 32, 0) } as i64), 0),
        (Point::Run, Box::new(|h| unsafe { iris_engine_run(h, 10) } as i64), 0),
        (Point::Frame, Box::new(|h| unsafe { iris_engine_frame(h, 1) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Advance, Box::new(|h| unsafe { iris_engine_advance(h, 1, &mut pump) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::WaitIdle, Box::new(|h| unsafe { iris_engine_wait_idle(h, 1, 1, &mut i) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::IrqLevels, Box::new(|h| unsafe { iris_engine_irq_levels(h) } as i64), 0),
        (Point::DrainEvents, Box::new(|h| unsafe { iris_engine_drain_events(h, evs.as_mut_ptr(), 2) } as i64), 0),
        (Point::DrainDirty, Box::new(|h| unsafe { iris_engine_drain_dirty(h, runs.as_mut_ptr(), 2) } as i64), 0),
        (Point::Trapped, Box::new(|h| unsafe { iris_engine_trapped(h, trapped.as_mut_ptr(), 2) } as i64), 0),
        (Point::ObserveLent, Box::new(|h| unsafe { iris_engine_observe_lent(h, 0, 8) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::CpuWrote, Box::new(|h| unsafe { iris_engine_cpu_wrote(h, 0, 8) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::ObserveAll, Box::new(|h| unsafe { iris_engine_observe_all(h) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Quiesce, Box::new(|h| unsafe { iris_engine_quiesce(h, 1) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Resume, Box::new(|h| unsafe { iris_engine_resume(h) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Save, Box::new(|h| unsafe { iris_engine_save(h, Some(put_none), null_mut()) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Load, Box::new(|h| unsafe { iris_engine_load(h, None, null_mut()) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::StateVersion, Box::new(|h| unsafe { iris_engine_state_version(h) } as i64), 0),
        (Point::Reset, Box::new(|h| unsafe { iris_engine_reset(h) } as i64), IRIS_ENGINE_FAULT as i64),
    ];
    for (point, mut call, answer) in entries {
        let f = Fixture::new(false);
        arm(point);
        assert_eq!(call(f.h), answer, "{point:?}");
        assert!(text(unsafe { iris_engine_fault(f.h) }).contains(&format!("injected panic at {point:?}")), "{point:?}");
        assert_eq!(wr(f.h, reg::DATA, 1), IRIS_ENGINE_FAULT, "{point:?}: the device stays faulted");
    }
    // The ones that report through something else.
    let f = Fixture::new(false);
    arm(Point::Fault);
    assert!(unsafe { iris_engine_fault(f.h) }.is_null());
    assert!(text(unsafe { iris_engine_fault(f.h) }).contains("injected panic at Fault"));
    arm(Point::LastError);
    assert!(unsafe { iris_engine_last_error(f.h) }.is_null());
    assert!(text(unsafe { iris_engine_fault(f.h) }).contains("injected panic at Fault"), "the first fault stays");
    let spare = Fixture::new(false);
    let h = spare.h;
    std::mem::forget(spare);
    arm(Point::Free);
    unsafe { iris_engine_free(h) };
    let cfg = config(false, None);
    let user = User { ram: Ram::new(), h: AtomicPtr::new(null_mut()), reenter: AtomicU32::new(0), answers: Mutex::new(Vec::new()), notified: AtomicU32::new(0) };
    arm(Point::New);
    assert!(unsafe { new(&cfg, &host(&user)) }.is_null());
    let h = unsafe { new(&cfg, &host(&user)) };
    assert!(!h.is_null(), "armed once");
    unsafe { iris_engine_free(h) };
}

#[test]
fn a_panic_inside_the_engine_is_caught_at_the_entry_and_reset_recovers() {
    let _g = ONE.lock();
    let f = Fixture::new(false);
    let h = f.h;
    wr(h, reg::BOOM, 0);
    assert_eq!(unsafe { iris_engine_run(h, 10) }, 0, "a panicking call answers as a fault does");
    assert!(text(unsafe { iris_engine_fault(h) }).contains("panic in Run: the mock board was told to"));
    let ev = events(h, 4);
    assert!(ev.iter().any(|e| e.kind == IRIS_ENGINE_EVENT_DEVICE_FAULT), "the shell can see why");
    // Every entry now answers as the trait says.
    assert_eq!(wr(h, reg::DATA, 1), IRIS_ENGINE_FAULT);
    assert_eq!(rd(h, reg::IRQ).0, IRIS_ENGINE_FAULT);
    assert_eq!(unsafe { iris_engine_push_words(h, 0, [1u32].as_ptr(), 1) }, 0);
    assert_eq!(unsafe { iris_engine_access_waits(h, reg::SUM, 32, 0) }, 0);
    let mut pump = IrisEnginePump::default();
    assert_eq!(unsafe { iris_engine_advance(h, 1, &mut pump) }, IRIS_ENGINE_FAULT);
    assert_eq!(pump.idle.state, IRIS_ENGINE_FAULTED);
    let (code, i) = idle(h, 1);
    assert_eq!((code, i.state), (IRIS_ENGINE_FAULT, IRIS_ENGINE_FAULTED));
    assert_eq!(unsafe { iris_engine_observe_all(h) }, IRIS_ENGINE_FAULT);
    assert_eq!(unsafe { iris_engine_save(h, Some(put_none), null_mut()) }, IRIS_ENGINE_FAULT);
    assert_eq!(unsafe { iris_engine_quiesce(h, 1) }, IRIS_ENGINE_FAULT, "reports the fault, still parks");
    assert!(text(unsafe { iris_engine_last_error(h) }).contains("told to"));
    assert_eq!(unsafe { iris_engine_reset(h) }, IRIS_ENGINE_OK);
    assert!(unsafe { iris_engine_fault(h) }.is_null());
    assert_eq!(wr(h, reg::DATA, 1), IRIS_ENGINE_OK);
}

#[test]
fn a_call_that_comes_in_during_a_call_is_refused_and_leaves_the_engine_alone() {
    let f = Fixture::new(false);
    let h = f.h;
    static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn log_cb(_: *mut c_void, level: c_int, t: *const c_char) {
        if level == 1 {
            LOGS.lock().push(text(t));
        }
    }
    assert_eq!(unsafe { iris_engine_log_set(Some(log_cb), null_mut(), 5) }, IRIS_ENGINE_OK);
    f.user.ram.put(8, 0xABCD);
    f.user.reenter.store(1, Ordering::SeqCst);
    wr(h, reg::DMA_READ, 8);
    assert_eq!(unsafe { iris_engine_run(h, 10) }, 1, "the outer call finished");
    f.user.reenter.store(0, Ordering::SeqCst);
    assert_eq!(*f.user.answers.lock(), [IRIS_ENGINE_FAULT, IRIS_ENGINE_FAULT, 0], "write, read and run came back as refused");
    assert!(unsafe { iris_engine_fault(h) }.is_null(), "the device is not faulted by it");
    assert_eq!(rd(h, reg::DMA_RESULT), (IRIS_ENGINE_OK, 0xABCD));
    assert_eq!(rd(h, reg::SUM), (IRIS_ENGINE_OK, 0), "the refused write queued nothing");
    assert!(LOGS.lock().iter().any(|l| l.contains("another was running")), "{:?}", LOGS.lock());
}

#[test]
fn a_handle_moves_between_threads_one_call_at_a_time() {
    struct H(*mut IrisEngine);
    // SAFETY: calls come one at a time; that is the contract being tested.
    unsafe impl Send for H {}
    let f = Fixture::new(false);
    let h = H(f.h);
    let t = std::thread::spawn(move || {
        let h = h;
        assert_eq!(wr(h.0, reg::DATA, 3), IRIS_ENGINE_OK);
        assert_eq!(unsafe { iris_engine_run(h.0, 10) }, 1);
    });
    t.join().unwrap();
    assert_eq!(rd(f.h, reg::SUM), (IRIS_ENGINE_OK, 3));
}

#[test]
fn bad_arguments_answer_without_touching_the_engine() {
    let f = Fixture::new(false);
    let h = f.h;
    assert_eq!(unsafe { iris_engine_write(null_mut(), 0, 32, 0) }, IRIS_ENGINE_FAULT);
    assert_eq!(unsafe { iris_engine_push_words(h, 0, null(), 4) }, 0);
    assert_eq!(unsafe { iris_engine_drain_events(h, null_mut(), 4) }, 0);
    assert_eq!(unsafe { iris_engine_save(h, None, null_mut()) }, IRIS_ENGINE_ERROR);
    assert_eq!(unsafe { iris_engine_read(h, reg::IRQ, 32, null_mut()) }, IRIS_ENGINE_OK);
    unsafe { iris_engine_free(null_mut()) };
}

#[test]
fn a_c_shell_takes_a_timed_write_a_head_s_picture_and_a_stopped_stream() {
    let f = Fixture::new(false);
    let h = f.h;
    assert_eq!(unsafe { iris_engine_write_at(h, 77, reg::DATA, 32, 3) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_heads(h) }, 2);
    let (mut w, mut hgt, mut buf) = (0u32, 0u32, [0u32; 8]);
    assert_eq!(unsafe { iris_engine_scanout(h, 1, buf.as_mut_ptr(), buf.len(), &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!((w, hgt, buf[0] >> 16), (4, 2, 2));
    assert_eq!(unsafe { iris_engine_scanout(h, 1, buf.as_mut_ptr(), 3, &mut w, &mut hgt) }, IRIS_ENGINE_ERROR, "too small: the size comes back");
    assert_eq!((w, hgt), (4, 2));
    assert_eq!(unsafe { iris_engine_scanout(h, 5, buf.as_mut_ptr(), buf.len(), &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!((w, hgt), (0, 0), "no such head: no frame");
    let mut when = 0u64;
    assert_eq!((unsafe { iris_engine_head_deadline(h, 1, &mut when) }, when), (1, 2_000));
    assert_eq!(unsafe { iris_engine_head_deadline(h, 2, &mut when) }, 0);
    assert_eq!(wr(h, reg::STOP, 0), IRIS_ENGINE_OK);
    let (code, i) = idle(h, 1);
    assert_eq!((code, i.state), (IRIS_ENGINE_OK, IRIS_ENGINE_STOPPED), "stopped is not a fault");
}

#[test]
fn a_c_shell_reads_a_heads_timing_and_its_ten_bit_picture() {
    let f = Fixture::new(false);
    let h = f.h;
    let mut t = IrisEngineHeadTiming::default();
    assert_eq!(unsafe { iris_engine_head_timing(h, 0, &mut t) }, 0, "none for head 0");
    assert_eq!(unsafe { iris_engine_head_timing(h, 1, &mut t) }, 1);
    assert_eq!((t.frame_clocks, t.clock_hz, t.fields, t.lines, t.epoch_ns, t.generation), (1_000_000, 60_000_000, 2, 525, -1, 3));
    let (mut w, mut hgt, mut eight, mut ten) = (0u32, 0u32, [0u32; 8], [0u32; 8]);
    assert_eq!(unsafe { iris_engine_scanout(h, 1, eight.as_mut_ptr(), 8, &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_scanout10(h, 1, ten.as_mut_ptr(), 8, &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!((w, hgt), (4, 2));
    for (a, b) in eight.iter().zip(&ten) {
        assert_eq!((b >> 22, b >> 12 & 0xff, b >> 2 & 0xff), (a >> 16 & 0xff, a >> 8 & 0xff, a & 0xff));
    }
    assert_eq!(unsafe { iris_engine_scanout10(h, 1, ten.as_mut_ptr(), 3, &mut w, &mut hgt) }, IRIS_ENGINE_ERROR, "too small: the size comes back");
}
