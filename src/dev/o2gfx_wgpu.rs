//! The O2's GBE scan-out on the GPU (`crate::wgpu::scanout`): `Gbe::render`
//! (o2gfx.rs) as a kernel. RAM stays the authority: the tiles the lists name
//! are copied to the GPU when the screen is marked changed, and the display
//! slot is the registers, colour maps and window-ID runs resolved on the CPU.

use std::sync::Arc;

use crate::dev::crime_mem::O2Memory;
use crate::dev::o2gfx::{Gbe, ScanRegs};
use crate::gfx_display::{Frame, Need};
use crate::traits::{BusDevice, BUS_OK};
use crate::wgpu::{defines, storage_layout, Fragment, Gpu, Scanout, ScanoutPipeline, ScanoutShaders};

const SCANOUT: Fragment = Fragment::new("o2_scanout.wgsl", include_str!("o2gfx_wgpu/scanout.wgsl"));

/// A tile: 64 KB.
const TILE_BYTES: usize = 0x10000;
const TILE_WORDS: usize = TILE_BYTES / 4;
/// Tiles the GPU holds: a 2048 x 2048 frame of 32-bit pixels is 256, and its
/// overlay 64. Tiles past that read as not in memory.
const POOL_TILES: usize = 512;
/// The most pixels a picture may have (what `render` is given room for).
const MAX_PIXELS: u32 = 2048 * 2048;
/// Rows the run table covers: the height field is 12 bits.
const MAX_ROWS: usize = 4096;
/// Window-ID runs a frame may have; rows past them take window ID 0.
const MAX_RUNS: usize = 1 << 18;
/// Entries of the frame's tile list `render` reads, and of the overlay's.
const FRAME_LIST: usize = 65536;
const OVERLAY_LIST: usize = 4096;
/// A slot word for a tile that is not in memory.
const NO_TILE: u32 = 0xffff_ffff;

// Slot word indices (`o2gfx_wgpu/scanout.wgsl` reads them as defines).
const D_W: usize = 0;
const D_BPP: usize = 1;
const D_PER_ROW: usize = 2;
const D_LINE_BYTES: usize = 3;
const D_FB_H: usize = 4;
const D_FB_DMA: usize = 5;
const D_FT_N: usize = 6;
const D_FT_OFF: usize = 7;
const D_OVR_DMA: usize = 8;
const D_OVR_PER_ROW: usize = 9;
const D_OVR_LINE: usize = 10;
const D_OT_N: usize = 11;
const D_OT_OFF: usize = 12;
const D_USE_DID: usize = 13;
const D_CRS_ON: usize = 14;
const D_CX: usize = 15;
const D_CY: usize = 16;
const D_CRS_CMAP1: usize = 17;
const D_MODES: usize = 20;
const D_GMAP: usize = D_MODES + 32;
const D_GLYPH: usize = D_GMAP + 256;
const D_CMAP: usize = D_GLYPH + 64;
/// Per row, the first of its runs (rows + 1 entries).
const D_ROWS: usize = D_CMAP + 6144;
/// Runs (x | window ID << 16), then the frame's tile slots, then the overlay's.
const D_RUNS: usize = D_ROWS + MAX_ROWS + 1;
const SLOT_WORDS: usize = D_RUNS + MAX_RUNS + FRAME_LIST + OVERLAY_LIST;


/// The tile layout of the normal planes and the overlay (section 2.3 of the
/// GBE specification), as `render` finds it.
struct Geometry {
    bpp: usize,
    per_row: usize,
    line_bytes: usize,
    fb_h: usize,
    dma: bool,
    ovr_per_row: usize,
    ovr_line: usize,
    ovr_dma: bool,
}

