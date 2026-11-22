//! The picture a head shows: which rows of it changed, and the rows composed for the shell.

use parking_lot::Mutex;
use std::sync::Arc;
use sgi_engine::{BoardEngine, HostMem, MemFault, Source, Swap, Write};
use sgi_kona::dev::kona::back::gl::{class, Class};
use sgi_kona::dev::kona::back::display::dac_cursor::{DacCursor, WORDS};
use sgi_kona::dev::kona::back::display::tables::Tables;
use sgi_kona::dev::kona::back::display::{ColourMap, Pixels};
use sgi_kona::dev::kona::back::store::layout::MAIN_COLOUR_WORD;
use sgi_kona::dev::kona::back::store::{Store, StoreOp, DID_PLANE};
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{head_rows, KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;
use sgi_kona::dev::kona::job::{ContextEvent, Grammar, Job};
use sgi_kona::dev::kona::rig::Rig;

#[derive(Default)]
struct Ram(Mutex<Vec<u8>>);

impl HostMem for Ram {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let m = self.0.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = m.get(addr as usize + i).copied().unwrap_or(0));
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        let mut m = self.0.lock();
        if m.len() < addr as usize + data.len() {
            m.resize(addr as usize + data.len(), 0);
        }
        m[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
}

type Engine = KonaEngine<Ram>;

const A: Source = Source { id: 1, charged: true };
const ALL: Option<(usize, usize)> = Some((0, 1024));

// ---- the engine's tiles mark the rows they draw; everything else marks all ----

/// A 64 x 64 GL window at the origin of the managed area (its rows are the bottom 64 of the display), drawn
/// on the caller's thread, with a colour set.
fn gl_window() -> Rig {
    let mut r = Rig::new(KonaConfig::default());
    r.x(WINDOW_CLIP_SETUP, &[0, 0, 64, 64, 0, 0, 64, 64, 0, 13, 0]);
    r.command(Grammar::Gl, CONFIGURE_FRAME_BUFFER_LAYOUT, &[0, 8, 0, 1, 0, 0, 24, 0, 0, 0]);
    r.command(Grammar::Gl, SET_VIEWPORT, &[0, 0, 64, 64]);
    r.command(Grammar::Gl, SET_CURRENT_COLOUR_4_COMPONENTS_SINGLE_FLOAT, &[1.0f32.to_bits(); 4]);
    r
}

fn ndc(w: f32) -> u32 {
    (w / 32.0 - 1.0).to_bits()
}

fn point(r: &mut Rig, x: f32, y: f32) {
    r.command(Grammar::Gl, BEGIN_PRIMITIVE_POINTS, &[0]);
    r.command(Grammar::Gl, SUBMIT_VERTEX_3_COMPONENTS_SINGLE_FLOAT, &[ndc(x), ndc(y), 0.0f32.to_bits()]);
    r.command(Grammar::Gl, END_OPEN_PRIMITIVE, &[0]);
}

fn damage(r: &Rig) -> Option<(usize, usize)> {
    r.pixels().take_damage()
}

#[test]
fn a_new_store_is_all_damage_once() {
    let r = Rig::new(KonaConfig::default());
    assert_eq!(damage(&r), ALL);
    assert_eq!(damage(&r), None);
}

/// A draw through the tiles marks the rows its rectangle covers, counted from the top.
#[test]
fn a_geometry_draw_marks_only_the_rows_it_wrote() {
    let mut r = gl_window();
    damage(&r);
    point(&mut r, 10.5, 10.5);
    r.back.drain();
    // Store row 10 is the 1013th from the top of 1024.
    assert_eq!(damage(&r), Some((1013, 1014)));
    assert_eq!(damage(&r), None, "taken once");
    point(&mut r, 3.5, 40.5);
    point(&mut r, 3.5, 20.5);
    r.back.drain();
    assert_eq!(damage(&r), Some((1024 - 41, 1024 - 20)), "two draws, one hull");
}

/// A geometry command that draws nothing, a retrace request and a read of the pipe change no pixel.
#[test]
fn what_changes_no_pixel_marks_nothing() {
    let mut r = gl_window();
    damage(&r);
    r.command(Grammar::Gl, SET_CURRENT_COLOUR_4_COMPONENTS_SINGLE_FLOAT, &[0.5f32.to_bits(); 4]);
    assert_eq!(damage(&r), None, "a colour");
    r.send(Job::Retrace { tag: 1 });
    assert_eq!(damage(&r), None, "a retrace request");
    let mut e = engine();
    e.take_damage(0);
    let _ = e.read(RETRACE_TAG, 32);
    let _ = e.read(0x800208, 32);
    assert_eq!(e.take_damage(0), None, "a read");
}

/// Each of these can change what the whole picture shows or how, or writes pixels no tile wrote: all rows.
#[test]
fn what_else_can_change_the_picture_marks_every_row() {
    const GC: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
    let steps: Vec<(&str, Box<dyn Fn(&mut Rig)>)> = vec![
        ("an X fill", Box::new(|r| {
            r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
            r.x(FILL_BOXES_TWO_CORNERS, &[2 << 16 | 3, 5 << 16 | 7]);
        })),
        ("a colour map load", Box::new(|r| {
            r.send(Job::Data { grammar: Grammar::Kernel, selector: LOAD_COLOUR_MAP_ENTRIES, args: &[0, 1], words: &[0x3ff] });
        })),
        ("a gamma load", Box::new(|r| {
            r.send(Job::Data { grammar: Grammar::Kernel, selector: LOAD_GAMMA_TABLE_ENTRIES, args: &[0, 1], words: &[0x123] });
        })),
        ("a cursor glyph load", Box::new(|r| {
            r.send(Job::Data { grammar: Grammar::Kernel, selector: LOAD_CURSOR_IMAGE_BLOCK, args: &[0], words: &[1; 4] });
        })),
        ("a cursor position", Box::new(|r| {
            r.send(Job::Indirect { selector: 0x102, value: 0x10_0010 });
        })),
        ("a display ID table entry", Box::new(|r| {
            r.x(LOAD_ONE_ENTRY_OF_DISPLAY_ID_TABLE, &[1, 0x1234]);
        })),
        ("a display mode byte", Box::new(|r| {
            r.x(DISPLAY_MODE_BYTE, &[0, 1, 4]);
        })),
        ("a swap", Box::new(|r| {
            r.x(DISPLAY_MODE_BYTE, &[1, 1, 1]);
        })),
        ("a clear of the accumulation planes", Box::new(|r| {
            r.command(Grammar::Gl, CLEAR_SELECTED_BUFFERS, &[sgi_kona::dev::kona::defs::gl::ACCUM_BUFFER_BIT]);
        })),
        ("a reset", Box::new(|r| r.back.reset())),
    ];
    for (what, step) in steps {
        let mut r = gl_window();
        damage(&r);
        step(&mut r);
        assert_eq!(damage(&r), ALL, "{what}");
    }
    // Every other job the back end is sent is no geometry command either: each GL selector of another class, each
    // kernel selector, and the other kinds of job, with zero operands.
    let mut r = gl_window();
    let mut each = |what: String, job: Job<'_>| {
        damage(&r);
        r.send(job);
        assert_eq!(damage(&r), ALL, "{what}");
    };
    let zeros = [0u32; 16];
    for selector in (0..0x200).filter(|&s| class(s) != Class::Geometry) {
        each(format!("GL selector {selector:#x} ({:?})", class(selector)), Job::Command { grammar: Grammar::Gl, selector, args: &zeros, packed: 0, handled: false });
    }
    for selector in 0..0x400 {
        each(format!("kernel selector {selector:#x}"), Job::Command { grammar: Grammar::Kernel, selector, args: &zeros, packed: 0, handled: false });
    }
    each("a GL data run".into(), Job::Data { grammar: Grammar::Gl, selector: DRAW_TEXELS_INTO_TEXTURE_MEMORY, args: &zeros, words: &zeros });
    each("a bitmap blit".into(), Job::Bitmap { header: &[4, 4, 0, 0, 0, 0], payload: &[0xf000_f000; 4] });
    each("a DMA in".into(), Job::DmaIn { io: 0x1000, physical: true, lines: 1, per_line: 16, pitch: 16 });
    for command in 0..0x80 {
        each(format!("video command {command:#x}"), Job::Video { command, data: &zeros });
    }
    each("a firmware start".into(), Job::FirmwareStart { combination: &zeros });
    each("a context activated".into(), Job::Context(ContextEvent::Activated { slot: 1, fresh: true }));
    each("a context switched out".into(), Job::Context(ContextEvent::Deactivated { slot: 1 }));
    each("a window released".into(), Job::Context(ContextEvent::WindowReleased { slot: 0 }));
    each("a word no framer knew".into(), Job::Dropped { word: 0 });
}

