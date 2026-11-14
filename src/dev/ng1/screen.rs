//! Newport's display state as the refresh loop caches it, and the planes it
//! copies for the CPU compositors.

use crate::compositor::CompositorSource;
use crate::debug_overlay::OverlaySource;
use crate::dev::ng1::bt445::Bt445;
use crate::dev::ng1::cmap::Cmap;
use crate::dev::ng1::rex3::Rex3;
#[cfg(feature = "rex-dirty-upload")]
use crate::dev::ng1::rex3_dirty::{copy_u32_rects, DamagePlane, DamageRect, DamageSnapshot};
use crate::dev::ng1::vc2::{
    Vc2, VC2_CTRL_BLACKOUT, VC2_CTRL_CURSOR_EN, VC2_CTRL_CURSOR_SIZE, VC2_CTRL_DID_EN,
    VC2_CTRL_VIDEO_TIMING_EN, VC2_REG_CURRENT_CURSOR_X, VC2_REG_CURSOR_ENTRY_PTR,
    VC2_REG_DID_ENTRY_PTR, VC2_REG_DISPLAY_CONTROL, VC2_REG_SCANLINE_LEN,
    VC2_REG_WORKING_CURSOR_Y,
};
use crate::dev::ng1::xmap9::Xmap9;
use crate::disp::{decode_vc2_timings, FB_MAX_H, FB_STRIDE};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Newport's planes and display state as the board's refresh left them, for
/// the one presenter that composes them itself (the GL window's shader).
/// Valid only while that presenter runs inside the call that was handed it:
/// the board's next refresh rewrites them.
#[derive(Clone)]
pub struct Ng1Screen(pub Arc<Mutex<Rex3Screen>>);

/// Independent revisions of the cached Newport display state. The source
/// identifies the screen that issued them, so replacing a screen cannot make
/// an old compositor cache appear current.
///
/// `vc2_regs` leaves out the cursor's position, shape pointer and enable
/// bits, which have a revision of their own: a mouse move must not look like
/// a new window-id table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ng1DisplayGenerations {
    pub(crate) source: u64,
    pub(crate) vc2_ram: u64,
    pub(crate) vc2_regs: u64,
    pub(crate) cursor: u64,
    pub(crate) xmap: u64,
    pub(crate) cmap: u64,
    pub(crate) ramdac: u64,
    /// Bumped when the CPU's decoded window-id map changes; only the tests
    /// compare it, so production skips the comparison.
    #[cfg(test)]
    pub(crate) did: u64,
    pub(crate) geometry: u64,
}

impl Ng1DisplayGenerations {
    fn new() -> Self {
        static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);
        let source = NEXT_SOURCE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("Newport display source identifiers exhausted");
        Self {
            source,
            vc2_ram: 0,
            vc2_regs: 0,
            cursor: 0,
            xmap: 0,
            cmap: 0,
            ramdac: 0,
            #[cfg(test)]
            did: 0,
            geometry: 0,
        }
    }

    fn bump(generation: &mut u64) {
        *generation = generation.checked_add(1).expect("Newport display generation exhausted");
    }
}

/// A Newport display snapshot with no access to either CPU framebuffer.
/// The generations change when the corresponding cached state changes.
#[cfg(feature = "wgpu")]
pub(crate) struct Ng1DisplaySource<'a> {
    pub(crate) generations: Ng1DisplayGenerations,
    pub(crate) did: &'a [u8],
    pub(crate) resident_did: &'a Mutex<Option<crate::dev::ng1::wgpu::direct::ResidentDid>>,
    pub(crate) xmap_mode: &'a [u32; 32],
    pub(crate) xmap_config: u8,
    pub(crate) xmap_cursor_cmap: u8,
    pub(crate) xmap_popup_cmap: u8,
    pub(crate) cmap: &'a [u32; 8192],
    pub(crate) ramdac: &'a [u32; 256],
    pub(crate) vc2_ram: &'a [u16],
    pub(crate) vc2_regs: &'a [u16; 32],
    pub(crate) topscan: usize,
    pub(crate) cursor_x_adjust: i32,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

pub struct Rex3Screen {
    pub width:  usize,
    pub height: usize,

