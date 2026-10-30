//! Ported from hw/misc/sgi-odyssey.c (pixel DMA, queries, colour conversions,
//! texture level build) in jamesbraid/qemu-sgi-origami.

use super::*;

#[derive(Clone, Copy, Default)]
pub struct OdysseyDMAEntry {
    pub addr: u64,
    pub len: u32,
}

#[derive(Clone, Copy, Default)]
pub struct OdysseyDMALayout {
    pub code: u8,
    pub bytes: u8,
    pub from: [u8; 8],
}

#[derive(Clone, Copy, Default)]
pub struct OdysseyDMASource {
    pub obj: i32,
    pub front: bool,
    pub x: i64,
    pub y: i64,
}

#[derive(Clone, Default)]
pub struct OdysseyDMAPiece {
    pub map: SGIDMAMap,
    pub at: u64,
    pub src: u64,
    pub len: u32,
}

// Anonymous enum: command-interpreter states.
pub const ODSY_CMD_IDLE: u32 = 0;
pub const ODSY_CMD_OPERANDS: u32 = 1;
pub const ODSY_CMD_COPY_RECT: u32 = 2;
pub const ODSY_CMD_PATTERN_LOAD: u32 = 3;
pub const ODSY_CMD_IMAGE_BOX_LOAD: u32 = 4;
pub const ODSY_CMD_IMAGE_BURST: u32 = 5;
pub const ODSY_CMD_MATRIX_LOAD: u32 = 6;
pub const ODSY_CMD_GL_RECT: u32 = 7;
pub const ODSY_CMD_SKIP: u32 = 8;

/// The table's physical address half, unpacked from a descriptor word.
pub fn odyssey_dma_desc_half(word: u32) -> u64 {
    ((word & ODSY_DMA_DESC_ADDR_LOW_MASK) as u64)
        | ((((word >> ODSY_DMA_DESC_ADDR_HIGH_SHIFT) & ODSY_DMA_DESC_ADDR_HIGH_MASK) as u64) << 17)
}

pub fn odyssey_dma_table_address(w1: u32, w3: u32) -> u64 {
    (odyssey_dma_desc_half(w1) << 24) | odyssey_dma_desc_half(w3)
}

/// The hard DMA-done interrupt: the routed source whose vector is byte 15:8
/// of the route-vectors register. A DMA the driver asks to be told about
/// completes on two events, the fence and this interrupt; the model raises it
/// for exactly the descriptors whose header word 7 bit 31 asks for it, once
/// the transfer is finished. Nothing is delivered before the driver routes it.
pub fn odyssey_dma_done(s: &mut SGIOdysseyState) {
    let mut f = s.front.lock();
    if odyssey_routed(&f) {
        odyssey_deliver_routed(&mut f, ODSY_ROUTE_VECTOR_DMA_SHIFT, "DMA-done", true);
    }
}

/// Bytes per pixel, indexed by word 7's bits 18:16.
pub static odyssey_dma_bpp: [u8; (ODSY_DMA_BPP_MASK + 1) as usize] = [2, 4, 8, 16, 12, 1, 6, 3];

/// The entry's page pairs, up to a zero pair or the twelfth, into e[].
/// Returns how many, or -1 when a read fails.
pub fn odyssey_dma_entries(bus: &SGIDMABus, table: u64, seq: u64, e: &mut [OdysseyDMAEntry]) -> i32 {
    let mut n: usize = 0;

    while n < ODSY_DMA_MAX_PAGES {
        let mut raw = [0u8; 8];

        if !sgi_dma_read(bus, table + ODSY_DMA_TABLE_HEADER + (n as u64) * 8, &mut raw, true, seq) {
            return -1;
        }
        let pte = ldq_be_p(&raw);
        if pte & (ODSY_DMA_PTE_LEN_MASK as u64) == 0 {
            return n as i32;
        }
        e[n].addr = pte >> ODSY_DMA_PTE_ADDR_SHIFT;
        e[n].len = (pte & (ODSY_DMA_PTE_LEN_MASK as u64)) as u32;
        n += 1;
    }
    n as i32
}

/// One pixel of a transfer at (x, y) of object obj, or of the colour buffers
/// the drawing mask selects when obj is -1, decoded by the header's source
/// format as an image transfer's pixel is. Clipped to the plane only.
pub fn odyssey_dma_put(s: &mut SGIOdysseyState, obj: i32, x: i64, y: i64, src: &[u8], code: u32) {
    let mut pixel: u32 = 0;

    odyssey_decode_pixel(s, code, src, &mut pixel);
    if obj >= 0 {
        if x >= 0 && y >= 0 {
            odyssey_object_store(s, obj, x as u32, y as u32, pixel);
        }
        return;
    }
    if x < 0 || y < 0 || x >= SGI_ODYSSEY_WIDTH as i64 || y >= SGI_ODYSSEY_HEIGHT as i64 {
        return;
    }
    for b in 0..ODSY_BUFFERS {
        if odyssey_draw_buffers(s) & (1u32 << b) != 0 {
            let word = if odyssey_code_is_index(code) { pixel } else { (pixel & !0xffu32) | 0xff };
            odyssey_buffer_store(s, b, x, y, word);
        }
    }
}

/// The source format of a DMA: word 7's low byte, the same code as an image
/// transfer header's. Another code keeps the earlier rule (4 bytes as
/// 0x00RRGGBB, 1 byte as a colour index), reported once.
pub fn odyssey_dma_code(s: &mut SGIOdysseyState, hdr: &[u32], bpp: u32) -> u32 {
    let code = hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK;

    if odyssey_pixel_bytes(code) == bpp {
        return code;
    }
    if !s.dma_code_reported {
        s.dma_code_reported = true;
        crate::qlog!("sgi-odyssey: DMA source format {:#x} at {} bytes a pixel is not modeled", code, bpp);
    }
    if bpp == 4 { ODSY_IMAGE_CODE_FALLBACK_XRGB } else { ODSY_IMAGE_CODE_CI }
}

/// True when hdr is a backing-store page the model copies, bpp a texel.
pub fn odyssey_dma_raw(hdr: &[u32], bpp: u32) -> bool {
    let fmt = hdr[ODSY_DMA_HDR_FORMAT];

    (hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK) == 0
        && ((fmt == ODSY_DMA_FMT_RAW16 && bpp == 2) || (fmt == ODSY_DMA_FMT_RAW32 && bpp == 4))
}

/// True, reported once, when hdr is an 8-byte backing-store page.
pub fn odyssey_dma_raw64(s: &mut SGIOdysseyState, hdr: &[u32], bpp: u32) -> bool {
    if hdr[ODSY_DMA_HDR_FORMAT] != ODSY_DMA_FMT_RAW64
        || bpp != 8
        || (hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK) != 0
    {
        return false;
    }
    if odyssey_dma_report(s, ODSY_DMA_REPORT_RAW64) {
        crate::qlog!(
            "sgi-odyssey: an 8-byte backing-store page of object {:#x} is not modeled",
            hdr[ODSY_DMA_HDR_DEST]
        );
    }
    true
}

pub fn odyssey_raw_nibble(byte: u32) -> u32 {
    (byte * 15 + 127) / 255
}

/// A stored texel as a backing-store page's bpp raw bytes.
pub fn odyssey_raw_pack(rgba: u32, bpp: u32, dst: &mut [u8]) {
    if bpp == 4 {
        stl_be_p(dst, rgba);
        return;
    }
    stw_be_p(
        dst,
        odyssey_raw_nibble(rgba >> 24) << 12
            | odyssey_raw_nibble((rgba >> 16) & 0xff) << 8
            | odyssey_raw_nibble((rgba >> 8) & 0xff) << 4
            | odyssey_raw_nibble(rgba & 0xff),
    );
}

