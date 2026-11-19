//! The texture libraries against the CPU, bit for bit: `lod_log2` and
//! `lod_exp2` against the host's `f32::log2` and `exp2`, and `tex_sample`,
//! `tex_sample3`, `tex_sample_enhanced` and `tex_environment` against
//! `raster::texel` over random textures in a sparse texture store. Each test
//! needs a GPU (or llvmpipe) and returns early without one. The adapter that
//! ran is printed (`cargo test -- --nocapture`).

use std::sync::Arc;

use crate::dev::kona::back::lodmath;
use super::{pack_desc, texture_source, TexMemory, LOD_MATH, TD_WORDS};
use crate::dev::kona::back::raster::texel::{self, default_weights, Combine, Detail, EnvMode, Filter, Sharpen, TexDesc, TexLevel, Wrap, CURVE_NODES};
use crate::dev::kona::back::store::texture::{TextureStore, PAGE_WORDS};
use crate::wgpu::{library, Fragment, Gpu, GpuOptions, Source};

pub(in crate::dev::kona::back) const PLANES: u32 = 4;
const PAGES: u32 = 8;

pub(in crate::dev::kona::back) struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() >> 32) as u32 % n
    }

    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo) as u32) as i32
    }

    pub fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    pub fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    pub fn coin(&mut self, one_in: u32) -> bool {
        self.below(one_in) == 0
    }
}

fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default(), Box::new(|_| {})) {
        Ok(g) => {
            eprintln!("gpu_lib parity on: {}", g.caps);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn storage(read_only: bool, binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn upload(gpu: &Gpu, label: &str, words: &[u32]) -> wgpu::Buffer {
    let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
    let buf = gpu.buffer(label, (words.len().max(4) * 4) as u64, usage);
    gpu.write(&buf, 0, bytemuck::cast_slice(words));
    buf
}

/// Run `src` (a kernel `main` over `ins` and `outs` in group 0, and the texture
/// memory in group 3 when `tex` is given) for `cases` invocations; the output
/// has `out_words` words a case.
fn dispatch(gpu: &Arc<Gpu>, src: &Source, ins: &[u32], cases: usize, out_words: usize, tex: Option<(&[u32], &TexMemory)>) -> Vec<u32> {
    let module = src.compile(&gpu.device).unwrap_or_else(|e| panic!("{e}"));
    let l0 = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("io"), entries: &[storage(true, 0), storage(false, 1)] });
    let l3 = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("tex"), entries: &[storage(true, 0), storage(true, 1), storage(true, 2)] });
    let layout = gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("gpu_lib"),
        bind_group_layouts: &[Some(&l0), None, None, if tex.is_some() { Some(&l3) } else { None }],
        immediate_size: 0,
    });
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let pipeline = gpu.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("gpu_lib"),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    if let Some(e) = pollster::block_on(scope.pop()) {
        panic!("pipeline: {e}");
    }
    let inb = upload(gpu, "ins", ins);
    let outb = gpu.buffer("outs", (cases * out_words * 4) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);
    let g0 = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("io"),
        layout: &l0,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: inb.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: outb.as_entire_binding() }],
    });
    let tex_bufs = tex.map(|(descs, mem)| (upload(gpu, "descs", descs), upload(gpu, "pages", &mem.pages), upload(gpu, "pool", &mem.pool)));
    let g3 = tex_bufs.as_ref().map(|(a, b, c)| {
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tex"),
            layout: &l3,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: a.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: b.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: c.as_entire_binding() },
            ],
        })
    });
    let mut enc = gpu.encoder("gpu_lib");
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("gpu_lib"), timestamp_writes: None });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &g0, &[]);
        if let Some(g3) = &g3 {
            pass.set_bind_group(3, g3, &[]);
        }
        pass.dispatch_workgroups((cases as u32).div_ceil(64), 1, 1);
    }
    gpu.submit(enc);
    let bytes = gpu.read_vec(&outb, &[(0, outb.size())]).unwrap_or_else(|e| panic!("{e}"));
    gpu.check().unwrap_or_else(|e| panic!("{e}"));
    bytemuck::cast_slice(&bytes).to_vec()
}