    // Framebuffer snapshots (copied from Rex3::fb_rgb / fb_aux each frame)
    pub fb_rgb: Vec<u32>,
    pub fb_aux: Vec<u32>,
    #[cfg(feature = "rex-dirty-upload")]
    fb_rgb_damage: Vec<DamageRect>,
    #[cfg(feature = "rex-dirty-upload")]
    fb_aux_damage: Vec<DamageRect>,
    #[cfg(feature = "rex-dirty-upload")]
    fb_rgb_tiles: usize,
    #[cfg(feature = "rex-dirty-upload")]
    fb_aux_tiles: usize,

    // Decoded DID buffer in display coordinates (2048×1024, stride 2048)
    pub did: Vec<u8>,

    /// The last frame the resident test path captured (stride 2048).
    #[cfg(all(test, feature = "wgpu"))]
    pub rgba: Vec<u32>,

    // VC2 state
    pub vc2_ram:  Vec<u16>,
    pub vc2_regs: [u16; 32],

    // CMAP palette (8192 × 0x00RRGGBB)
    pub cmap: [u32; 8192],

    // Bt445 RAMDAC gamma LUT (256 × 0x00RRGGBB)
    pub ramdac_palette: [u32; 256],

    // XMAP mode table + config
    pub xmap_mode:        [u32; 32],
    pub xmap_config:      u8,
    pub xmap_cursor_cmap: u8,
    pub xmap_popup_cmap:  u8,

    // Display geometry derived from VC2 video timings
    /// Framebuffer row that maps to display row 0 (before the compositor adds 1)
    pub topscan:         usize,
    /// Cursor X correction from VT timing decode
    pub cursor_x_adjust: i32,

    /// Bumped each time the resident test path captures a frame.
    #[cfg(all(test, feature = "wgpu"))]
    pub frame_seq: u64,
    display_generations: Ng1DisplayGenerations,
    display_geometry: (usize, usize, usize, i32),
    /// The VC2 revisions the timings above were decoded from.
    timings_from: Option<(u64, u64)>,
    /// The GPU's window-id map: display-owned history that outlives any one
    /// scan-out.
    #[cfg(feature = "wgpu")]
    resident_did: Mutex<Option<crate::dev::ng1::wgpu::direct::ResidentDid>>,
}

impl Rex3Screen {
    pub fn new() -> Self {
        Self {
            width:            0,
            height:           0,
            fb_rgb:           vec![0u32; FB_STRIDE * FB_MAX_H],
            fb_aux:           vec![0u32; FB_STRIDE * FB_MAX_H],
            #[cfg(feature = "rex-dirty-upload")]
            fb_rgb_damage: Vec::new(),
            #[cfg(feature = "rex-dirty-upload")]
            fb_aux_damage: Vec::new(),
            #[cfg(feature = "rex-dirty-upload")]
            fb_rgb_tiles: 0,
            #[cfg(feature = "rex-dirty-upload")]
            fb_aux_tiles: 0,
            did:              vec![0u8; FB_STRIDE * FB_MAX_H],
            #[cfg(all(test, feature = "wgpu"))]
            rgba:             vec![0u32; FB_STRIDE * FB_MAX_H],
            vc2_ram:          vec![0u16; 32768],
            vc2_regs:         [0u16; 32],
            cmap:             [0u32; 8192],
            ramdac_palette:   [0u32; 256],
            xmap_mode:        [0u32; 32],
            xmap_config:      0,
            xmap_cursor_cmap: 0,
            xmap_popup_cmap:  0,
            topscan:          0,
            cursor_x_adjust:  0,
            #[cfg(all(test, feature = "wgpu"))]
            frame_seq:        0,
            display_generations: Ng1DisplayGenerations::new(),
            display_geometry: (0, 0, 0, 0),
            timings_from: None,
            #[cfg(feature = "wgpu")]
            resident_did: Mutex::new(None),
        }
    }

