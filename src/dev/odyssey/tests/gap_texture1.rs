// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-odyssey/src/dev/odyssey/tests/gap_texture1.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! Texture uploads and readbacks. The engine keeps a texture level in board
//! memory at the guest's address, not in an object pool.

use super::dma::{descriptor, pte, put_table, PAGE_A, TABLE};
use crate::dev::odyssey::board::{Board, ODSY_HEIGHT, ODSY_WIDTH};
use crate::dev::odyssey::clip::ClipRect;
use crate::dev::odyssey::consts::{self, dma, gl_cmd, host, image, reg, tex, ODSY_USER_CMD_FIFO};
use crate::dev::odyssey::field::{level_surface, Codec, Field};
use crate::dev::odyssey::pipe::slot::DrawState;
use crate::dev::odyssey::pipe::texture::Unit;
use crate::dev::odyssey::tests::{
    board_with_ram, cmd, cmd64, pixel, reg_write, route_to_heart, screen_field, select, Ram,
    Recorder, DMA_VECTOR,
};
use crate::dev::odyssey::view::{Rows, View, MAX_DIM};
use crate::dev::odyssey::vram::Surface;
use std::sync::Arc;

/// Texel (x, y) of the level whose descriptor words are (w0, w1), as the
/// model's 0xRRGGBBAA word at the precision the level's format keeps.
fn texel(s: &Board, w0: u32, w1: u32, x: u32, y: u32) -> u32 {
    s.gpu_sync();
    let (surface, field) = level_surface(w0, w1).expect("a level the library emits");
    Rows::new(s.vram_ptr(), View::colour(&surface, field, MAX_DIM))
        .load_word(x as i32, y as i32)
        .unwrap()
}

/// Pixel (x, y) of the surface at `base` (`pitch` tiles a row, 1 << `log2`
/// bytes a pixel) read with `codec`, as the model's word.
fn mem_word(s: &Board, base: u32, pitch: u32, log2: u8, codec: Codec, x: u32, y: u32) -> u32 {
    s.gpu_sync();
    let surface = Surface::new(base, pitch, log2);
    Rows::new(
        s.vram_ptr(),
        View::colour(&surface, Field { at: 0, codec }, MAX_DIM),
    )
    .load_word(x as i32, y as i32)
    .unwrap()
}

/// A texel of an 8-byte-pixel object, whose field holds 12-bit lanes.
fn texel_lanes(s: &Board, base: u32, pitch: u32, x: u32, y: u32) -> u32 {
    mem_word(s, base, pitch, 3, Codec::Lanes, x, y)
}

/// The texel as the sampler reads it from a 2D level of `size` texels (a
/// power of two each way): 12-bit components, clamp to edge, nearest.
fn sampled(s: &mut Board, base: u32, word1: u32, size: (u32, u32), x: i64, y: i64) -> [u32; 4] {
    s.gpu_sync();
    let t = s.tex_mut();
    t.path = tex::MODE_2D;
    t.level0 = 0;
    t.levels[..3].copy_from_slice(&[base, word1, 0]);
    let (wl, hl) = (size.0.trailing_zeros(), size.1.trailing_zeros());
    t.sampler = [
        0x0800_0000 | wl << 10 | hl << 5,
        0x0004_0000 | size.0,
        0x0004_0000 | size.1,
        1,
        0x1300_1400,
        0,
        0,
    ];
    t.env = [1, 0, 0, 0, 0];
    t.lod = [0; 4];
    let st = s.issue_state();
    let mem = s.vram.ptr();
    // SAFETY: issued above and released below.
    let d = unsafe { &*st };
    let r = Unit {
        regs: &d.tex,
        latch: &d.latch,
        mem,
    }
    .fetch(0, x, y, 0);
    unsafe { DrawState::release(st) };
    r
}

/// A packed RRGGBBAA texel as the unit's 12-bit components.
fn c12(w: u32) -> [u32; 4] {
    [24, 16, 8, 0].map(|s| {
        let v = (w >> s) & 0xff;
        (v << 4) | (v >> 4)
    })
}

/// A board whose DMA-done interrupt is routed, so a transfer that asks for
/// it, or does not, shows.
fn routed_board() -> (Box<Board>, Arc<Recorder>, Arc<Ram>) {
    let (mut s, rec, ram) = board_with_ram();
    route_to_heart(&mut s);
    (s, rec, ram)
}

/// The destination select the library sends before it uploads to a level:
/// the level's base, its row of tiles and the pixel mode that names its storage.
fn select_level(s: &mut Board, base: u32, pitch: u32, mode: u32) {
    for w in [select(reg::DEST_SELECT), base, pitch, mode, 0] {
        cmd(s, w);
    }
}

/// One DMA upload: the header, with the source bytes in one entry.
fn upload(s: &mut Board, ram: &Ram, header: [u32; 8], source: &[u8]) {
    ram.put(PAGE_A, source);
    put_table(ram, TABLE, header, &[pte(PAGE_A, source.len() as u32)]);
    descriptor(s, TABLE);
}

/// The RGBA8 level the library selects and describes (index 17, 4-byte texels).
const RGBA8_MODE: u32 = 0x0800_24d0;
const RGBA8_LEVEL: u32 = 0x4110_0000;

/// A DMA upload lands where its header says; bit 31 of word 7 asks for DMA-done.
///
/// QEMU: sn1/odyssey/dma-texture-object
#[test]
fn a_dma_upload_fills_the_level_its_header_names_without_an_interrupt() {
    let (mut s, rec, ram) = routed_board();
    let base = 0x16a800;
    let mut rows = vec![];
    for y in 0..2u32 {
        for x in 0..256u32 {
            rows.extend_from_slice(&(x << 24 | y << 16 | (x ^ y) << 8 | 0xff).to_be_bytes());
        }
    }

    select_level(&mut s, base, 4, RGBA8_MODE);
    upload(
        &mut s,
        &ram,
        [
            base,
            0,
            0x10,
            2 << 16 | 0x100,
            0x8001_3000,
            0,
            0x400,
            0x3201_004b,
        ],
        &rows,
    );

    let level = RGBA8_LEVEL | 0x10;
    assert_eq!(texel(&s, base, level, 0, 0), 0x0000_00ff);
    assert_eq!(texel(&s, base, level, 129, 0), 0x8100_81ff);
    assert_eq!(texel(&s, base, level, 84, 1), 0x5401_55ff);
    assert_eq!(texel(&s, base, level, 0, 2), 0);
    assert_eq!(
        sampled(&mut s, base, level, (256, 256), 129, 0),
        c12(0x8100_81ff)
    );
    assert_eq!((pixel(&s, 0, 0, 0), pixel(&s, 0, 129, 0)), (0, 0));
    assert!(rec.events(DMA_VECTOR).is_empty());
}

