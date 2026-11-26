//! Completion words and pixel DMA: the host-write fence the driver waits
//! on, and the page-table transfers that move pixels between guest memory
//! and the board.

use crate::dev::odyssey::field::{Codec, Field};
use crate::dev::odyssey::view::{Rows, View, MAX_DIM};
use crate::dev::odyssey::vram::Surface;
use crate::dev::odyssey::board::Board;
use crate::dev::odyssey::consts::{ctx, dma, host, reg, ODSY_USER_CMD_FIFO};
use crate::dev::odyssey::tests::{
    board_with_ram, cmd, cmd64, pixel, pixel_write, reg_write, route_to_heart, select, DMA_VECTOR,
    FLAG_VECTOR, Ram, SOFTSYNC_VECTOR,
};
use std::sync::Arc;
use super::*;

const FENCE_BASE: u64 = 0xb70000;
pub(super) const TABLE: u64 = 0xb80000;
pub(super) const PAGE_A: u64 = 0xb84000;
const PAGE_B: u64 = 0xb88000;
/// A 16-pixel-wide RGBA8 object the transfers create.
const OBJECT: u32 = 0x0016_a800;

/// The way the driver's descriptor builder packs one 24-bit half of a
/// table address: bits 16:0 stay, bits 23:17 move to 29:23.
fn pack_half(half: u64) -> u32 {
    (half & 0x1ffff) as u32 | ((half & 0xfe_0000) << 6) as u32
}

/// Push a two-doubleword DMA descriptor the way the driver builds it: the
/// opcode over the packed high half of the table address, then the packed
/// low half over a zero word.
pub(super) fn descriptor(s: &mut Board, table: u64) {
    cmd64(s, dma::DESC_UPLOAD, pack_half(table >> 24));
    cmd64(s, pack_half(table & 0xff_ffff), 0);
}

/// An entry of the translation table: a page address over a byte count.
pub(super) fn pte(addr: u64, len: u32) -> u64 {
    addr << dma::PTE_ADDR_SHIFT | len as u64
}

pub(super) fn put_table(ram: &Ram, table: u64, header: [u32; 8], entries: &[u64]) {
    let mut t = vec![0u8; dma::TABLE_HEADER as usize + 16 * 8];
    for (i, w) in header.iter().enumerate() {
        t[i * 4..][..4].copy_from_slice(&w.to_be_bytes());
    }
    for (i, e) in entries.iter().enumerate() {
        t[dma::TABLE_HEADER as usize + i * 8..][..8].copy_from_slice(&e.to_be_bytes());
    }
    ram.put(table, &t);
}

/// A header naming a 16-pixel-wide RGBA8 surface at `OBJECT`: origin
/// (0, 0), one tile a row, word 4 the library's for 4-byte RGBA8 (pixel
/// and field 4 bytes), `rows` rows of 64 bytes, 4 bytes a pixel, source
/// format 0x4b, and the DMA-done request when `irq` is set.
fn object_header(rows: u32, irq: bool) -> [u32; 8] {
    [OBJECT, 0, 1, rows << 16 | 16, 0x8001_3042, 0, 64, if irq { dma::IRQ_REQUEST } else { 0 } | 1 << 16 | 0x4b]
}

/// Pixel (x, y) of the surface at `base` (`stride` tiles, 1 << `log2`
/// bytes a pixel) as the model's word, its field packed as `codec`.
fn mem_word(s: &Board, base: u32, stride: u32, log2: u8, codec: Codec, x: i32, y: i32) -> u32 {
    s.gpu_sync();
    let surface = Surface::new(base, stride, log2);
    Rows::new(s.vram_ptr(), View::colour(&surface, Field { at: 0, codec }, MAX_DIM)).load_word(x, y).unwrap()
}

/// Word `i` of the object, 16 texels a row.
fn object_word(s: &mut Board, i: u32) -> u32 {
    mem_word(s, OBJECT, 1, 2, Codec::Texel { bytes: 4, field: 5 }, (i % 16) as i32, (i / 16) as i32)
}

