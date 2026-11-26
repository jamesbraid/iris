//! Composed-frame pins for VPro, with no guest.
//!
//! A golden scene (`scenes.rs`) draws into a v12 board's planes; the board's
//! own scan-out (`Frame::snapshot` + `Frame::compose`, what the display thread
//! runs) turns the 1280 x 1024 screen into pixels, and the pixels are hashed.
//! The plane hashes in `scenes.tsv` stop at the window; this goes through the
//! window-id lookup, the display mode, the LUT and the gamma ramp as well.
//!
//! The executor is the board's: `VPRO_GPU=1 cargo test ... frame_pins` puts
//! every test board on the GPU unit and must give the same hashes.

use super::scenes;
use crate::dev::odyssey::board::Board;
use crate::dev::odyssey::frame::Frame;
use crate::test_frame_pin::frame_hash;

/// The frame this board's scan-out composes, as (width, height, 0x00RRGGBB
/// words): `Frame::snapshot` composes in that order, and the pins were taken
/// over it.
fn compose(b: &Board) -> (u32, u32, Vec<u32>) {
    let regs = b.regs().read();
    let frame = Frame::snapshot(&regs, b.front.lock().displayed_buffer).expect("the scan-out is set up");
    let (w, h) = (frame.width, frame.height);
    let mut px = vec![0u32; w as usize * h as usize];
    frame.compose(b.vram.ptr_shared(), &regs, &mut px, w as usize, h as usize);
    (w, h, px)
}

/// The identity gamma ramp the X server loads; with the LUT empty every
/// colour scans out black.
fn load_gamma(b: &mut Board) {
    use crate::dev::odyssey::consts::ODSY_GAMMA_BASE;
    for i in 0..0x600u32 {
        let v = (i * 0x3ff / 0x5ff) & 0x3ff;
        b.regs().set_lut((ODSY_GAMMA_BASE + i) as usize, v << 20 | v << 10 | v);
    }
}

fn pin(scene: &str) -> String {
    let mut c = scenes::Ctx::new();
    assert!(scenes::run_scene(scene, &mut c), "no scene {scene}");
    c.board.gpu_sync();
    load_gamma(&mut c.board);
    let (w, h, px) = compose(&c.board);
    let blank = {
        let mut c = scenes::Ctx::new();
        load_gamma(&mut c.board);
        compose(&c.board).2
    };
    let drawn = px.iter().zip(&blank).filter(|(a, b)| a != b).count();
    assert!(drawn >= 200, "scene {scene} changed only {drawn} screen pixels");
    eprintln!("vpro {scene}: {w}x{h}, {drawn} px differ from the blank screen");
    frame_hash(w as usize, h as usize, w as usize, &px)
}

const PINS: &[(&str, &str)] = &[
    ("tri_flat", "b026d871181d66698c13ba31437d98c2fad2e30b05a40eff3af585d9999e0b7f"),
    ("depth_less", "2050297367ba99b7e4f17735779066c3ede5b6f28db597911a0233834f6c09bd"),
    ("light_lit_blended_textured", "61d52b551368457d82640746151a1a7f5b1c1b8fabeb2328753ce197da42df7c"),
];

#[test]
fn vpro_composed_frames_are_pinned() {
    let mut bad = vec![];
    for (scene, want) in PINS {
        let got = pin(scene);
        assert_eq!(got, pin(scene), "{scene}: two runs");
        eprintln!("vpro frame hash {scene} {got}");
        if got != *want {
            bad.push(*scene);
        }
    }
    assert!(bad.is_empty(), "moved: {bad:?}");
}
