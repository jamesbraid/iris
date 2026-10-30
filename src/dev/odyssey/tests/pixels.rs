//! GL pixel transfers: image writes by burst and by DMA, reads, and the
//! copy block, against the headers the datasheet's "Pixel transfer
//! headers from the OpenGL library" section records.

use super::dma::{PAGE_A, TABLE, descriptor, pte, put_table};
use super::*;

/// A board with guest RAM whose one clip rectangle covers the screen, so
/// every in-plane pixel is the window's.
fn window() -> (Box<Board>, Arc<Ram>) {
    let (mut s, _, ram) = board_with_ram();
    s.clip_kept = 1;
    s.clip_rect[0] = ClipRect { x0: 0, y0: 0, x1: SGI_ODYSSEY_WIDTH as i32 - 1, y1: SGI_ODYSSEY_HEIGHT as i32 - 1 };
    (s, ram)
}

/// A GL image write by pixel burst, as the library sends a small
/// glDrawPixels: BEGIN with the image primitive, the raster box from
/// (x1, y1) to (x2, y2), the transfer header, then the data words under
/// the burst register for `fmt`, at most 16 a burst.
fn burst_image(s: &mut Board, (x1, y1, x2, y2): (u32, u32, u32, u32), header: u32, fmt: u32, words: &[u32]) {
    cmd(s, select(ODSY_REG_GL_BEGIN));
    cmd(s, ODSY_GL_PRIMITIVE_IMAGE);
    cmd(s, ODSY_CMD_IMAGE_BOX);
    for i in 0..10 {
        let v = match i {
            ODSY_IMAGE_BOX_X1_INDEX => x1,
            ODSY_IMAGE_BOX_Y1_INDEX => y1,
            ODSY_IMAGE_BOX_X2_INDEX => x2,
            ODSY_IMAGE_BOX_Y2_INDEX => y2,
            _ => 0,
        };
        cmd(s, (v as f32).to_bits());
    }
    cmd(s, ODSY_CMD_IMAGE_HEADER);
    cmd(s, header);
    for chunk in words.chunks(16) {
        let n = if chunk.len() == 16 { ODSY_CMD_ARITY_MASK } else { chunk.len() as u32 };
        cmd(s, select(ODSY_IMAGE_BURST_BASE | n << ODSY_CMD_ARITY_SHIFT | fmt));
        for &w in chunk {
            cmd(s, w);
        }
    }
    cmd(s, select(ODSY_REG_GL_END));
}

/// glDrawPixels of RGBA bytes, the header the library sends for a 2x1
/// image (count 2, source code 0x4b): the pixels keep the alpha they
/// carry. Before the datasheet's alpha section the model stored 255.
///
/// Datasheet: "Alpha from image writes" (established that the library
/// sends the alpha, inferred that the board stores it).
#[test]
fn a_gl_rgba_image_keeps_its_alpha() {
    let (mut s, _) = window();
    burst_image(&mut s, (40, 10, 42, 11), 0x0021_104b, ODSY_IMAGE_FMT_RGB, &[0x116c_c722, 0x2233_4400]);
    assert_eq!(pixel(&s, 0, 40, 10), 0x116c_c722);
    assert_eq!(pixel(&s, 0, 41, 10), 0x2233_4400);
}

/// Luminance-alpha bytes (0x48) and A, B, G, R bytes (0x4c) keep their
/// alpha too; RGB bytes (0x49) carry none and store 255.
#[test]
fn luminance_alpha_and_abgr_images_keep_alpha_and_rgb_stores_255() {
    let (mut s, _) = window();
    burst_image(&mut s, (40, 10, 42, 11), 0x0021_1048, ODSY_IMAGE_FMT_CI, &[0x116c_2233]);
    assert_eq!(pixel(&s, 0, 40, 10), 0x1111_116c);
    assert_eq!(pixel(&s, 0, 41, 10), 0x2222_2233);

    burst_image(&mut s, (40, 12, 41, 13), 0x0011_104c, ODSY_IMAGE_FMT_RGB, &[0x11c7_6c22]);
    assert_eq!(pixel(&s, 0, 40, 12), 0x226c_c711);

    burst_image(&mut s, (40, 14, 41, 15), 0x0011_1049, ODSY_IMAGE_FMT_RGB, &[0x1122_3300]);
    assert_eq!(pixel(&s, 0, 40, 14), 0x1122_33ff);
}

