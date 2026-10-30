//! GL pixel transfers: image writes by burst and by DMA, reads, and the
//! copy block, against the headers the datasheet's "Pixel transfer
//! headers from the OpenGL library" section records.

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
