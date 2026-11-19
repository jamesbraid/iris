//! A panic anywhere in the engine becomes a device fault: logged once, the
//! engine stops, every call answers FAULT until a reset. Injected at each
//! `extern "C"` entry, in the worker thread and in inline `run`.
#![cfg(feature = "fault-injection")]

use std::ffi::{c_int, c_void, CStr};
use std::ptr::{null, null_mut};

use iris_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use iris_mgras::dev::mgras::hq3::host;
use iris_mgras::ffi::*;
use iris_mgras::inject::{arm, Point};
use iris_mgras::interface::{BoardEngine, Event, HostMem, MemFault, Read, Swap, Write};
use parking_lot::Mutex;

/// `arm` is global: one test at a time.
static ONE: Mutex<()> = Mutex::new(());

unsafe extern "C" fn ram_read(_: *mut c_void, _: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    unsafe { std::ptr::write_bytes(buf, 0, len) };
    0
}

unsafe extern "C" fn ram_write(_: *mut c_void, _: u64, _: *const u8, _: usize, _: u32) -> c_int {
    0
}

fn new_handle(mode: u32) -> *mut IrisMgras {
    let cfg = IrisMgrasConfig { abi_version: IRIS_MGRAS_ABI_VERSION, board: 1, flags: 0, mode, queue_ops: 0, jit: 0 };
    let ops = IrisMgrasMemOps { ram_read: Some(ram_read), ram_write: Some(ram_write), bus_read: None, bus_write: None };
    let h = unsafe { iris_mgras_new(&cfg, &ops, null_mut(), None, null_mut()) };
    assert!(!h.is_null());
    h
}

