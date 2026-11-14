//! SPIKE (throwaway): can KONA's raster run as hooks on the shared wgpu
//! batch kernels? Single-sample triangles with the whole per-fragment
//! operation chain (ownership, polygon stipple, alpha, stencil, depth,
//! blend, logic op, colour mask) into the planar word store, against
//! `pixel::run` word for word. Needs a GPU or lavapipe; skips without one.

use std::sync::Arc;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use super::raster::ops::{BlendEq, BlendFactor, Compare, LogicOp, Ownership, StencilOp};
use super::raster::setup::{self, Shape};
use super::raster::{pixel, AlphaTest, BlendState, DrawState, Rect, StencilState, WVertex};
use super::store::{PixelStore, DID_PLANE};
use super::tiles::RasterJob;
use crate::wgpu::batch::{ArchShaders, Batch, BatchPipelines, Full};
use crate::wgpu::device::{Gpu, GpuOptions};
use crate::wgpu::library;
use crate::wgpu::memory::GpuMemory;
use crate::wgpu::shader::Fragment;
use crate::wgpu::tiles::TileGrid;

const KONA: Fragment = Fragment::new("kona.wgsl", include_str!("kona.wgsl"));

const REC_W: usize = 38;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GRec {
    slot: u32,
    one: u32,
    w: [u32; REC_W],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GSlot {
    w: [u32; 64],
}

/// GPU positions: the record's words 0..8 as is, the display-ID plane at 8.
const POSITIONS: usize = 9;
fn gpu_pos(w: u32) -> u32 {
    if w as usize == DID_PLANE {
        8
    } else {
        assert!(w < 8);
        w
    }
}

struct Spike {
    gpu: Arc<Gpu>,
    pipelines: BatchPipelines,
    batch: Batch<GRec, GSlot>,
    mem: GpuMemory,
    group: wgpu::BindGroup,
    w: u32,
    h: u32,
}

fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default()) {
        Ok(g) => {
            eprintln!("adapter: {} ({:?}, {:?}) driver {}", g.caps.adapter, g.caps.backend, g.caps.device_type, g.caps.driver);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

impl Spike {
    fn new(gpu: Arc<Gpu>, w: u32, h: u32) -> Spike {
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kona planes"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let pipelines = BatchPipelines::new(
            &gpu,
            &ArchShaders {
                label: "kona",
                raster: &[KONA],
                bin_accept: None,
                library: &[library::I64, library::WIDE],
                defines: &[("WIDTH", w), ("HEIGHT", h), ("PLANE", w * h)],
                layouts: &[&layout],
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let mem = GpuMemory::new(&gpu, "kona planes", (POSITIONS as u64) * (w * h) as u64 * 4).unwrap();
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kona planes"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: mem.buffer.as_entire_binding() }],
        });
        let batch = Batch::new(&gpu, &pipelines, TileGrid::new(w, h), 256);
        Spike { gpu, pipelines, batch, mem, group, w, h }
    }

    fn upload(&self, store: &PixelStore) {
        let plane = (self.w * self.h) as usize;
        let mut words = vec![0u32; POSITIONS * plane];
        for (cpu, gpu) in (0..8u32).map(|w| (w as usize, w)).chain([(DID_PLANE, 8)]) {
            for y in 0..self.h {
                for x in 0..self.w {
                    words[gpu as usize * plane + (y * self.w + x) as usize] = store.read(x, y, cpu);
                }
            }
        }
        self.mem.write(0, bytemuck::cast_slice(&words));
    }

    fn flush(&mut self) {
        self.batch.flush(&self.pipelines, &[&self.group]);
    }

    fn download(&mut self) -> Vec<u32> {
        bytemuck::cast_slice(&self.mem.read_all().unwrap()).to_vec()
    }
}

fn slot_of(ds: &DrawState, plane: u32) -> GSlot {
    let mut s = GSlot { w: [0; 64] };
    let mut fl = 0u32;
    let off = |w: u32| gpu_pos(w) * plane;
    let stencil_on = ds.stencil_bits != 0 && ds.stencil.enabled;
    if ds.depth_bits != 0 {
        fl |= 1;
    }
    if ds.depth_test {
        fl |= 2;
    }
    if ds.depth_mask {
        fl |= 4;
    }
    if ds.alpha.enabled {
        fl |= 8;
    }
    if stencil_on {
        fl |= 16;
    }
    if ds.blend.enabled {
        fl |= 32;
    }
    if ds.logic.is_some() {
        fl |= 64;
    }
    if ds.ownership.is_some() {
        fl |= 128;
    }
    if ds.polygon_stipple.is_some() {
        fl |= 256;
    }
    if ds.colour_mask != [true; 4] {
        fl |= 512;
    }
    s.w[0] = fl;
    s.w[1] = ds.colours as u32;
    s.w[2] = off(ds.colour[0]);
    s.w[3] = if ds.colours > 1 { off(ds.colour[1]) } else { 0 };
    s.w[4] = if ds.depth_bits != 0 { off(ds.depth_word) } else { 0 };
    s.w[5] = if stencil_on { off(ds.stencil_word) } else { 0 };
    s.w[6] = ds.depth_func as u32;
    s.w[7] = ds.alpha.func as u32;
    s.w[8] = ds.alpha.reference;
    s.w[9] = ds.stencil.func as u32;
    s.w[10] = ds.stencil.reference as u32;
    s.w[11] = ds.stencil.read_mask as u32;
    s.w[12] = ds.stencil.write_mask as u32;
    s.w[13] = ds.stencil.fail as u32;
    s.w[14] = ds.stencil.zfail as u32;
    s.w[15] = ds.stencil.zpass as u32;
    s.w[16] = ds.blend.src as u32;
    s.w[17] = ds.blend.dst as u32;
    s.w[18] = ds.blend.eq as u32;
    for k in 0..4 {
        s.w[19 + k] = ds.blend.constant[k];
    }
    s.w[23] = ds.logic.map_or(0, |l| l as u32);
    let m = ds.colour_mask;
    s.w[24] = (!m[0] as u32 * 0xff) << 16 | (!m[1] as u32 * 0xff) << 8 | (!m[2] as u32 * 0xff) | (!m[3] as u32 * 0xff) << 24;
    if let Some(o) = &ds.ownership {
        s.w[25] = o.reference;
        s.w[26] = o.mask;
        s.w[27] = off(DID_PLANE as u32);
    }
    s.w[28] = ds.origin[0] as u32;
    s.w[29] = ds.origin[1] as u32;
    if let Some(rows) = &ds.polygon_stipple {
        for (k, r) in rows.iter().enumerate() {
            s.w[32 + k] = *r;
        }
    }
    s
}

fn rec_of(job: &setup::TriJob, slot: u32) -> GRec {
    let t = &job.tri;
    let mut w = [0u32; REC_W];
    for k in 0..3 {
        let a = i32::try_from(t.a[k]).expect("edge x step fits i32 inside the guard band");
        let b = i32::try_from(t.b[k]).expect("edge y step fits i32 inside the guard band");
        w[k] = a as u32;
        w[3 + k] = b as u32;
        w[6 + 2 * k] = t.c[k] as u64 as u32;
        w[7 + 2 * k] = (t.c[k] as u64 >> 32) as u32;
        w[12] |= (t.tie[k] as u32) << k;
    }
    w[13] = t.origin[0] as u32;
    w[14] = t.origin[1] as u32;
    for k in 0..4 {
        w[15 + k] = t.at_origin[k].to_bits();
        w[19 + k] = t.dx[k].to_bits();
        w[23 + k] = t.dy[k].to_bits();
    }
    for (n, v) in [t.z.at, t.z.dx, t.z.dy].into_iter().enumerate() {
        w[27 + 2 * n] = v as u64 as u32;
        w[28 + 2 * n] = (v as u64 >> 32) as u32;
    }
    w[33] = job.rect.x0 as u32;
    w[34] = job.rect.y0 as u32;
    w[35] = job.rect.x1 as u32;
    w[36] = job.rect.y1 as u32;
    GRec { slot, one: 1.0f32.to_bits(), w }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u32) as i32
    }
    fn coin(&mut self, n: u32) -> bool {
        self.next() % n == 0
    }
}