/// The colour destination select the X server sends: the test screen's
/// pixel mode (Board::test_screen()).
pub(super) fn colour_destination(s: &mut Board) {
    for w in [select(reg::DEST_SELECT), 0x2000, 0x50, 0x0040_6d50, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
}

/// The pixel mode the library selects before it uploads to the object:
/// 4-byte pixels, one 4-byte RGBA8 field, a scratch surface.
fn object_destination(s: &mut Board) {
    for w in [select(reg::DEST_SELECT), OBJECT, 1, 0x0800_24d0, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
}

/// A board with the fence slots at `FENCE_BASE` and the interrupts routed.
fn fenced_board() -> (Box<Board>, Arc<Recorder>, Arc<Ram>) {
    let (mut s, rec, ram) = board_with_ram();
    reg_write(&mut s, host::WRITE_ADDR_UPPER, 0);
    reg_write(&mut s, host::WRITE_ADDR_LOWER, FENCE_BASE as u32);
    route_to_heart(&mut s);
    (s, rec, ram)
}

/// The host-write/sync command the driver's DMA completion wait uses: a
/// write of the command's 16-bit token into guest RAM at the host-write
/// base plus twice its slot number, and, only when the command's interrupt
/// bit is set, a one-shot softsync packet. The interrupt handler is the
/// only place that polls the halfword, so both effects are needed to
/// unblock the driver.
///
/// QEMU: command-fifo-host-write-sync.
#[test]
fn a_host_write_fence_lands_in_its_slot_and_interrupts_only_when_asked() {
    let (mut s, rec, ram) = fenced_board();
    assert_eq!(ram.half(FENCE_BASE + 4), 0);

    // Slot 2, no interrupt bit: the write lands, nothing is sent.
    for w in [select(reg::HOST_WRITE), 2, 0x5a00] {
        cmd(&mut s, w);
    }
    assert_eq!(ram.half(FENCE_BASE + 4), 0x5a00);
    assert!(rec.silent());

    // Slot 3, interrupt bit set: a second write and the softsync packet.
    for w in [select(reg::HOST_WRITE), dma::FENCE_INTERRUPT | 3, 0xa5a5] {
        cmd(&mut s, w);
    }
    assert_eq!(ram.half(FENCE_BASE + 6), 0xa5a5);
    assert_eq!(rec.events(SOFTSYNC_VECTOR), [true]);
    assert_eq!(rec.events(FLAG_VECTOR), Vec::<bool>::new());
}

/// The page-table pixel DMA engine: a scatter-gather buffer and its
/// translation table laid in guest RAM, a 0xe01 descriptor pushed, and the
/// whole buffer lands, across the page boundary, in the off-screen object
/// the header names, 16 texels a row. The table address is packed as the
/// driver packs it, with bits 23:17 set and a table above 16 MiB so the
/// high half is non-zero. The first entry starts part-way into its page.
/// The descriptor writes no fence and raises the DMA-done interrupt.
///
/// QEMU: command-fifo-dma-page-table.
#[test]
fn a_page_table_transfer_lands_across_pages_and_raises_dma_done() {
    let table_high = 0x1fe5300u64; // high half 1, low half 0xfe5300
    let first_off = 0x3000u64;
    let first_len = dma::PAGE_SIZE as u32 - first_off as u32;
    let page1_len = 0x40u32;
    let (mut s, rec, ram) = fenced_board();
    assert_eq!(pack_half(TABLE >> 24), 0);
    assert_eq!(pack_half(TABLE & 0xff_ffff), 0x2e00_0000);

    // A distinct big-endian word per pixel, so a misordered fetch shows.
    let buf0: Vec<u8> = (0..first_len / 4).flat_map(|i| (0xaa00_0000u32 | i).to_be_bytes()).collect();
    let buf1: Vec<u8> = (0..page1_len / 4).flat_map(|i| (0xbb00_0000u32 | i).to_be_bytes()).collect();
    ram.put(PAGE_A + first_off, &buf0);
    ram.put(PAGE_B, &buf1);
    put_table(
        &ram,
        TABLE,
        object_header((first_len + page1_len) / 64, true),
        &[pte(PAGE_A + first_off, first_len), pte(PAGE_B, page1_len)],
    );

    object_destination(&mut s);
    descriptor(&mut s, TABLE);

    let words = first_len / 4;
    assert_eq!(object_word(&mut s, 0), 0xaa00_0000);
    assert_eq!(object_word(&mut s, 1), 0xaa00_0001);
    assert_eq!(object_word(&mut s, words - 1), 0xaa00_0000 | (words - 1));
    assert_eq!(object_word(&mut s, words), 0xbb00_0000, "the first pixel of the second page");
    assert_eq!(object_word(&mut s, words + page1_len / 4 - 1), 0xbb00_0000 | (page1_len / 4 - 1));
    assert_eq!(object_word(&mut s, words + page1_len / 4), 0, "one texel past the transfer holds nothing");
    assert_eq!(ram.half(FENCE_BASE), 0, "no fence");
    assert!(rec.events(SOFTSYNC_VECTOR).is_empty());
    assert_eq!(rec.events(DMA_VECTOR), [true]);

    // A table above 16 MiB, one row: the high half reaches the decode.
    put_table(&ram, table_high, object_header(1, false), &[pte(PAGE_B, page1_len)]);
    descriptor(&mut s, table_high);
    assert_eq!(object_word(&mut s, 0), 0xbb00_0000);
    assert_eq!(object_word(&mut s, page1_len / 4 - 1), 0xbb00_0000 | (page1_len / 4 - 1));
}

/// An object upload takes the library's format and type pair (word 7 bits 4:0 and 8:5) as a write
/// to the draw sentinel does, not only the 8-bit source codes it already decodes: the pair gives
/// the texel from the bytes sent, whatever the pixel size, with a component the source lacks full
/// scale for alpha. odsy devices/buzz/pixel-paths.md, "Host-to-board table"
/// (`__glOdysseyImageCalcXferWriteParams`, established): the pair is in word 7 for texture classes
/// too; the host bytes a pixel follow the kernel's table. That the board converts the pair into
/// the texel is inferred from the names.
#[test]
fn an_object_upload_takes_the_librarys_format_and_type_pair() {
    // (format, type, pixel size code in word 7 bits 18:16, source bytes, texel)
    let words = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444].map(u32::to_be_bytes).concat();
    let cases: [(u32, u32, u32, &[u8], u32); 5] = [
        (0xb, 4, 3, &words, 0x1122_3344),
        (4, 2, 5, &[0x80], 0x8000_00ff),
        (9, 0xa, 0, &[0xf8, 0x10], 0xff00_84ff),
        (0xb, 8, 1, &[0xff, 0xc0, 0x08, 0x02], 0xff00_80aa),
        (0x11, 2, 1, &[0x33, 0x22, 0x11, 0x44], 0x1122_3344),
    ];
    for (i, (format, ty, size, bytes, texel)) in cases.into_iter().enumerate() {
        let (mut s, _, ram) = board_with_ram();
        ram.put(PAGE_A, bytes);
        let mut header = object_header(1, false);
        header[1] = i as u32;
        header[2] = 1;
        header[3] = 1 << 16 | 1;
        header[6] = bytes.len() as u32;
        header[7] = 0x3a00_0000 | size << 16 | ty << 5 | format;
        put_table(&ram, TABLE, header, &[pte(PAGE_A, bytes.len() as u32)]);
        object_destination(&mut s);
        descriptor(&mut s, TABLE);
        assert_eq!(object_word(&mut s, i as u32), texel, "format {format:#x} type {ty:#x}, {} bytes", bytes.len());
    }
}

/// A descriptor whose entries disagree with its header, one entry
/// claiming more bytes than the header's rectangle, fails the transfer
/// before anything lands. The board still consumed the descriptor, so the
/// DMA-done source the header asks for fires. Before the driver programs
/// the routing, a consumed descriptor sends nothing.
///
/// QEMU: command-fifo-dma-page-table (the failing and unrouted cases).
#[test]
fn a_transfer_that_disagrees_with_its_header_lands_nothing_but_still_completes() {
    let (mut s, rec, ram) = fenced_board();
    put_table(&ram, TABLE, object_header(1, true), &[pte(PAGE_A, 0xffff)]);
    ram.put(PAGE_A, &[0x11; 0x40]);

    object_destination(&mut s);
    descriptor(&mut s, TABLE);

    assert_eq!(rec.events(DMA_VECTOR), [true]);
    assert_eq!(object_word(&mut s, 0), 0);

    let (mut s, rec, ram) = board_with_ram();
    put_table(&ram, TABLE, object_header(1, true), &[pte(PAGE_A, 0xffff)]);
    descriptor(&mut s, TABLE);
    assert!(rec.silent(), "no destination is programmed");
}

/// The library uploads a texture, or the scratch image a zoomed
/// glDrawPixels draws from, while the window is still the pixel mode
/// (traced: pixtest zoombig, a 200x60 RGBA image to 0x16b800). An address
/// that is not the frame buffer the scan-out shows is memory: its texels
/// keep the source's alpha, where a put to the screen stores 255.
#[test]
fn an_upload_to_memory_the_select_does_not_name_keeps_its_alpha() {
    let (mut s, _, ram) = board_with_ram();
    ram.put(PAGE_A, &[0x11, 0x6c, 0xc7, 0x22].repeat(16));
    put_table(&ram, TABLE, object_header(1, false), &[pte(PAGE_A, 64)]);
    descriptor(&mut s, TABLE);
    assert_eq!(object_word(&mut s, 0), 0x116c_c722, "the window is selected; the object keeps its alpha");
}

/// One kick names one 128-byte work-list entry: the 32-byte header and up
/// to 12 page pairs, with no terminator when all twelve are used; the next
/// entry's words follow at once. Twelve one-row pairs fill the entry and
/// the 2 KiB after it are nonzero, as a following entry would be. The
/// transfer takes exactly the twelve rows.
///
/// QEMU: command-fifo-dma-full-entry.
#[test]
fn a_full_work_list_entry_takes_exactly_twelve_pages() {
    let (mut s, _, ram) = board_with_ram();
    let source = 0xb90000u64;
    let mut entries = Vec::new();
    for k in 0..12u64 {
        let row: Vec<u8> = (0..16u32).flat_map(|i| (0xcc00_0000u32 | (k as u32) << 8 | i).to_be_bytes()).collect();
        ram.put(source + k * dma::PAGE_SIZE, &row);
        entries.push(pte(source + k * dma::PAGE_SIZE, 64));
    }
    put_table(&ram, TABLE, object_header(12, false), &entries);
    // What would be the next entry: nonzero, so a missing terminator is noticed.
    let after = TABLE + dma::TABLE_HEADER + 12 * 8;
    ram.put(after, &[0xff; 0x800]);

    object_destination(&mut s);
    descriptor(&mut s, TABLE);

    for k in 0..12u32 {
        assert_eq!(object_word(&mut s, k * 16), 0xcc00_0000 | k << 8, "row {k}");
        assert_eq!(object_word(&mut s, k * 16 + 15), 0xcc00_0000 | k << 8 | 15, "row {k}");
    }
}

/// A clipped pixel transfer arrives as several operations: block 0x258
/// ahead of each, one entry per chunk whose header carries that chunk's
/// rows, the interrupt request on the last chunk only, and a host-write
/// tag after the last. Each chunk draws its own rows. A failed
/// operation's drain, block 0x044 with operand 0 and then a host-write
/// with slot 0xd0a, is accepted and writes its tag.
///
/// QEMU: command-fifo-dma-chunks.
#[test]
fn a_clipped_transfer_draws_chunk_by_chunk_and_interrupts_on_the_last() {
    let (mut s, rec, ram) = fenced_board();
    colour_destination(&mut s);
    let tables = [0xb80000u64, 0xb81000];
    let pages = [0xb84000u64, 0xb88000];
    let rows = [300u32, 364];

    for c in 0..2usize {
        let header = [
            0x2000,
            rows[c] << 16 | 200,
            0x50,
            1 << 16 | 2,
            0x8263_305a,
            0,
            8,
            if c == 1 { dma::IRQ_REQUEST } else { 0 } | 0x3201_0000,
        ];
        let mut px = [0u8; 8];
        px[..4].copy_from_slice(&(0x0010_2030u32 + c as u32).to_be_bytes());
        px[4..].copy_from_slice(&(0x0040_5060u32 + c as u32).to_be_bytes());
        ram.put(pages[c], &px);
        put_table(&ram, tables[c], header, &[pte(pages[c], 8)]);
        cmd(&mut s, 0x0001_0658);
        cmd(&mut s, 0x0012_0000);
        descriptor(&mut s, tables[c]);
        assert_eq!(rec.events(DMA_VECTOR).len(), c, "only the last chunk asks for the interrupt");
    }
    for w in [select(reg::HOST_WRITE), 3, 0x5a01] {
        cmd(&mut s, w);
    }

    assert_eq!(pixel(&s, 0, 200, 300), 0x1020_30ff);
    assert_eq!(pixel(&s, 0, 201, 300), 0x4050_60ff);
    assert_eq!(pixel(&s, 0, 200, 364), 0x1020_31ff);
    assert_eq!(pixel(&s, 0, 201, 364), 0x4050_61ff);
    assert_eq!(pixel(&s, 0, 200, 301), 0);
    assert_eq!(ram.half(FENCE_BASE + 6), 0x5a01);

    cmd(&mut s, 0x0001_0444);
    cmd(&mut s, 0);
    for w in [select(reg::HOST_WRITE), 0xd0a, 0x5a02] {
        cmd(&mut s, w);
    }
    assert_eq!(ram.half(FENCE_BASE + 2 * 0xd0a), 0x5a02);
}

/// A 3x2 image of 4-byte pixels at (200, 300) with a 4-byte gap after each
/// row: row 0 starts 8 bytes before the end of page A and finishes in page
/// B, then the gap, then row 1. The gap is never a pixel, and the header
/// asks for no interrupt unless word 7 says so.
///
/// QEMU: command-fifo-dma-image.
#[test]
fn an_image_transfer_skips_the_row_gap_and_crosses_pages() {
    let (mut s, rec, ram) = fenced_board();
    colour_destination(&mut s);
    let mut header = [0x2000, 300 << 16 | 200, 0x50, 2 << 16 | 3, 0x8263_305a, 4, 12, 0x3201_0000];
    let entries = [pte(PAGE_A + 0x3ff8, 8), pte(PAGE_B, 16)];
    ram.put(PAGE_A + 0x3ff8, &[0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00]);
    let b: Vec<u8> = [0x0000_00ffu32, 0xdead_beef, 0x0011_2233, 0x0044_5566, 0x0077_8899]
        .iter()
        .flat_map(|w| w.to_be_bytes())
        .collect();
    ram.put(PAGE_B, &b);
    put_table(&ram, TABLE, header, &entries);

    descriptor(&mut s, TABLE);

    assert_eq!(pixel(&s, 0, 200, 300), 0xff00_00ff);
    assert_eq!(pixel(&s, 0, 201, 300), 0x00ff_00ff);
    assert_eq!(pixel(&s, 0, 202, 300), 0x0000_ffff);
    assert_eq!(pixel(&s, 0, 200, 301), 0x1122_33ff);
    assert_eq!(pixel(&s, 0, 201, 301), 0x4455_66ff);
    assert_eq!(pixel(&s, 0, 202, 301), 0x7788_99ff);
    assert_eq!(pixel(&s, 0, 203, 300), 0);
    assert_eq!(pixel(&s, 0, 200, 302), 0);
    assert!(rec.events(DMA_VECTOR).is_empty(), "the header did not ask for the interrupt");

    // The same transfer asking for it: the pixels again, and the packet.
    header[7] |= dma::IRQ_REQUEST;
    put_table(&ram, TABLE, header, &entries);
    descriptor(&mut s, TABLE);
    assert_eq!(pixel(&s, 0, 200, 300), 0xff00_00ff);
    assert_eq!(rec.events(DMA_VECTOR), [true]);
}

/// A transfer to another destination, an off-screen surface, lands there
/// and leaves the screen alone. Its word 7 carries no format
/// code, so the 4-byte pixels keep the 0x00RRGGBB rule.
///
/// QEMU: command-fifo-dma-image (the object destination).
#[test]
fn an_image_transfer_to_an_object_leaves_the_plane_alone() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    let b: Vec<u8> = [0x0000_00ffu32, 0xdead_beef, 0x0011_2233].iter().flat_map(|w| w.to_be_bytes()).collect();
    ram.put(PAGE_B, &b);
    let header = [0x16a000, 0, 0x10, 1 << 16 | 3, 0x8002_5000, 0, 12, 0x3201_0000];
    put_table(&ram, TABLE, header, &[pte(PAGE_B, 12)]);

    descriptor(&mut s, TABLE);

    // Word 4 0x80025000: 8-byte pixels and fields, which hold the word whole.
    let word = |x| mem_word(&s, 0x16a000, 0x10, 3, Codec::Lanes, x, 0);
    assert_eq!(word(0), 0x0000_ffff);
    assert_eq!(word(1), 0xadbe_efff);
    assert_eq!(word(2), 0x1122_33ff);
    assert_eq!(word(3), 0);
    assert_eq!(pixel(&s, 0, 0, 0), 0);
}