/// A backing-store page's bpp raw bytes back as a stored texel.
pub fn odyssey_raw_unpack(src: &[u8], bpp: u32) -> u32 {
    if bpp == 4 {
        return ldl_be_p(src);
    }
    let v = lduw_be_p(src);
    ((v >> 12) & 0xf) * 0x11u32 << 24
        | ((v >> 8) & 0xf) * 0x11u32 << 16
        | ((v >> 4) & 0xf) * 0x11u32 << 8
        | (v & 0xf) * 0x11u32
}

/// A transfer into the colour buffer the screen shows (obj -1) or into
/// board-memory object obj: paint the header's rectangle from the source.
/// The source is a stream of pixel bytes spread over the entries: each row
/// takes rowbytes bytes, moving to the next entry whenever the current one's
/// count runs out, then the cursor steps over skip uncounted bytes. No byte
/// outside the entries is read.
pub fn odyssey_dma_to_buffer(s: &mut SGIOdysseyState, bus: &SGIDMABus, table: u64, seq: u64, hdr: &[u32], obj: i32) {
    let mut e = [OdysseyDMAEntry::default(); ODSY_DMA_MAX_PAGES];
    let x = hdr[ODSY_DMA_HDR_ORIGIN] & 0xffff;
    let y = hdr[ODSY_DMA_HDR_ORIGIN] >> 16;
    let w = hdr[ODSY_DMA_HDR_SIZE] & 0xffff;
    let h = if hdr[ODSY_DMA_HDR_SIZE] >> 16 != 0 { hdr[ODSY_DMA_HDR_SIZE] >> 16 } else { 0x10000 };
    let skip = hdr[ODSY_DMA_HDR_SKIP];
    let rowbytes = hdr[ODSY_DMA_HDR_ROWBYTES] & ODSY_DMA_ROWBYTES_MASK;
    let bpp = odyssey_dma_bpp[((hdr[ODSY_DMA_HDR_CONTROL] >> ODSY_DMA_BPP_SHIFT) & ODSY_DMA_BPP_MASK) as usize] as u32;
    let comps = odyssey_texel12_components(hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK);
    // A 12-bit texture source into an object: a byte or two a component.
    let texel12 = obj >= 0 && comps != 0 && (bpp == comps || bpp == 2 * comps);
    // An 8-bit luminance-alpha source into an object, two bytes a pixel.
    let lum_alpha = obj >= 0 && bpp == 2 && (hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK) == ODSY_IMAGE_CODE_LUM_ALPHA;
    let raw = obj >= 0 && odyssey_dma_raw(hdr, bpp);
    let mut payload: u64 = 0;
    let mut pos: u64 = 0;

    if obj >= 0 && odyssey_dma_raw64(s, hdr, bpp) {
        return;
    }
    if bpp != 4 && bpp != 3 && bpp != 1 && !texel12 && !lum_alpha && !raw {
        if !s.dma_bpp_reported {
            s.dma_bpp_reported = true;
            crate::qlog!(
                "sgi-odyssey: DMA to buffer {:#x} at {} bytes a pixel is not modeled",
                hdr[ODSY_DMA_HDR_DEST],
                bpp
            );
        }
        return;
    }
    let code = if texel12 || raw {
        hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK
    } else {
        odyssey_dma_code(s, hdr, bpp)
    };
    let n = odyssey_dma_entries(bus, table, seq, &mut e);
    if n < 0 {
        return;
    }
    let n = n as usize;
    for k in 0..n {
        payload += e[k].len as u64;
    }
    if rowbytes as u64 != (w as u64) * (bpp as u64) || payload != (rowbytes as u64) * (h as u64) {
        crate::qlog!(
            "sgi-odyssey: DMA header at {:#x} disagrees with itself: {}x{} at {} bytes a pixel, {} bytes a row, {} bytes in its entries",
            table, w, h, bpp, rowbytes, payload
        );
        return;
    }
    let mut row = vec![0u8; rowbytes as usize];
    let mut i: usize = 0;
    let mut left: u32 = if n > 0 { e[0].len } else { 0 };
    'done: {
        for r in 0..h {
            let mut got: u32 = 0;

            while got < rowbytes {
                if left == 0 {
                    // Checked above: the entries hold exactly h rows.
                    i += 1;
                    pos = 0;
                    left = e[i].len;
                }
                let take = left.min(rowbytes - got);
                if !sgi_dma_read(
                    bus,
                    e[i].addr.wrapping_add(pos),
                    &mut row[got as usize..(got + take) as usize],
                    true,
                    seq,
                ) {
                    break 'done;
                }
                got += take;
                pos += take as u64;
                left -= take;
            }
            if left != 0 {
                pos += skip as u64;
            }
            for c in 0..w {
                let px = &row[(c * bpp) as usize..];
                if raw {
                    odyssey_object_store(s, obj, x.wrapping_add(c), y.wrapping_add(r), odyssey_raw_unpack(px, bpp));
                    continue;
                }
                if texel12 {
                    odyssey_object_store(
                        s,
                        obj,
                        x.wrapping_add(c),
                        y.wrapping_add(r),
                        odyssey_decode_texel12(code, px, bpp != comps),
                    );
                    continue;
                }
                odyssey_dma_put(s, obj, (x as i64) + (c as i64), (y as i64) + (r as i64), px, code);
            }
        }
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

// Source format code 0x6c: a GL DMA image write of A, B, G, R bytes, one
// pixel a 4-byte word. ESTABLISHED from a model run: the software's payload
// carries the bytes in that order. INFERRED: that the board decodes the word
// in that order, from the image coming out right. Word 7 of the two observed
// writes differs in bits 29:25, which are ignored; every other bit must
// match, so an unseen form stays reported.
//
// Movie frames: the same A, B, G, R bytes under source format code 0x4c,
// with word 4 0x82233000. OBSERVED on the model: each 320x240 frame arrives
// as nine tiles covering it exactly, rows of 4 * width bytes with the rest
// of each source row skipped, first tile sent holding the first source rows.
// INFERRED from the picture: the byte order, that the source's first row is
// the top of the picture, and that the header's y counts up from the bottom
// of the window. What tells the board the rows run top down, and the window
// height its bottom-up y needs, remain unknown.

/// The draw-sentinel uploads this model takes: the captured RGBA16 write
/// (eight bytes a pixel), the ABGR write and the movie write (four bytes a
/// pixel; the movie sets *top_down). 0 for any other upload.
pub fn odyssey_dma_draw_sentinel_bytes(hdr: &[u32], top_down: &mut bool) -> u32 {
    let control = hdr[ODSY_DMA_HDR_CONTROL];

    *top_down = false;
    if hdr[ODSY_DMA_HDR_FORMAT] == 0x02635000 && control == 0x7a02006b {
        return 8;
    }
    if hdr[ODSY_DMA_HDR_FORMAT] == 0x80035000 && (control & !ODSY_DMA_ABGR_CONTROL_IGNORED) == ODSY_DMA_ABGR_CONTROL {
        return 4;
    }
    if hdr[ODSY_DMA_HDR_FORMAT] == ODSY_DMA_MOVIE_FORMAT
        && (control & !ODSY_DMA_ABGR_CONTROL_IGNORED) == ODSY_DMA_MOVIE_CONTROL
    {
        *top_down = true;
        return 4;
    }
    0
}

/// Stage the pixel bytes first so a failed source read cannot leave a
/// partially painted rectangle. Entry lengths count pixels; row gaps advance
/// the address cursor only while the current entry has bytes remaining.
pub fn odyssey_dma_draw_sentinel_short(s: &mut SGIOdysseyState, bus: &SGIDMABus, table: u64, seq: u64, hdr: &[u32]) {
    let mut e = [OdysseyDMAEntry::default(); ODSY_DMA_MAX_PAGES];
    let x = hdr[ODSY_DMA_HDR_ORIGIN] & 0xffff;
    let y = hdr[ODSY_DMA_HDR_ORIGIN] >> 16;
    let w = hdr[ODSY_DMA_HDR_SIZE] & 0xffff;
    let h = if hdr[ODSY_DMA_HDR_SIZE] >> 16 != 0 { hdr[ODSY_DMA_HDR_SIZE] >> 16 } else { 0x10000 };
    let skip = hdr[ODSY_DMA_HDR_SKIP];
    let rowbytes = hdr[ODSY_DMA_HDR_ROWBYTES] & ODSY_DMA_ROWBYTES_MASK;
    let mut payload_len: u64 = 0;
    let mut pos: u64 = 0;
    let mut out: u64 = 0;
    let mut top_down = false;
    let psize = odyssey_dma_draw_sentinel_bytes(hdr, &mut top_down);
    // APPROXIMATION: the window's height, for a top-down write's bottom-up y,
    // taken from the clip box's last row; the header carries no height. A
    // capture with a scissor box smaller than the window would tell this
    // rule from the board's own.
    let rows: i64 = (s.gl_clip_box[1] & ODSY_CLIP_BOX_FIELD_MASK) as i64 + 1;

    if psize == 0 {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_UPLOAD_SENTINEL) {
            crate::qlog!(
                "sgi-odyssey: DMA upload to sentinel {:#x} with format {:#x} and control {:#x} is not modeled",
                hdr[ODSY_DMA_HDR_DEST],
                hdr[ODSY_DMA_HDR_FORMAT],
                hdr[ODSY_DMA_HDR_CONTROL]
            );
        }
        return;
    }

    let n = odyssey_dma_entries(bus, table, seq, &mut e);
    if n < 0 {
        return;
    }
    let n = n as usize;
    for j in 0..n {
        payload_len += e[j].len as u64;
    }
    if w == 0
        || h == 0
        || rowbytes as u64 != (w as u64) * (psize as u64)
        || payload_len != (rowbytes as u64) * (h as u64)
    {
        crate::qlog!("sgi-odyssey: draw-sentinel DMA header at {:#x} disagrees with its payload", table);
        return;
    }

    let mut payload = vec![0u8; payload_len as usize];
    let mut i: usize = 0;
    let mut left: u32 = if n > 0 { e[0].len } else { 0 };
    for _r in 0..h {
        let mut got: u32 = 0;

        while got < rowbytes {
            if left == 0 {
                i += 1;
                pos = 0;
                left = e[i].len;
            }
            let take = left.min(rowbytes - got);
            let at = (out + got as u64) as usize;
            if pos > u64::MAX - e[i].addr
                || !sgi_dma_read(bus, e[i].addr + pos, &mut payload[at..at + take as usize], true, seq)
            {
                return;
            }
            got += take;
            pos += take as u64;
            left -= take;
        }
        if left != 0 {
            pos += skip as u64;
        }
        out += rowbytes as u64;
    }

    for r in 0..h {
        for c in 0..w {
            let off = ((r as u64) * (rowbytes as u64) + (c as u64) * (psize as u64)) as usize;
            let src = &payload[off..];
            let px = (x as i64) + (c as i64) + s.win_origin[0] as i64;
            let py = (if top_down {
                rows - (y as i64) - (h as i64) + (r as i64)
            } else {
                (y as i64) + (h as i64) - 1 - (r as i64)
            }) + s.win_origin[1] as i64;
            let mut rgba: u32;

            if psize == 4 {
                rgba = (odyssey_pack_pixel(src[3] as u32, src[2] as u32, src[1] as u32) & !0xffu32) | src[0] as u32;
            } else {
                let red = lduw_be_p(src);
                let green = lduw_be_p(&src[2..]);
                let blue = lduw_be_p(&src[4..]);
                let alpha = lduw_be_p(&src[6..]);

                rgba = odyssey_pack_pixel(
                    (red * 255 + 65535 / 2) / 65535,
                    (green * 255 + 65535 / 2) / 65535,
                    (blue * 255 + 65535 / 2) / 65535,
                );
                rgba = (rgba & !0xffu32) | ((alpha * 255 + 65535 / 2) / 65535);
            }
            odyssey_paint_pixel(s, px, py, rgba);
        }
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// True the first time reason (ODSY_DMA_REPORT_*) comes up: report it then.
pub fn odyssey_dma_report(s: &mut SGIOdysseyState, reason: u32) -> bool {
    if s.dma_read_reported & reason != 0 {
        return false;
    }
    s.dma_read_reported |= reason;
    true
}

const fn lay(code: u32, bytes: u8, from: [u8; 8]) -> OdysseyDMALayout {
    OdysseyDMALayout { code: code as u8, bytes, from }
}

const BZ: u8 = ODSY_DMA_BYTE_ZERO as u8;
const BI: u8 = ODSY_DMA_BYTE_INDEX as u8;

/// GL reads from a 12-bit-a-channel RGBA window (source class 3), by transfer
/// code. The model keeps eight bits a channel, so a byte read returns them as
/// stored and a 16-bit read replicates each byte. APPROXIMATION: how the
/// board narrows a 12-bit channel to eight bits is unknown.
pub static odyssey_dma_gl_layouts: [OdysseyDMALayout; 5] = [
    lay(1, 4, [0, 1, 2, 3, 0, 0, 0, 0]),  // RGBA, UNSIGNED_BYTE: R, G, B, A
    lay(6, 4, [3, 2, 1, 0, 0, 0, 0, 0]),  // A, B, G, R
    lay(2, 3, [0, 1, 2, 0, 0, 0, 0, 0]),  // RGB, UNSIGNED_BYTE
    lay(7, 3, [2, 1, 0, 0, 0, 0, 0, 0]),  // BGR, UNSIGNED_BYTE
    lay(9, 8, [0, 0, 1, 1, 2, 2, 3, 3]),  // RGBA, UNSIGNED_SHORT
];

/// An eight-bit RGBA visual uses code 8 for RGBA/UNSIGNED_BYTE.
pub static odyssey_dma_gl_rgba8_layout: OdysseyDMALayout = lay(8, 4, [0, 1, 2, 3, 0, 0, 0, 0]);

/// X reads, by word 7's format byte: the upload decode run backwards. The
/// luminance code is not invertible and no read of it has been seen.
pub static odyssey_dma_x_layouts: [OdysseyDMALayout; 4] = [
    lay(ODSY_IMAGE_CODE_ABGR, 4, [BZ, 2, 1, 0, 0, 0, 0, 0]),
    lay(ODSY_IMAGE_CODE_RGBA, 4, [0, 1, 2, 3, 0, 0, 0, 0]),
    lay(ODSY_IMAGE_CODE_RGB, 3, [0, 1, 2, 0, 0, 0, 0, 0]),
    lay(ODSY_IMAGE_CODE_CI, 1, [BI, 0, 0, 0, 0, 0, 0, 0]),
];

/// The read's layout, or None (reported once) when it is not modeled.
pub fn odyssey_dma_read_layout(s: &mut SGIOdysseyState, hdr: &[u32], bpp: u32) -> Option<&'static OdysseyDMALayout> {
    let xcode = hdr[ODSY_DMA_HDR_CONTROL] & ODSY_IMAGE_CODE_MASK;
    let fmt = hdr[ODSY_DMA_HDR_FORMAT];
    let cls = fmt & ODSY_DMA_FMT_CLASS_MASK;
    let code = (fmt >> ODSY_DMA_FMT_CODE_SHIFT) & ODSY_DMA_FMT_CODE_MASK;
    let mut l: Option<&'static OdysseyDMALayout> = None;

    if xcode != 0 {
        for t in odyssey_dma_x_layouts.iter() {
            if t.code as u32 == xcode {
                l = Some(t);
            }
        }
    } else if cls == ODSY_DMA_GL_CLASS_RGBA12 {
        for t in odyssey_dma_gl_layouts.iter() {
            if t.code as u32 == code {
                l = Some(t);
            }
        }
    } else if cls == ODSY_DMA_GL_CLASS_RGBA8 && code == 8 {
        l = Some(&odyssey_dma_gl_rgba8_layout);
    }
    if let Some(t) = l {
        if t.bytes as u32 == bpp {
            return Some(t);
        }
    }
    if odyssey_dma_report(s, ODSY_DMA_REPORT_LAYOUT) {
        crate::qlog!(
            "sgi-odyssey: DMA read with format byte {:#x}, word 4 {:#x} (class {}, code {}) at {} bytes a pixel is not modeled",
            xcode, fmt, cls, code, bpp
        );
    }
    None
}

/// Resolve word 0 to the read's source. False, reported once, when it names
/// nothing the model can read. A sentinel never creates or names a
/// board-memory object.
pub fn odyssey_dma_read_source(s: &mut SGIOdysseyState, hdr: &[u32], src: &mut OdysseyDMASource) -> bool {
    let base = hdr[ODSY_DMA_HDR_DEST];
    let fmt = hdr[ODSY_DMA_HDR_FORMAT];
    let selector = (fmt >> ODSY_DMA_FMT_SELECTOR_SHIFT) & ODSY_DMA_FMT_SELECTOR_MASK;

    src.obj = -1;
    // Front and back per pixel (odyssey_dma_read_pixel()).
    src.front = true;
    src.x = (hdr[ODSY_DMA_HDR_ORIGIN] & 0xffff) as i64;
    src.y = (hdr[ODSY_DMA_HDR_ORIGIN] >> 16) as i64;

    if base & ODSY_DMA_SENTINEL != 0 {
        if base == ODSY_DMA_SENTINEL_DRAW && selector == ODSY_DMA_FMT_SELECTOR_DRAW {
            if odyssey_dma_report(s, ODSY_DMA_REPORT_DRAW_SENTINEL) {
                crate::qlog!(
                    "sgi-odyssey: DMA read of the draw drawable ({:#x}); never captured, read as the read drawable is",
                    base
                );
            }
        } else if base != ODSY_DMA_SENTINEL_READ || selector != ODSY_DMA_FMT_SELECTOR_READ {
            if odyssey_dma_report(s, ODSY_DMA_REPORT_SENTINEL) {
                crate::qlog!("sgi-odyssey: DMA read of sentinel {:#x} with word 4 {:#x} is not modeled", base, fmt);
            }
            return false;
        }
        // INFERRED: both sentinels are the current window, whose origin the
        // model tracks; it keeps no separate read and draw drawable.
        src.x += s.win_origin[0] as i64;
        src.y += s.win_origin[1] as i64;
        src.front = fmt & ODSY_DMA_FMT_FRONT != 0;
        return true;
    }
    if base != 0 && base == s.dma_colour_base {
        return true;
    }
    src.obj = if base != 0 { odyssey_object_find(s, base) } else { -1 };
    if src.obj < 0 {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_SOURCE) {
            crate::qlog!("sgi-odyssey: DMA read of board base {:#x}, which holds no buffer", base);
        }
        return false;
    }
    true
}

