//! The direct scan-out over CPU-drawn planes against `SwCompositor`, pixel
//! for pixel: synthetic scenes covering every XMAP mode field, window-id
//! programs, cursors at the edges, scrolling and resizing; a sequence of
//! mutations uploaded by damage only; and a fuzz of random state. They run
//! on whatever adapter wgpu offers (`IRIS_WGPU_SOFTWARE=1` asks for a
//! software one) and skip with a note when there is none.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::Mutex;

use super::{Ng1Planes, Ng1Scanout};
use crate::compositor::{CompositorSource, SwCompositor};
use crate::dev::ng1::bt445::Bt445;
use crate::dev::ng1::cmap::Cmap;
use crate::dev::ng1::rex3_dirty::DamageRect;
use crate::dev::ng1::screen::{Ng1DisplaySource, Rex3Screen};
use crate::dev::ng1::vc2::{
    Vc2, VC2_CTRL_CURSOR_EN, VC2_CTRL_CURSOR_SIZE, VC2_CTRL_DID_EN, VC2_REG_CURRENT_CURSOR_X,
    VC2_REG_CURSOR_ENTRY_PTR, VC2_REG_DID_ENTRY_PTR, VC2_REG_DISPLAY_CONTROL, VC2_REG_WORKING_CURSOR_Y,
};
use crate::dev::ng1::vc2_timings::{apply_newport_resolution, NewportResolution};
use crate::dev::ng1::xmap9::Xmap9;
use crate::disp::FB_STRIDE;
use crate::wgpu::{Gpu, GpuOptions};

const DID_TAB: usize = 0x1000;
const PROG_BASE: usize = 0x2000;
const CURSOR_RAM: usize = 0x7000;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

fn gpu() -> Option<Arc<Gpu>> {
    let software = std::env::var_os("IRIS_WGPU_SOFTWARE").is_some_and(|v| v != "0");
    match Gpu::new(&GpuOptions { software, ..GpuOptions::default() }, Box::new(|_| {})) {
        Ok(g) => Some(g),
        Err(e) => {
            if std::env::var_os("IRIS_REQUIRE_GPU").is_some_and(|v| v == "1") { panic!("no GPU: {e}") }
            eprintln!("skipped: {e}");
            None
        }
    }
}

// ---- the rig ----------------------------------------------------------------

fn mode_word(buf_sel: bool, ovl_buf_sel: bool, msb_cmap: u32, pix_mode: u32, pix_size: u32, aux_pix_mode: u32, aux_msb: u32) -> u32 {
    buf_sel as u32 | (ovl_buf_sel as u32) << 1 | msb_cmap << 3 | pix_mode << 8 | pix_size << 10 | aux_pix_mode << 16 | aux_msb << 19
}

/// How the damage reaches the GPU planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upload {
    /// Both whole planes, every frame.
    Whole,
    /// The row bands the damage touches.
    Bands,
}

struct Rig {
    gpu: Arc<Gpu>,
    vc2: Mutex<Vc2>,
    xmap: Mutex<Xmap9>,
    cmap: Mutex<Cmap>,
    dac: Mutex<Bt445>,
    diag: AtomicU64,
    screen: Rex3Screen,
    rgb: Vec<u32>,
    aux: Vec<u32>,
    rgb_damage: Vec<DamageRect>,
    aux_damage: Vec<DamageRect>,
    sw: SwCompositor,
    planes: Ng1Planes,
    scan: Ng1Scanout,
    planes_valid: bool,
    last_size: (usize, usize),
}

