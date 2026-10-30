//! Ported from include/hw/display/sgi/scanout.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics scanout: one head's presented frame, dirty lines, cursor,
//! retrace and the queue of swap records. Display line 0 is the top.
//!
//! Adaptations: there is no QemuConsole. `frame` is the scanout's own
//! presented frame (`Vec<u32>` of 0x00RRGGBB, `frame_width` x `frame_height`),
//! which the iris glue copies to the screen; `dirty` is one bool per display
//! line instead of a bitmap. The retrace QEMUTimer is `retrace_deadline`
//! (virtual ns, -1 = disarmed), fired by sgi_scanout_poll(). `con`,
//! `sgi_scanout_init(_head)`'s DeviceState/head arguments and VMState are
//! dropped (init_head is kept with its `head` number, unused). `ops` is a
//! `P<SGIHeadOps>`.
use super::*;

pub const SGI_CURSOR_MAX: usize = 64;
pub const SGI_SWAP_MAX: usize = 16;
pub const SGI_SCANOUT_MAX_DIM: u32 = 16384;
/// 1 kHz: well above any SGI display's field rate.
pub const SGI_SCANOUT_MIN_PERIOD_NS: i64 = 1000000;

#[derive(Clone, Copy, Default, Debug)]
pub struct SGIScanoutMode {
    pub width: u32, // visible pixels and lines
    pub height: u32,
    pub frame_clocks: u64, // clocks per frame, blanking included
    pub clock_hz: u32,     // the clock they count; 0: no retrace
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SGISwapRecord {
    pub seq: u64,
    pub cookie: u64,
}

#[derive(Clone)]
pub struct SGIScanout {
    pub ops: P<SGIHeadOps>,
    pub opaque: Opaque,
    pub composite_cursor: bool, // draw the cursor into the frame

    pub mode: SGIScanoutMode,
    pub frame: Vec<u32>,  // frame_width x frame_height of 0x00RRGGBB; empty: none
    pub dirty: Vec<bool>, // one flag per display line
    pub frame_width: u32,
    pub frame_height: u32, // what frame was allocated for

    pub retrace_deadline: i64, // virtual ns of the next retrace; -1 disarmed
    pub epoch: i64,            // virtual time of retrace 0
    pub frames: u64,           // retraces since the epoch