/// Source pixel (x, y) as the plane's 0xRRGGBBAA word, or a raw index. A
/// colour-buffer pixel comes from the buffer its main drawable shows, or from
/// the other buffer for a back read. (x, y) is inside the plane.
pub fn odyssey_dma_read_pixel(s: &mut SGIOdysseyState, src: &OdysseyDMASource, x: i64, y: i64) -> u32 {
    if src.obj >= 0 {
        return odyssey_object_load(s, src.obj, x as u32, y as u32);
    }
    let id = odyssey_main_id(s, x as u32, y as u32);
    let shown = s.front.lock().displayed_buffer.checked_shr(id).unwrap_or(0) & 1;
    odyssey_buffer_load(s, if src.front { shown } else { (shown == 0) as u32 }, x, y)
}

/// Map piece p at guest address addr: an entry is one physical extent, so
/// the pages it runs over are consecutive.
pub fn odyssey_dma_map_piece(bus: &SGIDMABus, p: &mut OdysseyDMAPiece, addr: u64) -> i32 {
    let first = addr & !(ODSY_DMA_PAGE_SIZE - 1);
    let npages = (addr - first + p.len as u64).div_ceil(ODSY_DMA_PAGE_SIZE) as u32;
    let mut pages = vec![0u64; npages as usize];
    assert!(npages as usize <= ODSY_DMA_PIECE_PAGES);
    for (i, pg) in pages.iter_mut().enumerate() {
        *pg = first + (i as u64) * ODSY_DMA_PAGE_SIZE;
    }
    let pl = SGIDMAPageList { addr: pages, npages, page_size: ODSY_DMA_PAGE_SIZE as u32 };
    p.at = addr - first;
    sgi_dma_map(bus, &mut p.map, &pl, p.at, p.len as u64, DMA_DIRECTION_FROM_DEVICE, false, 0)
}