/// The library's format and type pairs, in the host pixel sizes of word 7, land as one RGBA8 texel.
///
/// QEMU: sn1/odyssey/dma-texture-gl-types
#[test]
fn a_dma_upload_decodes_the_library_format_and_type_pairs() {
    let (mut s, _, ram) = routed_board();
    let cases: [(u32, u32, u32, &[u8], u32); 5] = [
        (
            0xb,
            4,
            3,
            &[
                0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22, 0x33, 0x33, 0x33, 0x33, 0x44, 0x44,
                0x44, 0x44,
            ],
            0x1122_3344,
        ),
        (4, 2, 5, &[0x80], 0x8000_00ff),
        (9, 0xa, 0, &[0xf8, 0x10], 0xff00_84ff),
        (0xb, 8, 1, &[0xff, 0xc0, 0x08, 0x02], 0xff00_80aa),
        (0x11, 2, 1, &[0x33, 0x22, 0x11, 0x44], 0x1122_3344),
    ];
    for (i, (format, ty, size, source, want)) in cases.into_iter().enumerate() {
        let base = 0x19_0000 + i as u32 * 0x1000;
        select_level(&mut s, base, 2, RGBA8_MODE);
        upload(
            &mut s,
            &ram,
            [
                base,
                0,
                2,
                1 << 16 | 1,
                0x8001_5000,
                0,
                source.len() as u32,
                0x3a00_0000 | size << 16 | ty << 5 | format,
            ],
            source,
        );
        assert_eq!(
            texel(&s, base, RGBA8_LEVEL | 2, 0, 0),
            want,
            "format {format:#x} type {ty:#x}"
        );
    }
}

/// Each (L, A) byte pair is one texel, with L in red, green and blue.
///
/// QEMU: sn1/odyssey/dma-texture-luminance-alpha
#[test]
fn a_luminance_alpha_dma_upload_lands_as_one_texel_per_pair() {
    let (mut s, _, ram) = routed_board();
    let base = 0x16c800;
    let mut rows = vec![];
    for y in 0..2u32 {
        for x in 0..256u32 {
            rows.extend_from_slice(&[x as u8, (x ^ if y == 1 { 0xff } else { 0x0f }) as u8]);
        }
    }

    select_level(&mut s, base, 0x10, 0x0800_0030);
    upload(
        &mut s,
        &ram,
        [
            base,
            0,
            0x10,
            2 << 16 | 0x100,
            0x8000_1000,
            0,
            0x200,
            0x3200_0048,
        ],
        &rows,
    );

    let level = 0x00b0_0010;
    assert_eq!(texel(&s, base, level, 0, 0), 0x0000_000f);
    assert_eq!(texel(&s, base, level, 0x81, 0), 0x8181_818e);
    assert_eq!(texel(&s, base, level, 0xff, 1), 0xffff_ff00);
    assert_eq!(texel(&s, base, level, 0, 2), 0);
    assert_eq!(pixel(&s, 0, 0, 0), 0);
}

/// One-row, two-texel DMA upload of a 12-bit source into the RGBA8 level at `base`.
fn texel12_row(
    s: &mut Board,
    ram: &Ram,
    base: u32,
    code: u32,
    size_index: u32,
    bpp: u32,
    bytes: &[u8],
) -> [u32; 2] {
    select_level(s, base, 2, RGBA8_MODE);
    upload(
        s,
        ram,
        [
            base,
            0,
            2,
            1 << 16 | 2,
            0x8001_5000,
            0,
            2 * bpp,
            0x3a00_0000 | size_index << 16 | code,
        ],
        bytes,
    );
    [
        texel(s, base, RGBA8_LEVEL | 2, 0, 0),
        texel(s, base, RGBA8_LEVEL | 2, 1, 0),
    ]
}

/// The code gives the components' order; the size index gives byte or 16-bit each.
///
/// A 12-bit source case: base, code, size index, bytes a pixel, host bytes, texels wanted.
type Texel12Case = (u32, u32, u32, u32, &'static [u8], [u32; 2]);

/// QEMU: sn1/odyssey/dma-texture-12bit-sources
#[test]
fn a_12_bit_texture_source_decodes_by_its_code_and_component_size() {
    let (mut s, _, ram) = board_with_ram();
    let cases: [Texel12Case; 6] = [
        (
            0x180000,
            0x6c,
            2,
            8,
            &[
                0x11, 0x22, 0x55, 0x66, 0x99, 0xaa, 0xdd, 0xee, 0xff, 0xff, 0x22, 0x02, 0x3e, 0xb4,
                0x46, 0xfd,
            ],
            [0xdd99_5511, 0x473e_22ff],
        ),
        (
            0x181000,
            0x6b,
            2,
            8,
            &[
                0x11, 0x22, 0x55, 0x66, 0x99, 0xaa, 0xdd, 0xee, 0x00, 0x00, 0xff, 0xff, 0x80, 0x00,
                0x00, 0x7f,
            ],
            [0x1155_99dd, 0x00ff_8000],
        ),
        (
            0x182000,
            0x69,
            6,
            6,
            &[
                0x11, 0x22, 0x55, 0x66, 0x99, 0xaa, 0xdf, 0xdf, 0xa0, 0xa0, 0x00, 0x00,
            ],
            [0x1155_99ff, 0xdfa0_00ff],
        ),
        (
            0x183000,
            0x69,
            7,
            3,
            &[0x11, 0x55, 0x99, 0xd4, 0xa0, 0x00],
            [0x1155_99ff, 0xd4a0_00ff],
        ),
        (
            0x184000,
            0x68,
            1,
            4,
            &[0x11, 0x22, 0x55, 0x66, 0xd3, 0xd3, 0x7a, 0x7a],
            [0x1111_1155, 0xd3d3_d37a],
        ),
        (
            0x185000,
            0x62,
            0,
            2,
            &[0xa6, 0x8c, 0x87, 0x0d],
            [0xa6a6_a6ff, 0x8787_87ff],
        ),
    ];

    for (base, code, size_index, bpp, bytes, want) in cases {
        assert_eq!(
            texel12_row(&mut s, &ram, base, code, size_index, bpp, bytes),
            want,
            "code {code:#x} at {base:#x}"
        );
    }
    assert_eq!(pixel(&s, 0, 0, 0), 0);
}

/// Guest RAM that remembers each read as (address, length).
struct Spy {
    ram: Arc<Ram>,
    reads: parking_lot::Mutex<Vec<(u64, usize)>>,
}

impl crate::traits::BusDevice for Spy {
    fn read8(&self, addr: u64) -> crate::traits::BusRead8 {
        self.reads.lock().push((addr, 1));
        self.ram.read8(addr)
    }
    fn read32(&self, addr: u64) -> crate::traits::BusRead32 {
        self.reads.lock().push((addr, 4));
        self.ram.read32(addr)
    }
    fn read64(&self, addr: u64) -> crate::traits::BusRead64 {
        self.reads.lock().push((addr, 8));
        self.ram.read64(addr)
    }
    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
        self.reads.lock().push((addr, 8 * buf.len()));
        self.ram.read_block(addr, buf)
    }
    fn write8(&self, addr: u64, val: u8) -> u32 {
        self.ram.write8(addr, val)
    }
    fn write16(&self, addr: u64, val: u16) -> u32 {
        self.ram.write16(addr, val)
    }
    fn write32(&self, addr: u64, val: u32) -> u32 {
        self.ram.write32(addr, val)
    }
    fn write64(&self, addr: u64, val: u64) -> u32 {
        self.ram.write64(addr, val)
    }
}

