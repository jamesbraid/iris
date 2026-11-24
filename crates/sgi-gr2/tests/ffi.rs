//! The C ABI, driven the way a C shell would: a store and a read, the read port in a batch, a picture,
//! and the constructor's refusals.

use std::ffi::{c_int, c_void};
use std::ptr::{null, null_mut};

use sgi_engine::ffi::*;
use sgi_gr2::dev::gr2::hq2;
use sgi_gr2::ffi::*;

unsafe extern "C" fn ram_read(_: *mut c_void, _: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    unsafe { std::ptr::write_bytes(buf, 0, len) };
    0
}

unsafe extern "C" fn ram_write(_: *mut c_void, _: u64, _: *const u8, _: usize, _: u32) -> c_int {
    0
}

fn host() -> IrisEngineHost {
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

fn config(mode: u32, gpu: u32) -> IrisGr2Config {
    IrisGr2Config {
        common: IrisEngineConfig {
            abi_version: IRIS_ENGINE_ABI_VERSION,
            struct_size: std::mem::size_of::<IrisGr2Config>() as u32,
            mode,
            queue_ops: 0,
            gpu,
            gpu_backend: null(),
            lent_ptr: null_mut(),
            lent_len: 0,
            reserved: [0; 4],
        },
        ges: 2,
        ports: [0xdd, 0x3c, 0xfc, 0xff],
        hq_rev: 0,
        aux_window: 0,
        jit: 0,
    }
}

fn new() -> *mut IrisEngine {
    let h = unsafe { sgi_gr2_new(&config(1, IRIS_ENGINE_GPU_NONE), &host()) };
    assert!(!h.is_null());
    h
}

fn wr(h: *mut IrisEngine, off: u32, val: u64) {
    for _ in 0..1000 {
        match unsafe { iris_engine_write(h, off, 32, val) } {
            IRIS_ENGINE_OK => return,
            IRIS_ENGINE_BUSY => {
                let mut i = IrisEngineIdle::default();
                unsafe { iris_engine_wait_idle(h, 0, 1_000_000, &mut i) };
            }
            other => panic!("write {off:#x}: {other}"),
        }
    }
    panic!("write {off:#x} stayed busy");
}

fn rd(h: *mut IrisEngine, off: u32, bits: u32) -> u64 {
    for _ in 0..1000 {
        let mut v = 0u64;
        match unsafe { iris_engine_read(h, off, bits, &mut v) } {
            IRIS_ENGINE_OK => return v,
            IRIS_ENGINE_BUSY => {
                let mut i = IrisEngineIdle::default();
                unsafe { iris_engine_wait_idle(h, 0, 1_000_000, &mut i) };
            }
            other => panic!("read {off:#x}: {other}"),
        }
    }
    panic!("read {off:#x} stayed busy");
}

#[test]
fn the_constructor_refuses_what_the_board_does_not_do() {
    let h = host();
    assert!(unsafe { sgi_gr2_new(&config(0, IRIS_ENGINE_GPU_NONE), &h) }.is_null(), "inline");
    assert!(unsafe { sgi_gr2_new(&config(1, IRIS_ENGINE_GPU_AUTO), &h) }.is_null(), "a GPU unit");
    let mut c = config(1, IRIS_ENGINE_GPU_NONE);
    c.ges = 3;
    assert!(unsafe { sgi_gr2_new(&c, &h) }.is_null(), "three geometry engines");
    c = config(1, IRIS_ENGINE_GPU_NONE);
    c.aux_window = 3;
    assert!(unsafe { sgi_gr2_new(&c, &h) }.is_null(), "an unknown window kind");
    let mut mem = [0u64; 4];
    c = config(1, IRIS_ENGINE_GPU_NONE);
    c.common.lent_ptr = mem.as_mut_ptr().cast();
    c.common.lent_len = 32;
    assert!(unsafe { sgi_gr2_new(&c, &h) }.is_null(), "lent memory");
    assert!(unsafe { sgi_gr2_new(null(), &h) }.is_null());
}

#[test]
fn a_store_a_read_and_a_picture_through_the_c_abi() {
    let h = new();
    assert_eq!(rd(h, 0x6c000, 8), 0xdd, "the configured port");
    wr(h, 0x6a070, 0);
    wr(h, 0x6a078, 0);
    for (index, v) in [(hq2::PUC_INIT, 1), (hq2::PUC_COLOR, 0), (hq2::PUC_RECTI2D, 0), (hq2::PUC_DATA, 0), (hq2::PUC_DATA, 1279), (hq2::PUC_DATA, 1023)] {
        wr(h, 0x40000 + 4 * index, v);
    }
    let mut px = vec![0u32; 1280 * 1024];
    let (mut w, mut hgt) = (0u32, 0u32);
    assert_eq!(unsafe { iris_engine_scanout(h, 0, px.as_mut_ptr(), px.len(), &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!((w, hgt), (1280, 1024));
    assert_eq!(unsafe { iris_engine_heads(h) }, 1);
    let mut small = [0u32; 4];
    assert_eq!(unsafe { iris_engine_scanout(h, 0, small.as_mut_ptr(), small.len(), &mut w, &mut hgt) }, IRIS_ENGINE_ERROR, "a buffer too small");
    unsafe { iris_engine_free(h) };
}

#[test]
fn the_read_port_comes_out_in_a_batch_through_the_c_abi() {
    let h = new();
    // A context-save request makes the HQ2 queue an image on the read port.
    for id in [0x74, 0xe8] {
        wr(h, 0x40000 + 4 * hq2::GE_HQMSAV, id);
        wr(h, 0x40000 + 4 * hq2::PUC_DATA, 0);
        wr(h, 0x40000 + 4 * hq2::PUC_DATA, 0);
    }
    wr(h, 0x40000 + 4 * hq2::GE_CX_SAVE_MAIN, 0);
    let mut words = [0u32; 8];
    let mut got = 0;
    for _ in 0..1000 {
        got += unsafe { iris_engine_pull_words(h, 0x6a068, words.as_mut_ptr().add(got), words.len() - got) };
        if got == words.len() {
            break;
        }
        let mut i = IrisEngineIdle::default();
        unsafe { iris_engine_wait_idle(h, 0, 1_000_000, &mut i) };
    }
    assert_eq!(got, words.len());
    assert_eq!(words[0], 0x474c_4358, "the image header");
    assert_eq!(unsafe { iris_engine_pull_words(h, 0x6c000, words.as_mut_ptr(), 1) }, 0, "not a read port");
    unsafe { iris_engine_free(h) };
}

#[test]
fn the_cursor_entry_reports_the_vc1() {
    let h = new();
    let (mut x, mut y) = (0i32, 0i32);
    assert_eq!(unsafe { sgi_gr2_cursor_position(h, &mut x, &mut y) }, 0);
    wr(h, 0x6c040 + 0x18, 0x30);
    assert_eq!(unsafe { sgi_gr2_cursor_position(h, &mut x, &mut y) }, 1);
    assert_eq!(unsafe { sgi_gr2_cursor_position(h, null_mut(), &mut y) }, 0);
    unsafe { iris_engine_free(h) };
}