fn pick<T: Copy>(r: &mut Lcg, v: &[T]) -> T {
    v[r.next() as usize % v.len()]
}

const CMP: [Compare; 8] = [Compare::Never, Compare::Less, Compare::Equal, Compare::Lequal, Compare::Greater, Compare::Notequal, Compare::Gequal, Compare::Always];
const SOP: [StencilOp; 6] = [StencilOp::Keep, StencilOp::Zero, StencilOp::Replace, StencilOp::Incr, StencilOp::Decr, StencilOp::Invert];
const BF: [BlendFactor; 15] = [
    BlendFactor::Zero,
    BlendFactor::One,
    BlendFactor::SrcColour,
    BlendFactor::OneMinusSrcColour,
    BlendFactor::SrcAlpha,
    BlendFactor::OneMinusSrcAlpha,
    BlendFactor::DstAlpha,
    BlendFactor::OneMinusDstAlpha,
    BlendFactor::DstColour,
    BlendFactor::OneMinusDstColour,
    BlendFactor::SrcAlphaSaturate,
    BlendFactor::ConstantColour,
    BlendFactor::OneMinusConstantColour,
    BlendFactor::ConstantAlpha,
    BlendFactor::OneMinusConstantAlpha,
];
const BE: [BlendEq; 5] = [BlendEq::Add, BlendEq::Subtract, BlendEq::ReverseSubtract, BlendEq::Min, BlendEq::Max];
const LOP: [LogicOp; 16] = [
    LogicOp::Clear,
    LogicOp::And,
    LogicOp::AndReverse,
    LogicOp::Copy,
    LogicOp::AndInverted,
    LogicOp::Noop,
    LogicOp::Xor,
    LogicOp::Or,
    LogicOp::Nor,
    LogicOp::Equiv,
    LogicOp::Invert,
    LogicOp::OrReverse,
    LogicOp::CopyInverted,
    LogicOp::OrInverted,
    LogicOp::Nand,
    LogicOp::Set,
];