const LOD_KERNEL: Fragment = Fragment::new(
    "lod_test.wgsl",
    r#"
#include "kona_lodmath.wgsl"

@group(0) @binding(0) var<storage, read> ins: array<u32>;
@group(0) @binding(1) var<storage, read_write> outs: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let i = g.x;
    if i >= arrayLength(&ins) {
        return;
    }
    outs[2u * i] = lod_log2(ins[i]);
    outs[2u * i + 1u] = lod_exp2(ins[i]);
}
"#,
);

/// Equal as bits, or both NaN.
fn same(got: u32, want: f32) -> bool {
    let g = f32::from_bits(got);
    (g.is_nan() && want.is_nan()) || got == want.to_bits()
}

/// log2 and exp2 of f32 inputs: the GPU's bits against the host's `f32::log2` and `f32::exp2` (a
/// libm), and against the double-precision libm result rounded once to f32 (correctly rounded, bar a
/// double-rounding case in 2^29), which says whether a difference from the host is the host's.
#[test]
fn log2_and_exp2_match_the_host() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0x10C2_E7C2);
    let mut xs: Vec<f32> = Vec::new();
    // The level of detail's domain: rho * scale / 16 from 1e-6 up, and random positives of every exponent.
    for _ in 0..60_000 {
        xs.push(f32::from_bits(rng.below(0x7f80_0000)));
    }
    for _ in 0..60_000 {
        xs.push(1.0e-6 * (rng.f(0.0, 26.0)).exp2());
        xs.push(rng.f(0.0, 64.0));
    }
    // Near one, near the powers of two and the reduction's edge at the square root of two.
    for k in -64i32..=64 {
        xs.push(f32::from_bits((1.0f32.to_bits() as i32 + k) as u32));
        xs.push(f32::from_bits((std::f32::consts::SQRT_2.to_bits() as i32 + k) as u32));
        xs.push(f32::from_bits((0.5f32.to_bits() as i32 + k) as u32));
    }
    for e in -149..=127 {
        xs.push(2.0f32.powi(e));
    }
    // exp2's domain: the weight's (the level of detail at or below zero), whole ranges and tiny values.
    for _ in 0..60_000 {
        xs.push(-rng.f(0.0, 21.0));
        xs.push(rng.f(-150.0, 128.0));
        xs.push(rng.f(-1.0, 1.0) * 2.0f32.powi(-rng.range(0, 40)));
    }
    for k in -300..=300 {
        xs.push(k as f32 / 2.0);
        xs.push((k as f32) / 2.0 + 2.0f32.powi(-20));
    }
    for v in [0.0, -0.0, 1.0, -1.0, 128.0, 127.99999, -126.0, -149.0, -150.0, -150.00002, -149.9, -151.0, 1.0e-45, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, f32::MAX, f32::MIN_POSITIVE, 1.0e-40] {
        xs.push(v);
    }
    let ins: Vec<u32> = xs.iter().map(|x| x.to_bits()).collect();
    let src = Source::new("lod test").library(library::I64).library(library::WIDE).library(library::F64).library(LOD_MATH).body(LOD_KERNEL);
    let outs = dispatch(&gpu, &src, &ins, ins.len(), 2, None);
    // The GPU against the CPU twin (the same operations on native doubles) and against the double
    // libm rounded once: both must be exact. The host's f32 libm is only counted.
    let (mut bad, mut host_misses, mut host_examples) = (Vec::new(), 0usize, Vec::new());
    let mut check = |what: &str, x: f32, got: u32, twin: f32, cr: f32, host: f32| {
        if !same(got, twin) || !same(got, cr) {
            bad.push(format!("{what}({x:e} [{:#010x}]): gpu {:e} [{got:#010x}], cpu twin {twin:e}, rounded double libm {cr:e}", x.to_bits(), f32::from_bits(got)));
        }
        if !same(got, host) {
            host_misses += 1;
            if host_examples.len() < 5 {
                host_examples.push(format!("{what}({x:e} [{:#010x}]): gpu {:e}, host libm {host:e}", x.to_bits(), f32::from_bits(got)));
            }
        }
    };
    for (i, &x) in xs.iter().enumerate() {
        check("log2", x, outs[2 * i], lodmath::log2(x), (x as f64).log2() as f32, x.log2());
        check("exp2", x, outs[2 * i + 1], lodmath::exp2(x), (x as f64).exp2() as f32, x.exp2());
    }
    eprintln!("{} log2 and {} exp2 inputs: gpu differs from the CPU twin or the rounded double on {}; the host f32 libm, which the CPU no longer uses, differs on {host_misses} results (each by an ulp)", xs.len(), xs.len(), bad.len());
    for l in &host_examples {
        eprintln!("  host libm: {l}");
    }
    assert!(bad.is_empty(), "{} results differ:\n{}", bad.len(), bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
}