    fn decode_did_into(
        vc2_regs: &[u16; 32],
        vc2_ram: &[u16],
        width: usize,
        height: usize,
        did_buf: &mut [u8],
    ) -> bool {
        fn fill(dst: &mut [u8], value: u8) -> bool {
            #[cfg(test)]
            let changed = dst.iter().any(|&old| old != value);
            #[cfg(not(test))]
            let changed = false;
            dst.fill(value);
            changed
        }

        let display_ctrl = vc2_regs[VC2_REG_DISPLAY_CONTROL as usize];
        if (display_ctrl & VC2_CTRL_DID_EN) == 0 {
            return fill(did_buf, 0);
        }

        let mut changed = false;
        let did_ptr  = vc2_regs[VC2_REG_DID_ENTRY_PTR as usize];
        let scan_len = (vc2_regs[VC2_REG_SCANLINE_LEN as usize] >> 5) as usize;
        let ram      = vc2_ram;

        let effective_len = if scan_len > 0 && scan_len <= 2048 { scan_len } else { width };

        let mut y         = 0;
        let mut table_idx = did_ptr as usize;

        while y < height {
            if table_idx >= ram.len() { break; }
            let line_ptr = ram[table_idx];
            if line_ptr == 0xFFFF { break; }

            let mut ptr = line_ptr as usize;

            if ptr >= ram.len() {
                let start_idx = y * 2048;
                let end_idx   = y * 2048 + effective_len;
                if end_idx <= did_buf.len() { changed |= fill(&mut did_buf[start_idx..end_idx], 0); }
                y += 1; table_idx += 1; continue;
            }

            let entry = ram[ptr]; ptr += 1;
            let mut current_did = (entry & 0x1F) as u8;
            let mut current_x   = 0usize;

            loop {
                if current_x >= effective_len { break; }
                if ptr >= ram.len() {
                    let s = y * 2048 + current_x;
                    let e = y * 2048 + effective_len;
                    if e <= did_buf.len() { changed |= fill(&mut did_buf[s..e], current_did); }
                    break;
                }
                let next_entry  = ram[ptr]; ptr += 1;
                let next_x_raw  = ((next_entry >> 5) & 0x7FF) as usize;
                let next_did    = (next_entry & 0x1F) as u8;
                let is_eol      = next_x_raw == 0x7FF;
                let next_x      = if is_eol { effective_len } else { next_x_raw };
                let run_end     = next_x.max(current_x).min(effective_len);
                if run_end > current_x {
                    let s = y * 2048 + current_x;
                    let e = y * 2048 + run_end;
                    if e <= did_buf.len() { changed |= fill(&mut did_buf[s..e], current_did); }
                }
                current_x   = run_end;
                current_did = next_did;
                if is_eol { break; }
            }
            y += 1; table_idx += 1;
        }
        changed
    }

    fn decode_did(&mut self) -> bool {
        Self::decode_did_into(
            &self.vc2_regs,
            &self.vc2_ram,
            self.width,
            self.height,
            &mut self.did,
        )
    }

    #[cfg(all(test, feature = "wgpu"))]
    pub(crate) fn decoded_did_oracle(&self) -> Vec<u8> {
        let mut did = self.did.clone();
        Self::decode_did_into(
            &self.vc2_regs,
            &self.vc2_ram,
            self.width,
            self.height,
            &mut did,
        );
        did
    }

    // Returns (width, height, cursor_x_adjust).
    fn decode_video_timings(&self) -> (usize, usize, i32) {
        decode_vc2_timings(&self.vc2_regs, &self.vc2_ram)
    }

