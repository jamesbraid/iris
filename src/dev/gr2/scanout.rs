//! GR2's scan-out on the GPU (`crate::wgpu::scanout`): `gr2comp::compose`
//! as a kernel over the frame buffer, so a window on the same device shows
//! the frame without it coming back to the CPU. The CPU compose stays the
//! reference, and what a build or host without wgpu runs.
//!
//! The raster stays on the CPU, so the frame buffer is uploaded whole when
//! the display marks something changed (5 MB), and the slot with it: the
//! tables the kernel reads, resolved here as the CPU compose resolves them.

use std::sync::Arc;

use crate::wgpu::{defines, storage_layout, Fragment, Gpu, Scanout, ScanoutPipeline, ScanoutShaders};

use super::bt457::Bt457;
use super::re3::{FB_H, FB_W};
use super::vc1::Vc1;
use super::xmap5::{Xmap5, CLUT_SIZE};

const SCANOUT: Fragment = Fragment::new("gr2_scanout.wgsl", include_str!("shaders/scanout.wgsl"));

// Slot word offsets (`shaders/scanout.wgsl` reads them as defines).
const S_CURSOR_ON: usize = 0;
const S_CURSOR_X: usize = 1;
const S_CURSOR_Y: usize = 2;
/// The three Bt457s' read mask and gamma, resolved per input value.
const S_DAC: usize = 4;
/// Mode word per DID, for each of the five XMAP5s.
const S_MODE: usize = S_DAC + 3 * 256;
/// Each XMAP5's blackout colour, 0x00RRGGBB.
const S_MISC: usize = S_MODE + 5 * 32;
/// The cursor glyph: 32 rows of two words, 2 bits a pixel.
const S_GLYPH: usize = S_MISC + 8;
/// Per row, the index of its first run (rows + 1 entries).
const S_ROWS: usize = S_GLYPH + 64;
const S_CLUT: usize = S_ROWS + FB_H + 1;
/// Window ID runs (x | did << 16) of all rows in order.
const S_RUNS: usize = S_CLUT + 5 * CLUT_SIZE;
const SLOT_WORDS: usize = S_RUNS + FB_H * Vc1::MAX_DID_RUNS;

/// The display thread's scan-out over the board's frame buffer.
pub(crate) struct Gr2Scanout {
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    vram: wgpu::Buffer,
    group: wgpu::BindGroup,
    slot: Vec<u32>,
}

