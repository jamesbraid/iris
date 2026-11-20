//! A panic anywhere in the engine becomes a device fault: logged once, the
//! engine stops, every call answers as a faulted engine does until a reset.
//! The shared entries are tested in `sgi-engine`; here the board's entries,
//! the worker thread, inline `run` and a read.
#![cfg(feature = "fault-injection")]

use std::ffi::{c_int, c_void, CStr};
use std::ptr::null_mut;

use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;
use sgi_mgras::ffi::*;
use parking_lot::Mutex;
use sgi_engine::ffi::*;
use sgi_engine::inject::{arm, Point};
use sgi_engine::{BoardEngine, Event, HostMem, Idle, MemFault, Read, Refused, Swap, Write};

/// `arm` is global: one test at a time.
static ONE: Mutex<()> = Mutex::new(());

unsafe extern "C" fn ram_read(_: *mut c_void, _: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    unsafe { std::ptr::write_bytes(buf, 0, len) };
    0
}

unsafe extern "C" fn ram_write(_: *mut c_void, _: u64, _: *const u8, _: usize, _: u32) -> c_int {
    0
}

fn config(mode: u32) -> IrisMgrasConfig {
    IrisMgrasConfig {
        common: IrisEngineConfig {
            abi_version: IRIS_ENGINE_ABI_VERSION,
            struct_size: std::mem::size_of::<IrisMgrasConfig>() as u32,
            mode,
            queue_ops: 0,
            gpu: 0,
            gpu_backend: std::ptr::null(),
            lent_ptr: null_mut(),
            lent_len: 0,
            reserved: [0; 4],
        },
        board: 1,
        flags: 0,
        jit: 0,
        bdvers: [0; 2],
        cmap_rev: [0; 2],
        ge_revision: 0,
    }
}

fn host_cb() -> IrisEngineHost {
    IrisEngineHost {
        abi_version: IRIS_ENGINE_ABI_VERSION,
        struct_size: std::mem::size_of::<IrisEngineHost>() as u32,
        user: null_mut(),
        ram_read: Some(ram_read),
        ram_write: Some(ram_write),
        bus_read: None,
        bus_write: None,
        notify: None,
    }
}

fn new_handle() -> *mut IrisEngine {
    let h = unsafe { sgi_mgras_new(&config(0), &host_cb()) };
    assert!(!h.is_null());
    h
}

fn message(h: *mut IrisEngine) -> String {
    let p = unsafe { iris_engine_fault(h) };
    assert!(!p.is_null(), "no fault message");
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn region() -> IrisMgrasRegion {
    IrisMgrasRegion { page: 0x240, overlay: 0, drbsize: 0x31e, x0: 0, y0: 0, x1: 3, y1: 3 }
}

/// The board's own entries, and its constructor.
#[test]
fn a_panic_in_a_board_entry_is_a_device_fault() {
    let _g = ONE.lock();
    let r = region();
    let mut word = 0u64;
    let entries: Vec<(Point, Box<dyn FnMut(*mut IrisEngine) -> i64 + '_>, i64)> = vec![
        (Point::Observe, Box::new(|h| unsafe { sgi_mgras_observe(h, &r) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Pixel, Box::new(|h| unsafe { sgi_mgras_pixel(h, &r, 0, 0, &mut word) } as i64), IRIS_ENGINE_FAULT as i64),
        (Point::Flags, Box::new(|h| unsafe { sgi_mgras_flags(h) } as i64), 0),
    ];
    for (point, mut call, answer) in entries {
        let h = new_handle();
        arm(point);
        assert_eq!(call(h), answer, "{point:?}");
        assert!(message(h).contains(&format!("injected panic at {point:?}")), "{point:?}");
        assert_eq!(unsafe { iris_engine_write(h, host::CFIFO, 32, 0) }, IRIS_ENGINE_FAULT, "{point:?}");
        unsafe { iris_engine_free(h) };
    }
    arm(Point::New);
    assert!(unsafe { sgi_mgras_new(&config(0), &host_cb()) }.is_null());
}

/// The constructor refuses what it cannot build, and says nothing else.
#[test]
fn the_constructor_refuses_a_bad_configuration() {
    let mut c = config(0);
    c.board = 7;
    assert!(unsafe { sgi_mgras_new(&c, &host_cb()) }.is_null(), "no such board");
    let mut c = config(0);
    c.common.lent_len = 64;
    assert!(unsafe { sgi_mgras_new(&c, &host_cb()) }.is_null(), "MGRAS takes no lent memory");
    let mut c = config(0);
    c.common.abi_version = 2;
    assert!(unsafe { sgi_mgras_new(&c, &host_cb()) }.is_null(), "another ABI version");
    assert!(unsafe { sgi_mgras_new(std::ptr::null(), &host_cb()) }.is_null());
    assert!(unsafe { sgi_mgras_new(&config(0), std::ptr::null()) }.is_null());
    let h = unsafe { sgi_mgras_new(&config(1), &host_cb()) };
    assert!(!h.is_null());
    unsafe { iris_engine_free(h) };
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

/// A panic on the worker thread: the next wait and the next call say so, and
/// every entry answers as a faulted engine does.
#[test]
fn a_dead_worker_is_a_device_fault() {
    let _g = ONE.lock();
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Threaded), Null, None, None).unwrap();
    arm(Point::Exec);
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Done);
    assert_eq!(e.wait_idle(0, 10_000_000_000), Idle::Faulted);
    assert!(e.fault().is_some_and(|f| f.contains("injected panic at Exec")));
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Fault);
    assert_eq!(e.push_words(0, &[0]), 0);
    assert_eq!(e.read(host::SET_FLAGS, 32), Read::Fault);
    assert!(!e.access_waits(host::RASTER, 32, true) && !e.access_waits(host::RASTER, 32, false));
    assert_eq!(e.run(10), 0);
    assert_eq!(e.advance(1).idle, Idle::Faulted);
    let f = e.fault().unwrap();
    // Before a quiesce too: the fault, not "not quiesced".
    assert_eq!(e.observe_all(), Err(Refused::Faulted(f.clone())));
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::Faulted(f.clone())));
    assert_eq!(e.load(&mut sgi_engine::VecSource::new(vec![])), Err(Refused::Faulted(f.clone())));
    let mut ev = Vec::new();
    e.drain_events(&mut ev, usize::MAX);
    assert!(ev.iter().any(|x| matches!(x, Event::DeviceFault(m) if m.contains("injected panic at Exec"))), "{ev:?}");
    assert_eq!(e.quiesce(u64::MAX), Err(Refused::Faulted(f)), "reports the fault and parks");
    e.reset().unwrap();
    e.resume();
    assert_eq!(e.fault(), None);
    assert_eq!(e.write(host::CFIFO, 32, 0), Write::Done);
    assert_eq!(e.wait_idle(0, 10_000_000_000), Idle::Yes);
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
    assert!(matches!(e.quiesce(u64::MAX), Err(Refused::Faulted(_))));
    e.reset().unwrap();
    e.resume();
    arm(Point::Read);
    assert_eq!(e.read(host::RASTER, 32), Read::Fault);
    assert!(e.fault().is_some_and(|f| f.contains("injected panic at Read")));
}
