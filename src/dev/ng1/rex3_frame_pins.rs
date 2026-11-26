//! A composed-frame pin for Newport, with no guest.
//!
//! A started board takes a fixed register program (RGB24 blocks, a shaded
//! span, CI8 blocks), the display chain is set to a small fixed video mode,
//! and the board's own `Rex3Screen::refresh` + `SwCompositor` produce the
//! frame the `--ci` refresh thread would put in `Rex3Screen::rgba`. The frame
//! is hashed with blake3 over its size and `0xAABBGGRR` words.
//! Run twice and with each executor; the pin is a constant here.

use super::*;
use crate::compositor::SwCompositor;
use crate::dev::ng1::{bt445::Bt445, cmap::Cmap, vc2::*, xmap9::Xmap9};
use crate::disp::FB_STRIDE;
use crate::test_frame_pin::{colour_count, frame_hash};

const W: usize = 96;
const H: usize = 64;
const DID_LINE: usize = 0x500;
const CURSOR: usize = 0x900;

/// Fixed display chain: one video line table, two DID modes (left half RGB24,
/// right half CI8 through the CMAP), a 32x32 cursor.
fn configure_display(rex: &Rex3) {
    let mut vc2 = Vc2::new();
    vc2.regs[VC2_REG_VIDEO_ENTRY_PTR as usize] = 0x100;
    vc2.regs[VC2_REG_DID_ENTRY_PTR as usize] = 0x300;
    vc2.regs[VC2_REG_SCANLINE_LEN as usize] = (W << 5) as u16;
    vc2.regs[VC2_REG_CURSOR_ENTRY_PTR as usize] = CURSOR as u16;
    vc2.regs[VC2_REG_CURRENT_CURSOR_X as usize] = 27;
    vc2.regs[VC2_REG_WORKING_CURSOR_Y as usize] = 35;
    vc2.regs[VC2_REG_DISPLAY_CONTROL as usize] =
        VC2_CTRL_BLACKOUT | VC2_CTRL_VIDEO_TIMING_EN | VC2_CTRL_DID_EN | VC2_CTRL_CURSOR_EN;
    vc2.ram[0x100] = 0x200;
    vc2.ram[0x101] = H as u16;
    vc2.ram[0x200] = 0x8000 | ((W as u16 / 2) << 8);
    vc2.ram[0x201] = VT_CBLANK_XMAP_N as u16;
    vc2.ram[0x202] = 0x200;
    for y in 0..H {
        vc2.ram[0x300 + y] = DID_LINE as u16;
    }
    vc2.ram[DID_LINE] = 0;
    vc2.ram[DID_LINE + 1] = ((W as u16 / 2) << 5) | 1;
    vc2.ram[DID_LINE + 2] = 0xffe0;
    for y in 0..32 {
        vc2.ram[CURSOR + y * 2] = if y % 2 == 0 { 0xaaaa } else { 0x5555 };
        vc2.ram[CURSOR + 64 + y * 2] = 0x0f0f;
    }
    vc2.dirty = true;
    let mut xmap = Xmap9::new();
    xmap.mode_table[0] = (1 << 8) | (3 << 10) | (2 << 16);
    xmap.mode_table[1] = (1 << 10) | (2 << 16);
    xmap.cursor_cmap_msb = 3;
    xmap.popup_cmap_msb = 4;
    xmap.dirty = true;
    let mut cmap = Cmap::new(0);
    for (i, c) in cmap.palette.iter_mut().enumerate() {
        let i = i as u32;
        *c = ((i.wrapping_mul(3) & 255) << 16) | ((i.wrapping_mul(5) & 255) << 8) | (i.wrapping_mul(7) & 255);
    }
    cmap.dirty = true;
    let mut dac = Bt445::new();
    for (i, c) in dac.palette.iter_mut().enumerate() {
        *c = [i as u8; 3];
    }
    dac.dirty = true;
    *rex.vc2.lock() = vc2;
    *rex.xmap0.lock() = xmap;
    *rex.cmap0.lock() = cmap;
    *rex.bt445.lock() = dac;
}