impl Gr2Scanout {
    pub fn new(gpu: &Arc<Gpu>) -> Result<Gr2Scanout, String> {
        let layout = storage_layout(gpu, "gr2 scanout vram", true);
        let defines = defines![
            FB_W, FB_H, CLUT_SIZE, S_CURSOR_ON, S_CURSOR_X, S_CURSOR_Y, S_DAC, S_MODE, S_MISC, S_GLYPH, S_ROWS, S_CLUT, S_RUNS,
        ];
        let pipeline = ScanoutPipeline::new(
            gpu,
            &ScanoutShaders { label: "gr2", body: &[SCANOUT], library: &[], defines: &defines, layouts: &[&layout] },
        )?;
        let vram = gpu.buffer("gr2 vram", (FB_W * FB_H * 4) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gr2 scanout vram"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: vram.as_entire_binding() }],
        });
        let scanout = Scanout::new(gpu, &pipeline, (FB_W * FB_H) as u32, (SLOT_WORDS * 4) as u64);
        Ok(Gr2Scanout { pipeline, scanout, vram, group, slot: vec![0; SLOT_WORDS] })
    }

    /// Resolve the slot from the display state, as `gr2comp::compose` reads
    /// it; its used length in words.
    fn build(&mut self, vc1: &Vc1, xmap: &[Xmap5; 5], dac: &[Bt457; 3]) -> usize {
        let s = &mut self.slot;
        s[S_CURSOR_ON] = vc1.cursor_visible() as u32;
        let (cx, cy) = vc1.cursor_pos();
        s[S_CURSOR_X] = cx as u32;
        s[S_CURSOR_Y] = cy as u32;
        for (c, d) in dac.iter().enumerate() {
            for v in 0..256 {
                s[S_DAC + c * 256 + v] = d.lookup(v as u8) as u32;
            }
        }
        for (k, x) in xmap.iter().enumerate() {
            s[S_MODE + k * 32..S_MODE + (k + 1) * 32].copy_from_slice(&x.mode);
            s[S_MISC + k] = (x.misc[0] as u32) << 16 | (x.misc[1] as u32) << 8 | x.misc[2] as u32;
            s[S_CLUT + k * CLUT_SIZE..S_CLUT + (k + 1) * CLUT_SIZE].copy_from_slice(&x.clut);
        }
        s[S_GLYPH..S_GLYPH + 64].fill(0);
        if s[S_CURSOR_ON] != 0 {
            for y in 0..32 {
                for x in 0..32 {
                    s[S_GLYPH + y * 2 + x / 16] |= (vc1.cursor_pixel(x, y) as u32) << ((x % 16) * 2);
                }
            }
        }
        let mut runs = [(0u16, 0u8); Vc1::MAX_DID_RUNS];
        let mut n = 0;
        for y in 0..FB_H {
            s[S_ROWS + y] = n as u32;
            let count = vc1.line_did_runs(y, &mut runs);
            for &(x, did) in &runs[..count] {
                s[S_RUNS + n] = x as u32 | (did as u32) << 16;
                n += 1;
            }
        }
        s[S_ROWS + FB_H] = n as u32;
        S_RUNS + n
    }

    /// Upload the frame buffer and the slot and compose, no readback. The
    /// frame is in [`output`](Self::output) (0xAABBGGRR, rows of `FB_W`).
    pub fn compose(&mut self, vram: &[u32], vc1: &Vc1, xmap: &[Xmap5; 5], dac: &[Bt457; 3]) -> Result<(), String> {
        let n = self.build(vc1, xmap, dac);
        self.scanout.gpu().write(&self.vram, 0, bytemuck::cast_slice(vram));
        self.scanout.compose(&self.pipeline, FB_W as u32, FB_H as u32, bytemuck::cast_slice(&self.slot[..n]), &[&self.group])
    }

    /// Whether a frame has been composed, so [`output`](Self::output) holds one.
    pub fn composed(&self) -> bool {
        self.scanout.frames > 0
    }

    /// The device the scan-out composes on.
    pub fn gpu(&self) -> Arc<Gpu> {
        Arc::clone(self.scanout.gpu())
    }

    /// The composed frame's buffer on the device (0xAABBGGRR words, rows of `FB_W`).
    pub fn output(&self) -> &wgpu::Buffer {
        self.scanout.output()
    }

    /// The composed frame read back into `out` (`FB_W * FB_H` words).
    pub fn read_into(&mut self, out: &mut [u32]) -> Result<(), String> {
        self.scanout.read_output(FB_W as u32, FB_H as u32, out)
    }
}

