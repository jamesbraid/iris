//! A fill-rate benchmark for the integer drawing path.
//!
//! Each scene draws into a store of the managed area's size through the same
//! calls the back end makes for a textport or X command, with no guest and no
//! framing in the way, so the numbers are the fills'. Run it from a release
//! build, one test thread:
//!
//!     cargo test --release --lib kona::back::bench -- --ignored --nocapture --test-threads=1
//!
//! `BENCH_SECS` sets the time each scene takes (default half a second). The
//! figure is the best of five rounds, in megapixels of the requested region
//! per second.

use super::raster::{FillRule, Surface};
use super::store::PixelStore;
use super::token::x::XState;
use std::time::{Duration, Instant};

const W: u32 = 1280;
const H: u32 = 1024;
const GC_COPY: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];

fn pack(a: i32, b: i32) -> u32 {
    (a as u32) << 16 | (b as u32 & 0xffff)
}

fn secs() -> Duration {
    Duration::from_secs_f64(std::env::var("BENCH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5))
}

/// Run `f` (one call draws `pixels` pixels) for `secs()`, five rounds; the best
/// rate in megapixels a second.
fn rate(pixels: u64, mut f: impl FnMut()) -> f64 {
    let mut best: f64 = 0.0;
    for _ in 0..5 {
        let (start, mut calls) = (Instant::now(), 0u64);
        while start.elapsed() < secs() / 5 {
            f();
            calls += 1;
        }
        best = best.max((calls * pixels) as f64 / start.elapsed().as_secs_f64() / 1e6);
    }
    best
}

/// Whether any pixel of the store was drawn: a scene that draws nothing
/// would report a fast rate for no work.
fn drawn(store: &PixelStore) -> bool {
    (0..H).any(|y| (0..W).any(|x| store.read(x, y, 1) != 0))
}

fn x_scene(name: &str, setup: impl FnOnce(&mut XState), box_words: [u32; 2]) {
    let store = PixelStore::new(W, H);
    let surface = Surface::new(&store, 1);
    let mut x = XState::default();
    x.command(0x129, &GC_COPY);
    setup(&mut x);
    let (w, h) = ((box_words[1] >> 16) as i64 - (box_words[0] >> 16) as i64, (box_words[1] as u16 as i64) - (box_words[0] as u16 as i64));
    // One draw first: an operation like xor undoes itself every second time, so
    // the store is only checked after the first.
    x.draw_command(&surface, 0x154, &box_words);
    assert!(drawn(&store), "{name} drew nothing");
    let mpix = rate((w * h) as u64, || {
        x.draw_command(&surface, 0x154, &box_words);
    });
    println!("{name:<44} {mpix:>9.1} Mpix/s");
}

#[test]
#[ignore]
fn fill_rate() {
    let full = [pack(0, 0), pack(W as i32, H as i32)];
    x_scene("x box, solid, whole screen", |_| {}, full);
    x_scene("x box, solid, 64 x 64", |_| {}, [pack(100, 100), pack(164, 164)]);
    x_scene(
        "x box, two clip rectangles",
        |x| {
            x.command(0x145, &[2, 0, 0, W as u32 / 2 - 8]);
            x.command(0x12b, &[H, W as u32 / 2 + 8, 0, W, H]);
        },
        full,
    );
    x_scene(
        "x box, 8 x 8 stipple, whole screen",
        |x| {
            x.command(0x13d, &[1, 8, 8]);
            x.stipple.rows(&[0xaa00_0000, 0x5500_0000, 0xaa00_0000, 0x5500_0000, 0xf000_0000, 0x0f00_0000, 0xf000_0000, 0x0f00_0000]);
            x.command(0x13c, &[1]);
        },
        full,
    );
    x_scene(
        "x box, xor (reads the destination)",
        |x| {
            let mut gc = GC_COPY;
            gc[9] = 12; // gx 6, xor
            x.command(0x129, &gc);
        },
        full,
    );

    let store = PixelStore::new(W, H);
    let surface = Surface::new(&store, 1);
    let mpix = rate((W * H) as u64, || surface.fill_box(FillRule::OmitLeftBottom, 0, 0, W as i64, H as i64, 7));
    assert!(drawn(&store), "the textport box drew nothing");
    println!("{:<44} {mpix:>9.1} Mpix/s", "textport box, whole screen");
    // 16 x 16 glyphs at scattered positions, half the bits set.
    let bits: Vec<u32> = (0..8).map(|i| 0xa5a5_5a5a ^ (i * 0x0101_0101)).collect();
    let mut seed = 1u64;
    let mut place = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as i64 % (W as i64 - 16), (seed >> 11) as i64 % (H as i64 - 16))
    };
    let mpix = rate(16 * 16 * 64, || {
        for _ in 0..64 {
            let (x, y) = place();
            surface.bitmap(x, y, 16, 16, &bits, 9);
        }
    });
    assert!(drawn(&store), "the glyphs drew nothing");
    println!("{:<44} {mpix:>9.1} Mpix/s", "textport bitmap, 16 x 16 glyphs");
}
