//! Parity of `fragment.wgsl` with the CPU raster path: each function runs over random inputs on
//! the GPU and on the CPU, and the bits are compared. Needs a GPU (WGPU_BACKEND=gl picks Mesa's
//! llvmpipe, vulkan the discrete card); returns early without one.

use std::sync::Arc;

use super::FRAGMENT;
use crate::dev::kona::back::gl::pixels::accumulate_cpu;
use crate::dev::kona::back::raster::fog::{self, Fog};
use crate::dev::kona::back::raster::multisample::Samples;
use crate::dev::kona::back::raster::pixel::{run as raster_run, sample_get, sample_set, Planes};
use crate::dev::kona::back::raster::setup::{Plane, Tri, TriJob};
use crate::dev::kona::back::raster::smooth::{self, Cover, MAX_EDGES};
use crate::dev::kona::back::raster::{DrawState, Rect};
use crate::dev::kona::back::store::{PixelStore, Store};
use crate::dev::kona::back::tiles::RasterJob;
use crate::dev::kona::defs::{gl, pixel};
use crate::wgpu::device::{Gpu, GpuOptions};
use crate::wgpu::shader::{Fragment as Frag, Source};
use crate::wgpu::library;

const KERNEL: Frag = Frag::new(
    "fragment_test.wgsl",
    r#"
#include "kona_fragment.wgsl"

struct Case { w: array<u32, 32> }
struct Out { w: array<u32, 8> }

@group(0) @binding(0) var<storage, read> ins: array<Case>;
@group(0) @binding(1) var<storage, read_write> outs: array<Out>;
@group(0) @binding(2) var<storage, read> zero: array<u32>;

fn bf(x: u32) -> f32 {
    return bitcast<f32>(x);
}

fn fb(x: f32) -> u32 {
    return bitcast<u32>(x);
}

@compute @workgroup_size(64)
fn fog_main(@builtin(global_invocation_id) g: vec3<u32>) {
    let n = g.x;
    if n >= arrayLength(&ins) {
        return;
    }
    fnz = zero[0];
    var w = ins[n].w;
    var o: array<u32, 8>;
    o[0] = fb(fog_factor(w[0], bf(w[1]), bf(w[2]), bf(w[3])));
    o[1] = fb(exp_neg(bf(w[12])));
    let a = fog_apply(vec4<u32>(w[4], w[5], w[6], w[7]), vec3<f32>(bf(w[8]), bf(w[9]), bf(w[10])), bf(w[11]));
    o[2] = a.x;
    o[3] = a.y;
    o[4] = a.z;
    o[5] = a.w;
    outs[n].w = o;
}

@compute @workgroup_size(64)
fn smooth_main(@builtin(global_invocation_id) g: vec3<u32>) {
    let n = g.x;
    if n >= arrayLength(&ins) {
        return;
    }
    fnz = zero[0];
    var w = ins[n].w;
    var o: array<u32, 8>;
    let kind = w[0];
    if kind == 0u {
        o[0] = fb(smooth_quantise(bf(w[1])));
    } else if kind == 1u {
        o[0] = fb(half_plane(bf(w[1]), bf(w[2]), bf(w[3])));
    } else if kind == 2u {
        o[0] = fb(smooth_disc(bf(w[1]), bf(w[2]), bf(w[3]), bitcast<i32>(w[4]), bitcast<i32>(w[5])));
    } else {
        var planes: array<vec4<f32>, 8>;
        for (var k = 0u; k < w[1]; k++) {
            planes[k] = vec4<f32>(bf(w[4u + 3u * k]), bf(w[5u + 3u * k]), bf(w[6u + 3u * k]), 0.0);
        }
        o[0] = fb(smooth_edges(w[1], &planes, bf(w[2]), bf(w[3])));
    }
    outs[n].w = o;
}

@compute @workgroup_size(64)
fn ms_main(@builtin(global_invocation_id) g: vec3<u32>) {
    let n = g.x;
    if n >= arrayLength(&ins) {
        return;
    }
    fnz = zero[0];
    var w = ins[n].w;
    var o: array<u32, 8>;
    var run = array<u32, 6>(w[1], w[2], w[3], w[4], w[5], w[6]);
    let kind = w[0];
    if kind == 0u {
        o[0] = ms_sample_get(&run, w[7], w[8], w[9]);
    } else if kind == 1u {
        ms_sample_set(&run, w[7], w[8], w[9], w[10]);
        for (var k = 0u; k < 6u; k++) {
            o[k] = run[k];
        }
    } else if kind == 2u {
        o[0] = ms_allowed(w[1], w[2], bitcast<i32>(w[3]));
    } else if kind == 3u {
        o[0] = ms_alpha_samples(w[1], w[2]);
    } else {
        o[0] = ms_write_resolve(&run, w[7], w[8], w[9], vec4<u32>(w[10], w[11], w[12], w[13]));
        for (var k = 0u; k < 6u; k++) {
            o[1u + k] = run[k];
        }
    }
    outs[n].w = o;
}

@compute @workgroup_size(64)
fn accum_main(@builtin(global_invocation_id) g: vec3<u32>) {
    let n = g.x;
    if n >= arrayLength(&ins) {
        return;
    }
    fnz = zero[0];
    var w = ins[n].w;
    var o: array<u32, 8>;
    var acc = vec4<f32>(bf(w[1]), bf(w[2]), bf(w[3]), bf(w[4]));
    var src = vec4<f32>(0.0);
    if w[6] != 0u {
        src = acc_components8(w[5]);
    }
    var word = w[7];
    for (var q = 0u; q < w[9]; q++) {
        let op = w[10u + 2u * q];
        let v = bf(w[11u + 2u * q]);
        if op == ACCUM_RETURN {
            word = acc_return(acc, w[0], v, word, w[8]);
        } else {
            acc_pass(&acc, w[0], op, v, src);
        }
    }
    o[0] = fb(acc.x);
    o[1] = fb(acc.y);
    o[2] = fb(acc.z);
    o[3] = fb(acc.w);
    o[4] = word;
    outs[n].w = o;
}

@compute @workgroup_size(64)
fn stipple_main(@builtin(global_invocation_id) g: vec3<u32>) {
    let n = g.x;
    if n >= arrayLength(&ins) {
        return;
    }
    fnz = zero[0];
    var w = ins[n].w;
    var o: array<u32, 8>;
    if w[0] == 0u {
        o[0] = u32(stipple_line(w[1], w[2], bf(w[3]), bf(w[4]), bf(w[5]), bf(w[6]), bf(w[7])));
    } else {
        o[0] = u32(stipple_poly(w[1], bitcast<i32>(w[2]), bitcast<i32>(w[3])));
    }
    outs[n].w = o;
}
"#,
);

