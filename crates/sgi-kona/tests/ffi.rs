//! The constructor and the pipe's entries through the C ABI.

use std::ffi::{c_int, c_void, CString};
use std::ptr::{null, null_mut};

use sgi_engine::ffi::*;
use sgi_kona::ffi::*;

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
        geometry: null(),
        rm_type: null(),
        display: null(),
        pixel_depth: null(),
        texture_mb: 0,
        channels: 0,
        ktown2: 0,
        parts: null(),
        num_parts: 0,
        shared_identity: 0,
        geometry_serial: 0,
        user: null_mut(),
        flash_image: null(),
        flash_len: 0,
        flash_write: None,
        dma_widget: None,
        flags: 0,
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
fn the_boards_are_inputs_and_what_a_product_cannot_have_is_refused() {
    let c = |s: &str| CString::new(s).unwrap();
    let (ir2, ir3, ge14_2, ge16_4, dg5_2, rm9, gvo, medium) = (c("ir2"), c("ir3"), c("ge14-2"), c("ge16-4"), c("dg5-2"), c("rm9"), c("dg5-2-gvo"), c("medium"));
    let builds = |c: &IrisKonaConfig| {
        let h = unsafe { sgi_kona_new(c, &host()) };
        let ok = !h.is_null();
        if ok {
            unsafe { iris_engine_free(h) };
        }
        ok
    };
    // A rack pipe: a Ktown allows four raster managers and a DG5-2 with its own two channels.
    let mut rack = config(ir3.as_ptr());
    let (ktown, bogus) = (c("030-1067-001"), c("030-9999-001"));
    let parts = [ktown.as_ptr()];
    (rack.parts, rack.num_parts, rack.rm_count, rack.display, rack.channels, rack.texture_mb) = (parts.as_ptr(), 1, 4, dg5_2.as_ptr(), 2, 256);
    assert!(builds(&rack));
    (rack.parts, rack.num_parts, rack.ktown2) = (null(), 0, 1);
    assert!(builds(&rack), "an Onyx 3400's Ktown2 too");
    rack.ktown2 = 0;
    assert!(!builds(&rack), "a deskside holds two raster managers at most");
    let other = [bogus.as_ptr()];
    (rack.parts, rack.num_parts) = (other.as_ptr(), 1);
    assert!(!builds(&rack), "the chain holds no part but the Ktown");
    let twice = [ktown.as_ptr(), ktown.as_ptr()];
    (rack.parts, rack.num_parts) = (twice.as_ptr(), 2);
    assert!(!builds(&rack), "a part listed twice, which the chain's one-wire search would never finish");
    (rack.parts, rack.num_parts) = (null(), u32::MAX);
    assert!(!builds(&rack), "a count with no list is refused before a list the size of the count is built");
    let mut two = config(ir2.as_ptr());
    (two.geometry, two.pixel_depth) = (ge14_2.as_ptr(), medium.as_ptr());
    assert!(builds(&two), "a two-engine geometry board and medium pixels");
    two.geometry = ge16_4.as_ptr();
    assert!(!builds(&two), "the InfiniteReality2 takes the GE14 boards");
    let mut one = config(ir2.as_ptr());
    one.rm_type = rm9.as_ptr();
    assert!(!builds(&one), "the raster manager type is the product's");
    one.rm_type = null();
    one.texture_mb = 256;
    assert!(!builds(&one), "and so is the texture board");
    one.texture_mb = 0;
    one.channels = 8;
    one.display = dg5_2.as_ptr();
    assert!(!builds(&one), "a DG5-2 has two channels");
    one.channels = 0;
    one.display = gvo.as_ptr();
    assert!(!builds(&one), "the GVO option needs the shell's device for it");
    one.flags = 1;
    assert!(builds(&one), "with the option slave announced, a DG5-2 with a GVO");
}

unsafe extern "C" fn board_read(user: *mut c_void, addr: u32, data: *mut u32) -> c_int {
    let regs = unsafe { &*(user as *const std::sync::Mutex<std::collections::BTreeMap<u32, u32>>) };
    unsafe { *data = regs.lock().unwrap().get(&addr).copied().unwrap_or(0) };
    1
}

unsafe extern "C" fn board_write(user: *mut c_void, addr: u32, data: u32) -> c_int {
    let regs = unsafe { &*(user as *const std::sync::Mutex<std::collections::BTreeMap<u32, u32>>) };
    regs.lock().unwrap().insert(addr, data);
    1
}

