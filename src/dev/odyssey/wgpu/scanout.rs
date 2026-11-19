//! The VPro scan-out on the GPU (`crate::wgpu::scanout`): the display
//! slot the kernel reads, resolved on the CPU from the registers as
//! `Frame::snapshot` resolves them, and the unit the display thread runs.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use crate::wgpu::{Gpu, Scanout, ScanoutPipeline, ScanoutShaders};

use super::super::consts::{cursor, dispmode, lut, overlay};
use super::super::field::Codec;
use super::super::frame::Frame;
use super::super::reader::DisplayRegs;
use super::{codec_kind, Unsupported, VIEWS, VRAM, VRAM_MEM};

const SCANOUT: crate::wgpu::Fragment = crate::wgpu::Fragment::new("vpro_scanout.wgsl", include_str!("shaders/scanout.wgsl"));

/// What `shaders/scanout.wgsl` reads of a frame.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct DisplaySlot {
    head: [u32; 16],
    ids: [u32; 8 * dispmode::COUNT as usize],
    overlay: [u32; 2 * overlay::IDS as usize],
    gamma: [u32; 768],
    cursor: [u32; cursor::ENTRIES as usize],
    rgb: [u32; lut::SIZE as usize],
}

const _: () = assert!(size_of::<DisplaySlot>() == 4 * (16 + 256 + 16 + 768 + 256 + 8192));

impl DisplaySlot {
    /// The slot of `frame` (Frame::snapshot) under registers `regs`.
    pub fn of(frame: &Frame, regs: &DisplayRegs) -> Result<DisplaySlot, Unsupported> {
        let mut d = DisplaySlot::zeroed();
        let (fb, wid) = frame.surfaces();
        let fb_base = fb.base as u64 * super::super::vram::UNIT;
        let wid_base = wid.base as u64 * super::super::vram::UNIT;
        if fb_base >= 1 << 32 || wid_base >= 1 << 32 {
            return Err(Unsupported("a scan-out base past 4 GB".into()));
        }
        d.head[0] = fb_base as u32;
        d.head[1] = fb.pitch;
        d.head[2] = fb.bpp_log2 as u32;
        d.head[3] = fb.bytes_per_pixel();
        d.head[4] = wid_base as u32;
        d.head[5] = wid.pitch;
        d.head[6] = frame.width;
        d.head[7] = frame.height;
        d.head[8] = regs.cursor_x() as u32;
        d.head[9] = regs.cursor_y() as u32;
        d.head[10] = regs.cursor_shown() as u32 | if regs.cursor[cursor::CONTROL as usize] & cursor::CTL_CROSSHAIR != 0 { 2 } else { 0 };
        d.head[11] = frame.wid_layout.front.at as u32;
        d.head[12] = frame.wid_layout.back.at as u32;
        let ctx = &frame.ctx;
        for id in 0..dispmode::COUNT as usize {
            let show = &ctx.ids[id];
            let i = id * 8;
            d.ids[i] = show.buf as u32;
            d.ids[i + 1] = show.rgb as u32;
            d.ids[i + 2] = show.keep;
            d.ids[i + 3] = show.down;
            d.ids[i + 4] = show.base;
            d.ids[i + 5] = show.mask;
            for b in 0..2 {
                let f = &frame.fields[id][b];
                let kind = match f.codec {
                    Codec::Byte => super::KIND_BYTE,
                    c => codec_kind(c)?,
                };
                d.ids[i + 6 + b] = kind | (f.at as u32) << 24;
            }
        }
        for (o, (enabled, base)) in ctx.overlay.iter().enumerate() {
            d.overlay[o * 2] = *enabled as u32;
            d.overlay[o * 2 + 1] = *base;
        }
        for c in 0..3 {
            d.gamma[c * 256..(c + 1) * 256].copy_from_slice(&ctx.gamma[c]);
        }
        d.cursor = regs.cursor;
        d.rgb.copy_from_slice(&ctx.rgb[..]);
        Ok(d)
    }
}

/// The frame's pixels the scanout can hold.
const MAX_PIXELS: u32 = 2048 * 2048;

/// The display thread's scan-out on the GPU over the board's memory.
pub(crate) struct VproScanout {
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    vram_group: wgpu::BindGroup,
}

impl VproScanout {
    /// The device the scan-out (and the raster unit) runs on.
    pub fn gpu(&self) -> Arc<Gpu> {
        Arc::clone(self.scanout.gpu())
    }

    /// The display slot of `frame`; a base the kernel cannot address fails
    /// the device like any other scan-out error.
    fn slot(&self, frame: &Frame, regs: &DisplayRegs) -> Result<DisplaySlot, String> {
        DisplaySlot::of(frame, regs).map_err(|e| {
            self.scanout.gpu().fail(&format!("vpro scan-out: {}", e.0));
            e.0
        })
    }

    /// Over the raster unit's memory buffer.
    pub fn new(gpu: &Arc<Gpu>, vram: &wgpu::Buffer, words: u64) -> Result<VproScanout, String> {
        let layout = crate::wgpu::storage_layout(gpu, "vpro scanout vram", false);
        let pipeline = ScanoutPipeline::new(
            gpu,
            &ScanoutShaders {
                label: "vpro",
                body: &[SCANOUT],
                library: &[VRAM, VRAM_MEM, VIEWS],
                defines: &[("VRAM_WORD_MASK", (words - 1) as u32)],
                layouts: &[&layout],
            },
        )?;
        let vram_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vpro scanout vram"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: vram.as_entire_binding() }],
        });
        let scanout = Scanout::new(gpu, &pipeline, MAX_PIXELS, size_of::<DisplaySlot>() as u64);
        Ok(VproScanout { pipeline, scanout, vram_group })
    }

    /// Compose `frame` under `regs` into `out` (rows of `width`).
    pub fn run(&mut self, frame: &Frame, regs: &DisplayRegs, width: u32, height: u32, out: &mut [u32]) -> Result<(), String> {
        let slot = self.slot(frame, regs)?;
        self.scanout.run(&self.pipeline, width, height, bytemuck::bytes_of(&slot), &[&self.vram_group], out)
    }

    /// Compose `frame` under `regs` (rows of `width`) and submit it with no
    /// readback; the buffer it is in, for a renderer on this device.
    pub fn compose(&mut self, frame: &Frame, regs: &DisplayRegs, width: u32, height: u32) -> Result<wgpu::Buffer, String> {
        let slot = self.slot(frame, regs)?;
        self.scanout.compose(&self.pipeline, width, height, bytemuck::bytes_of(&slot), &[&self.vram_group])?;
        Ok(self.scanout.output().clone())
    }

    /// As `compose`, into `dst` as 0x00RRGGBB words of `stride_words` a row (`Scanout::compose_into`).
    pub fn compose_into(&mut self, frame: &Frame, regs: &DisplayRegs, width: u32, height: u32, dst: &wgpu::Buffer, stride_words: u32) -> Result<(), String> {
        let slot = self.slot(frame, regs)?;
        self.scanout.compose_into(&self.pipeline, width, height, bytemuck::bytes_of(&slot), &[&self.vram_group], dst, stride_words)
    }

    /// Whether `dst` can take a `width` by `height` frame of `stride_words` a row (`Scanout::check_into`).
    pub fn check_into(width: u32, height: u32, dst: &wgpu::Buffer, stride_words: u32) -> Result<(), String> {
        Scanout::check_into(width, height, dst, stride_words)
    }

    /// Frames composed.
    pub fn frames(&self) -> u64 {
        self.scanout.frames
    }
}