fn message(h: *mut IrisMgras) -> String {
    let p = unsafe { iris_mgras_fault(h) };
    assert!(!p.is_null(), "no fault message");
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn region() -> IrisMgrasRegion {
    IrisMgrasRegion { page: 0x240, overlay: 0, drbsize: 0x31e, x0: 0, y0: 0, x1: 3, y1: 3 }
}

/// Every entry, with what it answers when it panics.
#[test]
fn a_panic_at_each_entry_is_a_device_fault() {
    let _g = ONE.lock();
    let r = region();
    let mut word = 0u64;
    let mut pump = IrisMgrasPump::default();
    let mut ev: Vec<IrisMgrasEvent> = Vec::new();
    let mut buf = [0u8; 4];
    let entries: Vec<(Point, Box<dyn Fn(*mut IrisMgras) -> i64 + '_>, i64)> = vec![
        (Point::Write, Box::new(|h| unsafe { iris_mgras_write(h, host::CFIFO, 32, 0) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::PushWords, Box::new(|h| unsafe { iris_mgras_push_words(h, 0, [0u32].as_ptr(), 1) } as i64), 0),
        (Point::Run, Box::new(|h| unsafe { iris_mgras_run(h, 10) } as i64), 0),
        (Point::Flags, Box::new(|h| unsafe { iris_mgras_flags(h) } as i64), 0),
        (Point::AccessWaits, Box::new(|h| unsafe { iris_mgras_access_waits(h, host::RASTER, 32, 0) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Observe, Box::new(|h| unsafe { iris_mgras_observe(h, &r) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::ObserveAll, Box::new(|h| unsafe { iris_mgras_observe_all(h) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Quiesce, Box::new(|h| unsafe { iris_mgras_quiesce(h) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Resume, Box::new(|h| unsafe { iris_mgras_resume(h) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Reset, Box::new(|h| unsafe { iris_mgras_reset(h) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Save, Box::new(|h| unsafe { iris_mgras_save(h) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Load, Box::new(|h| unsafe { iris_mgras_load(h, buf.as_ptr(), 4) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::StateCopy, Box::new(|h| unsafe { iris_mgras_state_copy(h, null_mut(), 0) } as i64), 0),
    ];
    for (point, call, answer) in &entries {
        let h = new_handle(0);
        arm(*point);
        assert_eq!(call(h), *answer, "{point:?}");
        assert!(message(h).contains(&format!("injected panic at {point:?}")), "{point:?}: {}", message(h));
        // The device stays faulted, whatever is called.
        assert_eq!(unsafe { iris_mgras_write(h, host::CFIFO, 32, 0) }, IRIS_MGRAS_FAULT, "{point:?}");
        assert_eq!(unsafe { iris_mgras_read(h, host::STATUS, 32, &mut word) }, IRIS_MGRAS_FAULT, "{point:?}");
        unsafe { iris_mgras_free(h) };
    }
    let more: Vec<(Point, Box<dyn Fn(*mut IrisMgras) -> i64 + '_>, i64)> = vec![
        (Point::Read, Box::new(|h| unsafe { iris_mgras_read(h, host::STATUS, 32, std::ptr::null_mut()) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Advance, Box::new(|h| unsafe { iris_mgras_advance(h, 1, null_mut()) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::WaitIdle, Box::new(|h| unsafe { iris_mgras_wait_idle(h, 1, 1) } as i64), IRIS_MGRAS_FAULT as i64),
        (Point::Pixel, Box::new(|h| unsafe { iris_mgras_pixel(h, &r, 0, 0, &mut 0u64) } as i64), IRIS_MGRAS_FAULT as i64),
    ];
    for (point, call, answer) in &more {
        let h = new_handle(0);
        arm(*point);
        assert_eq!(call(h), *answer, "{point:?}");
        assert!(message(h).contains(&format!("injected panic at {point:?}")), "{point:?}");
        unsafe { iris_mgras_free(h) };
    }
    // The ones that cannot report through their result.
    let h = new_handle(0);
    arm(Point::DrainEvents);
    assert_eq!(unsafe { iris_mgras_drain_events(h, ev.as_mut_ptr(), 0) }, 0);
    assert!(message(h).contains("DrainEvents"));
    unsafe { iris_mgras_free(h) };
    let h = new_handle(0);
    arm(Point::Fault);
    assert!(unsafe { iris_mgras_fault(h) }.is_null());
    assert_eq!(unsafe { iris_mgras_write(h, host::CFIFO, 32, 0) }, IRIS_MGRAS_FAULT);
    assert!(message(h).contains("injected panic at Fault"));
    arm(Point::Free);
    unsafe { iris_mgras_free(h) };
    arm(Point::New);
    let cfg = IrisMgrasConfig { abi_version: IRIS_MGRAS_ABI_VERSION, board: 1, flags: 0, mode: 0, queue_ops: 0, jit: 0 };
    let ops = IrisMgrasMemOps { ram_read: Some(ram_read), ram_write: Some(ram_write), bus_read: None, bus_write: None };
    assert!(unsafe { iris_mgras_new(&cfg, &ops, null_mut(), None, null_mut()) }.is_null());
    let _ = (pump, null::<u8>());
}

/// A device fault ends at a reset.
#[test]
fn a_reset_clears_the_fault() {
    let _g = ONE.lock();
    let h = new_handle(0);
    arm(Point::Write);
    assert_eq!(unsafe { iris_mgras_write(h, host::CFIFO, 32, 0) }, IRIS_MGRAS_FAULT);
    assert_eq!(unsafe { iris_mgras_quiesce(h) }, IRIS_MGRAS_FAULT, "quiesce reports the fault but parks");
    assert_eq!(unsafe { iris_mgras_reset(h) }, IRIS_MGRAS_OK);
    assert!(unsafe { iris_mgras_fault(h) }.is_null());
    assert_eq!(unsafe { iris_mgras_resume(h) }, IRIS_MGRAS_OK);
    assert_eq!(unsafe { iris_mgras_write(h, host::CFIFO, 32, 0) }, IRIS_MGRAS_OK);
    unsafe { iris_mgras_free(h) };
}

#[derive(Default)]
struct Null;

impl HostMem for Null {
    fn ram_read(&self, _: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        out.fill(0);
        Ok(())
    }
    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

/// A panic on the worker thread: the next wait and the next call say so.
#[test]
fn a_dead_worker_is_a_device_fault() {
    let _g = ONE.lock();
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Threaded), Null, None, None).unwrap();
    arm(Point::Exec);
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Done);
    assert!(!e.wait_idle(0, 10_000_000_000));
    assert!(e.fault().is_some_and(|f| f.contains("injected panic at Exec")));
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Fault);
    assert_eq!(e.read(host::SET_FLAGS, 32), Read::Fault);
    let mut ev = Vec::new();
    e.drain_events(&mut ev);
    assert!(ev.iter().any(|x| matches!(x, Event::DeviceFault(m) if m.contains("injected panic at Exec"))), "{ev:?}");
    assert!(e.quiesce().is_err());
    e.reset().unwrap();
    e.resume();
    assert_eq!(e.fault(), None);
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Done);
    assert!(e.wait_idle(0, 10_000_000_000));
}

/// The same on `run`, and in a read that executes engine code.
#[test]
fn a_panic_in_run_or_read_is_a_device_fault() {
    let _g = ONE.lock();
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Null, None, None).unwrap();
    arm(Point::Exec);
    e.write(host::CFIFO, 32, 0);
    e.run(usize::MAX);
    assert!(e.fault().is_some_and(|f| f.contains("injected panic at Exec")));
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Fault);
    e.quiesce().unwrap_err();
    e.reset().unwrap();
    e.resume();
    arm(Point::Read);
    assert_eq!(e.read(host::RASTER, 32), Read::Fault);
    assert!(e.fault().is_some_and(|f| f.contains("injected panic at Read")));
}
