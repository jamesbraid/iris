//! The KONA raster back end driven the way a front end other than iris's
//! board would drive it: window-space triangles and clears into `KonaBack`,
//! with no host interface, command decoder or display.
//!
//! The scene is iris's own 150-triangle golden
//! (`back/golden/mod.rs`, `a_tiled_run_is_bit_exact_with_the_inline_one`):
//! the same generator, state and clear, and the store hash iris pins.

use sgi_kona::dev::kona::api::{KonaBack, Tri};
use sgi_kona::dev::kona::back::raster::ops::Compare;
use sgi_kona::dev::kona::back::raster::{ClearReq, DrawState, Rect, WVertex};
use sgi_kona::wgpu::{Gpu, GpuOptions};

/// `SCENE_150` in iris's `back/golden/mod.rs`.
const SCENE_150: u64 = 0xa949_d8e0_9bb7_4571;
const N: u32 = 64;
const TEXTURE_BYTES: u64 = 1 << 20;

fn state() -> DrawState {
    let full = Rect { x0: 0, y0: 0, x1: N as i32, y1: N as i32 };
    DrawState { clip: full, viewport: full, colour: [1, 0], colours: 1, depth_word: 3, depth_bits: 32, depth_test: true, depth_func: Compare::Lequal, ..DrawState::default() }
}

fn v(x: f32, y: f32, c: [u16; 4], z: u32) -> WVertex {
    WVertex { x: (x * 256.0) as i32, y: (y * 256.0) as i32, z, c, t: [[0.0; 4]; 2], d: 0.0, fog: [0.0; 2] }
}

fn scene() -> Vec<Tri> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |m: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as u32 % m
    };
    (0..150)
        .map(|_| {
            let p = |next: &mut dyn FnMut(u32) -> u32| (next(90) as f32 - 12.5, next(90) as f32 - 12.5);
            let c = |next: &mut dyn FnMut(u32) -> u32| [next(8192) as u16, next(8192) as u16, next(8192) as u16, 8191];
            let z = next(0xffff_ff00);
            let (a, b, d) = (p(&mut next), p(&mut next), p(&mut next));
            let (ca, cb, cd) = (c(&mut next), c(&mut next), c(&mut next));
            [v(a.0, a.1, ca, z), v(b.0, b.1, cb, z.saturating_add(next(1 << 20))), v(d.0, d.1, cd, z)]
        })
        .collect()
}

fn run(mut back: KonaBack) -> u64 {
    let ds = state();
    back.clear(&ds, &ClearReq { rect: ds.clip, colour: Some(0xff10_2030), depth: Some(0xffff_ff00), ..Default::default() });
    for t in scene() {
        back.draw(&ds, &[t]);
    }
    back.drain().expect("drain");
    back.checksum()
}

#[test]
fn cpu_reproduces_irises_golden_inline_and_tiled() {
    for (threads, tile) in [(0, 32), (1, 32), (3, 16), (4, 64), (2, 8)] {
        let back = KonaBack::new(N, N, TEXTURE_BYTES, tile, threads, None).expect("back end");

        assert_eq!(run(back), SCENE_150, "{threads} workers, {tile}-pixel tiles");
    }
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
    let back = KonaBack::new(N, N, TEXTURE_BYTES, 32, 0, Some(&gpu)).expect("back end");

    assert!(back.is_gpu());
    assert_eq!(run(back), SCENE_150);
}
