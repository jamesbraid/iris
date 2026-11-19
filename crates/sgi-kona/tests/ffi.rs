//! The constructor and the pipe's entries through the C ABI.

use std::ffi::{c_int, c_void, CString};
use std::ptr::{null, null_mut};

use sgi_kona::ffi::*;
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

fn config(product: *const std::ffi::c_char) -> IrisKonaConfig {
    IrisKonaConfig {
        common: IrisEngineConfig { abi_version: IRIS_ENGINE_ABI_VERSION, struct_size: std::mem::size_of::<IrisKonaConfig>() as u32, mode: 0, queue_ops: 0, gpu: 0, gpu_backend: null(), lent_ptr: null_mut(), lent_len: 0, reserved: [0; 4] },
        product,
        rm_count: 1,
        identity_index: 0,
        host_widget: 0,
        widget: 0,
        probe: 0,
        managed_width: 0,
        managed_height: 0,
        pool_threads: 0,
        pool_tile: 32,
    }
}

#[test]
fn the_constructor_builds_an_engine_the_shared_entries_drive() {
    let ir2 = CString::new("ir2").unwrap();
    let h = unsafe { sgi_kona_new(&config(ir2.as_ptr()), &host()) };
    assert!(!h.is_null());
    assert_eq!(unsafe { iris_engine_write(h, 0, 32, 1) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    let r = IrisKonaRegion { word: 1, x0: 0, y0: 0, x1: 3, y1: 3 };
    assert_eq!(unsafe { sgi_kona_observe(h, &r) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { sgi_kona_observe(h, null()) }, IRIS_ENGINE_ERROR);
    unsafe { iris_engine_free(h) };
}

#[test]
fn the_constructor_refuses_what_it_cannot_build() {
    let (ir2, bogus) = (CString::new("ir2").unwrap(), CString::new("ir9").unwrap());
    assert!(unsafe { sgi_kona_new(&config(null()), &host()) }.is_null(), "no product");
    assert!(unsafe { sgi_kona_new(&config(bogus.as_ptr()), &host()) }.is_null(), "no such product");
    let mut c = config(ir2.as_ptr());
    c.rm_count = 3;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "a pipe holds one or two raster managers");
    let mut c = config(ir2.as_ptr());
    c.widget = 16;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "a widget number is four bits");
    let mut c = config(ir2.as_ptr());
    c.common.lent_len = 64;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "KONA takes no lent memory");
    let mut c = config(ir2.as_ptr());
    c.common.abi_version = 2;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "another ABI version");
    assert!(unsafe { sgi_kona_new(std::ptr::null(), &host()) }.is_null());
}
