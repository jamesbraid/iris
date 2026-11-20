//! The picture the engine composes from its display bus and pixel planes:
//! the size, the generation, the CPU frame, a saved engine's frame, and (when
//! a GPU is there) the same frame lent and composed into a caller's buffer.

mod support;

use std::sync::Arc;

use sgi_engine::{BoardEngine, FrameWords, Refused, VecSource, Write};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::wgpu::{Gpu, GpuOptions};
use support::*;

/// A store to the display bus: `width` bytes of `data` to select `crs` of device `dev`.
fn bus<E: BoardEngine>(e: &mut E, dev: u32, crs: u32, width: u32, data: u32) {
    let off = 0x60000 + dev * 0x400 + (crs << 7) + ((width & 3) << 3);
    assert_eq!(e.write(off, 32, (data as u64) << (8 * (4 - width))), Write::Done);
}

/// Colour map entry `index` (both maps) is `rgb`.
fn cmap<E: BoardEngine>(e: &mut E, index: u32, rgb: u32) {
    bus(e, 3, 0, 1, index & 0xFF);
    bus(e, 3, 1, 1, index >> 8);
    bus(e, 3, 2, 4, rgb);
}

/// The recording that stores colour indices through the X server's window.
fn drawn_player(gpu: Option<&Arc<Gpu>>) -> Player {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-pixels-rectwrite").unwrap().1);
    let mut p = Player::on(config(&rep.head, Mode::Inline, None), Drive::Each, gpu);
    rep.cmds.iter().for_each(|c| p.apply(c));
    drive(&mut p.e);
    p
}

/// The palette the recording's indices show through: (x, y, index, colour), y from the top.
const SPOTS: [(usize, usize, u32, u32); 5] = [(96, 195, 0x160, 0x010203), (146, 195, 0x170, 0x112233), (194, 195, 0x168, 0x0a0b0c), (196, 195, 0, 0x040506), (96, 194, 8, 0x070809)];

fn paint<E: BoardEngine>(e: &mut E) {
    for (_, _, index, rgb) in SPOTS {
        cmap(e, index, rgb);
    }
}

fn check(out: &[u32], w: usize, why: &str) {
    for (x, y, index, rgb) in SPOTS {
        assert_eq!(out[y * w + x], rgb, "{why}: pixel ({x}, {y}), index {index:#x}");
    }
}

#[test]
fn the_cpu_frame_shows_the_planes_through_the_colour_map() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    assert_eq!(p.e.heads(), 1);
    assert_eq!(p.e.frame_size(0), Some((1280, 1024)), "before the VC3 is loaded: the PROM's mode");
    let mut out = Vec::new();
    assert_eq!(p.e.scanout(0, &mut out), Ok(Some((1280, 1024))));
    assert_eq!(out.len(), 1280 * 1024);
    check(&out, 1280, "cpu");
    assert_eq!(p.e.scanout(1, &mut out), Ok(None));
    assert!(out.is_empty());
}

#[test]
fn the_generation_moves_with_drawing_and_display_changes_and_nothing_else() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-pixels-rectwrite").unwrap().1);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    let g0 = p.e.frame_generation(0);
    assert_eq!(g0, 1, "a new engine");
    rep.cmds.iter().for_each(|c| p.apply(c));
    drive(&mut p.e);
    let g = p.e.frame_generation(0);
    assert!(g > g0, "drawing");
    assert_eq!(p.e.frame_generation(0), g, "idle");
    let mut out = Vec::new();
    p.e.scanout(0, &mut out).unwrap();
    assert_eq!(p.e.frame_generation(0), g, "a frame taken");
    // An index write alone changes nothing on the screen.
    bus(&mut p.e, 3, 0, 1, 5);
    assert_eq!(p.e.frame_generation(0), g, "a colour-map address");
    bus(&mut p.e, 3, 2, 4, 0xFFFFFF);
    assert!(p.e.frame_generation(0) > g, "a colour-map entry");
    assert_eq!(p.e.frame_generation(1), 0, "no such head");
}