/// A guest page with a guard byte pattern, and the transfer table at
/// `TABLE` naming the 4 bytes at its offset 16 for one pixel.
fn readback_table(s: &mut Board, ram: &Ram, header: [u32; 8]) {
    put_table(ram, TABLE, header, &[pte(PAGE_A + 16, 4)]);
    descriptor(s, TABLE);
}

/// Direction is word 7 bit 24 alone. The X server's 24-bit visual header
/// with the 0x4c format byte reads a screen pixel back as the bytes A, B,
/// G, R with alpha zero, and leaves the plane alone. The same header with
/// bit 24 clear paints the host bytes and writes nothing to the guest.
/// With a zero format byte the GL vocabulary applies: this word 4 is
/// class 2 code 11, ABGR bytes, so the same pixel comes back A, B, G, R
/// with its own alpha.
///
/// QEMU: readback/direction.
#[test]
fn a_readback_sends_the_pixel_to_the_guest_as_abgr() {
    let (mut s, rec, ram) = fenced_board();
    colour_destination(&mut s);
    let mut header = [0x2000, 300 << 16 | 200, 0x50, 0x10001, 0x8263_305a, 0, 4, 0x3301_004c];
    pixel_write(&mut s, 200, 300, 0xc0ff_eeff);
    ram.put(PAGE_A, &[0xa5; 36]);
    let mut expect = [0xa5u8; 36];

    readback_table(&mut s, &ram, header);
    expect[16..20].copy_from_slice(&[0x00, 0xee, 0xff, 0xc0]);
    assert_eq!(ram.get(PAGE_A, 36), expect);
    assert_eq!(pixel(&s, 0, 200, 300), 0xc0ff_eeff, "the plane is left alone");
    assert!(rec.events(DMA_VECTOR).is_empty(), "bit 31 clear: no DMA-done");

    // Bit 24 clear: an upload of the seed, and the guest stays as it was.
    ram.put(PAGE_A + 16, &[0x00, 0x11, 0x22, 0x33]);
    expect[16..20].copy_from_slice(&[0x00, 0x11, 0x22, 0x33]);
    header[7] = 0x3201_004c;
    readback_table(&mut s, &ram, header);
    assert_eq!(pixel(&s, 0, 200, 300), 0x3322_11ff);
    assert_eq!(ram.get(PAGE_A, 36), expect);

    // A zero format byte with this word 4: the GL read of ABGR bytes.
    ram.put(PAGE_A, &[0xa5; 36]);
    header[7] = 0x3301_0000;
    readback_table(&mut s, &ram, header);
    expect[16..20].copy_from_slice(&[0xff, 0x11, 0x22, 0x33]);
    assert_eq!(ram.get(PAGE_A, 36), expect);
    assert_eq!(pixel(&s, 0, 200, 300), 0x3322_11ff);
}

