//! The GPU scan-out against the CPU compose it stands in for: random GBE
//! states over random memory must give the same pixels, and only tiles the
//! engine wrote are copied again.

use std::sync::Arc;

use super::*;
use crate::dev::o2gfx::gbe_reg::*;
use crate::ip32::RAM_BASE;
use crate::wgpu::GpuOptions;

fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default(), Box::new(|_| {})) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn gbe() -> Gbe {
    let t = Arc::new(std::sync::atomic::AtomicU64::new(0));
    Gbe::new(Arc::new(move || t.load(std::sync::atomic::Ordering::Relaxed)))
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
    fn one_in(&mut self, n: u32) -> bool {
        self.below(n) == 0
    }
}

const LIST: u32 = 0x0020_0000;
const OVERLAY_LIST: u32 = 0x0020_8000;
const DID_PAGE: u32 = 0x1f0;
/// A page of the bus no bank answers at.
const UNMAPPED: u16 = 0x3000;

fn put(g: &Gbe, off: u32, v: u32) {
    g.write32((0x1600_0000 + off) as u64, v);
}

/// Tile lists, tiles, window IDs, maps and a cursor, drawn at random.
/// Returns the first memory tile of the frame planes.
fn scene(rng: &mut Rng, ram: &O2Memory, g: &Gbe) -> u32 {
    let bpp_code = rng.below(4);
    let bpp = 1u32 << bpp_code.min(2);
    // Usually a width whose line fills whole 32-byte units, as hardware has.
    let w = if rng.one_in(5) { 40 + rng.below(600) } else { (2 + rng.below(40)) * 32 / bpp * 2 };
    let h = 1 + rng.below(300);
    let w = w.min(1000);
    let wt = w * bpp / 512;
    let rhs = (w * bpp % 512).div_ceil(32);
    let fb_h = if rng.one_in(6) { h / 2 + 1 } else { h };
    let per_row = wt + (rhs != 0) as u32;
    let mut next_page = 0x100u32;
    let mut new_tile = |rng: &mut Rng, ram: &O2Memory, sparse: bool| -> u16 {
        let page = next_page;
        next_page += 1;
        let bytes: Vec<u8> = (0..0x10000).map(|_| if sparse { if rng.one_in(3) { rng.below(0x20) as u8 } else { 0 } } else { rng.next() as u8 }).collect();
        ram.write((page << 16) as usize, &bytes);
        page as u16
    };
    let mut entry = |rng: &mut Rng, ram: &O2Memory, sparse: bool, first: &mut u16| -> u16 {
        match rng.below(20) {
            0 | 1 => 0,
            2 => UNMAPPED,
            3 if *first != 0 => *first,
            _ => {
                let t = new_tile(rng, ram, sparse);
                if *first == 0 {
                    *first = t;
                }
                t
            }
        }
    };
    let mut first = 0u16;
    let n = (per_row * fb_h.div_ceil(128)) as usize;
    for i in 0..n {
        let e = entry(rng, ram, false, &mut first);
        ram.write(LIST as usize + 2 * i, &e.to_be_bytes());
    }
    let ovr = rng.below(3) != 0;
    let ovr_wt = w / 512;
    let ovr_rhs = (w % 512).div_ceil(32);
    let ovr_per_row = ovr_wt + (ovr_rhs != 0) as u32;
    if ovr {
        let mut first = 0u16;
        for i in 0..(ovr_per_row * h.div_ceil(128)) as usize {
            let e = entry(rng, ram, true, &mut first);
            ram.write(OVERLAY_LIST as usize + 2 * i, &e.to_be_bytes());
        }
    }
    // Window IDs: a table of frame entries and line tables, random or shaped.
    let use_did = rng.below(3) != 0;
    if use_did {
        let mut page = vec![0u8; 0x10000];
        if rng.one_in(2) {
            page.iter_mut().for_each(|b| *b = rng.next() as u8);
        } else {
            // One frame entry: block 1, offset 0, lasting the whole frame, and
            // a line table of rising ends.
            page[0..4].copy_from_slice(&((1u32 << 19) | 2047).to_be_bytes());
            let mut x = 0u32;
            for k in 0..200 {
                x += rng.below(w / 8 + 1);
                let le = (rng.below(32) << 11) | x.min(2047);
                page[512 + 2 * k..512 + 2 * k + 2].copy_from_slice(&(le as u16).to_be_bytes());
            }
        }
        ram.write((DID_PAGE << 16) as usize, &page);
    }
    let modes: Vec<u32> = (0..32)
        .map(|_| {
            let typ = rng.below(8);
            let cm = if rng.one_in(2) { 0 } else { rng.below(32) };
            (rng.below(4)) | typ << 2 | cm << 5 | if rng.one_in(3) { 1 << 10 } else { 0 }
        })
        .collect();
    let mut regs: Vec<(u32, u32)> = vec![
        (CTRLSTAT, 0x300a_a000),
        (DOTCLOCK, 0x0010_020f),
        (VT_XYMAX, 0x0042_9690),
        (FRM_CONTROL_SHADOW, LIST | 1),
        (OVR_CONTROL_SHADOW, if ovr { OVERLAY_LIST | 1 } else { 0 }),
        (DID_CONTROL_SHADOW, if use_did { DID_PAGE | 1 << 16 } else { 0 }),
        (VT_XY, 0),
        (VT_HCMAP, w << 12),
        (VT_VCMAP, h << 12),
        (FRM_SIZE_TILE, (wt << 5) | rhs | (bpp_code << 13)),
        (OVR_WIDTH_TILE, (ovr_wt << 5) | ovr_rhs),
        (FRM_SIZE_PIXEL, fb_h << 16),
    ];
    for (i, m) in modes.iter().enumerate() {
        regs.push((MODE + 4 * i as u32, if use_did || i == 0 { *m } else { modes[0] }));
    }
    let cx = rng.below(w + 40) as i32 - 20;
    let cy = rng.below(h + 40) as i32 - 20;
    regs.push((CRS_CTL, rng.below(4) / 2));
    regs.push((CRS_POS, ((cy as u32 & 0xfff) << 16) | (cx as u32 & 0xfff)));
    for i in 0..3 {
        regs.push((CRS_CMAP1 + 4 * i, rng.next()));
    }
    for i in 0..64 {
        regs.push((CRS_GLYPH + 4 * i, if rng.one_in(3) { 0 } else { rng.next() }));
    }
    for i in 0..256 {
        regs.push((GMAP + 4 * i, rng.next()));
    }
    for i in 0..6144 {
        regs.push((CMAP + 4 * i, rng.next()));
    }
    for (off, v) in regs {
        put(g, off, v);
    }
    first as u32
}