impl Geometry {
    fn of(r: &ScanRegs) -> Geometry {
        let rhs = (r.frm_tile & 0x1f) as usize;
        let wt = ((r.frm_tile >> 5) & 0xff) as usize;
        let per_row = wt + (rhs != 0) as usize;
        let line_bytes = wt * 512 + rhs * 32;
        let ovr_rhs = (r.ovr_tile & 0x1f) as usize;
        let ovr_wt = ((r.ovr_tile >> 5) & 0xff) as usize;
        let ovr_per_row = ovr_wt + (ovr_rhs != 0) as usize;
        let ovr_line = ovr_wt * 512 + ovr_rhs * 32;
        Geometry {
            bpp: 1 << ((r.frm_tile >> 13) & 3).min(2),
            per_row,
            line_bytes,
            fb_h: (r.frm_pixel >> 16) as usize,
            dma: r.frm_control & 1 != 0 && per_row != 0 && line_bytes != 0,
            ovr_per_row,
            ovr_line,
            ovr_dma: r.ovr_control & 1 != 0 && ovr_per_row != 0 && ovr_line != 0,
        }
    }
}

/// The display thread's scan-out of GBE.
pub struct O2Scanout {
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    tiles: wgpu::Buffer,
    group: wgpu::BindGroup,
    slot: Vec<u32>,
    /// A tile as read (64-bit words), and as the GPU takes it.
    raw: Vec<u64>,
    stage: Vec<u32>,
    did: Vec<u32>,
    /// The memory tile each list entry names, this frame and as last copied:
    /// a list that moved is copied again.
    pages: Vec<Option<usize>>,
    copied: Vec<Option<usize>>,
    clipped: bool,
}

