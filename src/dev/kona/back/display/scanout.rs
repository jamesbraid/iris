//! KONA's scan-out on the GPU (`crate::wgpu::scanout`): `compose` as a kernel,
//! one lane per screen pixel, so a window on the same device shows the frame
//! without it coming back to the CPU. The CPU compose stays the reference,
//! and what a build or host without wgpu runs.
//!
//! The pixel planes the compose reads (the colour words, the window-ID plane
//! and the overlay and popup planes) are copies of the store's, kept in one
//! GPU buffer. A store on the CPU is brought up to date each frame by
//! comparing it with a shadow of what the GPU holds, a 32 x 32 tile at a
//! time, and uploading the tiles that differ (`Planes::upload`, the one place
//! that names where the planes come from). A store whose planes live on the
//! GPU (`Store::Gpu`) copies the colour, second-buffer and window-ID planes
//! into the buffer on the device, with nothing compared or sent from the
//! host; the overlay and popup planes stay in the store's host copy and take
//! the comparison.
//!
//! The tables are resolved on the CPU into the display slot as the compose
//! reads them: the window kinds, the colour map through the gamma RAM, the
//! textport's map, direct colour's bytes and both cursors.

use std::sync::Arc;

use parking_lot::Mutex;

use super::tables::{Baked, Window, ENTRIES, IDS};
use super::ColourMap;
use crate::dev::kona::back::store::{Store, DID_PLANE, OVERLAY0_PLANE, OVERLAY1_PLANE, POPUP_PLANE};
use crate::dev::kona::defs::{OVERLAY_ID_FIRST, POPUP_ID};
use crate::wgpu::{Fragment, Gpu, Scanout, ScanoutPipeline, ScanoutShaders};

const SCANOUT: Fragment = Fragment::new("kona_scanout.wgsl", include_str!("shaders/scanout.wgsl"));

/// Tiles are compared and uploaded 32 pixels square.
const TILE: usize = 32;

/// The planes the kernel reads, in the order they sit in the buffer.
const PLANES: usize = 6;
const P_COLOUR: usize = 0;
const P_BACK: usize = 1;
const P_DID: usize = 2;
const P_OV0: usize = 3;
const P_OV1: usize = 4;
const P_POPUP: usize = 5;

// Slot word offsets (`shaders/scanout.wgsl` reads them as defines).
const H_FLAGS: usize = 0;
const H_W: usize = 1;
const H_H: usize = 2;
const C_ON: usize = 3;
const C_X: usize = 4;
const C_Y: usize = 5;
const C_THIRD: usize = 6;
const C_FG: usize = 7;
const C_BG: usize = 8;
const D_ON: usize = 9;
const D_X: usize = 10;
const D_Y: usize = 11;
const D_FG: usize = 12;
const D_BG: usize = 13;
/// The textport's colour map.
const S_MAP: usize = 16;
/// Two words a display ID: kind, bank, planes and opaque; then the map base.
const S_WIN: usize = S_MAP + super::COLOUR_MAP_ENTRIES;
/// Direct colour through the gamma RAM, a word for each byte value of red, green and blue.
const S_LUT: usize = S_WIN + IDS * 2;
/// The glyph cursor's 64 rows of four words.
const S_GLYPH: usize = S_LUT + 3 * 256;
/// The DAC cursor's 64 rows of four words, 16 pixels of two bits each.
const S_DAC: usize = S_GLYPH + 256;
const S_CMAP: usize = S_DAC + 256;
const SLOT_WORDS: usize = S_CMAP + ENTRIES;

const F_BLANK: u32 = 1;
const F_DEFAULT: u32 = 2;
const F_LAYERS: u32 = 4;

const K_TEXTPORT: u32 = 0;
const K_INDEX8: u32 = 1;
const K_INDEX12: u32 = 2;
const K_DIRECT: u32 = 3;
const K_LAYER: u32 = 4;
const K_BLACK: u32 = 5;