/// `tex_sat_i32`, `tex_sat_u32` and `div_exact` over 3-word cases [a, b, unused].
const EDGE_KERNEL: Fragment = Fragment::new(
    "edge_test.wgsl",
    r#"
#include "kona_texture.wgsl"

@group(0) @binding(0) var<storage, read> ins: array<u32>;
@group(0) @binding(1) var<storage, read_write> outs: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let i = g.x;
    if i * 2u >= arrayLength(&ins) {
        return;
    }
    let a = bitcast<f32>(ins[2u * i]);
    let b = bitcast<f32>(ins[2u * i + 1u]);
    outs[3u * i] = bitcast<u32>(tex_sat_i32(a));
    outs[3u * i + 1u] = tex_sat_u32(a);
    outs[3u * i + 2u] = bitcast<u32>(div_exact(a, b));
}
"#,
);

/// The edges of the float helpers the texture sampler leans on, against Rust: `as i32` and `as u32`
/// (saturating, NaN to 0) and IEEE division, for infinities, NaN, zeros, the saturation bounds and
/// random values. Subnormal operands and results are left out: the adapters flush them, which the
/// float probe records as a limit of the platform, not of these helpers.
#[test]
fn texture_saturation_and_division_edges_match_rust() {
    let Some(gpu) = gpu() else { return };
    let normal_or_zero = |x: f32| x == 0.0 || x.is_normal() || x.is_infinite() || x.is_nan();
    let mut xs: Vec<f32> = vec![
        0.0, -0.0, 1.0, -1.0, 0.5, -0.5, 1.5, -1.5, 2147483520.0, 2147483648.0, -2147483648.0, -2147483904.0, 4294967040.0, 4294967296.0, 8388608.0, 16777216.0, 1.0e10, -1.0e10, 1.0e38, -1.0e38,
        f32::MAX, f32::MIN, f32::MIN_POSITIVE, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, -f32::NAN, f32::from_bits(0x7fc0_0001), f32::from_bits(0xffa0_0000),
    ];
    let mut rng = Rng(0xED6E_5A71);
    for _ in 0..20_000 {
        xs.push(f32::from_bits(rng.next() as u32));
        xs.push(rng.f(-4.0e9, 4.0e9));
    }
    let xs: Vec<f32> = xs.into_iter().filter(|&x| normal_or_zero(x)).collect();
    let mut cases: Vec<(f32, f32)> = Vec::new();
    for &a in xs.iter().take(60) {
        for &b in xs.iter().take(60) {
            cases.push((a, b));
        }
    }
    for &a in &xs {
        cases.push((a, xs[(rng.next() as usize) % xs.len()]));
    }
    let ins: Vec<u32> = cases.iter().flat_map(|&(a, b)| [a.to_bits(), b.to_bits()]).collect();
    let outs = dispatch(&gpu, &texture_source("edge test", PLANES, PAGES).body(EDGE_KERNEL), &ins, cases.len(), 3, None);
    let mut bad = Vec::new();
    let mut kinds = [0usize; 3];
    for (i, &(a, b)) in cases.iter().enumerate() {
        let (si, su, dv) = (outs[3 * i] as i32, outs[3 * i + 1], outs[3 * i + 2]);
        if si != a as i32 {
            kinds[0] += 1;
            bad.push(format!("tex_sat_i32({a:e} [{:#010x}]) = {si}, Rust {}", a.to_bits(), a as i32));
        }
        if su != a as u32 {
            kinds[1] += 1;
            bad.push(format!("tex_sat_u32({a:e} [{:#010x}]) = {su}, Rust {}", a.to_bits(), a as u32));
        }
        let q = a / b;
        if normal_or_zero(q) && !same(dv, q) {
            kinds[2] += 1;
            bad.push(format!("div_exact({a:e} [{:#010x}], {b:e} [{:#010x}]) = {:e} [{dv:#010x}], IEEE {q:e}", a.to_bits(), b.to_bits(), f32::from_bits(dv)));
        }
    }
    eprintln!("{} cases; differ: tex_sat_i32 {}, tex_sat_u32 {}, div_exact {}", cases.len(), kinds[0], kinds[1], kinds[2]);
    assert!(bad.is_empty(), "{} results differ:\n{}", bad.len(), bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
}

/// A kernel over 16-word cases: [desc, kind, s, t, r, lod, 6..9 fragment colour, 10..13 texel, 14
/// unused, 15 zero], kind 0 sample, 1 sample3, 2 sample_enhanced, 3 environment.
const TEX_KERNEL: Fragment = Fragment::new(
    "tex_test.wgsl",
    r#"
#include "kona_texture.wgsl"

@group(0) @binding(0) var<storage, read> ins: array<u32>;
@group(0) @binding(1) var<storage, read_write> outs: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let i = g.x;
    if i * 16u >= arrayLength(&ins) {
        return;
    }
    let b = i * 16u;
    tex_nz = ins[b + 15u];
    let d = ins[b];
    let s = bitcast<f32>(ins[b + 2u]);
    let t = bitcast<f32>(ins[b + 3u]);
    let r = bitcast<f32>(ins[b + 4u]);
    let lod = bitcast<f32>(ins[b + 5u]);
    var o = vec4<u32>(0u);
    switch ins[b + 1u] {
        case 0u: { o = tex_sample(d, s, t, lod); }
        case 1u: { o = tex_sample3(d, s, t, r, lod); }
        case 2u: { o = tex_sample_enhanced(d, s, t, lod); }
        default: {
            o = tex_environment(d, vec4<u32>(ins[b + 6u], ins[b + 7u], ins[b + 8u], ins[b + 9u]), vec4<u32>(ins[b + 10u], ins[b + 11u], ins[b + 12u], ins[b + 13u]));
        }
    }
    outs[4u * i] = o.x;
    outs[4u * i + 1u] = o.y;
    outs[4u * i + 2u] = o.z;
    outs[4u * i + 3u] = o.w;
}
"#,
);

