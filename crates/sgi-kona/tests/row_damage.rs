//! A compose of only the rows the damage names shows what a compose of the whole frame does, and the textport,
//! bitmap, X server and DMA jobs name only the rows they drew.

use sgi_kona::dev::kona::back::display::Pixels;
use sgi_kona::dev::kona::back::x::{fill, pixel as x_pixel};
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::ident::KonaConfig;
use sgi_kona::dev::kona::job::{Grammar, Job};
use sgi_kona::dev::kona::rig::Rig;

// ---- a compose of the marked rows against a whole compose ----

/// The rows a viewer holds, kept up to date from the damage alone.
struct Viewer {
    w: usize,
    h: usize,
    shown: Vec<u32>,
    last: Vec<u32>,
}

/// What one look at the damage found: the hull it named, and the hull of the rows that really changed.
#[derive(Debug, PartialEq, Eq)]
struct Look {
    named: Option<(usize, usize)>,
    changed: Option<(usize, usize)>,
}

fn hull_of_changes(was: &[u32], now: &[u32], w: usize) -> Option<(usize, usize)> {
    let rows: Vec<usize> = (0..now.len() / w).filter(|&y| was[y * w..(y + 1) * w] != now[y * w..(y + 1) * w]).collect();
    Some((*rows.first()?, rows.last()? + 1))
}

impl Viewer {
    /// A viewer that has seen the whole picture as it is now, with the damage taken.
    fn new(p: &Pixels) -> Viewer {
        let (w, h) = p.size();
        let (w, h) = (w as usize, h as usize);
        p.take_damage();
        let mut v = Viewer { w, h, shown: vec![0; w * h], last: Vec::new() };
        p.window_rows((0, 0, w as u32, h as u32), 0..h, false, &mut v.shown, &mut Vec::new());
        v.last = v.shown.clone();
        v
    }

    /// Take the damage, compose its rows into what is shown and compare that with a whole compose.
    fn look(&mut self, p: &Pixels, what: &str) -> Look {
        let (w, h) = (self.w, self.h);
        let named = p.take_damage();
        if let Some((a, b)) = named {
            p.window_rows((0, 0, w as u32, h as u32), a..b, false, &mut self.shown, &mut Vec::new());
        }
        let mut whole = vec![0; w * h];
        p.window_rows((0, 0, w as u32, h as u32), 0..h, false, &mut whole, &mut Vec::new());
        if let Some(y) = (0..h).find(|&y| self.shown[y * w..(y + 1) * w] != whole[y * w..(y + 1) * w]) {
            panic!("{what}: row {y} is stale after composing {named:?}");
        }
        let changed = hull_of_changes(&self.last, &whole, w);
        self.last = whole;
        Look { named, changed }
    }
}

// ---- constructed cases: each job names the rows it drew and no more ----

fn rig() -> Rig {
    Rig::new(KonaConfig::default())
}

/// A 16-bit index context, foreground 0x90 in colour word 1; for 16-bit images pixel 9 shows as 0x90.
const GC: [u32; 13] = [0x900, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];

fn textport(r: &mut Rig, selector: u32, args: &[u32]) {
    r.command(Grammar::Textport, selector, args);
}

/// A rig whose colour map shows every index but 0 in colour, its damage taken, and a viewer of it.
fn lit() -> (Rig, Viewer) {
    let mut r = rig();
    for i in 1..256 {
        textport(&mut r, TEXTPORT_LOAD_COLOUR_MAP, &[i, 0x3ff, i * 4, 0]);
    }
    textport(&mut r, TEXTPORT_SET_COLOUR, &[9]);
    let v = Viewer::new(&r.pixels());
    (r, v)
}

/// The hull a job names counts display rows from the top of 1024: store row `y` is `1023 - y` from the top.
fn from_top(y0: usize, y1: usize) -> Option<(usize, usize)> {
    Some((1024 - y1, 1024 - y0))
}