type Case = [u32; 32];
type Out = [u32; 8];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn u(&mut self, n: u32) -> u32 {
        (self.next() >> 20) as u32 % n
    }

    fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * ((self.next() >> 40) as f32 / (1u32 << 24) as f32)
    }

    fn i(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.u((hi - lo + 1) as u32) as i32
    }
}

fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default(), Box::new(|_| {})) {
        Ok(g) => {
            eprintln!("fragment parity on {}", g.caps);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// Run entry point `entry` of the test kernel over `cases`.
fn run(gpu: &Arc<Gpu>, entry: &str, cases: &[Case]) -> Vec<Out> {
    let src = Source::new("fragment test").library(library::FLOAT).library(FRAGMENT).body(KERNEL);
    let bind = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    };
    let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("fragment"), entries: &[bind(0, true), bind(1, false), bind(2, true)] });
    let pipeline = src.compute_pipeline(&gpu.device, entry, &[&layout]).unwrap_or_else(|e| panic!("{e}"));
    let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
    let inb = gpu.buffer("fragment in", (cases.len() * 128) as u64, storage);
    let outb = gpu.buffer("fragment out", (cases.len() * 32) as u64, storage);
    let zero = gpu.buffer("fragment zero", 16, storage);
    gpu.write(&inb, 0, bytemuck::cast_slice(cases));
    gpu.write(&zero, 0, &[0u8; 16]);
    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("fragment"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: inb.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: outb.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: zero.as_entire_binding() },
        ],
    });
    let mut enc = gpu.encoder("fragment");
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("fragment"), timestamp_writes: None });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups((cases.len() as u32).div_ceil(64), 1, 1);
    }
    gpu.submit(enc);
    let bytes = gpu.read_vec(&outb, &[(0, outb.size())]).unwrap_or_else(|e| panic!("{e}"));
    gpu.check().unwrap_or_else(|e| panic!("{e}"));
    bytemuck::cast_slice(&bytes).to_vec()
}

