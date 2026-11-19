//! The VPro back end driven the way a front end other than iris's board
//! would drive it: no `Board`, no test encoder. The scene is iris's golden
//! `tri_smooth`: a clear and two smooth triangles into a 128 x 128 window.
//! The state words, transform and vertices below are what the board hands
//! the back end for that scene (captured from `Board::draw_prim`), and the
//! hash is the one iris keeps in `testdata/golden/scenes.tsv`.

use sgi_odyssey::dev::odyssey::assembly::{Prim, XfState, TRIANGLES};
use sgi_odyssey::dev::odyssey::back::{Clear, DepthMap, VproBack};
use sgi_odyssey::dev::odyssey::clip::ClipRect;
use sgi_odyssey::dev::odyssey::pipe::state::{BackendWords, FragmentWords, RasterWords, ShaderWords};
use sgi_odyssey::dev::odyssey::view::DestViews;
use sgi_odyssey::wgpu::{Gpu, GpuOptions};

/// `tri_smooth` in `src/dev/odyssey/testdata/golden/scenes.tsv`.
const GOLDEN: &str = "0abd15e373a57d0f63ce5f038e4a478731820db6e44f9bacf3ca8525c6a9cae3";

/// 32 MB: the screen's planes end at 0x1a00000 (the overlay at 21 MB).
const VRAM: u64 = 1 << 25;
/// The pixel mode `Board::test_screen` sets, and the window-id plane's
/// base and stride from the scan-out words it sets.
const PIXEL_MODE: [u32; 4] = [0x2000, 0x50, 0x0040_6d50, 0];
const WID_BASE: u32 = 0x14_2000;
const WID_STRIDE: u32 = 80;
const SIDE: i32 = 128;

const TRI_A: &[f32] = &[
        -0.84375, -0.84375, 0.0, 1.0, 1.0, 0.1, 0.1, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1e-45,
        1.0, 0.1, 0.1, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.71875, -0.609375, 0.0, 1.0, 0.1, 0.9, 0.2, 1.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1e-45, 0.1, 0.9, 0.2, 1.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, -0.375, 0.796875, 0.0, 1.0, 0.1, 0.2, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        0.0, 1e-45, 0.1, 0.2, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

const TRI_B: &[f32] = &[
        0.09375, 0.09375, 0.0, 1.0, 1.0, 1.0, 1.0, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1e-45,
        1.0, 1.0, 1.0, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.953125, 0.25, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1e-45, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.484375, 0.953125, 0.0, 1.0, 1.0, 0.9, 0.1, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        0.0, 1e-45, 1.0, 0.9, 0.1, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

fn words() -> FragmentWords {
    let dest = DestViews::of(&PIXEL_MODE, WID_BASE, WID_STRIDE);
    let mode = 8388608;
    let mut clip_rect = [ClipRect { x0: 0, y0: 0, x1: 0, y1: 0 }; 4];
    clip_rect[0] = ClipRect { x0: 0, y0: 0, x1: SIDE - 1, y1: SIDE - 1 };
    FragmentWords {
        raster: RasterWords {
            mode,
            polygon_mode: 10,
            clip_box: [4095, 4095],
            clip_rect,
            clip_kept: 1,
            line_point: [128, 128],
            line_stipple: [0, 65535, 0],
            depth_func: 1,
            ..Default::default()
        },
        shader: ShaderWords { mode, alpha: 28672, ..Default::default() },
        backend: BackendWords {
            mode,
            depth_func: 1,
            write_mask: 33550336,
            stencil: 234946304,
            blend: [512, 0, 0],
            logic_op: 3,
            draw_mask: 3,
            dest,
            ..Default::default()
        },
    }
}

fn transform() -> XfState {
    XfState { viewport: [0, 0, SIDE, SIDE], ..Default::default() }
}

fn scene(back: &mut VproBack) -> String {
    let w = words();
    let xf = transform();
    let depth = DepthMap { row: [0.0, 0.0, 0.5, 0.5], divisor: Some([-0.0, -0.0, -0.0, 1.0]), slack: 9.5367431640625e-7 };

    back.clear(
        &w.backend.dest,
        &w.raster.clip_rect[..1],
        &Clear { rect: (0, 0, 1279, 1023), rgba: Some([0.15, 0.2, 0.3, 1.0]), depth24: Some(0xff_ffff), stencil: Some((0, 255)) },
    );
    for tri in [TRI_A, TRI_B] {
        let mut prim = Prim::new(TRIANGLES, 3, tri, &xf);
        back.draw(&w, &depth, &mut prim).expect("draw");
    }
    back.sync().expect("sync");
    back.planes(&w.backend.dest, SIDE, SIDE).hash()
}

#[test]
fn cpu_reproduces_irises_golden() {
    let mut back = VproBack::new(VRAM, None).expect("back end");

    assert_eq!(scene(&mut back), GOLDEN);
}

#[test]
fn gpu_reproduces_irises_golden() {
    let gpu = match Gpu::new(&GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let mut back = VproBack::new(VRAM, Some(&gpu)).expect("back end");

    assert_eq!(scene(&mut back), GOLDEN);
    assert_eq!(back.fault(), None);
    let (_, records) = back.gpu_counts().expect("the GPU unit");
    assert!(records >= 3, "the GPU drew {records} records");
}