/// The X server's own 24-bit image, put by DMA into the colour buffer
/// under 0x4c, still stores 255: the alpha rule is for GL image writes.
#[test]
fn an_x_server_abgr_put_still_stores_255() {
    let (mut s, ram) = window();
    for w in [select(ODSY_REG_DEST_SELECT), 0x2000, 0x50, 0x2000_6950, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
    let table = 0xb80000u64;
    let page = 0xb84000u64;
    ram.put(page, &[0x7f, 0x01, 0x9b, 0xfd]);
    let header = [0x2000u32, 300 << 16 | 200, 0x50, 0x10001, 0x8223_305a, 0, 4, 0x3201_004c];
    let mut t = vec![0u8; ODSY_DMA_TABLE_HEADER as usize + 16];
    for (i, w) in header.iter().enumerate() {
        t[i * 4..][..4].copy_from_slice(&w.to_be_bytes());
    }
    t[ODSY_DMA_TABLE_HEADER as usize..][..8].copy_from_slice(&(page << ODSY_DMA_PTE_ADDR_SHIFT | 4).to_be_bytes());
    ram.put(table, &t);
    let half = |h: u64| (h & 0x1ffff) as u32 | ((h & 0xfe_0000) << 6) as u32;
    cmd64(&mut s, ODSY_DMA_DESC_UPLOAD, half(table >> 24));
    cmd64(&mut s, half(table & 0xff_ffff), 0);
    assert_eq!(pixel(&s, 0, 200, 300), 0xfd9b_01ff);
}

/// Word 4 of a GL read from the bound read drawable, as the library builds
/// it: class and code, bit 12, the read selector and the front bit.
fn read_word4(class: u32, code: u32) -> u32 {
    0x9000_1000 | class | code << 3
}

/// Read the `w` x 1 pixels at (x, y) of the window by DMA with word 4
/// `word4` at `bytes` bytes a pixel, into a guest page pre-filled with
/// 0xa5 guard bytes. Returns the 64 bytes at the start of the page.
fn dma_read(s: &mut Board, ram: &Ram, (x, y, w): (u32, u32, u32), word4: u32, bytes: u32) -> Vec<u8> {
    let bpp_code = DMA_BPP.iter().position(|&b| b as u32 == bytes).expect("a size the header can name") as u32;

    ram.put(PAGE_A, &[0xa5; 64]);
    put_table(
        ram,
        TABLE,
        [ODSY_DMA_SENTINEL_READ, y << 16 | x, 0x50, 1 << 16 | w, word4, 0, w * bytes, ODSY_DMA_TO_HOST | bpp_code << 16],
        &[pte(PAGE_A, w * bytes)],
    );
    descriptor(s, TABLE);
    ram.get(PAGE_A, 64)
}

/// A drawn pixel as 0xRRGGBBAA.
const READ_PIXEL: u32 = 0xa55a_3cff;

fn read_window() -> (Box<Board>, Arc<Ram>) {
    let (mut s, ram) = window();
    pixel_write(&mut s, 30, 40, READ_PIXEL);
    // A colour-map-free RGB mode, so the pixel is a colour and not an index.
    s.dispmode[0] = ODSY_GAMMA_GROUP << ODSY_DISPMODE_GROUP_SHIFT;
    (s, ram)
}

/// glReadPixels of a narrow window's packed types: the library asks for
/// each type's own header, and the board hands back that packing of the
/// pixel's top bits. Before the read table, none of these headers was
/// modelled and the guest buffer was left as it was.
///
/// Datasheet: "Pixel transfer headers from the OpenGL library", "Read
/// table" (established as the library's request, the packing inferred).
/// The narrowing is an approximation, see DMA_GL_READS.
#[test]
fn a_read_of_a_packed_type_returns_that_packing_of_the_pixel() {
    let (mut s, ram) = read_window();
    // (class, code, bytes, the word a pixel packs to)
    let cases: [(u32, u32, u32, u32); 9] = [
        (1, 13, 2, 0xa53f),      // RGBA, 4_4_4_4
        (1, 6, 2, 0xf35a),       // RGBA, 4_4_4_4_REV
        (1, 12, 2, 0xa2cf),      // RGBA, 5_5_5_1
        (1, 9, 2, 0x9d74),       // RGBA, 1_5_5_5_REV
        (1, 3, 2, 0xd167),       // BGRA, 1_5_5_5_REV
        (1, 10, 2, 0xa2c7),      // RGB, 5_6_5
        (2, 0, 4, 0xa596_93c3),  // RGBA, 10_10_10_2
        (2, 12, 4, 0xcf05_a696), // RGBA, 2_10_10_10_REV
        (1, 5, 2, 0x3ae9),       // BGRA, 5_5_5_1
    ];

    for (class, code, bytes, want) in cases {
        let got = dma_read(&mut s, &ram, (30, 40, 1), read_word4(class, code), bytes);

        assert_eq!(got[..bytes as usize], want.to_be_bytes()[4 - bytes as usize..], "class {class} code {code}");
        assert_eq!(got[bytes as usize], 0xa5, "class {class} code {code} wrote past the pixel");
    }
}

/// The byte forms: RGB, BGR, BGRA, ABGR and RGBA bytes of an eight-bit
/// window and the 16-bit luminance-alpha of a texture.
#[test]
fn a_read_of_a_byte_type_returns_the_pixel_bytes_in_the_requested_order() {
    let (mut s, ram) = read_window();
    let cases: [(u32, u32, usize, &[u8]); 7] = [
        (2, 8, 4, &[0xa5, 0x5a, 0x3c, 0xff]),
        (2, 1, 3, &[0xa5, 0x5a, 0x3c]),
        (2, 3, 3, &[0x3c, 0x5a, 0xa5]),
        (2, 4, 4, &[0x3c, 0x5a, 0xa5, 0xff]),
        (2, 11, 4, &[0xff, 0x3c, 0x5a, 0xa5]),
        (1, 1, 4, &[0xa5, 0x5a, 0x3c, 0xff]),
        (2, 6, 2, &[0xa5, 0xff]),
    ];

    for (class, code, bytes, want) in cases {
        let got = dma_read(&mut s, &ram, (30, 40, 1), read_word4(class, code), bytes as u32);

        assert_eq!(&got[..bytes], want, "class {class} code {code}");
        assert_eq!(got[bytes], 0xa5, "class {class} code {code} wrote past the pixel");
    }
}

/// Word 4's field codes (bits 23:20, then 27:24) added to a read header.
fn with_fields(word4: u32, first: u32, second: u32) -> u32 {
    word4 | first << 20 | second << 24
}

/// Class 2 code 2 is BGRA/UNSIGNED_INT_8_8_8_8_REV colour and also a
/// 32-bit depth word; class 0 code 0 is a stencil byte and also a colour
/// index byte. The field codes in word 4 tell them apart: a listed colour
/// pair is colour, (0,0) and (4,0) are depth, and (4,0) is stencil. The
/// (0,0) stencil or storage-0 index tie is served as an index.
///
/// Datasheet: "Read table" (established that the field codes differ; the
/// tie is unknown and labelled).
#[test]
fn field_codes_separate_a_colour_read_from_a_depth_or_stencil_read() {
    let (mut s, ram) = read_window();
    let colour = [0xff, 0xa5, 0x5a, 0x3c];

    // Colour pairs from the datasheet's table: A, R, G, B bytes.
    for (a, b) in [(6, 2), (2, 2), (3, 2), (7, 3), (2, 0)] {
        let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(2, 2), a, b), 4);
        assert_eq!(got[..5], [0xff, 0xa5, 0x5a, 0x3c, 0xa5], "fields ({a},{b})");
    }
    // Depth-looking pairs: not a colour.
    for (a, b) in [(0, 0), (4, 0)] {
        let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(2, 2), a, b), 4);
        assert_ne!(got[..4], colour, "fields ({a},{b}) are a depth read");
    }
    // Stencil (4,0) is the stencil plane's byte, not a colour index.
    let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(0, 0), 4, 0), 1);
    assert_eq!(got[..2], [0x00, 0xa5]);

    // The (0,0) tie is served as an index byte. On a window shown as colour
    // there is no index to return and the read is refused.
    let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(0, 0), 0, 0), 1);
    assert_eq!(got[..2], [0xa5, 0xa5], "refused on an RGB window");
    pixel_write(&mut s, 31, 40, 0x2a);
    s.dispmode[0] = 0;
    let got = dma_read(&mut s, &ram, (31, 40, 1), with_fields(read_word4(0, 0), 0, 0), 1);
    assert_eq!(got[..2], [0x2a, 0xa5]);
}