impl Rig {
    fn new(gpu: &Arc<Gpu>, res: NewportResolution) -> Rig {
        let mut vc2 = Vc2::new();
        apply_newport_resolution(&mut vc2, res);
        vc2.regs[VC2_REG_DISPLAY_CONTROL as usize] |= VC2_CTRL_DID_EN;
        let mut rig = Rig {
            gpu: Arc::clone(gpu),
            vc2: Mutex::new(vc2),
            xmap: Mutex::new(Xmap9::new()),
            cmap: Mutex::new(Cmap::new(0)),
            dac: Mutex::new(Bt445::new()),
            diag: AtomicU64::new(0),
            screen: Rex3Screen::new(),
            rgb: vec![0; FB_STRIDE * 1024],
            aux: vec![0; FB_STRIDE * 1024],
            rgb_damage: Vec::new(),
            aux_damage: Vec::new(),
            sw: SwCompositor::new(),
            planes: Ng1Planes::new(gpu),
            scan: Ng1Scanout::new(gpu).unwrap_or_else(|e| panic!("{e}")),
            planes_valid: false,
            last_size: (0, 0),
        };
        // A DID program with one window until a scene sets more.
        rig.set_did(&[(0, vec![])], |_| 0);
        for (i, c) in rig.dac.lock().palette.iter_mut().enumerate() {
            *c = [i as u8; 3];
        }
        rig
    }

    /// DID programs: `(initial window id, [(x, id from x)])`; `line` names the
    /// program of display row `y`.
    fn set_did(&mut self, progs: &[(u8, Vec<(u16, u8)>)], line: impl Fn(usize) -> usize) {
        let mut vc2 = self.vc2.lock();
        for (p, (first, spans)) in progs.iter().enumerate() {
            let at = PROG_BASE + p * 64;
            assert!(spans.len() < 60);
            vc2.ram[at] = *first as u16;
            for (i, (x, id)) in spans.iter().enumerate() {
                vc2.ram[at + 1 + i] = (x << 5) | *id as u16;
            }
            vc2.ram[at + 1 + spans.len()] = 0x7ff << 5;
        }
        for y in 0..1024 {
            vc2.ram[DID_TAB + y] = (PROG_BASE + line(y) * 64) as u16;
        }
        vc2.regs[VC2_REG_DID_ENTRY_PTR as usize] = DID_TAB as u16;
        vc2.regs[VC2_REG_DISPLAY_CONTROL as usize] |= VC2_CTRL_DID_EN;
        vc2.dirty = true;
    }

    fn set_modes(&mut self, modes: &[u32], cursor_cmap: u8, popup_cmap: u8) {
        let mut x = self.xmap.lock();
        x.mode_table = [0; 32];
        for (i, m) in modes.iter().enumerate() {
            x.mode_table[i] = *m;
        }
        x.cursor_cmap_msb = cursor_cmap;
        x.popup_cmap_msb = popup_cmap;
        x.dirty = true;
    }

    fn set_cmap(&mut self, f: impl Fn(usize) -> u32) {
        let mut c = self.cmap.lock();
        for (i, v) in c.palette.iter_mut().enumerate() {
            *v = f(i) & 0x00ff_ffff;
        }
        c.dirty = true;
    }

    fn set_gamma(&mut self, f: impl Fn(usize) -> [u8; 3]) {
        let mut d = self.dac.lock();
        for (i, v) in d.palette.iter_mut().enumerate() {
            *v = f(i);
        }
        d.dirty = true;
    }

    fn set_cursor(&mut self, x: u16, y: u16, big: bool, glyph_seed: u32) {
        let mut vc2 = self.vc2.lock();
        let mut rng = Lcg(glyph_seed as u64 + 1);
        for i in 0..256 {
            vc2.ram[CURSOR_RAM + i] = rng.next() as u16;
        }
        vc2.regs[VC2_REG_CURSOR_ENTRY_PTR as usize] = CURSOR_RAM as u16;
        vc2.regs[VC2_REG_CURRENT_CURSOR_X as usize] = x;
        vc2.regs[VC2_REG_WORKING_CURSOR_Y as usize] = y;
        let ctrl = &mut vc2.regs[VC2_REG_DISPLAY_CONTROL as usize];
        *ctrl |= VC2_CTRL_CURSOR_EN;
        if big { *ctrl |= VC2_CTRL_CURSOR_SIZE } else { *ctrl &= !VC2_CTRL_CURSOR_SIZE }
        vc2.dirty = true;
    }