    pub swaps: [SGISwapRecord; SGI_SWAP_MAX],
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

impl Default for SGIScanout {
    fn default() -> Self {
        SGIScanout {
            ops: P::null(),
            opaque: P::null(),
            composite_cursor: false,
            mode: SGIScanoutMode::default(),
            frame: Vec::new(),
            dirty: Vec::new(),
            frame_width: 0,
            frame_height: 0,
            retrace_deadline: -1,
            epoch: 0,
            frames: 0,
            swaps: [SGISwapRecord::default(); SGI_SWAP_MAX],
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

/// Bind a scanout to its head. ops.resolve_row is required; ops.publish is
/// required once a swap is queued. No mode yet.
pub fn sgi_scanout_init(s: &mut SGIScanout, ops: &SGIHeadOps, opaque: Opaque) {
    sgi_scanout_init_head(s, 0, ops, opaque);
}
/// The same, for a family with several outputs (`head` is its number).
pub fn sgi_scanout_init_head(s: &mut SGIScanout, head: u32, ops: &SGIHeadOps, opaque: Opaque) {
    let _ = head;
    assert!(ops.resolve_row.is_some());
    s.ops = P::from_ref(ops);
    s.opaque = opaque;
    s.cursor_drawn_first = 0;
    s.cursor_drawn_last = -1;
    s.retrace_deadline = -1;
}
pub fn sgi_scanout_cleanup(s: &mut SGIScanout) {
    s.retrace_deadline = -1;
    s.frame = Vec::new();
    s.dirty = Vec::new();
    s.frame_width = 0;
    s.frame_height = 0;
}

/// A new mode: new frame, everything dirty, retrace phase restarted now.
pub fn sgi_scanout_set_mode(s: &mut SGIScanout, mode: &SGIScanoutMode) {
    s.mode = *mode;
    sgi_scanout_sanitize(&mut s.mode);
    crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_scanout_mode: {}x{} frame {} clocks at {} Hz", s.mode.width, s.mode.height, s.mode.frame_clocks, s.mode.clock_hz);
    sgi_scanout_alloc(s);
    s.cursor_drawn_first = 0;
    s.cursor_drawn_last = -1;
    sgi_scanout_invalidate_all(s);
    sgi_scanout_restart(s);
}
/// Forget swaps and the cursor, repaint, restart the retrace phase.
pub fn sgi_scanout_reset(s: &mut SGIScanout) {
    s.swap_head = 0;
    s.swap_count = 0;
    s.cursor_on = false;
    sgi_scanout_invalidate_all(s);
    sgi_scanout_restart(s);
}

/// Virtual time of retrace n of the current mode, or -1 with no retrace.
pub fn sgi_scanout_retrace_time(s: &SGIScanout, n: u64) -> i64 {
    let hz = s.mode.clock_hz as u64;
    let fc = s.mode.frame_clocks;

    if hz == 0 || fc == 0 {
        return -1;
    }
    // n * fc / hz seconds, exactly, without overflowing for any n a run
    // reaches: whole seconds' worth of frames first, then the rest, whose
    // product fits because both factors are below 2^32.
    (s.epoch as u64)
        .wrapping_add((n / hz).wrapping_mul(fc).wrapping_mul(NANOSECONDS_PER_SECOND as u64))
        .wrapping_add(muldiv64((n % hz).wrapping_mul(fc), NANOSECONDS_PER_SECOND as u64, hz)) as i64
}

/// Put the next retrace at virtual time `when`, with the ones after it a
/// period apart. With no retrace in the current mode this does nothing.
pub fn sgi_scanout_set_next_retrace(s: &mut SGIScanout, when: i64) {
    let next = sgi_scanout_retrace_time(s, s.frames + 1);

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
    s.epoch = when - (next - s.epoch);
    sgi_scanout_arm(s);
}

/// Fire the retrace(s) due at `now_ns` (the timer callback's body); the
/// iris glue calls this from its tick.
pub fn sgi_scanout_poll(s: &mut SGIScanout, now_ns: i64) {
    // The timer callback: fire each retrace that has come due (the handler
    // re-arms the deadline for the next one).
    while s.retrace_deadline >= 0 && now_ns >= s.retrace_deadline {
        sgi_scanout_retrace(s);
    }
}

pub fn sgi_scanout_invalidate(s: &mut SGIScanout, first: i32, count: i32) {
    let h: i32 = if !s.frame.is_empty() { s.mode.height as i32 } else { 0 };
    let last = first.wrapping_add(count).wrapping_sub(1);

    let first = first.max(0);
    let last = last.min(h - 1);
    if first > last {
        return;
    }
    for d in &mut s.dirty[first as usize..=last as usize] {
        *d = true;
    }
}
pub fn sgi_scanout_invalidate_all(s: &mut SGIScanout) {
    sgi_scanout_invalidate(s, 0, s.mode.height as i32);
}
/// Plane rows y0 .. y1 changed. The plane is the displayed one and exactly
/// as tall as the mode: a bottom-origin row maps to display line
/// p.height - 1 - y.
pub fn sgi_scanout_invalidate_plane(s: &mut SGIScanout, p: &SGIPlane, y0: i32, y1: i32) {
    if p.bottom_origin {
        let first = p.height as i32 - 1 - y0.max(y1);
        let last = p.height as i32 - 1 - y0.min(y1);

        sgi_scanout_invalidate(s, first, last - first + 1);
    } else {
        sgi_scanout_invalidate(s, y0.min(y1), (y1 - y0).abs() + 1);
    }
}

/// Resolve the dirty lines, draw the cursor if composited, present.
pub fn sgi_scanout_update(s: &mut SGIScanout) {
    let ops = *s.ops.get();
    if let Some(pre_update) = ops.pre_update {
        pre_update(s.opaque);
    }
    if s.frame.is_empty() {
        return;
    }
    let h = s.mode.height as usize;
    let w = s.mode.width as usize;
    let resolve_row = ops.resolve_row.expect("resolve_row hook");
    let mut row = vec![0u32; w];
    let mut y = 0usize;
    while y < h {
        if !s.dirty[y] {
            y += 1;
            continue;
        }
        let mut end = y;
        while end < h && s.dirty[end] {
            end += 1;
        }
        for line in y..end {
            resolve_row(s.opaque, line as u32, &mut row);
            s.frame[line * w..(line + 1) * w].copy_from_slice(&row);
        }
        for d in &mut s.dirty[y..end] {
            *d = false;
        }
        if s.composite_cursor {
            sgi_scanout_draw_cursor(s, y as i32, end as i32 - 1);
        }
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_scanout_update: lines {}-{}", y, end - 1);
        y = end;
    }
    if s.composite_cursor {
        let mut first = 0;
        let mut last = 0;

        // The whole span the cursor covers now, not just the lines this
        // pass repainted: they are what the next change must repaint.
        sgi_scanout_cursor_span(s, &mut first, &mut last);
        s.cursor_drawn_first = first;
        s.cursor_drawn_last = last;
    }
}

/// A new cursor image, w x h pixels of 0xAARRGGBB (alpha 0 transparent),
/// with its hot spot at (hot_x, hot_y); argb holds w * h words.
pub fn sgi_scanout_cursor_define(
    s: &mut SGIScanout,
    w: u32,
    h: u32,
    hot_x: i32,
    hot_y: i32,
    argb: &[u32],
) {
    let cw = w.min(SGI_CURSOR_MAX as u32);
    let ch = h.min(SGI_CURSOR_MAX as u32);

    if cw != w || ch != h {
        crate::qlog!("sgi-scanout: a {}x{} cursor is clipped to {}x{}", w, h, cw, ch);
    }
    s.cursor_w = cw;
    s.cursor_h = ch;
    s.cursor_hot_x = hot_x;
    s.cursor_hot_y = hot_y;
    s.cursor_image = [0; SGI_CURSOR_MAX * SGI_CURSOR_MAX];
    for y in 0..ch as usize {
        s.cursor_image[y * cw as usize..(y + 1) * cw as usize]
            .copy_from_slice(&argb[y * w as usize..y * w as usize + cw as usize]);
    }
    // The host cursor (non-composited) is the iris glue's to present.
    if s.composite_cursor {
        sgi_scanout_cursor_invalidate(s);
    }
}
/// The hot spot is at display (x, y); `on` is whether the cursor shows.
pub fn sgi_scanout_cursor_move(s: &mut SGIScanout, x: i32, y: i32, on: bool) {
    s.cursor_x = x;
    s.cursor_y = y;
    s.cursor_on = on;
    if s.composite_cursor {
        sgi_scanout_cursor_invalidate(s);
    }
}

/// Queue a swap to take effect at a coming retrace. With SGI_SWAP_MAX
/// already waiting, publishes FAILED with SGI_FAIL_FULL at once.
pub fn sgi_scanout_queue_swap(s: &mut SGIScanout, seq: u64, cookie: u64) {
    if s.swap_count as usize == SGI_SWAP_MAX {
        sgi_head_publish(s.ops.get(), s.opaque, seq, SGI_DONE_FAILED, cookie, SGI_FAIL_FULL as u64);
        return;
    }
    s.swaps[((s.swap_head + s.swap_count) as usize) % SGI_SWAP_MAX] = SGISwapRecord { seq, cookie };
    s.swap_count += 1;
}

pub fn muldiv64(a: u64, b: u64, c: u64) -> u64 {
    ((a as u128 * b as u128) / c as u128) as u64
}

/// A mode may be derived from guest-programmed timing tables, so it is
/// checked before anything is allocated or armed from it. A size past
/// SGI_SCANOUT_MAX_DIM presents nothing, and a period shorter than
/// SGI_SCANOUT_MIN_PERIOD_NS, which would re-arm the retrace faster than
/// the host can run it, or a frame too long to count, has no retrace.
/// Returns whether the mode was used as given.
pub fn sgi_scanout_sanitize(mode: &mut SGIScanoutMode) -> bool {
    let mut ok = true;

    if mode.width > SGI_SCANOUT_MAX_DIM || mode.height > SGI_SCANOUT_MAX_DIM {
        crate::qlog!(
            "sgi-scanout: {}x{} exceeds the largest mode, {}x{}",
            mode.width,
            mode.height,
            SGI_SCANOUT_MAX_DIM,
            SGI_SCANOUT_MAX_DIM
        );
        mode.width = 0;
        mode.height = 0;
        ok = false;
    }
    if mode.clock_hz != 0
        && (mode.frame_clocks > u32::MAX as u64
            || (muldiv64(mode.frame_clocks, NANOSECONDS_PER_SECOND as u64, mode.clock_hz as u64) as i64)
                < SGI_SCANOUT_MIN_PERIOD_NS)
    {
        crate::qlog!(
            "sgi-scanout: a frame of {} clocks at {} Hz is outside the retrace periods modelled",
            mode.frame_clocks,
            mode.clock_hz
        );
        mode.clock_hz = 0;
        ok = false;
    }
    ok
}

pub fn sgi_scanout_arm(s: &mut SGIScanout) {
    let when = sgi_scanout_retrace_time(s, s.frames + 1);

    s.retrace_deadline = if when < 0 { -1 } else { when };
}

pub fn sgi_scanout_restart(s: &mut SGIScanout) {
    s.epoch = qemu_clock_virtual_ns();
    s.frames = 0;
    sgi_scanout_arm(s);
}

/// The timer callback.
pub fn sgi_scanout_retrace(s: &mut SGIScanout) {
    let mut swapped = false;

    s.frames += 1;
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_scanout_retrace: frame {} swaps waiting {}", s.frames, s.swap_count);
    if s.swap_count != 0 {
        let rec = s.swaps[s.swap_head as usize];

        s.swap_head = (s.swap_head + 1) % SGI_SWAP_MAX as u32;
        s.swap_count -= 1;
        crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_scanout_swap: seq {} cookie {:#x} frame {}", rec.seq, rec.cookie, s.frames);
        sgi_head_publish(s.ops.get(), s.opaque, rec.seq, SGI_DONE_SWAPPED, rec.cookie, 0);
        swapped = true;
    }
    if let Some(retrace) = s.ops.get().retrace {
        retrace(s.opaque);
    }
    // The family flipped what the lines resolve to: repaint them all.
    if swapped {
        sgi_scanout_invalidate_all(s);
    }
    sgi_scanout_arm(s);
}

/// The display lines the cursor image covers now; first > last if none.
pub fn sgi_scanout_cursor_span(s: &SGIScanout, first: &mut i32, last: &mut i32) {
    let top = s.cursor_y - s.cursor_hot_y;

    if !s.cursor_on || s.cursor_w == 0 || s.cursor_h == 0 {
        *first = 0;
        *last = -1;
        return;
    }
    *first = top;
    *last = top + s.cursor_h as i32 - 1;
}

/// A composited cursor changed: repaint where it was and where it is.
pub fn sgi_scanout_cursor_invalidate(s: &mut SGIScanout) {
    let mut first = 0;
    let mut last = 0;

    sgi_scanout_invalidate(s, s.cursor_drawn_first, s.cursor_drawn_last - s.cursor_drawn_first + 1);
    sgi_scanout_cursor_span(s, &mut first, &mut last);
    sgi_scanout_invalidate(s, first, last - first + 1);
}

pub fn sgi_scanout_blend(src: u32, dst: u32) -> u32 {
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

pub fn sgi_scanout_draw_cursor(s: &mut SGIScanout, first: i32, last: i32) {
    let left = s.cursor_x - s.cursor_hot_x;
    let top = s.cursor_y - s.cursor_hot_y;
    let w = s.mode.width as i32;

    if !s.cursor_on {
        return;
    }
    let first = first.max(top);
    let last = last.min(top + s.cursor_h as i32 - 1);
    let mut y = first;
    while y <= last {
        let src0 = (y - top) as usize * s.cursor_w as usize;
        let dst0 = y as usize * w as usize;

        for i in 0..s.cursor_w as i32 {
            let x = left + i;
            let src = s.cursor_image[src0 + i as usize];

            if x < 0 || x >= w || (src >> 24) == 0 {
                continue;
            }
            let d = &mut s.frame[dst0 + x as usize];
            *d = sgi_scanout_blend(src, *d);
        }
        y += 1;
    }
}

/// A frame and dirty map for the mode, when its size changed.
pub fn sgi_scanout_alloc(s: &mut SGIScanout) {
    let w = s.mode.width;
    let h = s.mode.height;

    if !s.frame.is_empty() && w == s.frame_width && h == s.frame_height {
        return;
    }
    s.frame = Vec::new();
    s.dirty = Vec::new();
    s.frame_width = 0;
    s.frame_height = 0;
    if w != 0 && h != 0 {
        s.frame = vec![0u32; w as usize * h as usize];
        s.dirty = vec![false; h as usize];
        s.frame_width = w;
        s.frame_height = h;
    }
}