fn defines() -> Vec<(&'static str, u32)> {
    vec![
        ("P_COLOUR", P_COLOUR as u32),
        ("P_BACK", P_BACK as u32),
        ("P_DID", P_DID as u32),
        ("P_OV0", P_OV0 as u32),
        ("P_POPUP", P_POPUP as u32),
        ("H_FLAGS", H_FLAGS as u32),
        ("H_W", H_W as u32),
        ("H_H", H_H as u32),
        ("C_ON", C_ON as u32),
        ("C_X", C_X as u32),
        ("C_Y", C_Y as u32),
        ("C_THIRD", C_THIRD as u32),
        ("C_FG", C_FG as u32),
        ("C_BG", C_BG as u32),
        ("D_ON", D_ON as u32),
        ("D_X", D_X as u32),
        ("D_Y", D_Y as u32),
        ("D_FG", D_FG as u32),
        ("D_BG", D_BG as u32),
        ("S_MAP", S_MAP as u32),
        ("S_WIN", S_WIN as u32),
        ("S_LUT", S_LUT as u32),
        ("S_GLYPH", S_GLYPH as u32),
        ("S_DAC", S_DAC as u32),
        ("S_CMAP", S_CMAP as u32),
        ("F_BLANK", F_BLANK),
        ("F_DEFAULT", F_DEFAULT),
        ("F_LAYERS", F_LAYERS),
        ("K_TEXTPORT", K_TEXTPORT),
        ("K_INDEX8", K_INDEX8),
        ("K_INDEX12", K_INDEX12),
        ("K_DIRECT", K_DIRECT),
        ("K_LAYER", K_LAYER),
        ("OVERLAY_FIRST", OVERLAY_ID_FIRST as u32),
        ("POPUP", POPUP_ID as u32),
    ]
}

/// The scan-out the display thread shares with the back end, which makes it
/// when the machine gives the board a GPU.
pub type Shared = Arc<Mutex<Option<GpuScanout>>>;

/// The planes in a GPU buffer and the shadow of what is in it.
struct Planes {
    buffer: wgpu::Buffer,
    /// What the buffer holds, plane after plane, rows of `w`.
    shadow: Vec<u32>,
    w: usize,
    h: usize,
    /// Tiles of the band being compared that changed.
    dirty: Vec<bool>,
}

