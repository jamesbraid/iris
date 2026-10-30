//! Ported from include/hw/display/sgi/scanout.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics scanout: one head's presented frame, dirty lines, cursor,
//! retrace and the queue of swap records. Display line 0 is the top.
//!
//! Adaptations: there is no QemuConsole. `frame` is the scanout's own
//! presented frame (`Vec<u32>` of 0x00RRGGBB, `frame_width` x `frame_height`),
//! which the iris glue copies to the screen; `dirty` is one bool per display
//! line instead of a bitmap. The retrace QEMUTimer is `retrace_deadline`
//! (virtual ns, -1 = disarmed), fired by Scanout::poll(). `con`,
//! `Scanout::init(_head)`'s DeviceState/head arguments and VMState are
//! dropped (init_head is kept with its `head` number, unused). `ops` is a
//! `P<HeadOps>`.
use super::*;

pub const SGI_CURSOR_MAX: usize = 64;
pub const SGI_SWAP_MAX: usize = 16;
pub const SGI_SCANOUT_MAX_DIM: u32 = 16384;
/// 1 kHz: well above any SGI display's field rate.
pub const SGI_SCANOUT_MIN_PERIOD_NS: i64 = 1000000;

#[derive(Clone, Copy, Default, Debug)]
pub struct ScanoutMode {
    pub width: u32, // visible pixels and lines
    pub height: u32,
    pub frame_clocks: u64, // clocks per frame, blanking included
    pub clock_hz: u32,     // the clock they count; 0: no retrace
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SwapRecord {
    pub seq: u64,
    pub cookie: u64,
}

#[derive(Clone)]
pub struct Scanout {
    pub ops: P<HeadOps>,
    pub opaque: Opaque,
    pub composite_cursor: bool, // draw the cursor into the frame

    pub mode: ScanoutMode,
    pub frame: Vec<u32>,  // frame_width x frame_height of 0x00RRGGBB; empty: none
    pub dirty: Vec<bool>, // one flag per display line
    pub frame_width: u32,
    pub frame_height: u32, // what frame was allocated for

    pub retrace_deadline: i64, // virtual ns of the next retrace; -1 disarmed
    pub epoch: i64,            // virtual time of retrace 0
    pub frames: u64,           // retraces since the epoch

    pub swaps: [SwapRecord; SGI_SWAP_MAX],
    pub swap_head: u32,
    pub swap_count: u32,

    /// 0xAARRGGBB, row-major, cursor_w x cursor_h
    pub cursor_image: [u32; SGI_CURSOR_MAX * SGI_CURSOR_MAX],
    pub cursor_w: u32,
    pub cursor_h: u32,
    pub cursor_hot_x: i32,
    pub cursor_hot_y: i32,
    pub cursor_x: i32, // the hot spot, in display coordinates
    pub cursor_y: i32,
    pub cursor_on: bool,
    /// lines the composited cursor was last drawn over, first > last: none
    pub cursor_drawn_first: i32,
    pub cursor_drawn_last: i32,
}

impl Default for Scanout {
    fn default() -> Self {
        Scanout {
            ops: P::null(),
            opaque: P::null(),
            composite_cursor: false,
            mode: ScanoutMode::default(),
            frame: Vec::new(),
            dirty: Vec::new(),
            frame_width: 0,
            frame_height: 0,
            retrace_deadline: -1,
            epoch: 0,
            frames: 0,
            swaps: [SwapRecord::default(); SGI_SWAP_MAX],
            swap_head: 0,
            swap_count: 0,
            cursor_image: [0; SGI_CURSOR_MAX * SGI_CURSOR_MAX],
            cursor_w: 0,
            cursor_h: 0,
            cursor_hot_x: 0,
            cursor_hot_y: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_on: false,
            cursor_drawn_first: 0,
            cursor_drawn_last: 0,
        }
    }
}

impl Scanout {
    /// Bind a scanout to its head. ops.resolve_row is required; ops.publish is
    /// required once a swap is queued. No mode yet.
    pub fn init(&mut self, ops: &HeadOps, opaque: Opaque) {
        self.init_head(0, ops, opaque);
    }
    /// The same, for a family with several outputs (`head` is its number).
    pub fn init_head(&mut self, head: u32, ops: &HeadOps, opaque: Opaque) {
        let _ = head;
        assert!(ops.resolve_row.is_some());
        self.ops = P::from_ref(ops);
        self.opaque = opaque;
        self.cursor_drawn_first = 0;
        self.cursor_drawn_last = -1;
        self.retrace_deadline = -1;
    }
    pub fn cleanup(&mut self) {
        self.retrace_deadline = -1;
        self.frame = Vec::new();
        self.dirty = Vec::new();
        self.frame_width = 0;
        self.frame_height = 0;
    }