/// Collected mismatches, printed together.
#[derive(Default)]
struct Bad(Vec<String>, usize);

impl Bad {
    fn check(&mut self, what: &str, input: impl FnOnce() -> String, got: u32, want: u32) {
        self.1 += 1;
        if got != want {
            if self.0.len() < 20 {
                self.0.push(format!("{what} {}: gpu {got:#010x}, cpu {want:#010x}", input()));
            } else {
                self.0.push(String::new());
            }
        }
    }

    fn done(self, group: &str) {
        let n = self.0.len();
        let shown: Vec<_> = self.0.into_iter().filter(|s| !s.is_empty()).collect();
        assert!(n == 0, "{group}: {n} of {} comparisons differ:\n{}", self.1, shown.join("\n"));
        eprintln!("{group}: {} comparisons bit-exact", self.1);
    }
}

fn fl(x: f32) -> u32 {
    x.to_bits()
}

#[test]
fn fog_matches_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0xf09_f09);
    let mut cases = Vec::new();
    let mut want = Vec::new();
    for n in 0..40000 {
        let mode = [gl::LINEAR, gl::EXP, gl::EXP2][n % 3];
        let (density, start, end) = (rng.f(0.0, 3.0), rng.f(0.0, 20.0), rng.f(0.0, 60.0));
        let colour = [rng.f(0.0, 1.0), rng.f(0.0, 1.0), rng.f(0.0, 1.0)];
        let fog = Fog::new(true, mode, density, start, if n % 50 == 7 { start } else { end }, colour).unwrap();
        // Coordinates around the interesting range of each curve, and its ends.
        let c = match (n / 3) % 8 {
            0 => start,
            1 => end,
            2 => 0.0,
            _ => rng.f(0.0, if mode == gl::LINEAR { end.max(1.0) * 1.2 } else { 25.0 / density.max(0.05) }),
        };
        let f = [0.0, 1.0, rng.f(0.0, 1.0), rng.f(0.0, 1.0)][n % 4];
        let x = match n % 10 {
            0 => -3.0,
            1 => 0.0,
            2 => 12.0,
            3 => 11.999999,
            4 => f32::NAN,
            5 => f32::INFINITY,
            6 => 1.0e-30,
            _ => rng.f(0.0, 13.0),
        };
        let src = [rng.u(8192), rng.u(8192), rng.u(8192), rng.u(8192)];
        let mut w = [0u32; 32];
        w[0] = match fog.curve {
            fog::Curve::Linear => fog::LINEAR,
            fog::Curve::Exp => fog::EXP,
            fog::Curve::Exp2 => fog::EXP2,
        } as u32;
        w[1] = fl(fog.a);
        w[2] = fl(fog.b);
        w[3] = fl(c);
        w[4..8].copy_from_slice(&src);
        w[8..11].copy_from_slice(&fog.colour.map(fl));
        w[11] = fl(f);
        w[12] = fl(x);
        let factor = match fog.curve {
            fog::Curve::Linear => fog.factor::<{ fog::LINEAR }>(c),
            fog::Curve::Exp => fog.factor::<{ fog::EXP }>(c),
            fog::Curve::Exp2 => fog.factor::<{ fog::EXP2 }>(c),
        };
        let applied = fog.apply(src, f);
        want.push((factor, fog::exp_neg(x), applied, c, x, f));
        cases.push(w);
    }
    let outs = run(&gpu, "fog_main", &cases);
    let mut bad = Bad::default();
    for (n, (o, (factor, e, applied, c, x, f))) in outs.iter().zip(&want).enumerate() {
        bad.check("fog_factor", || format!("case {n} mode {} a {} b {} c {c}", cases[n][0], f32::from_bits(cases[n][1]), f32::from_bits(cases[n][2])), o[0], fl(*factor));
        bad.check("exp_neg", || format!("x {x}"), o[1], fl(*e));
        for k in 0..4 {
            bad.check("fog_apply", || format!("case {n} channel {k} f {f}"), o[2 + k], applied[k]);
        }
    }
    let partial = want.iter().filter(|w| w.0 > 0.0 && w.0 < 1.0).count();
    assert!(partial > 5000, "only {partial} partial fog factors");
    bad.done("fog");
}

