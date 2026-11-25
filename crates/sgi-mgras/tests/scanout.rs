//! The picture the engine composes from its display bus and pixel planes:
//! the size, the generation, the CPU frame, a saved engine's frame, and (when
//! a GPU is there) the same frame lent and composed into a caller's buffer.

mod support;

use std::sync::Arc;

use sgi_engine::{BoardEngine, Event, FrameWords, Refused, VecSource, Write, DAMAGE_ANY};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode, Scan, DAMAGE_DISPLAY};
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

/// A shell's refresh takes the frame on its main thread: `scanout` does not run or wait for the queued work, so a
/// long draw behind it does not hold the refresh up. The frame is what the planes hold.
#[test]
fn scanout_does_not_wait_for_the_queued_work() {
    use sgi_engine::Read;
    use sgi_mgras::dev::mgras::hq3::host;
    let mut cfg = Config::new(Board::High, Mode::Inline);
    cfg.queue_ops = 64;
    let mut e = MgrasEngine::new(cfg, Mem::default(), None, None).unwrap();
    for i in 0..8 {
        assert_eq!(e.write(host::RASTER + 0x40, 32, i), Write::Done);
    }
    let idle = |e: &mut MgrasEngine<Mem>| match e.read(host::STATUS, 32) {
        Read::Value(v) => v as u32 & (host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE),
        other => panic!("{other:?}"),
    };
    assert_eq!(idle(&mut e), 0, "work is queued");
    let mut out = Vec::new();
    assert!(e.scanout(0, &mut out).unwrap().is_some());
    assert_eq!(idle(&mut e), 0, "and the scan-out left it queued");
}

/// The GPU frame, lent or composed into a buffer, is taken as `scanout` is: the queued work is not run or waited
/// for (a refresh's compose under a shell's lock would stall every access behind the draw).
#[test]
fn a_gpu_compose_does_not_wait_for_the_queued_work() {
    use sgi_engine::Read;
    use sgi_mgras::dev::mgras::hq3::host;
    let Some(gpu) = gpu() else { return };
    let mut cfg = Config::new(Board::High, Mode::Inline);
    cfg.queue_ops = 64;
    let mut e = MgrasEngine::new(cfg, Mem::default(), Some(&gpu), None).unwrap();
    for i in 0..8 {
        assert_eq!(e.write(host::RASTER + 0x40, 32, i), Write::Done);
    }
    let idle = |e: &mut MgrasEngine<Mem>| match e.read(host::STATUS, 32) {
        Read::Value(v) => v as u32 & (host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE),
        other => panic!("{other:?}"),
    };
    assert_eq!(idle(&mut e), 0, "work is queued");
    let dst = Arc::new(gpu.buffer("test destination", 1280 * 1024 * 4, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC));
    assert_eq!(e.compose_into(0, &dst, 1280), Ok(Some((1280, 1024))));
    assert_eq!(idle(&mut e), 0, "compose_into left it queued");
    assert!(e.lend_frame(0).unwrap().is_some());
    assert_eq!(idle(&mut e), 0, "lend_frame left it queued");
}

/// The DAC gamma entry `index` is `rgb`, ten bits each, written as the ADV716x takes it: red, green, blue in turn,
/// each a halfword with bits 9:2 in the first byte and 1:0 in the second.
fn gamma<E: BoardEngine>(e: &mut E, index: u32, rgb: [u32; 3]) {
    bus(e, 6, 0, 1, index);
    for v in rgb {
        bus(e, 6, 1, 2, (v >> 2) << 8 | (v & 3));
    }
}

/// Gamma entries the SPOTS' colours index (1, 2 and 3 of each channel's table), with low bits an 8-bit path loses.
const RAMP: [[u32; 3]; 3] = [[0x111, 0x222, 0x333], [0x2a5, 0x1c7, 0x3fe], [0x0f1, 0x37d, 0x155]];

fn load_gamma<E: BoardEngine>(e: &mut E) {
    for (i, v) in RAMP.iter().enumerate() {
        gamma(e, i as u32 + 1, *v);
    }
}

/// The 10-bit pixel for colour 0x010203 (red index 1, green index 2, blue index 3), red low.
fn spot10() -> u32 {
    RAMP[2][2] << 20 | RAMP[1][1] << 10 | RAMP[0][0]
}

#[test]
fn scanout10_gives_the_dacs_ten_bit_gamma_red_low_and_its_top_eight_bits_are_scanouts() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    load_gamma(&mut p.e);
    assert_eq!((p.e.head_bits(0), p.e.head_bits(1)), (10, 8));
    let (mut eight, mut ten) = (Vec::new(), Vec::new());
    assert_eq!(p.e.scanout(0, &mut eight), Ok(Some((1280, 1024))));
    assert_eq!(p.e.scanout10(0, &mut ten), Ok(Some((1280, 1024))));
    assert_eq!(p.e.scanout10(1, &mut Vec::new()), Ok(None), "one head");
    assert_eq!(ten[195 * 1280 + 96], spot10(), "the gamma's own ten bits, red in the low bits");
    let top = |c: u32| (c >> 2) & 0xff;
    for (a, t) in eight.iter().zip(&ten) {
        assert_eq!(*a, top(*t) << 16 | top(t >> 10) << 8 | top(t >> 20), "the top eight bits are scanout's");
        assert_eq!(t >> 30, 0);
    }
}

