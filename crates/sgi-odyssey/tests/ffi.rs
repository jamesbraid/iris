//! The constructor and the board's entries through the C ABI: what a shell calls before the
//! shared entries (which `sgi-engine` tests) have an engine to act on.

use std::ffi::{c_int, c_void, CString};
use std::ptr::{null, null_mut};

use sgi_odyssey::dev::odyssey::engine::memory_bytes;
use sgi_odyssey::ffi::*;
use sgi_engine::ffi::*;

unsafe extern "C" fn ram_read(_: *mut c_void, _: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    unsafe { std::ptr::write_bytes(buf, 0, len) };
    0
}

unsafe extern "C" fn ram_write(_: *mut c_void, _: u64, _: *const u8, _: usize, _: u32) -> c_int {
    0
}

fn host() -> IrisEngineHost {
    IrisEngineHost { abi_version: IRIS_ENGINE_ABI_VERSION, struct_size: std::mem::size_of::<IrisEngineHost>() as u32, user: null_mut(), ram_read: Some(ram_read), ram_write: Some(ram_write), bus_read: None, bus_write: None, notify: None }
}

fn config(product: *const std::ffi::c_char, mode: u32) -> IrisVproConfig {
    IrisVproConfig {
        common: IrisEngineConfig { abi_version: IRIS_ENGINE_ABI_VERSION, struct_size: std::mem::size_of::<IrisVproConfig>() as u32, mode, queue_ops: 0, gpu: 0, gpu_backend: null(), lent_ptr: null_mut(), lent_len: 0, reserved: [0; 4] },
        product,
        nic_record: null(),
        nic_serial: 0,
        widget: 11,
        revision: 0xa,
        memory_size: 128,
        memory_banks: 4,
        cas_latency: 3,
        backend_version: 1,
        pool_threads: 0,
        pool_tile: 0,
    }
}

#[test]
fn the_constructor_builds_an_engine_the_shared_entries_drive() {
    let v12 = CString::new("v12").unwrap();
    for mode in [0, 1] {
        let h = unsafe { sgi_odyssey_new(&config(v12.as_ptr(), mode), &host()) };
        assert!(!h.is_null(), "mode {mode}");
        let mut id = 0u64;
        assert_eq!(unsafe { iris_engine_read(h, 0x4, 32, &mut id) }, IRIS_ENGINE_OK);
        assert_ne!(id, 0, "the identity register answers");
        assert_eq!(unsafe { iris_engine_write(h, 0x110000, 32, 0) }, IRIS_ENGINE_OK);
        assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
        assert_eq!(unsafe { sgi_odyssey_observe(h, 0, 0, 3, 3) }, IRIS_ENGINE_OK);
        unsafe { iris_engine_free(h) };
    }
}

#[test]
fn the_constructor_refuses_what_it_cannot_build_and_the_memory_size_follows_the_product() {
    let (v12, bogus) = (CString::new("v12").unwrap(), CString::new("v99").unwrap());
    assert!(unsafe { sgi_odyssey_new(&config(bogus.as_ptr(), 0), &host()) }.is_null(), "no such product");
    let mut c = config(v12.as_ptr(), 0);
    c.common.abi_version = 2;
    assert!(unsafe { sgi_odyssey_new(&c, &host()) }.is_null(), "another ABI version");
    let mut c = config(v12.as_ptr(), 0);
    let mut word = 0u64;
    c.common.lent_ptr = (&mut word as *mut u64 as *mut u8).wrapping_add(1);
    c.common.lent_len = 8;
    assert!(unsafe { sgi_odyssey_new(&c, &host()) }.is_null(), "a misaligned lent region");
    assert!(unsafe { sgi_odyssey_new(std::ptr::null(), &host()) }.is_null());
    assert!(unsafe { sgi_odyssey_new(&config(v12.as_ptr(), 0), std::ptr::null()) }.is_null());
    assert_eq!(unsafe { sgi_odyssey_memory_bytes(&config(v12.as_ptr(), 0)) }, memory_bytes("v12").unwrap());
    assert_eq!((unsafe { sgi_odyssey_memory_bytes(&config(bogus.as_ptr(), 0)) }, unsafe { sgi_odyssey_memory_bytes(null()) }), (0, 0));
}

/// With no product the properties are the board's, checked; a product overrides the ones it names.
#[test]
fn the_board_properties_configure_the_board_and_a_product_overrides_them() {
    let mut c = config(null(), 0);
    c.revision = 3;
    c.memory_size = 32;
    c.memory_banks = 2;
    c.cas_latency = 2;
    c.nic_serial = 0x1234;
    let record = CString::new("Name:SHELLBOARD").unwrap();
    c.nic_record = record.as_ptr();
    assert_eq!(unsafe { sgi_odyssey_memory_bytes(&c) }, 32 << 20);
    let h = unsafe { sgi_odyssey_new(&c, &host()) };
    assert!(!h.is_null());
    let mut id = 0u64;
    assert_eq!(unsafe { iris_engine_read(h, 0x4, 32, &mut id) }, IRIS_ENGINE_OK);
    assert_eq!(id >> 28, 3, "the revision property reaches the identity register");
    unsafe { iris_engine_free(h) };

    let v12 = CString::new("v12").unwrap();
    c.product = v12.as_ptr();
    assert_eq!(unsafe { sgi_odyssey_memory_bytes(&c) }, 128 << 20, "the product's memory size wins");

    let cases: [(&str, fn(&mut IrisVproConfig)); 5] = [("banks", |c| c.memory_banks = 3), ("cas latency", |c| c.cas_latency = 1), ("memory size", |c| c.memory_size = 48), ("revision", |c| c.revision = 16), ("serial", |c| c.nic_serial = 1 << 48)];
    for (what, bad) in cases {
        let mut c = config(null(), 0);
        bad(&mut c);
        assert!(unsafe { sgi_odyssey_new(&c, &host()) }.is_null(), "{what} is refused");
    }
}