/// A colour-index window reads back its index: one byte under class 0 code
/// 1, a big-endian short under class 1 code 14. A pixel the display shows
/// as a colour reads its red byte under class 0 code 1.
#[test]
fn a_colour_index_read_returns_the_index() {
    let (mut s, ram) = read_window();
    pixel_write(&mut s, 31, 40, 0x5a3);
    s.dispmode[0] = 0;

    let got = dma_read(&mut s, &ram, (31, 40, 1), read_word4(1, 14), 2);
    assert_eq!(got[..3], [0x05, 0xa3, 0xa5]);
    pixel_write(&mut s, 31, 40, 0x2a);
    let got = dma_read(&mut s, &ram, (31, 40, 1), read_word4(0, 1), 1);
    assert_eq!(got[..2], [0x2a, 0xa5]);

    s.dispmode[0] = ODSY_GAMMA_GROUP << ODSY_DISPMODE_GROUP_SHIFT;
    let got = dma_read(&mut s, &ram, (30, 40, 1), read_word4(0, 1), 1);
    assert_eq!(got[..2], [0xa5, 0xa5], "the red byte of 0xa55a3cff");
}

/// A GL image write by DMA to the draw drawable: the table at `TABLE` names
/// `payload` for a `w` x 1 box at (x, y) under word 4 `word4` and word 7's
/// low bits `code`, four bytes a pixel, rows bottom-up.
fn dma_write(s: &mut Board, ram: &Ram, (x, y, w): (u32, u32, u32), word4: u32, code: u32, payload: &[u8]) {
    ram.put(PAGE_A, payload);
    put_table(
        ram,
        TABLE,
        [ODSY_DMA_SENTINEL_DRAW, y << 16 | x, 0x50, 1 << 16 | w, word4, 0, w * 4, 1 << 30 | 1 << 16 | code],
        &[pte(PAGE_A, w * 4)],
    );
    descriptor(s, TABLE);
}

