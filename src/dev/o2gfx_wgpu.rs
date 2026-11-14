//! The O2's GBE scan-out on the GPU (`crate::wgpu::scanout`): `Gbe::render`
//! (o2gfx.rs) as a kernel, so the picture is made where a wgpu window shows it.
//!
//! RAM stays the only authority: the frame buffer is not drawn into on the
//! GPU, it is copied from RAM. The GPU keeps a pool of the 64 KB tiles the
//! tile lists name. A tile is copied when it first enters a list, when the
//! rendering engine wrote it since the last copy ([`O2Memory::take_tile`]),
//! and on every tile of the picture when a full refresh is asked for, which
//! is how a store by the CPU, which marks nothing, reaches the screen. The
//! display slot is the registers, colour maps and window-ID runs as `render`
//! reads them, resolved on the CPU for the frame.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dev::crime_mem::O2Memory;
use crate::dev::o2gfx::{Gbe, ScanRegs};
use crate::traits::{BusDevice, BUS_OK};
use crate::wgpu::{Fragment, Gpu, Scanout, ScanoutPipeline, ScanoutShaders};

const SCANOUT: Fragment = Fragment::new("o2_scanout.wgsl", include_str!("o2gfx_wgpu/scanout.wgsl"));

/// A tile: 64 KB.
const TILE_BYTES: usize = 0x10000;
const TILE_WORDS: usize = TILE_BYTES / 4;
/// Tiles the pool holds: a 2048 x 2048 frame of 32-bit pixels is 256, and its
/// overlay 64.
const POOL_TILES: usize = 512;
/// The most pixels a picture may have (what `render` is given room for).
const MAX_PIXELS: u32 = 2048 * 2048;
/// Rows the run table covers: the height field is 12 bits.
const MAX_ROWS: usize = 4096;
/// Window-ID runs a frame may have; more falls back to the CPU compose.
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

fn defines() -> Vec<(&'static str, u32)> {
    vec![
        ("TILE_WORDS", TILE_WORDS as u32),
        ("NO_TILE", NO_TILE),
        ("D_W", D_W as u32),
        ("D_BPP", D_BPP as u32),
        ("D_PER_ROW", D_PER_ROW as u32),
        ("D_LINE_BYTES", D_LINE_BYTES as u32),
        ("D_FB_H", D_FB_H as u32),
        ("D_FB_DMA", D_FB_DMA as u32),
        ("D_FT_N", D_FT_N as u32),
        ("D_FT_OFF", D_FT_OFF as u32),
        ("D_OVR_DMA", D_OVR_DMA as u32),
        ("D_OVR_PER_ROW", D_OVR_PER_ROW as u32),
        ("D_OVR_LINE", D_OVR_LINE as u32),
        ("D_OT_N", D_OT_N as u32),
        ("D_OT_OFF", D_OT_OFF as u32),
        ("D_USE_DID", D_USE_DID as u32),
        ("D_CRS_ON", D_CRS_ON as u32),
        ("D_CX", D_CX as u32),
        ("D_CY", D_CY as u32),
        ("D_CRS_CMAP1", D_CRS_CMAP1 as u32),
        ("D_MODES", D_MODES as u32),
        ("D_GMAP", D_GMAP as u32),
        ("D_GLYPH", D_GLYPH as u32),
        ("D_CMAP", D_CMAP as u32),
        ("D_ROWS", D_ROWS as u32),
        ("D_RUNS", D_RUNS as u32),
    ]
}

/// Which memory tile each pool slot holds.
struct Pool {
    slot_of: HashMap<usize, u32>,
    owner: Vec<Option<usize>>,
    /// The frame a slot was last needed in.
    used: Vec<u64>,
    frame: u64,
}

impl Pool {
    fn new(slots: usize) -> Pool {
        Pool { slot_of: HashMap::new(), owner: vec![None; slots], used: vec![0; slots], frame: 0 }
    }

    fn clear(&mut self) {
        self.slot_of.clear();
        self.owner.fill(None);
    }

    /// The slot for memory tile `page` in this frame, and whether it must be
    /// copied in: a tile that is not resident takes a free slot or the one
    /// least recently needed, if none is needed by this frame.
    fn take(&mut self, page: usize) -> Option<(u32, bool)> {
        if let Some(&s) = self.slot_of.get(&page) {
            self.used[s as usize] = self.frame;
            return Some((s, false));
        }
        let s = (0..self.owner.len())
            .filter(|&s| self.owner[s].is_none() || self.used[s] < self.frame)
            .min_by_key(|&s| (self.owner[s].is_some(), self.used[s]))?;
        if let Some(old) = self.owner[s].replace(page) {
            self.slot_of.remove(&old);
        }
        self.slot_of.insert(page, s as u32);
        self.used[s] = self.frame;
        Some((s as u32, true))
    }