/// A C shell gives the engine the option board's device, the engine probes it as the firmware does, and the board's channel
/// has a window the shell reads.
#[test]
fn a_c_shell_attaches_the_option_board_and_reads_its_channels_window() {
    let (ir2, gvo) = (CString::new("ir2").unwrap(), CString::new("dg5-2-gvo").unwrap());
    let mut cfg = config(ir2.as_ptr());
    (cfg.display, cfg.flags) = (gvo.as_ptr(), 1);
    let h = unsafe { sgi_kona_new(&cfg, &host()) };
    assert!(!h.is_null());
    let regs = std::sync::Mutex::new(std::collections::BTreeMap::<u32, u32>::new());
    let user = &regs as *const _ as *mut c_void;
    assert_eq!(unsafe { sgi_kona_set_display_bus(h, user, Some(board_read), Some(board_write)) }, IRIS_ENGINE_OK);
    let video = |command: u32, data: &[u32]| {
        let sum = data.iter().fold(command, |s, w| s.wrapping_add(*w));
        for w in [0x1a6, command].into_iter().chain(data.iter().copied()).chain([sum]) {
            assert_eq!(unsafe { iris_engine_write(h, 0xc10000, 32, w as u64) }, IRIS_ENGINE_OK);
        }
        assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    };
    let (mut x, mut y, mut w, mut hgt) = (0u32, 0u32, 0u32, 0u32);
    assert_eq!(unsafe { sgi_kona_channel_window(h, 0xc6, &mut x, &mut y, &mut w, &mut hgt) }, IRIS_ENGINE_ERROR, "not probed");
    video(0x53, &[0]);
    assert_eq!(regs.lock().unwrap()[&0xc831], 0xc6 + 0x3e8, "the probe reached the callbacks");
    let mut format = [0u32; 383];
    (format[0], format[1], format[23], format[24]) = (0xc6, 1, 486, 720);
    video(0x16, &format);
    video(0x14, &[0xc6, 0, 485, 720, 486, 1, 1, 0]);
    assert_eq!(unsafe { sgi_kona_channel_window(h, 0xc6, &mut x, &mut y, &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!((x, y, w, hgt), (0, 1024 - 1 - 485, 720, 486));
    let mut small = [0u32; 4];
    let (mut sw, mut sh) = (0, 0);
    assert_eq!(unsafe { sgi_kona_channel_scanout(h, 0xc6, small.as_mut_ptr(), 4, &mut sw, &mut sh) }, IRIS_ENGINE_ERROR, "too small: the size comes back");
    assert_eq!((sw, sh), (720, 486));
    let mut whole = vec![0u32; 720 * 486];
    assert_eq!(unsafe { sgi_kona_channel_scanout(h, 0xc6, whole.as_mut_ptr(), whole.len(), &mut sw, &mut sh) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { sgi_kona_channel_scanout(h, 7, whole.as_mut_ptr(), whole.len(), &mut sw, &mut sh) }, IRIS_ENGINE_ERROR, "no such channel");
    assert_eq!(unsafe { sgi_kona_set_display_bus(h, null_mut(), None, None) }, IRIS_ENGINE_OK, "detached");
    unsafe { iris_engine_free(h) };
}

/// The scan-out entry hands the shells' 0x00RRGGBB where the engine composes 0xAABBGGRR, and says
/// the size when the buffer is too small.
#[test]
fn the_scanout_entry_copies_the_frame_as_0x00rrggbb() {
    let ir2 = CString::new("ir2").unwrap();
    let h = unsafe { sgi_kona_new(&config(ir2.as_ptr()), &host()) };
    assert!(!h.is_null());
    // Colour 1 is red; a point at (100, 50), counted up from the bottom row.
    for w in [1u32, 3, 1, 0xff << 2, 0, 0, 1, 2, 1, 1, 6, 100, 50] {
        assert_eq!(unsafe { iris_engine_write(h, 0xc10000, 32, w as u64) }, IRIS_ENGINE_OK);
    }
    assert_eq!(unsafe { iris_engine_quiesce(h, u64::MAX) }, IRIS_ENGINE_OK);
    let (mut w, mut hgt) = (0u32, 0u32);
    assert_eq!(unsafe { sgi_kona_scanout(h, null_mut(), 0, &mut w, &mut hgt) }, IRIS_ENGINE_ERROR);
    assert_eq!((w, hgt), (1280, 1024), "the size, when the buffer is too small");
    assert_eq!(unsafe { sgi_kona_scanout(h, null_mut(), 0, null_mut(), null_mut()) }, IRIS_ENGINE_ERROR);
    let mut frame = vec![0u32; 1280 * 1024];
    assert_eq!(unsafe { sgi_kona_scanout(h, frame.as_mut_ptr(), frame.len(), &mut w, &mut hgt) }, IRIS_ENGINE_OK);
    assert_eq!(frame[(1023 - 50) * 1280 + 100], 0x00ff_0000, "red, with nothing in the top byte");
    assert_eq!(frame.iter().filter(|&&p| p != 0).count(), 1);
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
    c.pool_threads = u32::MAX;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "a thread for every worker asked for");
    let mut c = config(ir2.as_ptr());
    c.managed_width = 1 << 20;
    c.managed_height = 1 << 20;
    assert!(unsafe { sgi_kona_new(&c, &host()) }.is_null(), "a managed area past the largest drawable");
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

/// A Rust shell that owns the process's GPU device builds the pipe on it: the lent frame is a buffer
/// of that device, and the loss callback it gave is the engine's.
#[test]
fn an_engine_built_on_the_shells_device_lends_a_buffer_of_that_device() {
    let loss = sgi_engine::GpuLoss::new();
    let Ok(gpu) = sgi_kona::wgpu::Gpu::new(&sgi_kona::wgpu::GpuOptions::default(), loss.callback()) else { return };
    let ir2 = CString::new("ir2").unwrap();
    let h = unsafe { new_on(&config(ir2.as_ptr()), &host(), gpu.clone(), loss) };
    assert!(!h.is_null());
    assert_eq!(unsafe { iris_engine_write(h, 0, 32, 1) }, IRIS_ENGINE_OK);
    let dst = gpu.buffer("test destination", 1280 * 1024 * 4, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);
    let done = unsafe { call_compose(h, &dst) };
    assert_eq!(done, Some((1280, 1024)), "composed into a buffer of the shell's device");
    // A destination too small for the frame is an error of the call: the device and the pipe go on.
    let small = gpu.buffer("test small", 4096, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);
    assert_eq!(unsafe { call_compose(h, &small) }, None, "refused");
    assert_eq!(unsafe { iris_engine_fault(h) }, std::ptr::null(), "no fault");
    assert_eq!(unsafe { call_compose(h, &dst) }, Some((1280, 1024)), "and the next call works");
    unsafe { iris_engine_free(h) };
}

unsafe fn call_compose(h: *mut IrisEngine, dst: &wgpu::Buffer) -> Option<(u32, u32)> {
    use sgi_engine::ffi::call;
    unsafe { call::<sgi_kona::dev::kona::engine::KonaEngine<sgi_engine::ffi::CMem>, _>(h, sgi_engine::inject::Point::Pixel, None, |e| sgi_engine::BoardEngine::compose_into(e, 0, dst, 1280).ok().flatten()) }
}

#[test]
fn the_link_reset_entry_resets_the_interrupt_registers_and_keeps_the_map_ram() {
    let ir2 = CString::new("ir2").unwrap();
    let h = unsafe { sgi_kona_new(&config(ir2.as_ptr()), &host()) };
    assert!(!h.is_null());
    let rd = |off: u32| {
        let mut v = 0u64;
        assert_eq!(unsafe { iris_engine_read(h, off, 32, &mut v) }, IRIS_ENGINE_OK);
        v
    };
    let wr = |off: u32, val: u64| {
        while unsafe { iris_engine_write(h, off, 32, val) } == IRIS_ENGINE_BUSY {
            unsafe { iris_engine_run(h, usize::MAX) };
        }
        unsafe { iris_engine_run(h, usize::MAX) };
    };
    wr(0x5c, 0x3e2210);
    wr(0x100008, 0x2000_1234);
    assert_eq!(rd(0x5c), 0x3e2210);
    assert_eq!(unsafe { sgi_kona_link_reset(h) }, IRIS_ENGINE_OK);
    assert_eq!((rd(0x5c), rd(0x100008)), (0, 0x2000_1234));
    unsafe { iris_engine_free(h) };
}
