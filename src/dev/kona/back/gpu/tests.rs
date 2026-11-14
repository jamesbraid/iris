//! The GPU arm against the CPU arm: the same ops through `Store::apply` on both, then every word
//! of every plane compared, straight from the GPU's planes and through the mirror after `observe`.
//! The CPU arm is the reference. Needs a GPU or llvmpipe; each test returns at once without one.
//!
//! `KONA_TEST_GPU` picks the device: a backend name (`vulkan`, `gl`) or `software` for the
//! software adapter on the GL backend.

use std::sync::Arc;

use super::*;
use crate::dev::kona::back::raster::ops::{BlendEq, BlendFactor, Compare, LogicOp, Ownership, StencilOp};
use crate::dev::kona::back::raster::setup::{self, Shape};
use crate::dev::kona::back::raster::{AlphaTest, BlendState, StencilState, Surface, WVertex};
use crate::dev::kona::back::store::Store;
use crate::dev::kona::back::x::fill::X_BOX;
use crate::dev::kona::back::x::pixel::{X_PIXEL_COPY, X_PIXEL_SETUP};
use crate::dev::kona::back::x::state::XState;
use crate::wgpu::GpuOptions;

/// One device for the whole test process, a handle of its own for each caller, or None without one.
pub(in crate::dev::kona::back) fn shared_gpu() -> Option<Arc<Gpu>> {
    static DEVICE: std::sync::OnceLock<Option<Arc<Gpu>>> = std::sync::OnceLock::new();
    DEVICE.get_or_init(open).as_ref().map(|g| g.share())
}

/// The GL backend's context lock times out when several threads drive it at once, so a test that
/// uses the device holds this for its whole run.
pub(in crate::dev::kona::back) fn serial() -> parking_lot::MutexGuard<'static, ()> {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    LOCK.lock()
}

fn gpu() -> Option<Arc<Gpu>> {
    shared_gpu()
}

