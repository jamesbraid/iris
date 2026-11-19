//! The constructor and the board's entries through the C ABI: what a shell calls before the
//! shared entries (which `sgi-engine` tests) have an engine to act on.

use std::ffi::{c_int, c_void, CString};
use std::ptr::{null, null_mut};

use iris_vpro::dev::odyssey::engine::memory_bytes;
use iris_vpro::ffi::*;
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
        widget: 11,
        pool_threads: 0,
        pool_tile: 0,
    }
}

#[test]
fn the_constructor_builds_an_engine_the_shared_entries_drive() {
    let v12 = CString::new("v12").unwrap();
    for mode in [0, 1] {
        let h = unsafe { iris_vpro_new(&config(v12.as_ptr(), mode), &host()) };
        assert!(!h.is_null(), "mode {mode}");
        let mut id = 0u64;
        assert_eq!(unsafe { iris_engine_read(h, 0x4, 32, &mut id) }, IRIS_ENGINE_OK);
        assert_ne!(id, 0, "the identity register answers");
        assert_eq!(unsafe { iris_engine_write(h, 0x110000, 32, 0) }, IRIS_ENGINE_OK);
        assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
        assert_eq!(unsafe { iris_vpro_observe(h, 0, 0, 3, 3) }, IRIS_ENGINE_OK);
        unsafe { iris_engine_free(h) };
    }
}

#[test]
fn the_constructor_refuses_what_it_cannot_build_and_the_memory_size_follows_the_product() {
    let (v12, bogus) = (CString::new("v12").unwrap(), CString::new("v99").unwrap());
    assert!(unsafe { iris_vpro_new(&config(null(), 0), &host()) }.is_null(), "no product");
    assert!(unsafe { iris_vpro_new(&config(bogus.as_ptr(), 0), &host()) }.is_null(), "no such product");
    let mut c = config(v12.as_ptr(), 0);
    c.common.abi_version = 2;
    assert!(unsafe { iris_vpro_new(&c, &host()) }.is_null(), "another ABI version");
    let mut c = config(v12.as_ptr(), 0);
    let mut word = 0u64;
    c.common.lent_ptr = (&mut word as *mut u64 as *mut u8).wrapping_add(1);
    c.common.lent_len = 8;
    assert!(unsafe { iris_vpro_new(&c, &host()) }.is_null(), "a misaligned lent region");
    assert!(unsafe { iris_vpro_new(std::ptr::null(), &host()) }.is_null());
    assert!(unsafe { iris_vpro_new(&config(v12.as_ptr(), 0), std::ptr::null()) }.is_null());
    assert_eq!(unsafe { iris_vpro_memory_bytes(v12.as_ptr()) }, memory_bytes("v12").unwrap());
    assert_eq!((unsafe { iris_vpro_memory_bytes(bogus.as_ptr()) }, unsafe { iris_vpro_memory_bytes(null()) }), (0, 0));
}