    fn move_cursor(&mut self, x: u16, y: u16) {
        let mut vc2 = self.vc2.lock();
        vc2.regs[VC2_REG_CURRENT_CURSOR_X as usize] = x;
        vc2.regs[VC2_REG_WORKING_CURSOR_Y as usize] = y;
        vc2.dirty = true;
    }

    fn fill_rgb(&mut self, r: DamageRect, mut f: impl FnMut(usize, usize) -> u32) {
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                self.rgb[y * FB_STRIDE + x] = f(x, y);
            }
        }
        self.rgb_damage.push(r);
    }

    fn fill_aux(&mut self, r: DamageRect, mut f: impl FnMut(usize, usize) -> u32) {
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                self.aux[y * FB_STRIDE + x] = f(x, y);
            }
        }
        self.aux_damage.push(r);
    }

    fn full(&self) -> DamageRect {
        DamageRect { x: 0, y: 0, width: FB_STRIDE, height: 1024 }
    }

    fn cpu_source<'a>(state: &'a Ng1DisplaySource<'_>, rgb: &'a [u32], aux: &'a [u32]) -> CompositorSource<'a> {
        CompositorSource {
            fb_rgb: rgb,
            fb_aux: aux,
            fb_rgb_damage: &[],
            fb_aux_damage: &[],
            did: state.did,
            xmap_mode: state.xmap_mode,
            xmap_config: state.xmap_config,
            xmap_cursor_cmap: state.xmap_cursor_cmap,
            xmap_popup_cmap: state.xmap_popup_cmap,
            cmap: state.cmap,
            ramdac: state.ramdac,
            vc2_ram: state.vc2_ram,
            vc2_regs: state.vc2_regs,
            topscan: state.topscan,
            cursor_x_adjust: state.cursor_x_adjust,
            width: state.width,
            height: state.height,
        }
    }

    /// Refresh the screen's display state (decoding the CPU's window ids as
    /// the reference), upload the planes by `mode`, and compose with the
    /// direct kernel: the frame (0xAABBGGRR, rows of `width`) and the GPU's
    /// window-id map.
    fn direct_frame(&mut self, mode: Upload) -> (Vec<u32>, Vec<u8>) {
        self.screen.refresh_display_state(&self.vc2, &self.xmap, &self.cmap, &self.dac, &self.diag);
        let (w, h) = (self.screen.width, self.screen.height);
        let resized = self.last_size != (w, h);
        self.last_size = (w, h);
        let whole = !self.planes_valid || resized || mode == Upload::Whole;
        let damage = (!whole).then_some((&self.rgb_damage[..], &self.aux_damage[..]));
        self.planes.sync(&self.gpu, &self.rgb, &self.aux, damage);
        self.planes_valid = true;
        let state = self.screen.display_state();
        self.scan.seed_did(&state).unwrap_or_else(|e| panic!("seed: {e}"));
        let frame = self.scan.compose(&state, &self.planes).unwrap_or_else(|e| panic!("direct compose: {e}"));
        let bytes = self.gpu.read_vec(&frame, &[(0, (w * h * 4) as u64)]).expect("readback");
        let out = bytemuck::cast_slice::<u8, u32>(&bytes).to_vec();
        let did = self.gpu.read_vec(self.scan.did_for_test(), &[(0, (FB_STRIDE * 1024) as u64)]).expect("did readback");
        (out, did)
    }
}

struct Diff {
    n: usize,
    first: Option<(usize, usize, u32, u32)>,
}

fn diff(a: &[u32], b: &[u32], w: usize) -> Diff {
    let mut d = Diff { n: 0, first: None };
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            d.n += 1;
            d.first.get_or_insert((i % w, i / w, *x, *y));
        }
    }
    d
}

struct Scores {
    pixels: usize,
    direct: usize,
    did: usize,
    distinct: usize,
}