/// Rows count up from the bottom in the store and down from the top in the damage, across the words of the bitmap.
#[test]
fn rows_of_a_tile_are_marked_across_the_words_of_the_bitmap() {
    let store = Store::with_texture(8, 200, Default::default());
    let rows = |y0, y1| {
        store.cpu().take_damage();
        store.cpu().touch_rows(y0, y1);
        store.cpu().take_damage()
    };
    assert_eq!(rows(0, 200), Some((0, 200)), "all of it");
    assert_eq!(rows(0, 1), Some((199, 200)), "the bottom row");
    assert_eq!(rows(199, 200), Some((0, 1)), "the top row");
    assert_eq!(rows(10, 150), Some((50, 190)), "across three words");
    assert_eq!(rows(136, 200), Some((0, 64)), "a word exactly");
    assert_eq!(rows(70, 70), None, "no rows");
    assert_eq!(rows(-5, 3), Some((197, 200)), "clipped below");
    assert_eq!(rows(190, 400), Some((0, 10)), "clipped above");
}

/// A swap held for the retrace shows at it, with no store behind it.
#[test]
fn a_swap_shows_at_the_retrace_and_marks_every_row() {
    let mut r = gl_window();
    r.back.latch_swaps(true);
    r.x(DISPLAY_MODE_BYTE, &[1, 1, 1]);
    damage(&r);
    r.back.frame();
    assert_eq!(damage(&r), ALL);
    r.back.frame();
    assert_eq!(damage(&r), None, "once");
}