/// The X server's colour-index save of screen pixels, as the Motif text
/// cursor's save-under sends it: the frame buffer named by address (not
/// the read drawable), word 4 class 0 code 1, the 0x41 format byte, one
/// byte a pixel. Board memory records no display mode, so the byte is the
/// field's as it stands. Refusing the read left the guest's buffer as it
/// was, and the cursor's restore then painted that (zero) back: a black
/// block wherever the cursor had been.
#[test]
fn an_index_read_of_board_memory_by_address_returns_the_field_byte() {
    let (mut s, _, ram) = fenced_board();
    colour_destination(&mut s);
    pixel_write(&mut s, 200, 300, 0x2a);
    ram.put(PAGE_A, &[0xa5; 36]);
    let mut expect = [0xa5u8; 36];

    put_table(&ram, TABLE, [0x2000, 300 << 16 | 200, 0x50, 0x10001, 0x8263_3008, 0, 1, 0x3305_0041], &[pte(PAGE_A + 16, 1)]);
    descriptor(&mut s, TABLE);

    expect[16] = 0x2a;
    assert_eq!(ram.get(PAGE_A, 36), expect);
    assert_eq!(pixel(&s, 0, 200, 300), 0x2a, "the plane is left alone");
}

/// The X server's 24-bit TrueColor visuals have red mask 0xff and blue
/// mask 0xff0000, so an XPutImage pixel is 0x00BBGGRR and the server's DMA
/// put names it with source code 0x4c, bytes A, B, G, R. An orange pixel
/// (red 0xfd, green 0x9b, blue 0x01) arrives as 00 01 9b fd and must land
/// orange, not blue.
///
/// QEMU: readback/put-abgr-x-image.
#[test]
fn an_xputimage_pixel_in_abgr_order_lands_orange_not_blue() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    ram.put(PAGE_A + 16, &[0x00, 0x01, 0x9b, 0xfd]);

    readback_table(&mut s, &ram, [0x2000, 300 << 16 | 200, 0x50, 0x10001, 0x8263_305a, 0, 4, 0x3201_004c]);

    assert_eq!(pixel(&s, 0, 200, 300), 0xfd9b_01ff);
}

/// The kernel driver's context switch: one 8-byte store to the switch port
/// completes by writing a nonzero word into the shadow page the shadow
/// address registers name, at the offset the driver polls. No interrupt
/// and no command-FIFO words are involved.
///
/// QEMU: context-switch-completion.
#[test]
fn a_context_switch_writes_its_done_word_into_the_shadow_page() {
    let (mut s, rec, ram) = board_with_ram();
    let base = 0xb70000u64;
    reg_write(&mut s, ctx::SHADOW_ADDR_UPPER, 0);
    reg_write(&mut s, ctx::SHADOW_ADDR_LOWER, base as u32);
    let done = |ram: &Ram| ram.get(base + ctx::SWITCH_DONE_OFFSET, 4);
    assert_eq!(done(&ram), [0; 4]);

    s.mmio_write(ctx::SWITCH_PORT, 0x0000_0700_0001_0001, 8);

    assert_ne!(done(&ram), [0; 4]);
    assert!(rec.silent());
}

/// With no shadow address programmed, the reset value, the switch has
/// nowhere to complete into. It writes nothing rather than guess an
/// address.
///
/// QEMU: context-switch-without-shadow-address-is-a-guest-error.
#[test]
fn a_context_switch_with_no_shadow_address_writes_nothing() {
    let (mut s, _, ram) = board_with_ram();
    let before = ram.get(0, 0x2000);

    s.mmio_write(ctx::SWITCH_PORT, 0x0000_0700_0001_0001, 8);

    assert_eq!(ram.get(0, 0x2000), before);
}


/// The pixel mode the X server selects at the screen before it puts a
/// colour-index image (traced: LightWave Modeler's viewports).
const CI16_MODE: u32 = 0x2008_6980;

/// Index `(x, y)` of the screen's front field as CI16_MODE lays it out.
fn index_at(s: &Board, x: i32, y: i32) -> u32 {
    use crate::dev::odyssey::field::Layout;
    use crate::dev::odyssey::vram::Format;
    s.gpu_sync();
    let f = Format::decode(CI16_MODE);
    let field = Layout::of(f).field(false);
    Rows::new(s.vram_ptr(), View::colour(&Surface::new(0x2000, 0x50, f.fb_log2), field, MAX_DIM)).load_word(x, y).unwrap()
}