#[test]
fn a_textport_box_names_the_rows_it_filled() {
    let (mut r, mut v) = lit();
    textport(&mut r, TEXTPORT_FILL_BOX, &[9, 20, 40, 30]);
    let l = v.look(&r.pixels(), "a box");
    assert_eq!(l.named, from_top(21, 31), "the fill rule leaves out the bottom row");
    assert_eq!(l.changed, l.named);
    // A box the store clips to nothing names nothing.
    textport(&mut r, TEXTPORT_FILL_BOX, &[9, 2000, 40, 2010]);
    assert_eq!(v.look(&r.pixels(), "off the top").named, None);
    // Nor does one whose rows are on the store but whose columns are not.
    textport(&mut r, TEXTPORT_FILL_BOX, &[2000, 20, 2010, 30]);
    assert_eq!(v.look(&r.pixels(), "off the right").named, None);
}

#[test]
fn a_textport_point_names_its_row_and_colour_changes_name_none() {
    let (mut r, mut v) = lit();
    textport(&mut r, TEXTPORT_PLOT_POINT, &[100, 300]);
    let l = v.look(&r.pixels(), "a point");
    assert_eq!((l.named, l.changed), (from_top(300, 301), from_top(300, 301)));
    textport(&mut r, TEXTPORT_SET_COLOUR, &[0x5a]);
    textport(&mut r, TEXTPORT_SET_RASTER_POSITION, &[5, 5]);
    assert_eq!(v.look(&r.pixels(), "colour and position").named, None);
}

/// The colour map is what every pixel is shown through.
#[test]
fn a_textport_colour_map_load_names_every_row() {
    let (mut r, mut v) = lit();
    textport(&mut r, TEXTPORT_FILL_BOX, &[9, 20, 40, 30]);
    v.look(&r.pixels(), "a box");
    textport(&mut r, TEXTPORT_LOAD_COLOUR_MAP, &[9, 0, 0, 0x3ff]);
    let l = v.look(&r.pixels(), "a colour map load");
    assert_eq!(l.named, Some((0, 1024)));
    assert_eq!(l.changed, from_top(21, 31));
}

#[test]
fn a_bitmap_blit_names_the_rows_of_its_glyph() {
    let (mut r, mut v) = lit();
    textport(&mut r, TEXTPORT_SET_RASTER_POSITION, &[300, 700]);
    // 8 wide, 4 high, origin (0, 0): rows 700 to 703; the top and bottom rows have bits set.
    r.send(Job::Bitmap { header: &[8, 4, 0, 0, 9, 0], payload: &[0xff00_00ff] });
    let l = v.look(&r.pixels(), "a glyph");
    assert_eq!(l.named, from_top(700, 704));
    assert_eq!(l.changed, l.named);
    // A glyph off the store draws nothing.
    r.send(Job::Bitmap { header: &[8, 4, 0, 4000, 9, 0], payload: &[0xffff_ffff] });
    assert_eq!(v.look(&r.pixels(), "off the store").named, None);
}

#[test]
fn an_x_fill_names_the_rows_of_its_boxes() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    assert_eq!(v.look(&r.pixels(), "a context load").named, None);
    r.x(FILL_BOXES_TWO_CORNERS, &[2 << 16 | 3, 5 << 16 | 7]);
    let l = v.look(&r.pixels(), "a box");
    assert_eq!(l.named, from_top(1023 - 6, 1023 - 2), "X rows 3 to 6");
    assert_eq!(l.changed, l.named);
    // Two boxes: one hull.
    r.x(FILL_BOXES_TWO_CORNERS, &[10 << 16 | 100, 12 << 16 | 101]);
    r.x(FILL_BOXES_TWO_CORNERS, &[10 << 16 | 200, 12 << 16 | 201]);
    let l = v.look(&r.pixels(), "two boxes");
    assert_eq!(l.named, from_top(1023 - 200, 1023 - 99));
    assert_eq!(l.changed, l.named);
}

#[test]
fn an_x_image_names_the_rows_it_wrote() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0, 1, 1, 4, 4, 2, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[4, 2]);
    r.send(Job::Data { grammar: Grammar::Kernel, selector: WINDOW_SYSTEM_PIXEL_WRITE, args: &[4, 2], words: &[0x0009_0009; 4] });
    let l = v.look(&r.pixels(), "an image");
    assert_eq!(l.named, from_top(1023 - 3, 1023 - 1), "X rows 2 and 3");
    assert_eq!(l.changed, l.named);
}