/// A store with random words in pages 0 to 3 and 5 of planes 0 to 3, the rest absent.
pub(in crate::dev::kona::back) fn random_store(rng: &mut Rng) -> TextureStore {
    let store = TextureStore::default();
    for plane in 0..PLANES as u16 {
        for pg in [0u32, 1, 2, 3, 5] {
            for i in 0..PAGE_WORDS as u32 {
                let w = rng.next() as u32;
                // Some runs of one colour and alpha 0 or 255, which a bilinear blend would flatten.
                let w = match rng.below(8) {
                    0 => 0xff00_0000 | (w & 0x00ff_ffff),
                    1 => 0,
                    _ => w,
                };
                assert!(store.ex().set(plane, pg * PAGE_WORDS as u32 + i, w));
            }
        }
    }
    store
}

fn random_level(rng: &mut Rng, w: u16, h: u16, base: u32) -> TexLevel {
    let extra = rng.below(4) as u16;
    let (ox, oy) = (rng.below(3) as u16, rng.below(3) as u16);
    TexLevel { plane: if rng.coin(40) { 7 } else { rng.below(PLANES) as u16 }, off: base, pitch: w + extra + ox, ox, oy, w, h, d: 0, slice: 0 }
}

fn random_filter(rng: &mut Rng) -> Filter {
    match rng.below(4) {
        0 => Filter::Nearest,
        1 => Filter::Linear,
        _ => Filter::Mip { within_linear: rng.coin(2), between_linear: rng.coin(2) },
    }
}

fn random_wrap(rng: &mut Rng) -> [Wrap; 3] {
    std::array::from_fn(|_| if rng.coin(2) { Wrap::Clamp } else { Wrap::Repeat })
}

#[derive(Clone, Copy, PartialEq)]
pub(in crate::dev::kona::back) enum Flavour {
    Plain,
    Volume,
    Enhanced,
}