    /// Copy framebuffer and display state into this cache.
    /// Returns `true` if the display resolution changed.
    ///
    /// With `rex-dirty-upload`, only rectangles named by `fb_damage` are
    /// copied into the cached planes. Otherwise `copy_fb` preserves the
    /// original full-copy/borrowed-frame behavior.
    pub(crate) fn refresh(
        &mut self,
        fb_rgb: &[u32],
        fb_aux: &[u32],
        #[cfg(feature = "rex-dirty-upload")] fb_damage: &DamageSnapshot,
        #[cfg(not(feature = "rex-dirty-upload"))] copy_fb: bool,
        vc2: &Mutex<Vc2>,
        xmap:     &Mutex<Xmap9>,
        cmap:     &Mutex<Cmap>,
        bt445:    &Mutex<Bt445>,
        diag:     &AtomicU64,
    ) -> bool {
        #[cfg(feature = "rex-dirty-upload")]
        {
            fb_damage.rectangles_into(DamagePlane::Rgb, &mut self.fb_rgb_damage);
            fb_damage.rectangles_into(DamagePlane::Aux, &mut self.fb_aux_damage);
            self.fb_rgb_tiles = fb_damage.tile_count(DamagePlane::Rgb);
            self.fb_aux_tiles = fb_damage.tile_count(DamagePlane::Aux);
            let rgb_bytes = self
                .fb_rgb_damage
                .iter()
                .map(|rect| rect.bytes())
                .sum::<usize>();
            let aux_bytes = self
                .fb_aux_damage
                .iter()
                .map(|rect| rect.bytes())
                .sum::<usize>();
            crate::pf_counter!(
                crate::perfetto::events::REX3_DIRTY_RGB_TILES,
                self.fb_rgb_tiles
            );
            crate::pf_counter!(
                crate::perfetto::events::REX3_DIRTY_AUX_TILES,
                self.fb_aux_tiles
            );
            crate::pf_counter!(crate::perfetto::events::REX3_COPY_RGB_BYTES, rgb_bytes);
            crate::pf_counter!(crate::perfetto::events::REX3_COPY_AUX_BYTES, aux_bytes);
            if rgb_bytes != 0 || aux_bytes != 0 {
                crate::pf_scope!(crate::perfetto::events::REX3_FB_COPY, rgb_bytes + aux_bytes);
                diag.fetch_or(Rex3::DIAG_LOOP_FB_COPY, Ordering::Relaxed);
                copy_u32_rects(&mut self.fb_rgb, fb_rgb, &self.fb_rgb_damage);
                copy_u32_rects(&mut self.fb_aux, fb_aux, &self.fb_aux_damage);
                diag.fetch_and(!Rex3::DIAG_LOOP_FB_COPY, Ordering::Relaxed);
            }
        }
        #[cfg(not(feature = "rex-dirty-upload"))]
        if copy_fb {
            diag.fetch_or(Rex3::DIAG_LOOP_FB_COPY, Ordering::Relaxed);
            self.fb_rgb[..fb_rgb.len()].copy_from_slice(fb_rgb);
            self.fb_aux[..fb_aux.len()].copy_from_slice(fb_aux);
            diag.fetch_and(!Rex3::DIAG_LOOP_FB_COPY, Ordering::Relaxed);
        }

        self.refresh_display_state(vc2, xmap, cmap, bt445, diag)
    }