fn scene(rex: &Rex3) {
    rex3init(rex);
    // RGB24 blocks, one per colour
    reg(rex, REX3_DRAWMODE1, DM1_RGB24_SRC);
    reg(rex, REX3_WRMASK, 0xFFFFFF);
    for (i, (r, g, b)) in [(200u32, 30u32, 30u32), (30, 200, 30), (30, 30, 200), (230, 230, 60)].iter().enumerate() {
        let x = (i as i32) * 12;
        reg(rex, REX3_COLORRED, r << 11);
        reg(rex, REX3_COLORGRN, g << 11);
        reg(rex, REX3_COLORBLUE, b << 11);
        reg(rex, REX3_XYENDI, xy(x + 11, 19));
        reg(rex, REX3_XYSTARTI, xy(x, 0));
        reg_go(rex, REX3_DRAWMODE0, DM0_DRAW_BLOCK);
    }
    // Shaded spans, 16 rows
    reg(rex, REX3_COLORGRN, 0);
    reg(rex, REX3_COLORBLUE, 90 << 11);
    reg(rex, REX3_SLOPERED, 5 << 11);
    reg(rex, REX3_SLOPEGRN, 0);
    reg(rex, REX3_SLOPEBLUE, 0);
    for row in 0..16 {
        reg(rex, REX3_COLORRED, (row as u32 * 9) << 11);
        reg(rex, REX3_XYENDI, xy(47, 24 + row));
        reg(rex, REX3_XYSTARTI, xy(0, 24 + row));
        reg_go(rex, REX3_DRAWMODE0, DM0_DRAW_SPAN | (1 << 18));
    }
    // CI8 blocks in the right half
    reg(rex, REX3_SLOPERED, 0);
    reg(rex, REX3_DRAWMODE1, DM1_CI8_SRC);
    reg(rex, REX3_WRMASK, 0xFF);
    for i in 0..4 {
        reg(rex, REX3_COLORI, 40 + i as u32 * 50);
        reg(rex, REX3_XYENDI, xy(48 + i * 12 + 11, 40));
        reg(rex, REX3_XYSTARTI, xy(48 + i * 12, 5));
        reg_go(rex, REX3_DRAWMODE0, DM0_DRAW_BLOCK);
    }
    rex.wait_idle();
}

/// The frame the CI refresh path would publish, as `0xAABBGGRR` words.
fn composed(rex: &Rex3) -> (usize, usize, Vec<u32>) {
    rex.wait_idle();
    let mut screen = rex.head.screen().lock();
    let diag = AtomicU64::new(0);
    let rgb = unsafe { &*rex.fb_rgb.get() };
    let aux = unsafe { &*rex.fb_aux.get() };
    // The first refresh copies everything; under dirty upload that is a full-damage snapshot.
    #[cfg(feature = "rex-dirty-upload")]
    let copy_fb = {
        let mut all = crate::dev::ng1::rex3_dirty::DamageSnapshot::default();
        all.mark_full(crate::dev::ng1::rex3_dirty::DamagePlane::Rgb);
        all.mark_full(crate::dev::ng1::rex3_dirty::DamagePlane::Aux);
        all
    };
    #[cfg(feature = "rex-dirty-upload")]
    let copy_fb = &copy_fb;
    #[cfg(not(feature = "rex-dirty-upload"))]
    let copy_fb = true;
    screen.refresh(&rgb[..], &aux[..], copy_fb, &rex.vc2, &rex.xmap0, &rex.cmap0, &rex.bt445, &diag);
    assert_eq!((screen.width, screen.height), (W, H));
    // What the refresh thread latches at each retrace (rex3.rs, `screen.topscan = topscan`).
    screen.topscan = unsafe { (*rex.context.get()).topscan as usize };
    let mut sw = SwCompositor::new();
    sw.compose_pixels(&screen.compositor_source());
    let mut out = Vec::with_capacity(W * H);
    for y in 0..H {
        out.extend_from_slice(&sw.pixels()[y * FB_STRIDE..y * FB_STRIDE + W]);
    }
    (W, H, out)
}

fn with_board<T>(jit: bool, read: impl FnOnce(&Rex3) -> T) -> T {
    // No refresh thread: it latches TOPSCAN and the VC2 state on a host timer,
    // which moved the whole frame by a row from run to run.
    // A stopping REX3 saves the draw shapes it saw, to $HOME/.iris unless told
    // otherwise; keep the pin away from the developer's profile.
    std::env::set_var("IRIS_REX_JIT_PROFILE", std::env::temp_dir().join("iris-frame-pin-rex-jit-profile.bin"));
    let rex = &make_rex3_unstarted();
    start_rex3_processor_only(rex);
    #[cfg(feature = "rex-jit")]
    rex.jit_enabled.store(jit, Ordering::Relaxed);
    #[cfg(not(feature = "rex-jit"))]
    let _ = jit;
    configure_display(rex);
    scene(rex);
    read(rex)
}

fn run_px(jit: bool) -> (usize, usize, Vec<u32>) {
    let (w, h, px) = with_board(jit, composed);
    let colours = colour_count(w, h, w, &px);
    assert!(colours > 12, "the frame is not a blank: {colours} colours");
    (w, h, px)
}

fn run_once(jit: bool) -> String {
    let (w, h, px) = run_px(jit);
    frame_hash(w, h, w, &px)
}

const PIN: &str = "176f245ba321eddfd06b185d5a6081bd45a04e9871ed8b3748cdb8f441eb7aa0";

#[test]
fn newport_composed_frame_is_pinned() {
    let a = run_once(false);
    let b = run_once(false);
    assert_eq!(a, b, "two interpreter runs");
    #[cfg(feature = "rex-jit")]
    assert_eq!(a, run_once(true), "the shader JIT draws the same frame");
    eprintln!("newport frame hash {a}");
    assert_eq!(a, PIN);
}