/// An image cut by the scissor names the rows inside it.
#[test]
fn an_x_image_cut_by_the_scissor_names_the_rows_inside_it() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_SCISSOR_LOAD, &[0, 0, 100, 3]);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0, 1, 1, 4, 4, 2, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[4, 2]);
    r.send(Job::Data { grammar: Grammar::Kernel, selector: WINDOW_SYSTEM_PIXEL_WRITE, args: &[4, 2], words: &[0x0009_0009; 4] });
    let l = v.look(&r.pixels(), "an image under a scissor");
    assert_eq!(l.named, Some((2, 3)), "X row 2");
    assert_eq!(l.changed, l.named);
}

/// An image wholly outside the scissor names nothing.
#[test]
fn an_x_image_outside_the_scissor_names_nothing() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_SCISSOR_LOAD, &[0, 0, 3, 100]);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0, 1, 1, 4, 4, 2, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[4, 2]);
    r.send(Job::Data { grammar: Grammar::Kernel, selector: WINDOW_SYSTEM_PIXEL_WRITE, args: &[4, 2], words: &[0x0009_0009; 4] });
    assert_eq!(v.look(&r.pixels(), "an image right of the scissor").named, None);
}

/// An image cut by the plan's clip names the rows inside the clip.
#[test]
fn an_x_image_past_the_bottom_names_the_rows_that_fit() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0, 1, 1, 4, 4, 4, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[4, 1022]);
    r.send(Job::Data { grammar: Grammar::Kernel, selector: WINDOW_SYSTEM_PIXEL_WRITE, args: &[4, 1022], words: &[0x0009_0009; 8] });
    let l = v.look(&r.pixels(), "an image off the bottom");
    assert_eq!(l.named, from_top(0, 2), "X rows 1022 and 1023");
    assert_eq!(l.changed, l.named);
}

#[test]
fn a_dma_image_names_the_rows_it_wrote() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0x10, 1, 1, 4, 4, 3, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[0, 50]);
    r.put_words(0x40_0000, &[0x0009_0009; 6]);
    r.send(Job::DmaIn { io: 0x40_0000, physical: true, lines: 3, per_line: 8, pitch: 8 });
    let l = v.look(&r.pixels(), "a DMA image");
    assert_eq!(l.named, from_top(1023 - 52, 1023 - 49), "X rows 50 to 52");
    assert_eq!(l.changed, l.named);
}

#[test]
fn an_x_copy_names_the_rows_it_wrote() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(FILL_BOXES_TWO_CORNERS, &[0, 4 << 16 | 4]);
    v.look(&r.pixels(), "the source");
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[1, 1, 1, 0, 3, 3, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_COPY, &[1, 1, 10, 500]);
    let l = v.look(&r.pixels(), "a copy");
    assert_eq!(l.named, from_top(1023 - 502, 1023 - 499), "X rows 500 to 502");
    assert_eq!(l.changed, l.named);
}

/// A read, a finish and a setup draw nothing.
#[test]
fn a_pixel_read_and_the_state_commands_name_nothing() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(FILL_BOXES_TWO_CORNERS, &[2 << 16 | 3, 5 << 16 | 7]);
    v.look(&r.pixels(), "a box");
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[2, 1, 0, 8, 3, 1, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_READ, &[2, 3]);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_FINISH, &[0]);
    r.x(WINDOW_SYSTEM_DRAWABLE_ORIGIN_AND_SIZE_LOAD, &[0, 0, 100, 100]);
    assert_eq!(v.look(&r.pixels(), "reads and state").named, None);
}

/// What can change how the picture is shown, or any row of it, still names every row.
#[test]
fn display_state_still_names_every_row() {
    for (what, step) in [
        ("a display ID table entry", Box::new(|r: &mut Rig| { r.x(LOAD_ONE_ENTRY_OF_DISPLAY_ID_TABLE, &[1, 0x1234]); }) as Box<dyn Fn(&mut Rig)>),
        ("a display mode byte", Box::new(|r| { r.x(DISPLAY_MODE_BYTE, &[0, 1, 4]); })),
        ("a swap", Box::new(|r| { r.x(DISPLAY_MODE_BYTE, &[1, 1, 1]); })),
        ("a colour map", Box::new(|r| { r.send(Job::Data { grammar: Grammar::Kernel, selector: LOAD_COLOUR_MAP_ENTRIES, args: &[0, 1], words: &[0x3ff] }); })),
        ("a gamma table", Box::new(|r| { r.send(Job::Data { grammar: Grammar::Kernel, selector: LOAD_GAMMA_TABLE_ENTRIES, args: &[0, 1], words: &[0x123] }); })),
        ("a cursor position", Box::new(|r| { r.send(Job::Indirect { selector: 0x102, value: 0x10_0010 }); })),
    ] {
        let (mut r, mut v) = lit();
        step(&mut r);
        assert_eq!(v.look(&r.pixels(), what).named, Some((0, 1024)), "{what}");
    }
}

