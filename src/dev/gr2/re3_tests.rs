//! RE3 unit tests: drawing semantics on a bare `Re3` (no threads), plus the
//! CPU -> FIFO -> RE3 thread path through the bus.

use super::re3::*;
use super::*;

fn bare_re3() -> Box<Re3> {
    // SAFETY: Re3 is plain data and valid when zeroed.
    unsafe { Box::<Re3>::new_zeroed().assume_init() }
}

/// Draw state a textport-style client sets up: full scissor, all planes, COPY.
fn setup(r: &mut Re3) {
    for (reg, v) in [
        (RE3_LOGIC_OP, ROP_COPY),
        (RE3_COLOR_PLANE_MASK, 0x00ff_ffff),
        (RE3_CLIP_X_LO, (0)),
        (RE3_CLIP_X_HI, (FB_W as u32 - 1)),
        (RE3_CLIP_Y_LO, 0),
        (RE3_CLIP_Y_HI, FB_H as u32 - 1),
    ] {
        r.write_reg(reg, v);
    }
}

fn flat(r: &mut Re3, x: u32, y: u32, n: u32, ci: u32) {
    r.write_reg(RE3_RED_START, ci << 11);
    r.write_reg(RE3_CUR_X, x);
    r.write_reg(RE3_CUR_Y, y);
    r.write_reg(RE3_SPAN_LEN, n);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_FLAT_5);
}

fn px(r: &Re3, x: usize, y: usize) -> u32 {
    r.vram[y * FB_W + x]
}

#[test]
fn flat_span_draws_numpix_pixels() {
    let mut r = bare_re3();
    setup(&mut r);
    flat(&mut r, 100, 10, 7, 0x42);
    assert_eq!(px(&r, 99, 10), 0);
    for x in 100..107 {
        assert_eq!(px(&r, x, 10), 0x42, "x={x}");
    }
    assert_eq!(px(&r, 107, 10), 0);
}

#[test]
fn scissor_clips_span() {
    let mut r = bare_re3();
    setup(&mut r);
    r.write_reg(RE3_CLIP_X_LO, 10);
    r.write_reg(RE3_CLIP_X_HI, 12);
    flat(&mut r, 5, 0, 20, 1);
    let drawn: Vec<usize> = (0..30).filter(|&x| px(&r, x, 0) != 0).collect();
    assert_eq!(drawn, vec![10, 11, 12]);
}

#[test]
fn pattern_masks_span_msb_first() {
    let mut r = bare_re3();
    setup(&mut r);
    r.write_reg(RE3_PATTERN_ON, 1);
    r.write_reg(RE3_PATTERN_HI, 0b1010_0000_0000_0001);
    r.write_reg(RE3_PATTERN_LO, 0);
    flat(&mut r, 0, 0, 16, 9);
    let drawn: Vec<usize> = (0..16).filter(|&x| px(&r, x, 0) != 0).collect();
    assert_eq!(drawn, vec![0, 2, 15]);
}

#[test]
fn pixmask_limits_written_bits() {
    let mut r = bare_re3();
    setup(&mut r);
    r.vram[0] = 0x00ab_cdef;
    r.write_reg(RE3_COLOR_PLANE_MASK, 0x0000_00ff);
    flat(&mut r, 0, 0, 1, 0x12);
    assert_eq!(px(&r, 0, 0), 0x00ab_cd12);
}

#[test]
fn aux_planes_untouched_when_auxmask_zero() {
    let mut r = bare_re3();
    setup(&mut r);
    r.vram[0] = 0x5a00_0000;
    flat(&mut r, 0, 0, 1, 0x33);
    assert_eq!(px(&r, 0, 0), 0x5a00_0033);
}

#[test]
fn rop_xor() {
    let mut r = bare_re3();
    setup(&mut r);
    r.vram[0] = 0x0f;
    r.write_reg(RE3_LOGIC_OP, 6);
    flat(&mut r, 0, 0, 1, 0xff);
    assert_eq!(px(&r, 0, 0), 0xf0);
}