impl Rig {
    /// One frame through the kernel and through `SwCompositor`, which is
    /// the reference.
    fn parity(&mut self, label: &str, mode: Upload) -> Scores {
        let (direct, did_gpu) = self.direct_frame(mode);
        let (w, h) = (self.screen.width, self.screen.height);
        let state = self.screen.display_state();
        let cpu = Rig::cpu_source(&state, &self.rgb, &self.aux);
        self.sw.compose_pixels(&cpu);
        // Both are 0xAABBGGRR: the kernel's frame and the CPU's compare as they are.
        let want: Vec<u32> = (0..h).flat_map(|y| self.sw.pixels()[y * FB_STRIDE..][..w].to_vec()).collect();
        let dd = diff(&direct, &want, w);
        if let Some((x, y, a, b)) = dd.first {
            eprintln!("{label} {mode:?}: first at ({x},{y}) kernel {a:08x} software {b:08x} did {}", state.did[y * FB_STRIDE + x]);
        }
        let did = (0..h)
            .map(|y| (0..w).filter(|&x| did_gpu[y * FB_STRIDE + x] != state.did[y * FB_STRIDE + x]).count())
            .sum();
        Scores { pixels: w * h, direct: dd.n, did, distinct: want.iter().collect::<std::collections::HashSet<_>>().len() }
    }
}

fn check(label: &str, mode: Upload, s: &Scores) {
    assert_eq!((s.direct, s.did), (0, 0), "{label} {mode:?}: {} of {} pixels differ, {} window ids", s.direct, s.pixels, s.did);
}

// ---- scenes -----------------------------------------------------------------

fn rect(x: usize, y: usize, width: usize, height: usize) -> DamageRect {
    DamageRect { x, y, width, height }
}

fn hash2(x: usize, y: usize) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9e3779b1) ^ (y as u32).wrapping_mul(0x85ebca6b);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b3c6d);
    h ^ (h >> 12)
}

/// 24-bit true-colour desktop, a root pattern with detail in every channel.
fn scene_rgb24(rig: &mut Rig) {
    rig.set_modes(&[mode_word(false, false, 0, 1, 3, 0, 0)], 7, 8);
    rig.set_cmap(|i| hash2(i, 3));
    rig.fill_rgb(rig.full(), |x, y| hash2(x, y) & 0x00ff_ffff);
    rig.fill_aux(rig.full(), |_, _| 0);
    rig.set_cursor(900, 500, false, 1);
}

fn scene_ci8(rig: &mut Rig) {
    rig.set_modes(&[mode_word(false, false, 5, 0, 1, 0, 0)], 7, 8);
    rig.set_cmap(|i| hash2(i, 9));
    rig.fill_rgb(rig.full(), |x, y| (x / 4 + y / 4) as u32 & 0xff | (hash2(x, y) & 0xff00));
    rig.fill_aux(rig.full(), |_, _| 0);
    rig.set_cursor(100, 100, true, 2);
}

fn scene_rgb_expand(rig: &mut Rig, pix_size: u32, buf_sel: bool) {
    rig.set_modes(&[mode_word(buf_sel, false, 3, 1, pix_size, 0, 0)], 7, 8);
    rig.fill_rgb(rig.full(), |x, y| hash2(x, y));
    rig.fill_aux(rig.full(), |_, _| 0);
    rig.set_cursor(10, 10, false, 3);
}