    /// Snapshot Newport display controls and tables without accessing either
    /// CPU framebuffer. Returns `true` if the display resolution changed.
    fn refresh_display_state_inner(
        &mut self,
        vc2: &Mutex<Vc2>,
        xmap: &Mutex<Xmap9>,
        cmap: &Mutex<Cmap>,
        bt445: &Mutex<Bt445>,
        diag: &AtomicU64,
        decode_did: bool,
    ) -> bool {
        let mut resized = false;

        diag.fetch_or(
            Rex3::DIAG_LOCK_VC2 | Rex3::DIAG_LOOP_VC2_COPY,
            Ordering::Relaxed,
        );
        {
            let mut vc2 = vc2.lock();
            if vc2.dirty {
                if self.vc2_ram != vc2.ram {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.vc2_ram);
                }
                if cursor_regs(&self.vc2_regs) != cursor_regs(&vc2.regs) {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.cursor);
                }
                if other_regs_differ(&self.vc2_regs, &vc2.regs) {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.vc2_regs);
                }
                self.vc2_ram.copy_from_slice(&vc2.ram);
                self.vc2_regs.copy_from_slice(&vc2.regs);
                vc2.dirty = false;
            }
        }
        diag.fetch_and(!(Rex3::DIAG_LOCK_VC2 | Rex3::DIAG_LOOP_VC2_COPY), Ordering::Relaxed);

        diag.fetch_or(Rex3::DIAG_LOCK_CMAP0 | Rex3::DIAG_LOOP_CMAP_COPY, Ordering::Relaxed);
        {
            let mut cmap = cmap.lock();
            if cmap.dirty {
                if self.cmap != cmap.palette {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.cmap);
                }
                self.cmap.copy_from_slice(&cmap.palette);
                cmap.dirty = false;
            }
        }
        diag.fetch_and(!(Rex3::DIAG_LOCK_CMAP0 | Rex3::DIAG_LOOP_CMAP_COPY), Ordering::Relaxed);

        {
            let mut dac = bt445.lock();
            if dac.dirty {
                let palette = dac.palette_as_rgb();
                if self.ramdac_palette != palette {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.ramdac);
                }
                self.ramdac_palette = palette;
                dac.dirty = false;
            }
        }

        diag.fetch_or(Rex3::DIAG_LOCK_XMAP0 | Rex3::DIAG_LOOP_XMAP_COPY, Ordering::Relaxed);
        {
            let mut xmap = xmap.lock();
            if xmap.dirty {
                if self.xmap_mode != xmap.mode_table
                    || self.xmap_config != xmap.config
                    || self.xmap_cursor_cmap != xmap.cursor_cmap_msb
                    || self.xmap_popup_cmap != xmap.popup_cmap_msb
                {
                    Ng1DisplayGenerations::bump(&mut self.display_generations.xmap);
                }
                self.xmap_mode.copy_from_slice(&xmap.mode_table);
                self.xmap_config      = xmap.config;
                self.xmap_cursor_cmap = xmap.cursor_cmap_msb;
                self.xmap_popup_cmap  = xmap.popup_cmap_msb;
                xmap.dirty = false;
            }
        }
        diag.fetch_and(!(Rex3::DIAG_LOCK_XMAP0 | Rex3::DIAG_LOOP_XMAP_COPY), Ordering::Relaxed);

        // ── 2. Check display-enable bits ─────────────────────────────────────────
        let display_ctrl    = self.vc2_regs[VC2_REG_DISPLAY_CONTROL as usize];
        let video_timing_en = (display_ctrl & VC2_CTRL_VIDEO_TIMING_EN) != 0;
        let display_en      = (display_ctrl & VC2_CTRL_BLACKOUT)        != 0;

        if !video_timing_en || !display_en {
            return resized;
        }

        // ── 3. Decode video timings and DID ──────────────────────────────────────
        // The decode walks the table in VC2 RAM (0.4-0.75 ms), and only the
        // RAM and the registers other than the cursor's can change its answer.
        let timings_from = (self.display_generations.vc2_ram, self.display_generations.vc2_regs);
        if self.timings_from != Some(timings_from) {
            diag.fetch_or(Rex3::DIAG_LOOP_VID_TIMINGS, Ordering::Relaxed);
            let (w, h, cursor_x_adjust) = self.decode_video_timings();
            diag.fetch_and(!Rex3::DIAG_LOOP_VID_TIMINGS, Ordering::Relaxed);
            self.timings_from = Some(timings_from);

            self.cursor_x_adjust = cursor_x_adjust;
            if w > 0 && h > 0 && (w != self.width || h != self.height) {
                println!("Rex3: Resolution changed to {}x{} cursor_x_adjust={}", w, h, cursor_x_adjust);
                self.width  = w;
                self.height = h;
                resized = true;
            }
        }

        if decode_did {
            diag.fetch_or(Rex3::DIAG_LOOP_DECODE_DID, Ordering::Relaxed);
            let did_changed = self.decode_did();
            #[cfg(test)]
            if did_changed {
                Ng1DisplayGenerations::bump(&mut self.display_generations.did);
            }
            #[cfg(not(test))]
            let _ = did_changed;
            diag.fetch_and(!Rex3::DIAG_LOOP_DECODE_DID, Ordering::Relaxed);
        }

        resized
    }

    pub(crate) fn refresh_display_state(
        &mut self,
        vc2: &Mutex<Vc2>,
        xmap: &Mutex<Xmap9>,
        cmap: &Mutex<Cmap>,
        bt445: &Mutex<Bt445>,
        diag: &AtomicU64,
    ) -> bool {
        self.refresh_display_state_inner(vc2, xmap, cmap, bt445, diag, true)
    }

    /// As [`refresh_display_state`](Self::refresh_display_state), leaving the
    /// window-id map to the GPU's expansion of the VC2 table.
    #[cfg(feature = "wgpu")]
    pub(crate) fn refresh_gpu_display_state(
        &mut self,
        vc2: &Mutex<Vc2>,
        xmap: &Mutex<Xmap9>,
        cmap: &Mutex<Cmap>,
        bt445: &Mutex<Bt445>,
        diag: &AtomicU64,
    ) -> bool {
        self.refresh_display_state_inner(vc2, xmap, cmap, bt445, diag, false)
    }

    /// Borrow only display state. Geometry includes externally supplied
    /// topscan as well as the decoded dimensions and cursor adjustment.
    #[cfg(feature = "wgpu")]
    pub(crate) fn display_state(&mut self) -> Ng1DisplaySource<'_> {
        let geometry = (self.width, self.height, self.topscan, self.cursor_x_adjust);
        if self.display_geometry != geometry {
            Ng1DisplayGenerations::bump(&mut self.display_generations.geometry);
            self.display_geometry = geometry;
        }
        Ng1DisplaySource {
            generations: self.display_generations,
            did: &self.did,
            resident_did: &self.resident_did,
            xmap_mode: &self.xmap_mode,
            xmap_config: self.xmap_config,
            xmap_cursor_cmap: self.xmap_cursor_cmap,
            xmap_popup_cmap: self.xmap_popup_cmap,
            cmap: &self.cmap,
            ramdac: &self.ramdac_palette,
            vc2_ram: &self.vc2_ram,
            vc2_regs: &self.vc2_regs,
            topscan: self.topscan,
            cursor_x_adjust: self.cursor_x_adjust,
            width: self.width,
            height: self.height,
        }
    }

    /// The source `SwCompositor` and the GL compositor compose from.
    pub fn compositor_source(&self) -> CompositorSource<'_> {
        CompositorSource {
            fb_rgb: &self.fb_rgb,
            fb_aux: &self.fb_aux,
            #[cfg(feature = "rex-dirty-upload")]
            fb_rgb_damage: &self.fb_rgb_damage,
            #[cfg(not(feature = "rex-dirty-upload"))]
            fb_rgb_damage: &[],
            #[cfg(feature = "rex-dirty-upload")]
            fb_aux_damage: &self.fb_aux_damage,
            #[cfg(not(feature = "rex-dirty-upload"))]
            fb_aux_damage: &[],
            did:              &self.did,
            xmap_mode:        &self.xmap_mode,
            xmap_config:      self.xmap_config,
            xmap_cursor_cmap: self.xmap_cursor_cmap,
            xmap_popup_cmap:  self.xmap_popup_cmap,
            cmap:             &self.cmap,
            ramdac:           &self.ramdac_palette,
            vc2_ram:          &self.vc2_ram,
            vc2_regs:         &self.vc2_regs,
            topscan:          self.topscan,
            cursor_x_adjust:  self.cursor_x_adjust,
            width:            self.width,
            height:           self.height,
        }
    }

    /// Build an `OverlaySource` borrowing from this struct's caches.
    pub fn overlay_source(&self) -> OverlaySource<'_> {
        OverlaySource {
            width:            self.width,
            height:           self.height,
            topscan:          self.topscan,
            cursor_x_adjust:  self.cursor_x_adjust,
            vc2_regs:         &self.vc2_regs,
            xmap_mode:        &self.xmap_mode,
            xmap_cursor_cmap: self.xmap_cursor_cmap,
            xmap_popup_cmap:  self.xmap_popup_cmap,
            cmap:             &self.cmap,
            fb_rgb:           &self.fb_rgb,
            fb_aux:           &self.fb_aux,
            did:              &self.did,
        }
    }
}