#[test]
fn smooth_coverage_matches_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0x5300_7);
    let mut cases = Vec::new();
    let mut want = Vec::new();
    let mut label = Vec::new();
    // quantise
    for n in 0..8000 {
        let cov = match n % 8 {
            0 => 0.0,
            1 => 1.0,
            2 => 0.5 / 255.0,
            3 => rng.u(256) as f32 / 255.0 + 0.5 / 255.0,
            _ => rng.f(0.0, 1.0),
        };
        let mut w = [0u32; 32];
        w[1] = fl(cov);
        cases.push(w);
        want.push(fl(smooth::quantise(cov)));
        label.push(format!("quantise({cov})"));
    }
    // half_plane: random unit normals and the axis-aligned ones
    for n in 0..30000 {
        let (nx, ny) = match n % 6 {
            0 => (1.0, 0.0),
            1 => (0.0, -1.0),
            2 => (-1.0, 0.0),
            _ => {
                let a = rng.f(0.0, std::f32::consts::TAU);
                (a.cos(), a.sin())
            }
        };
        let s = if n % 7 == 0 { [0.0, 0.5, -0.5, 0.25][n / 7 % 4] } else { rng.f(-1.0, 1.0) };
        let mut w = [0u32; 32];
        w[0] = 1;
        w[1] = fl(nx);
        w[2] = fl(ny);
        w[3] = fl(s);
        cases.push(w);
        want.push(fl(smooth::half_plane(nx, ny, s)));
        label.push(format!("half_plane({nx}, {ny}, {s})"));
    }
    // disc, through the CPU's public coverage()
    for _ in 0..20000 {
        let (cx, cy, r) = (rng.f(0.0, 20.0), rng.f(0.0, 20.0), rng.f(0.05, 6.0));
        let (x, y) = (cx as i32 + rng.i(-8, 8), cy as i32 + rng.i(-8, 8));
        let t = Tri { tie: [true; 3], cover: Cover::Disc { x: cx, y: cy, r }, ..Tri::default() };
        let mut w = [0u32; 32];
        w[0] = 2;
        w[1] = fl(cx);
        w[2] = fl(cy);
        w[3] = fl(r);
        w[4] = x as u32;
        w[5] = y as u32;
        cases.push(w);
        want.push(fl(smooth::coverage(&t, x, y)));
        label.push(format!("disc({cx}, {cy}, {r}) at ({x}, {y})"));
    }
    // edges: a polygon's planes, tuned so most pixels are partly covered
    for _ in 0..20000 {
        let n = 1 + rng.u(MAX_EDGES as u32) as usize;
        let (ox, oy) = (rng.i(-50, 50), rng.i(-50, 50));
        let (x, y) = (ox + rng.i(0, 15), oy + rng.i(0, 15));
        let (i, j) = ((x - ox) as f32, (y - oy) as f32);
        let mut plane = [[0.0f32; 3]; MAX_EDGES];
        let mut w = [0u32; 32];
        w[0] = 3;
        w[1] = n as u32;
        w[2] = fl(i);
        w[3] = fl(j);
        for k in 0..n {
            let a = rng.f(0.0, std::f32::consts::TAU);
            let (nx, ny) = (a.cos(), a.sin());
            let d = -(nx * i + ny * j) + rng.f(-0.9, 0.9) + if rng.u(4) == 0 { 1.0 } else { 0.0 };
            plane[k] = [nx, ny, d];
            w[4 + 3 * k] = fl(nx);
            w[5 + 3 * k] = fl(ny);
            w[6 + 3 * k] = fl(d);
        }
        let t = Tri { origin: [ox, oy], cover: Cover::Edges { inner: [false; 3], n: n as u8, plane }, ..Tri::default() };
        cases.push(w);
        want.push(fl(smooth::coverage(&t, x, y)));
        label.push(format!("edges n {n} at ({x}, {y}) origin ({ox}, {oy})"));
    }
    let outs = run(&gpu, "smooth_main", &cases);
    let mut bad = Bad::default();
    for (n, o) in outs.iter().enumerate() {
        bad.check("smooth", || label[n].clone(), o[0], want[n]);
    }
    // Partial coverage is common in the inputs, or the test says little about the ramps.
    let partial = want.iter().filter(|&&b| f32::from_bits(b) > 0.0 && f32::from_bits(b) < 1.0).count();
    assert!(partial > 15000, "only {partial} partial coverages");
    bad.done("smooth");
}