impl O2Scanout {
    pub fn new(gpu: &Arc<Gpu>) -> Result<O2Scanout, String> {
        let layout = storage_layout(gpu, "o2 scanout tiles", true);
        let defines = defines![
            TILE_WORDS, NO_TILE, D_W, D_BPP, D_PER_ROW, D_LINE_BYTES, D_FB_H, D_FB_DMA, D_FT_N, D_FT_OFF, D_OVR_DMA,
            D_OVR_PER_ROW, D_OVR_LINE, D_OT_N, D_OT_OFF, D_USE_DID, D_CRS_ON, D_CX, D_CY, D_CRS_CMAP1, D_MODES, D_GMAP,
            D_GLYPH, D_CMAP, D_ROWS, D_RUNS,
        ];
        let pipeline = ScanoutPipeline::new(
            gpu,
            &ScanoutShaders { label: "o2", body: &[SCANOUT], library: &[], defines: &defines, layouts: &[&layout] },
        )?;
        let tiles = gpu.buffer("o2 scanout tiles", (POOL_TILES * TILE_BYTES) as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("o2 scanout tiles"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: tiles.as_entire_binding() }],
        });
        let scanout = Scanout::new(gpu, &pipeline, MAX_PIXELS, (SLOT_WORDS * 4) as u64);
        Ok(O2Scanout {
            pipeline,
            scanout,
            tiles,
            group,
            slot: Vec::new(),
            raw: vec![0; TILE_BYTES / 8],
            stage: vec![0; TILE_WORDS],
            did: vec![0; TILE_WORDS],
            pages: Vec::new(),
            copied: Vec::new(),
            clipped: false,
        })
    }

    pub fn gpu(&self) -> Arc<Gpu> {
        Arc::clone(self.scanout.gpu())
    }

    /// The last frame composed, for this tick's consumer (`Scanout::show`).
    pub fn show(&mut self, size: (u32, u32), need: Need, frame: &mut Frame) -> Result<(), String> {
        self.scanout.show(size.0, size.1, need, frame)
    }

    /// The memory tile at guest physical `p`, as 32-bit words holding its
    /// big-endian values, into `words`. False when it cannot be read.
    fn read_tile(ram: &O2Memory, p: usize, raw: &mut [u64], words: &mut [u32]) -> bool {
        if ram.phys().read_block(p as u64, raw) != BUS_OK {
            return false;
        }
        for (i, w) in raw.iter().enumerate() {
            words[2 * i] = (w >> 32) as u32;
            words[2 * i + 1] = *w as u32;
        }
        true
    }

    /// Append the entries of a tile list at bus address `ctl & !0x1f` to
    /// `out`: where each tile is in memory, if it is.
    fn list(ram: &O2Memory, ctl: u32, n: usize, out: &mut Vec<Option<usize>>) {
        let base = ctl & 0xffff_ffe0;
        let bus = ram.phys();
        out.extend((0..n).map(|i| {
            let e = ram.dma_offset(base.wrapping_add(2 * i as u32)).map(|p| bus.read16(p as u64).data).unwrap_or(0) as u32;
            if e == 0 { None } else { ram.dma_offset(e << 16) }
        }));
    }

    /// Compose the picture GBE is scanning out; its size, None when the
    /// display is off. `upload` copies the tiles it reads from memory, which
    /// a mark or a rescan asks for; a tile list that moved is copied anyway.
    /// An error is a failed GPU, which has stopped the machine: nothing is
    /// shown in its place.
    pub fn compose(&mut self, gbe: &Gbe, ram: &O2Memory, upload: bool) -> Result<Option<(u32, u32)>, String> {
        let Some(r) = gbe.scan_regs() else { return Ok(None) };
        let (w, h) = (r.width as usize, r.height as usize);
        if w * h > MAX_PIXELS as usize {
            return Ok(None);
        }
        let g = Geometry::of(&r);
        self.pages.clear();
        if g.dma {
            Self::list(ram, r.frm_control, (g.per_row * g.fb_h.div_ceil(128)).min(FRAME_LIST), &mut self.pages);
        }
        let frame_n = self.pages.len();
        if g.ovr_dma {
            Self::list(ram, r.ovr_control, (g.ovr_per_row * h.div_ceil(128)).min(OVERLAY_LIST), &mut self.pages);
        }
        let upload = upload || self.pages != self.copied;
        self.fill_header(&r, &g);
        self.window_ids(ram, &r);
        self.slot[D_FT_OFF] = self.slot.len() as u32;
        let (mut next, mut whole) = (0, true);
        for i in 0..self.pages.len() {
            if i == frame_n {
                self.slot[D_OT_OFF] = self.slot.len() as u32;
            }
            let word = match self.pages[i] {
                None => NO_TILE,
                Some(_) if next == POOL_TILES => {
                    if !std::mem::replace(&mut self.clipped, true) {
                        log::warn!("o2 scan-out: the picture names more than {POOL_TILES} tiles; the rest show black");
                    }
                    NO_TILE
                }
                Some(p) => {
                    let slot = next as u32;
                    next += 1;
                    if upload && !self.copy_tile(ram, p, slot) {
                        whole = false;
                        NO_TILE
                    } else {
                        slot
                    }
                }
            };
            self.slot.push(word);
        }
        if frame_n == self.pages.len() {
            self.slot[D_OT_OFF] = self.slot.len() as u32;
        }
        self.slot[D_FT_N] = frame_n as u32;
        self.slot[D_OT_N] = (self.pages.len() - frame_n) as u32;
        if upload {
            self.copied.clear();
            if whole {
                self.copied.extend_from_slice(&self.pages);
            }
        }
        self.scanout.compose(&self.pipeline, w as u32, h as u32, bytemuck::cast_slice(&self.slot), &[&self.group])?;
        Ok(Some((w as u32, h as u32)))
    }

    /// Copy the memory tile at `p` into pool slot `slot`. False when it
    /// cannot be read.
    fn copy_tile(&mut self, ram: &O2Memory, p: usize, slot: u32) -> bool {
        if !Self::read_tile(ram, p, &mut self.raw, &mut self.stage) {
            return false;
        }
        self.scanout.gpu().write(&self.tiles, slot as u64 * TILE_BYTES as u64, bytemuck::cast_slice(&self.stage));
        true
    }

    /// The slot's header: the registers as `render` reads them.
    fn fill_header(&mut self, r: &ScanRegs, g: &Geometry) {
        let s = &mut self.slot;
        s.clear();
        s.resize(D_RUNS, 0);
        s[D_W] = r.width;
        s[D_BPP] = g.bpp as u32;
        s[D_PER_ROW] = g.per_row as u32;
        s[D_LINE_BYTES] = g.line_bytes as u32;
        s[D_FB_H] = g.fb_h as u32;
        s[D_FB_DMA] = g.dma as u32;
        s[D_OVR_DMA] = g.ovr_dma as u32;
        s[D_OVR_PER_ROW] = g.ovr_per_row as u32;
        s[D_OVR_LINE] = g.ovr_line as u32;
        let (cx, cy) = r.cursor_xy();
        s[D_CRS_ON] = (r.crs_ctl & 1) as u32;
        s[D_CX] = cx as u32;
        s[D_CY] = cy as u32;
        s[D_CRS_CMAP1..D_CRS_CMAP1 + 3].copy_from_slice(&r.crs_cmap1);
        s[D_MODES..D_MODES + 32].copy_from_slice(&r.modes);
        s[D_GMAP..D_GMAP + 256].copy_from_slice(&r.gmap);
        s[D_GLYPH..D_GLYPH + 64].copy_from_slice(&r.glyph);
        s[D_CMAP..D_CMAP + 6144].copy_from_slice(&r.cmap);
    }

    /// The window-ID runs of every row (`x | id << 16`, appended to the
    /// slot, and the row starts in its table), resolved as `render` fills its
    /// per-row ID array. Nothing when the DID tables are off or all the
    /// windows have one mode. Rows past `MAX_RUNS` runs have none.
    fn window_ids(&mut self, ram: &O2Memory, r: &ScanRegs) {
        let (w, h) = (r.width as usize, r.height as usize);
        let use_did = r.did_control & (1 << 16) != 0 && r.modes.iter().any(|&m| m != r.modes[0]);
        self.slot[D_USE_DID] = use_did as u32;
        if !use_did {
            return;
        }
        // The table is one 64 KB page.
        let have = ram.dma_offset((r.did_control & 0xffff) << 16).is_some_and(|p| Self::read_tile(ram, p, &mut self.raw, &mut self.did));
        let did = &self.did;
        let rd16 = |a: u32| -> u32 {
            if !have || a as usize >= TILE_BYTES {
                return 0;
            }
            let word = did[(a >> 2) as usize];
            if a & 2 == 0 { word >> 16 } else { word & 0xffff }
        };
        let rd32 = |a: u32| rd16(a) << 16 | rd16(a + 2);
        let s = &mut self.slot;
        let mut runs = 0usize;
        let mut frame_i = 0u32;
        for y in 0..h {
            s[D_ROWS + y] = runs as u32;
            // A row has at most 256 spans and the one after them.
            if runs + 257 > MAX_RUNS {
                continue;
            }
            // Frame entry: 25:19 block, 18:11 offset, 10:0 last line.
            let fe = rd32(4 * frame_i.min(16383));
            let (block, off, yend) = ((fe >> 19) & 0x7f, (fe >> 11) & 0xff, fe & 0x7ff);
            let lt = 512 * block + 2 * off;
            let (mut x, mut k, mut last) = (0usize, 0u32, 0u32);
            while x < w && k < 256 - off {
                let le = rd16(lt + 2 * k);
                let (d, xend) = (le >> 11, (le & 0x7ff) as usize);
                if x <= xend {
                    s.push(x as u32 | d << 16);
                    runs += 1;
                    last = d;
                    x = (xend + 1).min(w);
                }
                k += 1;
            }
            // Anything past the table's last span keeps that span's ID.
            if x < w {
                s.push(x as u32 | last << 16);
                runs += 1;
            }
            if y as u32 >= yend {
                frame_i += 1;
            }
        }
        s[D_ROWS + h] = runs as u32;
    }
}

#[cfg(test)]
mod tests;