pub(in crate::dev::kona::back) fn random_desc(rng: &mut Rng, flavour: Flavour) -> TexDesc {
    let mut d = TexDesc {
        count: 1 + rng.below(6) as u8,
        min: random_filter(rng),
        mag: random_filter(rng),
        wrap: random_wrap(rng),
        lod_scale: [rng.below(40) as u8, rng.below(40) as u8, rng.below(40) as u8],
        volume: flavour == Flavour::Volume,
        class: rng.below(6) as u8,
        env: [EnvMode::Modulate, EnvMode::Decal, EnvMode::Blend, EnvMode::Replace][rng.below(4) as usize],
        env_colour: std::array::from_fn(|_| rng.below(8192)),
        ..Default::default()
    };
    if flavour == Flavour::Enhanced {
        d.mag = Filter::Linear;
        d.count = 2 + rng.below(4) as u8;
    }
    // Level sizes are not powers of two now and then; levels sit one after another from a base that
    // straddles pages, including the absent ones.
    let (w0, h0, d0) = if flavour == Flavour::Volume { (1 + rng.below(24) as u16, 1 + rng.below(24) as u16, 1 + rng.below(8) as u16) } else { (1 + rng.below(64) as u16, 1 + rng.below(64) as u16, 0) };
    let mut base = rng.below(5 * PAGE_WORDS as u32);
    for i in 0..d.count as usize {
        let (w, h, dp) = ((w0 >> i).max(1), (h0 >> i).max(1), (d0 >> i).max(1));
        let mut l = random_level(rng, w, h, base);
        if flavour == Flavour::Volume {
            l.d = dp;
            l.slice = (l.oy as u32 + h as u32) * l.pitch as u32 + l.ox as u32 + w as u32 + rng.below(3);
            base += l.slice * dp as u32 + 8;
        } else {
            base += (l.oy as u32 + h as u32) * l.pitch as u32 + l.ox as u32 + w as u32;
        }
        d.levels[i] = l;
    }
    match flavour {
        Flavour::Enhanced => {
            let weights: [f32; CURVE_NODES] = if rng.coin(3) { default_weights() } else { std::array::from_fn(|_| rng.f(-2.0, 2.0)) };
            if rng.coin(2) {
                let (w, h) = (1 << rng.below(5), 1 << rng.below(5));
                let mut image = random_level(rng, w, h, base);
                image.plane = rng.below(PLANES) as u16;
                d.combine = Combine::Detail(Detail { modulate: rng.coin(2), scale: rng.below(5) as u8, channels: rng.below(4) as u8, image, weights });
            } else {
                d.combine = Combine::Sharpen(Sharpen { channels: rng.below(4) as u8, weights });
            }
        }
        _ => {
            if rng.coin(10) {
                d.combine = Combine::Sharpen(Sharpen { channels: 0, weights: default_weights() });
            }
        }
    }
    d
}

fn random_lod(rng: &mut Rng, enhanced: bool) -> f32 {
    match rng.below(6) {
        0 => rng.below(8) as f32,
        1 => rng.below(8) as f32 + 0.5,
        2 if enhanced => -(rng.below(24) as f32),
        3 | 4 if enhanced => -rng.f(0.0, 21.0),
        _ => rng.f(-4.0, 8.0),
    }
}

/// A texture coordinate: anywhere, or on a texel's edge or centre of level 0 (where a floor and a
/// half-texel offset decide the texel).
fn random_coord(rng: &mut Rng, size: u16) -> f32 {
    match rng.below(5) {
        0 => rng.f(-3.0, 4.0),
        1 => (rng.range(-3 * size as i32, 4 * size as i32) as f32) / size as f32,
        2 => (rng.range(-3 * size as i32, 4 * size as i32) as f32 + 0.5) / size as f32,
        3 => rng.f(-1.0e6, 1.0e6),
        _ => rng.f(-1.0, 2.0),
    }
}