impl Planes {
    fn new(gpu: &Gpu, w: usize, h: usize) -> Planes {
        let buffer = gpu.buffer("kona scanout planes", (PLANES * w * h * 4) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        Planes { buffer, shadow: vec![0; PLANES * w * h], w, h, dirty: Vec::new() }
    }

    /// Bring the planes `need` names up to date with the store's arrays at
    /// `positions`. A plane not needed keeps what it had, and the next frame
    /// that needs it finds the difference. An array not yet allocated is zeros,
    /// as it is in the buffer; arrays are never freed or moved.
    ///
    /// The store is read as it is, without waiting for drawing, as the CPU
    /// compose does.
    fn upload(&mut self, gpu: &Gpu, store: &Store, positions: [usize; PLANES], need: [bool; PLANES]) {
        let (w, h) = (self.w, self.h);
        let n = w * h;
        let tiles = w.div_ceil(TILE);
        for p in (0..PLANES).filter(|&p| need[p]) {
            if store.copy_plane(positions[p], &self.buffer, (p * n * 4) as u64) {
                continue;
            }
            let Some(array) = store.cpu().array(positions[p]) else { continue };
            let pitch = array.pitch();
            for band in 0..h.div_ceil(TILE) {
                let (y0, y1) = (band * TILE, ((band + 1) * TILE).min(h));
                self.dirty.clear();
                self.dirty.resize(tiles, false);
                for y in y0..y1 {
                    let row = &mut self.shadow[p * n + y * w..p * n + (y + 1) * w];
                    for (t, chunk) in row.chunks_mut(TILE).enumerate() {
                        let mut diff = 0u32;
                        for (i, s) in chunk.iter_mut().enumerate() {
                            // SAFETY: the pixel is inside the managed area, which the array covers.
                            let v = unsafe { array.load(y * pitch + t * TILE + i) };
                            diff |= v ^ *s;
                            *s = v;
                        }
                        self.dirty[t] |= diff != 0;
                    }
                }
                let mut t = 0;
                while t < tiles {
                    if !self.dirty[t] {
                        t += 1;
                        continue;
                    }
                    let t0 = t;
                    while t < tiles && self.dirty[t] {
                        t += 1;
                    }
                    let (x0, x1) = (t0 * TILE, (t * TILE).min(w));
                    if x0 == 0 && x1 == w {
                        // Whole rows are contiguous.
                        self.write(gpu, p * n + y0 * w, p * n + y1 * w);
                    } else {
                        for y in y0..y1 {
                            self.write(gpu, p * n + y * w + x0, p * n + y * w + x1);
                        }
                    }
                }
            }
        }
    }

    /// Queue the shadow's words `from..to`.
    fn write(&self, gpu: &Gpu, from: usize, to: usize) {
        gpu.write(&self.buffer, from as u64 * 4, bytemuck::cast_slice(&self.shadow[from..to]));
    }
}

/// Which planes a frame reads, so a plane nothing looks at is not compared.
fn needs(tables: &Baked) -> [bool; PLANES] {
    if tables.is_blank() {
        return [false; PLANES];
    }
    let mut need = [false; PLANES];
    need[P_COLOUR] = true;
    if tables.is_default() {
        return need;
    }
    need[P_DID] = true;
    need[P_BACK] = (0..IDS).any(|id| tables.window(id) == Window::Direct { bank: 1 });
    if tables.has_layers() {
        need[P_OV0] = true;
        need[P_OV1] = true;
        need[P_POPUP] = true;
    }
    need
}

/// The display thread's scan-out over the store's planes.
pub struct GpuScanout {
    gpu: Arc<Gpu>,
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    planes: Planes,
    group: wgpu::BindGroup,
    slot: Vec<u32>,
}

impl GpuScanout {
    /// A scan-out of a `w` by `h` managed area on `gpu`. None when the planes
    /// do not fit the device's buffer limits: the board composes on the CPU,
    /// as a device without the capability would.
    pub fn new(gpu: &Arc<Gpu>, w: u32, h: u32) -> Result<Option<GpuScanout>, String> {
        let bytes = (PLANES as u64) * w as u64 * h as u64 * 4;
        if bytes > gpu.caps.max_storage_binding || bytes > gpu.caps.max_buffer {
            eprintln!("kona: scan-out on the CPU: {} MiB of planes do not fit the device's {} MiB storage binding", bytes >> 20, gpu.caps.max_storage_binding >> 20);
            return Ok(None);
        }
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kona scanout planes"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let defines = defines();
        let pipeline = ScanoutPipeline::new(gpu, &ScanoutShaders { label: "kona", body: &[SCANOUT], library: &[], defines: &defines, layouts: &[&layout] })?;
        let planes = Planes::new(gpu, w as usize, h as usize);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kona scanout planes"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: planes.buffer.as_entire_binding() }],
        });
        let scanout = Scanout::new(gpu, &pipeline, w * h, (SLOT_WORDS * 4) as u64);
        Ok(Some(GpuScanout { gpu: Arc::clone(gpu), pipeline, scanout, planes, group, slot: vec![0; SLOT_WORDS] }))
    }

    /// The frame composed last: 0xAABBGGRR words, rows of the managed width.
    pub fn output(&self) -> &wgpu::Buffer {
        self.scanout.output()
    }

    /// Resolve the slot from the tables, as `compose` reads them.
    fn build(&mut self, map: &ColourMap, t: &Baked) {
        let s = &mut self.slot;
        s[H_FLAGS] = t.is_blank() as u32 * F_BLANK | t.is_default() as u32 * F_DEFAULT | t.has_layers() as u32 * F_LAYERS;
        s[H_W] = self.planes.w as u32;
        s[H_H] = self.planes.h as u32;
        let near = |v: i64| v.clamp(-(1 << 24), 1 << 24) as i32 as u32;
        match t.glyph() {
            Some((x0, y0, [third, fg, bg], rows)) => {
                s[C_ON] = 1;
                (s[C_X], s[C_Y], s[C_THIRD], s[C_FG], s[C_BG]) = (near(x0), near(y0), third, fg, bg);
                s[S_GLYPH..S_GLYPH + 256].copy_from_slice(rows);
            }
            None => s[C_ON] = 0,
        }
        match t.dac_draw() {
            Some(d) => {
                s[D_ON] = 1;
                (s[D_X], s[D_Y], s[D_FG], s[D_BG]) = (near(d.x0 as i64), near(d.y0 as i64), d.foreground, d.background);
                s[S_DAC..S_DAC + 256].fill(0);
                for y in 0..super::dac_cursor::SIZE {
                    for x in 0..super::dac_cursor::SIZE {
                        s[S_DAC + y * 4 + x / 16] |= (d.cursor.pixel(x, y) as u32) << ((x % 16) * 2);
                    }
                }
            }
            None => s[D_ON] = 0,
        }
        for i in 0..super::COLOUR_MAP_ENTRIES {
            s[S_MAP + i] = map.get(i as u32);
        }
        for id in 0..IDS {
            let (word, base) = match t.window(id) {
                Window::Textport => (K_TEXTPORT, 0),
                Window::Index8 { base } => (K_INDEX8, base),
                Window::Index12 { base } => (K_INDEX12, base),
                Window::Direct { bank } => (K_DIRECT | (bank as u32 & 1) << 8, 0),
                Window::Layer { base, planes, opaque, bank } => (K_LAYER | (bank as u32 & 1) << 8 | (planes as u32) << 16 | (opaque as u32) << 24, base),
                Window::Cleared | Window::Unsupported(_) => (K_BLACK, 0),
            };
            s[S_WIN + id * 2] = word;
            s[S_WIN + id * 2 + 1] = base;
        }
        for (c, lut) in t.lut().iter().enumerate() {
            for (v, b) in lut.iter().enumerate() {
                s[S_LUT + c * 256 + v] = *b as u32;
            }
        }
        s[S_CMAP..S_CMAP + ENTRIES].copy_from_slice(t.cmap());
    }

    /// Bring the planes up to date, resolve the tables and compose; no
    /// readback. `word` is the colour word position of the main window.
    pub fn compose(&mut self, store: &Store, word: usize, map: &ColourMap, tables: &Baked) -> Result<(), String> {
        if (store.cpu().width() as usize, store.cpu().height() as usize) != (self.planes.w, self.planes.h) {
            return Err("the managed area changed size".into());
        }
        let positions = [word, word + 1, DID_PLANE, OVERLAY0_PLANE, OVERLAY1_PLANE, POPUP_PLANE];
        self.planes.upload(&self.gpu, store, positions, needs(tables));
        self.build(map, tables);
        let (w, h) = (self.planes.w as u32, self.planes.h as u32);
        self.scanout.compose(&self.pipeline, w, h, bytemuck::cast_slice(&self.slot), &[&self.group])
    }

    /// Read the last composed frame into `out`, rows of the managed width.
    pub fn read(&mut self, out: &mut [u32]) -> Result<(), String> {
        self.scanout.read_output(self.planes.w as u32, self.planes.h as u32, out)
    }

    /// Report a failure to the device, which stops the machine once; the
    /// error is sticky, so every later compose fails too.
    pub fn fail(&self, why: &str) {
        self.gpu.fail(&format!("kona scan-out: {why}"));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tables::Tables;
    use super::super::{compose, dac_cursor};
    use super::*;
    use crate::dev::kona::back::store::Store;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }
    }

    /// The test device, or none on a host without an adapter.
    fn test_gpu() -> Option<Arc<Gpu>> {
        match Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(|_| {})) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("skipped: {e}");
                None
            }
        }
    }

    const W: u32 = 150;
    const H: u32 = 100;

    /// What a scene turns on.
    #[derive(Clone, Copy)]
    struct Scene {
        seed: u64,
        tables: bool,
        gamma: bool,
        layers: bool,
        glyph: bool,
        dac: bool,
        blank: bool,
    }

    impl Scene {
        fn new(seed: u64) -> Scene {
            Scene { seed, tables: true, gamma: false, layers: false, glyph: false, dac: false, blank: false }
        }
    }

    /// Random words in every plane the compose reads, and tables to match the scene.
    fn scene(sc: &Scene) -> (Store, ColourMap, Tables) {
        let mut rng = Rng(sc.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let store = Store::new(W, H);
        for y in 0..H {
            for x in 0..W {
                // Colour words with the index in the top byte, the low byte and the colour alike.
                store.write(x, y, 1, rng.next());
                store.write(x, y, 2, rng.next());
                // Main IDs 0 to 15 and overlay groups 0 to 31, so some name no entry.
                store.write(x, y, DID_PLANE, (rng.next() % 16) << 2 | (rng.next() % 32) << 8);
                store.write(x, y, OVERLAY0_PLANE, rng.next() & 0xff);
                store.write(x, y, OVERLAY1_PLANE, rng.next() & 0xff);
                store.write(x, y, POPUP_PLANE, rng.next() & 0x3);
            }
        }
        let mut map = ColourMap::default();
        for i in 0..256 {
            map.load(&[i, rng.next() & 0x3ff, rng.next() & 0x3ff, rng.next() & 0x3ff]);
        }
        let mut t = Tables::default();
        if sc.tables {
            let colours: Vec<u32> = (0..ENTRIES).map(|_| rng.next() & 0x3fff_ffff).collect();
            t.load_colours(0, &colours);
            // Every kind: 8 and 12-bit indices (fields 7 and 11), 30-bit RGB (13), luminance (15), an odd width.
            for id in 0..16u32 {
                let field = [7, 11, 13, 15, 3, 7, 11, 1][(rng.next() % 8) as usize];
                let base = (rng.next() % (ENTRIES as u32 - 4096)) & !0xff;
                t.load_did(id, base << 7 | field << 3 | (rng.next() % 3) << 1);
                if rng.next() % 3 == 0 {
                    t.load_mode(id, [4, 0x14, 8, 1][(rng.next() % 4) as usize]);
                }
                if rng.next() % 3 == 0 {
                    t.load_bank(id, (rng.next() & 1) as u8);
                }
            }
            if sc.layers {
                // The popup is a 2-plane map, the overlays of 2, 4 and 8 planes, opaque or not, in either buffer.
                t.load_did(POPUP_ID as u32, 0x2810 << 7 | 1 << 3);
                for g in 0..32u32 {
                    let id = OVERLAY_ID_FIRST as u32 + g;
                    let planes = [1, 3, 7][(g % 3) as usize];
                    t.load_did(id, ((rng.next() % 0x7000) & !0xff) << 7 | planes << 3);
                    t.load_mode(id, 1 | (rng.next() % 2) << 4);
                    t.load_bank(id, (rng.next() & 1) as u8);
                }
            }
        }
        if sc.gamma {
            let page: Vec<u32> = (0..0x4000).map(|_| rng.next() & 0xfff).collect();
            t.load_gamma(0, &page);
        }
        if sc.glyph {
            let glyph: Vec<u32> = (0..260).map(|_| rng.next()).collect();
            t.load_cursor(0, &glyph);
            t.indirect(0x101, 0);
            // The box starts 32 left of and above this position, so it lands part off the frame.
            t.indirect(0x102, (rng.next() % 200) | (rng.next() % 150) << 13);
            t.indirect(0x103, 2);
        }
        if sc.dac {
            let mut words: Vec<u32> = (0..dac_cursor::WORDS).map(|_| rng.next()).collect();
            words[0] = 0;
            t.load_dac_cursor(dac_cursor::DacCursor::from_request(&words).unwrap());
            let (x, y) = ((rng.next() % 200) as i16 - 30, (rng.next() % 150) as i16 - 30);
            let [xl, xh] = x.to_le_bytes();
            let [yl, yh] = y.to_le_bytes();
            for (selector, value) in [(0xb00, 0x04), (0xb00, 0x02), (0xb02, 5), (0xb00, 0x00), (0xb00, 0x02), (0xb02, xl as u32), (0xb02, xh as u32), (0xb02, yl as u32), (0xb02, yh as u32)] {
                t.indirect(selector, value);
            }
        }
        t.set_blank(sc.blank);
        (store, map, t)
    }

    /// The scan-out's frame and the CPU compose's, word for word.
    fn assert_same(sc: &mut GpuScanout, name: &str, store: &Store, map: &ColourMap, tables: &Tables) {
        let baked = tables.snapshot();
        let mut cpu = vec![0u32; (W * H) as usize];
        compose(store, 1, map, &baked, &mut cpu, W as usize);
        sc.compose(store, 1, map, &baked).expect("compose");
        let mut got = vec![0u32; (W * H) as usize];
        sc.read(&mut got).expect("read");
        for y in 0..H as usize {
            for x in 0..W as usize {
                let i = y * W as usize + x;
                assert_eq!(got[i], cpu[i], "{name}: pixel ({x}, {y}) differs: gpu {:#010x}, cpu {:#010x}", got[i], cpu[i]);
            }
        }
    }

    /// Random planes through random tables, with the layers, gamma, both
    /// cursors and the blank in and out of the picture: the GPU's frame is the
    /// CPU compose's, word for word. Returns without an adapter.
    #[test]
    fn gpu_scanout_matches_the_cpu_compose() {
        let Some(gpu) = test_gpu() else { return };
        eprintln!("kona scan-out parity on {}", gpu.caps);
        let mut sc = GpuScanout::new(&gpu, W, H).expect("pipeline").expect("fits");
        for seed in 1..=3 {
            let (store, map, t) = scene(&Scene::new(seed));
            assert_same(&mut sc, &format!("tables, seed {seed}"), &store, &map, &t);
        }
        for seed in 10..13 {
            let s = Scene { gamma: true, layers: true, glyph: true, dac: true, ..Scene::new(seed) };
            let (store, map, t) = scene(&s);
            assert_same(&mut sc, &format!("everything, seed {seed}"), &store, &map, &t);
        }
        // The default composition: no tables, the textport's map by the low byte.
        let (store, map, t) = scene(&Scene { tables: false, ..Scene::new(20) });
        assert_same(&mut sc, "default", &store, &map, &t);
        let (store, map, t) = scene(&Scene { glyph: true, dac: false, ..Scene::new(21) });
        assert_same(&mut sc, "glyph only", &store, &map, &t);
        let (store, map, t) = scene(&Scene { dac: true, tables: false, ..Scene::new(22) });
        assert_same(&mut sc, "dac cursor over the default", &store, &map, &t);
        let (store, map, t) = scene(&Scene { blank: true, glyph: true, dac: true, layers: true, ..Scene::new(23) });
        assert_same(&mut sc, "blank", &store, &map, &t);
    }

    /// Only the tiles that changed are uploaded, and a plane that was not needed
    /// catches up when it is: a store changed between frames, and tables that
    /// turn the layers on later, show as the CPU compose shows them.
    #[test]
    fn changes_between_frames_reach_the_gpu() {
        let Some(gpu) = test_gpu() else { return };
        let mut sc = GpuScanout::new(&gpu, W, H).expect("pipeline").expect("fits");
        let (store, map, mut t) = scene(&Scene::new(30));
        assert_same(&mut sc, "first", &store, &map, &t);
        // A pixel on a tile edge, a tile corner and the last column and row.
        for (x, y) in [(31, 31), (32, 32), (W - 1, H - 1), (0, 0), (W - 1, 0)] {
            store.write(x, y, 1, 0x1234_5678 ^ x ^ y);
            store.write(x, y, DID_PLANE, 3 << 2);
            assert_same(&mut sc, &format!("pixel ({x}, {y})"), &store, &map, &t);
        }
        // A whole band of rows changes.
        for x in 0..W {
            store.write(x, 40, 1, x * 77);
        }
        assert_same(&mut sc, "a row", &store, &map, &t);
        // Planes that were not read so far are filled, then the tables ask for them.
        for x in 0..W {
            for y in 0..H {
                store.write(x, y, OVERLAY0_PLANE, (x + y) & 0xf);
                store.write(x, y, POPUP_PLANE, (x ^ y) & 3);
            }
        }
        let (_, _, layered) = scene(&Scene { layers: true, ..Scene::new(30) });
        assert_same(&mut sc, "layers on later", &store, &map, &layered);
        // A cleared store reads zeros.
        // SAFETY: nothing draws in this test.
        unsafe { store.reset() };
        assert_same(&mut sc, "reset", &store, &map, &layered);
        t.set_blank(true);
        assert_same(&mut sc, "blank", &store, &map, &t);
    }

    /// A lost device stops the machine once and the display shows no frame from
    /// the CPU in its place.
    #[test]
    fn a_failed_gpu_is_fatal_and_publishes_no_frame() {
        use super::super::screen::Display;
        use super::super::Compositor;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let gpu = match Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(move |_| { seen.fetch_add(1, Ordering::SeqCst); })) {
            Ok(g) => g,
            Err(e) => return eprintln!("skipped: {e}"),
        };
        let store = Arc::new(Store::new(W, H));
        let display = Arc::new(Display::detached());
        let scanout = GpuScanout::new(&gpu, W, H).expect("pipeline").expect("fits");
        let mut c = Compositor::new(store, 1, Arc::default(), Arc::default(), display.clone()).with_scanout(Arc::new(Mutex::new(Some(scanout))));
        let screen = display.out().clone();
        screen.requests.request();
        c.frame();
        assert!(screen.latest().is_some(), "composes while the device is up");
        gpu.device.destroy();
        // The loss callback runs from a poll, which a submission makes.
        gpu.poll();
        for _ in 0..2 {
            screen.requests.request();
            c.frame();
        }
        assert!(gpu.check().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the machine hears it once");
        assert_eq!(screen.latest().map(|f| f.seq), Some(1), "no frame is published after");
    }

    /// A store whose planes live on the GPU scans out the same frame as the CPU compose of a CPU
    /// store given the same draws, with the colour, back and window-ID planes copied on the device
    /// and the overlay and popup planes (host copy) compared and uploaded.
    #[test]
    fn a_gpu_store_scans_out_like_a_cpu_store() {
        use crate::dev::kona::back::raster::{ClearReq, DrawState, Rect};
        use crate::dev::kona::back::store::{StoreOp, texture::TextureStore};
        let Some(gpu) = crate::dev::kona::back::gpu::tests::shared_gpu() else { return };
        let _serial = crate::dev::kona::back::gpu::tests::serial();
        let mut sc = GpuScanout::new(&gpu, W, H).expect("pipeline").expect("fits");
        for seed in [31u64, 32] {
            let (cpu, map, t) = scene(&Scene { layers: true, gamma: true, ..Scene::new(seed) });
            let gs = Store::with_gpu(gpu.clone(), W, H, TextureStore::default()).expect("store");
            // The same random planes on the GPU: the host planes through `write`, and some clears
            // and boxes on top of them as records, applied to both.
            for w in [1usize, 2, DID_PLANE, OVERLAY0_PLANE, OVERLAY1_PLANE, POPUP_PLANE] {
                for y in 0..H {
                    for x in 0..W {
                        gs.write(x, y, w, cpu.read(x, y, w));
                    }
                }
            }
            let full = Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 };
            let ds = DrawState { clip: full, viewport: full, colour: [1, 2], colours: 2, ..DrawState::default() };
            let req = ClearReq { rect: Rect { x0: 10, y0: 5, x1: 90, y1: 60 }, colour: Some(0xff12_3456), ..Default::default() };
            for s in [&cpu, &gs] {
                s.apply(StoreOp::Clear { ds: &ds, req: &req });
                s.apply(StoreOp::FillBox { word: 1, x1: 40, y1: 20, x2: 120, y2: 80, value: 0x0300_0077 });
                s.apply(StoreOp::FillBox { word: DID_PLANE, x1: 0, y1: 0, x2: 60, y2: 40, value: 4 << 2 });
            }
            assert_same(&mut sc, &format!("gpu store, seed {seed}"), &gs, &map, &t);
            assert_eq!(cpu.checksum(), gs.checksum(), "the GPU store holds what the CPU store does");
        }
    }

    /// The slot holds every table the compose reads, and the planes stay under the binding.
    #[test]
    fn slot_regions_do_not_overlap() {
        assert!(S_WIN >= S_MAP + 256 && S_LUT >= S_WIN + IDS * 2 && S_GLYPH >= S_LUT + 768);
        assert!(S_DAC >= S_GLYPH + 256 && S_CMAP >= S_DAC + 256 && SLOT_WORDS == S_CMAP + ENTRIES);
        assert!(S_MAP > D_BG);
    }
}