fn scene_overlay(rig: &mut Rig) {
    // 24-bit base; overlay planes (two buffers) and popup bits in blocks.
    rig.set_modes(
        &[mode_word(false, false, 0, 1, 3, 2, 2), mode_word(false, true, 0, 1, 3, 6, 3), mode_word(false, false, 0, 1, 3, 0, 0)],
        7,
        9,
    );
    rig.set_cmap(|i| hash2(i, 5));
    rig.fill_rgb(rig.full(), |x, y| hash2(x, y) & 0x00ff_ffff);
    rig.fill_aux(rig.full(), |x, y| {
        let h = hash2(x / 8, y / 8);
        let overlay = if h & 3 == 0 { (h >> 8) & 0xff } else { 0 };
        let overlay2 = if h & 5 == 0 { (h >> 16) & 0xff } else { 0 };
        let popup = if (h >> 28) & 3 == 0 { (h >> 4) & 3 } else { 0 };
        overlay << 8 | overlay2 << 16 | popup << 2
    });
    rig.set_did(&[(0, vec![(400, 1), (900, 2)])], |_| 0);
    rig.set_cursor(640, 400, false, 4);
}

/// Several window types by DID: root 24-bit, an 8-bit CI window with its
/// own cmap block, 12-bit double-buffered (both buffers), 4-bit expanded,
/// an overlay-bearing window, a popup-only strip.
fn scene_windows(rig: &mut Rig) {
    rig.set_modes(
        &[
            mode_word(false, false, 0, 1, 3, 0, 0),  // 0 root: 24-bit RGB
            mode_word(false, false, 4, 0, 1, 0, 0),  // 1 8-bit CI, cmap block 4
            mode_word(true, false, 0x10, 0, 2, 0, 0), // 2 12-bit CI, buf B
            mode_word(false, false, 0, 1, 2, 0, 0),  // 3 12-bit RGB, buf A
            mode_word(true, true, 0, 1, 2, 2, 5),    // 4 12-bit RGB buf B, overlay buf B
            mode_word(false, false, 9, 1, 1, 6, 6),  // 5 8-bit RGB332, overlay
            mode_word(false, false, 0, 1, 0, 0, 0),  // 6 4-bit RGB
            mode_word(true, false, 12, 0, 0, 7, 7),  // 7 4-bit CI buf B, overlay
        ],
        11,
        12,
    );
    rig.set_cmap(|i| hash2(i, 77));
    rig.fill_rgb(rig.full(), |x, y| hash2(x, y));
    rig.fill_aux(rig.full(), |x, y| {
        let h = hash2(x / 4, y / 4);
        ((h & 0xff) << 8) | (((h >> 8) & 0xff) << 16) | (if h & 0x300 == 0 { (h >> 20) & 3 } else { 0 }) << 2
    });
    let progs = vec![
        (0u8, vec![]),
        (0, vec![(100, 1), (500, 0), (700, 2), (1000, 3)]),
        (3, vec![(50, 4), (300, 5), (640, 6), (1100, 7)]),
        (7, vec![(10, 0), (11, 1), (13, 2), (1279, 3)]),
    ];
    rig.set_did(&progs, |y| match y {
        0..=99 => 0,
        100..=399 => 1,
        400..=699 => 2,
        700..=701 => 3,
        _ => (y / 7) % 3,
    });
    rig.set_cursor(333, 777, true, 6);
}

fn scene_cursor_edges(rig: &mut Rig, x: u16, y: u16, big: bool) {
    scene_rgb24(rig);
    rig.set_cursor(x, y, big, 11);
}

fn scene_scroll(rig: &mut Rig, topscan: usize) {
    scene_windows(rig);
    rig.screen.topscan = topscan;
}

fn run_parity(gpu: &Arc<Gpu>, label: &str, build: &dyn Fn(&mut Rig), res: NewportResolution, mode: Upload) -> Scores {
    let mut rig = Rig::new(gpu, res);
    build(&mut rig);
    let s = rig.parity(label, mode);
    check(label, mode, &s);
    s
}

