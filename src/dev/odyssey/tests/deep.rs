//! Windows that keep more than eight bits a channel: 10:10:10:2 (the 30-plane visuals) and 12-bit
//! lanes. A colour drawn into one, read back and shown keeps its width, and an 8-bit window is as before.

use crate::dev::odyssey::board::Board;
use crate::dev::odyssey::consts::{dispmode, reg, write, ODSY_USER_CMD_FIFO};
use crate::dev::odyssey::field::{deep8, Deep, Layout};
use crate::dev::odyssey::tests::display::{assert_same, backend, dispmode as set_dispmode, follow, whole, DISPMODE_RGB};
use crate::dev::odyssey::tests::pixels::{dma_read, dma_read_from, dma_write_sized, read_word4, window, with_fields};
use crate::dev::odyssey::tests::{select, Ram};
use crate::dev::odyssey::view::{Rows, View, MAX_DIM};
use crate::dev::odyssey::vram::{Format, Surface};
use std::sync::Arc;
use super::dma::{descriptor, pte, put_table, PAGE_A, TABLE};

/// Status words of the test screen's window with 4-byte 10:10:10:2 fields and 8-byte 12-bit lanes.
const STATUS_10: u32 = 0x0040_6d60;
const STATUS_12: u32 = 0x0040_6cd0;

/// A window's display mode: field units, the A and B field's offsets in 2-byte units, display format.
fn mode(units: u32, a: u32, b: u32, format: u32) -> u32 {
    (DISPMODE_RGB & !(7 << 19 | 7 << 16 | 7 << 13 | 0xf << dispmode::WIDTH_SHIFT)) | units << 19 | a << 16 | b << 13 | format << dispmode::WIDTH_SHIFT
}

/// The gamma table that shows a channel as it is: ten bits out of twelve in, by the table's three ranges
/// (entries 0 to 0x1ff take the value, 0x200 to 0x2ff every second one, then every fourth).
fn ramp(s: &mut Board) {
    backend(s, 0x3000_0000 | 0x1a00 << 14 | 0x600);
    for t in 0..0x600u32 {
        let v = if t < 0x200 { t >> 2 } else if t < 0x300 { (t - 0x100) >> 1 } else { t - 0x200 };
        backend(s, v << 20 | v << 10 | v);
    }
}

/// A window of `status` on the test screen, shown through the identity gamma ramp: the board, its
/// RAM, and the front colour field.
fn deep_window(status: u32, display: u32) -> (Box<Board>, Arc<Ram>, Rows) {
    deep_window_of(status, display, 80)
}