/// A depth + colour only state: the narrowest slice.
fn state_depth(w: u32, h: u32, r: &mut Lcg) -> DrawState {
    let full = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    DrawState {
        clip: full,
        viewport: full,
        colour: [1, 0],
        colours: 1,
        depth_word: 3,
        depth_bits: 32,
        depth_test: true,
        depth_func: pick(r, &[Compare::Less, Compare::Lequal, Compare::Greater]),
        depth_mask: true,
        ..DrawState::default()
    }
}

/// Every operation of the single-sample chain, drawn at random.
fn state_full(w: u32, h: u32, r: &mut Lcg) -> DrawState {
    let mut d = state_depth(w, h, r);
    d.colours = 1 + r.coin(4) as u8;
    d.colour[1] = 2;
    d.depth_bits = if r.coin(5) { 0 } else { 32 };
    d.depth_test = !r.coin(4);
    d.depth_func = pick(r, &CMP);
    d.depth_mask = !r.coin(3);
    if r.coin(3) {
        d.alpha = AlphaTest { enabled: true, func: pick(r, &CMP), reference: r.next() % 8192 };
    }
    if r.coin(3) {
        d.stencil_bits = 8;
        d.stencil_word = 4;
        d.stencil = StencilState {
            enabled: true,
            func: pick(r, &CMP),
            reference: r.next() as u8,
            read_mask: r.next() as u8 | 0x0f,
            write_mask: r.next() as u8 | 0xf0,
            fail: pick(r, &SOP),
            zfail: pick(r, &SOP),
            zpass: pick(r, &SOP),
        };
    }
    if r.coin(2) {
        d.blend = BlendState {
            enabled: true,
            src: pick(r, &BF),
            dst: pick(r, &BF),
            eq: pick(r, &BE),
            constant: [r.next() % 8192, r.next() % 8192, r.next() % 8192, r.next() % 8192],
        };
    }
    if r.coin(4) {
        d.logic = Some(pick(r, &LOP));
    }
    if r.coin(4) {
        d.colour_mask = [!r.coin(3), !r.coin(3), !r.coin(3), !r.coin(3)];
    }
    if r.coin(4) {
        d.ownership = Some(Ownership { reference: r.next() % 64, mask: pick(r, &[0x3f, 0x7c0, 0x800, 0x3]) });
    }
    if r.coin(4) {
        let mut rows = [0u32; 32];
        for x in rows.iter_mut() {
            *x = r.next() | r.next();
        }
        d.polygon_stipple = Some(rows);
    }
    d.origin = [r.range(-8, 8), r.range(-8, 8)];
    d
}

fn vtx(x: f32, y: f32, c: [u16; 4], z: u32) -> WVertex {
    WVertex { x: (x * 256.0) as i32, y: (y * 256.0) as i32, z, c, t: [[0.0; 4]; 2], d: 0.0, fog: [0.0; 2] }
}

fn rand_tri(r: &mut Lcg, w: u32, h: u32, size: i32, flat: bool) -> [WVertex; 3] {
    let cx = r.range(-8, w as i32 + 8);
    let cy = r.range(-8, h as i32 + 8);
    let col = |r: &mut Lcg| [(r.next() % 8192) as u16, (r.next() % 8192) as u16, (r.next() % 8192) as u16, (r.next() % 8192) as u16];
    let c0 = col(r);
    let z0 = r.next();
    let mut v = [vtx(0.0, 0.0, c0, z0); 3];
    for item in v.iter_mut() {
        let x = cx as f32 + (r.range(-size * 8, size * 8) as f32) / 8.0;
        let y = cy as f32 + (r.range(-size * 8, size * 8) as f32) / 8.0;
        *item = if flat { vtx(x, y, c0, z0) } else { vtx(x, y, col(r), r.next()) };
    }
    v
}