/// The X server puts a colour-index image as 2-byte big-endian indices,
/// source code 0x61 at bpp index 0, into the front field at byte 6 of the
/// 16-byte screen pixel (word 4 0x83331071, LightWave's header with a
/// smaller rectangle). Each index lands whole up to the board's 12 bits,
/// the row gap is skipped, and nothing outside the rectangle is touched.
///
/// QEMU: readback/put-ci16-x-image.
#[test]
fn a_ci16_image_put_lands_its_indices_in_the_index_field() {
    let (mut s, _, ram) = board_with_ram();
    for w in [select(reg::DEST_SELECT), 0x2000, 0x50, CI16_MODE, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
    let src: Vec<u8> = [0x0123u16, 0x0456, 0x0fff, 0xdead, 0x0001, 0x0a5a, 0x1234]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    ram.put(PAGE_A, &src);
    let header = [0x2000, 312 << 16 | 147, 0x50, 2 << 16 | 3, 0x8333_1071, 2, 6, 0x3200_0061];
    put_table(&ram, TABLE, header, &[pte(PAGE_A, 12)]);

    descriptor(&mut s, TABLE);

    assert_eq!(index_at(&s, 147, 312), 0x123);
    assert_eq!(index_at(&s, 148, 312), 0x456);
    assert_eq!(index_at(&s, 149, 312), 0xfff);
    assert_eq!(index_at(&s, 147, 313), 0x001);
    assert_eq!(index_at(&s, 148, 313), 0xa5a);
    assert_eq!(index_at(&s, 149, 313), 0x234, "indices keep the board's 12 bits");
    assert_eq!(index_at(&s, 150, 312), 0);
    assert_eq!(index_at(&s, 146, 313), 0);
    assert_eq!(index_at(&s, 147, 314), 0);
}

/// The X server's 30-bit TrueColor save of screen pixels, as the Motif text
/// cursor's save-under sends it in nedit's file dialog: the frame buffer
/// named by address with the window's pixel mode selected, word 7 write
/// code 0x10c (ABGR, 2_10_10_10_REV), four bytes a pixel. Refusing the
/// code left the guest's buffer as it was, and the cursor's restore then
/// painted that back: the glyph columns under the cursor vanished.
#[test]
fn a_30_bit_save_under_returns_abgr_2_10_10_10_rev() {
    use crate::dev::odyssey::field::Layout;
    use crate::dev::odyssey::vram::Format;
    const MODE: u32 = 0x2000_6960;
    let (mut s, _, ram) = fenced_board();
    for w in [select(reg::DEST_SELECT), 0x2000, 0x50, MODE, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
    s.gpu_sync();
    let f = Format::decode(MODE);
    let rows = Rows::new(s.vram_ptr(), View::colour(&Surface::new(0x2000, 0x50, f.fb_log2), Layout::of(f).field(false), MAX_DIM));
    rows.store_word(200, 300, 0xffff_ffff);
    rows.store_word(201, 300, 0x4747_4700);
    ram.put(PAGE_A, &[0xa5; 40]);
    let mut expect = [0xa5u8; 40];

    put_table(&ram, TABLE, [0x2000, 300 << 16 | 200, 0x50, 0x10002, 0x8223_3062, 0, 8, 0xb301_010c], &[pte(PAGE_A + 16, 8)]);
    descriptor(&mut s, TABLE);

    // Ten bits a channel: 0x47 widens to 0x11d, as the plane holds it.
    expect[16..20].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
    expect[20..24].copy_from_slice(&0x11d4_751du32.to_be_bytes());
    assert_eq!(ram.get(PAGE_A, 40), expect);
}

/// buttonfly's texture font: a luminance-alpha texture uploaded by address
/// with no level selected, word 4 0x80001000 (2-byte fields, format byte
/// 0) and source code 0x48, into the level its descriptor later names as
/// format 0xb, a luminance byte then an alpha byte. Each texel keeps its
/// two bytes. Stored as four 4-bit components, a clear texel (L 0xff, A 0)
/// read back with alpha 0xf0 and every glyph drew as an opaque box.
#[test]
fn a_luminance_alpha_texture_upload_keeps_its_alpha() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    ram.put(PAGE_B, &[0xff, 0x00, 0x80, 0xff, 0x3c, 0x5a]);
    let header = [0x16d000, 0, 0x10, 1 << 16 | 3, 0x8000_1000, 0, 6, 0x3200_0048];
    put_table(&ram, TABLE, header, &[pte(PAGE_B, 6)]);

    descriptor(&mut s, TABLE);

    let lum_alpha = Codec::Texel { bytes: 2, field: crate::dev::odyssey::field::FIELD_TWO };
    let word = |x| mem_word(&s, 0x16d000, 0x10, 1, lum_alpha, x, 0);
    assert_eq!(word(0), 0xffff_ff00);
    assert_eq!(word(1), 0x8080_80ff);
    assert_eq!(word(2), 0x3c3c_3c5a);
}

/// Level descriptor `i`'s three words, as the library sends them.
fn tex_level(s: &mut Board, i: u32, word0: u32, word1: u32) {
    for w in [select(reg::TEX_LEVEL + i), word0, word1, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
}

/// The board model checks each texture level the library names against
/// the upload that wrote its memory: buttonfly's luminance-alpha font
/// (format index 0xb over a 2-byte upload of code 0x48) agrees; the same
/// memory named as format index 0xc (four 4-bit components) does not, and
/// the disagreement is counted and kept for `vpro status`.
#[cfg(feature = "vprodiag")]
#[test]
fn a_level_is_checked_against_the_upload_that_wrote_it() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    ram.put(PAGE_B, &[0xff, 0x00, 0x80, 0xff]);
    put_table(&ram, TABLE, [0x16d000, 0, 0x10, 1 << 16 | 2, 0x8000_1000, 0, 4, 0x3200_0048], &[pte(PAGE_B, 4)]);
    descriptor(&mut s, TABLE);

    tex_level(&mut s, 0, 0x16d000, 0x00b0_0010);
    s.gpu_sync();
    assert_eq!((s.diag.packing.checked, s.diag.packing.mismatch_count), (1, 0));

    // Each upload is checked once: a second draw's descriptor is not.
    tex_level(&mut s, 0, 0x16d000, 0x00c0_0010);
    s.gpu_sync();
    assert_eq!((s.diag.packing.checked, s.diag.packing.mismatch_count), (1, 0));

    descriptor(&mut s, TABLE);
    tex_level(&mut s, 0, 0x16d000, 0x00c0_0010);
    s.gpu_sync();
    assert_eq!((s.diag.packing.checked, s.diag.packing.mismatch_count), (2, 1));
    assert!(s.diag.packing.mismatches[0].contains("word 4 0x80001000"), "{}", s.diag.packing.mismatches[0]);
}

/// glprim's 256x256 LUMINANCE (and INTENSITY) texture: a one-byte source,
/// code 0x42, uploaded by address with no level selected under the same
/// word 4 as buttonfly's font, into the level its descriptor names as
/// format index 4, one byte a texel's value. The board model's packing
/// check found it stored as four 4-bit components.
#[test]
fn a_luminance_texture_upload_keeps_its_byte() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    ram.put(PAGE_B, &[0x00, 0x80, 0xff, 0x5a]);
    put_table(&ram, TABLE, [0x16d000, 0, 0x10, 1 << 16 | 4, 0x8000_1000, 0, 4, 0x3205_0042], &[pte(PAGE_B, 4)]);

    descriptor(&mut s, TABLE);
    tex_level(&mut s, 0, 0x16d000, 0x0040_0010);
    s.gpu_sync();

    let one = Codec::Texel { bytes: 2, field: crate::dev::odyssey::field::FIELD_ONE };
    let word = |x| mem_word(&s, 0x16d000, 0x10, 1, one, x, 0);
    assert_eq!([word(0), word(1), word(2), word(3)], [0, 0x8080_8080, 0xffff_ffff, 0x5a5a_5a5a]);
    #[cfg(feature = "vprodiag")]
    assert_eq!((s.diag.packing.checked, s.diag.packing.mismatch_count), (1, 0));
}

/// An 8-byte level names format index 33 (four components) over an
/// upload stored as four 12-bit lanes: the same storage, not a
/// disagreement.
#[cfg(feature = "vprodiag")]
#[test]
fn an_eight_byte_level_over_lanes_is_the_same_packing() {
    let (mut s, _, ram) = board_with_ram();
    colour_destination(&mut s);
    ram.put(PAGE_B, &[0x11, 0x22, 0x33, 0x44]);
    put_table(&ram, TABLE, [0x16b000, 0, 0x10, 1 << 16 | 1, 0x8002_5000, 0, 4, 0x3201_004b], &[pte(PAGE_B, 4)]);

    descriptor(&mut s, TABLE);
    tex_level(&mut s, 0, 0x16b000, 0x8210_0010);
    s.gpu_sync();

    assert_eq!((s.diag.packing.checked, s.diag.packing.mismatch_count), (1, 0));
}