fn stencil_at(s: &Board, x: i32, y: i32) -> u32 {
    let p = s.stencil_mem.plane(s.stencil_plane).expect("the stencil plane");
    s.stencil_mem.plane_load(&p, x, y)
}

/// A depth read from a 12/12/12/12 window is class 1 code 0, two bytes a
/// pixel: the plane's 24-bit depth word in its 16-bit form (exponent and
/// the top ten mantissa bits).
///
/// Datasheet: "Depth and stencil headers" (the request is established; that
/// the board hands back the plane's 16-bit form is inferred).
#[test]
fn a_16_bit_depth_read_returns_the_planes_16_bit_form() {
    let (mut s, ram) = read_window();
    s.depth_store(30, 40, 0x002b_5a1f);

    let got = dma_read(&mut s, &ram, (30, 40, 1), read_word4(1, 0), 2);
    assert_eq!(got[..3], [0x2b, 0x5a, 0xa5]);
}

/// The field codes (0,0) and (4,0) on class 2 code 2 mark a depth read of
/// four bytes, the plane's 24-bit word; (4,0) on class 0 code 0 a stencil
/// read of one byte. The same header with a colour pair is colour, as the
/// test above has it. Nothing here is the colour planes.
///
/// Datasheet: "Read table" (established that the field codes separate them;
/// that the plane holds the board's word is inferred; the stencil plane is a
/// model choice).
#[test]
fn a_depth_or_stencil_read_returns_the_planes_word_or_byte() {
    let (mut s, ram) = read_window();
    s.depth_store(30, 40, 0x00d2_a45f);
    s.stencil_store(30, 40, 0xc7);

    for (a, b) in [(0, 0), (4, 0)] {
        let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(2, 2), a, b), 4);
        assert_eq!(got[..5], [0x00, 0xd2, 0xa4, 0x5f, 0xa5], "depth, fields ({a},{b})");
    }
    let got = dma_read(&mut s, &ram, (30, 40, 1), with_fields(read_word4(0, 0), 4, 0), 1);
    assert_eq!(got[..2], [0xc7, 0xa5]);
}

/// A depth image is format 0, type 4, word 4's size field 1, and a stencil
/// image the same with size 0. Each goes to its own plane, one four-byte
/// word a pixel, and the colour planes stay as they were.
///
/// Datasheet: "Depth and stencil headers", established as the library's
/// request.
#[test]
fn a_depth_or_stencil_image_by_dma_writes_its_own_plane() {
    let (mut s, ram) = read_window();

    dma_write(&mut s, &ram, (30, 40, 2), 1 << 13 | 0x1000, 0x080, &[0xff, 0x12, 0x34, 0x56, 0x00, 0xab, 0xcd, 0xef]);
    assert_eq!(s.depth_load(30, 40), 0x0012_3456, "bits 31:24 are not depth");
    assert_eq!(s.depth_load(31, 40), 0x00ab_cdef);

    dma_write(&mut s, &ram, (30, 40, 2), 0x1000, 0x080, &[0, 0, 0, 0x11, 0, 0, 0, 0xc7]);
    assert_eq!(stencil_at(&s, 30, 40), 0x11);
    assert_eq!(stencil_at(&s, 31, 40), 0xc7);
    assert_eq!(s.depth_load(30, 40), 0x0012_3456, "a stencil write leaves depth");
    assert_eq!(pixel(&s, 0, 30, 40), READ_PIXEL, "and colour");
}

/// The same code by pixel burst names no plane: nothing says depth or
/// stencil, so it paints neither, and does not land in the colour planes as
/// RGB.
#[test]
fn a_depth_or_stencil_image_by_burst_paints_nothing() {
    let (mut s, _) = window();

    burst_image(&mut s, (40, 10, 42, 11), 0x0021_1080, ODSY_IMAGE_FMT_RGB, &[0x0012_3456, 0x00ab_cdef]);
    for buffer in 0..2 {
        assert_eq!(pixel(&s, buffer, 40, 10), 0);
        assert_eq!(pixel(&s, buffer, 41, 10), 0);
    }
}