/// Pack the read's rectangle into out, host row by host row, rowbytes each.
/// A colour-index byte is the plane word itself, taken only where scanout
/// would take that word as an index: a colour-buffer pixel whose main
/// drawable's display mode is an index group, with a word that fits eight
/// bits. Anything else refuses the whole read, reported once. A colour
/// layout over an index-mode pixel returns the raw index word's bytes as
/// colour and is reported once: no inverse colormap is applied.
pub fn odyssey_dma_read_pack(s: &mut SGIOdysseyState, src: &OdysseyDMASource, l: &OdysseyDMALayout, w: u32, h: u32, bottom_up: bool, out: &mut [u8]) -> bool {
    let index_read = l.from[0] as u32 == ODSY_DMA_BYTE_INDEX;
    let mut o: usize = 0;

    if index_read && src.obj >= 0 {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_INDEX_OBJECT) {
            crate::qlog!(
                "sgi-odyssey: DMA index read of board object {:#x}, which records no display mode",
                s.obj_base[src.obj as usize]
            );
        }
        return false;
    }
    for r in 0..h {
        let board_row = if bottom_up { h - 1 - r } else { r };

        for c in 0..w {
            let x = src.x + c as i64;
            let y = src.y + board_row as i64;
            let word = odyssey_dma_read_pixel(s, src, x, y);
            let mut group = ODSY_GAMMA_GROUP;

            if src.obj < 0 {
                let id = odyssey_main_id(s, x as u32, y as u32);
                group = odyssey_display_group(s, id);
            }
            if index_read && (group >= ODSY_GAMMA_GROUP || word > 0xff) {
                if odyssey_dma_report(s, ODSY_DMA_REPORT_INDEX) {
                    crate::qlog!(
                        "sgi-odyssey: DMA index read at ({}, {}) finds word {:#x} in display-mode group {}, not an eight-bit index",
                        x, y, word, group
                    );
                }
                return false;
            }
            if !index_read && group < ODSY_GAMMA_GROUP && odyssey_dma_report(s, ODSY_DMA_REPORT_COLOUR_OVER_INDEX) {
                crate::qlog!(
                    "sgi-odyssey: DMA colour read at ({}, {}) finds an index pixel (display-mode group {}); its word {:#x} is returned as colour",
                    x, y, group, word
                );
            }
            for b in 0..l.bytes as usize {
                let from = l.from[b] as u32;

                out[o] = if from == ODSY_DMA_BYTE_INDEX {
                    word as u8
                } else if from == ODSY_DMA_BYTE_ZERO {
                    0
                } else {
                    (word >> (24 - 8 * from)) as u8
                };
                o += 1;
            }
        }
    }
    true
}