/// A DMA image to the screen stores the same memory and written flag by rows as by pixels, across
/// source forms, pixel sizes and field offsets, rectangles that leave the view, and a memory object.
#[test]
fn a_dma_image_to_the_screen_stores_the_same_by_rows_as_by_pixels() {
    let mut seed = 0x9e37_79b9u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let boards = || {
        let (mut s, _, ram) = board_with_ram();
        for i in 0..1200u32 {
            pixel_write(&mut s, (i % 40) as i32, 1 + (i / 40) as i32, i.wrapping_mul(0x9e37_79b1) | 1);
        }
        (s, ram)
    };
    let ((mut fast, fast_ram), (mut slow, slow_ram)) = (boards(), boards());
    slow.slow_dma = true;
    let rects = [(200u32, 300u32, 3u32, 2u32), (0, 0, 1, 1), (15, 13, 17, 5), (5, 7, 300, 4), (1270, 10, 20, 3), (0, 4090, 5, 10)];
    let controls = [0x3201_0000u32, 0x3201_004b, 0x3201_004c, 0x3201_006b, 0x3207_0000, 0x3205_0000];
    // Word 4: 16-byte pixels (front, back), 4-byte, 8-byte, 8-byte with the field across its word, 2-byte
    // pixels with a 4-byte field, and a 2-byte (coded) field.
    let word4s = [0x8263_305au32, 0x0263_305a, 0x8001_305a, 0x8022_305a, 0x8032_305a, 0x8000_305a, 0x8263_005a];
    const SCREEN: u32 = 0x2000;
    let mut cases = Vec::new();
    for (i, w4) in word4s.into_iter().enumerate() {
        let used = if i == 0 { controls.to_vec() } else { vec![controls[0], controls[2]] };
        for c in used {
            cases.extend(rects.iter().map(|r| (SCREEN, w4, c, *r, false)));
        }
    }
    for c in [controls[0], controls[1]] {
        cases.extend(rects[..3].iter().map(|r| (OBJECT, word4s[0], c, *r, false)));
    }
    // Last: the scale and bias stay set on both boards.
    cases.extend(rects.iter().map(|r| (SCREEN, word4s[0], controls[1], *r, true)));
    let mut written = false;
    for (case, (dest, word4, control, (x, y, w, h), scaled)) in cases.into_iter().enumerate() {
        let bpp = match (control >> 16) & 7 { 1 => 4, 5 => 1, _ => 3 };
        let rowbytes = w * bpp;
        let src: Vec<u8> = (0..rowbytes * h).map(|_| next() as u8).collect();
        let at = 0x40_0ff0u64;
        let mut flags = [false; 2];
        for (k, (s, ram)) in [(&mut *fast, &fast_ram), (&mut *slow, &slow_ram)].into_iter().enumerate() {
            for v in [select(reg::DEST_SELECT), SCREEN, 0x50, 0x0040_6d50, 0] {
                s.mmio_write(ODSY_USER_CMD_FIFO, v as u64, 4);
            }
            if scaled {
                s.tex_mut().scale_bias[0] = 2048;
            }
            ram.put(at, &src);
            let header = [dest, y << 16 | x, 0x50, h << 16 | w, word4, 0, rowbytes, control];
            put_table(ram, TABLE, header, &[pte(at, rowbytes * h)]);
            s.vram_ptr().take_written();
            descriptor(s, TABLE);
            s.gpu_sync();
            flags[k] = s.vram_ptr().take_written();
        }
        let what = format!("case {case} (dest {dest:#x}, word 4 {word4:#x}, control {control:#x}, {w}x{h} at {x},{y}, scaled {scaled})");
        assert_eq!(flags[0], flags[1], "{what}: written flag by rows, by pixels");
        written |= flags[0];
        let (a, b) = (fast.vram.ptr_shared(), slow.vram.ptr_shared());
        let (a, b) = (a.words_at(0, a.size()), b.words_at(0, b.size()));
        if a != b {
            let i = a.iter().zip(b).position(|(p, q)| p != q).unwrap();
            panic!("{what}: word {:#x} is {:#018x} by rows, {:#018x} by pixels", 8 * i, a[i], b[i]);
        }
    }
    assert!(written, "some case stored pixels");
}

/// The DMA counters tell the row-store path from the per-pixel one, say why it was not taken, and
/// separate destinations and source pixel sizes.
#[test]
fn the_dma_counters_split_descriptors_by_destination_pixel_size_and_path() {
    const SCREEN: u32 = 0x2000;
    let (mut s, _, ram) = board_with_ram();
    let put = |s: &mut Board, dest: u32, word4: u32, control: u32, (x, y, w, h): (u32, u32, u32, u32)| {
        let bpp = match (control >> 16) & 7 { 1 => 4, 5 => 1, _ => 3 };
        let at = 0x40_0ff0u64;
        for v in [select(reg::DEST_SELECT), SCREEN, 0x50, 0x0040_6d50, 0] {
            s.mmio_write(ODSY_USER_CMD_FIFO, v as u64, 4);
        }
        ram.put(at, &vec![0x55u8; (w * h * bpp) as usize]);
        put_table(&ram, TABLE, [dest, y << 16 | x, 0x50, h << 16 | w, word4, 0, w * bpp, control], &[pte(at, w * h * bpp)]);
        descriptor(s, TABLE);
        s.gpu_sync();
    };
    let (rgba, bytes3) = (0x3201_0000u32, 0x3207_0000u32);
    put(&mut s, SCREEN, 0x8263_305a, rgba, (200, 300, 3, 2));
    put(&mut s, SCREEN, 0x8263_305a, bytes3, (200, 300, 4, 1));
    put(&mut s, SCREEN, 0x8263_305a, rgba, (1270, 10, 20, 3));
    put(&mut s, OBJECT, 0x8263_305a, rgba, (0, 0, 2, 2));
    s.tex_mut().scale_bias[0] = 2048;
    put(&mut s, SCREEN, 0x8263_305a, rgba, (0, 0, 5, 1));
    // A sentinel upload (two 4-byte pixels), a one-pixel read to the host, a header with no
    // destination (declined) and a header that disagrees with its entries (failed).
    let (sentinel, to_host, nowhere) = (dma::SENTINEL_DRAW, 0x3301_004c, 0);
    put_table(&ram, TABLE, [sentinel, 40 << 16 | 30, 0x50, 1 << 16 | 2, 0x1000, 0, 8, 1 << 30 | 1 << 16 | 0x080], &[pte(PAGE_A, 8)]);
    descriptor(&mut s, TABLE);
    put_table(&ram, TABLE, [SCREEN, 300 << 16 | 200, 0x50, 0x10001, 0x8263_305a, 0, 4, to_host], &[pte(PAGE_A + 16, 4)]);
    descriptor(&mut s, TABLE);
    put_table(&ram, TABLE, [nowhere, 0, 0x50, 0x10001, 0x8263_305a, 0, 4, rgba], &[pte(PAGE_A, 4)]);
    descriptor(&mut s, TABLE);
    put_table(&ram, TABLE, [SCREEN, 0, 0x50, 0x10002, 0x8263_305a, 0, 8, rgba], &[pte(PAGE_A, 4)]);
    descriptor(&mut s, TABLE);
    s.gpu_sync();
    let st = s.dma_stats.snapshot();
    let at = |kind: usize, bytes: usize| (st.cells[kind][bytes].descriptors, st.cells[kind][bytes].pixels, st.cells[kind][bytes].fast_descriptors, st.cells[kind][bytes].fast_pixels);
    // Nine started; seven completed, one declined, one failed.
    assert_eq!((st.seen, st.declined), (9, 1));
    // Screen, 4 bytes: the row-store descriptor (6 pixels), the one off the right edge and the scaled one.
    assert_eq!(at(0, 4), (3, 6 + 60 + 5, 1, 6));
    assert_eq!(at(0, 3), (1, 4, 0, 0));
    assert_eq!(at(1, 4), (1, 4, 0, 0));
    assert_eq!(at(2, 4), (1, 2, 0, 0));
    assert_eq!(at(3, 4), (1, 1, 0, 0));
    let done: u64 = st.cells.iter().flatten().map(|c| c.descriptors).sum();
    assert_eq!(st.seen - st.declined - done, 1, "the header that disagrees with itself");
    // By reason: size, scale, then the row that leaves the view.
    assert_eq!(st.slow, [0, 1, 1, 0, 1]);
    let mut text = Vec::new();
    st.report(&mut text).unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("screen 4 bytes: 3 descriptors, 71 pixels, 1 by rows (6 pixels)"), "{text}");
}