#[test]
fn wid_test_rejects_other_windows() {
    let mut r = bare_re3();
    setup(&mut r);
    r.vram[0] = 0x3000_0000; // cid 3
    r.vram[1] = 0x5000_0000; // cid 5
    r.write_reg(RE3_WID_TEST_ON, 1);
    r.write_reg(RE3_FB_OPTIONS, 1); // 4 WID planes
    r.write_reg(RE3_WINDOW_ID, 3);
    flat(&mut r, 0, 0, 2, 0x7);
    assert_eq!(px(&r, 0, 0), 0x3000_0007);
    assert_eq!(px(&r, 1, 0), 0x5000_0000);
}

#[test]
fn shaded_span_steps_colour() {
    let mut r = bare_re3();
    setup(&mut r);
    r.write_reg(RE3_CUR_X, 0);
    r.write_reg(RE3_CUR_Y, 0);
    r.write_reg(RE3_X_STEP, 1 << 14);
    r.write_reg(RE3_RED_START, 10 << 11);
    r.write_reg(RE3_RED_STEP, 1 << 11);
    r.write_reg(RE3_SPAN_LEN, 4);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_SMOOTH);
    let got: Vec<u32> = (0..4).map(|x| px(&r, x, 0)).collect();
    assert_eq!(got, vec![10, 11, 12, 13]);
}

/// One SMOOTH pixel at (1, 0), Bayer threshold 8, in RGB12 with XOR over
/// buffer nibbles R 8, G 3, B 0 (expanded 0x88, 0x33, 0x00).
fn xor_rgb12_pixel(dither: u32) -> u32 {
    let mut r = bare_re3();
    setup(&mut r);
    r.set_pixfmt(PIXFMT_RGB12);
    r.vram[1] = 0x038 | 0x038 << 12;
    for (reg, v) in [
        (RE3_LOGIC_OP, 6),
        (RE3_DITHER_ON, dither),
        (RE3_RED_START, 0xc3 << 11),
        (RE3_GREEN_START, 0x28 << 11),
        (RE3_BLUE_START, 0x7f << 11),
        (RE3_CUR_X, 1),
        (RE3_CUR_Y, 0),
        (RE3_X_STEP, 1 << 14),
        (RE3_SPAN_LEN, 1),
        (RE3_DRAW_TRIGGER, DRAW_SMOOTH),
    ] {
        r.write_reg(reg, v);
    }
    px(&r, 1, 0)
}

/// The logic op runs on the 24-bit colour, then the result is dithered:
/// 0xc3 ^ 0x88 = 0x4b -> 4, 0x28 ^ 0x33 = 0x1b -> 2 (dithered up), 0x7f -> 7.
/// Dithering first would give 4 ^ 8 = 3 and 2 ^ 3 = 1.
#[test]
fn rop_runs_before_dither_in_rgb12() {
    assert_eq!(xor_rgb12_pixel(1), 0x724 | 0x724 << 12);
    // No dither: the same op, truncated.
    assert_eq!(xor_rgb12_pixel(0), 0x714 | 0x714 << 12);
}

#[test]
fn copy_rect_handles_overlap_both_ways() {
    let mut r = bare_re3();
    setup(&mut r);
    for y in 0..8 {
        r.vram[y * FB_W] = y as u32 + 1;
    }
    // Move rows 0..6 up by 2 (overlapping), like a textport scroll.
    r.copy_rect(0, 0, 1, 6, 0, 2);
    let col: Vec<u32> = (0..8).map(|y| px(&r, 0, y)).collect();
    assert_eq!(col, vec![1, 2, 1, 2, 3, 4, 5, 6]);
    // And back down.
    r.copy_rect(0, 2, 1, 6, 0, 0);
    let col: Vec<u32> = (0..8).map(|y| px(&r, 0, y)).collect();
    assert_eq!(col, vec![1, 2, 3, 4, 5, 6, 5, 6]);
}