/// A pixel store with `n` pixels in a row and words `0..words` allocated.
fn store(n: u32, words: usize) -> PixelStore {
    let s = PixelStore::new(n, 1);
    (0..words).for_each(|w| s.ensure(w));
    s
}

#[test]
fn multisample_bits_match_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0x00a5_a5);
    let n = 30000u32;
    let s = store(n, 8);
    let mut cases = Vec::new();
    let mut want: Vec<Vec<u32>> = Vec::new();
    let mut label = Vec::new();
    for idx in 0..n {
        let kind = idx % 5;
        let mut w = [0u32; 32];
        w[0] = kind;
        let mut run = [0u32; 6];
        run.iter_mut().for_each(|v| *v = rng.next() as u32);
        w[1..7].copy_from_slice(&run);
        match kind {
            0 | 1 => {
                // Colour at 24 bits, depth at 24 or 16, stencil at 8; the layouts' sizes.
                let (stride, width, count) = [(24u32, 24u32, 8u32), (24, 24, 4), (16, 16, 8), (16, 16, 4), (8, 8, 8), (24, 24, 4)][rng.u(6) as usize];
                let k = rng.u(count);
                let v = rng.next() as u32;
                (w[7], w[8], w[9], w[10]) = (k, stride, width, v);
                for (wd, val) in run.iter().enumerate() {
                    s.write(idx, 0, wd, *val);
                }
                let arrays: Vec<_> = (0..6).map(|wd| s.array(wd).unwrap()).collect();
                if kind == 0 {
                    want.push(vec![sample_get(&arrays, idx as usize, k as usize, stride, width)]);
                } else {
                    sample_set(&arrays, idx as usize, k as usize, stride, width, v);
                    want.push((0..6).map(|wd| s.read(idx, 0, wd)).collect());
                }
                label.push(format!("sample k {k} stride {stride} width {width} run {run:x?}"));
            }
            2 => {
                let count = [4u8, 8][rng.u(2) as usize];
                let (cover, x) = (rng.next() as u32, rng.i(-20, 20));
                (w[1], w[2], w[3]) = (cover, count as u32, x as u32);
                let ms = Samples { count, at: [[0; 2]; 8], cover, alpha_to_mask: false, alpha_to_one: false, colour: (0, 0), depth: (0, 0), stencil: (0, 0) };
                want.push(vec![ms.allowed(x)]);
                label.push(format!("allowed cover {cover:#x} count {count} x {x}"));
            }
            3 => {
                let count = [4u8, 8][rng.u(2) as usize];
                let alpha = if rng.u(2) == 0 { [0, 1, 4095, 4096, 8191, 8192, 20000][rng.u(7) as usize] } else { rng.u(8192) };
                (w[1], w[2]) = (alpha, count as u32);
                let ms = Samples { count, at: [[0; 2]; 8], cover: !0, alpha_to_mask: true, alpha_to_one: false, colour: (0, 0), depth: (0, 0), stencil: (0, 0) };
                want.push(vec![ms.alpha_samples(alpha)]);
                label.push(format!("alpha_samples {alpha} count {count}"));
            }
            _ => {
                // The colour half of a multisampled fragment, against shade_samples itself.
                let count = [4u32, 8][rng.u(2) as usize];
                let words = (count * 24 / 32) as usize;
                let live = 1 + rng.u((1 << count) - 1);
                let src = [rng.u(8192), rng.u(8192), rng.u(8192), rng.u(8192)];
                // The sample words start at word 1; the pixel's own word is 0.
                let mut run = [0u32; 6];
                (0..words).for_each(|k| run[k] = rng.next() as u32);
                let main = rng.next() as u32;
                s.write(idx, 0, 0, main);
                for k in 0..6 {
                    s.write(idx, 0, 1 + k, if k < words { run[k] } else { 0 });
                }
                w[1..7].copy_from_slice(&run);
                (w[7], w[8], w[9]) = (count, 24, live);
                w[10..14].copy_from_slice(&src);
                let mut ds = DrawState::default();
                ds.colour = [0, 0];
                ds.colours = 1;
                ds.samples = Some(Samples { count: count as u8, at: [[0; 2]; 8], cover: !0, alpha_to_mask: false, alpha_to_one: false, colour: (1, words as u32), depth: (0, 0), stencil: (0, 0) });
                let planes = Planes::new(&s, &ds);
                planes.shade_samples(&ds, idx as usize, src, |_| 0, live);
                let mut out = vec![s.read(idx, 0, 0)];
                out.extend((0..6).map(|k| s.read(idx, 0, 1 + k)));
                want.push(out);
                label.push(format!("resolve count {count} live {live:#x} src {src:?} run {run:x?}"));
            }
        }
        cases.push(w);
    }
    let outs = run(&gpu, "ms_main", &cases);
    let mut bad = Bad::default();
    for (i, (o, want)) in outs.iter().zip(&want).enumerate() {
        // The unused words of an eight-sample run's resolve beyond what the count stores are zero on both sides.
        for (k, &v) in want.iter().enumerate() {
            bad.check(&format!("multisample kind {} word {k}", cases[i][0]), || label[i].clone(), o[k], v);
        }
    }
    bad.done("multisample");
}