/// A draw-sentinel upload stores the same memory, written flag and counts by runs as by pixels across forms,
/// rectangles, pixel modes, draw masks, clips, window offsets and the id test; fast exactly where taken.
#[test]
fn a_draw_sentinel_upload_stores_the_same_by_runs_as_by_pixels() {
    use crate::dev::odyssey::clip::ClipRect;
    use crate::dev::odyssey::consts::{ODSY_ID_COMPARE_ENABLE, ODSY_ID_COMPARE_MASK_SHIFT};
    use crate::dev::odyssey::dma::DMA_BPP;
    use crate::dev::odyssey::frame::pbj;
    let mut seed = 0x2545_f491u32;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as usize % n
    };
    let boards = || {
        let (mut s, _, ram) = board_with_ram();
        // Board::test_screen()'s window-id buffer, ids 0 to 3 in runs of four pixels over the first rows.
        s.regs().set_pbj(pbj::OVERLAY_BASE, 0x14_2000);
        s.regs().set_pbj(pbj::STRIDE, 80);
        let did = s.dest_mem().did;
        for y in 0..128 {
            for x in 0..1280 {
                did.store1(x, y, ((x >> 2) + y) as u8 & 3);
            }
        }
        (s, ram)
    };
    let ((mut fast, fast_ram), (mut slow, slow_ram)) = (boards(), boards());
    slow.slow_dma = true;
    // (bytes a pixel, word 4, code): RGB, BGR, RGBA, ABGR, the movie's top-down ABGR, BGRA, ARGB; then forms
    // the runs do not take: 2-byte luminance, 3_3_2, red alone, 5_6_5, depth.
    let forms = [(3, 0x1000u32, 0x049u32), (3, 0x1000, 0x04d), (4, 0x1000, 0x04b), (4, 0x1000, 0x04c), (4, dma::MOVIE_FORMAT, 0x04c), (4, 0x1000, 0x051), (4, 0x1000, 0x050), (2, 0x1000, 0x062), (1, 0x1000, 0x0a9), (1, 0x1000, 0x044), (2, 0x1000, 0x189), (4, 0x1000, 0x080)];
    let rects = [(30u32, 40u32, 1u32, 1u32), (30, 40, 17, 5), (0, 0, 300, 4), (1270, 10, 20, 3), (1900, 5, 40, 2), (5, 120, 33, 20), (0, 4090, 5, 10), (63, 63, 3, 3)];
    // Pixel modes and whether their colour fields take runs: RGBA8 fields (two 16-byte pixels, 4-byte pixels, a scratch
    // surface) do; the id planes (a byte field) and a 2-byte coded field do not.
    let modes = [(0x0040_6d50u32, true), (0x2040_6d50, true), (0x0000_24d0, true), (0x0800_24d0, true), (0x2008_0080, false), (0x0000_2450, false)];
    let masks = [1u32, 4, 5, 0, 1, 1];
    let clips: [&[(i32, i32, i32, i32)]; 6] = [&[], &[(0, 0, 2047, 4095)], &[(10, 10, 100, 60)], &[(0, 0, 50, 50), (40, 30, 200, 130)], &[(0, 0, 20, 4), (30, 0, 31, 200), (35, 5, 90, 90), (1000, 0, 1900, 4095)], &[(2000, 0, 2100, 5)]];
    let origins = [[0, 0], [0, 0], [7, 3], [-5, -2], [0, 0]];
    let (mut took, mut declined_by_runs, mut declined_fields, mut fast_before) = (0, 0, 0, (0, 0));
    for case in 0..600 {
        let (bytes, word4, code) = forms[next(forms.len())];
        let ((x, y, w, h), (mode, takes), mask) = (rects[next(rects.len())], modes[next(modes.len())], masks[next(masks.len())]);
        let (clip, origin, id) = (clips[next(clips.len())], origins[next(origins.len())], next(3));
        let (rowbytes, scratch) = (w * bytes, mode == 0x0800_24d0);
        let total = (rowbytes * h) as usize;
        let src: Vec<u8> = (0..total).map(|_| next(256) as u8).collect();
        let split = if total > 1 { 1 + next(total - 1) } else { total };
        let bpp_idx = DMA_BPP.iter().position(|&b| b as u32 == bytes).unwrap() as u32;
        let control = if word4 == dma::MOVIE_FORMAT { dma::MOVIE_CONTROL } else { 1 << 30 | bpp_idx << 16 | code };
        let header = [dma::SENTINEL_DRAW, y << 16 | x, 0x50, h << 16 | w, word4, 0, rowbytes, control];
        let entries = [pte(PAGE_A, split as u32), pte(PAGE_B, (total - split) as u32)];
        let (mut flags, mut stats) = ([false; 2], Vec::new());
        for (k, (s, ram)) in [(&mut *fast, &fast_ram), (&mut *slow, &slow_ram)].into_iter().enumerate() {
            let select_words = if scratch { [OBJECT, 1, mode, 0] } else { [0x2000, 0x50, mode, 0] };
            for v in [select(reg::DEST_SELECT)].into_iter().chain(select_words) {
                s.mmio_write(ODSY_USER_CMD_FIFO, v as u64, 4);
            }
            s.draw_mask = mask;
            s.win.origin = origin;
            s.win.clip_kept = clip.len() as u32;
            for (r, &(x0, y0, x1, y1)) in s.win.clip_rect.iter_mut().zip(clip) {
                *r = ClipRect { x0, y0, x1, y1 };
            }
            s.win.id_compare = if id == 0 { 0 } else { ODSY_ID_COMPARE_ENABLE | 3 << ODSY_ID_COMPARE_MASK_SHIFT | (id as u32 - 1) };
            ram.put(PAGE_A, &src[..split]);
            ram.put(PAGE_B, &src[split..]);
            put_table(ram, TABLE, header, &entries);
            s.vram_ptr().take_written();
            descriptor(s, TABLE);
            s.gpu_sync();
            flags[k] = s.vram_ptr().take_written();
            stats.push(s.dma_stats.snapshot());
        }
        let what = format!("case {case} ({bytes} bytes, code {code:#x}, word 4 {word4:#x}, {w}x{h} at {x},{y}, mode {mode:#x}, mask {mask}, clip {clip:?}, origin {origin:?}, id {id})");
        assert_eq!(flags[0], flags[1], "{what}: written flag by runs, by pixels");
        let (a, b) = (fast.vram.ptr_shared(), slow.vram.ptr_shared());
        let (a, b) = (a.words_at(0, a.size()), b.words_at(0, b.size()));
        if a != b {
            let i = a.iter().zip(b).position(|(p, q)| p != q).unwrap();
            panic!("{what}: word {:#x} is {:#018x} by runs, {:#018x} by pixels", 8 * i, a[i], b[i]);
        }
        let (f, s) = (&stats[0], &stats[1]);
        assert_eq!((f.seen, f.declined), (s.seen, s.declined), "{what}: counts");
        for (cf, cs) in f.cells.iter().flatten().zip(s.cells.iter().flatten()) {
            assert_eq!((cf.descriptors, cf.pixels), (cs.descriptors, cs.pixels), "{what}: cells");
            assert_eq!((cs.fast_descriptors, cs.fast_pixels), (0, 0), "{what}: nothing is fast by pixels");
        }
        // A descriptor adds its whole size to the fast counts, or nothing.
        let now = f.cells[2].iter().fold((0, 0), |t, c| (t.0 + c.fast_descriptors, t.1 + c.fast_pixels));
        let by_runs = now.0 - fast_before.0;
        assert_eq!((by_runs, now.1 - fast_before.1), if by_runs == 1 { (1, (w * h) as u64) } else { (0, 0) }, "{what}: fast counts");
        fast_before = now;
        // A buffer selected, a byte form of 3 or 4 bytes and fields that take runs: taken by runs, else not.
        let expect = takes && mask != 0 && matches!(bytes, 3 | 4) && code != 0x080;
        assert_eq!(by_runs, expect as u64, "{what}: taken by runs");
        if expect {
            took += 1;
        } else if takes {
            declined_by_runs += 1;
        } else if mask != 0 && matches!(bytes, 3 | 4) && code != 0x080 {
            declined_fields += 1;
        }
    }
    assert!(took > 50 && declined_by_runs > 20 && declined_fields > 20, "the cases cover each: {took} by runs, {declined_by_runs} by form or mask, {declined_fields} by field");
}