fn pack(x: i32, y: i32) -> u32 {
    (x as u32) << 16 | (y as u32 & 0xffff)
}

/// Every other way the X server draws reaches the store through the same fill op: no row is left stale.
#[test]
fn the_other_x_draws_leave_no_row_stale() {
    let (mut r, mut v) = lit();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_DRAWABLE_ORIGIN_AND_SIZE_LOAD, &[0, 0, 100, 100]);
    let steps: Vec<(&str, Box<dyn Fn(&mut Rig)>)> = vec![
        ("absolute points", Box::new(|r| { r.x(DRAW_ABSOLUTE_POINTS, &[pack(3, 4)]); })),
        ("background points", Box::new(|r| { r.x(DRAW_POINTS_IN_BACKGROUND_COLOUR, &[pack(5, 40)]); })),
        ("relative points", Box::new(|r| { r.x(DRAW_RELATIVE_POINTS, &[pack(7, 60)]); })),
        ("segments", Box::new(|r| { r.x(DRAW_LINE_SEGMENTS, &[pack(1, 70), pack(30, 90)]); })),
        ("spans", Box::new(|r| { r.x(FILL_SPANS_BURST_OF_2, &[2, pack(1, 20), pack(4, 22), 5, 2]); })),
        ("a two-rectangle clip", Box::new(|r| {
            r.x(WINDOW_SYSTEM_CLIP_LOAD, &[2, 0, 0, 10, 10, 0, 50, 10, 60]);
            r.x(FILL_BOXES_TWO_CORNERS, &[0, pack(20, 99)]);
        })),
        ("an exclusive-or over drawn pixels", Box::new(|r| {
            let mut gc = GC;
            gc[GC_BLEND_AND_LOGIC] = 12;
            r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &gc);
            r.x(FILL_BOXES_TWO_CORNERS, &[pack(0, 0), pack(8, 8)]);
        })),
    ];
    for (what, step) in steps {
        step(&mut r);
        let l = v.look(&r.pixels(), what);
        assert!(l.named.is_some(), "{what} named no row: {l:?}");
    }
}

/// The selectors of the X server path that draw or only load state for a draw: no picture row depends on them
/// beyond what they draw.
fn x_path(selector: u32) -> bool {
    fill::paints(selector)
        || fill::is_draw(selector)
        || x_pixel::is_pixel(selector)
        || matches!(
            selector,
            WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD
                | WINDOW_SYSTEM_DRAWABLE_ORIGIN_AND_SIZE_LOAD
                | WINDOW_SYSTEM_FOUR_WORD_DATA_PRELOAD_BYTE_FORM
                | WINDOW_SYSTEM_CLIP_LOAD
                | WINDOW_SYSTEM_SCISSOR_LOAD
                | sgi_kona::dev::kona::back::x::stipple::X_LOAD_STIPPLE
                | sgi_kona::dev::kona::back::x::stipple::X_STORE_STIPPLE
                | sgi_kona::dev::kona::back::x::stipple::X_ROTATE_STIPPLE
        )
}

/// Each kernel selector with zero operands names every row unless it is the X server path's.
#[test]
fn only_the_x_server_path_of_the_kernel_commands_names_fewer_rows() {
    let (mut r, mut v) = lit();
    let zeros = [0u32; 16];
    for selector in 0..0x400 {
        v.look(&r.pixels(), "before");
        r.send(Job::Command { grammar: Grammar::Kernel, selector, args: &zeros, packed: 0, handled: false });
        let l = v.look(&r.pixels(), &format!("kernel selector {selector:#x}"));
        assert_eq!(l.named == Some((0, 1024)), !x_path(selector), "kernel selector {selector:#x} named {:?}", l.named);
    }
}