// ---- a head sees the rows of its own window ----

fn engine() -> Engine {
    Engine::new(KonaConfig::ir2(2).unwrap().with_pool(0, 32), Ram::default(), Options::default(), None).expect("engine")
}

fn store(e: &mut Engine, off: u32, val: u32) {
    e.set_source(A);
    assert_eq!(e.write(off, 32, val as u64), Write::Done);
    e.run(usize::MAX);
}

fn kernel(e: &mut Engine, token: u32, words: &[u32]) {
    store(e, COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0]);
    words[1..].iter().for_each(|&w| store(e, COMMAND_PAGE, w));
}

fn video(e: &mut Engine, command: u32, data: &[u32]) {
    let sum = data.iter().fold(command, |s, w| s.wrapping_add(*w));
    for w in [0x1a6, command].into_iter().chain(data.iter().copied()).chain([sum]) {
        store(e, COMMAND_PAGE, w);
    }
}

fn load_format(e: &mut Engine, channel: u32, pixels: u32, lines: u32) {
    let mut format = [0u32; 383];
    (format[0], format[1], format[19], format[20], format[21], format[22], format[23], format[24]) = (channel, 1, 60 << 16, 1, lines + 40, pixels + 100, lines, pixels);
    video(e, VIDEO_LOAD_FORMAT, &format);
}

/// Two heads: head 0 the whole 1280 x 1024, head 1 a 640 x 480 window whose top row is the 500th from the
/// bottom (display row 524) at x 16.
fn two_heads() -> Engine {
    let mut e = engine();
    load_format(&mut e, 0, 1280, 1024);
    load_format(&mut e, 1, 640, 480);
    video(&mut e, VIDEO_RESIZE, &[1, 16, 499, 640, 480, 1, 1, 0]);
    assert_eq!((e.heads(), e.frame_size(1)), (2, Some((640, 480))));
    e
}

#[test]
fn a_head_is_told_the_first_look_and_then_what_changed() {
    let mut e = two_heads();
    assert_eq!((e.take_damage(0), e.take_damage(1)), (Some((0, 1024)), Some((0, 480))), "the first look is everything");
    assert_eq!((e.take_damage(0), e.take_damage(1)), (None, None));
}

/// A hull of the managed rows is the rows of the window it meets, counted from the window's first.
#[test]
fn a_head_takes_the_rows_of_the_hull_that_its_window_shows() {
    // Head 1's window is the rows 524 to 1003.
    assert_eq!(head_rows((1011, 1012), 524, 480), None, "below the window");
    assert_eq!(head_rows((0, 524), 524, 480), None, "above it, to its first row");
    assert_eq!(head_rows((500, 530), 524, 480), Some((0, 6)), "across its top");
    assert_eq!(head_rows((1000, 1024), 524, 480), Some((476, 480)), "across its bottom");
    assert_eq!(head_rows((600, 601), 524, 480), Some((76, 77)));
    assert_eq!(head_rows((0, 1024), 0, 1024), Some((0, 1024)), "a whole-area head");
}

/// Damage one head has not asked for stays for it.
#[test]
fn damage_waits_for_a_head_that_has_not_asked() {
    let mut e = two_heads();
    e.take_damage(0);
    e.take_damage(1);
    kernel(&mut e, WINDOW_CLIP_SETUP, &[0, 0, 64, 64, 0, 0, 64, 64, 0, 13, 0]);
    e.take_damage(0);
    assert_eq!(e.take_damage(1), Some((0, 480)));
}

/// A swap held for the retrace shows at it.
#[test]
fn the_engine_marks_the_retrace_that_shows_a_swap() {
    let mut e = engine();
    e.take_damage(0);
    kernel(&mut e, DISPLAY_MODE_BYTE, &[0x3c0001, 13, 1]);
    assert!(e.swap_waiting());
    e.take_damage(0);
    e.frame(0);
    assert_eq!(e.take_damage(0), Some((0, 1024)));
    assert_eq!(e.take_damage(0), None);
}

// ---- rows composed into the caller's buffer ----

const W: u32 = 96;
const H: u32 = 48;