#[test]
fn accumulation_matches_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0xacc_acc);
    const PIXELS: u32 = 64;
    let mut cases = Vec::new();
    let mut want: Vec<[u32; 5]> = Vec::new();
    let mut label = Vec::new();
    let value = |rng: &mut Rng| -> f32 {
        match rng.u(8) {
            0 => 0.0,
            1 => -0.0,
            2 => 0.5,
            3 => -1.0,
            4 => rng.f(-1.0, 1.0) * 1.0e4,
            _ => rng.f(-2.0, 2.0),
        }
    };
    for config in 0..300 {
        let nwords = [3usize, 4][config % 2];
        let mask = rng.u(16);
        let npass = 1 + rng.u(4) as usize;
        let ops = [pixel::ACCUM, pixel::ACCUM_LOAD, pixel::ACCUM_RETURN, pixel::ACCUM_MULT, pixel::ACCUM_ADD];
        let passes: Vec<(u32, f32)> = (0..npass).map(|_| (ops[rng.u(5) as usize], value(&mut rng))).collect();
        let has_colour = rng.u(5) != 0;
        let store = Store::new(PIXELS, 1);
        // The accumulation words 20.., the colour buffer read from word 5 and the one drawn to, 6.
        for w in (20..24).chain([5, 6]) {
            store.cpu().ensure(w);
        }
        let mut ds = DrawState::default();
        ds.clip = Rect { x0: 0, y0: 0, x1: PIXELS as i32, y1: 1 };
        ds.colour = [6, 0];
        ds.colours = 1;
        ds.colour_mask = [mask & 1 != 0, mask & 2 != 0, mask & 4 != 0, mask & 8 != 0];
        let mut init = Vec::new();
        for x in 0..PIXELS {
            let acc: [f32; 4] = std::array::from_fn(|_| value(&mut rng));
            let (colour, old) = (rng.next() as u32, rng.next() as u32);
            for (k, a) in acc.iter().enumerate().take(nwords) {
                store.cpu().write(x, 0, 20 + k, a.to_bits());
            }
            store.cpu().write(x, 0, 5, colour);
            store.cpu().write(x, 0, 6, old);
            init.push((acc, colour, old));
        }
        accumulate_cpu(&store, &ds, 20..20 + nwords as u32, has_colour.then_some(5), &passes);
        for (x, (acc, colour, old)) in init.iter().enumerate() {
            let mut w = [0u32; 32];
            w[0] = nwords as u32;
            for k in 0..4 {
                w[1 + k] = acc[k].to_bits();
            }
            (w[5], w[6], w[7], w[8], w[9]) = (*colour, has_colour as u32, *old, mask, npass as u32);
            for (q, (op, v)) in passes.iter().enumerate() {
                w[10 + 2 * q] = *op;
                w[11 + 2 * q] = v.to_bits();
            }
            cases.push(w);
            let mut o = [0u32; 5];
            for k in 0..4 {
                // Words the pixel has none of are not stored; the shader leaves them as given.
                o[k] = if k < nwords { store.cpu().read(x as u32, 0, 20 + k) } else { acc[k].to_bits() };
            }
            o[4] = store.cpu().read(x as u32, 0, 6);
            want.push(o);
            label.push(format!("config {config} words {nwords} mask {mask:#x} passes {passes:?} acc {acc:?} colour {colour:#x} old {old:#x} has_colour {has_colour}"));
        }
    }
    let outs = run(&gpu, "accum_main", &cases);
    let mut bad = Bad::default();
    for (i, o) in outs.iter().enumerate() {
        for k in 0..5 {
            bad.check(&format!("accumulation word {k}"), || label[i].clone(), o[k], want[i][k]);
        }
    }
    let returned = want.iter().filter(|w| w[4] != 0).count();
    assert!(returned > 5000, "only {returned} nonzero drawn words");
    bad.done("accumulation");
}