fn fill_random(store: &PixelStore, w: u32, h: u32, r: &mut Lcg) {
    for pos in [1usize, 2, 3, 4, DID_PLANE] {
        for y in 0..h {
            for x in 0..w {
                let v = match pos {
                    3 => r.next() << 1 | (r.next() & 1),
                    4 => r.next() & 0xff,
                    DID_PLANE => (r.next() % 64) << 2 | (r.coin(8) as u32 * 0x1f) << 8,
                    _ => r.next(),
                };
                store.write(x, y, pos, v);
            }
        }
    }
}

struct Outcome {
    prims: usize,
    drawn: usize,
    bad: usize,
}

/// Draw `n` random triangles on both, compare every word position.
#[allow(clippy::too_many_arguments)]
fn parity(w: u32, h: u32, n: usize, size: i32, flat: bool, full: bool, seed: u64, batch_size: usize) -> Option<Outcome> {
    let gpu = gpu()?;
    let mut sp = Spike::new(gpu, w, h);
    let store = PixelStore::new(w, h);
    let mut r = Lcg(seed);
    fill_random(&store, w, h, &mut r);
    sp.upload(&store);
    let states: Vec<DrawState> = (0..8).map(|_| if full { state_full(w, h, &mut r) } else { state_depth(w, h, &mut r) }).collect();
    let screen = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    let before: Vec<u32> = (0..8u32).map(|x| x as usize).chain([DID_PLANE]).flat_map(|pos| (0..h).flat_map(move |y| (0..w).map(move |x| (pos, x, y)))).map(|(pos, x, y)| store.read(x, y, pos)).collect();
    let (mut drawn, mut inbatch) = (0, 0);
    let mut slots: Vec<Option<u32>> = vec![None; states.len()];
    for _ in 0..n {
        let si = r.next() as usize % states.len();
        let ds = &states[si];
        let tri = rand_tri(&mut r, w, h, size, flat);
        let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) else { continue };
        pixel::run(&store, &RasterJob::Tri(job), ds, screen);
        drawn += 1;
        let slot = match slots[si] {
            Some(s) => s,
            None => {
                let s = match sp.batch.slot(slot_of(ds, w * h)) {
                    Ok(s) => s,
                    Err(Full) => {
                        sp.flush();
                        slots = vec![None; states.len()];
                        sp.batch.slot(slot_of(ds, w * h)).unwrap()
                    }
                };
                slots[si] = Some(s);
                s
            }
        };
        let rect = job.rect;
        match sp.batch.prim(rec_of(&job, slot), rect.x0, rect.y0, rect.x1 - 1, rect.y1 - 1) {
            Ok(_) => inbatch += 1,
            Err(Full) => panic!("batch full"),
        }
        if inbatch >= batch_size {
            sp.flush();
            slots = vec![None; states.len()];
            inbatch = 0;
        }
    }
    sp.flush();
    let got = sp.download();
    sp.gpu.check().unwrap();
    let plane = (w * h) as usize;
    let mut bad = 0;
    let after: Vec<u32> = (0..8u32).map(|x| x as usize).chain([DID_PLANE]).flat_map(|pos| (0..h).flat_map(move |y| (0..w).map(move |x| (pos, x, y)))).map(|(pos, x, y)| store.read(x, y, pos)).collect();
    let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    eprintln!("cpu raster changed {changed} of {} words", before.len());
    for (cpu, g) in (0..8u32).map(|x| (x as usize, x as usize)).chain([(DID_PLANE, 8)]) {
        for y in 0..h {
            for x in 0..w {
                let a = got[g * plane + (y * w + x) as usize];
                let b = store.read(x, y, cpu);
                if a != b {
                    if bad < 12 {
                        eprintln!("pos {cpu} ({x},{y}) gpu {a:08x} cpu {b:08x}");
                    }
                    bad += 1;
                }
            }
        }
    }
    Some(Outcome { prims: n, drawn, bad })
}

fn check(o: Option<Outcome>, what: &str) {
    if let Some(o) = o {
        eprintln!("{what}: {} prims, {} set up, {} words differ", o.prims, o.drawn, o.bad);
        assert_eq!(o.bad, 0, "{what}");
    }
}