fn open() -> Option<Arc<Gpu>> {
    let pick = std::env::var("KONA_TEST_GPU").unwrap_or_default();
    let opts = match pick.as_str() {
        "" => GpuOptions::default(),
        "software" => GpuOptions { backend: "gl".into(), software: true, ..Default::default() },
        backend => GpuOptions { backend: backend.into(), ..Default::default() },
    };
    match Gpu::new(&opts, Box::new(|why| panic!("GPU failed: {why}"))) {
        Ok(g) => {
            eprintln!("adapter: {}", g.caps);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    /// A float in [lo, hi).
    fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * ((self.next() & 0xff_ffff) as f32 / 16_777_216.0)
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

/// Every operation of the single-sample chain, drawn at random: colour words 1 and 2, depth 3,
/// stencil 4, ownership by the display-ID plane.
fn state_full(w: u32, h: u32, r: &mut Lcg) -> DrawState {
    let full = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    let mut d = DrawState {
        clip: full,
        viewport: full,
        colour: [1, 2],
        colours: 1 + r.coin(4) as u8,
        depth_word: 3,
        depth_bits: if r.coin(5) { 0 } else { 32 },
        depth_test: !r.coin(4),
        depth_func: pick(r, &CMP),
        depth_mask: !r.coin(3),
        ..DrawState::default()
    };
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

fn rand_tri(r: &mut Lcg, w: u32, h: u32, size: i32) -> [WVertex; 3] {
    let cx = r.range(-8, w as i32 + 8);
    let cy = r.range(-8, h as i32 + 8);
    let col = |r: &mut Lcg| [(r.next() % 8192) as u16, (r.next() % 8192) as u16, (r.next() % 8192) as u16, (r.next() % 8192) as u16];
    let mut v = [vtx(0.0, 0.0, [0; 4], 0); 3];
    for item in v.iter_mut() {
        let x = cx as f32 + (r.range(-size * 8, size * 8) as f32) / 8.0;
        let y = cy as f32 + (r.range(-size * 8, size * 8) as f32) / 8.0;
        *item = vtx(x, y, col(r), r.next());
    }
    v
}

/// The word positions the tests use: the record's first five and the display-ID plane.
const WORDS: [usize; 6] = [1, 2, 3, 4, 5, DID_PLANE];

/// Planes of random words, with a few too wide for an index target in word 1.
fn fill_random(store: &PixelStore, w: u32, h: u32, r: &mut Lcg, wide_one_in: u32, words: &[usize]) {
    for &pos in words {
        for y in 0..h {
            for x in 0..w {
                let v = match pos {
                    1 => {
                        if r.coin(wide_one_in) {
                            0x1_0000 | (r.next() & 0xffff)
                        } else {
                            r.next() & 0xffff
                        }
                    }
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

/// Both arms, starting from the same planes.
struct Pair {
    cpu: Store,
    gpu: Store,
    w: u32,
    h: u32,
    /// The word positions the test draws in and compares.
    words: Vec<usize>,
}

impl Pair {
    fn new(dev: Arc<Gpu>, w: u32, h: u32, r: &mut Lcg, wide_one_in: u32, chunk_planes: Option<u32>) -> Pair {
        Pair::with_textures(dev, w, h, r, wide_one_in, chunk_planes, &TextureStore::default)
    }

    /// As `new`, with texture memory `tex` makes (called once for each arm, so it must make the same words).
    fn with_textures(dev: Arc<Gpu>, w: u32, h: u32, r: &mut Lcg, wide_one_in: u32, chunk_planes: Option<u32>, tex: &dyn Fn() -> TextureStore) -> Pair {
        Pair::build(dev, w, h, r, wide_one_in, chunk_planes, tex, WORDS.to_vec())
    }

    #[allow(clippy::too_many_arguments, reason = "a pair is its device, size, random planes, chunking, texture memory and words")]
    fn build(dev: Arc<Gpu>, w: u32, h: u32, r: &mut Lcg, wide_one_in: u32, chunk_planes: Option<u32>, tex: &dyn Fn() -> TextureStore, words: Vec<usize>) -> Pair {
        let init = PixelStore::with_texture(w, h, tex());
        fill_random(&init, w, h, r, wide_one_in, &words);
        let gpu = match chunk_planes {
            Some(n) => Store::Gpu(GpuStore::chunked(dev, w, h, tex(), n).expect("store")),
            None => Store::with_gpu(dev, w, h, tex()).expect("store"),
        };
        let Store::Gpu(g) = &gpu else { unreachable!() };
        g.load(&init);
        Pair { cpu: Store::Cpu(init), gpu, w, h, words }
    }

    fn g(&self) -> &GpuStore {
        match &self.gpu {
            Store::Gpu(g) => g,
            Store::Cpu(_) => unreachable!(),
        }
    }

    fn screen(&self) -> Rect {
        Rect { x0: 0, y0: 0, x1: self.w as i32, y1: self.h as i32 }
    }

    /// Do `op` on both.
    fn both(&self, op: StoreOpOwned) {
        op.apply(&self.cpu);
        op.apply(&self.gpu);
    }

    /// Words the GPU's planes hold that differ from the CPU arm's, straight from the planes.
    fn planes_differ(&self) -> usize {
        let mut bad = 0;
        for &word in &self.words {
            let got = self.g().download(word);
            for y in 0..self.h {
                for x in 0..self.w {
                    let (a, b) = (got[(y * self.w + x) as usize], self.cpu.read(x, y, word));
                    if a != b {
                        if bad < 8 {
                            eprintln!("word {word} at ({x},{y}): gpu {a:08x}, cpu {b:08x}");
                        }
                        bad += 1;
                    }
                }
            }
        }
        bad
    }

    /// Words of the mirror that differ from the CPU arm's inside `r`, after an `observe` of `r`.
    fn mirror_differs(&self, word: usize, r: Rect) -> usize {
        self.gpu.observe(word, r);
        let m = self.gpu.cpu();
        let mut bad = 0;
        for y in r.y0.max(0)..r.y1.min(self.h as i32) {
            for x in r.x0.max(0)..r.x1.min(self.w as i32) {
                bad += (m.read(x as u32, y as u32, word) != self.cpu.read(x as u32, y as u32, word)) as usize;
            }
        }
        bad
    }

    fn finish(&self, what: &str) {
        assert_eq!(self.planes_differ(), 0, "{what}: words of the GPU planes differ from the CPU arm's");
        for &word in &self.words {
            assert_eq!(self.mirror_differs(word, self.screen()), 0, "{what}: word {word} of the mirror differs after observe");
        }
        assert_eq!(self.cpu.cpu().checksum(), self.gpu.cpu().checksum(), "{what}: the checksum of the observed mirror");
        self.g().state.lock().gpu.check().unwrap();
    }
}

/// The ops the tests make, owned so both arms can be handed the same.
#[derive(Clone)]
enum StoreOpOwned {
    Tri(DrawState, TriJob),
    Clear(DrawState, ClearReq),
    Fill { word: usize, x1: i64, y1: i64, x2: i64, y2: i64, value: u32 },
    Plot { word: usize, x: i64, y: i64, value: u32 },
    Bitmap { word: usize, x0: i64, y0: i64, width: u32, height: u32, bits: Vec<u32>, value: u32 },
}

impl StoreOpOwned {
    fn apply(&self, s: &Store) {
        let op = match self {
            StoreOpOwned::Tri(ds, job) => StoreOp::Tri { ds, job },
            StoreOpOwned::Clear(ds, req) => StoreOp::Clear { ds, req },
            StoreOpOwned::Fill { word, x1, y1, x2, y2, value } => StoreOp::FillBox { word: *word, x1: *x1, y1: *y1, x2: *x2, y2: *y2, value: *value },
            StoreOpOwned::Plot { word, x, y, value } => StoreOp::Plot { word: *word, x: *x, y: *y, value: *value },
            StoreOpOwned::Bitmap { word, x0, y0, width, height, bits, value } => StoreOp::Bitmap { word: *word, x0: *x0, y0: *y0, width: *width, height: *height, bits, value: *value },
        };
        s.apply(op);
    }
}

fn rand_clear(r: &mut Lcg, w: u32, h: u32) -> ClearReq {
    let (x0, y0) = (r.range(-6, w as i32), r.range(-6, h as i32));
    ClearReq {
        rect: Rect { x0, y0, x1: x0 + 1 + r.range(0, 60), y1: y0 + 1 + r.range(0, 40) },
        colour: r.coin(2).then(|| r.next()),
        depth: r.coin(2).then(|| r.next() & 0xffff_ff00),
        stencil: r.coin(2).then(|| r.next() as u8),
        colour_mask: [!r.coin(4), !r.coin(4), !r.coin(4), !r.coin(4)],
        depth_mask: !r.coin(4),
        stencil_mask: if r.coin(3) { 0xff } else { r.next() as u8 },
    }
}

fn rand_bitmap(r: &mut Lcg, w: u32, h: u32) -> StoreOpOwned {
    let (bw, bh) = (1 + r.next() % 40, 1 + r.next() % 40);
    let n = (bw * bh).div_ceil(32) as usize;
    // Sometimes fewer words than the size needs: the rest read as clear.
    let have = if r.coin(5) { n / 2 } else { n };
    StoreOpOwned::Bitmap {
        word: 1 + r.next() as usize % 3,
        x0: r.range(-20, w as i32 + 10) as i64,
        y0: r.range(-20, h as i32 + 10) as i64,
        width: bw,
        height: bh,
        bits: (0..have).map(|_| r.next() | r.next()).collect(),
        value: r.next(),
    }
}

fn rand_fill(r: &mut Lcg, w: u32, h: u32) -> StoreOpOwned {
    let (x, y) = (r.range(-10, w as i32 + 10) as i64, r.range(-10, h as i32 + 10) as i64);
    let word = 1 + r.next() as usize % 4;
    match r.next() % 6 {
        0 => StoreOpOwned::Plot { word, x, y, value: r.next() },
        // A reversed box draws nothing.
        1 => StoreOpOwned::Fill { word, x1: x + 30, y1: y + 20, x2: x, y2: y, value: r.next() },
        _ => StoreOpOwned::Fill { word, x1: x, y1: y, x2: x + r.range(0, 70) as i64, y2: y + r.range(0, 50) as i64, value: r.next() },
    }
}

#[test]
fn triangles_match_the_cpu_arm() {
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    for (seed, batch) in [(1u64, 4096usize), (2, 37)] {
        let (w, h) = (160u32, 96u32);
        let mut r = Lcg(seed);
        let p = Pair::new(dev.clone(), w, h, &mut r, 100, None);
        let states: Vec<DrawState> = (0..8).map(|_| state_full(w, h, &mut r)).collect();
        let (mut drawn, mut open) = (0, 0);
        for _ in 0..600 {
            let ds = &states[r.next() as usize % states.len()];
            let tri = rand_tri(&mut r, w, h, 30);
            let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) else { continue };
            p.both(StoreOpOwned::Tri(*ds, job));
            drawn += 1;
            open += 1;
            if open >= batch {
                // Observe something small so the batch goes out, as a reader would.
                p.gpu.observe(1, Rect { x0: 0, y0: 0, x1: 1, y1: 1 });
                open = 0;
            }
        }
        eprintln!("seed {seed}: {drawn} triangles; {:?}", p.g().stats());
        p.finish(&format!("triangles, seed {seed}, observe every {batch}"));
    }
}

#[test]
fn clears_boxes_and_bitmaps_match_the_cpu_arm() {
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (160u32, 96u32);
    let mut r = Lcg(3);
    let p = Pair::new(dev, w, h, &mut r, 100, None);
    let states: Vec<DrawState> = (0..6).map(|_| state_full(w, h, &mut r)).collect();
    for step in 0..500 {
        match r.next() % 10 {
            0..=2 => {
                let q = rand_clear(&mut r, w, h);
                let ds = states[r.next() as usize % states.len()];
                p.both(StoreOpOwned::Clear(ds, q));
            }
            3..=5 => {
                let ds = &states[r.next() as usize % states.len()];
                let tri = rand_tri(&mut r, w, h, 30);
                if let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) {
                    p.both(StoreOpOwned::Tri(*ds, job));
                }
            }
            6 | 7 => p.both(rand_fill(&mut r, w, h)),
            _ => p.both(rand_bitmap(&mut r, w, h)),
        }
        if step % 97 == 96 {
            assert_eq!(p.mirror_differs(1, Rect { x0: 10, y0: 10, x1: 90, y1: 60 }), 0, "step {step}");
        }
    }
    eprintln!("{:?}", p.g().stats());
    p.finish("clears, boxes and bitmaps");
}

/// A request as `x::fill`'s own random test makes it.
enum Step {
    Cmd(u32, Vec<u32>),
    Rows(Vec<u32>),
}

struct Req {
    steps: Vec<Step>,
    args: [u32; 2],
}

fn pack(a: i32, b: i32) -> u32 {
    (a as u32) << 16 | (b as u32 & 0xffff)
}

fn request(r: &mut Lcg, w: i64, h: i64, max_clips: u32) -> Req {
    let mut rnd = |n: u32| r.next() % n;
    let rect = |rnd: &mut dyn FnMut(u32) -> u32| -> [i64; 4] {
        let (x1, y1) = (rnd(w as u32 + 6) as i64 - 8, rnd(h as u32 + 6) as i64 - 6);
        let (bw, bh) = (6 + rnd(w as u32 - 4) as i64, 6 + rnd(h as u32 - 4) as i64);
        if rnd(12) == 0 {
            [x1 + bw, y1 + bh, x1, y1]
        } else {
            [x1, y1, x1 + bw, y1 + bh]
        }
    };
    let mut steps = Vec::new();
    let stippled = rnd(3) != 0;
    if stippled {
        let sizes = [1u32, 2, 3, 5, 8, 17, 32];
        let (bw, bh) = (sizes[rnd(7) as usize], sizes[rnd(7) as usize]);
        steps.push(Step::Cmd(0x13d, vec![1, bw, bh]));
        steps.push(Step::Rows((0..bh).map(|_| rnd(u32::MAX) | rnd(u32::MAX) << 7).collect()));
        let so = (rnd(80) as i64 - 40, rnd(80) as i64 - 40);
        steps.push(Step::Cmd(0x13e, vec![1, so.0 as i32 as u32, so.1 as i32 as u32]));
        steps.push(Step::Cmd(0x13c, vec![1]));
    }
    let pos = (rnd(40) as i64 - 20, rnd(40) as i64 - 20);
    steps.push(Step::Cmd(0x12a, vec![pos.0 as i32 as u32, pos.1 as i32 as u32, w as u32, h as u32]));
    let gx = rnd(16);
    let mask = match rnd(5) {
        0 | 1 => 0xffff,
        _ => 1 + rnd(0xffff),
    };
    let (fg, bg) = (rnd(0x1_0000), rnd(0x1_0000));
    let opaque = stippled && rnd(2) == 0;
    let word = |v: u32| (v & 0xfff0) << 4 | (v & 0xf) << 3;
    let mut gc = [0, 0x1ffe, 0, 0x1ffe, 0, 0, 0x180c, 0x40001, 1, 2 * gx, 5, 1, 1];
    gc[0] = word(fg);
    gc[2] = word(bg);
    gc[4] = (mask >> 4) & 0xfff | (mask & 0xf) << 20 | if opaque { 0x8000_0000 } else { 0 };
    steps.push(Step::Cmd(0x129, gc.to_vec()));
    match rnd(3) {
        0 => {}
        1 => {
            let c = rect(&mut rnd);
            steps.push(Step::Cmd(0x12b, vec![1, c[0] as i32 as u32, c[1] as i32 as u32, c[2] as i32 as u32, c[3] as i32 as u32]));
        }
        _ => {
            let n = 2 + rnd(max_clips - 1) as usize;
            let mut flat = vec![n as u32];
            for _ in 0..n {
                flat.extend(rect(&mut rnd).iter().map(|&v| v as i32 as u32));
            }
            let (lead, last) = flat.split_at(flat.len() - 5);
            for c in lead.chunks(4) {
                steps.push(Step::Cmd(0x145, c.to_vec()));
            }
            steps.push(Step::Cmd(0x12b, last.to_vec()));
        }
    }
    if rnd(4) == 0 {
        let s = [rnd(50) as i64 - 10, rnd(36) as i64 - 8, 10 + rnd(60) as i64, 8 + rnd(40) as i64];
        steps.push(Step::Cmd(0x12d, vec![s[0] as i32 as u32, s[1] as i32 as u32, s[2] as u32, s[3] as u32]));
    }
    let b = rect(&mut rnd);
    Req { steps, args: [pack((b[0] - pos.0) as i32, (b[1] - pos.1) as i32), pack((b[2] - pos.0) as i32, (b[3] - pos.1) as i32)] }
}

fn load(x: &mut XState, steps: &[Step]) {
    for s in steps {
        match s {
            Step::Cmd(sel, a) => {
                x.command(*sel, a);
            }
            Step::Rows(r) => x.stipple.rows(r),
        }
    }
}

/// Everything the GPU arm takes, interleaved with observes at random times, against the CPU arm.
/// `max_clips` above four makes some requests fall to the CPU.
fn run(seed: u64, steps: usize, max_clips: u32, chunk_planes: Option<u32>, triangles: bool) -> Option<(Stats, u64, u64)> {
    let _serial = serial();
    let dev = gpu()?;
    let (w, h) = (96u32, 64u32);
    let mut r = Lcg(seed);
    let p = Pair::new(dev, w, h, &mut r, 3000, chunk_planes);
    let states: Vec<DrawState> = (0..6).map(|_| state_full(w, h, &mut r)).collect();
    let (mut drawn, mut refused) = (0u64, 0u64);
    let (mut copies, mut copies_refused, mut refused_here) = (0u64, 0u64, 0u64);
    for step in 0..steps {
        let roll = r.next() % 100;
        if (40..55).contains(&roll) {
            let rq = request(&mut r, w as i64, h as i64, max_clips);
            let (cw, ch) = (1 + r.range(1, 40) as u32, 1 + r.range(1, 30) as u32);
            let setup_words = [1, 1, 1, 0, cw, ch, 0];
            let at = [r.range(-6, w as i32) as u32, r.range(-6, h as i32) as u32, r.range(-6, w as i32) as u32, r.range(-6, h as i32) as u32];
            let (mut xc, mut xg) = (XState::default(), XState::default());
            load(&mut xc, &rq.steps);
            load(&mut xg, &rq.steps);
            for (x, s) in [(&mut xc, &p.cpu), (&mut xg, &p.gpu)] {
                x.pixel_command(&Surface::new(s, 1), X_PIXEL_SETUP, &setup_words, true);
                x.pixel_command(&Surface::new(s, 1), X_PIXEL_COPY, &at, true);
            }
            let (c, rf) = (xc.pixel_counts().copies, xc.pixel_counts().bad_destination);
            // The GPU arm counts a copy it refuses as a copy, its refusals coming from its verdicts, and
            // one it runs on the CPU as the CPU arm does.
            assert_eq!(xg.pixel_counts().copies + xg.pixel_counts().bad_destination, c + rf, "seed {seed} step {step}: copies that reached the store");
            refused_here += xg.pixel_counts().bad_destination;
            copies += c + rf;
            copies_refused += rf;
        } else if triangles && roll < 40 {
            let ds = &states[r.next() as usize % states.len()];
            let tri = rand_tri(&mut r, w, h, 24);
            if let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) {
                p.both(StoreOpOwned::Tri(*ds, job));
            }
        } else if roll >= 85 {
            let word = WORDS[r.next() as usize % WORDS.len()];
            let (x0, y0) = (r.range(-4, w as i32), r.range(-4, h as i32));
            let region = Rect { x0, y0, x1: x0 + 1 + r.range(1, 60), y1: y0 + 1 + r.range(1, 40) };
            assert_eq!(p.mirror_differs(word, region), 0, "seed {seed} step {step}: word {word} of the mirror in {region:?}");
        } else if triangles && roll >= 75 {
            let q = rand_clear(&mut r, w, h);
            let ds = states[r.next() as usize % states.len()];
            p.both(StoreOpOwned::Clear(ds, q));
        } else {
            let rq = request(&mut r, w as i64, h as i64, max_clips);
            let (mut xc, mut xg) = (XState::default(), XState::default());
            load(&mut xc, &rq.steps);
            load(&mut xg, &rq.steps);
            xc.draw_command(&Surface::new(&p.cpu, 1), X_BOX, &rq.args);
            xg.draw_command(&Surface::new(&p.gpu, 1), X_BOX, &rq.args);
            let key = |x: &XState| (x.fills.drawn, x.fills.bad_destination, x.fills.bad_context);
            assert_eq!(key(&xc), key(&xg), "seed {seed} step {step}: the request's outcome differs");
            drawn += xg.fills.drawn;
            refused += xg.fills.bad_destination;
        }
    }
    let refused_copies = p.g().copies_refused();
    assert_eq!(refused_copies + refused_here, copies_refused, "seed {seed}: copies the GPU refused on a wide word, against the CPU arm's");
    p.finish(&format!("seed {seed}"));
    eprintln!("seed {seed}: {drawn} boxes drawn, {refused} requests refused, {copies} copies ({copies_refused} refused)");
    Some((p.g().stats(), drawn, refused))
}

fn report(what: &str, o: Option<(Stats, u64, u64)>) {
    if let Some((st, drawn, _)) = o {
        eprintln!("{what}: {st:?}");
        assert!(drawn > 0);
    }
}

#[test]
fn x_fills_match_the_cpu_arm() {
    for seed in [11, 12, 13] {
        report(&format!("fills, seed {seed}"), run(seed, 400, 4, None, false));
    }
}

#[test]
fn interleaved_ops_and_observes_match_the_cpu_arm() {
    for seed in [21, 22, 23, 24] {
        report(&format!("interleaved, seed {seed}"), run(seed, 500, 4, None, true));
    }
}

/// More than four clip rectangles: the GPU kernels do not take them, so those requests run on the CPU
/// against the mirror and go back.
#[test]
fn requests_the_kernels_do_not_take_run_on_the_cpu() {
    for seed in [41, 42] {
        let o = run(seed, 400, 7, None, false);
        if let Some((st, ..)) = &o {
            assert!(st.cpu_ops > 0, "some requests had more than four clip rectangles");
        }
        report(&format!("many clips, seed {seed}"), o);
    }
}

/// Planes spread over several chunks of the plane table.
#[test]
fn planes_in_several_chunks_match_the_cpu_arm() {
    for seed in [51, 52] {
        report(&format!("chunks of two planes, seed {seed}"), run(seed, 400, 4, Some(2), true));
    }
}

/// A store that holds more texture pages than the GPU's pool can has its textured triangles drawn
/// on the CPU against the mirror, among the triangles that have no texture, which the GPU still draws.
#[test]
fn textures_the_pool_cannot_hold_run_on_the_cpu() {
    use super::super::gpu_lib::tests::{random_desc, random_store, Flavour, Rng};
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (96u32, 64u32);
    let mut r = Lcg(61);
    let mut rng = Rng(0x7E27_0789);
    let p = Pair::with_textures(dev, w, h, &mut r, 100, None, &|| random_store(&mut Rng(0x6161)));
    // The test store holds 20 pages.
    p.g().state.lock().tex.limit_pages(10);
    let mut states: Vec<DrawState> = (0..4).map(|_| state_full(w, h, &mut r)).collect();
    states[0].texture = Some(random_desc(&mut rng, Flavour::Plain));
    states[1].texture = Some(random_desc(&mut rng, Flavour::Enhanced));
    let mut on_cpu = 0;
    for _ in 0..200 {
        let ds = &states[r.next() as usize % states.len()];
        let tri = rand_tex_tri(&mut r, w, h, 24);
        let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) else { continue };
        on_cpu += ds.texture.is_some() as u32;
        p.both(StoreOpOwned::Tri(*ds, job));
    }
    assert!(on_cpu > 0);
    assert_eq!(p.g().stats().cpu_ops, on_cpu as u64);
    p.finish("triangles, some on the CPU");
}

/// A reset zeroes the planes on both sides, and a restore of one plane reaches the GPU.
#[test]
fn reset_and_restore_reach_the_gpu() {
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (64u32, 32u32);
    let mut r = Lcg(71);
    let p = Pair::new(dev, w, h, &mut r, 100, None);
    p.both(StoreOpOwned::Fill { word: 1, x1: 0, y1: 0, x2: 30, y2: 20, value: 0x1234 });
    for s in [&p.cpu, &p.gpu] {
        // SAFETY: nothing draws concurrently in a test.
        s.apply(StoreOp::Reset(unsafe { crate::dev::kona::back::store::Drained::assume() }));
    }
    p.finish("after a reset");
    assert!(p.g().download(1).iter().all(|&v| v == 0));
    let words: Vec<u32> = (0..w * h).map(|i| i.wrapping_mul(2654435761)).collect();
    for s in [&p.cpu, &p.gpu] {
        // SAFETY: as above.
        s.apply(StoreOp::Restore { word: 2, words: &words, drained: unsafe { crate::dev::kona::back::store::Drained::assume() } });
    }
    p.finish("after a restore");
    assert_eq!(p.g().download(2), words);
}

/// A fill that reads its destination is answered from the width bound when records can have stored
/// nothing wider, and a read of tiles nothing wrote is answered by the mirror.
#[test]
fn widths_and_clean_tiles_are_tracked_per_tile() {
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (128u32, 64u32);
    let store = Store::with_gpu(dev, w, h, TextureStore::default()).expect("store");
    let Store::Gpu(g) = &store else { unreachable!() };
    let all = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    // Fresh planes are zero: nothing can be wide.
    assert!(g.proven(1, all, 0xff));
    // A narrow value in one tile, a wide one in another.
    store.apply(StoreOp::FillBox { word: 1, x1: 0, y1: 0, x2: 20, y2: 20, value: 0x1234 });
    store.apply(StoreOp::FillBox { word: 1, x1: 63, y1: 0, x2: 90, y2: 20, value: 0x12_3456 });
    let tile = |x0: i32| Rect { x0, y0: 0, x1: x0 + 30, y1: 30 };
    assert!(g.proven(1, tile(0), 0xffff), "16 bits fit a 16-bit target");
    assert!(!g.proven(1, tile(0), 0xff), "but not an 8-bit one");
    assert!(!g.proven(1, tile(64), 0xffff), "24 bits do not fit 16");
    assert!(g.proven(1, tile(64), 0xff_ffff));
    assert!(g.proven(1, Rect { x0: 100, y0: 40, x1: 128, y1: 64 }, 0xff), "tiles nothing wrote stay zero");
    // The first read of a written tile goes to the GPU, the second does not, and tiles that were
    // never written do not.
    let before = g.stats();
    g.observe(1, tile(0));
    let once = g.stats();
    assert_eq!(once.readbacks, before.readbacks + 1);
    g.observe(1, tile(0));
    g.observe(1, Rect { x0: 100, y0: 40, x1: 128, y1: 64 });
    assert_eq!(g.stats().readbacks, once.readbacks);
    assert_eq!(store.cpu().read(5, 5, 1), 0x1234);
    // Only the dirty tiles are read, whole.
    store.apply(StoreOp::FillBox { word: 1, x1: 0, y1: 0, x2: 4, y2: 4, value: 7 });
    let w0 = g.stats().readback_words;
    g.observe(1, all);
    assert_eq!(g.stats().readback_words - w0, 2 * 32 * 32, "the tile of the 4x4 box and the wide one nothing has read yet");
}

/// A triangle whose vertices carry texture words s/w, t/w, r/w, 1/w.
fn rand_tex_tri(r: &mut Lcg, w: u32, h: u32, size: i32) -> [WVertex; 3] {
    let mut v = rand_tri(r, w, h, size);
    for item in v.iter_mut() {
        item.t[0] = [r.f(-1.5, 2.5), r.f(-1.5, 2.5), r.f(-1.5, 2.5), r.f(0.4, 2.5)];
    }
    v
}

/// Both arms' texture memory, to change the same way.
fn tex_both(p: &Pair, change: impl Fn(&TextureStore)) {
    change(p.cpu.cpu().texture());
    change(p.gpu.cpu().texture());
}

/// Triangles under bound textures of every kind, with texture memory changing between them: the
/// GPU arm must follow the changes by writing only the pages that changed.
#[test]
fn textured_triangles_match_the_cpu_arm() {
    use super::super::gpu_lib::tests::{random_desc, random_store, Flavour, Rng};
    use crate::dev::kona::back::store::texture::PAGE_WORDS;
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (128u32, 80u32);
    let mut r = Lcg(81);
    let mut rng = Rng(0x7E27_0456);
    let p = Pair::with_textures(dev, w, h, &mut r, 100, None, &|| random_store(&mut Rng(0x5151)));
    let states: Vec<DrawState> = (0..9)
        .map(|k| {
            let mut ds = state_full(w, h, &mut r);
            let flavour = [Flavour::Plain, Flavour::Volume, Flavour::Enhanced][k % 3];
            ds.texture = Some(random_desc(&mut rng, flavour));
            ds
        })
        .collect();
    let mut drawn = 0;
    let mut pages_after_first = 0;
    for step in 0..360 {
        if step == 120 {
            // Few words of held pages, a page that did not exist, a cleared stretch and a dropped plane.
            pages_after_first = p.g().stats().tex_pages;
            tex_both(&p, |t| {
                assert!(t.ex().set(0, 5, 0x8040_2010));
                assert!(t.ex().set(1, 4 * PAGE_WORDS as u32 + 7, 0xff10_2030));
                t.ex().clear(2, PAGE_WORDS as u32 - 50, 200);
                t.ex().drop_plane(3);
            });
        }
        let ds = &states[r.next() as usize % states.len()];
        let tri = rand_tex_tri(&mut r, w, h, 36);
        let Ok(job) = setup::shaped(&tri, ds, 0, Shape::Plain) else { continue };
        p.both(StoreOpOwned::Tri(*ds, job));
        drawn += 1;
    }
    let st = p.g().stats();
    eprintln!("{drawn} textured triangles; {st:?}");
    assert_eq!(st.cpu_ops, 0, "every textured triangle ran on the GPU");
    assert_eq!(pages_after_first, 20, "the first sync wrote the pages the store held");
    let second = st.tex_pages - pages_after_first;
    assert_eq!(second, 4, "the second sync wrote the four pages that changed, not the store");
    p.finish("textured triangles");
}

/// A triangle's vertices with fog (coordinate / w and 1 / w) and a distance along the line.
fn rand_fog_tri(r: &mut Lcg, w: u32, h: u32, size: i32) -> [WVertex; 3] {
    let mut v = rand_tri(r, w, h, size);
    for item in v.iter_mut() {
        let q = r.f(0.3, 2.0);
        item.fog = [r.f(0.0, 12.0) * q, q];
        item.d = r.f(-4.0, 120.0);
    }
    v
}

/// What a draw state carries for the features past the plain chain, at random.
fn state_features(w: u32, h: u32, r: &mut Lcg) -> DrawState {
    use crate::dev::kona::back::raster::fog::Fog;
    use crate::dev::kona::defs::gl;
    let mut d = state_full(w, h, r);
    if r.coin(2) {
        let mode = [gl::LINEAR, gl::EXP, gl::EXP2][r.next() as usize % 3];
        d.fog = Fog::new(true, mode, r.f(0.02, 0.5), r.f(0.0, 4.0), r.f(2.0, 14.0), [r.f(0.0, 1.0), r.f(0.0, 1.0), r.f(0.0, 1.0)]);
    }
    if r.coin(2) {
        d.line_stipple = Some((r.next() as u16 | 0x0101, 1 + r.next() % 5));
    }
    d
}

/// Triangles of every shape (plain, the fan of a smoothed polygon or line, the square of a smoothed
/// point), with fog, a line stipple and the polygon stipple, as the CPU arm draws them.
#[test]
fn fog_smoothing_and_line_stipple_match_the_cpu_arm() {
    use crate::dev::kona::back::raster::setup::Outline;
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (128u32, 80u32);
    let mut r = Lcg(91);
    let p = Pair::new(dev, w, h, &mut r, 100, None);
    let states: Vec<DrawState> = (0..10).map(|_| state_features(w, h, &mut r)).collect();
    let (mut drawn, mut smooth, mut disc, mut fogged, mut stippled) = (0, 0, 0, 0, 0);
    for _ in 0..500 {
        let ds = &states[r.next() as usize % states.len()];
        let tri = rand_fog_tri(&mut r, w, h, 30);
        let line = r.coin(3);
        let shaped = match r.next() % 4 {
            0 => {
                let Some(o) = Outline::new(&tri, ds) else { continue };
                let inner = [r.coin(3), r.coin(3), r.coin(3)];
                smooth += 1;
                setup::shaped(&tri, ds, 0, Shape::Smooth { inner, outline: &o })
            }
            1 => {
                disc += 1;
                let c = (r.f(0.0, w as f32), r.f(0.0, h as f32));
                setup::shaped(&tri, ds, 0, Shape::Disc { x: c.0, y: c.1, r: r.f(0.6, 5.0) })
            }
            _ => setup::shaped(&tri, ds, 0, Shape::Plain),
        };
        let Ok(mut job) = shaped else { continue };
        job.line = line;
        fogged += ds.fog.is_some() as u32;
        stippled += (line && ds.line_stipple.is_some()) as u32;
        p.both(StoreOpOwned::Tri(*ds, job));
        drawn += 1;
    }
    eprintln!("{drawn} triangles ({smooth} smoothed polygons, {disc} points; {fogged} fogged, {stippled} line-stippled); {:?}", p.g().stats());
    assert_eq!(p.g().stats().cpu_ops, 0, "every triangle ran on the GPU");
    assert!(smooth > 20 && disc > 20 && fogged > 50 && stippled > 20);
    p.finish("fog, smoothing and line stipple");
}

/// A multisampled draw state: 4 or 8 samples at a pattern, colour samples in words 6.., depth after
/// them and stencil after those, with the mask and alpha options at random.
fn state_samples(w: u32, h: u32, r: &mut Lcg) -> DrawState {
    use crate::dev::kona::back::raster::multisample::Multisample;
    let mut d = state_features(w, h, r);
    let count = if r.coin(2) { 4 } else { 8 };
    let mut m = Multisample::default();
    m.configure(count);
    m.set_pattern(0x80a1 + r.next() % 7);
    m.alpha_to_mask = r.coin(3);
    m.alpha_to_one = r.coin(4);
    m.mask_on = r.coin(2);
    m.mask_value = r.f(0.0, 1.0);
    m.mask_invert = r.coin(4);
    let (cn, dn, sn) = (count as u32 * 24 / 32, if r.coin(4) { count as u32 * 16 / 32 } else { count as u32 * 24 / 32 }, count as u32 / 4);
    let (c0, d0, s0) = (6, 6 + cn, 6 + cn + dn);
    d.samples = m.draw((c0, cn), (d0, if r.coin(6) { 0 } else { dn }), (s0, if r.coin(6) { 0 } else { sn }));
    d
}

/// Triangles and clears in multisampled windows: the samples' tests and writes, the mean written
/// back to the pixel, the mask and alpha options, with every blend, logic operation and mask.
#[test]
fn multisampled_draws_match_the_cpu_arm() {
    use crate::dev::kona::back::raster::setup::Outline;
    let _serial = serial();
    let Some(dev) = gpu() else { return };
    let (w, h) = (96u32, 64u32);
    let mut r = Lcg(101);
    let mut words = WORDS.to_vec();
    words.extend(6..6 + 6 + 6 + 2);
    words.sort();
    let p = Pair::build(dev, w, h, &mut r, 100, None, &TextureStore::default, words);
    let states: Vec<DrawState> = (0..10).map(|_| state_samples(w, h, &mut r)).collect();
    let (mut tris, mut clears, mut smoothed) = (0, 0, 0);
    for _ in 0..450 {
        let ds = &states[r.next() as usize % states.len()];
        if r.coin(8) {
            p.both(StoreOpOwned::Clear(*ds, rand_clear(&mut r, w, h)));
            clears += 1;
            continue;
        }
        let tri = rand_fog_tri(&mut r, w, h, 28);
        let shaped = match r.next() % 5 {
            0 => {
                let Some(o) = Outline::new(&tri, ds) else { continue };
                smoothed += 1;
                setup::shaped(&tri, ds, 0, Shape::Smooth { inner: [r.coin(3), r.coin(3), r.coin(3)], outline: &o })
            }
            _ => setup::shaped(&tri, ds, 0, Shape::Plain),
        };
        let Ok(mut job) = shaped else { continue };
        job.line = r.coin(4);
        p.both(StoreOpOwned::Tri(*ds, job));
        tris += 1;
    }
    eprintln!("{tris} multisampled triangles ({smoothed} smoothed), {clears} clears; {:?}", p.g().stats());
    assert_eq!(p.g().stats().cpu_ops, 0, "every op ran on the GPU");
    assert!(tris > 300 && clears > 20);
    p.finish("multisampled draws");
}