/// The board reads only the table and the source, in address order and page runs.
///
/// QEMU: sn1/odyssey/command-fifo-dma-trace
#[test]
fn a_dma_upload_reads_only_its_table_and_source_in_page_runs() {
    let (mut s, _, ram) = board_with_ram();
    let spy = Arc::new(Spy {
        ram: ram.clone(),
        reads: Default::default(),
    });
    s.guest = Some(spy.clone());
    let (base, source) = (0x16a800, 0xb84000u64);
    let pixels: Vec<u8> = (0..4224).map(|i| i as u8).collect();

    select_level(&mut s, base, 66, RGBA8_MODE);
    upload(
        &mut s,
        &ram,
        [
            base,
            0,
            66,
            1 << 16 | 1056,
            0x8001_3000,
            0,
            4224,
            1 << 16 | 0x4b,
        ],
        &pixels,
    );

    let level = RGBA8_LEVEL | 66;
    assert_eq!(texel(&s, base, level, 0, 0), 0x0001_0203);
    assert_eq!(texel(&s, base, level, 1, 0), 0x0405_0607);
    assert_eq!(texel(&s, base, level, 1055, 0), 0x7c7d_7e7f);
    let reads = spy.reads.lock();
    let table = TABLE..TABLE + dma::TABLE_HEADER + 16;
    let mut next = source;
    for &(addr, len) in reads.iter() {
        let end = addr + len as u64;
        if addr >= source && addr < source + 4224 {
            assert_eq!(addr, next, "the source is read in address order, once");
            next = end;
        } else {
            assert!(
                table.contains(&addr) && end <= table.end,
                "read of {len} bytes at {addr:#x} is outside the table and source"
            );
        }
        assert_eq!(
            addr >> 12,
            (end - 1) >> 12,
            "an access of {len} bytes at {addr:#x} crosses a page"
        );
    }
    assert_eq!(next, source + 4224, "the whole source is read");
}

/// One word through the user command port, as the GL library's own stores
/// reach the board.
fn user(s: &mut Board, w: u32) {
    s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
}

fn user_words(s: &mut Board, words: &[u32]) {
    for &w in words {
        user(s, w);
    }
}

fn coord(v: u32) -> u32 {
    (v as f32).to_bits()
}

/// The upload prologue: the window goes to (0, 0), then the level is selected.
fn upload_begin(
    s: &mut Board,
    base: u32,
    pitch: u32,
    mode: u32,
    (x, y): (u32, u32),
    (w, h): (u32, u32),
) {
    let c = consts::cmd::IMAGE_BOX;
    user_words(
        s,
        &[
            0x0001_1453,
            1,
            0,
            0,
            0,
            0,
            select(reg::PATTERN_MODE),
            0,
            0x0001_0405,
            0x2400,
            select(reg::WINDOW_ORIGIN),
            0,
            0,
            select(reg::WINDOW_CLIP),
            0,
            0x00ff_ffff,
            0,
            0,
            0,
            0,
            0,
            0,
            select(reg::DEST_SELECT),
            base,
            pitch,
            mode,
            0,
            select(reg::WRITE_MASK),
            0x01ff_ffff,
            select(reg::CLIP_BOX),
            x << 12 | (x + w),
            y << 12 | (y + h),
            0x2900_812f,
            select(reg::GL_BEGIN),
            gl_cmd::PRIMITIVE_IMAGE,
            c,
            0x3f80_0000,
            0x3f80_0000,
            0x3f80_0000,
            0x3f80_0000,
            coord(x),
            coord(y),
            0,
            coord(x + w),
            coord(y + h),
            0,
            gl_cmd::VERTEX_2I,
            0,
            0,
        ],
    );
}

/// The end of an upload, and the screen select the next draw sends.
fn upload_end(s: &mut Board) {
    user_words(
        s,
        &[
            select(reg::GL_END),
            consts::cmd::WINDOW_RESTORE,
            select(reg::DEST_SELECT),
            0x2000,
            0x50,
            0x2000_6850,
            0,
        ],
    );
}

/// The burst alone: its tag with the word count, then the words.
fn upload_burst(s: &mut Board, fmt: u32, words: &[u32]) {
    let n = words.len() as u32;
    user(
        s,
        select(image::BURST_BASE | (n - n / 16) << consts::cmd::ARITY_SHIFT | fmt),
    );
    user_words(s, words);
}

/// The screen corner nothing drew on.
fn screen_untouched(s: &Board) {
    for (x, y) in [(0, 0), (7, 7), (256, 256)] {
        assert_eq!(pixel(s, 0, x, y), 0, "screen ({x}, {y})");
    }
}

/// Luminance lands in red, green and blue. Alpha is not measured, so not asserted.
///
/// QEMU: sn1/odyssey/command-fifo-gl-texture-luminance
#[test]
fn a_luminance_texture_upload_by_burst_fills_the_level() {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16b000;

    upload_begin(&mut s, base, 2, 0x0800_0020, (0, 0), (8, 8));
    user_words(
        &mut s,
        &[
            consts::cmd::IMAGE_HEADER,
            0x0f00_1042,
            consts::cmd::IMAGE_HEADER_TAIL,
        ],
    );
    for y in 0..8u32 {
        let row: [u32; 2] = std::array::from_fn(|x| {
            let l = 4 * (4 * x as u32 + 8 * y);
            l << 24 | (l + 4) << 16 | (l + 8) << 8 | (l + 12)
        });
        if y > 0 {
            user_words(
                &mut s,
                &[
                    consts::cmd::IMAGE_HEADER,
                    0x0f81_0042,
                    consts::cmd::IMAGE_HEADER_TAIL,
                ],
            );
        }
        upload_burst(&mut s, image::FMT_CI, &row);
    }
    upload_end(&mut s);

    for y in (0..8).step_by(7) {
        for x in (0..8).step_by(3) {
            let l = 4 * (x + 8 * y);
            assert_eq!(
                texel(&s, base, 0x0040_0002, x, y) >> 8,
                l << 16 | l << 8 | l,
                "texel ({x}, {y})"
            );
        }
    }
    assert_eq!(texel(&s, base, 0x0040_0002, 3, 3) >> 8, 0x6c_6c6c);
    screen_untouched(&s);
}