#[test]
fn the_display_bus_answers_while_work_is_queued() {
    let mut cfg = Config::new(Board::High, Mode::Inline);
    cfg.queue_ops = 64;
    let mut e = MgrasEngine::new(cfg, Mem::default(), None, None).unwrap();
    for i in 0..8 {
        e.write(sgi_mgras::dev::mgras::hq3::host::RASTER + 0x40, 32, i);
    }
    assert!(!e.access_waits(0x60000 + 6 * 0x400 + (2 << 7), 32, false));
    cmap(&mut e, 0x123, 0x445566);
    bus(&mut e, 3, 0, 1, 0x23);
    bus(&mut e, 3, 1, 1, 0x01);
    assert_eq!(e.read(0x60000 + 4 * 0x400 + (2 << 7), 32), sgi_engine::Read::Value(0x445566), "read back through colour map 0, with the queue not run");
}

#[test]
fn a_saved_engines_frame_is_the_same_frame() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    p.e.quiesce(u64::MAX).unwrap();
    let mut state = Vec::new();
    p.e.save(&mut state).unwrap();
    let mut a = Vec::new();
    p.e.scanout(0, &mut a).unwrap();
    let mut e = MgrasEngine::new(config("sgi-mgras", Mode::Inline, None), Mem::default(), None, None).unwrap();
    let mut blank = Vec::new();
    e.scanout(0, &mut blank).unwrap();
    assert_ne!(blank, a);
    e.load(&mut VecSource::new(state.clone())).unwrap();
    let mut b = Vec::new();
    e.scanout(0, &mut b).unwrap();
    assert!(a == b, "the frame after a load differs");
    check(&b, 1280, "loaded");
    // A state with no display bus is refused whole.
    state.retain(|c| c.name != "dcb");
    assert!(matches!(e.load(&mut VecSource::new(state)), Err(Refused::Mismatch(_))));
}

#[test]
fn a_reset_clears_the_display_bus() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    let g = p.e.frame_generation(0);
    p.e.quiesce(u64::MAX).unwrap();
    p.e.reset().unwrap();
    assert!(p.e.frame_generation(0) > g);
    let mut out = Vec::new();
    p.e.scanout(0, &mut out).unwrap();
    assert!(out.iter().all(|&p| p == 0), "an engine that drew nothing shows colour map entry 0, which is black");
}

fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))) {
        Ok(g) => Some(g),
        Err(e) => {
            assert!(std::env::var_os("IRIS_REQUIRE_GPU").is_none(), "no GPU: {e}");
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn read_words(gpu: &Gpu, buf: &wgpu::Buffer, words: usize) -> Vec<u32> {
    let mut out = vec![0u32; words];
    gpu.read(buf, &[(0, words as u64 * 4)], bytemuck::cast_slice_mut(&mut out)).expect("read back");
    out
}

#[test]
fn the_gpu_frame_is_the_cpu_frame_lent_or_composed_into_a_buffer() {
    let Some(gpu) = gpu() else { return };
    let mut p = drawn_player(Some(&gpu));
    paint(&mut p.e);
    let mut cpu = drawn_player(None);
    paint(&mut cpu.e);
    let (mut want, mut got) = (Vec::new(), Vec::new());
    cpu.e.scanout(0, &mut want).unwrap();
    assert_eq!(p.e.scanout(0, &mut got), Ok(Some((1280, 1024))));
    assert!(got == want, "the GPU frame read back differs from the CPU frame");
    check(&got, 1280, "gpu");

    // Lent: the board's buffer, 0xAABBGGRR rows of the width.
    let g = p.e.frame_generation(0);
    let lent = p.e.lend_frame(0).unwrap().expect("a lent frame");
    assert_eq!((lent.width, lent.height, lent.stride, lent.words, lent.generation), (1280, 1024, 1280, FrameWords::Abgr, g));
    let buf = lent.buffer.downcast_ref::<wgpu::Buffer>().expect("a wgpu buffer");
    let words = read_words(&gpu, buf, 1280 * 1024);
    let abgr = |p: u32| 0xFF00_0000 | (p & 0xFF) << 16 | p & 0xFF00 | p >> 16 & 0xFF;
    assert!(words.iter().zip(&want).all(|(w, c)| *w == abgr(*c)), "the lent frame differs");

    // Composed into a buffer at a stride wider than the frame.
    let stride = 1280 + 16;
    let dst = Arc::new(gpu.buffer("test destination", (stride * 1024 * 4) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC));
    assert_eq!(p.e.compose_into(0, &dst, stride as u32), Ok(Some((1280, 1024))));
    let words = read_words(&gpu, &dst, stride * 1024);
    for y in 0..1024 {
        assert!(words[y * stride..y * stride + 1280] == want[y * 1280..(y + 1) * 1280], "row {y}");
    }

    // A destination that cannot take the frame is an error of the call, not a fault.
    let small = gpu.buffer("too small", 4096, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);
    assert!(p.e.compose_into(0, &small, 1280).is_err());
    assert!(p.e.compose_into(0, &dst, 100).is_err(), "a stride under the width");
    let no_storage = gpu.buffer("no storage", (1280 * 1024 * 4) as u64, wgpu::BufferUsages::COPY_SRC);
    assert!(p.e.compose_into(0, &no_storage, 1280).is_err());
    assert!(p.e.compose_into(0, &5u32, 1280).is_err(), "not a buffer");
    assert_eq!(p.e.fault(), None, "a refused destination leaves the engine running");
    assert_eq!(p.e.compose_into(0, &dst, stride as u32), Ok(Some((1280, 1024))));
    assert_eq!(p.e.compose_into(1, &dst, stride as u32), Ok(None));
}

#[test]
fn without_a_gpu_unit_there_is_nothing_to_lend() {
    let mut p = drawn_player(None);
    assert!(p.e.lend_frame(0).unwrap().is_none());
    let (gpu_less, _) = (&mut p.e, ());
    assert_eq!(gpu_less.compose_into(0, &5u32, 1280), Ok(None));
}

/// An option board's device on the display bus: the engine calls out for a slot it has no device for, the
/// device answers and takes stores there, and what the engine models is not offered to it. Without a device
/// (or where it declines) the slot keeps the engine's scratch register, and the device survives a reset.
#[test]
fn a_shell_device_on_the_display_bus_answers_the_slots_the_engine_has_none_for() {
    use sgi_mgras::dev::mgras::display::BusSlave;
    struct Board44 {
        stores: Arc<parking_lot::Mutex<Vec<(u32, u32, u32, u32)>>>,
    }
    impl BusSlave for Board44 {
        fn read(&mut self, dev: u32, crs: u32, _width: u32) -> Option<u32> {
            // Slot 20 answers a probe byte at block 0 and a counter at block 2; any other is left alone.
            match (dev, crs) {
                (20, 0) => Some(0xa5),
                (20, 2) => Some(self.stores.lock().len() as u32),
                _ => None,
            }
        }
        fn write(&mut self, dev: u32, crs: u32, width: u32, data: u32) -> bool {
            if dev == 20 {
                self.stores.lock().push((dev, crs, width, data));
            }
            dev == 20
        }
    }
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Mem::default(), None, None).unwrap();
    let read = |e: &mut MgrasEngine<Mem>, dev: u32, crs: u32| match e.read(0x60000 + dev * 0x400 + (crs << 7), 32) {
        sgi_engine::Read::Value(v) => v as u32,
        other => panic!("{other:?}"),
    };
    // No device attached: the engine's scratch register holds what was stored.
    bus(&mut e, 20, 1, 4, 0x1234);
    assert_eq!(read(&mut e, 20, 1), 0x1234);
    assert_eq!(read(&mut e, 20, 0), 0);
    let stores = Arc::new(parking_lot::Mutex::new(Vec::new()));
    e.set_display_bus(Some(Box::new(Board44 { stores: stores.clone() })));
    assert_eq!(read(&mut e, 20, 0), 0xa5, "the device answers its probe byte");
    bus(&mut e, 20, 1, 4, 0x77);
    assert_eq!(stores.lock().len(), 1, "and takes the store");
    assert_eq!(stores.lock()[0], (20, 1, 4, 0x77));
    assert_eq!(read(&mut e, 20, 2), 1, "a block it answers by its own state");
    assert_eq!(read(&mut e, 20, 1), 0x1234, "a block it declines is the engine's scratch register, unchanged by the device's store");
    // The engine's own devices are not offered to it: the board-version slot answers as before.
    assert_ne!(read(&mut e, 9, 0), 0xa5);
    assert_eq!(stores.lock().len(), 1);
    e.quiesce(u64::MAX).unwrap();
    e.reset().unwrap();
    assert_eq!(read(&mut e, 20, 0), 0xa5, "the device is the shell's: it stays through a reset");
    e.set_display_bus(None);
    assert_eq!(read(&mut e, 20, 0), 0, "detached");
}