    /// A new mode: new frame, everything dirty, retrace phase restarted now.
    pub fn set_mode(&mut self, mode: &ScanoutMode) {
        self.mode = *mode;
        self.mode.sanitize();
        crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_scanout_mode: {}x{} frame {} clocks at {} Hz", self.mode.width, self.mode.height, self.mode.frame_clocks, self.mode.clock_hz);
        self.alloc();
        self.cursor_drawn_first = 0;
        self.cursor_drawn_last = -1;
        self.invalidate_all();
        self.restart();
    }
    /// Forget swaps and the cursor, repaint, restart the retrace phase.
    pub fn reset(&mut self) {
        self.swap_head = 0;
        self.swap_count = 0;
        self.cursor_on = false;
        self.invalidate_all();
        self.restart();
    }

    /// Virtual time of retrace n of the current mode, or -1 with no retrace.
    pub fn retrace_time(&self, n: u64) -> i64 {
        let hz = self.mode.clock_hz as u64;
        let fc = self.mode.frame_clocks;

        if hz == 0 || fc == 0 {
            return -1;
        }
        // n * fc / hz seconds, exactly, without overflowing for any n a run
        // reaches: whole seconds' worth of frames first, then the rest, whose
        // product fits because both factors are below 2^32.
        (self.epoch as u64)
            .wrapping_add((n / hz).wrapping_mul(fc).wrapping_mul(NANOSECONDS_PER_SECOND as u64))
            .wrapping_add(muldiv64((n % hz).wrapping_mul(fc), NANOSECONDS_PER_SECOND as u64, hz)) as i64
    }

    /// Put the next retrace at virtual time `when`, with the ones after it a
    /// period apart. With no retrace in the current mode this does nothing.
    pub fn set_next_retrace(&mut self, when: i64) {
        let next = self.retrace_time(self.frames + 1);

        if next < 0 {
            return;
        }
        // A negative retrace time reads as no retrace, which would stop the
        // retrace for good; an idle timer's -1 is a request for one now.
        let mut when = when;
        if when < 0 {
            when = qemu_clock_virtual_ns();
        }
        // Move the epoch rather than restart the count, so retrace n stays
        // epoch plus n periods and the next one, frames + 1, lands on when.
        self.epoch = when - (next - self.epoch);
        self.arm();
    }

    /// Fire the retrace(s) due at `now_ns` (the timer callback's body); the
    /// iris glue calls this from its tick.
    pub fn poll(&mut self, now_ns: i64) {
        // The timer callback: fire each retrace that has come due (the handler
        // re-arms the deadline for the next one).
        while self.retrace_deadline >= 0 && now_ns >= self.retrace_deadline {
            self.retrace();
        }
    }

    pub fn invalidate(&mut self, first: i32, count: i32) {
        let h: i32 = if !self.frame.is_empty() { self.mode.height as i32 } else { 0 };
        let last = first.wrapping_add(count).wrapping_sub(1);

        let first = first.max(0);
        let last = last.min(h - 1);
        if first > last {
            return;
        }
        for d in &mut self.dirty[first as usize..=last as usize] {
            *d = true;
        }
    }
    pub fn invalidate_all(&mut self) {
        self.invalidate(0, self.mode.height as i32);
    }
    /// Plane rows y0 .. y1 changed. The plane is the displayed one and exactly
    /// as tall as the mode: a bottom-origin row maps to display line
    /// p.height - 1 - y.
    pub fn invalidate_plane(&mut self, p: &Plane, y0: i32, y1: i32) {
        if p.bottom_origin {
            let first = p.height as i32 - 1 - y0.max(y1);
            let last = p.height as i32 - 1 - y0.min(y1);

            self.invalidate(first, last - first + 1);
        } else {
            self.invalidate(y0.min(y1), (y1 - y0).abs() + 1);
        }
    }