#[test]
fn flat_depth_triangles_match_cpu() {
    check(parity(160, 96, 400, 24, true, false, 1, 4096), "flat+depth");
}

#[test]
fn gouraud_depth_triangles_match_cpu() {
    check(parity(160, 96, 400, 24, false, false, 2, 4096), "gouraud+depth");
}

#[test]
fn large_gouraud_depth_triangles_match_cpu() {
    check(parity(256, 128, 200, 120, false, false, 3, 4096), "large gouraud+depth");
}

#[test]
fn full_fragment_chain_matches_cpu() {
    check(parity(160, 96, 600, 30, false, true, 4, 4096), "full chain");
}

#[test]
fn full_fragment_chain_small_batches_match_cpu() {
    check(parity(160, 96, 600, 30, false, true, 5, 37), "full chain, 37-prim batches");
}

/// Cost: wall time per phase for `KONA_SPIKE_PRIMS` triangles of about
/// `KONA_SPIKE_SIZE` pixels on a 1280x1024 target.
#[test]
#[ignore = "bench"]
fn bench() {
    let (w, h) = (1280u32, 1024u32);
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let n = env("KONA_SPIKE_PRIMS", 4096);
    let size = env("KONA_SPIKE_SIZE", 16) as i32;
    let reps = env("KONA_SPIKE_REPS", 5);
    let full = std::env::var("KONA_SPIKE_FULL").is_ok();
    let mode = std::env::var("KONA_SPIKE_MODE").unwrap_or_else(|_| "both".into());
    let mut r = Lcg(99);
    let store = PixelStore::new(w, h);
    fill_random(&store, w, h, &mut r);
    let states: Vec<DrawState> = (0..4).map(|_| if full { state_full(w, h, &mut r) } else { state_depth(w, h, &mut r) }).collect();
    let screen = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    // Set-up is shared and runs on the CPU either way.
    let t = Instant::now();
    let mut jobs = Vec::new();
    while jobs.len() < n {
        let si = r.next() as usize % states.len();
        let tri = rand_tri(&mut r, w, h, size, false);
        if let Ok(job) = setup::shaped(&tri, &states[si], 0, Shape::Plain) {
            jobs.push((si, job));
        }
    }
    let setup_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let pixels: u64 = jobs.iter().map(|(_, j)| ((j.rect.x1 - j.rect.x0) as u64) * ((j.rect.y1 - j.rect.y0) as u64)).sum();
    eprintln!("bench: {n} prims, size {size}, {:.0} box pixels/prim, full={full}, set-up {:.0} ns/prim", pixels as f64 / n as f64, setup_ns);
    if mode == "cpu" || mode == "both" {
        for rep in 0..reps {
            let t = Instant::now();
            for (si, job) in &jobs {
                pixel::run(&store, &RasterJob::Tri(*job), &states[*si], screen);
            }
            let ns = t.elapsed().as_nanos() as f64;
            eprintln!("cpu  rep {rep}: {:.3} ms/batch, {:.0} ns/prim", ns / 1e6, ns / n as f64);
        }
    }
    if mode == "gpu" || mode == "both" {
        let Some(gpu) = gpu() else { return };
        let mut sp = Spike::new(gpu, w, h);
        sp.upload(&store);
        for rep in 0..reps {
            let t = Instant::now();
            let mut slots: Vec<Option<u32>> = vec![None; states.len()];
            let mut i = 0;
            let mut batches = 0;
            while i < jobs.len() {
                let (si, job) = &jobs[i];
                let slot = match slots[*si] {
                    Some(s) => s,
                    None => {
                        let s = sp.batch.slot(slot_of(&states[*si], w * h)).unwrap_or_else(|_| unreachable!());
                        slots[*si] = Some(s);
                        s
                    }
                };
                let rect = job.rect;
                match sp.batch.prim(rec_of(job, slot), rect.x0, rect.y0, rect.x1 - 1, rect.y1 - 1) {
                    Ok(_) => i += 1,
                    Err(Full) => {
                        sp.flush();
                        batches += 1;
                        slots = vec![None; states.len()];
                    }
                }
            }
            let record = t.elapsed().as_nanos() as f64;
            sp.flush();
            batches += 1;
            sp.gpu.wait(None).unwrap();
            let ns = t.elapsed().as_nanos() as f64;
            eprintln!("gpu  rep {rep}: {:.3} ms total ({:.3} ms before the last flush), {} batches, {:.0} ns/prim", ns / 1e6, record / 1e6, batches, ns / n as f64);
        }
        sp.gpu.check().unwrap();
    }
}