/// A 96 x 48 store with a pattern in the colour word and the display ID plane (every other 16 columns the
/// cleared window 5, the rest the textport's), the glyph cursor and the DAC cursor both on and each over two of
/// the 16-row bands the compose works in.
fn patterned() -> Pixels {
    let store = Arc::new(Store::with_texture(W, H, Default::default()));
    for y in 0..H as i64 {
        for x in 0..W as i64 {
            store.apply(StoreOp::Plot { word: MAIN_COLOUR_WORD as usize, x, y, value: ((x * 3 + y * 5) & 0xff) as u32 });
            store.apply(StoreOp::Plot { word: DID_PLANE, x, y, value: if x / 16 % 2 == 1 { 5 << 2 } else { 0 } });
        }
    }
    let mut map = ColourMap::default();
    for i in 0..256u32 {
        map.load(&[i, (i * 4) & 0x3ff, (i * 8) & 0x3ff, (i * 12) & 0x3ff]);
    }
    let mut t = Tables::default();
    t.load_did(5, 0);
    let mut glyph = [0xaaaa_aaaau32; 260];
    glyph[256..].copy_from_slice(&[0x3ff, 0x2aa, 0x155, 0]);
    t.load_cursor(0, &glyph);
    t.indirect(XMAP_CURSOR_IMAGE_BASE, 0);
    t.indirect(XMAP_CURSOR_POSITION, 42 << 13 | 52);
    t.indirect(XMAP_CURSOR_MODE, 2);
    let mut request = vec![0xffff_ffffu32; WORDS];
    request[..7].copy_from_slice(&[0, 0x3ff, 0, 0x155, 0x0aa, 0x0bb, 0x0cc]);
    t.load_dac_cursor(DacCursor::from_request(&request).expect("a cursor"));
    let dac = |t: &mut Tables, index: u32, values: &[u32]| {
        for (i, v) in [index & 0xff, index >> 8].into_iter().enumerate() {
            let _ = i;
            t.indirect(0xb00, v);
        }
        values.iter().for_each(|&v| t.indirect(0xb02, v));
    };
    dac(&mut t, 0x204, &[5]);
    dac(&mut t, 0x200, &[40, 0, 14, 0]);
    Pixels::new(store, Arc::new(Mutex::new(map)), Arc::new(Mutex::new(t)))
}

/// Rows `rows` of the window of `frame`, as the shells see it.
fn window_of(frame: &[u32], (x, y, w, h): (u32, u32, u32, u32)) -> Vec<u32> {
    (y..y + h).flat_map(|row| frame[(row * W + x) as usize..(row * W + x + w) as usize].iter().map(|p| (p & 0xff) << 16 | (p & 0xff00) | (p >> 16 & 0xff))).collect()
}

#[test]
fn rows_of_a_window_are_the_rows_of_the_whole_frame() {
    let pixels = patterned();
    let frame = pixels.frame();
    // The cursors are on the frame: without them the rows below would not test their placement.
    let plain = Pixels::new(Arc::new(Store::with_texture(W, H, Default::default())), Default::default(), Default::default()).frame();
    assert_ne!(frame, plain);
    for window in [(0, 0, W, H), (8, 3, 60, 37), (0, 20, W, 28)] {
        let whole = window_of(&frame, window);
        let h = window.3 as usize;
        for rows in [0..h, 0..1, 5..h - 2, 15..17, 16..33, 31..h, h - 1..h + 4] {
            const KEEP: u32 = 0xdead_beef;
            let mut out = vec![KEEP; whole.len()];
            pixels.window_rows(window, rows.clone(), false, &mut out, &mut Vec::new());
            let w = window.2 as usize;
            for row in 0..h {
                let want: &[u32] = if rows.contains(&row) { &whole[row * w..(row + 1) * w] } else { &[KEEP; 96][..w] };
                assert_eq!(&out[row * w..(row + 1) * w], want, "window {window:?} rows {rows:?} row {row}");
            }
        }
    }
}

#[test]
fn a_blank_window_is_black_in_the_rows_asked_for() {
    let pixels = patterned();
    let mut out = vec![7; 60 * 37];
    pixels.window_rows((8, 3, 60, 37), 4..9, true, &mut out, &mut Vec::new());
    assert!(out[..4 * 60].iter().all(|&p| p == 7));
    assert!(out[4 * 60..9 * 60].iter().all(|&p| p == 0));
    assert!(out[9 * 60..].iter().all(|&p| p == 7));
}

/// The compose asks for a buffer that holds a window: a smaller one is the caller's mistake, not a write past it.
#[test]
#[should_panic(expected = "too small")]
fn a_buffer_that_cannot_hold_the_window_is_refused() {
    let pixels = patterned();
    pixels.window_rows((0, 0, W, H), 0..H as usize, false, &mut vec![0; 10], &mut Vec::new());
}