    /// Forget a tile whose copy failed.
    fn drop_tile(&mut self, page: usize) {
        if let Some(s) = self.slot_of.remove(&page) {
            self.owner[s as usize] = None;
        }
    }
}

/// The display thread's scan-out of GBE, over a pool of memory tiles.
pub struct O2Scanout {
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    tiles: wgpu::Buffer,
    group: wgpu::BindGroup,
    pool: Pool,
    /// The memory the pool's copies are of: changes when it was replaced.
    generation: (u64, u64),
    slot: Vec<u32>,
    stage: Vec<u32>,
    did: Vec<u32>,
    /// The size of the last frame composed.
    pub shown: Option<(u32, u32)>,
    /// Tiles copied to the GPU, and frames composed.
    pub tiles_uploaded: u64,
    pub frames: u64,
}

impl O2Scanout {
    /// A scan-out on `gpu`, noting the rendering engine's writes by tile.
    pub fn new(gpu: &Arc<Gpu>, ram: &O2Memory) -> Result<O2Scanout, String> {
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("o2 scanout tiles"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let defines = defines();
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
        ram.track_tiles();
        Ok(O2Scanout {
            pipeline,
            scanout,
            tiles,
            group,
            pool: Pool::new(POOL_TILES),
            generation: (ram.epoch(), ram.decode_gen()),
            slot: Vec::new(),
            stage: vec![0; TILE_WORDS],
            did: vec![0; TILE_WORDS],
            shown: None,
            tiles_uploaded: 0,
            frames: 0,
        })
    }

    pub fn gpu(&self) -> Arc<Gpu> {
        Arc::clone(self.scanout.gpu())
    }

    /// The last frame composed: 0xAABBGGRR words, rows of its width.
    pub fn output(&self) -> &wgpu::Buffer {
        self.scanout.output()
    }

    /// Read the last frame composed back into `out`. An error is the
    /// device's: it has been raised to the machine (see [`Gpu::fail`]).
    pub fn read(&mut self, out: &mut [u32]) -> Result<(), String> {
        let (w, h) = self.shown.ok_or("o2 scanout: nothing composed")?;
        self.scanout.read_output(w, h, out).inspect_err(|e| self.scanout.gpu().fail(e))
    }

    /// The memory tile at guest physical `p`, as 32-bit words holding its
    /// big-endian values, into `words`. False when it cannot be read.
    fn read_tile(ram: &O2Memory, p: usize, words: &mut [u32]) -> bool {
        let mut q = vec![0u64; TILE_BYTES / 8];
        if ram.phys().read_block(p as u64, &mut q) != BUS_OK {
            return false;
        }
        for (i, w) in q.iter().enumerate() {
            words[2 * i] = (w >> 32) as u32;
            words[2 * i + 1] = *w as u32;
        }
        true
    }

    /// The entries of a tile list at bus address `ctl & !0x1f`: where each
    /// tile is in memory, if it is.
    fn list(ram: &O2Memory, ctl: u32, n: usize) -> Vec<Option<usize>> {
        let base = ctl & 0xffff_ffe0;
        let bus = ram.phys();
        (0..n)
            .map(|i| {
                let e = ram.dma_offset(base.wrapping_add(2 * i as u32)).map(|p| bus.read16(p as u64).data).unwrap_or(0) as u32;
                if e == 0 { None } else { ram.dma_offset(e << 16) }
            })
            .collect()
    }

    /// Compose the picture GBE is scanning out into [`output`](Self::output).
    /// `full` copies every tile the picture reads, not just the ones the
    /// rendering engine wrote. A picture the kernel does not take is
    /// [`Composed::Unsupported`] and is left to the CPU compose. An error is a
    /// failed GPU: it has been raised to the machine, which stops (see
    /// [`Gpu::fail`]), and the caller does not substitute the CPU.
    pub fn compose(&mut self, gbe: &Gbe, ram: &O2Memory, full: bool) -> Result<Composed, String> {
        self.compose_frame(gbe, ram, full).inspect_err(|e| self.scanout.gpu().fail(e))
    }

    fn compose_frame(&mut self, gbe: &Gbe, ram: &O2Memory, full: bool) -> Result<Composed, String> {
        self.shown = None;
        let Some(r) = gbe.scan_regs() else { return Ok(Composed::Off) };
        let (w, h) = (r.width as usize, r.height as usize);
        if w * h > MAX_PIXELS as usize {
            return Ok(Composed::Off);
        }
        let generation = (ram.epoch(), ram.decode_gen());
        if generation != self.generation {
            self.generation = generation;
            self.pool.clear();
        }
        self.pool.frame += 1;

        // The normal planes' tiles (section 2.3 of the GBE specification), as
        // `render` finds them.
        let rhs = (r.frm_tile & 0x1f) as usize;
        let wt = ((r.frm_tile >> 5) & 0xff) as usize;
        let bpp = 1usize << ((r.frm_tile >> 13) & 3).min(2);
        let fb_h = (r.frm_pixel >> 16) as usize;
        let per_row = wt + (rhs != 0) as usize;
        let line_bytes = wt * 512 + rhs * 32;
        let dma = r.frm_control & 1 != 0 && per_row != 0 && line_bytes != 0;
        let frame_tiles = if dma { Self::list(ram, r.frm_control, (per_row * fb_h.div_ceil(128)).min(FRAME_LIST)) } else { Vec::new() };
        let ovr_rhs = (r.ovr_tile & 0x1f) as usize;
        let ovr_per_row = ((r.ovr_tile >> 5) & 0xff) as usize + (ovr_rhs != 0) as usize;
        let ovr_line = ((r.ovr_tile >> 5) & 0xff) as usize * 512 + ovr_rhs * 32;
        let ovr_dma = r.ovr_control & 1 != 0 && ovr_per_row != 0 && ovr_line != 0;
        let overlay_tiles = if ovr_dma { Self::list(ram, r.ovr_control, (ovr_per_row * h.div_ceil(128)).min(OVERLAY_LIST)) } else { Vec::new() };

        let mut tile_slots = Vec::with_capacity(frame_tiles.len() + overlay_tiles.len());
        for p in frame_tiles.iter().chain(overlay_tiles.iter()) {
            tile_slots.push(match *p {
                Some(p) => match self.resident(ram, p, full)? {
                    Some(slot) => slot,
                    None => return Ok(Composed::Unsupported),
                },
                None => NO_TILE,
            });
        }
        let (frame_slots, overlay_slots) = tile_slots.split_at(frame_tiles.len());

        let Some(runs) = self.window_ids(ram, &r) else { return Ok(Composed::Unsupported) };
        self.fill_slot(&r, bpp, per_row, line_bytes, fb_h, dma, ovr_per_row, ovr_line, ovr_dma, &runs, frame_slots, overlay_slots);
        let bytes = bytemuck::cast_slice(&self.slot);
        self.scanout.compose(&self.pipeline, w as u32, h as u32, bytes, &[&self.group])?;
        self.shown = Some((w as u32, h as u32));
        self.frames += 1;
        Ok(Composed::Frame(w as u32, h as u32))
    }

    /// The pool slot holding the memory tile at `p`, copied in when it is not
    /// there, when the engine wrote it, or when `full` asks for it.
    /// None when the pool has no room for it.
    fn resident(&mut self, ram: &O2Memory, p: usize, full: bool) -> Result<Option<u32>, String> {
        // Taken before the copy: a write during it is found next time.
        let written = ram.take_tile(p);
        let page = p >> 16;
        let Some((slot, fresh)) = self.pool.take(page) else { return Ok(None) };
        if fresh || written || full {
            if !Self::read_tile(ram, p, &mut self.stage) {
                self.pool.drop_tile(page);
                return Ok(Some(NO_TILE));
            }
            self.scanout.gpu().write(&self.tiles, slot as u64 * TILE_BYTES as u64, bytemuck::cast_slice(&self.stage));
            self.tiles_uploaded += 1;
        }
        Ok(Some(slot))
    }

    /// The window-ID runs of every row (`x | id << 16`, then the row
    /// starts), resolved as `render` fills its per-row ID array. Empty rows
    /// when the DID tables are off or all the windows have one mode.
    /// None when there are more runs than the slot holds.
    fn window_ids(&mut self, ram: &O2Memory, r: &ScanRegs) -> Option<Runs> {
        let h = r.height as usize;
        let w = r.width as usize;
        let use_did = r.did_control & (1 << 16) != 0 && r.modes.iter().any(|&m| m != r.modes[0]);
        let mut out = Runs { use_did, rows: Vec::new(), runs: Vec::new() };
        if !use_did {
            return Some(out);
        }
        // The table is one 64 KB page.
        let have = ram.dma_offset((r.did_control & 0xffff) << 16).is_some_and(|p| Self::read_tile(ram, p, &mut self.did));
        let did = &self.did;
        let rd16 = |a: u32| -> u32 {
            if !have || a as usize >= TILE_BYTES {
                return 0;
            }
            let word = did[(a >> 2) as usize];
            if a & 2 == 0 { word >> 16 } else { word & 0xffff }
        };
        let rd32 = |a: u32| rd16(a) << 16 | rd16(a + 2);
        let mut frame_i = 0u32;
        out.rows.reserve(h + 1);
        for y in 0..h {
            out.rows.push(out.runs.len() as u32);
            // Frame entry: 25:19 block, 18:11 offset, 10:0 last line.
            let fe = rd32(4 * frame_i.min(16383));
            let (block, off, yend) = ((fe >> 19) & 0x7f, (fe >> 11) & 0xff, fe & 0x7ff);
            let lt = 512 * block + 2 * off;
            let (mut x, mut k, mut last) = (0usize, 0u32, 0u32);
            while x < w && k < 256 - off {
                let le = rd16(lt + 2 * k);
                let (d, xend) = (le >> 11, (le & 0x7ff) as usize);
                if x <= xend {
                    out.runs.push(x as u32 | d << 16);
                    last = d;
                    x = (xend + 1).min(w);
                }
                k += 1;
            }
            // Anything past the table's last span keeps that span's ID.
            if x < w {
                out.runs.push(x as u32 | last << 16);
            }
            if y as u32 >= yend {
                frame_i += 1;
            }
            if out.runs.len() > MAX_RUNS {
                return None;
            }
        }
        out.rows.push(out.runs.len() as u32);
        Some(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_slot(
        &mut self,
        r: &ScanRegs,
        bpp: usize,
        per_row: usize,
        line_bytes: usize,
        fb_h: usize,
        dma: bool,
        ovr_per_row: usize,
        ovr_line: usize,
        ovr_dma: bool,
        runs: &Runs,
        frame_slots: &[u32],
        overlay_slots: &[u32],
    ) {
        let s = &mut self.slot;
        s.clear();
        s.resize(D_RUNS, 0);
        s[D_W] = r.width;
        s[D_BPP] = bpp as u32;
        s[D_PER_ROW] = per_row as u32;
        s[D_LINE_BYTES] = line_bytes as u32;
        s[D_FB_H] = fb_h as u32;
        s[D_FB_DMA] = dma as u32;
        s[D_OVR_DMA] = ovr_dma as u32;
        s[D_OVR_PER_ROW] = ovr_per_row as u32;
        s[D_OVR_LINE] = ovr_line as u32;
        s[D_USE_DID] = runs.use_did as u32;
        let (cx, cy) = r.cursor_xy();
        s[D_CRS_ON] = (r.crs_ctl & 1) as u32;
        s[D_CX] = cx as u32;
        s[D_CY] = cy as u32;
        s[D_CRS_CMAP1..D_CRS_CMAP1 + 3].copy_from_slice(&r.crs_cmap1);
        s[D_MODES..D_MODES + 32].copy_from_slice(&r.modes);
        s[D_GMAP..D_GMAP + 256].copy_from_slice(&r.gmap);
        s[D_GLYPH..D_GLYPH + 64].copy_from_slice(&r.glyph);
        s[D_CMAP..D_CMAP + 6144].copy_from_slice(&r.cmap);
        s[D_ROWS..D_ROWS + runs.rows.len()].copy_from_slice(&runs.rows);
        s.extend_from_slice(&runs.runs);
        s[D_FT_N] = frame_slots.len() as u32;
        s[D_FT_OFF] = s.len() as u32;
        s.extend_from_slice(frame_slots);
        s[D_OT_N] = overlay_slots.len() as u32;
        s[D_OT_OFF] = s.len() as u32;
        s.extend_from_slice(overlay_slots);
    }
}

/// What a compose made of the display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composed {
    /// The display is off: black, as `Gbe::render` finds it.
    Off,
    /// A picture past what the kernel holds (more tiles than the pool, more
    /// window-ID runs than the slot): the CPU compose makes it. Not a failure.
    Unsupported,
    Frame(u32, u32),
}

/// The window IDs of a frame as runs per row.
struct Runs {
    use_did: bool,
    rows: Vec<u32>,
    runs: Vec<u32>,
}

#[cfg(test)]
mod tests;