/// Write the packed payload into the table's entries: rowbytes bytes a host
/// row, moving to the next entry when one is used up, then skipping skip
/// uncounted bytes, exactly as an upload reads its source. Every piece is
/// mapped before any byte is written, so a table that fails to map writes
/// nothing. No byte outside the pieces is touched.
pub fn odyssey_dma_read_write(s: &mut SGIOdysseyState, bus: &SGIDMABus, e: &[OdysseyDMAEntry], rowbytes: u32, h: u32, skip: u32, payload: &mut [u8]) -> bool {
    let n = e.len();
    let mut pieces: Vec<OdysseyDMAPiece> = Vec::new();
    let mut pos: u64 = 0;
    let mut done: u64 = 0;
    let mut left: u32 = if n > 0 { e[0].len } else { 0 };
    let mut ok = true;
    let mut i: usize = 0;

    let mut r: u32 = 0;
    while r < h && ok {
        let mut got: u32 = 0;

        while got < rowbytes {
            let mut p = OdysseyDMAPiece::default();

            if left == 0 {
                // Checked by the caller: the entries hold exactly h rows.
                i += 1;
                pos = 0;
                left = e[i].len;
            }
            p.len = left.min(rowbytes - got);
            p.src = done;
            let plen = p.len;
            pieces.push(p);
            if odyssey_dma_map_piece(bus, pieces.last_mut().unwrap(), e[i].addr + pos) != 0 {
                ok = false;
                break;
            }
            got += plen;
            done += plen as u64;
            pos += plen as u64;
            left -= plen;
        }
        if left != 0 {
            pos += skip as u64;
        }
        r += 1;
    }
    for p in pieces.iter_mut() {
        if ok {
            let a = p.src as usize;
            if !sgi_dma_map_copy(&mut p.map, p.at, &mut payload[a..a + p.len as usize], true) {
                ok = false;
            }
        }
        sgi_dma_unmap(&mut p.map);
    }
    if !ok && odyssey_dma_report(s, ODSY_DMA_REPORT_WRITE) {
        crate::qlog!("sgi-odyssey: DMA read's host pages could not all be written");
    }
    ok
}