#[test]
fn texture_sampling_and_environment_match_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut rng = Rng(0x7E27_0123);
    let store = random_store(&mut rng);
    let mem = TexMemory::from_store(&store, PLANES, PAGES);
    let mut descs: Vec<(TexDesc, Flavour)> = Vec::new();
    for flavour in [Flavour::Plain, Flavour::Volume, Flavour::Enhanced] {
        for _ in 0..80 {
            descs.push((random_desc(&mut rng, flavour), flavour));
        }
    }
    let mut packed = Vec::new();
    for (d, _) in &descs {
        packed.extend_from_slice(&pack_desc(d));
    }
    assert_eq!(packed.len(), descs.len() * TD_WORDS);
    // (kind, desc index) for every case.
    struct Case {
        desc: usize,
        kind: u32,
        s: f32,
        t: f32,
        r: f32,
        lod: f32,
        f: [u32; 4],
        texel: [u32; 4],
    }
    let mut cases = Vec::new();
    for _ in 0..160_000 {
        let desc = rng.below(descs.len() as u32) as usize;
        let (d, flavour) = &descs[desc];
        let kind = match (flavour, rng.below(4)) {
            (Flavour::Volume, 0 | 1) => 1,
            (Flavour::Enhanced, 0 | 1) => 2,
            (_, 3) => 3,
            (Flavour::Volume, _) => 1,
            _ => 0,
        };
        let l0 = d.levels[0];
        let f13 = |rng: &mut Rng| match rng.below(6) {
            0 => 0,
            1 => 8191,
            _ => rng.below(8192),
        };
        cases.push(Case {
            desc,
            kind,
            s: random_coord(&mut rng, l0.w.max(1)),
            t: random_coord(&mut rng, l0.h.max(1)),
            r: random_coord(&mut rng, l0.d.max(1)),
            lod: random_lod(&mut rng, kind == 2),
            f: std::array::from_fn(|_| f13(&mut rng)),
            texel: std::array::from_fn(|_| if rng.coin(5) { 255 * rng.below(2) } else { rng.below(256) }),
        });
    }
    let mut ins = Vec::with_capacity(cases.len() * 16);
    for c in &cases {
        let mut w = [0u32; 16];
        w[0] = (c.desc * TD_WORDS) as u32;
        w[1] = c.kind;
        w[2] = c.s.to_bits();
        w[3] = c.t.to_bits();
        w[4] = c.r.to_bits();
        w[5] = c.lod.to_bits();
        w[6..10].copy_from_slice(&c.f);
        w[10..14].copy_from_slice(&c.texel);
        ins.extend_from_slice(&w);
    }
    let src = texture_source("tex test", PLANES, PAGES).body(TEX_KERNEL);
    let outs = dispatch(&gpu, &src, &ins, cases.len(), 4, Some((&packed, &mem)));
    let mut bad = Vec::new();
    let mut per_kind = [0usize; 4];
    let mut bad_kind = [0usize; 4];
    // A control: a kernel that wrote nothing would not match, but say how much of what matched is not zero.
    let mut nonzero = [0usize; 4];
    for (i, c) in cases.iter().enumerate() {
        let d = &descs[c.desc].0;
        let want = match c.kind {
            0 => texel::sample(&store, d, c.s, c.t, c.lod),
            1 => texel::sample3(&store, d, c.s, c.t, c.r, c.lod),
            2 => texel::sample_enhanced(&store, d, c.s, c.t, c.lod),
            _ => texel::environment(d, c.f, c.texel),
        };
        let got = [outs[4 * i], outs[4 * i + 1], outs[4 * i + 2], outs[4 * i + 3]];
        per_kind[c.kind as usize] += 1;
        nonzero[c.kind as usize] += (want != [0; 4]) as usize;
        if got != want {
            bad_kind[c.kind as usize] += 1;
            if bad.len() < 15 {
                bad.push(format!(
                    "kind {} desc {} (count {} min {:?} mag {:?} wrap {:?} class {} env {:?} combine {}) s {:e} t {:e} r {:e} lod {:e}: gpu {got:?}, cpu {want:?}",
                    c.kind, c.desc, d.count, d.min, d.mag, d.wrap, d.class, d.env,
                    match d.combine { Combine::None => "none", Combine::Detail(_) => "detail", Combine::Sharpen(_) => "sharpen" },
                    c.s, c.t, c.r, c.lod
                ));
            }
        }
    }
    eprintln!("cases per kind (sample, sample3, enhanced, environment) {per_kind:?}, nonzero results {nonzero:?}, mismatches {bad_kind:?}");
    assert!(nonzero.iter().zip(per_kind).all(|(&n, c)| n * 2 > c), "most results should be something: {nonzero:?} of {per_kind:?}");
    assert!(bad.is_empty(), "mismatches {bad_kind:?} of {per_kind:?}:\n{}", bad.join("\n"));
}