/// An 8x8 GL_RGB texture by burst, three packed bytes a texel, into an
/// RGBA8 level: alpha 255.
///
/// QEMU: sn1/odyssey/command-fifo-gl-texture-rgb
#[test]
fn an_rgb_texture_upload_by_burst_stores_alpha_255() {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16ad00;

    upload_begin(&mut s, base, 2, RGBA8_MODE, (0, 0), (8, 8));
    user_words(
        &mut s,
        &[
            consts::cmd::IMAGE_HEADER,
            0x0f00_1049,
            consts::cmd::IMAGE_HEADER_TAIL,
        ],
    );
    for y in 0..8u32 {
        let bytes: Vec<u8> = (0..8u32)
            .flat_map(|x| [(32 * x) as u8, (32 * y) as u8, 128])
            .collect();
        let row: Vec<u32> = bytes
            .chunks(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
            .collect();
        if y > 0 {
            user_words(
                &mut s,
                &[
                    consts::cmd::IMAGE_HEADER,
                    0x0f81_0049,
                    consts::cmd::IMAGE_HEADER_TAIL,
                ],
            );
        }
        upload_burst(&mut s, image::FMT_RGB, &row);
    }
    upload_end(&mut s);

    let level = RGBA8_LEVEL | 2;
    assert_eq!(texel(&s, base, level, 0, 0), 0x0000_80ff);
    assert_eq!(texel(&s, base, level, 4, 3), 0x8060_80ff);
    assert_eq!(texel(&s, base, level, 7, 7), 0xe0e0_80ff);
    assert_eq!(texel(&s, base, level, 8, 0), 0);
    screen_untouched(&s);
}

/// The 64x64 RGBA texel: (4 x, 4 y, the 8x8-cell checker, 255).
fn tex_rgba(x: u32, y: u32) -> u32 {
    let b = if (x / 8 + y / 8) & 1 != 0 { 0xff } else { 0 };
    (4 * x) << 24 | (4 * y) << 16 | b << 8 | 0xff
}

/// A 64x64 GL_RGBA texture by 256 sixteen-word bursts: the level holds the
/// words as sent, the unit samples them and the screen is untouched.
///
/// QEMU: sn1/odyssey/command-fifo-gl-texture-rgba
#[test]
fn an_rgba_texture_upload_by_burst_holds_the_words_as_sent() {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16a800;

    upload_begin(&mut s, base, 4, RGBA8_MODE, (0, 0), (64, 64));
    user_words(
        &mut s,
        &[
            consts::cmd::IMAGE_HEADER,
            0x0f00_104b,
            consts::cmd::IMAGE_HEADER_TAIL,
        ],
    );
    for i in (0..64 * 64).step_by(16) {
        let words: Vec<u32> = (i..i + 16).map(|j| tex_rgba(j % 64, j / 64)).collect();
        upload_burst(&mut s, image::FMT_RGB, &words);
    }
    upload_end(&mut s);

    let level = RGBA8_LEVEL | 4;
    for (x, y, want) in [
        (0, 0, 0x0000_00ffu32),
        (8, 0, 0x2000_ffff),
        (32, 31, tex_rgba(32, 31)),
        (63, 63, tex_rgba(63, 63)),
    ] {
        assert_eq!(texel(&s, base, level, x, y), want, "texel ({x}, {y})");
        assert_eq!(
            sampled(&mut s, base, level, (64, 64), x as i64, y as i64),
            c12(want),
            "sampled ({x}, {y})"
        );
    }
    screen_untouched(&s);
}

/// A board with guest RAM whose one clip rectangle covers the screen.
fn window_board() -> (Box<Board>, Arc<Ram>) {
    let (mut s, _, ram) = board_with_ram();
    s.win.clip_kept = 1;
    s.win.clip_rect[0] = ClipRect {
        x0: 0,
        y0: 0,
        x1: ODSY_WIDTH as i32 - 1,
        y1: ODSY_HEIGHT as i32 - 1,
    };
    (s, ram)
}

/// A GL image write by pixel burst to the current destination.
fn image(
    s: &mut Board,
    (x1, y1, x2, y2): (u32, u32, u32, u32),
    group: u32,
    code: u32,
    fmt: u32,
    words: &[u32],
) {
    for w in [
        0x2900_812f,
        select(reg::GL_BEGIN),
        gl_cmd::PRIMITIVE_IMAGE,
        consts::cmd::IMAGE_BOX,
        0,
        0,
        0,
        0,
        coord(x1),
        coord(y1),
        0,
        coord(x2),
        coord(y2),
        0,
        gl_cmd::VERTEX_2I,
        0,
        0,
    ] {
        cmd(s, w);
    }
    for w in [
        consts::cmd::IMAGE_HEADER,
        group << 20 | code,
        consts::cmd::IMAGE_HEADER_TAIL,
    ] {
        cmd(s, w);
    }
    let n = words.len() as u32;
    cmd(
        s,
        select(image::BURST_BASE | (n - n / 16) << consts::cmd::ARITY_SHIFT | fmt),
    );
    for &w in words {
        cmd(s, w);
    }
    cmd(s, select(reg::GL_END));
}

/// Code 0x4c keeps its A byte in a texture level. The screen's A is not measured.
///
/// QEMU: sn1/odyssey/command-fifo-image-alpha-texture
#[test]
fn an_abgr_image_into_a_texture_object_keeps_its_alpha() {
    let (mut s, ram) = window_board();

    select_level(&mut s, 0x16c000, 2, RGBA8_MODE);
    image(
        &mut s,
        (2, 3, 4, 4),
        2,
        0x4c,
        image::FMT_RGB,
        &[0x226c_c711, 0x0033_2244],
    );
    assert_eq!(texel(&s, 0x16c000, RGBA8_LEVEL | 2, 2, 3), 0x11c7_6c22);
    assert_eq!(texel(&s, 0x16c000, RGBA8_LEVEL | 2, 3, 3), 0x4422_3300);

    let source = [0x22, 0x6c, 0xc7, 0x11, 0x00, 0x33, 0x22, 0x44];
    upload(
        &mut s,
        &ram,
        [
            0x16a800,
            0,
            0x10,
            1 << 16 | 0x100,
            0x8001_3000,
            0,
            0x400,
            0x3201_004c,
        ],
        &source.repeat(128),
    );
    assert_eq!(texel(&s, 0x16a800, RGBA8_LEVEL | 0x10, 0, 0), 0x11c7_6c22);
    assert_eq!(texel(&s, 0x16a800, RGBA8_LEVEL | 0x10, 1, 0), 0x4422_3300);
}

/// Each luminance code lands under the burst tag the library sends it with.
/// Unsent pairings and the luminance level's alpha byte are not measured.
///
/// QEMU: sn1/odyssey/command-fifo-image-luminance-tags
#[test]
fn luminance_codes_land_under_the_burst_tags_the_library_sends() {
    let (mut s, _) = window_board();

    image(
        &mut s,
        (100, 100, 102, 101),
        0,
        0x62,
        image::FMT_RGBA16,
        &[0x2020_c0c0],
    );
    assert_eq!(
        (pixel(&s, 0, 100, 100), pixel(&s, 0, 101, 100)),
        (0x2020_20ff, 0xc0c0_c0ff)
    );
    image(
        &mut s,
        (200, 200, 202, 201),
        0,
        0x68,
        image::FMT_RGBA16,
        &[0x3030_7070, 0xa0a0_1010],
    );
    assert_eq!(
        (pixel(&s, 0, 200, 200), pixel(&s, 0, 201, 200)),
        (0x3030_3070, 0xa0a0_a010)
    );
    image(
        &mut s,
        (300, 300, 304, 301),
        4,
        0x48,
        image::FMT_RGB,
        &[0x1020_a0b0, 0x4050_6070],
    );
    for (x, want) in [
        (300, 0x1010_1020),
        (301, 0xa0a0_a0b0),
        (302, 0x4040_4050),
        (303, 0x6060_6070),
    ] {
        assert_eq!(pixel(&s, 0, x, 300), want, "x {x}");
    }

    // The luminance level's alpha byte is not asserted.
    select_level(&mut s, 0x16c000, 2, 0x0800_0020);
    image(&mut s, (0, 0, 2, 1), 2, 0x62, image::FMT_CI, &[0x2020_c0c0]);
    assert_eq!(texel(&s, 0x16c000, 0x0040_0002, 0, 0) >> 8, 0x20_2020);
    assert_eq!(texel(&s, 0x16c000, 0x0040_0002, 1, 0) >> 8, 0xc0_c0c0);
    select_level(&mut s, 0x16d000, 2, 0x0800_0030);
    image(&mut s, (0, 4, 2, 5), 2, 0x48, image::FMT_CI, &[0x1020_a0b0]);
    assert_eq!(texel(&s, 0x16d000, 0x00b0_0002, 0, 4), 0x1010_1020);
    assert_eq!(texel(&s, 0x16d000, 0x00b0_0002, 1, 4), 0xa0a0_a0b0);

    // Code 0x68 under tag 0x10 keeps its alpha in a luminance-alpha level.
    image(
        &mut s,
        (0, 2, 2, 3),
        2,
        0x68,
        image::FMT_CI,
        &[0x3030_7070, 0xa0a0_1010],
    );
    assert_eq!(texel(&s, 0x16d000, 0x00b0_0002, 0, 2), 0x3030_3070);
    assert_eq!(texel(&s, 0x16d000, 0x00b0_0002, 1, 2), 0xa0a0_a010);
}

/// The X server's 24-bit pixel for colour 0xRRGGBB: red mask 0xff and blue
/// mask 0xff0000, so the word 0x00BBGGRR, which code 0x4c carries as A, B, G, R.
fn x_pixel(rgb: u32) -> u32 {
    (rgb & 0xff) << 16 | (rgb & 0xff00) | (rgb >> 16) & 0xff
}

fn window_origin(s: &mut Board, x: u32, y: u32) {
    for w in [select(reg::WINDOW_ORIGIN), x, y] {
        cmd(s, w);
    }
}

/// Up to four inclusive rectangles (x0, y0, x1, y1); the rest empty.
fn window_clip(s: &mut Board, rects: &[(u32, u32, u32, u32)]) {
    cmd(s, select(reg::WINDOW_CLIP));
    for i in 0..4 {
        match rects.get(i) {
            Some(&(x0, y0, x1, y1)) => cmd64(s, x0 << 12 | y0, x1 << 12 | y1),
            None => cmd64(s, 0x1001, 0),
        }
    }
}

/// An image to the screen adds the signed window origin; a level's box is not offset.
/// Screen alpha for code 0x4c is not measured.
///
/// QEMU: sn1/odyssey/command-fifo-image-raster-window-origin
#[test]
fn an_image_adds_the_window_origin_on_the_screen_but_not_in_a_level() {
    let (mut s, _) = window_board();
    let rgb = |s: &Board, x, y| pixel(s, 0, x, y) >> 8;

    window_origin(&mut s, 8, 6);
    image(
        &mut s,
        (2, 3, 4, 4),
        2,
        0x4c,
        image::FMT_RGB,
        &[x_pixel(0x112233), x_pixel(0x445566)],
    );
    assert_eq!(
        (rgb(&s, 10, 9), rgb(&s, 11, 9), rgb(&s, 2, 3)),
        (0x11_2233, 0x44_5566, 0)
    );

    window_origin(&mut s, 0x1ffe, 0x1ffe);
    window_clip(&mut s, &[(0, 0, 1279, 1023)]);
    image(
        &mut s,
        (1, 1, 3, 3),
        4,
        0x4c,
        image::FMT_RGB,
        &[
            x_pixel(0xff0000),
            x_pixel(0x00ff00),
            x_pixel(0x0000ff),
            x_pixel(0xffffff),
        ],
    );
    assert_eq!(
        (rgb(&s, 0, 0), rgb(&s, 0, 1), rgb(&s, 1, 0)),
        (0xff_ffff, 0, 0)
    );

    window_origin(&mut s, 100, 200);
    window_clip(&mut s, &[(102, 201, 102, 201)]);
    image(
        &mut s,
        (1, 1, 3, 3),
        4,
        0x4c,
        image::FMT_RGB,
        &[
            x_pixel(0xff0000),
            x_pixel(0x00ff00),
            x_pixel(0x0000ff),
            x_pixel(0xffffff),
        ],
    );
    assert_eq!(
        (rgb(&s, 102, 201), rgb(&s, 101, 201), rgb(&s, 102, 202)),
        (0x00_ff00, 0, 0)
    );

    window_origin(&mut s, 8, 9);
    select_level(&mut s, 0x16c000, 2, RGBA8_MODE);
    image(
        &mut s,
        (2, 3, 3, 4),
        1,
        0x4c,
        image::FMT_RGB,
        &[0x5a00_0000 | x_pixel(0xabcdef)],
    );
    assert_eq!(texel(&s, 0x16c000, RGBA8_LEVEL | 2, 2, 3), 0xabcd_ef5a);
    assert_eq!(texel(&s, 0x16c000, RGBA8_LEVEL | 2, 10, 12), 0);
    assert_eq!(pixel(&s, 0, 10, 12), 0);
}

const FENCE: u32 = 0xb70000;
const GUARD: u8 = 0xa5;

/// Readback probe setup: fence at FENCE, interrupts routed, window origin (512, 256).
fn readback_board() -> (Box<Board>, Arc<Recorder>, Arc<Ram>) {
    let (mut s, rec, ram) = board_with_ram();
    reg_write(&mut s, host::WRITE_ADDR_UPPER, 0);
    reg_write(&mut s, host::WRITE_ADDR_LOWER, FENCE);
    route_to_heart(&mut s);
    select_level(&mut s, 0x2000, 0x50, 0x2000_6950);
    window_origin(&mut s, 512, 256);
    (s, rec, ram)
}

/// Run one transfer: the table with `entries`, then the descriptor.
fn transfer(s: &mut Board, ram: &Ram, header: [u32; 8], entries: &[u64]) {
    put_table(ram, TABLE, header, entries);
    descriptor(s, TABLE);
}

/// A read returns the header's rectangle with no window origin; alpha is 0 for code 0x4c.
///
/// QEMU: sn1/odyssey/readback/object-source
#[test]
fn a_read_of_an_object_returns_its_rectangle_with_a_zero_alpha() {
    let (mut s, rec, ram) = readback_board();
    let mut header = [
        0x16a000,
        1 << 16 | 5,
        0x10,
        2 << 16 | 3,
        0x8002_5000,
        0,
        12,
        0x3201_004c,
    ];
    let entry = [pte(PAGE_A + 16, 24)];
    let px: Vec<u8> = (0..6u32)
        .flat_map(|i| (0x5a00_0000 | x_pixel(0x10_2030 + i * 0x01_0101)).to_be_bytes())
        .collect();
    ram.put(PAGE_A + 16, &px);

    transfer(&mut s, &ram, header, &entry);
    assert_eq!(texel_lanes(&s, 0x16a000, 0x10, 5, 1), 0x1020_305a);
    assert!(rec.events(DMA_VECTOR).is_empty());

    ram.put(PAGE_A, &[GUARD; 56]);
    header[7] = 0xb301_004c;
    transfer(&mut s, &ram, header, &entry);
    let mut expect = [GUARD; 56];
    expect[16..40].copy_from_slice(&px);
    for i in 0..6 {
        expect[16 + 4 * i] = 0;
    }
    assert_eq!(ram.get(PAGE_A, 56), expect);
    assert_eq!(rec.events(DMA_VECTOR), [true]);
}

/// A GL read of one RGBA8 texel (class 2, code 8, no format byte) returns
/// the uploaded bytes and raises DMA-done.
///
/// QEMU: sn1/odyssey/readback/gl-rgba8-object
#[test]
fn a_gl_read_of_an_rgba8_texel_returns_the_uploaded_bytes() {
    let (mut s, rec, ram) = readback_board();
    let mut header = [0x16bc00, 0, 0x20, 0x10001, 0x8001_3042, 0, 4, 0x3201_004b];
    let entry = [pte(PAGE_A + 16, 4)];
    ram.put(PAGE_A + 16, &[0x11, 0x22, 0x33, 0x44]);

    transfer(&mut s, &ram, header, &entry);
    assert_eq!(texel(&s, 0x16bc00, RGBA8_LEVEL | 0x20, 0, 0), 0x1122_3344);

    ram.put(PAGE_A, &[GUARD; 36]);
    header[7] = 0xb301_0000;
    transfer(&mut s, &ram, header, &entry);
    let mut expect = [GUARD; 36];
    expect[16..20].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    assert_eq!(ram.get(PAGE_A, 36), expect);
    assert_eq!(rec.events(DMA_VECTOR), [true]);
}

/// Store `word` in colour buffer `buffer` (0 front field A, 1 field B) of
/// the screen at (x, y).
fn buffer_write(s: &Board, buffer: u32, x: i32, y: i32, word: u32) {
    s.gpu_sync();
    screen_field(s, buffer).store_word(x, y, word);
}

/// Block 0x2031, the copy, with its eight operands.
fn copy_block(s: &mut Board, ops: [u32; 8]) {
    cmd(s, consts::cmd::COPY_RECT_TRIGGER);
    for w in ops {
        cmd(s, w);
    }
}

/// A copy from either drawable reads back as 16-bit channels.
/// The level is selected with 12-bit lanes (status 0x08004850).
///
/// QEMU: sn1/odyssey/readback/gl-rgba16-scratch-copy
#[test]
fn a_gl_copy_of_the_front_or_back_drawable_reads_back_as_16_bit_channels() {
    let (mut s, _, ram) = readback_board();
    let header = [0x16a800, 0, 0x20, 0x10001, 0x8002_504b, 0, 8, 0xb302_0000];
    let (x, y) = (512 + 64, 256 + 63);
    cmd64(&mut s, reg::READ_ORIGIN, 256 << 16 | 512);
    select_level(&mut s, 0x16a800, 0x20, 0x0800_4850);

    for front in [false, true] {
        buffer_write(&s, 0, x, y, if front { 0xff00_ffff } else { 0x00ff_00ff });
        buffer_write(&s, 1, x, y, if front { 0x00ff_00ff } else { 0xff00_ffff });
        copy_block(
            &mut s,
            [
                0x3000_0019,
                0x003f_0040,
                0x8020_0050,
                0x0001_0001,
                if front { 0x9263_3042 } else { 0x1263_3042 },
                0x16a800,
                0,
                0x3802_204b,
            ],
        );
        assert_eq!(
            texel_lanes(&s, 0x16a800, 0x20, 0, 0),
            0xff00_ffff,
            "front {front}"
        );

        ram.put(PAGE_A, &[GUARD; 40]);
        transfer(&mut s, &ram, header, &[pte(PAGE_A + 16, 8)]);
        let mut expect = [GUARD; 40];
        expect[16..24].copy_from_slice(&[0xff, 0xff, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(ram.get(PAGE_A, 40), expect, "front {front}");
    }
}

/// Copies fill 128x64 chunks; window rows land bottom row first.
///
/// QEMU: sn1/odyssey/readback/gl-rgba16-scratch-copy-chunks
#[test]
fn chunked_copies_into_a_level_read_back_as_16_bit_channels() {
    let (mut s, rec, ram) = readback_board();
    let (sample_x, sample_y, chunk_y) =
        ([0u32, 17, 64, 127], [0u32, 23, 32, 63], [64u32, 0, 64, 0]);
    let header = [
        0x16a800,
        0,
        0x20,
        64 << 16 | 128,
        0x8002_504b,
        0,
        0x400,
        0xb302_0000,
    ];
    let entries = [pte(PAGE_A, 0x8000), pte(PAGE_A + 0x8000, 0x8000)];
    let payload = 128 * 64 * 8;
    cmd64(&mut s, reg::READ_ORIGIN, 256 << 16 | 512);
    window_origin(&mut s, 17, 29);
    select_level(&mut s, 0x16a800, 0x20, 0x0800_4850);

    for (chunk, &top) in chunk_y.iter().enumerate() {
        let mut colours = [0u32; 4];
        for i in 0..4 {
            let (x, y) = ((512 + sample_x[i]) as i32, (256 + top + sample_y[i]) as i32);
            colours[i] = 0x1020_3040 + chunk as u32 * 0x1010_1000 + i as u32 * 0x0102_0304;
            buffer_write(&s, 0, x, y, colours[i] ^ 0x00ff_ffff);
            buffer_write(&s, 1, x, y, colours[i]);
        }
        copy_block(
            &mut s,
            [
                0x3000_0019,
                top << 16,
                0x8020_0050,
                64 << 16 | 128,
                0x1263_3042,
                0x16a800,
                0,
                0xf802_204b,
            ],
        );
        for i in 0..4 {
            assert_eq!(
                texel_lanes(&s, 0x16a800, 0x20, sample_x[i], 63 - sample_y[i]),
                colours[i],
                "chunk {chunk} sample {i}"
            );
        }

        let mut expect = vec![GUARD; payload + 32];
        expect[16..16 + payload].fill(0);
        for i in 0..4 {
            let at = 16 + (63 - sample_y[i]) as usize * 1024 + sample_x[i] as usize * 8;
            for c in 0..4 {
                let byte = (colours[i] >> (24 - 8 * c)) as u8;
                expect[at + 2 * c..][..2].copy_from_slice(&[byte, byte]);
            }
        }
        ram.put(PAGE_A - 16, &vec![GUARD; payload + 32]);
        transfer(&mut s, &ram, header, &entries);
        assert!(
            ram.get(PAGE_A - 16, payload + 32) == expect,
            "chunk {chunk}: the payload differs"
        );
        assert_eq!(rec.events(DMA_VECTOR).len(), chunk + 1);
    }

    buffer_write(&s, 1, 512, 256 + 128, 0x7654_3210);
    copy_block(
        &mut s,
        [
            0x3000_0019,
            128 << 16,
            0x8020_0050,
            0x0001_0001,
            0x1263_3042,
            0x16a800,
            511,
            0xf802_204b,
        ],
    );
    assert_eq!(texel_lanes(&s, 0x16a800, 0x20, 511, 0), 0x7654_3210);
    assert_eq!(
        texel_lanes(&s, 0x16a800, 0x20, 127, 0),
        0x1020_3040 + 3 * 0x1010_1000 + 0x0306_090c,
        "the chunk is left alone"
    );
    assert_eq!(texel_lanes(&s, 0x16a800, 0x20, 0, 64), 0);
}

/// A window-to-level copy of two pixels, read back by DMA as RGBA bytes.
///
/// QEMU: sn1/odyssey/readback/gl-rgba8-scratch-copy
#[test]
fn a_gl_copy_into_an_rgba8_level_reads_back_as_rgba_bytes() {
    let (mut s, _, ram) = readback_board();
    let colours = [0x1234_5678u32, 0x90ab_cdef];
    let header = [0x16bc00, 0, 0x20, 0x10002, 0x8001_3042, 0, 8, 0xb301_0000];
    let level = RGBA8_LEVEL | 0x20;
    cmd64(&mut s, reg::READ_ORIGIN, 256 << 16 | 512);
    select_level(&mut s, 0x16bc00, 0x20, RGBA8_MODE);
    window_origin(&mut s, 0, 0);
    for (i, &c) in colours.iter().enumerate() {
        buffer_write(&s, 0, 512 + 64 + i as i32, 256 + 63, c);
        buffer_write(&s, 1, 512 + 64 + i as i32, 256 + 63, c);
    }

    for i in 0..2u32 {
        copy_block(
            &mut s,
            [
                0x3000_0019,
                0x003f_0040 + i,
                0x8020_0050,
                0x0001_0001,
                0x9223_3002,
                0x16bc00,
                i,
                0x3801_210b,
            ],
        );
    }
    assert_eq!(
        (
            texel(&s, 0x16bc00, level, 0, 0),
            texel(&s, 0x16bc00, level, 1, 0)
        ),
        (colours[0], colours[1])
    );
    ram.put(PAGE_A, &[GUARD; 40]);
    transfer(&mut s, &ram, header, &[pte(PAGE_A + 16, 8)]);
    let mut expect = [GUARD; 40];
    expect[16..20].copy_from_slice(&colours[0].to_be_bytes());
    expect[20..24].copy_from_slice(&colours[1].to_be_bytes());
    assert_eq!(ram.get(PAGE_A, 40), expect);

    // The captured read origin plus the source offset selects (320, 319).
    cmd64(&mut s, reg::READ_ORIGIN, 0x0100_0100);
    select_level(&mut s, 0x16a800, 0x20, RGBA8_MODE);
    buffer_write(&s, 0, 320, 319, 0x2468_ace0);
    buffer_write(&s, 1, 320, 319, 0x2468_ace0);
    copy_block(
        &mut s,
        [
            0x3000_0019,
            0x003f_0040,
            0x8020_0050,
            0x0001_0001,
            0x9223_3002,
            0x16a800,
            0,
            0x3801_210b,
        ],
    );
    assert_eq!(texel(&s, 0x16a800, level, 0, 0), 0x2468_ace0);
    ram.put(PAGE_A, &[GUARD; 36]);
    transfer(
        &mut s,
        &ram,
        [0x16a800, 0, 0x20, 0x10001, 0x8001_3042, 0, 4, 0xb301_0000],
        &[pte(PAGE_A + 16, 4)],
    );
    let mut expect = [GUARD; 36];
    expect[16..20].copy_from_slice(&[0x24, 0x68, 0xac, 0xe0]);
    assert_eq!(ram.get(PAGE_A, 36), expect);
}

/// Index reads of a level are refused, a model choice that is not measured.
///
/// QEMU: sn1/odyssey/readback/index-follows-display-mode
#[test]
#[ignore = "unknown: the engine now answers a colour-index read of board memory by address (an_index_read_of_board_memory_by_address_returns_the_field_byte); this refusal was never measured"]
fn an_index_read_of_a_level_is_refused() {
    let (mut s, _, ram) = readback_board();
    let entry = [pte(PAGE_A + 16, 1)];

    ram.put(PAGE_A + 16, &[7]);
    transfer(
        &mut s,
        &ram,
        [0x16a000, 0, 0x10, 0x10001, 0x8333_1008, 0, 1, 0x3205_0041],
        &entry,
    );
    ram.put(PAGE_A, &[GUARD; 33]);
    transfer(
        &mut s,
        &ram,
        [0x16a000, 0, 0x10, 0x10001, 0x8333_1008, 0, 1, 0xb305_0041],
        &entry,
    );
    assert_eq!(ram.get(PAGE_A, 33), [GUARD; 33]);
}

/// An index read of the screen by address follows the display mode of the id under the pixel.
///
/// QEMU: sn1/odyssey/readback/index-follows-display-mode
#[test]
fn an_index_read_of_the_screen_follows_the_display_mode_of_the_id_under_the_pixel() {
    use crate::dev::odyssey::consts::{dispmode, ODSY_GAMMA_GROUP};
    use crate::dev::odyssey::field::FIELD_INDEX;

    let (mut s, _, ram) = readback_board();
    let screen = s.pbj(crate::dev::odyssey::frame::pbj::PIXEL_BASE);
    let header = [
        screen,
        40 << 16 | 31,
        0x50,
        1 << 16 | 1,
        0x8333_1008,
        0,
        1,
        0xb305_0041,
    ];
    let entry = [pte(PAGE_A + 16, 1)];
    let field = Field {
        at: 6,
        codec: Codec::Texel {
            bytes: 2,
            field: FIELD_INDEX,
        },
    };

    s.regs().set_dispmode(0, 0x004b_5f10);
    Rows::new(
        s.vram_ptr(),
        View::colour(&Surface::new(screen, 80, 4), field, MAX_DIM),
    )
    .store_word(31, 40, 0xff);
    ram.put(PAGE_A, &[GUARD; 33]);
    transfer(&mut s, &ram, header, &entry);
    assert_eq!(
        ram.get(PAGE_A + 16, 1),
        [0xff],
        "an index window gives its index byte"
    );

    s.regs()
        .set_dispmode(0, ODSY_GAMMA_GROUP << dispmode::GROUP_SHIFT);
    ram.put(PAGE_A, &[GUARD; 33]);
    transfer(&mut s, &ram, header, &entry);
    assert_eq!(
        ram.get(PAGE_A, 33),
        [GUARD; 33],
        "an RGB mode is not an index read"
    );
}

fn put_texel(s: &Board, w0: u32, w1: u32, x: u32, y: u32, word: u32) {
    s.gpu_sync();
    let (surface, field) = level_surface(w0, w1).expect("a level the library emits");
    Rows::new(s.vram_ptr(), View::colour(&surface, field, MAX_DIM))
        .store_word(x as i32, y as i32, word);
}

/// A page-out then page-in restores each texel at the format's precision.
/// Byte packing at 2 and 4 bytes a texel is the model's own, not measured.
///
/// QEMU: sn1/odyssey/readback/raw-texture-paging
#[test]
fn a_level_paged_out_and_back_in_keeps_its_texels() {
    let (mut s, _, ram) = readback_board();
    let entry = |n| [pte(PAGE_A, n)];

    // 2 bytes a texel: RGBA 4:4:4:4 (level index 12).
    let word1 = 0x00c0_0001;
    put_texel(&s, 0x100800, word1, 0, 0, 0xff00_00ff);
    put_texel(&s, 0x100800, word1, 1, 0, 0x00ff_8080);
    ram.put(PAGE_A, &[GUARD; 8]);
    transfer(
        &mut s,
        &ram,
        [0x100800, 0, 1, 1 << 16 | 2, 0x1001, 0, 4, 0x3300_0000],
        &entry(4),
    );
    assert_eq!(
        ram.get(PAGE_A, 8),
        [0xf0, 0x0f, 0x0f, 0x88, GUARD, GUARD, GUARD, GUARD]
    );
    transfer(
        &mut s,
        &ram,
        [0x101000, 0, 1, 1 << 16 | 2, 0x1001, 0, 4, 0x3200_0000],
        &entry(4),
    );
    assert_eq!(texel(&s, 0x101000, word1, 0, 0), 0xff00_00ff);
    assert_eq!(texel(&s, 0x101000, word1, 1, 0), 0x00ff_8888);

    // 4 bytes a texel: RGBA8 (index 17), the word as stored.
    let word1 = 0x4110_0001;
    put_texel(&s, 0x103000, word1, 0, 0, 0xff00_00ff);
    put_texel(&s, 0x103000, word1, 1, 0, 0x00ff_8080);
    ram.put(PAGE_A, &[GUARD; 12]);
    transfer(
        &mut s,
        &ram,
        [0x103000, 0, 1, 1 << 16 | 2, 0x13002, 0, 8, 0x3301_0000],
        &entry(8),
    );
    assert_eq!(
        ram.get(PAGE_A, 12),
        [0xff, 0x00, 0x00, 0xff, 0x00, 0xff, 0x80, 0x80, GUARD, GUARD, GUARD, GUARD]
    );
    transfer(
        &mut s,
        &ram,
        [0x103800, 0, 1, 1 << 16 | 2, 0x13002, 0, 8, 0x3201_0000],
        &entry(8),
    );
    assert_eq!(texel(&s, 0x103800, word1, 0, 0), 0xff00_00ff);
    assert_eq!(texel(&s, 0x103800, word1, 1, 0), 0x00ff_8080);

    // 8 bytes a texel: RGBA 12:12:12:12 (index 33).
    let word1 = 0x8210_0001;
    put_texel(&s, 0x104000, word1, 0, 0, 0xff00_00ff);
    put_texel(&s, 0x104000, word1, 1, 0, 0x00ff_8080);
    ram.put(PAGE_A, &[GUARD; 16]);
    transfer(
        &mut s,
        &ram,
        [0x104000, 0, 1, 1 << 16 | 2, 0x25004, 0, 16, 0x3302_0000],
        &entry(16),
    );
    assert_ne!(ram.get(PAGE_A, 16), [GUARD; 16], "the page is written");
    transfer(
        &mut s,
        &ram,
        [0x104800, 0, 1, 1 << 16 | 2, 0x25004, 0, 16, 0x3202_0000],
        &entry(16),
    );
    assert_eq!(texel(&s, 0x104800, word1, 0, 0), 0xff00_00ff);
    assert_eq!(texel(&s, 0x104800, word1, 1, 0), 0x00ff_8080);
}