#[test]
fn the_gpu_ten_bit_frame_is_the_cpu_frame_read_back_or_composed_into_a_buffer() {
    let Some(gpu) = gpu() else { return };
    let mut p = drawn_player(Some(&gpu));
    let mut cpu = drawn_player(None);
    for e in [&mut p.e, &mut cpu.e] {
        paint(e);
        load_gamma(e);
    }
    let (mut want, mut got) = (Vec::new(), Vec::new());
    cpu.e.scanout10(0, &mut want).unwrap();
    assert_eq!(p.e.scanout10(0, &mut got), Ok(Some((1280, 1024))));
    assert_eq!(got[195 * 1280 + 96], spot10());
    assert!(got == want, "the GPU's wide frame read back differs from the CPU's");
    let stride = 1280 + 16;
    let dst = Arc::new(gpu.buffer("wide destination", (stride * 1024 * 4) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC));
    assert_eq!(p.e.compose_into10(0, &dst, stride as u32), Ok(Some((1280, 1024))));
    let words = read_words(&gpu, &dst, stride * 1024);
    for y in 0..1024 {
        assert!(words[y * stride..y * stride + 1280] == want[y * 1280..(y + 1) * 1280], "row {y}");
    }
    assert!(p.e.compose_into10(0, &dst, 100).is_err(), "a stride under the width");
    assert_eq!(p.e.compose_into10(1, &dst, stride as u32), Ok(None));
    // The 8-bit compose after it is as it was.
    let mut eight = Vec::new();
    cpu.e.scanout(0, &mut want).unwrap();
    p.e.scanout(0, &mut eight).unwrap();
    assert!(eight == want, "the narrow frame is unchanged by the wide one");
}

/// The damage marks the engine posted since the last call: (store, y0, y1).
fn damage<E: BoardEngine>(e: &mut E) -> Vec<(u64, i32, i32)> {
    let mut events = Vec::new();
    e.drain_events(&mut events, 64);
    events.into_iter().filter_map(|ev| if let Event::Damage(m) = ev { Some((m.store, m.rect.y0, m.rect.y1)) } else { None }).collect()
}

/// A VC3 register by the three-byte form that stores an index and a halfword at once.
fn vc3<E: BoardEngine>(e: &mut E, reg: u32, value: u32) {
    bus(e, 8, 0, 3, reg << 16 | value);
}

#[test]
fn a_bus_store_that_leaves_the_picture_alone_posts_no_damage() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Mem::default(), None, None).unwrap();
    cmap(&mut e, 5, 0x123456);
    gamma(&mut e, 7, [0x111, 0x222, 0x333]);
    vc3(&mut e, 0x1d, 3);
    damage(&mut e);
    let g = e.frame_generation(0);
    // Selects and addresses, the SRAM pointer, the line counter, and values written over themselves.
    bus(&mut e, 7, 1, 1, 5);
    bus(&mut e, 3, 0, 1, 9);
    bus(&mut e, 6, 0, 1, 4);
    bus(&mut e, 8, 0, 1, 0x1d);
    vc3(&mut e, 7, 0x100);
    vc3(&mut e, 0xb, 12);
    cmap(&mut e, 5, 0x123456);
    gamma(&mut e, 7, [0x111, 0x222, 0x333]);
    vc3(&mut e, 0x1d, 3);
    bus(&mut e, 20, 2, 4, 1);
    assert_eq!(damage(&mut e), vec![]);
    assert_eq!(e.frame_generation(0), g);
}

#[test]
fn a_colour_map_gamma_mode_or_table_store_that_changes_something_posts_damage_for_everything() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Mem::default(), None, None).unwrap();
    let all = vec![(DAMAGE_ANY, 0, 4095)];
    cmap(&mut e, 5, 0x123456);
    assert_eq!(damage(&mut e), all, "a colour map entry");
    gamma(&mut e, 7, [0x111, 0x222, 0x333]);
    assert_eq!(damage(&mut e), all, "a gamma entry");
    bus(&mut e, 6, 0, 1, 4);
    bus(&mut e, 6, 2, 1, 0);
    assert_eq!(damage(&mut e), all, "the pixel mask blanking the screen");
    bus(&mut e, 7, 1, 1, 0);
    bus(&mut e, 7, 3, 4, 0xFF);
    assert_eq!(damage(&mut e), all, "a buffer select");
    bus(&mut e, 7, 1, 1, 8);
    bus(&mut e, 7, 4, 4, 0x25);
    assert_eq!(damage(&mut e), all, "a main mode");
    bus(&mut e, 7, 5, 4, 0x28);
    assert_eq!(damage(&mut e), all, "an overlay mode");
    bus(&mut e, 7, 1, 1, 0);
    bus(&mut e, 7, 6, 4, 0x1234_5678);
    assert_eq!(damage(&mut e), all, "the DIB pointers");
    bus(&mut e, 7, 2, 4, 0x10);
    assert_eq!(damage(&mut e), all, "the cursor colour base");
    vc3(&mut e, 4, 0x200);
    assert_eq!(damage(&mut e), all, "the window-ID frame table's address");
    vc3(&mut e, 7, 0x300);
    bus(&mut e, 8, 3, 2, 0x8001);
    assert_eq!(damage(&mut e), all, "a word of VC3 SRAM");
}