/// A short buffer is refused with the frame's size, and a buffer of that size gets the frame.
#[test]
fn the_scanout_tells_the_size_when_the_buffer_is_short_and_copies_when_it_fits() {
    let v12 = CString::new("v12").unwrap();
    let h = unsafe { sgi_odyssey_new(&config(v12.as_ptr(), 0), &host()) };
    assert!(!h.is_null());
    let (mut w, mut hgt) = (0u32, 0u32);
    let mut small = [0xdead_beefu32; 4];
    assert_eq!(unsafe { sgi_odyssey_scanout(h, small.as_mut_ptr(), small.len(), &mut w, &mut hgt) }, IRIS_ENGINE_ERROR, "a short buffer is refused");
    assert!(w > 0 && hgt > 0 && small.iter().all(|&p| p == 0xdead_beef), "with the size, and nothing copied");
    let mut frame = vec![0xdead_beefu32; w as usize * hgt as usize];
    let (mut w2, mut h2) = (0u32, 0u32);
    assert_eq!(unsafe { sgi_odyssey_scanout(h, frame.as_mut_ptr(), frame.len(), &mut w2, &mut h2) }, IRIS_ENGINE_OK);
    assert_eq!((w2, h2), (w, hgt));
    assert!(frame.iter().all(|&p| p >> 24 == 0), "0x00RRGGBB: no pixel keeps the sentinel");
    assert_eq!(unsafe { sgi_odyssey_scanout(h, null_mut(), 0, null_mut(), &mut hgt) }, IRIS_ENGINE_ERROR);
    unsafe { iris_engine_free(h) };
}

/// The counters exist with a GPU unit and are refused without one.
#[test]
fn the_gpu_counters_are_there_with_a_gpu_unit_and_refused_without() {
    let v12 = CString::new("v12").unwrap();
    let mut counters = IrisVproGpuCounters::default();
    let h = unsafe { sgi_odyssey_new(&config(v12.as_ptr(), 0), &host()) };
    assert_eq!(unsafe { sgi_odyssey_gpu_counters(h, &mut counters) }, IRIS_ENGINE_ERROR, "no GPU unit");
    assert_eq!(unsafe { sgi_odyssey_gpu_counters(h, null_mut()) }, IRIS_ENGINE_ERROR);
    unsafe { iris_engine_free(h) };

    let mut cfg = config(v12.as_ptr(), 0);
    cfg.common.gpu = IRIS_ENGINE_GPU_AUTO;
    let h = unsafe { sgi_odyssey_new(&cfg, &host()) };
    if h.is_null() {
        return; // no GPU adapter on this host
    }
    assert_eq!(unsafe { sgi_odyssey_gpu_counters(h, &mut counters) }, IRIS_ENGINE_OK);
    assert_eq!((counters.faults, counters.rejections), (0, 0));
    unsafe { iris_engine_free(h) };
}

#[test]
fn the_cursor_accessor_answers_the_registers_and_refuses_a_null_out() {
    let v12 = CString::new("v12").unwrap();
    let h = unsafe { sgi_odyssey_new(&config(v12.as_ptr(), 0), &host()) };
    let (mut x, mut y, mut shown) = (9i32, 9i32, 9u32);
    assert_eq!(unsafe { sgi_odyssey_cursor(h, &mut x, &mut y, &mut shown) }, IRIS_ENGINE_OK);
    assert_eq!((x, y, shown), (0, 0, 0), "off at power-on");
    assert_eq!(unsafe { sgi_odyssey_cursor(h, null_mut(), &mut y, &mut shown) }, IRIS_ENGINE_ERROR);
    unsafe { iris_engine_free(h) };
}

/// A Rust shell that owns the process's GPU device builds the engine on it: the GPU unit is there
/// without the configuration asking for one, and the device's loss callback is the engine's.
#[test]
fn an_engine_built_on_the_shells_device_has_a_gpu_unit_on_it() {
    let loss = sgi_engine::GpuLoss::new();
    let Ok(gpu) = sgi_odyssey::wgpu::Gpu::new(&sgi_odyssey::wgpu::GpuOptions::default(), loss.callback()) else { return };
    let v12 = CString::new("v12").unwrap();
    let h = unsafe { new_on(&config(v12.as_ptr(), 0), &host(), gpu, loss) };
    assert!(!h.is_null());
    let mut counters = IrisVproGpuCounters::default();
    assert_eq!(unsafe { sgi_odyssey_gpu_counters(h, &mut counters) }, IRIS_ENGINE_OK, "a GPU unit, though gpu was 0 in the configuration");
    unsafe { iris_engine_free(h) };
}