    /// Resolve the dirty lines, draw the cursor if composited, present.
    pub fn update(&mut self) {
        let ops = *self.ops.get();
        if let Some(pre_update) = ops.pre_update {
            pre_update(self.opaque);
        }
        if self.frame.is_empty() {
            return;
        }
        let h = self.mode.height as usize;
        let w = self.mode.width as usize;
        let resolve_row = ops.resolve_row.expect("resolve_row hook");
        let mut row = vec![0u32; w];
        let mut y = 0usize;
        while y < h {
            if !self.dirty[y] {
                y += 1;
                continue;
            }
            let mut end = y;
            while end < h && self.dirty[end] {
                end += 1;
            }
            for line in y..end {
                resolve_row(self.opaque, line as u32, &mut row);
                self.frame[line * w..(line + 1) * w].copy_from_slice(&row);
            }
            for d in &mut self.dirty[y..end] {
                *d = false;
            }
            if self.composite_cursor {
                self.draw_cursor(y as i32, end as i32 - 1);
            }
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_scanout_update: lines {}-{}", y, end - 1);
            y = end;
        }
        if self.composite_cursor {
            let mut first = 0;
            let mut last = 0;

            // The whole span the cursor covers now, not just the lines this
            // pass repainted: they are what the next change must repaint.
            self.cursor_span(&mut first, &mut last);
            self.cursor_drawn_first = first;
            self.cursor_drawn_last = last;
        }
    }

    /// A new cursor image, w x h pixels of 0xAARRGGBB (alpha 0 transparent),
    /// with its hot spot at (hot_x, hot_y); argb holds w * h words.
    pub fn cursor_define(&mut self, w: u32, h: u32, hot_x: i32, hot_y: i32, argb: &[u32]) {
        let cw = w.min(SGI_CURSOR_MAX as u32);
        let ch = h.min(SGI_CURSOR_MAX as u32);

        if cw != w || ch != h {
            crate::qlog!("sgi-scanout: a {}x{} cursor is clipped to {}x{}", w, h, cw, ch);
        }
        self.cursor_w = cw;
        self.cursor_h = ch;
        self.cursor_hot_x = hot_x;
        self.cursor_hot_y = hot_y;
        self.cursor_image = [0; SGI_CURSOR_MAX * SGI_CURSOR_MAX];
        for y in 0..ch as usize {
            self.cursor_image[y * cw as usize..(y + 1) * cw as usize]
                .copy_from_slice(&argb[y * w as usize..y * w as usize + cw as usize]);
        }
        // The host cursor (non-composited) is the iris glue's to present.
        if self.composite_cursor {
            self.cursor_invalidate();
        }
    }
    /// The hot spot is at display (x, y); `on` is whether the cursor shows.
    pub fn cursor_move(&mut self, x: i32, y: i32, on: bool) {
        self.cursor_x = x;
        self.cursor_y = y;
        self.cursor_on = on;
        if self.composite_cursor {
            self.cursor_invalidate();
        }
    }

    /// Queue a swap to take effect at a coming retrace. With SGI_SWAP_MAX
    /// already waiting, publishes FAILED with SGI_FAIL_FULL at once.
    pub fn queue_swap(&mut self, seq: u64, cookie: u64) {
        if self.swap_count as usize == SGI_SWAP_MAX {
            self.ops.get().publish(self.opaque, seq, SGI_DONE_FAILED, cookie, SGI_FAIL_FULL as u64);
            return;
        }
        self.swaps[((self.swap_head + self.swap_count) as usize) % SGI_SWAP_MAX] = SwapRecord { seq, cookie };
        self.swap_count += 1;
    }
}

pub fn muldiv64(a: u64, b: u64, c: u64) -> u64 {
    ((a as u128 * b as u128) / c as u128) as u64
}

impl ScanoutMode {
    /// A mode may be derived from guest-programmed timing tables, so it is
    /// checked before anything is allocated or armed from it. A size past
    /// SGI_SCANOUT_MAX_DIM presents nothing, and a period shorter than
    /// SGI_SCANOUT_MIN_PERIOD_NS, which would re-arm the retrace faster than
    /// the host can run it, or a frame too long to count, has no retrace.
    /// Returns whether the mode was used as given.
    pub fn sanitize(&mut self) -> bool {
        let mut ok = true;

        if self.width > SGI_SCANOUT_MAX_DIM || self.height > SGI_SCANOUT_MAX_DIM {
            crate::qlog!(
                "sgi-scanout: {}x{} exceeds the largest mode, {}x{}",
                self.width,
                self.height,
                SGI_SCANOUT_MAX_DIM,
                SGI_SCANOUT_MAX_DIM
            );
            self.width = 0;
            self.height = 0;
            ok = false;
        }
        if self.clock_hz != 0
            && (self.frame_clocks > u32::MAX as u64
                || (muldiv64(self.frame_clocks, NANOSECONDS_PER_SECOND as u64, self.clock_hz as u64) as i64)
                    < SGI_SCANOUT_MIN_PERIOD_NS)
        {
            crate::qlog!(
                "sgi-scanout: a frame of {} clocks at {} Hz is outside the retrace periods modelled",
                self.frame_clocks,
                self.clock_hz
            );
            self.clock_hz = 0;
            ok = false;
        }
        ok
    }
}

impl Scanout {
    pub fn arm(&mut self) {
        let when = self.retrace_time(self.frames + 1);

        self.retrace_deadline = if when < 0 { -1 } else { when };
    }