/// ms a frame of a `BENCH_W` x `BENCH_H` (720 x 486) image of 4-byte pixels (source code 0, the fallback) DMA'd to the
/// screen, as many rows a table as its twelve entries take. `BENCH_FRAMES` a run (0: build only), `BENCH_RUNS` runs; the far corner is checked.
#[test]
#[ignore = "benchmark"]
fn bench_image_dma() {
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (w, h, runs, frames) = (env("BENCH_W", 720), env("BENCH_H", 486), env("BENCH_RUNS", 5), env("BENCH_FRAMES", 4));
    let (mut s, _, ram) = board_with_ram();
    // `test_screen`'s 80 tiles, more only for a wider screen.
    let pitch = (w as u32).div_ceil(16).max(80);
    for w in [select(reg::DEST_SELECT), 0x2000, pitch, 0x0040_6d50, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, w as u64, 4);
    }
    s.win.clip_kept = 1;
    s.win.clip_rect[0] = crate::dev::odyssey::clip::ClipRect { x0: 0, y0: 0, x1: w as i32 - 1, y1: h as i32 - 1 };
    let rowbytes = w * 4;
    let per_table = (12 * 0xffff / rowbytes).max(1);
    let base = 0x40_0000u64;
    let mut px = vec![0u8; rowbytes * h];
    let mut seed = 0x1234_5678u32;
    for p in px.chunks_mut(4) {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        p.copy_from_slice(&(seed | 0xff).to_be_bytes());
    }
    ram.put(base, &px);
    let frame = |s: &mut Board| {
        let mut y = 0;
        while y < h {
            let rows = per_table.min(h - y);
            let (mut at, mut left) = (base + (y * rowbytes) as u64, rows * rowbytes);
            let mut entries = Vec::new();
            while left > 0 {
                let n = left.min(0xffff);
                entries.push(pte(at, n as u32));
                at += n as u64;
                left -= n;
            }
            let header = [0x2000, (y as u32) << 16, pitch, (rows as u32) << 16 | w as u32, 0x8263_305a, 0, rowbytes as u32, 0x3201_0000];
            put_table(&ram, TABLE, header, &entries);
            descriptor(s, TABLE);
            y += rows;
        }
    };
    if frames > 0 {
        frame(&mut s);
        let f = crate::dev::odyssey::vram::Format::decode(0x0040_6d50);
        let l = crate::dev::odyssey::field::Layout::of(f);
        let screen = Rows::new(s.vram_ptr(), View::colour(&Surface::new(0x2000, pitch, f.fb_log2), l.field(false), MAX_DIM));
        s.gpu_sync();
        assert_ne!(screen.load_word(w as i32 - 1, h as i32 - 1).unwrap_or(0), 0, "the far corner pixel was stored");
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
    println!("vpro image dma {w}x{h} 4-byte ({per_table} rows a table): ms a frame {ms:.2?} ({:.2} ns a pixel at the min)", min * 1e6 / (w * h) as f64);
}

/// ms a frame of a `BENCH_W` x `BENCH_H` (720 x 486) image of `BENCH_BYTES` (3) byte pixels DMA'd to the draw sentinel,
/// `BENCH_ROWS` rows a table, `BENCH_ID` the id test on; `BENCH_FRAMES` a run (0: build only), `BENCH_RUNS` runs.
#[test]
#[ignore = "benchmark"]
fn bench_sentinel_dma() {
    use crate::dev::odyssey::consts::{ODSY_ID_COMPARE_ENABLE, ODSY_ID_COMPARE_MASK_SHIFT};
    use crate::dev::odyssey::frame::pbj;
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (w, h, runs, frames, bytes) = (env("BENCH_W", 720), env("BENCH_H", 486), env("BENCH_RUNS", 5), env("BENCH_FRAMES", 4), env("BENCH_BYTES", 3));
    let (mut s, _, ram) = board_with_ram();
    let pitch = (w as u32).div_ceil(16).max(80);
    for v in [select(reg::DEST_SELECT), 0x2000, pitch, 0x0040_6d50, 0] {
        s.mmio_write(ODSY_USER_CMD_FIFO, v as u64, 4);
    }
    s.win.clip_kept = 1;
    s.win.clip_rect[0] = crate::dev::odyssey::clip::ClipRect { x0: 0, y0: 0, x1: w as i32 - 1, y1: h as i32 - 1 };
    if env("BENCH_ID", 0) != 0 {
        // The id buffer after the 16-byte-pixel frame; every id is 0 and the test compares all eight bits with 0.
        let base = 0x2000 + pitch * (h as u32).div_ceil(16) * 256;
        s.regs().set_pbj(pbj::OVERLAY_BASE, base);
        s.regs().set_pbj(pbj::STRIDE, pitch);
        s.win.id_compare = ODSY_ID_COMPARE_ENABLE | 0xff << ODSY_ID_COMPARE_MASK_SHIFT;
    }
    let rowbytes = w * bytes;
    let per_table = env("BENCH_ROWS", (12 * 0xffff / rowbytes).max(1));
    let base = 0x40_0000u64;
    let mut px = vec![0u8; rowbytes * h];
    let mut seed = 0x1234_5678u32;
    for p in px.iter_mut() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *p = (seed >> 24) as u8;
    }
    ram.put(base, &px);
    // RGB bytes (format 9, type 2) or RGBA bytes (format 0xb): the library's codes.
    let (bpp_code, code) = if bytes == 3 { (7, 0x049) } else { (1, 0x04b) };
    let frame = |s: &mut Board| {
        let mut y = 0;
        while y < h {
            let rows = per_table.min(h - y);
            let (mut at, mut left) = (base + (y * rowbytes) as u64, rows * rowbytes);
            let mut entries = Vec::new();
            while left > 0 {
                let n = left.min(0xffff);
                entries.push(pte(at, n as u32));
                at += n as u64;
                left -= n;
            }
            let header = [dma::SENTINEL_DRAW, (y as u32) << 16, 0x50, (rows as u32) << 16 | w as u32, 0x1000, 0, rowbytes as u32, 1 << 30 | bpp_code << 16 | code];
            put_table(&ram, TABLE, header, &entries);
            descriptor(s, TABLE);
            y += rows;
        }
    };
    if frames > 0 {
        frame(&mut s);
        let f = crate::dev::odyssey::vram::Format::decode(0x0040_6d50);
        let l = crate::dev::odyssey::field::Layout::of(f);
        let screen = Rows::new(s.vram_ptr(), View::colour(&Surface::new(0x2000, pitch, f.fb_log2), l.field(false), MAX_DIM));
        s.gpu_sync();
        assert_ne!(screen.load_word(w as i32 - 1, h as i32 - 1).unwrap_or(0), 0, "the far corner pixel was stored");
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
    println!("vpro sentinel dma {w}x{h} {bytes}-byte ({per_table} rows a table): ms a frame {ms:.2?} ({:.2} ns a pixel at the min)", min * 1e6 / (w * h) as f64);
}