#[test]
fn the_direct_scanout_matches_the_software_compositor_on_every_scene() {
    let Some(gpu) = gpu() else { return };
    let r = NewportResolution::Res1280x1024;
    let scenes: Vec<(&str, Box<dyn Fn(&mut Rig)>, NewportResolution)> = vec![
        ("ci8_cmap", Box::new(scene_ci8), r),
        ("rgb24", Box::new(scene_rgb24), r),
        ("rgb12_bufA", Box::new(|g| scene_rgb_expand(g, 2, false)), r),
        ("rgb12_bufB", Box::new(|g| scene_rgb_expand(g, 2, true)), r),
        ("rgb8_332", Box::new(|g| scene_rgb_expand(g, 1, false)), r),
        ("rgb4", Box::new(|g| scene_rgb_expand(g, 0, true)), r),
        ("overlay_popup", Box::new(scene_overlay), r),
        ("windows_8types", Box::new(scene_windows), r),
        ("cursor32_inside", Box::new(|g| scene_cursor_edges(g, 600, 500, false)), r),
        ("cursor64_inside", Box::new(|g| scene_cursor_edges(g, 600, 500, true)), r),
        ("cursor32_left_edge", Box::new(|g| scene_cursor_edges(g, 5, 300, false)), r),
        ("cursor64_top_left", Box::new(|g| scene_cursor_edges(g, 2, 2, true)), r),
        ("cursor32_right_edge", Box::new(|g| scene_cursor_edges(g, 1290, 200, false)), r),
        ("cursor64_bottom_edge", Box::new(|g| scene_cursor_edges(g, 640, 1050, true)), r),
        ("topscan_100", Box::new(|g| scene_scroll(g, 100)), r),
        ("topscan_wrap_1000", Box::new(|g| scene_scroll(g, 1000)), r),
        ("res_1024x768", Box::new(scene_windows), NewportResolution::Res1024x768),
        ("res_1280x960", Box::new(scene_overlay), NewportResolution::Res1280x960),
    ];
    for mode in [Upload::Whole, Upload::Bands] {
        for (name, build, res) in scenes.iter() {
            let s = run_parity(&gpu, name, &**build, *res, mode);
            assert!(s.distinct > 1, "{name}: a flat frame proves nothing");
        }
    }
}

/// Mutations on one rig, uploaded by damage only: palette and gamma
/// changes, plane blocks, a cursor move, a DID edit, a mode edit, scrolling
/// onto rows updated off-screen. The last step drops the damage report, so
/// the kernel must differ there: the harness notices a missed upload.
#[test]
fn damage_driven_uploads_track_every_mutation() {
    let Some(gpu) = gpu() else { return };
    let mut rig = Rig::new(&gpu, NewportResolution::Res1280x1024);
    scene_windows(&mut rig);
    let step = |name: &str, rig: &mut Rig| {
        let s = rig.parity(name, Upload::Bands);
        check(name, Upload::Bands, &s);
        rig.rgb_damage.clear();
        rig.aux_damage.clear();
    };
    step("initial", &mut rig);
    step("unchanged", &mut rig);
    rig.set_cmap(|i| hash2(i, 1234));
    step("colormap_change", &mut rig);
    rig.set_gamma(|i| [(255 - i) as u8, (i * 2) as u8, (i / 2) as u8]);
    step("gamma_change", &mut rig);
    rig.fill_rgb(rect(200, 300, 400, 250), |x, y| hash2(x ^ 55, y));
    step("rgb_block_400x250", &mut rig);
    rig.fill_aux(rect(700, 100, 123, 77), |x, y| hash2(x, y) & 0x00ff_ff0c);
    step("aux_block_123x77", &mut rig);
    rig.fill_rgb(rect(1270, 1000, 10, 24), |x, y| hash2(y, x));
    step("rgb_right_edge_strip", &mut rig);
    rig.fill_rgb(rect(1280, 10, 300, 20), |x, y| hash2(y, x));
    step("rgb_offscreen_x_1280_plus", &mut rig);
    rig.move_cursor(321, 123);
    step("cursor_move", &mut rig);
    rig.set_cursor(50, 60, false, 99);
    step("cursor_glyph_change", &mut rig);
    rig.set_did(&[(1, vec![(300, 2)]), (3, vec![(600, 0), (900, 5)])], |y| (y / 100) & 1);
    step("did_table_change", &mut rig);
    rig.set_modes(&[mode_word(false, false, 1, 0, 1, 0, 0); 8], 1, 2);
    step("xmap_modes_change", &mut rig);
    rig.screen.topscan = 17;
    step("topscan_17", &mut rig);
    // Offscreen pixmap rows are only in the planes; a later scroll shows them.
    rig.fill_rgb(rect(0, 600, 1280, 300), |x, y| hash2(x + 9, y + 9));
    rig.screen.topscan = 400;
    step("scroll_onto_updated_rows", &mut rig);
    // Negative control: pixels change but the damage list misses them.
    rig.fill_rgb(rect(300, 300, 200, 200), |x, y| hash2(x ^ 7, y ^ 7));
    rig.rgb_damage.clear();
    let s = rig.parity("missed_damage", Upload::Bands);
    assert!(s.direct > 0, "a plane that changed with no damage report must show as a difference");
}