#[test]
fn writebuf_then_readbuf_stream() {
    let mut r = bare_re3();
    setup(&mut r);
    r.write_reg(RE3_CUR_X, 4);
    r.write_reg(RE3_CUR_Y, 3);
    r.write_reg(RE3_X_STEP, 1 << 14);
    r.write_reg(RE3_SPAN_LEN, 3);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_HOST_WRITE);
    for v in [0x11, 0x22, 0x33] {
        r.write_reg(RE3_HOST_DATA, v);
    }
    assert_eq!((px(&r, 4, 3), px(&r, 5, 3), px(&r, 6, 3)), (0x11, 0x22, 0x33));
    assert_eq!(r.ctx.stream, STREAM_IDLE);

    r.write_reg(RE3_CUR_X, 4);
    r.write_reg(RE3_SPAN_LEN, 3);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_HOST_READ);
    let mut got = vec![];
    for _ in 0..3 {
        got.push(r.ctx.reg[RE3_HOST_DATA]);
        r.read_buffer();
    }
    assert_eq!(got, vec![0x11, 0x22, 0x33]);
    assert_eq!(r.ctx.stream, STREAM_IDLE);
}

// ── Through the bus: CPU register window -> RE3 FIFO -> RE3 thread ─────────

/// A started board, stopped and freed when the test ends. Bind it with
/// `let g = &live_gr2(..);` so helpers taking `&Gr2` coerce.
pub(super) fn live_gr2(variant: Gr2Variant) -> crate::test_board::TestBoard<Gr2> {
    let g = Gr2::new(variant, crate::status::MachineStatus::detached());
    let g = crate::test_board::TestBoard::new(g);
    g.start();
    g
}

/// Store, retrying on bus back-pressure like the CPU does.
pub(super) fn w32(g: &Gr2, off: u32, val: u32) {
    while g.write32(GR2_BASE + off as u64, val) == BUS_BUSY {
        std::hint::spin_loop();
    }
}

pub(super) fn r32(g: &Gr2, off: u32) -> u32 {
    loop {
        let r = g.read32(GR2_BASE + off as u64);
        if r.status != BUS_BUSY {
            return r.data;
        }
    }
}

fn re3_reg(reg: usize) -> u32 {
    if reg < 0x20 { 0x6c200 + reg as u32 * 4 } else { 0x6c280 + (reg as u32 - 0x20) * 4 }
}

#[test]
fn cpu_register_window_draws_and_reads_back() {
    let g = &live_gr2(Gr2Variant::Xz);
    w32(g, re3_reg(RE3_LOGIC_OP), ROP_COPY);
    w32(g, re3_reg(RE3_COLOR_PLANE_MASK), 0xffffff);
    w32(g, re3_reg(RE3_CLIP_X_HI), 1279);
    w32(g, re3_reg(RE3_CLIP_Y_HI), 1023);
    w32(g, re3_reg(RE3_RED_START), 0x5 << 11);
    w32(g, re3_reg(RE3_XY_PACKED), (20 << 12) | (30));
    w32(g, re3_reg(RE3_SPAN_LEN), 2);
    w32(g, re3_reg(RE3_DRAW_TRIGGER), DRAW_FLAT_5);
    g.wait_idle();
    assert_eq!(g.vram()[20 * FB_W + 30], 5);
    assert_eq!(g.vram()[20 * FB_W + 31], 5);

    // HOST_READ streamed through CPU HOST_DATA reads.
    w32(g, re3_reg(RE3_HOST_PLANE), HPLANE_COLOR);
    w32(g, re3_reg(RE3_X_STEP), 1 << 14);
    w32(g, re3_reg(RE3_XY_PACKED), (20 << 12) | (29));
    w32(g, re3_reg(RE3_SPAN_LEN), 3);
    w32(g, re3_reg(RE3_DRAW_TRIGGER), DRAW_HOST_READ);
    let got: Vec<u32> = (0..3).map(|_| r32(g, re3_reg(RE3_HOST_DATA))).collect();
    assert_eq!(got, vec![0, 5, 5]);
}