fn cpu(g: &Gbe, ram: &O2Memory) -> Option<(u32, u32, Vec<u32>)> {
    let mut out = vec![0u32; 2048 * 2048];
    let (w, h) = g.render_from(ram, &mut out)?;
    out.truncate((w * h) as usize);
    Some((w, h, out))
}

fn on_gpu(sc: &mut O2Scanout, g: &Gbe, ram: &O2Memory, full: bool) -> Option<(u32, u32, Vec<u32>)> {
    let Composed::Frame(w, h) = sc.compose(g, ram, full).expect("compose") else { return None };
    let mut out = vec![0u32; (w * h) as usize];
    sc.read(&mut out).expect("read back");
    Some((w, h, out))
}

fn assert_same(what: &str, a: &Option<(u32, u32, Vec<u32>)>, b: &Option<(u32, u32, Vec<u32>)>) {
    let (Some((w, h, c)), Some((gw, gh, g))) = (a, b) else {
        assert_eq!(a.is_some(), b.is_some(), "{what}: one is off");
        return;
    };
    assert_eq!((w, h), (gw, gh), "{what}: size");
    let bad: Vec<(u32, u32, u32, u32)> = (0..c.len()).filter(|&i| c[i] != g[i]).take(4).map(|i| (i as u32 % w, i as u32 / w, c[i], g[i])).collect();
    assert!(bad.is_empty(), "{what}: (x, y, cpu, gpu) differ at {bad:x?}");
}

#[test]
fn random_gbe_states_come_out_as_the_cpu_compose_has_them() {
    let Some(gpu) = gpu() else { return };
    eprintln!("o2 scan-out parity on {}", gpu.caps);
    let mut busy = 0;
    for seed in 1..=24u64 {
        let ram = Arc::new(O2Memory::single(32 << 20));
        let g = gbe();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
        scene(&mut rng, &ram, &g);
        let mut sc = O2Scanout::new(&gpu, &ram).expect("scan-out");
        let want = cpu(&g, &ram);
        assert_same(&format!("seed {seed}"), &want, &on_gpu(&mut sc, &g, &ram, false));
        // The scenes must show something, or equal frames prove nothing.
        let (_, _, px) = want.expect("display on");
        let colours: std::collections::HashSet<u32> = px.iter().copied().collect();
        busy += (colours.len() > 64) as u32;
    }
    assert!(busy >= 20, "{busy} of 24 scenes had more than a few colours");
}

#[test]
fn the_display_off_composes_nothing() {
    let Some(gpu) = gpu() else { return };
    let ram = Arc::new(O2Memory::single(32 << 20));
    let g = gbe();
    let mut sc = O2Scanout::new(&gpu, &ram).expect("scan-out");
    assert_eq!(sc.compose(&g, &ram, true).expect("compose"), Composed::Off);
    assert!(cpu(&g, &ram).is_none());
}