    pub fn restart(&mut self) {
        self.epoch = qemu_clock_virtual_ns();
        self.frames = 0;
        self.arm();
    }

    /// The timer callback.
    pub fn retrace(&mut self) {
        let mut swapped = false;

        self.frames += 1;
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_scanout_retrace: frame {} swaps waiting {}", self.frames, self.swap_count);
        if self.swap_count != 0 {
            let rec = self.swaps[self.swap_head as usize];

            self.swap_head = (self.swap_head + 1) % SGI_SWAP_MAX as u32;
            self.swap_count -= 1;
            crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_scanout_swap: seq {} cookie {:#x} frame {}", rec.seq, rec.cookie, self.frames);
            self.ops.get().publish(self.opaque, rec.seq, SGI_DONE_SWAPPED, rec.cookie, 0);
            swapped = true;
        }
        if let Some(retrace) = self.ops.get().retrace {
            retrace(self.opaque);
        }
        // The family flipped what the lines resolve to: repaint them all.
        if swapped {
            self.invalidate_all();
        }
        self.arm();
    }

    /// The display lines the cursor image covers now; first > last if none.
    pub fn cursor_span(&self, first: &mut i32, last: &mut i32) {
        let top = self.cursor_y - self.cursor_hot_y;

        if !self.cursor_on || self.cursor_w == 0 || self.cursor_h == 0 {
            *first = 0;
            *last = -1;
            return;
        }
        *first = top;
        *last = top + self.cursor_h as i32 - 1;
    }

    /// A composited cursor changed: repaint where it was and where it is.
    pub fn cursor_invalidate(&mut self) {
        let mut first = 0;
        let mut last = 0;

        self.invalidate(self.cursor_drawn_first, self.cursor_drawn_last - self.cursor_drawn_first + 1);
        self.cursor_span(&mut first, &mut last);
        self.invalidate(first, last - first + 1);
    }
}

pub fn scanout_blend(src: u32, dst: u32) -> u32 {
    let a = src >> 24;
    let mut out: u32 = 0;

    if a == 0xff {
        return src & 0xffffff;
    }
    let mut sh = 0;
    while sh < 24 {
        let s8 = (src >> sh) & 0xff;
        let d8 = (dst >> sh) & 0xff;

        out |= ((s8 * a + d8 * (255 - a) + 127) / 255) << sh;
        sh += 8;
    }
    out
}

impl Scanout {
    pub fn draw_cursor(&mut self, first: i32, last: i32) {
        let left = self.cursor_x - self.cursor_hot_x;
        let top = self.cursor_y - self.cursor_hot_y;
        let w = self.mode.width as i32;

        if !self.cursor_on {
            return;
        }
        let first = first.max(top);
        let last = last.min(top + self.cursor_h as i32 - 1);
        let mut y = first;
        while y <= last {
            let src0 = (y - top) as usize * self.cursor_w as usize;
            let dst0 = y as usize * w as usize;

            for i in 0..self.cursor_w as i32 {
                let x = left + i;
                let src = self.cursor_image[src0 + i as usize];

                if x < 0 || x >= w || (src >> 24) == 0 {
                    continue;
                }
                let d = &mut self.frame[dst0 + x as usize];
                *d = scanout_blend(src, *d);
            }
            y += 1;
        }
    }

    /// A frame and dirty map for the mode, when its size changed.
    pub fn alloc(&mut self) {
        let w = self.mode.width;
        let h = self.mode.height;

        if !self.frame.is_empty() && w == self.frame_width && h == self.frame_height {
            return;
        }
        self.frame = Vec::new();
        self.dirty = Vec::new();
        self.frame_width = 0;
        self.frame_height = 0;
        if w != 0 && h != 0 {
            self.frame = vec![0u32; w as usize * h as usize];
            self.dirty = vec![false; h as usize];
            self.frame_width = w;
            self.frame_height = h;
        }
    }
}