#[test]
fn a_cursor_store_posts_damage_for_the_rows_the_glyph_covered_and_covers_and_none_while_it_is_hidden() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Mem::default(), None, None).unwrap();
    vc3(&mut e, 3, 100 + 31);
    vc3(&mut e, 2, 40 + 31);
    assert_eq!(damage(&mut e), vec![], "a hidden cursor moves nothing on the screen");
    vc3(&mut e, 0x1d, 3);
    assert_eq!(damage(&mut e), vec![(DAMAGE_DISPLAY, 100, 131)], "shown: its 32 rows");
    vc3(&mut e, 3, 300 + 31);
    assert_eq!(damage(&mut e), vec![(DAMAGE_DISPLAY, 100, 331)], "moved: the rows it left and the rows it reaches, joined");
    vc3(&mut e, 0x1d, 0xb);
    assert_eq!(damage(&mut e), vec![(DAMAGE_DISPLAY, 300, 363)], "a 64 by 64 glyph");
    vc3(&mut e, 0x1d, 0);
    assert_eq!(damage(&mut e), vec![(DAMAGE_DISPLAY, 300, 363)], "hidden: the rows it covered");
}

#[test]
fn rows_of_a_scan_source_are_the_rows_scanout_composes_while_the_rest_of_the_buffer_is_left_alone() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    let mut want = Vec::new();
    p.e.scanout(0, &mut want).unwrap();
    let src = p.e.scan_source().expect("a CPU engine has one");
    let mut out = vec![0xdead_beef; 1280 * 1024];
    assert_eq!(src.rows(190, 200, &mut out[..10]), Ok(Scan::Short(1280, 1024)), "a buffer too short for the picture");
    assert_eq!(src.rows(190, 200, &mut out), Ok(Scan::Done(1280, 1024)));
    assert!(out[190 * 1280..200 * 1280] == want[190 * 1280..200 * 1280]);
    assert!(out[..190 * 1280].iter().chain(&out[200 * 1280..]).all(|&w| w == 0xdead_beef), "the other rows are not touched");
    check(&out, 1280, "rows");
    // A change to the bus shows in the next call, and the whole picture is the same as scanout's.
    cmap(&mut p.e, 0x160, 0x778899);
    p.e.scanout(0, &mut want).unwrap();
    assert_eq!(src.rows(0, 4096, &mut out), Ok(Scan::Done(1280, 1024)));
    assert!(out == want);
}

#[test]
fn a_scan_source_composes_from_another_thread_while_the_engine_is_called() {
    let mut p = drawn_player(None);
    paint(&mut p.e);
    let src = p.e.scan_source().unwrap();
    let mut want = Vec::new();
    p.e.scanout(0, &mut want).unwrap();
    let t = std::thread::spawn(move || {
        let mut out = vec![0; 1280 * 1024];
        for _ in 0..8 {
            assert_eq!(src.rows(0, 1024, &mut out), Ok(Scan::Done(1280, 1024)));
        }
        out
    });
    for i in 0..64 {
        bus(&mut p.e, 7, 1, 1, i & 7);
        let _ = p.e.frame_generation(0);
    }
    assert!(t.join().unwrap() == want);
}

/// `save` and `rows` take `inner` and the display lock in one order: a save while another thread composes must not
/// deadlock. The watchdog aborts, since a deadlock hangs the test.
#[test]
fn a_save_while_another_thread_composes_does_not_deadlock() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Mem::default(), None, None).unwrap();
    let src = e.scan_source().unwrap();
    e.quiesce(u64::MAX).unwrap();
    let (stop, saves) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
    let (s2, v2) = (stop.clone(), saves.clone());
    std::thread::spawn(move || {
        let mut last = u64::MAX;
        while !s2.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let now = v2.load(Ordering::Relaxed);
            if now == last && !s2.load(Ordering::Relaxed) {
                eprintln!("deadlock: {now} saves");
                std::process::abort();
            }
            last = now;
        }
    });
    let s3 = stop.clone();
    let t = std::thread::spawn(move || {
        let mut out = vec![0u32; 1280 * 1024];
        while !s3.load(Ordering::Relaxed) {
            src.rows(0, 2, &mut out).unwrap();
        }
    });
    for _ in 0..1000 {
        e.save(&mut Vec::new()).unwrap();
        saves.fetch_add(1, Ordering::Relaxed);
    }
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
}