/// The registers that place and shape the cursor: its position, its
/// pattern's address and the enable and size bits of the display control.
fn cursor_regs(regs: &[u16; 32]) -> [u16; 4] {
    [
        regs[VC2_REG_CURRENT_CURSOR_X as usize],
        regs[VC2_REG_WORKING_CURSOR_Y as usize],
        regs[VC2_REG_CURSOR_ENTRY_PTR as usize],
        regs[VC2_REG_DISPLAY_CONTROL as usize] & (VC2_CTRL_CURSOR_EN | VC2_CTRL_CURSOR_SIZE),
    ]
}

/// Whether any register but the cursor's, or those bits of the display
/// control, differs.
fn other_regs_differ(a: &[u16; 32], b: &[u16; 32]) -> bool {
    const CURSOR_BITS: u16 = VC2_CTRL_CURSOR_EN | VC2_CTRL_CURSOR_SIZE;
    a.iter().zip(b).enumerate().any(|(i, (x, y))| match i as u8 {
        VC2_REG_CURRENT_CURSOR_X | VC2_REG_WORKING_CURSOR_Y | VC2_REG_CURSOR_ENTRY_PTR => false,
        VC2_REG_DISPLAY_CONTROL => (x ^ y) & !CURSOR_BITS != 0,
        _ => x != y,
    })
}