/// The test device, or none on a host without an adapter.
#[cfg(test)]
pub(super) fn test_gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(|_| {})) {
        Ok(g) => Some(g),
        Err(e) => {
            if std::env::var_os("IRIS_REQUIRE_GPU").is_some_and(|v| v == "1") { panic!("no GPU: {e}") }
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// A frame composed on `gpu` and read back into rows of `OUT_STRIDE`.
#[cfg(test)]
pub(super) fn gpu_frame(gpu: &Arc<Gpu>, vram: &[u32], vc1: &Vc1, xmap: &[Xmap5; 5], dac: &[Bt457; 3]) -> Vec<u32> {
    let mut sc = Gr2Scanout::new(gpu).expect("scan-out pipeline");
    sc.compose(vram, vc1, xmap, dac).expect("compose");
    let mut words = vec![0u32; FB_W * FB_H];
    sc.read_into(&mut words).expect("read");
    let mut out = vec![0u32; super::gr2comp::OUT_STRIDE * FB_H];
    for y in 0..FB_H {
        out[y * super::gr2comp::OUT_STRIDE..][..FB_W].copy_from_slice(&words[y * FB_W..][..FB_W]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::gr2comp;
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }
    }

    fn set16(b: &mut [u8], a: usize, v: u16) {
        b[a] = (v >> 8) as u8;
        b[a + 1] = v as u8;
    }

    /// What a scene varies.
    struct Scene {
        seed: u64,
        /// Cursor on, and the position its registers hold.
        cursor: Option<(u16, u16)>,
        /// The video entry point: 0x800 applies the cursor's X offset.
        vid_ep: u16,
        /// No window ID table: one run of ID 0.
        no_did_table: bool,
        /// Modes that leave the overlay and popup planes out of the picture.
        plain_modes: bool,
        readmask: u8,
        /// Constrain every mode word to one pixel mode and PD mode.
        only: Option<(u32, u32)>,
    }

    impl Scene {
        fn new(seed: u64) -> Scene {
            Scene { seed, cursor: None, vid_ep: 0, no_did_table: false, plain_modes: false, readmask: 0xff, only: None }
        }
    }

    struct State {
        vram: Vec<u32>,
        vc1: Box<Vc1>,
        xmap: Box<[Xmap5; 5]>,
        dac: Box<[Bt457; 3]>,
    }

    fn state(sc: &Scene) -> State {
        let mut rng = Rng(sc.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        // SAFETY: these are plain data, and all-zero is a valid power-on state.
        let mut vc1: Box<Vc1> = unsafe { Box::new_zeroed().assume_init() };
        let mut xmap: Box<[Xmap5; 5]> = unsafe { Box::new_zeroed().assume_init() };
        let mut dac: Box<[Bt457; 3]> = unsafe { Box::new_zeroed().assume_init() };

        let vram: Vec<u32> = (0..FB_W * FB_H).map(|_| rng.next()).collect();

        for xm in xmap.iter_mut() {
            for m in xm.mode.iter_mut() {
                let mut mode = rng.next();
                if let Some((pix, pd)) = sc.only {
                    mode = (mode & !(7 << 24) & !(3 << 16)) | pix << 24 | pd << 16;
                }
                if sc.plain_modes {
                    mode &= !(0xf << 20) & !(3 << 5);
                }
                *m = mode;
            }
            for c in xm.clut.iter_mut() {
                *c = rng.next();
            }
            for m in xm.misc.iter_mut() {
                *m = rng.next() as u8;
            }
        }
        for d in dac.iter_mut() {
            d.readmask = sc.readmask;
            for p in d.palette.iter_mut() {
                *p = rng.next() as u8;
            }
        }

        set16(&mut vc1.regs, super::super::vc1::VID_EP, sc.vid_ep);
        if !sc.no_did_table {
            // Four line tables, rows alternating among them: one run, an
            // odd start, several runs and the longest the compositor follows.
            set16(&mut vc1.regs, super::super::vc1::DID_EP, 0xb00);
            let tables = [0x7000usize, 0x7100, 0x7200, 0x7300];
            for y in 0..FB_H {
                set16(&mut vc1.sram, 0xb00 + y * 2, tables[y % 4] as u16);
            }
            set16(&mut vc1.sram, tables[0], 1);
            set16(&mut vc1.sram, tables[0] + 2, (400 << 5) | 5);
            set16(&mut vc1.sram, tables[1], 0); // clamped to one run
            set16(&mut vc1.sram, tables[2], 6);
            let mut x = 0u32;
            for k in 0..6 {
                set16(&mut vc1.sram, tables[2] + 2 + k * 2, ((x << 5) | (rng.next() & 31)) as u16);
                x += 1 + rng.next() % 300;
            }
            set16(&mut vc1.sram, tables[3], 64);
            for k in 0..64 {
                set16(&mut vc1.sram, tables[3] + 2 + k * 2, (((k as u32 * 19) << 5) | (rng.next() & 31)) as u16);
            }
        }
        if let Some((xl, yl)) = sc.cursor {
            vc1.sysctl = super::super::vc1::SYSCTL_CURSOR | super::super::vc1::SYSCTL_CURSOR_DISPLAY;
            set16(&mut vc1.regs, super::super::vc1::CUR_EP, 0x100);
            set16(&mut vc1.regs, super::super::vc1::CUR_XL, xl);
            set16(&mut vc1.regs, super::super::vc1::CUR_YL, yl);
            for w in 0..128 {
                set16(&mut vc1.sram, 0x100 + w * 2, rng.next() as u16);
            }
        }
        State { vram, vc1, xmap, dac }
    }

    fn check(gpu: &Arc<Gpu>, name: &str, sc: &Scene) {
        let s = state(sc);
        let mut cpu = vec![0u32; gr2comp::OUT_STRIDE * FB_H];
        gr2comp::compose(&s.vram, &s.vc1, &s.xmap, &s.dac, &mut cpu);
        let got = gpu_frame(gpu, &s.vram, &s.vc1, &s.xmap, &s.dac);
        for y in 0..FB_H {
            for x in 0..FB_W {
                let i = y * gr2comp::OUT_STRIDE + x;
                assert_eq!(got[i], cpu[i], "{name}: pixel ({x}, {y}) differs: gpu {:#010x}, cpu {:#010x}", got[i], cpu[i]);
            }
        }
    }

    /// Random frame buffer words through random mode words, tables and
    /// gamma, with the window ID tables, overlays, popups and the cursor in
    /// and out of the picture: the GPU's frame is the CPU compose's, word
    /// for word. Returns without an adapter.
    #[test]
    fn gpu_scanout_matches_the_cpu_compose() {
        let Some(gpu) = test_gpu() else { return };
        eprintln!("gr2 scan-out parity on {}", gpu.caps);
        for seed in 1..=3 {
            check(&gpu, &format!("random modes, seed {seed}"), &Scene::new(seed));
        }
        // Each pixel mode as an RGB visual and as a colour index.
        for pix in 0..8 {
            for pd in [0, 1, 3] {
                let sc = Scene { plain_modes: true, only: Some((pix, pd)), ..Scene::new(10 + pix as u64) };
                check(&gpu, &format!("pixel mode {pix}, PD mode {pd}"), &sc);
            }
        }
        check(&gpu, "no window ID table", &Scene { no_did_table: true, ..Scene::new(20) });
        check(&gpu, "read mask", &Scene { readmask: 0x3c, ..Scene::new(21) });
        // The cursor: inside, on each edge and off the picture, with and
        // without the X offset of the 0x800 entry point.
        for (n, (xl, yl, ep)) in [(300, 200, 0), (550, 235, 0x800), (0, 0, 0), (1270, 1000, 0), (1530, 1050, 0x800), (260, 40, 0x800), (2000, 2000, 0)]
            .into_iter()
            .enumerate()
        {
            let sc = Scene { cursor: Some((xl, yl)), vid_ep: ep, ..Scene::new(30 + n as u64) };
            check(&gpu, &format!("cursor at ({xl}, {yl}), entry {ep:#x}"), &sc);
        }
    }

    /// A lost device stops the machine once and the board shows no frame
    /// from the CPU in its place.
    #[test]
    fn a_failed_gpu_is_fatal_and_publishes_no_frame() {
        use crate::display::{Composed, Head, Want};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let gpu = match Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(move |_| { seen.fetch_add(1, Ordering::SeqCst); })) {
            Ok(g) => g,
            Err(e) => {
            if std::env::var_os("IRIS_REQUIRE_GPU").is_some_and(|v| v == "1") { panic!("no GPU: {e}") }
            return eprintln!("skipped: {e}");
        }
        };
        let board = super::super::Gr2::with_gpu(super::super::Gr2Variant::Xz, crate::status::MachineStatus::detached(), Some(gpu.share()))
            .expect("the GPU scan-out");
        let mut screen = crate::disp::Rex3Screen::new();
        let want = Want { readback: false, gpu: false };
        assert!(matches!(Head::compose(&*board, &mut screen, want), Composed::Frame), "composes while the device is up");
        // Not `device.destroy()`: Vulkan and Metal report that late or not at all.
        gpu.fail("wgpu device lost (test): injected");
        assert!(matches!(Head::compose(&*board, &mut screen, want), Composed::NotReady));
        assert!(matches!(Head::compose(&*board, &mut screen, want), Composed::NotReady), "and keeps failing");
        assert!(gpu.check().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the machine hears it once");
        // A board without the GPU path still composes on the CPU.
        let cpu = super::super::Gr2::new(super::super::Gr2Variant::Xz, crate::status::MachineStatus::detached());
        assert!(matches!(Head::compose(&*cpu, &mut screen, want), Composed::Frame));
    }

    /// The slot holds every window ID run the CPU follows, and the used part
    /// of it is what is uploaded.
    #[test]
    fn slot_fits_the_longest_tables() {
        assert_eq!(SLOT_WORDS, S_RUNS + FB_H * Vc1::MAX_DID_RUNS);
        assert!(S_CLUT >= S_ROWS + FB_H + 1);
    }
}