/// A board-to-host transfer (word 7 bit 24): read the header's rectangle from
/// its source and write it, laid out as the header says, into the table's
/// entries. Any refusal is reported once and writes nothing to the guest; the
/// caller still raises DMA-done when bit 31 asks for it. The payload is in
/// guest memory before this returns.
pub fn odyssey_dma_from_board(s: &mut SGIOdysseyState, bus: &SGIDMABus, table: u64, seq: u64, hdr: &[u32]) {
    let mut e = [OdysseyDMAEntry::default(); ODSY_DMA_MAX_PAGES];
    let w = hdr[ODSY_DMA_HDR_SIZE] & 0xffff;
    let h = if hdr[ODSY_DMA_HDR_SIZE] >> 16 != 0 { hdr[ODSY_DMA_HDR_SIZE] >> 16 } else { 0x10000 };
    let skip = hdr[ODSY_DMA_HDR_SKIP];
    let rowbytes = hdr[ODSY_DMA_HDR_ROWBYTES] & ODSY_DMA_ROWBYTES_MASK;
    let bpp = odyssey_dma_bpp[((hdr[ODSY_DMA_HDR_CONTROL] >> ODSY_DMA_BPP_SHIFT) & ODSY_DMA_BPP_MASK) as usize] as u32;
    let mut src = OdysseyDMASource::default();
    let mut total: u64 = 0;

    let raw = odyssey_dma_raw(hdr, bpp);

    if odyssey_dma_raw64(s, hdr, bpp) {
        return;
    }
    let l = if raw { None } else { odyssey_dma_read_layout(s, hdr, bpp) };
    if (!raw && l.is_none()) || !odyssey_dma_read_source(s, hdr, &mut src) {
        return;
    }
    if raw && src.obj < 0 {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_SOURCE) {
            crate::qlog!(
                "sgi-odyssey: raw DMA read of {:#x}, which is not a board-memory object, is not modeled",
                hdr[ODSY_DMA_HDR_DEST]
            );
        }
        return;
    }
    let n = odyssey_dma_entries(bus, table, seq, &mut e);
    if n < 0 {
        return;
    }
    let n = n as usize;
    for k in 0..n {
        total += e[k].len as u64;
    }
    if rowbytes as u64 != (w as u64) * (bpp as u64) || total != (rowbytes as u64) * (h as u64) {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_SHAPE) {
            crate::qlog!(
                "sgi-odyssey: DMA read header at {:#x} disagrees with itself: {}x{} at {} bytes a pixel, {} bytes a row, {} bytes in its entries",
                table, w, h, bpp, rowbytes, total
            );
        }
        return;
    }
    let inside = if src.obj >= 0 {
        // Rows past an object's store were never written: the model holds no
        // value for them, so a read reaching them is refused, not zero-filled.
        src.x + (w as i64) <= s.obj_width[src.obj as usize] as i64
            && src.y + (h as i64) <= s.obj_rows[src.obj as usize] as i64
    } else {
        src.x >= 0
            && src.y >= 0
            && src.x + (w as i64) <= SGI_ODYSSEY_WIDTH as i64
            && src.y + (h as i64) <= SGI_ODYSSEY_HEIGHT as i64
    };
    if !inside {
        if odyssey_dma_report(s, ODSY_DMA_REPORT_BOUNDS) {
            crate::qlog!(
                "sgi-odyssey: DMA read of {}x{} at ({}, {}) from {:#x} leaves its source",
                w, h, src.x, src.y, hdr[ODSY_DMA_HDR_DEST]
            );
        }
        return;
    }
    let mut payload = vec![0u8; total as usize];
    if raw {
        for r in 0..h {
            for c in 0..w {
                let word = odyssey_object_load(s, src.obj, (src.x + c as i64) as u32, (src.y + r as i64) as u32);
                let off = ((r as u64) * (rowbytes as u64) + (c as u64) * (bpp as u64)) as usize;
                odyssey_raw_pack(word, bpp, &mut payload[off..]);
            }
        }
        if odyssey_dma_read_write(s, bus, &e[..n], rowbytes, h, skip, &mut payload) {
            crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_dma_read: seq {} source {:#x} at {},{} {}x{} wrote {} bytes", seq, hdr[ODSY_DMA_HDR_DEST], src.x, src.y, w, h, total);
        }
        return;
    }
    let l = l.unwrap();
    if !odyssey_dma_read_pack(s, &src, l, w, h, hdr[ODSY_DMA_HDR_CONTROL] & ODSY_DMA_BOTTOM_UP != 0, &mut payload) {
        return;
    }
    if !odyssey_dma_read_write(s, bus, &e[..n], rowbytes, h, skip, &mut payload) {
        return;
    }
    crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_dma_read: seq {} source {:#x} at {},{} {}x{} wrote {} bytes", seq, hdr[ODSY_DMA_HDR_DEST], src.x, src.y, w, h, total);
}

/// A 0xe01/0xe03 descriptor: read the table's header, move the pixels, and
/// raise the DMA-done interrupt when the header asks for it. A header with
/// word 7 bit 24 set is a read; any other names its destination in word 0:
/// the colour buffer the screen shows, or an off-screen object whose row
/// length is word 2's (16-pixel units), created when no select named it
/// first. A zero word 0 names no buffer and is refused. An upload to a
/// sentinel (GL's DMA image write) is accepted only by the draw-sentinel
/// adapter. The 0xe03 opcode is reported and not performed.
pub fn odyssey_dma_descriptor(s: &mut SGIOdysseyState, readback: bool) {
    let table = odyssey_dma_table_address(s.cmd_operand[0], s.cmd_operand[1]);
    s.dma_seq += 1;
    let seq = s.dma_seq;
    let bus = SGIDMABus {
        as_: address_space_memory(),
        ops: P::from_ref(&odyssey_head_ops),
        opaque: P::new(s).opaque(),
    };
    let mut raw = [0u8; ODSY_DMA_TABLE_HEADER as usize];
    let mut hdr = [0u32; ODSY_DMA_HDR_WORDS];

    if !sgi_dma_read(&bus, table, &mut raw, true, seq) {
        return;
    }
    for i in 0..ODSY_DMA_HDR_WORDS {
        hdr[i] = ldl_be_p(&raw[i * 4..]);
    }
    if readback {
        if !s.dma_readback_reported {
            s.dma_readback_reported = true;
            crate::qlog!("sgi-odyssey: DMA opcode 0xe03 (buffer {:#x}) is not modeled", hdr[ODSY_DMA_HDR_DEST]);
        }
    } else if hdr[ODSY_DMA_HDR_CONTROL] & ODSY_DMA_TO_HOST != 0 {
        odyssey_dma_from_board(s, &bus, table, seq, &hdr);
    } else if hdr[ODSY_DMA_HDR_DEST] & ODSY_DMA_SENTINEL != 0 {
        if hdr[ODSY_DMA_HDR_DEST] == ODSY_DMA_SENTINEL_DRAW {
            odyssey_dma_draw_sentinel_short(s, &bus, table, seq, &hdr);
        } else if odyssey_dma_report(s, ODSY_DMA_REPORT_UPLOAD_SENTINEL) {
            crate::qlog!(
                "sgi-odyssey: DMA upload to sentinel {:#x} with word 4 {:#x} is not modeled",
                hdr[ODSY_DMA_HDR_DEST],
                hdr[ODSY_DMA_HDR_FORMAT]
            );
        }
    } else if s.dma_colour_base != 0 && hdr[ODSY_DMA_HDR_DEST] == s.dma_colour_base {
        odyssey_dma_to_buffer(s, &bus, table, seq, &hdr, -1);
    } else if hdr[ODSY_DMA_HDR_DEST] == 0 {
        crate::qlog!("sgi-odyssey: DMA header at {:#x} names no destination buffer", table);
    } else {
        // The header's row length is the object's, as a select's.
        let obj = odyssey_object_select(
            s,
            hdr[ODSY_DMA_HDR_DEST],
            hdr[ODSY_DMA_HDR_WIDTH].wrapping_mul(ODSY_DEST_WIDTH_UNIT),
        );

        if obj >= 0 {
            odyssey_dma_to_buffer(s, &bus, table, seq, &hdr, obj);
        }
    }
    if hdr[ODSY_DMA_HDR_CONTROL] & ODSY_DMA_IRQ_REQUEST != 0 {
        odyssey_dma_done(s);
    }
}

// The raster-position query: the library sends 0x4c0 + kind with the same
// count, fence and sequence as the current-value query (kind 0); kind 3 is
// glRasterPos's update. After it completes the library copies the record at
// query base + 0x93c into the raster colour (RGBA context) or the one at
// + 0x928 (colour-index context), and the record at + 0x950 into the raster
// texture coordinate.
//
// APPROXIMATION: the board's raster colour is the current colour after
// lighting; this model answers with the current colour and index records
// unlit, and the current texture coordinate untransformed, beside the kind-0
// records. Lit raster colours and kinds 1 and 2 are not modeled.

pub static odyssey_query_offset: [u32; 5] = [0x900, 0x914, 0x928, 0x950, 0x978];

/// Public initial values: index 1, colour white, normal +Z, edge true.
pub static odyssey_query_defaults: [[u32; 5]; 5] = [
    [ODSY_GL_COLOR_INDEX, 1, 0, 0, 0],
    [ODSY_GL_COLOR_3F, ODSY_F32_ONE, ODSY_F32_ONE, ODSY_F32_ONE, 0],
    [ODSY_GL_NORMAL_3F, 0, 0, ODSY_F32_ONE, 0],
    [ODSY_GL_TEXCOORD_2F, 0, 0, 0, 0],
    [0x2f00c02e, 0, 0, 0, 0],
];

pub fn odyssey_query_records_reset(s: &mut SGIOdysseyState) {
    s.query_current = odyssey_query_defaults;
    s.query_current_valid = 0x1f;
}

pub fn odyssey_query_cancel(s: &mut SGIOdysseyState) {
    if s.query_stage != 0 {
        crate::qlog!("sgi-odyssey: interrupted current-query sequence");
    }
    s.query_epoch += 1;
    s.query_stage = 0;
    s.query_count = 0;
    s.query_sync_index = 0;
    s.query_kind = 0;
    s.query_base = 0;
    s.query_sync_base = 0;
    s.query_captured_epoch = 0;
    s.query_captured_valid = 0;
    s.query_captured = [[0; 5]; 5];
    s.cmd_state = ODSY_CMD_IDLE;
    s.cmd_tag = 0;
    s.cmd_operand_count = 0;
    s.cmd_operand_index = 0;
    s.cmd_operand = [0; SGI_ODYSSEY_CMD_OPERANDS as usize];
}

/// Retain a completed setter in its public record: the whole command word
/// and its raw operands, never a converted value. The 0xce80 twins of the
/// index and colour words set the same records as their 0xc580 siblings (a
/// model choice). A byte colour operand above 255 has no defined answer: the
/// record is retained for the context save but makes queries fail
/// observably until the next colour setter. The current texture record
/// admits the captured two- and four-float setters; other formats stay
/// unsupported.
pub fn odyssey_query_update(s: &mut SGIOdysseyState, tag: u32) {
    let record: u32;
    let mut supported = true;

    match tag {
        ODSY_GL_COLOR_INDEX | ODSY_GL_REPLAY_INDEX | ODSY_GL_COLOR_INDEX_F | ODSY_GL_REPLAY_INDEX_F => {
            record = ODSY_QUERY_INDEX;
        }
        ODSY_GL_COLOR_3UB | ODSY_MODE_INDEX_ZEROS => {
            record = ODSY_QUERY_COLOUR;
            for i in 0..3 {
                supported &= s.cmd_operand[i] <= 0xff;
            }
        }
        ODSY_GL_COLOR_4UB => {
            record = ODSY_QUERY_COLOUR;
            for i in 0..4 {
                supported &= s.cmd_operand[i] <= 0xff;
            }
        }
        ODSY_GL_COLOR_3I
        | ODSY_GL_INDEX_MODE_COLOR_3I
        | ODSY_GL_COLOR_3F
        | ODSY_GL_INDEX_MODE_COLOR_3F
        | ODSY_GL_COLOR_4F
        | ODSY_GL_INDEX_MODE_COLOR_4F
        | ODSY_GL_COLOR_3US
        | ODSY_GL_COLOR_3UI
        | ODSY_GL_COLOR_3B
        | ODSY_GL_COLOR_3S
        | ODSY_GL_COLOR_4US
        | ODSY_GL_COLOR_4UI
        | ODSY_GL_COLOR_4B
        | ODSY_GL_COLOR_4S
        | ODSY_GL_COLOR_4I => {
            record = ODSY_QUERY_COLOUR;
        }
        ODSY_GL_NORMAL_3B | ODSY_GL_NORMAL_3S | ODSY_GL_NORMAL_3I | ODSY_GL_NORMAL_3F => {
            record = ODSY_QUERY_NORMAL;
        }
        ODSY_GL_TEXCOORD_1F | ODSY_GL_TEXCOORD_2F | ODSY_GL_TEXCOORD_3F | ODSY_GL_TEXCOORD_4F => {
            record = ODSY_QUERY_TEXCOORD;
        }
        0x2f00c02e | 0x3000c02e => {
            record = ODSY_QUERY_EDGE;
        }
        // Kept raw like the float forms: the kernel replays the record
        // through the command FIFO at a context restore.
        ODSY_GL_TEXCOORD_1I | ODSY_GL_TEXCOORD_2I | ODSY_GL_TEXCOORD_3I | ODSY_GL_TEXCOORD_4I => {
            record = ODSY_QUERY_TEXCOORD;
        }
        _ => {
            if (tag & ODSY_GL_TEXCOORD_SHAPE) != ODSY_GL_TEXCOORD_TAG {
                return;
            }
            record = ODSY_QUERY_TEXCOORD;
            supported = false;
        }
    }
    let rec = record as usize;
    s.query_current_valid &= !(1u32 << record);
    if record != ODSY_QUERY_TEXCOORD || supported {
        // Every word mapped to a record above has at most four operands.
        let arity = odyssey_cmd_arity(tag) as usize;
        assert!(arity < s.query_current[rec].len());
        s.query_current[rec] = [0; 5];
        s.query_current[rec][0] = tag;
        for k in 0..arity {
            s.query_current[rec][1 + k] = s.cmd_operand[k];
        }
    }
    if supported {
        s.query_current_valid |= 1u32 << record;
    } else {
        crate::qlog!("sgi-odyssey: unsupported current-query record {} tag {:#010x}", record, tag);
    }
}

/// A query's completion words must not report failure after changing guest
/// memory. Admit only a whole aligned scalar, big-endian. Returns 0 or an
/// SGIFailure. (The QEMU model also demanded a direct writable RAM section;
/// here guest memory is written through the bus.)
pub fn odyssey_publish_word(bus: &SGIDMABus, addr: u64, value: u32, size: u32) -> i32 {
    let size64 = size as u64;
    let mut bus_addr: u64 = addr;

    if (addr & (size64 - 1)) != 0 || addr > u64::MAX - size64 {
        crate::qlog!("sgi-odyssey: invalid publication address {:#x}", addr);
        return SGI_FAIL_BOUNDS as i32;
    }
    if let Some(ops) = bus.ops.as_opt() {
        if let Some(dma_address) = ops.dma_address {
            if !dma_address(bus.opaque, addr, &mut bus_addr) {
                crate::qlog!("sgi-odyssey: publication address {:#x} cannot be composed", addr);
                return SGI_FAIL_BUS as i32;
            }
        }
    }
    if (bus_addr & (size64 - 1)) != 0 || bus_addr > u64::MAX - size64 {
        crate::qlog!("sgi-odyssey: invalid bus address {:#x}", bus_addr);
        return SGI_FAIL_BOUNDS as i32;
    }
    let r = if size == 2 {
        address_space_write(&bus.as_, bus_addr, &(value as u16).to_be_bytes())
    } else {
        address_space_write(&bus.as_, bus_addr, &value.to_be_bytes())
    };
    if r != MEMTX_OK {
        crate::qlog!("sgi-odyssey: publication at {:#x} is not aligned writable RAM", bus_addr);
        return SGI_FAIL_BUS as i32;
    }
    0
}

pub fn sgi_odyssey_publish_be16(bus: &SGIDMABus, addr: u64, value: u16) -> i32 {
    odyssey_publish_word(bus, addr, value as u32, 2)
}

pub fn sgi_odyssey_publish_be32(bus: &SGIDMABus, addr: u64, value: u32) -> i32 {
    odyssey_publish_word(bus, addr, value, 4)
}

pub fn odyssey_query_begin(s: &mut SGIOdysseyState) {
    s.query_stage = 1;
    s.query_base = (((s.ctx_shadow_addr_upper & 0xffff) as u64) << 32) | s.ctx_shadow_addr_lower as u64;
    s.query_sync_base = (((s.host_write_addr_upper & 0xffff) as u64) << 32) | s.host_write_addr_lower as u64;
    s.query_captured_epoch = s.query_epoch;
    s.query_captured_valid = s.query_current_valid;
    s.query_captured = s.query_current;
}

pub fn odyssey_query_finish(s: &mut SGIOdysseyState, sequence: u32) {
    let bus = SGIDMABus {
        as_: address_space_memory(),
        ops: P::from_ref(&odyssey_head_ops),
        opaque: P::new(s).opaque(),
    };
    let sync_offset: u64 = 2 * (s.query_sync_index & ODSY_DMA_FENCE_SLOT_MASK) as u64;

    s.query_stage = 0;
    for i in 0..odyssey_query_offset.len() {
        if s.query_captured_valid & (1u32 << i) == 0 {
            crate::qlog!("sgi-odyssey: unsupported current-query record {} in snapshot", i);
        }
    }
    if s.query_base == 0
        || s.query_base > u64::MAX - 0x980
        || s.query_captured_epoch != s.query_epoch
        || s.query_captured_valid != 0x1f
    {
        crate::qlog!("sgi-odyssey: unsupported current-query snapshot");
        return;
    }
    for i in 0..odyssey_query_offset.len() {
        let words = 1 + odyssey_cmd_arity(s.query_captured[i][0]) as usize;

        for j in 0..words {
            let addr = s.query_base + odyssey_query_offset[i] as u64 + 4 * j as u64;

            if sgi_odyssey_publish_be32(&bus, addr, s.query_captured[i][j]) != 0 {
                crate::qlog!("sgi-odyssey: current-query payload failed");
                return;
            }
        }
    }
    let mut i = 0;
    while s.query_kind == ODSY_QUERY_KIND_RASTER && i < 2 {
        let rec = s.query_captured[if i != 0 { ODSY_QUERY_COLOUR } else { ODSY_QUERY_INDEX } as usize];
        let at = s.query_base + (if i != 0 { ODSY_QUERY_RASTER_COLOUR } else { ODSY_QUERY_RASTER_INDEX }) as u64;

        for j in 0..(1 + odyssey_cmd_arity(rec[0]) as usize) {
            if sgi_odyssey_publish_be32(&bus, at + 4 * j as u64, rec[j]) != 0 {
                crate::qlog!("sgi-odyssey: raster query payload failed");
                return;
            }
        }
        i += 1;
    }
    if sgi_odyssey_publish_be32(&bus, s.query_base + 0x97c, s.query_count) != 0 {
        crate::qlog!("sgi-odyssey: current-query counter failed");
        return;
    }
    if s.query_sync_base > u64::MAX - sync_offset
        || sgi_odyssey_publish_be16(&bus, s.query_sync_base + sync_offset, (sequence & 0xffff) as u16) != 0
    {
        crate::qlog!("sgi-odyssey: current-query sync failed");
        return;
    }
    if s.query_sync_index & ODSY_DMA_FENCE_INTERRUPT != 0 {
        odyssey_deliver_softsync(&mut s.front.lock());
    }
}

/// Match the shared rasterizer's eight-bit channel conversion.
pub fn odyssey_float_channel(bits: u32) -> u32 {
    let c = odyssey_f32(bits);

    if !(c > 0.0) {
        return 0;
    }
    if c >= 1.0 {
        return 255;
    }
    (c * 255.0 + 0.5) as u32
}

/// The GL library's conversion of a signed-integer colour or normal operand
/// to a float, in single precision as its query decoder does. The exact
/// scales are 1/255, 1/65535 and 1/(2^32 - 2304); ODSY_F32_SCALE_32 is the
/// single-precision quotient of the last, exactly 1/(2^32 - 2560). Formats
/// 22 and 23 form 2v + 1 in 32-bit integer arithmetic before converting; 13
/// and 24 convert v first, then form v * 2 + 1 with a fused multiply-add.
/// This is the software's expectation for what the current value means; the
/// board's own arithmetic is unknown.
pub fn odyssey_signed_to_float(word: u32, format: u32) -> u32 {
    let scale_bits: u32;
    let mut f: f32;

    match format {
        22 => {
            scale_bits = ODSY_F32_SCALE_8;
            f = word.wrapping_mul(2).wrapping_add(1) as i32 as f32;
        }
        23 => {
            scale_bits = ODSY_F32_SCALE_16;
            f = word.wrapping_mul(2).wrapping_add(1) as i32 as f32;
        }
        _ => {
            scale_bits = ODSY_F32_SCALE_32;
            f = (word as i32 as f32).mul_add(2.0, 1.0);
        }
    }
    f *= f32::from_bits(scale_bits);
    f.to_bits()
}

/// One component of an integer colour word as float bits. The operand is
/// taken to its type's width and converted by OpenGL's rule, c / (2^n - 1)
/// for an unsigned type and (2c + 1) / (2^n - 1) for a signed one; the int
/// type keeps glColor3i's existing conversion. INFERRED: that the board
/// converts by OpenGL's rule; the library sends the raw components.
pub fn odyssey_integer_colour(tag: u32, word: u32) -> u32 {
    let mut code = tag & ODSY_GL_COLOR_TYPE_MASK;
    let c: f64;

    code = code.wrapping_sub(if code >= ODSY_GL_COLOR_TYPE_4 { ODSY_GL_COLOR_TYPE_4 } else { ODSY_GL_COLOR_TYPE_3 });
    match code {
        0 => c = (word as u8) as f64 / 255.0,
        1 => c = (word as u16) as f64 / 65535.0,
        2 => c = word as f64 / 4294967295.0,
        3 => c = (2.0 * ((word as u8 as i8) as f64) + 1.0) / 255.0,
        4 => c = (2.0 * ((word as u16 as i16) as f64) + 1.0) / 65535.0,
        _ => return odyssey_signed_to_float(word, ODSY_GL_COLOR_3I & ODSY_GL_COLOR_TYPE_MASK),
    }
    odyssey_f32_bits(c as f32)
}

/// True for the integer colour words other than glColor3ub and glColor3i.
pub fn odyssey_is_integer_colour(word: u32) -> bool {
    matches!(
        word,
        ODSY_GL_COLOR_3US
            | ODSY_GL_COLOR_3UI
            | ODSY_GL_COLOR_3B
            | ODSY_GL_COLOR_3S
            | ODSY_GL_COLOR_4UB
            | ODSY_GL_COLOR_4US
            | ODSY_GL_COLOR_4UI
            | ODSY_GL_COLOR_4B
            | ODSY_GL_COLOR_4S
            | ODSY_GL_COLOR_4I
    )
}

/// True when base lies within the board memory the active product reports
/// (the "memory-size" property, in megabytes), false and reported, once per
/// reset, otherwise. A destination select's base is in 16-byte units, so the
/// capacity is converted into that unit first. A guest that addresses past
/// the reported size is a guest bug, kept observable.
pub fn odyssey_board_address_valid(s: &mut SGIOdysseyState, base: u32) -> bool {
    let limit: u64 = (s.memory_size as u64) * (1u64 << 20) / 16;

    if (base as u64) < limit {
        return true;
    }
    if !s.board_oob_reported {
        s.board_oob_reported = true;
        crate::qlog!("sgi-odyssey: destination base {:#x} is beyond the {} MiB this board reports", base, s.memory_size);
    }
    crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_board_oob: destination base {:#x} exceeds the {} MiB this board reports", base, s.memory_size);
    false
}

/// Build the next mipmap level from the level above by taking every second
/// texel (a model policy), for the one construction template the model has
/// seen: RGBA, RGB and luminance-alpha levels, with sampler word 4 and the
/// level entry's format field differing per component set. The store keeps
/// whole texel words whatever the level's format, so the copy is the same.
pub fn odyssey_texture_build_level(s: &mut SGIOdysseyState) -> bool {
    let p = s.cmd_operand;
    let entry = [s.tex_levels[60 * 3], s.tex_levels[60 * 3 + 1], s.tex_levels[60 * 3 + 2]];
    let ws = s.tex_sampler[1];
    let hs = s.tex_sampler[2];
    let format: u32;

    match s.tex_sampler[4] {
        0x13640000 => format = 0x41100000,
        0x12e40000 => format = 0x00c00000,
        0x11e40000 => format = 0x00b00000,
        _ => return false,
    }
    if ws == 0 || hs == 0 || ws > i32::MAX as u32 || hs > i32::MAX as u32 || !is_power_of_2(ws as u64) || !is_power_of_2(hs as u64) {
        return false;
    }
    let wd = (ws / 2).max(1);
    let hd = (hs / 2).max(1);
    let source_units = ((ws + 15) / 16 + 1) & !1u32;
    let dest_units = ((wd + 15) / 16 + 1) & !1u32;
    if s.gl_primitive != ODSY_GL_QUADS
        || (s.tex_mode & ODSY_TEX_MODE_2D) == 0
        || s.tex_level0 != 60
        || entry[1] != (format | source_units)
        || entry[2] != 0
        || s.tex_sampler[0] != ((ctz32(ws) << 10) | (ctz32(hs) << 5))
        || s.tex_sampler[3] != 0
        || s.tex_sampler[5] != 0
        || s.tex_sampler[6] != 0
        || p[0] != 0
        || p[1] != 0
        || p[2] != 0
        || p[3] != 0
        || p[4] != 0
        || p[5] != 0
        || p[6] != 0
        || p[7] != odyssey_f32_bits(wd as f32)
        || p[8] != odyssey_f32_bits(hd as f32)
        || p[9] != 0
        || p[10] != 0
        || p[11] != 0
        || p[12] != odyssey_f32_bits(1.0f32 - 0.5f32 / wd as f32)
        || p[13] != odyssey_f32_bits(1.0f32 - 0.5f32 / hd as f32)
    {
        return false;
    }
    let source_base = entry[0] & 0x1fffffff;
    if source_base == 0 || source_base == s.dest_base || s.obj_count > SGI_ODYSSEY_OBJECTS {
        return false;
    }
    let source = odyssey_object_find(s, source_base);
    let dest = odyssey_image_object(s);
    if source < 0 || dest < 0 {
        return false;
    }
    let (su, du) = (source as usize, dest as usize);
    if s.obj_width[su] != source_units * 16 || s.obj_rows[su] < hs || s.obj_width[du] != dest_units * 16 {
        return false;
    }
    let source_end = ((hs - 1) as u64)
        .checked_mul(s.obj_width[su] as u64)
        .and_then(|v| v.checked_add(s.obj_offset[su] as u64))
        .and_then(|v| v.checked_add(ws as u64));
    let Some(source_end) = source_end else { return false };
    if source_end > s.obj_used as u64 || source_end > (s.obj_mem.bytes_len / 4) as u64 || !odyssey_object_rows(s, dest, hd) {
        return false;
    }
    for y in 0..hd {
        for x in 0..wd {
            let v = odyssey_object_load(s, source, 2 * x, 2 * y);
            odyssey_object_store(s, dest, x, y, v);
        }
    }
    true
}