/// Random modes, window-id programs, cursor, scroll and damage, frame
/// after frame, uploaded whole and by bands.
#[test]
fn the_direct_scanout_matches_the_software_compositor_on_random_state() {
    let Some(gpu) = gpu() else { return };
    let rounds: usize = std::env::var("IRIS_NG1_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let mut rng = Lcg(2026);
    for mode in [Upload::Whole, Upload::Bands] {
        let mut rig = Rig::new(&gpu, NewportResolution::Res1280x1024);
        scene_windows(&mut rig);
        for round in 0..rounds {
            // Random modes (any field combination), DID programs, cursor.
            let modes: Vec<u32> = (0..32).map(|_| rng.next() & 0x00ff_ffff).collect();
            rig.set_modes(&modes, rng.below(256) as u8, rng.below(256) as u8);
            rig.set_cmap(|i| hash2(i, round) ^ (i as u32).wrapping_mul(0x01000193));
            let seed = rng.next();
            rig.set_gamma(|i| {
                let h = hash2(i, seed as usize);
                [h as u8, (h >> 8) as u8, (h >> 16) as u8]
            });
            let nprog = 1 + rng.below(8) as usize;
            let progs: Vec<(u8, Vec<(u16, u8)>)> = (0..nprog)
                .map(|_| {
                    let n = rng.below(10) as usize;
                    let mut xs: Vec<u16> = (0..n).map(|_| rng.below(1300) as u16).collect();
                    xs.sort_unstable();
                    (rng.below(32) as u8, xs.into_iter().map(|x| (x, rng.below(32) as u8)).collect())
                })
                .collect();
            let r0 = rng.next();
            rig.set_did(&progs, |y| (hash2(y, r0 as usize) as usize) % nprog);
            rig.set_cursor(rng.below(1400) as u16, rng.below(1100) as u16, rng.below(2) == 0, rng.next());
            rig.screen.topscan = rng.below(1024) as usize;
            for _ in 0..(1 + rng.below(5)) {
                let w = 1 + rng.below(700) as usize;
                let h = 1 + rng.below(400) as usize;
                let x = rng.below(2048 - w as u32) as usize;
                let y = rng.below(1024 - h as u32) as usize;
                let s = rng.next() as usize;
                if rng.below(2) == 0 {
                    rig.fill_rgb(rect(x, y, w, h), |x, y| hash2(x + s, y));
                } else {
                    rig.fill_aux(rect(x, y, w, h), |x, y| hash2(x, y + s) & !0x3);
                }
            }
            let s = rig.parity(&format!("fuzz round {round}"), mode);
            check(&format!("fuzz round {round}"), mode, &s);
            rig.rgb_damage.clear();
            rig.aux_damage.clear();
        }
    }
}

/// A mouse move writes the cursor's 16-byte header and nothing else: no VC2
/// RAM, no window-id expansion, no tables.
#[test]
fn a_cursor_move_writes_only_the_cursor_header() {
    let Some(gpu) = gpu() else { return };
    let mut rig = Rig::new(&gpu, NewportResolution::Res1280x1024);
    scene_windows(&mut rig);
    let s = rig.parity("windows", Upload::Bands);
    check("windows", Upload::Bands, &s);
    let before = rig.scan.traffic();
    for i in 0..10u16 {
        rig.move_cursor(300 + 17 * i, 200 + 9 * i);
        let s = rig.parity("cursor move", Upload::Bands);
        check("cursor move", Upload::Bands, &s);
    }
    let after = rig.scan.traffic();
    assert_eq!(after.did_expansions.submitted, before.did_expansions.submitted, "a mouse move re-expanded the window ids");
    assert_eq!(after.display.attempted_bytes - before.display.attempted_bytes, 10 * 16);
    assert_eq!(after.frames_submitted - before.frames_submitted, 10);
}

/// Frames with nothing new upload nothing and compose the same pixels.
#[test]
fn an_unchanged_frame_uploads_no_display_state() {
    let Some(gpu) = gpu() else { return };
    let mut rig = Rig::new(&gpu, NewportResolution::Res1280x1024);
    scene_windows(&mut rig);
    let (first, _) = rig.direct_frame(Upload::Bands);
    let before = rig.scan.traffic();
    let (again, _) = rig.direct_frame(Upload::Bands);
    let after = rig.scan.traffic();
    assert!(first == again, "an unchanged frame composed different pixels");
    assert_eq!(after.display.attempted_bytes, before.display.attempted_bytes);
    assert_eq!(after.did_expansions.attempts, before.did_expansions.attempts);
}

/// Band uploads write the rows damage touches and no others.
#[test]
fn band_uploads_write_only_the_damaged_rows() {
    let Some(gpu) = gpu() else { return };
    let planes = Ng1Planes::new(&gpu);
    let words = vec![7u32; FB_STRIDE * 1024];
    let row = 4 * FB_STRIDE as u64;
    let rects = [rect(10, 100, 8, 16), rect(500, 110, 8, 16), rect(0, 900, 64, 32), rect(0, 1000, 1, 100)];
    // 100..126 merged, 900..932, and 1000 clipped to the last row.
    assert_eq!(planes.write_bands(&gpu, crate::dev::ng1::rex3_dirty::DamagePlane::Rgb, &words, &rects), (26 + 32 + 24) * row);
    assert_eq!(planes.write_bands(&gpu, crate::dev::ng1::rex3_dirty::DamagePlane::Aux, &words, &[]), 0);
    assert_eq!(planes.write_all(&gpu, crate::dev::ng1::rex3_dirty::DamagePlane::Aux, &words), 1024 * row);
}

/// A GPU that takes over from CPU-composed frames starts from the CPU's
/// window-id history, which the VC2 table alone cannot rebuild.
#[test]
fn the_scanout_takes_over_the_cpu_window_id_history() {
    let Some(gpu) = gpu() else { return };
    let mut rig = Rig::new(&gpu, NewportResolution::Res1280x1024);
    scene_windows(&mut rig);
    rig.screen.refresh_display_state(&rig.vc2, &rig.xmap, &rig.cmap, &rig.dac, &rig.diag);
    // A shorter table: rows below it keep the ids the CPU decoded before.
    rig.set_did(&[(5, vec![(200, 6)])], |_| 0);
    rig.vc2.lock().ram[DID_TAB + 300] = 0xffff;
    rig.screen.refresh_display_state(&rig.vc2, &rig.xmap, &rig.cmap, &rig.dac, &rig.diag);
    let state = rig.screen.display_state();
    rig.planes.sync(&rig.gpu, &rig.rgb, &rig.aux, None);
    assert!(rig.scan.compose(&state, &rig.planes).is_err(), "an unseeded history must be refused, not guessed");
    rig.scan.seed_did(&state).unwrap_or_else(|e| panic!("{e}"));
    let s = rig.parity("takeover", Upload::Whole);
    check("takeover", Upload::Whole, &s);
}