#[test]
fn stipples_match_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0x57_1991e);
    let (width, height) = (40i32, 24i32);
    let mut cases = Vec::new();
    let mut want = Vec::new();
    let mut label = Vec::new();
    for config in 0..60 {
        let line = config % 2 == 0;
        let (ox, oy) = (rng.i(0, 40), rng.i(0, 40));
        let rect = Rect { x0: ox, y0: oy, x1: ox + width, y1: oy + height };
        // Every pixel of the rectangle is inside this triangle.
        let mut tri = Tri { c: [1; 3], origin: [ox, oy], at_origin: [8191.0; 4], z: Plane::default(), ..Tri::default() };
        let store = PixelStore::new(128, 80);
        store.ensure(0);
        let mut ds = DrawState::default();
        ds.clip = rect;
        ds.viewport = rect;
        ds.colour = [0, 0];
        ds.colours = 1;
        let rows: [u32; 32] = std::array::from_fn(|_| rng.next() as u32);
        let (pattern, repeat) = (rng.next() as u16, 1 + rng.u(9));
        let d = [rng.f(-5.0, 60.0), rng.f(-2.0, 2.0), rng.f(-2.0, 2.0)];
        let (wox, woy) = (rng.i(0, 9), rng.i(0, 9));
        if line {
            tri.line_d = d;
            ds.line_stipple = Some((pattern, repeat));
        } else {
            ds.polygon_stipple = Some(rows);
            ds.origin = [wox, woy];
        }
        let job = TriJob { tri, line, rect, state: 0 };
        raster_run(&store, &RasterJob::Tri(job), &ds, rect);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let mut w = [0u32; 32];
                if line {
                    w[1] = pattern as u32;
                    w[2] = repeat;
                    w[3..6].copy_from_slice(&d.map(fl));
                    w[6] = fl((x - ox) as f32);
                    w[7] = fl((y - oy) as f32);
                } else {
                    w[0] = 1;
                    w[1] = rows[((y - woy) & 31) as usize];
                    w[2] = x as u32;
                    w[3] = wox as u32;
                }
                cases.push(w);
                want.push(store.read(x as u32, y as u32, 0) != 0);
                label.push(format!("{} stipple at ({x}, {y}) config {config}", if line { "line" } else { "polygon" }));
            }
        }
    }
    let outs = run(&gpu, "stipple_main", &cases);
    let mut bad = Bad::default();
    for (i, o) in outs.iter().enumerate() {
        bad.check("stipple", || label[i].clone(), o[0], want[i] as u32);
    }
    // Both outcomes occur, or the test says nothing.
    assert!(want.iter().any(|&b| b) && want.iter().any(|&b| !b));
    bad.done("stipple");
}