/// The first frame copies every tile of the picture; after that, only tiles
/// the engine wrote. Registers are not memory, and a store by the CPU shows
/// at the next full refresh, as it shows at the next scan on the CPU.
#[test]
fn only_tiles_the_engine_wrote_are_copied_again() {
    let Some(gpu) = gpu() else { return };
    let ram = Arc::new(O2Memory::single(32 << 20));
    let g = gbe();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    // Plain planes: no holes, so every tile counts.
    let (tile, list) = (0x0100_0000u32, LIST);
    let bytes: Vec<u8> = (0..0x10000).map(|_| rng.next() as u8).collect();
    for t in 0..4u32 {
        ram.write((list + 2 * t) as usize, &(((tile >> 16) + t) as u16).to_be_bytes());
        ram.write((tile + t * 0x10000) as usize, &bytes);
    }
    for (off, v) in [
        (CTRLSTAT, 0x300a_a000u32),
        (DOTCLOCK, 0x0010_020f),
        (VT_XYMAX, 0x0042_9690),
        (FRM_CONTROL_SHADOW, list | 1),
        (VT_XY, 0),
        (VT_HCMAP, 512 << 12),
        (VT_VCMAP, 128 << 12),
        (FRM_SIZE_TILE, (4 << 5) | (2 << 13)),
        (FRM_SIZE_PIXEL, 128 << 16),
        (MODE, 5 << 2),
    ] {
        put(&g, off, v);
    }
    for i in 0..256 {
        put(&g, GMAP + 4 * i, i * 0x0101_0100);
    }
    let mut sc = O2Scanout::new(&gpu, &ram).expect("scan-out");
    let check = |sc: &mut O2Scanout, what: &str, full: bool| {
        assert_same(what, &cpu(&g, &ram), &on_gpu(sc, &g, &ram, full));
    };
    check(&mut sc, "first frame", false);
    assert_eq!(sc.tiles_uploaded, 4, "every tile of the picture comes over once");

    check(&mut sc, "nothing changed", false);
    assert_eq!(sc.tiles_uploaded, 4, "nothing marked, nothing copied");

    // The engine writes one word of the third tile.
    let mut win = ram.window();
    win.begin();
    win.put_be(RAM_BASE as usize + tile as usize + 2 * 0x10000 + 0x40, 4, 0xdead_beef);
    win.end();
    check(&mut sc, "engine write", false);
    assert_eq!(sc.tiles_uploaded, 5, "only the written tile");

    // A register is not memory.
    put(&g, CRS_CTL, 1);
    put(&g, CRS_POS, (10 << 16) | 20);
    put(&g, CRS_CMAP1, 0xff00_ff00);
    for i in 0..64 {
        put(&g, CRS_GLYPH + 4 * i, 0xffff_ffff);
    }
    check(&mut sc, "cursor", false);
    assert_eq!(sc.tiles_uploaded, 5, "a register marks no tile");

    // A store by the CPU marks nothing; the next full refresh finds it.
    ram.write(tile as usize + 0x10000 + 0x80, &[0x12, 0x34, 0x56, 0x78]);
    let stale = on_gpu(&mut sc, &g, &ram, false);
    assert_ne!(cpu(&g, &ram).map(|c| c.2), stale.map(|c| c.2), "the CPU store is not seen without a mark");
    check(&mut sc, "full refresh", true);
    assert_eq!(sc.tiles_uploaded, 9, "a full refresh copies the four tiles of the picture");

    // Memory put back wholesale invalidates every copy.
    let words: Vec<u32> = ram.bank_words().into_iter().flatten().copied().collect();
    ram.restore_words(&words).expect("restore");
    check(&mut sc, "restored", false);
    assert_eq!(sc.tiles_uploaded, 13, "a restore copies the picture again");
}

/// A picture past what the kernel holds is not a failure: it is left to the
/// CPU compose and the device is not touched.
#[test]
fn a_picture_past_the_pool_is_left_to_the_cpu_without_failing_the_gpu() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let Ok(gpu) = Gpu::new(&GpuOptions::default(), Box::new(move |_| { seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst); })) else { return };
    let ram = Arc::new(O2Memory::single(32 << 20));
    let g = gbe();
    // 1000 x 300 pixels of 32 bits: 8 tiles a row, 3 rows; give the pool no room.
    scene(&mut Rng(77), &ram, &g);
    let mut sc = O2Scanout::new(&gpu, &ram).expect("scan-out");
    sc.pool = Pool::new(0);
    assert_eq!(sc.compose(&g, &ram, true).expect("not a failure"), Composed::Unsupported);
    assert!(sc.shown.is_none());
    assert!(gpu.check().is_ok());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// A GPU that fails reports to the machine once and the compose returns the
/// error: there is no CPU substitute.
#[test]
fn a_failed_gpu_stops_the_scanout_and_does_not_fall_back() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let gpu = match Gpu::new(&GpuOptions::default(), Box::new(move |_| { seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst); })) {
        Ok(g) => g,
        Err(e) => return eprintln!("skipped: {e}"),
    };
    let ram = Arc::new(O2Memory::single(32 << 20));
    let g = gbe();
    scene(&mut Rng(5), &ram, &g);
    let mut sc = O2Scanout::new(&gpu.share(), &ram).expect("scan-out");
    sc.compose(&g, &ram, true).expect("a healthy device composes");
    gpu.device.destroy();
    gpu.poll();
    let e = sc.compose(&g, &ram, true).expect_err("a lost device cannot compose");
    assert!(e.contains("lost"), "{e}");
    assert!(sc.shown.is_none());
    assert!(sc.compose(&g, &ram, true).is_err(), "and stays failed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "the machine hears it once");
}