/// The same with a surface `pitch` tiles wide.
fn deep_window_of(status: u32, display: u32, pitch: u32) -> (Box<Board>, Arc<Ram>, Rows) {
    let (mut s, ram) = window();
    for w in [select(reg::DEST_SELECT), 0x2000, pitch, status, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
    ramp(&mut s);
    set_dispmode(&mut s, 0, display);
    let f = Format::decode(status);
    let l = Layout::of(f);
    let rows = Rows::new(s.vram_ptr(), View::colour(&Surface::new(0x2000, pitch, f.fb_log2), l.front, MAX_DIM));
    (s, ram, rows)
}

fn window_10() -> (Box<Board>, Arc<Ram>, Rows) {
    deep_window(STATUS_10, mode(2, 6, 2, 9))
}

fn window_12() -> (Box<Board>, Arc<Ram>, Rows) {
    deep_window(STATUS_12, mode(4, 4, 0, 6))
}

/// The row's 10-bit gradient: red up, green down, blue by sevens.
fn ten(x: u32) -> [u32; 3] {
    [x, 1023 - x, (x * 7) & 1023]
}

fn bytes_of(words: impl Iterator<Item = u32>) -> Vec<u8> {
    words.flat_map(u32::to_be_bytes).collect()
}

/// The pixels of a wide frame's row 0 as (red, green, blue) of ten bits.
fn shown_wide(s: &mut Board, n: u32) -> Vec<[u32; 3]> {
    s.scanout_update();
    let (w, _, frame) = s.test_reader().compose_wide().expect("a frame");
    frame[..n.min(w) as usize].iter().map(|p| [(p >> 20) & 0x3ff, (p >> 10) & 0x3ff, p & 0x3ff]).collect()
}

/// A 10:10:10:2 image DMA'd to a 30-plane window is stored whole, read back whole and shown with
/// ten bits a channel; that the board keeps them is inferred (odyssey.rst, deep colour store).
#[test]
fn a_10_bit_image_is_stored_read_and_shown_with_ten_bits() {
    let (mut s, ram, rows) = window_10();
    let image = bytes_of((0..1024).map(|x| {
        let [r, g, b] = ten(x);
        r << 22 | g << 12 | b << 2 | 3
    }));
    // RGBA, UNSIGNED_INT_10_10_10_2: format 0xb, type 8.
    dma_write_sized(&mut s, &ram, (0, 0, 1024), 0x1000, 0x10b, 4, &image);
    for x in [0, 1, 2, 3, 255, 256, 511, 512, 1000, 1023] {
        let [r, g, b] = ten(x as u32);
        assert_eq!(rows.load_raw(x, 0) >> 32, 0, "four bytes");
        assert_eq!(rows.load_raw(x, 0) as u32, r << 22 | g << 12 | b << 2 | 3, "the word at {x}");
    }
    let shown = shown_wide(&mut s, 1024);
    for x in 0..1024u32 {
        assert_eq!(shown[x as usize], ten(x), "pixel {x} on a ten-bit ramp");
    }
    // Its top eight bits are what the 8-bit frame shows, in all three channels.
    s.scanout_update();
    let [r8, g8, b8] = ten(1000).map(|c| c >> 2);
    assert_eq!(s.test_reader().frame[1000] & 0xff_ffff, r8 << 16 | g8 << 8 | b8);
    // Read back as the same type: the pixel, whole.
    let word4 = with_fields(read_word4(2, 12), 6, 2);
    let got = dma_read(&mut s, &ram, (1000, 0, 1), word4, 4);
    let [r, g, b] = ten(1000);
    assert_eq!(u32::from_be_bytes(got[..4].try_into().unwrap()), 3 << 30 | b << 20 | g << 10 | r, "2_10_10_10_REV");
    let got = dma_read(&mut s, &ram, (1000, 0, 1), with_fields(read_word4(2, 0), 6, 2), 4);
    assert_eq!(u32::from_be_bytes(got[..4].try_into().unwrap()), r << 22 | g << 12 | b << 2 | 3, "10_10_10_2");
}

/// A 16-bit RGBA image DMA'd to a 12-bit window keeps twelve bits, reads back spread to sixteen and
/// shows through the gamma table at twelve bits in (inferred; the rounding is an approximation).
#[test]
fn a_16_bit_image_is_stored_read_and_shown_with_twelve_bits() {
    let (mut s, ram, rows) = window_12();
    let twelve = |x: u32| -> Deep { [x, 4095 - x, (x * 5) & 4095, 4095] };
    let spread = |v: u32| (v << 4 | v >> 8) as u16;
    let mut image = Vec::new();
    for x in 0..1024u32 {
        // Every fourth twelve-bit value, so the row covers the range in 1024 pixels.
        for c in twelve(x * 4 + 1) {
            image.extend_from_slice(&spread(c).to_be_bytes());
        }
    }
    // RGBA, UNSIGNED_SHORT: format 0xb, type 3.
    dma_write_sized(&mut s, &ram, (0, 0, 1024), 0x1000, 0x6b, 8, &image);
    for x in [0, 1, 255, 256, 511, 1023] {
        assert_eq!(rows.load_deep(x, 0), Some(twelve(x as u32 * 4 + 1)), "pixel {x} kept its twelve bits");
    }
    // The identity ramp maps twelve bits in to ten out: the top ten of the twelve.
    let shown = shown_wide(&mut s, 1024);
    for x in 0..1024u32 {
        let c = twelve(x * 4 + 1);
        assert_eq!(shown[x as usize], [c[0] >> 2, c[1] >> 2, c[2] >> 2], "pixel {x}");
    }
    // Read back as RGBA, UNSIGNED_SHORT: the twelve bits, spread.
    let word4 = (with_fields(read_word4(3, 9), 4, 0) & !0xe000) | 2 << 13;
    let got = dma_read(&mut s, &ram, (300, 0, 1), word4, 8);
    let c = twelve(300 * 4 + 1);
    let want: Vec<u8> = c.iter().flat_map(|&v| spread(v).to_be_bytes()).collect();
    assert_eq!(got[..8], want[..]);
}

/// An 8-bit picture shows the same in a 12-bit window as in an 8-bit one, by the draw drawable (which stores an
/// 8-bit source at the field's width) as by the old per-pixel path.
#[test]
fn an_8_bit_picture_shows_the_same_in_a_12_bit_window() {
    let (mut deep, ram, _) = window_12();
    let (mut flat, ram8) = window();
    ramp(&mut flat);
    set_dispmode(&mut flat, 0, DISPMODE_RGB);
    let pixels: Vec<u8> = (0..256u32).flat_map(|x| [x as u8, (255 - x) as u8, (x * 3) as u8, 0xff]).collect();
    // RGBA, UNSIGNED_BYTE.
    dma_write_sized(&mut deep, &ram, (0, 0, 256), 0x1000, 0x4b, 4, &pixels);
    dma_write_sized(&mut flat, &ram8, (0, 0, 256), 0x1000, 0x4b, 4, &pixels);
    assert_eq!(shown_wide(&mut deep, 256), shown_wide(&mut flat, 256));
    // The 8-bit frame, which the QEMU shell shows, agrees in every channel.
    deep.scanout_update();
    flat.scanout_update();
    assert_eq!(deep.test_reader().frame[..256], flat.test_reader().frame[..256]);
}

/// A pixel-transfer scale or bias keeps an image written by address off the deep path: a 16-bit image the
/// 8-bit path does not take is left out, not stored unscaled.
#[test]
fn a_scaled_image_does_not_take_the_deep_path() {
    let (mut s, ram, rows) = window_12();
    let image: Vec<u8> = (0..16u16).flat_map(|x| [0x1000 + x, 0x2000, 0x3000, 0xffff]).flat_map(u16::to_be_bytes).collect();
    ram.put(PAGE_A, &image);
    let word4 = 0x8000_0000 | 4 << 20 | 2 << 13 | 3 << 16 | 0x42;
    let header = [0x2000, 0, 80, 1 << 16 | 16, word4, 0, 16 * 8, 0x3200_0000 | 2 << 16 | 0x6b];
    let blank = rows.load_deep(3, 0);
    s.tex_mut().scale_bias[0] = 2048;
    put_table(&ram, TABLE, header, &[pte(PAGE_A, 16 * 8)]);
    descriptor(&mut s, TABLE);
    assert_eq!(rows.load_deep(3, 0), blank, "not stored");
    s.tex_mut().scale_bias[0] = crate::dev::odyssey::consts::tex::SCALE_BIAS_ONE;
    descriptor(&mut s, TABLE);
    assert_eq!(rows.load_deep(3, 0).map(|c| c[1]), Some(0x200), "stored once the scale is the identity");
}

/// A deep image written to the draw drawable keeps to the window's clip and its id test, whether it goes by
/// clip runs or, with the id test on, a pixel at a time.
#[test]
fn a_deep_image_follows_the_clip_and_the_id_test() {
    use crate::dev::odyssey::consts::{ODSY_ID_COMPARE_ENABLE, ODSY_ID_COMPARE_MASK_SHIFT};
    let (mut s, ram, rows) = window_12();
    let image: Vec<u8> = (0..16u16).flat_map(|x| [0x1000 + x, 0x2000, 0x3000, 0xffff]).flat_map(u16::to_be_bytes).collect();
    // The screen's depth plane shares these bytes, so what an untouched pixel holds is read first.
    let blank = rows.load_deep(8, 1);
    s.win.clip_rect[0].x1 = 7;
    dma_write_sized(&mut s, &ram, (0, 1, 16), 0x1000, 0x6b, 8, &image);
    assert_eq!(rows.load_deep(7, 1).map(|c| c[1]), Some(0x200), "inside the clip");
    assert_eq!(rows.load_deep(8, 1), blank, "outside it");
    // The id under the pixels is 0: a test for id 1 passes nothing, a test for id 0 passes the clip's pixels.
    s.win.id_compare = ODSY_ID_COMPARE_ENABLE | 1 | 0xff << ODSY_ID_COMPARE_MASK_SHIFT;
    dma_write_sized(&mut s, &ram, (0, 2, 16), 0x1000, 0x6b, 8, &image);
    assert_eq!(rows.load_deep(7, 2), blank, "the id test fails");
    s.win.id_compare = ODSY_ID_COMPARE_ENABLE | 0xff << ODSY_ID_COMPARE_MASK_SHIFT;
    dma_write_sized(&mut s, &ram, (0, 3, 16), 0x1000, 0x6b, 8, &image);
    assert_eq!((rows.load_deep(7, 3).map(|c| c[1]), rows.load_deep(8, 3)), (Some(0x200), blank));
}

/// A deep image, by the draw drawable and by the screen's address, marks the lines it stored and the picture
/// of the lines marked is the whole picture's.
#[test]
fn a_deep_image_marks_the_rows_it_stored() {
    let (mut s, ram, _) = window_12();
    let mut shown = Vec::new();
    follow(&mut s, &mut shown);
    let image: Vec<u8> = (0..16u16).flat_map(|x| [0x1000 + 16 * x, 0x2345, 0x3000, 0xffff]).flat_map(u16::to_be_bytes).collect();
    dma_write_sized(&mut s, &ram, (20, 40, 16), 0x1000, 0x6b, 8, &image);
    assert_eq!(follow(&mut s, &mut shown), 16, "the tile row of line 40");
    let w = s.test_reader().frame_width as usize;
    assert_same(&shown, &whole(&mut s), w, "a draw-drawable image");
    ram.put(PAGE_A, &image);
    put_table(&ram, TABLE, [0x2000, 100 << 16 | 20, 80, 1 << 16 | 16, 0x8000_0000 | 4 << 20 | 2 << 13 | 3 << 16 | 0x42, 0, 16 * 8, 0x3200_0000 | 2 << 16 | 0x6b], &[pte(PAGE_A, 16 * 8)]);
    descriptor(&mut s, TABLE);
    assert_eq!(follow(&mut s, &mut shown), 16, "the tile row of line 100");
    assert_same(&shown, &whole(&mut s), w, "an image by address");
}

/// The address form of an image write (the screen's own address, not the draw sentinel) takes the
/// same deep source into a window whose pixel mode is selected.
#[test]
fn a_deep_image_written_by_address_is_stored_whole() {
    let (mut s, ram, rows) = window_12();
    let mut image = Vec::new();
    for x in 0..16u32 {
        for v in [x * 257 & 0xffff, 0xffff - x * 31, x * 4096, 0xffff] {
            image.extend_from_slice(&(v as u16).to_be_bytes());
        }
    }
    ram.put(PAGE_A, &image);
    // Word 4: 8-byte field at unit 4 (the 12-bit window's front), 16-byte pixels.
    let word4 = 0x8000_0000 | 4 << 20 | 2 << 13 | 3 << 16 | 0x42;
    put_table(&ram, TABLE, [0x2000, 0, 80, 1 << 16 | 16, word4, 0, 16 * 8, 0x3200_0000 | 2 << 16 | 0x6b], &[pte(PAGE_A, 16 * 8)]);
    descriptor(&mut s, TABLE);
    let round = |v: u32| (v * 4095 + 32767) / 65535;
    for x in [0u32, 1, 7, 15] {
        let v = [x * 257 & 0xffff, 0xffff - x * 31, x * 4096, 0xffff].map(round);
        assert_eq!(rows.load_deep(x as i32, 0), Some(v), "pixel {x}");
    }
}

/// A 16-bit RGB image (three shorts, six bytes a pixel, no alpha) into a 12-bit window, by the draw drawable
/// and by the screen's address: the last pixel of a row is exactly six bytes, and alpha is full.
#[test]
fn a_16_bit_rgb_image_is_stored_with_full_alpha() {
    let image: Vec<u8> = (0..16u32).flat_map(|x| [x * 257, 0xffff - x * 31, x * 4096]).flat_map(|v| (v as u16).to_be_bytes()).collect();
    let round = |v: u32| (v * 4095 + 32767) / 65535;
    let bpp_code = crate::dev::odyssey::dma::DMA_BPP.iter().position(|&b| b == 6).unwrap() as u32;
    for by_address in [false, true] {
        let (mut s, ram, rows) = window_12();
        if by_address {
            ram.put(PAGE_A, &image);
            let word4 = 0x8000_0000 | 4 << 20 | 2 << 13 | 3 << 16 | 0x42;
            put_table(&ram, TABLE, [0x2000, 0, 80, 1 << 16 | 16, word4, 0, 16 * 6, 0x3200_0000 | bpp_code << 16 | 0x69], &[pte(PAGE_A, 16 * 6)]);
            descriptor(&mut s, TABLE);
        } else {
            dma_write_sized(&mut s, &ram, (0, 0, 16), 0x1000, 0x69, 6, &image);
        }
        for x in [0u32, 1, 7, 15] {
            let v = [x * 257, 0xffff - x * 31, x * 4096].map(round);
            assert_eq!(rows.load_deep(x as i32, 0), Some([v[0], v[1], v[2], 0xfff]), "pixel {x}, by address {by_address}");
        }
    }
}

/// A 2_10_10_10_REV image into a 12-bit window repeats each ten-bit component's top bits to twelve (an
/// approximation, odyssey.rst DMA), through the general decode.
#[test]
fn a_10_bit_rev_image_repeats_its_top_bits_in_a_12_bit_window() {
    let (mut s, ram, rows) = window_12();
    let image = bytes_of((0..16u32).map(|x| {
        let [r, g, b] = ten(x * 64 + 5);
        3 << 30 | b << 20 | g << 10 | r
    }));
    // RGBA, UNSIGNED_INT_2_10_10_10_REV: format 0xc, type 8.
    dma_write_sized(&mut s, &ram, (0, 0, 16), 0x1000, 0x10c, 4, &image);
    let widen = |v: u32| v << 2 | v >> 8;
    for x in [0u32, 3, 15] {
        let [r, g, b] = ten(x * 64 + 5);
        assert_eq!(rows.load_deep(x as i32, 0), Some([widen(r), widen(g), widen(b), 0xfff]), "pixel {x}");
    }
}

/// A read of a buffer by address returns the field its front bit names: the A field for front, B for back.
#[test]
fn a_deep_read_by_address_takes_the_front_or_the_back_field() {
    let (mut s, ram, _) = window_12();
    let bpp_code = crate::dev::odyssey::dma::DMA_BPP.iter().position(|&b| b == 8).unwrap() as u32;
    // Pixel 3 of field A (unit 4) is 0x1000 on every channel, of field B (unit 0) 0x3000.
    for (unit, v) in [(4u32, 0x1000u16), (0, 0x3000)] {
        let image: Vec<u8> = (0..16).flat_map(|_| [v, v, v, 0xffff]).flat_map(u16::to_be_bytes).collect();
        ram.put(PAGE_A, &image);
        let word4 = 0x8000_0000 | unit << 20 | 2 << 13 | 3 << 16 | 0x42;
        put_table(&ram, TABLE, [0x2000, 0, 80, 1 << 16 | 16, word4, 0, 16 * 8, 0x3200_0000 | bpp_code << 16 | 0x6b], &[pte(PAGE_A, 16 * 8)]);
        descriptor(&mut s, TABLE);
    }
    let read = |s: &mut Board, front: u32| {
        let word4 = (with_fields(read_word4(3, 9), 4, 0) & !0xe000 & !(1 << 31)) | 2 << 13 | front << 31;
        let got = dma_read_from(s, &ram, 0x2000, (3, 0, 1), word4, 8);
        u16::from_be_bytes([got[0], got[1]])
    };
    assert_eq!((read(&mut s, 1), read(&mut s, 0)), (0x1001, 0x3003), "front is the A field, back the B");
}

/// Twelve bits are a lossless carrier for what the fields hold: a deep field returns what it stores,
/// and its 8-bit word is the top eight bits of that.
#[test]
fn a_deep_field_returns_what_it_stores() {
    use crate::dev::odyssey::field::{Codec, Field, FIELD_PACKED};
    for field in [Field { at: 0, codec: Codec::Lanes }, Field { at: 0, codec: Codec::Texel { bytes: 4, field: FIELD_PACKED } }] {
        let bits = if field.codec == Codec::Lanes { 12 } else { 10 };
        for x in (0..4096u32).step_by(7).chain([4095]) {
            let c: Deep = [x, 4095 - x, (x * 5) & 4095, x ^ 0x5a5];
            let v = field.store_deep(c);
            let back = field.load_deep(v);
            if bits == 12 {
                assert_eq!(back, c);
            } else {
                // Ten bits: the top ten of each, widened; alpha two bits.
                let w10 = |c: u32| (c >> 2 << 2) | (c >> 10);
                assert_eq!([back[0], back[1], back[2]], [w10(c[0]), w10(c[1]), w10(c[2])]);
            }
            assert_eq!(field.load(v) >> 24, back[0] >> 4, "the 8-bit word is the top eight of the red");
        }
    }
    assert_eq!(deep8(255), 4095);
}

/// ms a frame of a `BENCH_W` x `BENCH_H` image DMA'd to a deep window: `BENCH_FORMAT` rgba16, rgb10a2 or
/// rgba8, `BENCH_PATH` address or sentinel, `BENCH_FRAMES` a run (0: build only), `BENCH_RUNS` runs.
#[test]
#[ignore = "benchmark"]
fn bench_deep_image_dma() {
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let text = |k: &str, d: &str| std::env::var(k).unwrap_or(d.into());
    let (w, h, runs, frames) = (env("BENCH_W", 720), env("BENCH_H", 486), env("BENCH_RUNS", 5), env("BENCH_FRAMES", 4));
    let (format, path) = (text("BENCH_FORMAT", "rgba16"), text("BENCH_PATH", "address"));
    let pitch = (w as u32).div_ceil(16).max(80);
    // (status, display mode, word 4's field part, bytes a pixel, source code)
    let (status, display, field, bpp, code) = match format.as_str() {
        "rgb10a2" => (STATUS_10, mode(2, 6, 2, 9), 6 << 20 | 1 << 13, 4usize, 0x10bu32),
        "rgba8" => (STATUS_12, mode(4, 4, 0, 6), 4 << 20 | 2 << 13, 4, 0x4b),
        _ => (STATUS_12, mode(4, 4, 0, 6), 4 << 20 | 2 << 13, 8, 0x6b),
    };
    let (mut s, ram, rows) = deep_window_of(status, display, pitch);
    s.win.clip_kept = 1;
    s.win.clip_rect[0] = crate::dev::odyssey::clip::ClipRect { x0: 0, y0: 0, x1: w as i32 - 1, y1: h as i32 - 1 };
    let bpp_code = crate::dev::odyssey::dma::DMA_BPP.iter().position(|&b| b as usize == bpp).unwrap() as u32;
    let rowbytes = w * bpp;
    let per_table = (12 * 0xffff / rowbytes).max(1);
    let base = 0x40_0000u64;
    let mut px = vec![0u8; rowbytes * h];
    let mut seed = 0x1234_5678u32;
    for p in px.chunks_mut(4) {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        p.copy_from_slice(&(seed | 0x0303_0303).to_be_bytes());
    }
    ram.put(base, &px);
    let frame = |s: &mut Board| {
        let mut y = 0;
        while y < h {
            let n = per_table.min(h - y);
            let (mut at, mut left) = (base + (y * rowbytes) as u64, n * rowbytes);
            let mut entries = Vec::new();
            while left > 0 {
                let m = left.min(0xffff);
                entries.push(pte(at, m as u32));
                at += m as u64;
                left -= m;
            }
            let header = if path == "sentinel" {
                [crate::dev::odyssey::consts::dma::SENTINEL_DRAW, (y as u32) << 16, 0x50, (n as u32) << 16 | w as u32, 0x1000, 0, rowbytes as u32, 1 << 30 | bpp_code << 16 | code]
            } else {
                [0x2000, (y as u32) << 16, pitch, (n as u32) << 16 | w as u32, 0x8000_0000 | 3 << 16 | field | 0x42, 0, rowbytes as u32, 0x3200_0000 | bpp_code << 16 | code]
            };
            put_table(&ram, TABLE, header, &entries);
            descriptor(s, TABLE);
            y += n;
        }
    };
    if frames > 0 {
        frame(&mut s);
        s.gpu_sync();
        assert_ne!(rows.load_raw(w as i32 - 1, h as i32 - 1), 0, "the far corner pixel was stored");
    }
    let mut ms = Vec::new();
    for _ in 0..runs {
        let t = std::time::Instant::now();
        for _ in 0..frames {
            frame(&mut s);
        }
        s.gpu_sync();
        ms.push(t.elapsed().as_secs_f64() * 1e3 / frames.max(1) as f64);
    }
    let min = ms.iter().cloned().fold(f64::MAX, f64::min);
    println!("vpro deep image dma {w}x{h} {format} by {path}: ms a frame {ms:.2?} ({:.2} ns a pixel at the min)", min * 1e6 / (w * h) as f64);
}
